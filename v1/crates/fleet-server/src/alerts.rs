//! 告警：每 30 s 用 health::evaluate 的结果对齐 incident 表。
//! 只看 active 的机器（其它生命周期 evaluate 不给原因），所以退役/上架中的机器天然不告警。
//! 去抖：同一 (规则, 机器) 连续出现满 60 s 才开告警；续费类立即开。被静默的不开。

use crate::state::{AppState, Event};
use crate::views::{self, Scope};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

const DEBOUNCE_S: i64 = 60;

pub async fn run_forever(st: AppState) {
    let mut first_seen: HashMap<(String, Uuid), OffsetDateTime> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    loop {
        tick.tick().await;
        if let Err(e) = evaluate_once(&st, &mut first_seen).await {
            tracing::error!("告警评估失败：{e:#}");
        }
    }
}

async fn evaluate_once(st: &AppState, first_seen: &mut HashMap<(String, Uuid), OffsetDateTime>) -> anyhow::Result<()> {
    let now = OffsetDateTime::now_utc();
    let ms = views::load(&st.db, Scope::Live, None).await?;
    let silences: Vec<(Option<Uuid>, Option<String>)> = sqlx::query_as(
        "select subject_id, rule from silence where deleted_at is null and now() between starts_at and ends_at",
    )
    .fetch_all(&st.db)
    .await?;
    let silenced = |rule: &str, id: Uuid| {
        silences.iter().any(|(s, r)| s.is_none_or(|s| s == id) && r.as_deref().is_none_or(|r| r == rule))
    };

    let mut current: HashSet<(String, Uuid)> = HashSet::new();
    let mut changed = false;
    for m in &ms {
        for r in &m.health.reasons {
            let k = (r.key.to_string(), m.id);
            current.insert(k.clone());
            let since = *first_seen.entry(k).or_insert(now);
            let immediate = r.key.starts_with("renewal_");
            if (!immediate && (now - since).whole_seconds() < DEBOUNCE_S) || silenced(r.key, m.id) {
                continue;
            }
            let res = sqlx::query(
                "insert into incident (id, rule, subject_id, severity, summary, detail)
                 values ($1, $2, $3, $4::severity, $5, $6)
                 on conflict (rule, subject_id) where state = 'firing'
                 do update set last_seen_at = now(), summary = excluded.summary, severity = excluded.severity",
            )
            .bind(crate::db::new_id())
            .bind(r.key)
            .bind(m.id)
            .bind(r.severity)
            .bind(format!("{}：{}", m.name, r.text))
            .bind(json!({"name": m.name}))
            .execute(&st.db)
            .await?;
            changed |= res.rows_affected() > 0;
        }
    }
    first_seen.retain(|k, _| current.contains(k));

    // 原因消失的告警恢复（只处理仍在 live 列表里的机器；退役的由退役作业关闭）
    let live: Vec<Uuid> = ms.iter().map(|m| m.id).collect();
    let open: Vec<(Uuid, String, Uuid)> =
        sqlx::query_as("select id, rule, subject_id from incident where state = 'firing' and subject_id = any($1)").bind(&live).fetch_all(&st.db).await?;
    for (id, rule, subject) in open {
        if !current.contains(&(rule, subject)) {
            sqlx::query("update incident set state = 'resolved', resolved_at = now() where id = $1").bind(id).execute(&st.db).await?;
            changed = true;
        }
    }
    if changed {
        st.emit(Event::Incidents);
    }
    Ok(())
}
