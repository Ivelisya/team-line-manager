//! 告警：每 30 s 用 health::evaluate 的结果对齐 incident 表，并在触发/恢复时发通知。
//! 只看 active 的机器（其它生命周期 evaluate 不给原因），所以退役/上架中的机器天然不告警。
//!
//! 去抖（开、关对称）：
//! - 开：同一 (规则, 机器) 连续出现满 60 s 才开告警；续费类立即开。被静默的不开。
//! - 关：原因连续消失满 60 s 才恢复；续费类立即恢复（按日期判定，不会抖）。
//! - 复发：恢复后 30 分钟内同一 (规则, 机器) 又满足开的条件，重开原告警（`reopen_count` +1，清掉确认），不新建。
//!
//! 通知（见 notify.rs）：新告警发「触发」；仍在触发的严重告警每 4 小时提醒一次，续费类每天一次；
//! 原因消失恢复发「恢复」，只在上次恢复通知之后发过触发/提醒时才发（复发后没再通知过的不重复报恢复）。
//! 复发重开：已报过恢复的，距那条恢复通知满 5 分钟就立即发「触发」，不满则等满 5 分钟仍在触发再发，
//! 所以反复抖动的告警每 5 分钟最多一对触发/恢复，真故障最多晚 5 分钟报。被静默（含维护）的不发任何通知。

use crate::notify::{self, Kind, Notice};
use crate::state::{AppState, Event};
use crate::views::{self, Scope};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

/// 开、关去抖时长
const DEBOUNCE_S: i64 = 60;
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

/// 仍在触发的告警这一轮该发的通知：报过恢复的，距那条恢复通知满 REOPEN_QUIET_S 发「触发」；否则按持续提醒间隔发「提醒」
fn due(inc: &Open, severity: &str, now: OffsetDateTime) -> Option<Kind> {
    match inc.told_resolved_at {
        Some(t) => ((now - t).whole_seconds() >= REOPEN_QUIET_S).then_some(Kind::Firing),
        None => {
            let every = repeat_interval_s(&inc.rule, severity)?;
            ((now - inc.notified_at.unwrap_or(inc.opened_at)).whole_seconds() >= every).then_some(Kind::Repeat)
        }
    }
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
    // 告警只看健康判定，不要本月流量聚合
    let ms = views::load_with(&st.db, Scope::Live, None, false).await?;
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
    for m in &ms {
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
                    notify::spawn_dispatch(st, notice(kind, inc.id, inc.opened_at));
                }
                continue;
            }

            let immediate = r.key.starts_with("renewal_");
            if (!immediate && (now - since).whole_seconds() < DEBOUNCE_S) || silenced(r.key, m.id) {
                continue;
            }
            // 窗口内刚恢复过：重开原告警（清掉确认），按 `due` 决定是否马上通知（退役等流程关掉的、带 closed_by 的不重开）
            let reopened = sqlx::query_as::<_, Open>(&format!(
                "update incident set state = 'firing', resolved_at = null, ack_at = null, ack_by = null, reopen_count = reopen_count + 1, last_seen_at = now(),
                     severity = $3::severity, summary = $4
                 where id = (select id from incident where rule = $1 and subject_id = $2 and state = 'resolved' and resolved_at > $5 and not detail ? 'closed_by'
                             order by resolved_at desc limit 1)
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
                    notify::spawn_dispatch(st, notice(kind, inc.id, inc.opened_at));
                }
                continue;
            }
            let id = crate::db::new_id();
            let res = sqlx::query(
                "insert into incident (id, rule, subject_id, severity, summary, detail)
                 values ($1, $2, $3, $4::severity, $5, $6)
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
                notify::spawn_dispatch(st, notice(Kind::Firing, id, now));
            }
        }
    }
    state.first_seen.retain(|k, _| current.contains(k));
    if !touch.is_empty() {
        sqlx::query("update incident set last_seen_at = now() where id = any($1)").bind(&touch).execute(&st.db).await?;
    }

    // 原因消失的告警恢复（只处理仍在 live 列表里的机器；退役的由退役作业关闭）。
    // `open` 里剩下的就是这一轮没有对应原因的告警。
    let names: HashMap<Uuid, &str> = ms.iter().map(|m| (m.id, m.name.as_str())).collect();
    for (_, inc) in open {
        let Some(name) = names.get(&inc.subject_id) else { continue };
        let since = *state.clean_since.entry(inc.id).or_insert(now);
        if !inc.rule.starts_with("renewal_") && (now - since).whole_seconds() < DEBOUNCE_S {
            continue;
        }
        state.clean_since.remove(&inc.id);
        sqlx::query("update incident set state = 'resolved', resolved_at = now() where id = $1").bind(inc.id).execute(&st.db).await?;
        changed = true;
        // 恢复通知：上次恢复通知之后发过触发/提醒、且当前没被静默时才发
        if !silenced(&inc.rule, inc.subject_id) {
            let sent: bool = sqlx::query_scalar(
                "select exists (select 1 from notify_log where incident_id = $1 and kind in ('firing', 'repeat')
                 and ts > coalesce((select max(ts) from notify_log where incident_id = $1 and kind = 'resolved'), '-infinity'))",
            )
            .bind(inc.id)
            .fetch_one(&st.db)
            .await?;
            if sent {
                notify::spawn_dispatch(
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
                );
            }
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
        assert!(same_summary("线路机2：配置未应用（期望 3，已应用 2）", "线路机2：配置未应用（期望 4，已应用 2）"));
        assert!(!same_summary("团队1-01：5 分钟没有上报", "团队1-01：从未上报"));
    }

    #[test]
    fn repeat_intervals() {
        assert_eq!(repeat_interval_s("egress_fail", "critical"), Some(4 * 3600));
        assert_eq!(repeat_interval_s("relay_apply_lag", "warning"), None);
        assert_eq!(repeat_interval_s("renewal_due", "warning"), Some(24 * 3600));
        assert_eq!(repeat_interval_s("renewal_overdue", "critical"), Some(24 * 3600));
    }
}
