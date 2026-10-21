//! 告警：每 30 s 用 health::evaluate 的结果对齐 incident 表，并在触发/恢复时发通知。
//! 只看 active 的机器（其它生命周期 evaluate 不给原因），所以退役/上架中的机器天然不告警。
//!
//! 去抖（开、关对称）：
//! - 开：同一 (规则, 机器) 连续出现满去抖时长才开告警（出口不通 150 s、TCP 不通 90 s、其余 60 s）；续费类立即开。被静默的不开。
//! - 关：原因连续消失满同样的时长才恢复；续费类立即恢复（按日期判定，不会抖）。
//! - 复发：恢复后 30 分钟内同一 (规则, 机器) 又满足开的条件，重开原告警（`reopen_count` +1，清掉确认），不新建。
//!
//! 通知（见 notify.rs）：新告警发「触发」；仍在触发的严重告警每 4 小时提醒一次，续费类每天一次；
//! 原因消失恢复发「恢复」，只在上次恢复通知之后发过触发/提醒时才发（复发后没再通知过的不重复报恢复）。
//! 复发重开：已报过恢复的，距那条恢复通知满 5 分钟就立即发「触发」，不满则等满 5 分钟仍在触发再发，
//! 所以反复抖动的告警每 5 分钟最多一对触发/恢复，真故障最多晚 5 分钟报。被静默（含维护）的不发任何通知。

use crate::notify::{self, Kind, Notice};
use crate::state::{AppState, Event};
use crate::views;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

/// 开、关去抖时长：探测类结果要等下一轮探测才会变，单次偶发失败也常见，拉长一些
/// （出口探测周期长，150 s；TCP 探测 90 s）；其余规则 60 s
fn debounce_s(rule: &str) -> i64 {
    match rule {
        "egress_fail" => 150,
        "tcp_fail" => 90,
        _ => 60,
    }
}
/// 恢复后这么久内复发算同一条告警
const REOPEN_WINDOW_S: i64 = 30 * 60;
/// 报过恢复后这么久内复发不再报触发（抖动限流）
const REOPEN_QUIET_S: i64 = 5 * 60;
/// 严重告警持续触发的提醒间隔
const REPEAT_CRITICAL_S: i64 = 4 * 3600;
/// 续费类提醒间隔（每天最多一次）
const REPEAT_RENEWAL_S: i64 = 24 * 3600;

/// 评估循环在两次之间要记住的东西
#[derive(Default)]
pub struct AlertState {
    first_seen: HashMap<(String, Uuid), OffsetDateTime>,
    /// incident id → 原因从什么时候起一直没有
    clean_since: HashMap<Uuid, OffsetDateTime>,
}

pub async fn run_forever(st: AppState) {
    let mut state = AlertState::default();
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Err(e) = evaluate_once(&st, &mut state, OffsetDateTime::now_utc()).await {
            tracing::error!("告警评估失败：{e:#}");
        }
    }
}

#[derive(sqlx::FromRow)]
struct Open {
    id: Uuid,
    rule: String,
    subject_id: Uuid,
    severity: String,
    summary: String,
    opened_at: OffsetDateTime,
    notified_at: Option<OffsetDateTime>,
    /// 最近一条通知是「恢复」时它的发送时间（复发重开后还没再通知过）
    told_resolved_at: Option<OffsetDateTime>,
}

/// `Open` 的列（查询和重开的 returning 共用）
const OPEN_COLS: &str = "id, rule, subject_id, severity::text severity, summary, opened_at, notified_at,
    (select case when l.kind = 'resolved' then l.ts end from notify_log l where l.incident_id = incident.id and l.kind in ('firing', 'repeat', 'resolved')
     order by l.ts desc, l.id desc limit 1) told_resolved_at";

/// 仍在触发的告警这一轮该发的通知：报过恢复的，距那条恢复通知满 REOPEN_QUIET_S 发「触发」；否则按持续提醒间隔发「提醒」。
/// notify_log 在送达后才写（发送队列积压时更晚），`notified_at` 在入队时就更新：比那条恢复通知新，说明「触发」已经在发了，按提醒间隔算
fn due(inc: &Open, severity: &str, now: OffsetDateTime) -> Option<Kind> {
    match inc.told_resolved_at.filter(|t| inc.notified_at.is_none_or(|n| n < *t)) {
        Some(t) => ((now - t).whole_seconds() >= REOPEN_QUIET_S).then_some(Kind::Firing),
        None => {
            let every = repeat_interval_s(&inc.rule, severity)?;
            ((now - inc.notified_at.unwrap_or(inc.opened_at)).whole_seconds() >= every).then_some(Kind::Repeat)
        }
    }
}

