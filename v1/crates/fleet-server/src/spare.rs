//! 备用团队（team.spare）：替换下来的机器放进备用团队，订阅过期后再留 1 天，由中控自动删除。每小时的维护任务（lib.rs housekeeping）调 `run_once`：
//! 1. 备用团队里在用（active）的机器，有订阅且账单时区的今天晚于「到期日 + 1 天」（`expired`：账单时区「到期日 + 2 天」的 0 点起）
//!    → 发起退役，与手动退役同一条路径（api::machines::start_retire，带清理；宿主机连带它上面的同机 IP 节点）。
//!    操作人记「系统」，作业参数与审计带 auto = spare_expiry；经通知渠道发一条「自动删除」。
//! 2. 这样退役完的机器（最近一次退役作业带 auto = spare_expiry）：彻底删除不被拦（db::purge_blocker）时接着彻底删除（api::machines::start_purge）；
//!    被拦的（入口流量还计入别的机器的统计、宿主机上还有同机 IP 节点没删）留到条件满足后的那一轮。
//!
//! 机器（含它上面的同机 IP 节点）有进行中的作业时跳过，下一轮再看；彻底删除失败过的不再自动重建（等人重试那个作业）。
//! 续费顺延了到期日自然不删。到期前的提醒复用续费告警（health.rs 在提醒文字后接 `due_note`）：过期第 1 天的「已过期」告警就是「明天自动删除」的通知。

use crate::api::machines::{start_purge, start_retire};
use crate::notify::{self, Kind, Notice};
use crate::state::AppState;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

/// 自动删除的操作人（作业的创建者、审计的操作人）
pub const ACTOR: &str = "系统";
/// 作业参数与审计里的 auto 标记
pub const AUTO: &str = "spare_expiry";

/// 到期日之后满一天：账单时区的今天晚于到期日 + 1 天（到期日 10-20：10-21 还留着，10-22 起删）
pub fn expired(today: Date, due: Date) -> bool { due.next_day().is_some_and(|d| today > d) }

/// 备用团队的机器在续费提醒后面接的一句（`due_days` = 到期日 − 账单时区的今天）
pub fn due_note(due_days: i64) -> &'static str {
    match due_days {
        1.. => "，备用节点过期满 1 天后自动删除",
        0 => "，备用节点后天自动删除",
        -1 => "，备用节点明天自动删除",
        _ => "，备用节点即将自动删除",
    }
}

/// 一轮做了什么（测试用；日志在各步里记）
#[derive(Debug, Default)]
pub struct Ran {
    /// 发起了退役的机器（不含随宿主机退役的同机 IP 节点）
    pub retired: Vec<Uuid>,
    /// 建了彻底删除作业的机器
    pub purged: Vec<Uuid>,
    /// 已经过期满 1 天、但有进行中的作业，这一轮跳过的
    pub busy: Vec<Uuid>,
    /// 已经过期满 1 天、但它上面有不在备用团队的同机 IP 节点（退役会连带它们），不自动删的
    pub kept: Vec<Uuid>,
}

#[derive(sqlx::FromRow)]
struct Due {
    id: Uuid,
    kind: String,
    name: String,
    due_date: Date,
    today: Date,
    busy: bool,
    keeps_live: bool,
}

