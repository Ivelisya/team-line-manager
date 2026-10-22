//! 备用团队（team.spare，只有一个）与到期回收。退役 = 移进备用团队（api::machines::move_to_spare），机器照常运行；
//! 订阅过期满 1 天后由中控清理并移进回收站（recycle.rs）。替换下来的旧节点也放这里。每小时的维护任务（lib.rs housekeeping）调 `run_once`：
//! 1. `recycle_pass`：已退役、还没进回收站、没有进行中作业的机器（上架取消的、这一版之前就退役了的）移进回收站。
//!    退役作业完成的机器在作业收尾时（jobs::steps::finalize，同一个事务）就进了回收站，不经这里。
//! 2. `purge_pass`：回收站里的机器行，db::purge_blocker 放行时物理删除（api::machines::start_purge）；被拦的（入口流量还计入别的机器的统计、
//!    宿主机上还有同机 IP 节点没删）留到条件满足后的那一轮。
//! 3. 备用团队里在用的机器，有订阅且账单时区的今天晚于「到期日 + 1 天」（`expired`：账单时区 `recycle_on` 的 0 点起）
//!    → 发起退役作业，与「立即回收」同一条路径（api::machines::start_retire，带清理；宿主机连带它上面的同机 IP 节点）。
//!    操作人记「系统」，作业参数与审计带 auto = spare_expiry；经通知渠道发一条「自动回收」。
//!
//! 机器（含它上面的同机 IP 节点）有进行中的作业时跳过，下一轮再看；物理删除失败过的不再自动重建（等人重试那个作业）。
//! 续费顺延了到期日自然不回收。到期前的提醒复用续费告警（health.rs 在提醒文字后接 `due_note` 和还指向它的软路由出口）：
//! 过期第 1 天的「已过期」告警就是「明天自动回收」的通知。软路由不自动改：出口还指向它的照样回收（到期时供应商通常已经停机）。

use crate::api::machines::{start_purge, start_retire};
use crate::notify::{self, Kind, Notice};
use crate::state::{AppState, Event};
use time::{Date, OffsetDateTime};
use uuid::Uuid;

/// 自动回收的操作人（作业的创建者、审计的操作人）
pub const ACTOR: &str = "系统";
/// 作业参数与审计里的 auto 标记：备用到期
pub const AUTO: &str = "spare_expiry";
/// 审计里的 auto 标记：已退役的机器由维护任务收进回收站（`recycle_pass`）
pub const SWEEP: &str = "retired_sweep";

/// 到期日之后满一天：账单时区的今天晚于到期日 + 1 天（到期日 10-20：10-21 还留着，10-22 起回收）
pub fn expired(today: Date, due: Date) -> bool { due.next_day().is_some_and(|d| today > d) }

/// 自动回收的日子（账单时区）：到期日 + 2 天，这一天的 0 点起 `expired` 成立。机器视图的 recycle_on、退役预览都按它
pub fn recycle_on(due: Date) -> Date { due.next_day().and_then(Date::next_day).unwrap_or(due) }

/// 能不能「移进备用」（退役的默认去向），不能时给出原因（只能立即回收）。`sub`：订阅的 (到期日, 账单时区的今天)，没有订阅为 None。
/// 退役预览、提交共用
pub fn spare_eligible(kind: &str, lifecycle: &str, in_spare: bool, sub: Option<(Date, Date)>) -> Result<(), &'static str> {
    if kind == "relay" {
        return Err("线路机不属于任何团队，只能立即回收");
    }
    if in_spare {
        return Err("已经在备用团队里，只能立即回收");
    }
    match lifecycle {
        "active" => {}
        "retiring" | "retired" => return Err("已经在退役"),
        _ => return Err("还没上线（上架中或上架失败），只能立即回收"),
    }
    match sub {
        None => Err("没有订阅，没有到期日可等，只能立即回收"),
        Some((due, today)) if expired(today, due) => Err("订阅已经过期满 1 天，只能立即回收"),
        Some(_) => Ok(()),
    }
}

/// 备用团队的机器在续费提醒后面接的一句（`due_days` = 到期日 − 账单时区的今天）
pub fn due_note(due_days: i64) -> &'static str {
    match due_days {
        1.. => "，备用节点过期满 1 天后自动回收",
        0 => "，备用节点后天自动回收",
        -1 => "，备用节点明天自动回收",
        _ => "，备用节点即将自动回收",
    }
}

/// 一轮做了什么（测试用；日志在各步里记）
#[derive(Debug, Default)]
pub struct Ran {
    /// 发起了退役的机器（不含随宿主机退役的同机 IP 节点）
    pub retired: Vec<Uuid>,
    /// 由 `recycle_pass` 收进回收站的机器
    pub recycled: Vec<Uuid>,
    /// 建了物理删除作业的机器
    pub purged: Vec<Uuid>,
    /// 已经过期满 1 天、但有进行中的作业，这一轮跳过的
    pub busy: Vec<Uuid>,
    /// 已经过期满 1 天、但它上面有不在备用团队的同机 IP 节点（退役会连带它们），不自动回收的
    pub kept: Vec<Uuid>,
}

