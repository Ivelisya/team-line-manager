//! 告警对齐（机器 alerts.rs、中控自身 selfmon.rs、软路由 routers/alerts.rs、探测点 probe_points/、来源画像 ipintel.rs 共用）：把此刻成立的问题对齐到 incident 表——
//! 开（被静默的不开）、摘要实质变了才改、严重度只升不降（升了马上再发一次「触发」、清掉之前的确认）、开的时候没能入队的补发「触发」、
//! 按 `alerts::repeat_interval_s` 发提醒、不再成立的由调用方决定留着 / 恢复 / 悄悄关掉。
//! 摘要没变（或只有数字在变）的「最近仍在」最多 5 分钟刷新一次，摘要里的数字随之更新。
//!
//! 复发与限流（判断函数在 alerts.rs）：
//! - 恢复后 30 分钟内同一 (规则, 对象) 又成立（`ready`）：重开原告警（`reopen_count` +1、清掉确认），不新建；悄悄关掉的（带 closed_by：对象删了、停用了、来源画像过期）不重开；
//! - 重开时已报过恢复的，距那条恢复通知满 5 分钟才发「触发」（不满则等满 5 分钟仍在触发再发），恢复只在上次恢复通知之后发过触发 / 提醒时才报：
//!   反复抖动的告警每 5 分钟最多一对触发 / 恢复，真故障最多晚 5 分钟报。被静默的不发任何通知（已报过触发的告警恢复时照报「恢复」，维护窗口内不报）；已确认的告警不再发「提醒」。
//!
//! 维护窗口（软路由、探测点；全局维护也算）同机器：开着的告警保持原状（不改、不恢复、不发通知），不开新的；
//! 维护结束后按那时的状态再判。对象删了、停用了这类悄悄关掉（`Close::Quiet`）的照关。

use crate::alerts::{due, rank, same_summary, told_firing, REOPEN_WINDOW_S, TOLD_RESOLVED_SQL};
use crate::notify::{self, Kind, Notice};
use crate::silence::Silences;
use crate::state::{AppState, Event};
use serde_json::{json, Value};
use std::collections::HashMap;
use time::OffsetDateTime;
use uuid::Uuid;

/// 告警引擎：每条 incident 属于写它的那个引擎（`incident.engine`），各引擎只读、只开、只关自己的，规则名随便取，不靠前缀分地盘。
/// 新加引擎在这里加一项即可
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Engine {
    /// 机器告警（alerts.rs，按 health::evaluate 的原因）
    Machine,
    /// 来源画像异常（ipintel.rs）
    Ipintel,
    /// 中控自身（selfmon.rs）
    Central,
    /// 软路由（routers/alerts.rs）
    Router,
    /// 探测点（probe_points/）
    Probe,
}

impl Engine {
    /// 这个引擎的对象能开维护窗口、维护中冻结开着的告警（机器的维护由 alerts.rs 自己处理；中控、来源画像没有维护的说法，维护对它们只是静默）
    const fn has_maintenance(self) -> bool { matches!(self, Engine::Router | Engine::Probe) }

    /// `incident.engine` 里存的值
    pub const fn as_str(self) -> &'static str {
        match self {
            Engine::Machine => "machine",
            Engine::Ipintel => "ipintel",
            Engine::Central => "central",
            Engine::Router => "router",
            Engine::Probe => "probe",
        }
    }
}

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
    /// 有人确认过（升级、复发重开会清掉）：不再发「提醒」
    ack_at: Option<OffsetDateTime>,
    /// 最近一条送达的通知是「恢复」时它的发送时间（复发重开后还没再通知过）
    told_resolved_at: Option<OffsetDateTime>,
}

