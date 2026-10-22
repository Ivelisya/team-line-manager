//! 告警对齐（中控自身 selfmon.rs、软路由 routers/alerts.rs、来源画像 ipintel.rs 共用）：把此刻成立的问题对齐到 incident 表——
//! 开（被静默的不开）、摘要实质变了才改、严重度只升不降（升了马上再发一次「触发」）、开的时候没能入队的补发「触发」、
//! 按 `alerts::repeat_interval_s` 发提醒、不再成立的由调用方决定留着 / 恢复（报过触发、没被静默的发「恢复」）/ 悄悄关掉。
//! 摘要没变的「最近仍在」最多 5 分钟刷新一次（同机器的告警）。机器的告警（alerts.rs）另有复发重开与限流，不走这里。

use crate::alerts::{rank, repeat_interval_s, same_summary};
use crate::notify::{self, Kind, Notice};
use crate::silence::Silences;
use crate::state::{AppState, Event};
use serde_json::{json, Value};
use std::collections::HashMap;
use time::OffsetDateTime;
use uuid::Uuid;

/// 一条在告警的
#[derive(sqlx::FromRow)]
pub struct Firing {
    pub id: Uuid,
    pub rule: String,
    pub subject_id: Uuid,
    severity: String,
    summary: String,
    detail: Value,
    opened_at: OffsetDateTime,
    /// 最近一次把触发 / 提醒写进发送队列的时间；null = 开的时候入队失败
    notified_at: Option<OffsetDateTime>,
}

/// 规则以 `<prefix>_` 开头的在告警的
pub async fn firing(db: &sqlx::PgPool, prefix: &str) -> sqlx::Result<Vec<Firing>> {
    sqlx::query_as(
        "select id, rule, subject_id, severity::text severity, summary, detail, opened_at, notified_at from incident
         where state = 'firing' and rule like $1 escape '!'",
    )
    .bind(format!("{prefix}!_%"))
    .fetch_all(db)
    .await
}

/// 此刻成立的一个问题
pub struct Want<'a> {
    pub rule: &'a str,
    pub subject: Uuid,
    /// 对象名：摘要的前缀（「名字：」）、通知里的对象
    pub name: &'a str,
    pub severity: &'a str,
    /// 不带前缀的摘要
    pub text: String,
    /// 开、改摘要时写进 detail；None = 不写
    pub detail: Option<Value>,
    /// 还没开的这一轮能不能开（去抖满了）
    pub ready: bool,
}

/// 不再成立的告警这一轮怎么办
pub enum Close<'a> {
    /// 先留着（恢复去抖还没满、无从判断）
    Keep,
    /// 恢复；报过触发、没被静默的发「恢复」，对象名是这个
    Resolve(&'a str),
    /// 关掉、不发恢复，detail 记上 `closed_by`
    Quiet(&'static str),
}

/// 这一轮开、改、关了多少条
#[derive(Default)]
pub struct Tally {
    pub opened: usize,
    pub updated: usize,
    pub resolved: usize,
}

/// 对齐一轮。`open`：`firing` 读出来的；`exists`：新开时对象还在的 SQL 条件（`$3` 是对象 id，不用查就是 `true`）；
/// `close`：`want` 里没有的那些怎么办。有变化时发 `Event::Incidents`
pub async fn reconcile<'n>(
    st: &AppState,
    open: Vec<Firing>,
    want: Vec<Want<'_>>,
    exists: &str,
    now: OffsetDateTime,
    mut close: impl FnMut(&Firing) -> Close<'n>,
) -> anyhow::Result<Tally> {
    let silences = Silences::load(&st.db).await?;
    let mut open: HashMap<(String, Uuid), Firing> = open.into_iter().map(|o| ((o.rule.clone(), o.subject_id), o)).collect();
    let notice = |kind, id, rule: &str, severity: &str, name: &str, text: &str, opened_at| Notice {
        kind,
        incident_id: Some(id),
        rule: rule.into(),
        severity: severity.into(),
        machine: name.into(),
        summary: text.into(),
        at: now,
        opened_at: Some(opened_at),
    };
    let mut t = Tally::default();
    let mut touch: Vec<Uuid> = vec![];
    for w in want {
        let summary = format!("{}：{}", w.name, w.text);
        let silenced = silences.matches(w.rule, w.subject);
        if let Some(inc) = open.remove(&(w.rule.to_string(), w.subject)) {
            let escalated = rank(w.severity) > rank(&inc.severity);
            let sev = if escalated { w.severity } else { inc.severity.as_str() };
            if escalated || !same_summary(&inc.summary, &summary) || w.detail.as_ref().is_some_and(|d| *d != inc.detail) {
                sqlx::query("update incident set summary = $2, severity = $3::severity, detail = coalesce($4, detail), last_seen_at = now() where id = $1")
                    .bind(inc.id)
                    .bind(&summary)
                    .bind(sev)
                    .bind(&w.detail)
                    .execute(&st.db)
                    .await?;
                t.updated += 1;
            } else {
                touch.push(inc.id);
            }
            let kind = match inc.notified_at {
                _ if escalated => Some(Kind::Firing),
                None => Some(Kind::Firing),
                Some(n) => repeat_interval_s(w.rule, sev).filter(|every| (now - n).whole_seconds() >= *every).map(|_| Kind::Repeat),
            };
            if let Some(kind) = kind.filter(|_| !silenced) {
                notify::enqueue(st, notice(kind, inc.id, w.rule, sev, w.name, &w.text, inc.opened_at)).await;
            }
            continue;
        }
        if !w.ready || silenced {
            continue;
        }
        let id = crate::db::new_id();
        let res = sqlx::query(&format!(
            "insert into incident (id, rule, subject_id, severity, summary, detail)
             select $1, $2, $3, $4::severity, $5, coalesce($6, '{{}}') where {exists}
             on conflict (rule, subject_id) where state = 'firing' do nothing"
        ))
        .bind(id)
        .bind(w.rule)
        .bind(w.subject)
        .bind(w.severity)
        .bind(&summary)
        .bind(&w.detail)
        .execute(&st.db)
        .await?;
        if res.rows_affected() > 0 {
            t.opened += 1;
            notify::enqueue(st, notice(Kind::Firing, id, w.rule, w.severity, w.name, &w.text, now)).await;
        }
    }
    if !touch.is_empty() {
        sqlx::query("update incident set last_seen_at = now() where id = any($1) and last_seen_at < now() - interval '5 minutes'").bind(&touch).execute(&st.db).await?;
    }
    for inc in open.into_values() {
        let (closed_by, name) = match close(&inc) {
            Close::Keep => continue,
            Close::Resolve(name) => (json!({}), Some(name)),
            Close::Quiet(by) => (json!({"closed_by": by}), None),
        };
        sqlx::query("update incident set state = 'resolved', resolved_at = now(), detail = detail || $2 where id = $1 and state = 'firing'")
            .bind(inc.id)
            .bind(closed_by)
            .execute(&st.db)
            .await?;
        t.resolved += 1;
        if let Some(name) = name.filter(|_| inc.notified_at.is_some() && !silences.matches(&inc.rule, inc.subject_id)) {
            let text = notify::strip_machine(&inc.summary, name);
            notify::enqueue(st, notice(Kind::Resolved, inc.id, &inc.rule, &inc.severity, name, text, inc.opened_at)).await;
        }
    }
    if t.opened + t.updated + t.resolved > 0 {
        st.emit(Event::Incidents);
    }
    Ok(t)
}
