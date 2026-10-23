//! 中控收不到上报的时段：判 agent 离线时不算它们（视图、告警、作业判线路机在线都用，`AppState::report_gaps`）

use crate::views::AGENT_OFFLINE_S;
use serde_json::{json, Value};
use time::OffsetDateTime;

/// 中控收不到上报的时段：中控没在跑（进程启动时补记：上一个进程最后还在跑的时刻到启动，见 `started`）；库连不上——agent 的请求因库不可用
/// （`db::unavailable`）处理不了，而且整整 `GAP_MIN_S` 秒一份上报都没入库（中途有入库的，只是个别请求失败，不算）。
/// 判 agent 离线时这些时段不算它的沉默：恢复后各 agent 退避中的重发（最长约 90 秒）要一会儿才到，按墙钟算会把一直在线的
/// 机器成批判成离线（开一批「agent 离线」严重告警再成批恢复），等摘除的退役、换线路机作业也会判线路机不在线而失败。
/// 停机之前就已经离线的机器照样离线：只扣掉中断的那一段，不是从恢复时重新计时。
/// 最近一小时的时段同时存进库里（app_setting 的 `report_gaps`）：中断恢复后不久又重启一次（连着两次部署、崩溃重启），
/// 新进程只看得到上一个进程最后还在跑的时刻——那已经是恢复之后的了，前一段中断要从库里接着算，否则重发还没到的 agent 照样成批判离线
#[derive(Default)]
pub struct ReportGaps(std::sync::Mutex<Gaps>);

#[derive(Default)]
struct Gaps {
    /// 最近一次上报入库（进程启动也算）
    last_ok: Option<OffsetDateTime>,
    /// 这之后最近一次因库不可用失败
    last_fail: Option<OffsetDateTime>,
    /// 已经结束的中断时段 [起, 止]
    done: Vec<(OffsetDateTime, OffsetDateTime)>,
    /// 最近一次写进库里的存活时刻（`alive`）
    alive_saved: Option<OffsetDateTime>,
}

/// 连续这么久一份上报都没入库、其间又有因库不可用失败的，才算中控收不到上报
const GAP_MIN_S: i64 = 30;
/// 存中断时段的 app_setting 键：`[["起", "止"], …]`（RFC 3339）
const GAPS_KEY: &str = "report_gaps";
/// 中控自己记的存活时刻（app_setting，RFC 3339 字符串）：告警循环每轮写，进程启动时它就是上一个进程最后还在跑的时刻
const ALIVE_KEY: &str = "central_alive_at";
/// 存活时刻和库里的相差这么久以上才重写（每轮都写一遍只会产生死元组）
const ALIVE_EVERY_S: i64 = 30;

/// 加一段中断，和已有的重叠、相接的并成一段：同一段停机不会因为连着重启记两次而被重复扣掉
/// （一直没收到上报时，每次启动补记的都是「库里最新的上报时刻 → 启动」，起点相同）
fn add_gap(done: &mut Vec<(OffsetDateTime, OffsetDateTime)>, gap: (OffsetDateTime, OffsetDateTime)) {
    done.push(gap);
    done.sort();
    let mut merged: Vec<(OffsetDateTime, OffsetDateTime)> = Vec::with_capacity(done.len());
    for (from, to) in done.drain(..) {
        match merged.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => merged.push((from, to)),
        }
    }
    *done = merged;
}

impl ReportGaps {
    /// 进程启动时调：之前的进程记在库里的时段接着算，再补一段「上一个进程最后还在跑的时刻 → 现在」，存回库里。
    /// 起点取它自己记的存活时刻（`alive`）与库里最新的上报时刻中晚的那个；没记过存活时刻（第一次部署）就是最新的上报时刻。
    /// 不能只看上报：上一个进程在跑、只是全网都没上报的时段（模拟器停了、边缘机挂了）不是中控收不到，扣掉的话一直离线的机器每次重启后都被判在线
    pub async fn started(&self, db: &sqlx::PgPool) -> sqlx::Result<()> {
        let last: Option<OffsetDateTime> = sqlx::query_scalar(
            "select greatest((select max(last_report_at) from agent_state), (select (value #>> '{}')::timestamptz from app_setting where key = $1))",
        )
        .bind(ALIVE_KEY)
        .fetch_one(db)
        .await?;
        let saved: Option<Value> = sqlx::query_scalar("select value from app_setting where key = $1").bind(GAPS_KEY).fetch_optional(db).await?;
        let now = OffsetDateTime::now_utc();
        {
            let mut g = self.0.lock().unwrap();
            let rfc = |v: &Value| OffsetDateTime::parse(v.as_str()?, &time::format_description::well_known::Rfc3339).ok();
            for p in saved.as_ref().and_then(Value::as_array).into_iter().flatten() {
                if let (Some(from), Some(to)) = (rfc(&p[0]), rfc(&p[1])) {
                    add_gap(&mut g.done, (from, to));
                }
            }
            if let Some(l) = last.filter(|l| *l < now) {
                add_gap(&mut g.done, (l, now));
            }
            g.done.retain(|(_, to)| (now - *to).whole_hours() < 1);
            g.last_ok = Some(now);
        }
        self.save(db).await
    }