/// 恢复时要不要报「恢复」：上次恢复通知之后发过（入队过）触发/提醒才报。
/// 看入队时就刷新的 `notified_at`，不看 notify_log（送达后才写，触发还在队列里时查不到，恢复通知就被吞了）；
/// 同一渠道的队列先进先出，恢复排在触发之后
fn told_firing(inc: &Open) -> bool {
    inc.notified_at.is_some_and(|n| inc.told_resolved_at.is_none_or(|t| n > t))
}

/// 摘要里的数字（分钟数、代数）每次都在变，不算「摘要变了」
fn same_summary(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.chars().filter(|c| !c.is_ascii_digit()).collect::<String>();
    a == b || norm(a) == norm(b)
}

/// 持续提醒的间隔：续费类每天一次，其它只有严重告警每 4 小时一次
fn repeat_interval_s(rule: &str, severity: &str) -> Option<i64> {
    if rule.starts_with("renewal_") {
        Some(REPEAT_RENEWAL_S)
    } else if severity == "critical" {
        Some(REPEAT_CRITICAL_S)
    } else {
        None
    }
}

/// 评估一轮。`now` 由调用方给（测试可以快进去抖时间）。
pub async fn evaluate_once(st: &AppState, state: &mut AlertState, now: OffsetDateTime) -> anyhow::Result<()> {
    // 与列表、实时指标共用在用机器视图（最多旧 3 s）
    let ms = views::live(st).await?;
    let silences: Vec<(Option<Uuid>, Option<String>)> = sqlx::query_as(
        "select subject_id, rule from silence where deleted_at is null and now() between starts_at and ends_at",
    )
    .fetch_all(&st.db)
    .await?;
    let silenced = |rule: &str, id: Uuid| {
        silences.iter().any(|(s, r)| s.is_none_or(|s| s == id) && r.as_deref().is_none_or(|r| r == rule))
    };
    // ipintel_ 开头的规则由 ipintel.rs 自己开和关（一次性异常，24 小时后恢复），这里不碰，否则会被当成「原因消失」恢复掉
    let mut open: HashMap<(String, Uuid), Open> = sqlx::query_as::<_, Open>(&format!(
        "select {OPEN_COLS} from incident where state = 'firing' and rule not like 'ipintel!_%' escape '!'"
    ))
    .fetch_all(&st.db)
    .await?
    .into_iter()
    .map(|o| ((o.rule.clone(), o.subject_id), o))
    .collect();

    let open_ids: HashSet<Uuid> = open.values().map(|o| o.id).collect();
    let mut current: HashSet<(String, Uuid)> = HashSet::new();
    let mut changed = false;
    let mut touch: Vec<Uuid> = vec![];
    for m in ms.iter() {
        for r in &m.health.reasons {
            // info 级只用于界面提示（如访问策略暂停新连接），不开告警
            if r.severity == "info" {
                continue;
            }
            let k = (r.key.to_string(), m.id);
            current.insert(k.clone());
            let since = *state.first_seen.entry(k.clone()).or_insert(now);
            let summary = format!("{}：{}", m.name, r.text);
            let notice = |kind, id, opened_at| Notice {
                kind,
                incident_id: Some(id),
                rule: r.key.to_string(),
                severity: r.severity.to_string(),
                machine: m.name.clone(),
                summary: r.text.clone(),
                at: now,
                opened_at: Some(opened_at),
            };

            if let Some(inc) = open.remove(&k) {
                // 已在告警：同步最新严重度/摘要；只有严重度变了或摘要实质变了才通知前端刷新
                state.clean_since.remove(&inc.id);
                if inc.severity != r.severity || inc.summary != summary {
                    sqlx::query("update incident set last_seen_at = now(), summary = $2, severity = $3::severity where id = $1")
                        .bind(inc.id)
                        .bind(&summary)
                        .bind(r.severity)
                        .execute(&st.db)
                        .await?;
                    changed |= inc.severity != r.severity || !same_summary(&inc.summary, &summary);
                } else {
                    touch.push(inc.id);
                }
                // 持续提醒；复发后已报过恢复的，满限流时长再发一次「触发」
                if let Some(kind) = due(&inc, r.severity, now).filter(|_| !silenced(r.key, m.id)) {
                    notify::enqueue(st, notice(kind, inc.id, inc.opened_at)).await;
                }
                continue;
            }

            let immediate = r.key.starts_with("renewal_");
            if (!immediate && (now - since).whole_seconds() < debounce_s(r.key)) || silenced(r.key, m.id) {
                continue;
            }
            // 窗口内刚恢复过：重开原告警（清掉确认），按 `due` 决定是否马上通知（退役等流程关掉的、带 closed_by 的不重开）。
            // 重开与新建都要求机器此刻仍在用：视图是稍早取的，退役事务若在那之后提交（发起退役时已关掉它的告警），
            // 不能再给退役中的机器开一条永远不会恢复的告警。for share：退役事务还没提交时等它提交，再按新的 lifecycle 判断
            let reopened = sqlx::query_as::<_, Open>(&format!(
                "update incident set state = 'firing', resolved_at = null, ack_at = null, ack_by = null, reopen_count = reopen_count + 1, last_seen_at = now(),
                     severity = $3::severity, summary = $4
                 where id = (select id from incident where rule = $1 and subject_id = $2 and state = 'resolved' and resolved_at > $5 and not detail ? 'closed_by'
                             order by resolved_at desc limit 1)
                   and exists (select 1 from machine where id = $2 and lifecycle = 'active' for share)
                 returning {OPEN_COLS}"
            ))
            .bind(r.key)
            .bind(m.id)
            .bind(r.severity)
            .bind(&summary)
            .bind(now - time::Duration::seconds(REOPEN_WINDOW_S))
            .fetch_optional(&st.db)
            .await?;
            if let Some(inc) = reopened {
                changed = true;
                if let Some(kind) = due(&inc, r.severity, now) {
                    notify::enqueue(st, notice(kind, inc.id, inc.opened_at)).await;
                }
                continue;
            }
            let id = crate::db::new_id();
            let res = sqlx::query(
                "insert into incident (id, rule, subject_id, severity, summary, detail)
                 select $1, $2, $3, $4::severity, $5, $6 where exists (select 1 from machine where id = $3 and lifecycle = 'active' for share)
                 on conflict (rule, subject_id) where state = 'firing' do nothing",
            )
            .bind(id)
            .bind(r.key)
            .bind(m.id)
            .bind(r.severity)
            .bind(&summary)
            .bind(json!({"name": m.name}))
            .execute(&st.db)
            .await?;
            if res.rows_affected() > 0 {
                changed = true;
                notify::enqueue(st, notice(Kind::Firing, id, now)).await;
            }
        }
    }
    state.first_seen.retain(|k, _| current.contains(k));
    // 原因没变的在告警只在 last_seen_at 旧了 5 分钟以上才刷新：每 30 秒把所有在告警的行都重写一遍只会产生死元组，
    // 界面上的「最近仍在」差几分钟无所谓
    if !touch.is_empty() {
        sqlx::query("update incident set last_seen_at = now() where id = any($1) and last_seen_at < now() - interval '5 minutes'")
            .bind(&touch)
            .execute(&st.db)
            .await?;
    }

    // 原因消失的告警恢复：只处理 active 的机器。其它生命周期 evaluate 不给原因，不能当成「恢复」——
    // 退役中的在发起退役时已经关掉（closed_by = retire，见 api/machines.rs retire）。
    // `open` 里剩下的就是这一轮没有对应原因的告警。
    let active: HashMap<Uuid, (&str, bool)> =
        ms.iter().filter(|m| m.lifecycle == "active").map(|m| (m.id, (m.name.as_str(), m.health.probe_unknown))).collect();
    for (_, inc) in open {
        let Some(&(name, probe_unknown)) = active.get(&inc.subject_id) else { continue };
        // 探测结果过期（线路机 agent 离线、探测停了）：出口 / TCP 的原因消失不等于恢复，保持原状，
        // 等有新探测再判（去抖从那时重新算），否则线路机一离线这些告警就成批假恢复、补传后又重开
        if probe_unknown && matches!(inc.rule.as_str(), "egress_fail" | "tcp_fail") {
            state.clean_since.remove(&inc.id);
            continue;
        }
        let since = *state.clean_since.entry(inc.id).or_insert(now);
        if !inc.rule.starts_with("renewal_") && (now - since).whole_seconds() < debounce_s(&inc.rule) {
            continue;
        }
        state.clean_since.remove(&inc.id);
        sqlx::query("update incident set state = 'resolved', resolved_at = now() where id = $1").bind(inc.id).execute(&st.db).await?;
        changed = true;
        // 恢复通知：上次恢复通知之后发过触发/提醒、且当前没被静默时才发
        if !silenced(&inc.rule, inc.subject_id) && told_firing(&inc) {
            notify::enqueue(
                st,
                Notice {
                    kind: Kind::Resolved,
                    incident_id: Some(inc.id),
                    rule: inc.rule.clone(),
                    severity: inc.severity.clone(),
                    machine: name.to_string(),
                    summary: notify::strip_machine(&inc.summary, name).to_string(),
                    at: now,
                    opened_at: Some(inc.opened_at),
                },
            )
            .await;
        }
    }
    // 已经不在告警的计数清掉
    state.clean_since.retain(|id, _| open_ids.contains(id));
    if changed {
        st.emit(Event::Incidents);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_digits_do_not_count_as_change() {
        assert!(same_summary("团队1-01：5 分钟没有上报", "团队1-01：6 分钟没有上报"));
        assert!(same_summary("线路机2：配置未应用（配置代数 3 · 已应用 2）", "线路机2：配置未应用（配置代数 4 · 已应用 2）"));
        assert!(!same_summary("团队1-01：5 分钟没有上报", "团队1-01：从未上报"));
    }

    #[test]
    fn reopen_firing_not_resent_while_in_flight() {
        let now = OffsetDateTime::now_utc();
        let ago = |s: i64| now - time::Duration::seconds(s);
        let inc = |notified: Option<OffsetDateTime>, resolved: OffsetDateTime| Open {
            id: Uuid::nil(),
            rule: "relay_xray_down".into(),
            subject_id: Uuid::nil(),
            severity: "critical".into(),
            summary: String::new(),
            opened_at: ago(3600),
            notified_at: notified,
            told_resolved_at: Some(resolved),
        };
        // 恢复通知 10 分钟前，之后没再发过：发「触发」
        assert!(matches!(due(&inc(Some(ago(900)), ago(600)), "critical", now), Some(Kind::Firing)));
        // 「触发」30 s 前已开始发送、还没写进 notify_log（最近一条仍是恢复）：不重发
        assert!(due(&inc(Some(ago(30)), ago(600)), "critical", now).is_none());
    }

    #[test]
    fn resolved_notice_follows_queued_firing() {
        let now = OffsetDateTime::now_utc();
        let ago = |s: i64| now - time::Duration::seconds(s);
        let inc = |notified: Option<OffsetDateTime>, resolved: Option<OffsetDateTime>| Open {
            id: Uuid::nil(),
            rule: "relay_xray_down".into(),
            subject_id: Uuid::nil(),
            severity: "critical".into(),
            summary: String::new(),
            opened_at: ago(3600),
            notified_at: notified,
            told_resolved_at: resolved,
        };
        // 从没通知过（如开告警时被静默）：不报恢复
        assert!(!told_firing(&inc(None, None)));
        // 触发已入队（notify_log 里可能还没有）：报恢复
        assert!(told_firing(&inc(Some(ago(30)), None)));
        // 报过恢复后复发、又入队了触发：报恢复
        assert!(told_firing(&inc(Some(ago(30)), Some(ago(600)))));
        // 最近一条是恢复、之后没再入队触发：不重复报
        assert!(!told_firing(&inc(Some(ago(900)), Some(ago(600)))));
    }

    #[test]
    fn probe_rules_debounce_longer() {
        assert_eq!(debounce_s("egress_fail"), 150);
        assert_eq!(debounce_s("tcp_fail"), 90);
        assert_eq!(debounce_s("relay_xray_down"), 60);
        assert_eq!(debounce_s("agent_offline"), 60);
    }

    #[test]
    fn repeat_intervals() {
        assert_eq!(repeat_interval_s("egress_fail", "critical"), Some(4 * 3600));
        assert_eq!(repeat_interval_s("relay_apply_lag", "warning"), None);
        assert_eq!(repeat_interval_s("renewal_due", "warning"), Some(24 * 3600));
        assert_eq!(repeat_interval_s("renewal_overdue", "critical"), Some(24 * 3600));
    }
}
