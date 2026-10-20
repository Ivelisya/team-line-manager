//! 告警：每 30 s 用 health::evaluate 的结果对齐 incident 表，并在触发/恢复时发通知。
//! 只看 active 的机器（其它生命周期 evaluate 不给原因），所以退役/上架中的机器天然不告警。
//!
//! 去抖：
//! - 开：同一 (规则, 机器) 连续出现满 60 s 才开告警；续费类立即开。被静默的不开。
//! - 关：原因连续 2 个评估周期（约 60 s）都没有了才恢复，避免探测抖动造成开-关-开。
//!
//! 通知（见 notify.rs）：新告警发「触发」；原因消失恢复发「恢复」（只发给收到过触发/提醒的渠道，
//! 即 notify_log 里有这条告警的触发记录）；仍在触发的严重告警每 4 小时提醒一次，续费类每天一次；
//! 被静默（含维护）的不发任何通知。

use crate::notify::{self, Kind, Notice};
use crate::state::{AppState, Event};
use crate::views::{self, Scope};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

const DEBOUNCE_S: i64 = 60;
/// 连续这么多个干净周期才恢复
const CLEAN_CYCLES: u32 = 2;
/// 严重告警持续触发的提醒间隔
const REPEAT_CRITICAL_S: i64 = 4 * 3600;
/// 续费类提醒间隔（每天最多一次）
const REPEAT_RENEWAL_S: i64 = 24 * 3600;

/// 评估循环在两次之间要记住的东西
#[derive(Default)]
pub struct AlertState {
    first_seen: HashMap<(String, Uuid), OffsetDateTime>,
    /// incident id → 已连续干净的周期数
    clean: HashMap<Uuid, u32>,
}

pub async fn run_forever(st: AppState) {
    let mut state = AlertState::default();
    let mut tick = tokio::time::interval(Duration::from_secs(30));
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
    let mut open: HashMap<(String, Uuid), Open> = sqlx::query_as::<_, Open>(
        "select id, rule, subject_id, severity::text severity, summary, opened_at, notified_at from incident where state = 'firing'",
    )
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
            let k = (r.key.to_string(), m.id);
            current.insert(k.clone());
            let since = *state.first_seen.entry(k.clone()).or_insert(now);
            let summary = format!("{}：{}", m.name, r.text);

            if let Some(inc) = open.remove(&k) {
                // 已在告警：同步最新严重度/摘要；只有严重度变了或摘要实质变了才通知前端刷新
                state.clean.remove(&inc.id);
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
                // 持续提醒
                if let Some(every) = repeat_interval_s(r.key, r.severity) {
                    let last = inc.notified_at.unwrap_or(inc.opened_at);
                    if (now - last).whole_seconds() >= every && !silenced(r.key, m.id) {
                        notify::spawn_dispatch(
                            st,
                            Notice {
                                kind: Kind::Repeat,
                                incident_id: Some(inc.id),
                                rule: r.key.to_string(),
                                severity: r.severity.to_string(),
                                machine: m.name.clone(),
                                summary: r.text.clone(),
                                at: now,
                                opened_at: Some(inc.opened_at),
                            },
                        );
                    }
                }
                continue;
            }

            let immediate = r.key.starts_with("renewal_");
            if (!immediate && (now - since).whole_seconds() < DEBOUNCE_S) || silenced(r.key, m.id) {
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
                notify::spawn_dispatch(
                    st,
                    Notice {
                        kind: Kind::Firing,
                        incident_id: Some(id),
                        rule: r.key.to_string(),
                        severity: r.severity.to_string(),
                        machine: m.name.clone(),
                        summary: r.text.clone(),
                        at: now,
                        opened_at: Some(now),
                    },
                );
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
        let n = state.clean.entry(inc.id).or_insert(0);
        *n += 1;
        if *n < CLEAN_CYCLES {
            continue;
        }
        state.clean.remove(&inc.id);
        sqlx::query("update incident set state = 'resolved', resolved_at = now() where id = $1").bind(inc.id).execute(&st.db).await?;
        changed = true;
        // 恢复通知：只在发过触发/提醒、且当前没被静默时发
        if !silenced(&inc.rule, inc.subject_id) {
            let sent: bool = sqlx::query_scalar("select exists (select 1 from notify_log where incident_id = $1 and kind in ('firing', 'repeat'))")
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
    state.clean.retain(|id, _| open_ids.contains(id));
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