#[derive(sqlx::FromRow)]
struct Due {
    id: Uuid,
    kind: String,
    name: String,
    reason: Option<String>,
    due_date: Date,
    today: Date,
    busy: bool,
    keeps_live: bool,
}

/// `now`：按这一刻算各订阅账单时区里的「今天」（测试传固定时刻）
pub async fn run_once(st: &AppState, now: OffsetDateTime) -> anyhow::Result<Ran> {
    let mut ran = Ran::default();
    // 先收、再删上一轮及更早退役完的，最后发起这一轮的退役（刚发起的还在退役中，本来也收不了）
    recycle_pass(st, &mut ran).await?;
    purge_pass(st, &mut ran).await?;
    let rows: Vec<Due> = sqlx::query_as(
        "select m.id, m.kind::text kind, display_name(m.kind::text, m.label, t.name, m.number) name, m.lifecycle_reason reason, s.due_date,
                ($1::timestamptz at time zone s.billing_tz)::date today,
                exists (select 1 from job j where j.state in ('queued', 'running', 'waiting', 'cancelling')
                        and (j.target_id = m.id or j.target_id in (select x.id from machine x where x.host_id = m.id))) busy,
                exists (select 1 from machine x left join team tx on tx.id = x.team_id
                        where x.host_id = m.id and x.lifecycle <> 'retired' and not coalesce(tx.spare, false)) keeps_live
         from machine m join team t on t.id = m.team_id join subscription_eff s on s.machine_id = m.id
         where t.spare and m.lifecycle = 'active'
         order by t.sort_order, t.name, m.number, m.id",
    )
    .bind(now)
    .fetch_all(&st.db)
    .await?;
    for r in rows.into_iter().filter(|r| expired(r.today, r.due_date)) {
        // 宿主机退役会连带它上面的同机 IP 节点：有不在备用团队里的（还在正常用）就不自动回收，留给人处理
        if r.keeps_live {
            tracing::warn!(machine = %r.id, "备用节点 {} 已过期满 1 天，但它上面有不在备用团队的同机 IP 节点：不自动回收", r.name);
            ran.kept.push(r.id);
            continue;
        }
        if r.busy {
            tracing::info!(machine = %r.id, "备用节点 {} 已过期满 1 天，但有进行中的作业：这一轮先不回收", r.name);
            ran.busy.push(r.id);
            continue;
        }
        // 进备用时记下的退役原因留着，后面接上为什么现在回收
        let why = format!("备用到期自动回收（到期日 {}）", r.due_date);
        let reason = match r.reason.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(prev) => format!("{prev}；{why}"),
            None => why,
        };
        match start_retire(st, ACTOR, r.id, &r.name, &r.kind, Some(&reason), true, Some((AUTO, now))).await {
            Ok(v) => {
                tracing::info!(machine = %r.id, "备用节点 {} 已过期满 1 天（到期日 {}）：已自动发起退役", r.name, r.due_date);
                let ip: Vec<&str> = v["ip_node_jobs"].as_array().into_iter().flatten().filter_map(|j| j["name"].as_str()).collect();
                let with = if ip.is_empty() { String::new() } else { format!("（同机 IP 节点 {} 一并回收）", ip.join("、")) };
                notify::enqueue(
                    st,
                    Notice {
                        kind: Kind::Auto,
                        incident_id: None,
                        rule: AUTO.into(),
                        severity: "warning".into(),
                        machine: r.name.clone(),
                        summary: format!("备用节点已过期满 1 天（到期日 {}）：已自动退役并清理服务{with}，完成后移进回收站", r.due_date),
                        at: now,
                        opened_at: None,
                    },
                )
                .await;
                ran.retired.push(r.id);
            }
            // 线路机上还有入口、作业正在执行一步、刚被别人退役……：记下来，下一轮再看
            Err(e) => tracing::warn!(machine = %r.id, "备用节点 {} 到期自动退役没有发起：{}", r.name, e.message),
        }
    }
    Ok(ran)
}

/// 已退役、还没进回收站的机器（不经退役作业收尾的：上架取消、这一版之前就退役了的）移进回收站。有进行中作业的等下一轮
async fn recycle_pass(st: &AppState, ran: &mut Ran) -> anyhow::Result<()> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "select m.id from machine m where m.lifecycle = 'retired' and m.recycled_at is null
           and not exists (select 1 from job j where j.target_id = m.id and j.state in ('queued', 'running', 'waiting', 'cancelling'))
         order by m.id",
    )
    .fetch_all(&st.db)
    .await?;
    for id in ids {
        let mut tx = st.db.begin().await?;
        if crate::recycle::record(&mut tx, id, ACTOR, Some(SWEEP)).await?.is_some() {
            tx.commit().await?;
            tracing::info!(machine = %id, "已退役的机器移进回收站");
            ran.recycled.push(id);
        }
    }
    if !ran.recycled.is_empty() {
        st.emit(Event::Machines);
    }
    Ok(())
}