/// 这个引擎在告警的
pub async fn firing(db: &sqlx::PgPool, engine: Engine) -> sqlx::Result<Vec<Firing>> {
    sqlx::query_as(&format!(
        "select id, rule, subject_id, severity::text severity, summary, detail, opened_at, notified_at, ack_at, {TOLD_RESOLVED_SQL} told_resolved_at from incident
         where state = 'firing' and engine = $1"
    ))
    .bind(engine.as_str())
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
    /// 开着，但不单独通知（原因已经由别的告警在报，如软路由出口不通是因为节点本身在告警）：不发触发 / 提醒，也就没有恢复通知。
    /// 开的时候没通知过，原因单独留下来（这个标记没了）时按「没入队过」补发触发
    pub quiet: bool,
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

/// 这一轮开（含重开）、改、关了多少条
#[derive(Default)]
pub struct Tally {
    pub opened: usize,
    pub updated: usize,
    pub resolved: usize,
}

/// 触发通知的摘要：近 24 小时同对象同规则出现过不止一次（开过的条数 + 30 分钟内复发合并进去的次数，同告警页的次数）时末尾加「（近 24 小时第 N 次）」；提醒和恢复不加
async fn chronic(st: &AppState, engine: Engine, rule: &str, subject: Uuid, kind: Kind, text: &str) -> String {
    if kind != Kind::Firing {
        return text.to_string();
    }
    let n: i64 = sqlx::query_scalar("select count(*) + coalesce(sum(reopen_count), 0) from incident where engine = $1 and rule = $2 and subject_id = $3 and opened_at > now() - interval '24 hours'")
        .bind(engine.as_str())
        .bind(rule)
        .bind(subject)
        .fetch_one(&st.db)
        .await
        .unwrap_or(0);
    if n > 1 { format!("{text}（近 24 小时第 {n} 次）") } else { text.to_string() }
}

/// 对齐一轮。`open`：`firing` 按同一个 `engine` 读出来的；`exists`：新开时对象还在的 SQL 条件（`$3` 是对象 id，不用查就是 `true`）；
/// `close`：`want` 里没有的那些怎么办。有变化时发 `Event::Incidents`
pub async fn reconcile<'n>(
    st: &AppState,
    engine: Engine,
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
    // 摘要只有数字（分钟数、代数）在变的：随「最近仍在」最多 5 分钟写一次
    let mut touch: Vec<(Uuid, String)> = vec![];
    let maintenance = |id: Uuid| engine.has_maintenance() && silences.maintenance(id);
    for w in want {
        // 维护中：开着的保持原状（从 open 里拿走，下面也不当成恢复），不开新的
        if maintenance(w.subject) {
            open.remove(&(w.rule.to_string(), w.subject));
            continue;
        }
        let summary = format!("{}：{}", w.name, w.text);
        let silenced = silences.matches(w.rule, w.subject);
        if let Some(inc) = open.remove(&(w.rule.to_string(), w.subject)) {
            let escalated = rank(w.severity) > rank(&inc.severity);
            let sev = if escalated { w.severity } else { inc.severity.as_str() };
            if escalated || !same_summary(&inc.summary, &summary) || w.detail.as_ref().is_some_and(|d| *d != inc.detail) {
                // 升级时清掉之前的确认（同机器的告警）：确认的是警告，严重是新情况
                sqlx::query(
                    "update incident set summary = $2, severity = $3::severity, detail = coalesce($4, detail), last_seen_at = now(),
                         ack_at = case when $5 then null else ack_at end, ack_by = case when $5 then null else ack_by end
                     where id = $1",
                )
                .bind(inc.id)
                .bind(&summary)
                .bind(sev)
                .bind(&w.detail)
                .bind(escalated)
                .execute(&st.db)
                .await?;
                t.updated += 1;
            } else {
                touch.push((inc.id, summary));
            }
            // 升级马上再发一次「触发」；否则按 `due`：重开后报过恢复的满 5 分钟发「触发」、没入队过的补发、到间隔的提醒
            let kind = if escalated { Some(Kind::Firing) } else { due(w.rule, sev, inc.notified_at, inc.told_resolved_at, inc.ack_at.is_some(), now) };
            if let Some(kind) = kind.filter(|_| !silenced && !w.quiet) {
                let text = chronic(st, engine, w.rule, w.subject, kind, &w.text).await;
                notify::enqueue(st, notice(kind, inc.id, w.rule, sev, w.name, &text, inc.opened_at)).await;
            }
            continue;
        }
        if !w.ready || silenced {
            continue;
        }
        // 窗口内刚恢复过（不是悄悄关掉的）：重开原告警（复发计数 +1、清掉确认），按 `due` 决定马上通知还是等满限流时长。对象也要还在（`exists`）
        let reopened: Option<(Uuid, OffsetDateTime, Option<OffsetDateTime>, Option<OffsetDateTime>)> = sqlx::query_as(&format!(
            "update incident set state = 'firing', resolved_at = null, ack_at = null, ack_by = null, reopen_count = reopen_count + 1, last_seen_at = now(),
                 severity = $2::severity, summary = $4, detail = coalesce($6, detail)
             where id = (select id from incident where engine = $7 and rule = $1 and subject_id = $3 and state = 'resolved' and resolved_at > $5 and not detail ? 'closed_by'
                         order by resolved_at desc limit 1)
               and {exists}
             returning id, opened_at, notified_at, {TOLD_RESOLVED_SQL} told_resolved_at"
        ))
        .bind(w.rule)
        .bind(w.severity)
        .bind(w.subject)
        .bind(&summary)
        .bind(now - time::Duration::seconds(REOPEN_WINDOW_S))
        .bind(&w.detail)
        .bind(engine.as_str())
        .fetch_optional(&st.db)
        .await?;
        if let Some((id, opened_at, notified_at, told_resolved_at)) = reopened {
            t.opened += 1;
            if let Some(kind) = due(w.rule, w.severity, notified_at, told_resolved_at, false, now).filter(|_| !w.quiet) {
                let text = chronic(st, engine, w.rule, w.subject, kind, &w.text).await;
                notify::enqueue(st, notice(kind, id, w.rule, w.severity, w.name, &text, opened_at)).await;
            }
            continue;
        }
        let id = crate::db::new_id();
        let res = sqlx::query(&format!(
            "insert into incident (id, engine, rule, subject_id, severity, summary, detail)
             select $1, $7, $2, $3, $4::severity, $5, coalesce($6, '{{}}') where {exists}
             on conflict (engine, rule, subject_id) where state = 'firing' do nothing"
        ))
        .bind(id)
        .bind(w.rule)
        .bind(w.subject)
        .bind(w.severity)
        .bind(&summary)
        .bind(&w.detail)
        .bind(engine.as_str())
        .execute(&st.db)
        .await?;
        if res.rows_affected() > 0 {
            t.opened += 1;
            if !w.quiet {
                let text = chronic(st, engine, w.rule, w.subject, Kind::Firing, &w.text).await;
                notify::enqueue(st, notice(Kind::Firing, id, w.rule, w.severity, w.name, &text, now)).await;
            }
        }
    }
    if !touch.is_empty() {
        let (ids, sums): (Vec<Uuid>, Vec<String>) = touch.into_iter().unzip();
        sqlx::query(
            "update incident set last_seen_at = now(), summary = u.s from unnest($1::uuid[], $2::text[]) u(id, s)
             where incident.id = u.id and incident.last_seen_at < now() - interval '5 minutes'",
        )
        .bind(&ids)
        .bind(&sums)
        .execute(&st.db)
        .await?;
    }
    for inc in open.into_values() {
        let (closed_by, name) = match close(&inc) {
            Close::Keep => continue,
            Close::Resolve(_) if maintenance(inc.subject_id) => continue,
            Close::Resolve(name) => (json!({}), Some(name)),
            Close::Quiet(by) => (json!({"closed_by": by}), None),
        };
        sqlx::query("update incident set state = 'resolved', resolved_at = now(), detail = detail || $2 where id = $1 and state = 'firing'")
            .bind(inc.id)
            .bind(closed_by)
            .execute(&st.db)
            .await?;
        t.resolved += 1;
        // 上次恢复通知之后发过触发 / 提醒才报恢复（复发后还没再通知过的不重复报）。报过触发的不看普通静默（静默期间恢复了也要给触发一个下文），维护窗口照旧不发
        if let Some(name) = name.filter(|_| told_firing(inc.notified_at, inc.told_resolved_at) && !silences.maintenance(inc.subject_id)) {
            let text = notify::strip_machine(&inc.summary, name);
            notify::enqueue(st, notice(Kind::Resolved, inc.id, &inc.rule, &inc.severity, name, text, inc.opened_at)).await;
        }
    }
    if t.opened + t.updated + t.resolved > 0 {
        st.emit(Event::Incidents);
    }
    Ok(t)
}