    /// 把最近一小时的时段存进库里（启动时、库中断恢复后）
    pub async fn save(&self, db: &sqlx::PgPool) -> sqlx::Result<()> {
                let v: Vec<[String; 2]> = self.0.lock().unwrap().done.iter().map(|(f, t)| [crate::meta::fmt_ts(*f), crate::meta::fmt_ts(*t)]).collect();
        sqlx::query(
            "insert into app_setting (key, value, updated_by) values ($1, $2, 'fleet-server')
             on conflict (key) do update set value = excluded.value, updated_at = now(), updated_by = excluded.updated_by",
        )
        .bind(GAPS_KEY)
        .bind(json!(v))
        .execute(db)
        .await
        .map(|_| ())
    }

    /// 告警循环每轮调：把「中控此刻还在跑」记进库里（和上次写的相差满 `ALIVE_EVERY_S` 才写），下次启动时 `started` 从这里补中断
    pub async fn alive(&self, db: &sqlx::PgPool, now: OffsetDateTime) -> sqlx::Result<()> {
        if self.0.lock().unwrap().alive_saved.is_some_and(|t| (now - t).whole_seconds() < ALIVE_EVERY_S) {
            return Ok(());
        }
        let at = crate::meta::fmt_ts(now);
        sqlx::query(
            "insert into app_setting (key, value, updated_by) values ($1, $2, 'fleet-server')
             on conflict (key) do update set value = excluded.value, updated_at = now(), updated_by = excluded.updated_by",
        )
        .bind(ALIVE_KEY)
        .bind(json!(at))
        .execute(db)
        .await?;
        self.0.lock().unwrap().alive_saved = Some(now);
        Ok(())
    }

    /// 一份上报入库了：之前在失败的，从上次入库到现在算一段（够 `GAP_MIN_S` 的话）。记了新的一段返回 true（调用方存进库里）
    pub fn ok(&self, now: OffsetDateTime) -> bool {
        let mut g = self.0.lock().unwrap();
        let mut added = false;
        if let (Some(from), Some(_)) = (g.last_ok, g.last_fail.take()) {
            if (now - from).whole_seconds() >= GAP_MIN_S {
                add_gap(&mut g.done, (from, now));
                added = true;
            }
        }
        g.last_ok = Some(now);
        // 结束一小时以上的不再影响判定：之后一直没报的早就按自己的沉默算离线了
        g.done.retain(|(_, to)| (now - *to).whole_hours() < 1);
        added
    }

    /// agent 的请求因库不可用没能处理
    pub fn failed(&self, now: OffsetDateTime) { self.0.lock().unwrap().last_fail = Some(now); }

    /// 上次上报在 `last` 的 agent 到 `now` 算不算在线：沉默的时长扣掉其间中控收不到上报的时段（还没恢复的这一段算到最近一次失败）
    pub fn online(&self, last: OffsetDateTime, now: OffsetDateTime) -> bool {
        let g = self.0.lock().unwrap();
        let open = g.last_ok.zip(g.last_fail).filter(|(from, to)| (*to - *from).whole_seconds() >= GAP_MIN_S);
        let deaf: i64 = g.done.iter().copied().chain(open).filter(|(_, to)| *to > last).map(|(from, to)| (to - from.max(last)).whole_seconds()).sum();
        (now - last).whole_seconds() - deaf < AGENT_OFFLINE_S
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 中控收不到上报的时段不算 agent 的沉默；停机之前就离线的照样离线；中途有入库的不算中控收不到
    #[test]
    fn report_gaps_do_not_count_as_silence() {
        let t0 = OffsetDateTime::now_utc();
        let s = |n: i64| t0 + time::Duration::seconds(n);
        // 中控停了 0–300 秒（进程启动时补记的那段）：停之前 20 秒报过的，恢复后 99 秒还算在线、101 秒算离线
        let g = ReportGaps::default();
        {
            let mut x = g.0.lock().unwrap();
            x.done.push((s(0), s(300)));
            x.last_ok = Some(s(300));
        }
        assert!(g.online(s(-20), s(399)) && !g.online(s(-20), s(401)));
        assert!(!g.online(s(-3600), s(301)), "停机之前一小时就不报了：照样离线");
        assert!(!ReportGaps::default().online(s(-20), s(399)), "没有中断：按墙钟算");
        // 库连不上：400 秒入库之后一直失败，还没恢复的这一段算到最近一次失败
        g.ok(s(400));
        g.failed(s(410));
        g.failed(s(560));
        assert!(g.online(s(395), s(565)));
        // 恢复：400–570 记成一段
        g.ok(s(570));
        assert!(g.online(s(395), s(640)) && !g.online(s(395), s(700)));
        // 中途有入库的（个别请求失败），不算中控收不到
        let h = ReportGaps::default();
        for (i, ok) in [(0, true), (5, false), (10, true), (15, false), (20, true), (40, false)] {
            if ok {
                h.ok(s(i));
            } else {
                h.failed(s(i));
            }
        }
        assert!(h.0.lock().unwrap().done.is_empty());
        assert!(!h.online(s(-120), s(41)));
    }
}