/// 回收站里的机器行物理删除。同机 IP 节点排在前面：宿主机要等它们删完（purge_blocker），同一轮里宿主机被拦、下一轮再删
async fn purge_pass(st: &AppState, ran: &mut Ran) -> anyhow::Result<()> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "select m.id, display_name(m.kind::text, m.label, t.name, m.number) from machine m left join team t on t.id = m.team_id
         where m.recycled_at is not null and m.lifecycle = 'retired'
           and not exists (select 1 from job j where j.target_id = m.id
                           and (j.state in ('queued', 'running', 'waiting', 'cancelling') or (j.kind = 'purge' and j.state = 'failed')))
         order by m.host_id is null, m.id",
    )
    .fetch_all(&st.db)
    .await?;
    for (id, name) in rows {
        if let Some(why) = crate::db::purge_blocker(&st.db, id).await? {
            tracing::debug!(machine = %id, "回收站里的 {name} 暂不能物理删除：{why}");
            continue;
        }
        match start_purge(st, ACTOR, id, &name, Some(AUTO)).await {
            Ok(_) => {
                tracing::info!(machine = %id, "回收站里的 {name}：自动物理删除");
                ran.purged.push(id);
            }
            Err(e) => tracing::warn!(machine = %id, "回收站里的 {name} 自动物理删除没有发起：{}", e.message),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    #[test]
    fn expired_after_one_full_day() {
        let due = date!(2026 - 10 - 20);
        assert!(!expired(date!(2026 - 10 - 19), due), "到期前");
        assert!(!expired(due, due), "到期日当天");
        assert!(!expired(date!(2026 - 10 - 21), due), "到期日 + 1 天当天还留着");
        assert!(expired(date!(2026 - 10 - 22), due), "到期日 + 1 天之后回收");
        assert!(expired(date!(2026 - 12 - 01), due));
        // 跨月、跨年
        assert!(!expired(date!(2027 - 01 - 01), date!(2026 - 12 - 31)) && expired(date!(2027 - 01 - 02), date!(2026 - 12 - 31)));
        assert!(!expired(date!(2028 - 03 - 01), date!(2028 - 02 - 29)) && expired(date!(2028 - 03 - 02), date!(2028 - 02 - 29)));
        // recycle_on 就是 expired 成立的第一天
        for due in [due, date!(2026 - 12 - 31), date!(2028 - 02 - 28)] {
            let on = recycle_on(due);
            assert!(expired(on, due) && !expired(on.previous_day().unwrap(), due), "{due}");
        }
        assert_eq!(recycle_on(date!(2026 - 11 - 15)), date!(2026 - 11 - 17));
    }

    #[test]
    fn note_matches_recycle_day() {
        // 到期日 10-20 在 10-22 回收：10-20（due_days 0）写「后天」，10-21（-1）写「明天」
        let due = date!(2026 - 10 - 20);
        assert!(!expired(date!(2026 - 10 - 21), due) && expired(date!(2026 - 10 - 22), due));
        assert_eq!(due_note(0), "，备用节点后天自动回收");
        assert_eq!(due_note(-1), "，备用节点明天自动回收");
        assert!(due_note(5).contains("过期满 1 天后") && due_note(-3).contains("即将"));
    }

    /// 各种机器能不能进备用（设计 §2.2）：不能的只剩立即回收
    #[test]
    fn spare_eligibility_table() {
        let today = date!(2026 - 10 - 23);
        let ok = Some((date!(2026 - 11 - 15), today));
        let due_today = Some((today, today));
        let one_day_over = Some((date!(2026 - 10 - 22), today));
        let gone = Some((date!(2026 - 10 - 21), today));
        let e = |kind, lc, in_spare, sub| spare_eligible(kind, lc, in_spare, sub);
        assert_eq!(e("vps", "active", false, ok), Ok(()), "在用、有订阅、没过期");
        assert_eq!(e("proxy", "active", false, due_today), Ok(()), "今天到期");
        assert_eq!(e("vps", "active", false, one_day_over), Ok(()), "过期第 1 天还能进（明天才回收）");
        assert!(e("vps", "active", false, gone).unwrap_err().contains("过期满 1 天"));
        assert!(e("vps", "active", false, None).unwrap_err().contains("没有订阅"));
        assert!(e("relay", "active", false, ok).unwrap_err().contains("线路机"));
        assert!(e("vps", "active", true, ok).unwrap_err().contains("已经在备用"));
        assert!(e("vps", "provisioning", false, ok).unwrap_err().contains("上架中或上架失败"));
        assert!(e("vps", "failed", false, ok).unwrap_err().contains("上架中或上架失败"));
        assert_eq!(e("vps", "retiring", false, ok), Err("已经在退役"));
    }
}