/// `now`：按这一刻算各订阅账单时区里的「今天」（测试传固定时刻）
pub async fn run_once(st: &AppState, now: OffsetDateTime) -> anyhow::Result<Ran> {
    let mut ran = Ran::default();
    // 先删上一轮及更早退役完的，再发起这一轮的退役（刚发起的还在退役中，本来也删不了）
    purge_pass(st, &mut ran).await?;
    let rows: Vec<Due> = sqlx::query_as(
        "select m.id, m.kind::text kind, display_name(m.kind::text, m.label, t.name, m.number) name, s.due_date, ($1::timestamptz at time zone s.billing_tz)::date today,
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
        // 宿主机退役会连带它上面的同机 IP 节点：有不在备用团队里的（还在正常用）就不自动删，留给人处理
        if r.keeps_live {
            tracing::warn!(machine = %r.id, "备用节点 {} 已过期满 1 天，但它上面有不在备用团队的同机 IP 节点：不自动删除", r.name);
            ran.kept.push(r.id);
            continue;
        }
        if r.busy {
            tracing::info!(machine = %r.id, "备用节点 {} 已过期满 1 天，但有进行中的作业：这一轮先不删", r.name);
            ran.busy.push(r.id);
            continue;
        }
        let reason = format!("备用到期自动删除（到期日 {}）", r.due_date);
        match start_retire(st, ACTOR, r.id, &r.name, &r.kind, Some(&reason), true, Some(AUTO)).await {
            Ok(v) => {
                tracing::info!(machine = %r.id, "备用节点 {} 已过期满 1 天（到期日 {}）：已自动发起退役", r.name, r.due_date);
                let ip: Vec<&str> = v["ip_node_jobs"].as_array().into_iter().flatten().filter_map(|j| j["name"].as_str()).collect();
                let with = if ip.is_empty() { String::new() } else { format!("（同机 IP 节点 {} 一并退役）", ip.join("、")) };
                notify::enqueue(
                    st,
                    Notice {
                        kind: Kind::Auto,
                        incident_id: None,
                        rule: AUTO.into(),
                        severity: "warning".into(),
                        machine: r.name.clone(),
                        summary: format!("备用节点已过期满 1 天（到期日 {}）：已自动退役并清理服务{with}，退役完成后彻底删除", r.due_date),
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

/// 自动退役完的机器接着彻底删除。同机 IP 节点排在前面：宿主机要等它们删完（purge_blocker），同一轮里宿主机被拦、下一轮再删
async fn purge_pass(st: &AppState, ran: &mut Ran) -> anyhow::Result<()> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "select m.id, display_name(m.kind::text, m.label, t.name, m.number) from machine m left join team t on t.id = m.team_id
         where m.lifecycle = 'retired'
           and (select j.params->>'auto' from job j where j.target_id = m.id and j.kind = 'retire' order by j.created_at desc limit 1) = $1
           and not exists (select 1 from job j where j.target_id = m.id
                           and (j.state in ('queued', 'running', 'waiting', 'cancelling') or (j.kind = 'purge' and j.state = 'failed')))
         order by m.host_id is null, m.id",
    )
    .bind(AUTO)
    .fetch_all(&st.db)
    .await?;
    for (id, name) in rows {
        if let Some(why) = crate::db::purge_blocker(&st.db, id).await? {
            tracing::debug!(machine = %id, "备用节点 {name} 暂不能彻底删除：{why}");
            continue;
        }
        match start_purge(st, ACTOR, id, &name, Some(AUTO)).await {
            Ok(_) => {
                tracing::info!(machine = %id, "备用节点 {name} 已退役：自动彻底删除");
                ran.purged.push(id);
            }
            Err(e) => tracing::warn!(machine = %id, "备用节点 {name} 自动彻底删除没有发起：{}", e.message),
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
        assert!(expired(date!(2026 - 10 - 22), due), "到期日 + 1 天之后删");
        assert!(expired(date!(2026 - 12 - 01), due));
        // 跨月、跨年
        assert!(!expired(date!(2027 - 01 - 01), date!(2026 - 12 - 31)) && expired(date!(2027 - 01 - 02), date!(2026 - 12 - 31)));
        assert!(!expired(date!(2028 - 03 - 01), date!(2028 - 02 - 29)) && expired(date!(2028 - 03 - 02), date!(2028 - 02 - 29)));
    }

    #[test]
    fn note_matches_deletion_day() {
        // 到期日 10-20 在 10-22 删：10-20（due_days 0）写「后天」，10-21（-1）写「明天」
        let due = date!(2026 - 10 - 20);
        assert!(!expired(date!(2026 - 10 - 21), due) && expired(date!(2026 - 10 - 22), due));
        assert_eq!(due_note(0), "，备用节点后天自动删除");
        assert_eq!(due_note(-1), "，备用节点明天自动删除");
        assert!(due_note(5).contains("过期满 1 天后") && due_note(-3).contains("即将"));
    }
}
