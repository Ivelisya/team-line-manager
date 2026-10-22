//! 告警：每 30 s 用 health::evaluate 的结果对齐 incident 表，并在触发/恢复时发通知。
//! 只看 active 的机器（其它生命周期 evaluate 不给原因），所以退役/上架中的机器天然不告警。
//!
//! 去抖（开、关对称）：
//! - 开：同一 (规则, 机器) 连续出现满去抖时长才开告警（出口不通 150 s、TCP 不通 90 s、其余 60 s）；续费类与流量额度立即开。被静默的不开。
//! - 关：原因连续消失满同样的时长才恢复；续费类与流量额度立即恢复（按日期、小时汇总判定，不会抖）。
//! - 复发：恢复后 30 分钟内同一 (规则, 机器) 又满足开的条件，重开原告警（`reopen_count` +1，清掉确认），不新建。
//!
//! 通知（见 notify.rs）：新告警发「触发」；严重度升到严重（流量额度从吃紧到超限）马上再发一次「触发」；
//! 仍在触发的严重告警每 4 小时提醒一次，续费类与流量额度每天一次；
//! 原因消失恢复发「恢复」，只在上次恢复通知之后发过触发/提醒时才发（复发后没再通知过的不重复报恢复）；
//! 某个渠道没收到这条的触发（发送放弃了、渠道是后来加的），发送时不给它发恢复（notify.rs `got_firing`）。
//! 复发重开：已报过恢复的，距那条恢复通知满 5 分钟就立即发「触发」，不满则等满 5 分钟仍在触发再发，
//! 所以反复抖动的告警每 5 分钟最多一对触发/恢复，真故障最多晚 5 分钟报。被静默（含维护）的不发任何通知。
//! 「发过」= 写进了库里的发送队列（notify_outbox，`notified_at` 同一事务刷新）：之后由发送循环重试到送达或放弃，中控重启也不丢。

use crate::notify::{self, Kind, Notice};
use crate::selfmon;
use crate::silence::Silences;
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
/// 续费类与流量额度的提醒间隔（每天最多一次）
const REPEAT_SLOW_S: i64 = 24 * 3600;

/// 严重度的高低（同库里 severity 枚举的顺序）
fn rank(severity: &str) -> u8 {
    match severity {
        "critical" => 2,
        "warning" => 1,
        _ => 0,
    }
}

/// 续费类与流量额度：按日期、小时汇总判定，不会抖——开、关都不去抖，持续提醒每天最多一次
fn slow(rule: &str) -> bool {
    rule.starts_with("renewal_") || rule == "traffic_quota_high"
}

/// 全网同时失联至少要这么多台 agent 离线（另要超过一半）
const MASS_OFFLINE_MIN: usize = 3;

/// 有自己 agent 的在用机器（不含维护中的、同机 IP 节点）里同时离线的超过一半、且至少 `MASS_OFFLINE_MIN` 台时给出 (离线, 总数)：
/// 多半是 agent 到不了中控（边缘机、Caddy、DNS），由 selfmon.rs 开一条中控侧的告警，不按单机各报一条 agent 离线
pub fn agents_unreachable(ms: &[views::MachineView]) -> Option<(usize, usize)> {
    let online: Vec<bool> = ms
        .iter()
        .filter(|m| m.lifecycle == "active" && !m.maintenance && m.host_machine.is_none())
        .filter_map(|m| m.agent.as_ref().map(|a| a.online))
        .collect();
    let off = online.iter().filter(|o| !**o).count();
    (off >= MASS_OFFLINE_MIN && off * 2 > online.len()).then_some((off, online.len()))
}

/// 评估循环在两次之间要记住的东西
#[derive(Default)]
pub struct AlertState {
    first_seen: HashMap<(String, Uuid), OffsetDateTime>,
    /// incident id → 原因从什么时候起一直没有
    clean_since: HashMap<Uuid, OffsetDateTime>,
}

/// 每 30 秒：记下中控还在跑（views::ReportGaps::alive），评估机器的告警，成功了 ping 死人开关；再检查中控自身（selfmon.rs）
pub async fn run_forever(st: AppState) {
    let mut state = AlertState::default();
    let mut heartbeat = selfmon::Heartbeat::default();
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let now = OffsetDateTime::now_utc();
        if let Err(e) = st.report_gaps.alive(&st.db, now).await {
            tracing::warn!("记录中控存活时刻失败：{e}");
        }
        match evaluate_once(&st, &mut state, now).await {
            Ok(()) => heartbeat.beat(&st),
            Err(e) => tracing::error!("告警评估失败：{e:#}"),
        }
        if let Err(e) = selfmon::check_once(&st, now).await {
            tracing::error!("中控自检失败：{e:#}");
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
    /// 最近一次把触发 / 提醒写进发送队列的时间（notify.rs `enqueue`）；null = 还没写进去过（开告警时入队失败）
    notified_at: Option<OffsetDateTime>,
    /// 最近一条送达的通知是「恢复」时它的发送时间（复发重开后还没再通知过）。只看送达的：恢复没发出去，渠道那边这条告警还没结束
    told_resolved_at: Option<OffsetDateTime>,
}

/// `Open` 的列（查询和重开的 returning 共用）
const OPEN_COLS: &str = "id, rule, subject_id, severity::text severity, summary, opened_at, notified_at,
    (select case when l.kind = 'resolved' then l.ts end from notify_log l where l.incident_id = incident.id and l.ok and l.kind in ('firing', 'repeat', 'resolved')
     order by l.ts desc, l.id desc limit 1) told_resolved_at";

/// 仍在触发的告警这一轮该发的通知：报过恢复的，距那条恢复通知满 REOPEN_QUIET_S 发「触发」；还没入队过（开告警时入队失败）的补发「触发」；
/// 否则按持续提醒间隔发「提醒」。notify_log 在送达后才写（渠道失败重试时更晚），`notified_at` 在入队时就更新：
/// 比那条恢复通知新，说明「触发」已经在发了，按提醒间隔算
fn due(inc: &Open, severity: &str, now: OffsetDateTime) -> Option<Kind> {
    match (inc.told_resolved_at.filter(|t| inc.notified_at.is_none_or(|n| n < *t)), inc.notified_at) {
        (Some(t), _) => ((now - t).whole_seconds() >= REOPEN_QUIET_S).then_some(Kind::Firing),
        (None, None) => Some(Kind::Firing),
        (None, Some(n)) => {
            let every = repeat_interval_s(&inc.rule, severity)?;
            ((now - n).whole_seconds() >= every).then_some(Kind::Repeat)
        }
    }
}

/// 恢复时要不要报「恢复」：上次恢复通知之后发过（入队过）触发/提醒才报。
/// 看入队时就刷新的 `notified_at`，不看 notify_log（送达后才写，触发还在队列里时查不到，恢复通知就被吞了）；
/// 同一渠道的队列先进先出，恢复排在触发之后，没收到触发的渠道发送时跳过
fn told_firing(inc: &Open) -> bool {
    inc.notified_at.is_some_and(|n| inc.told_resolved_at.is_none_or(|t| n > t))
}

/// 摘要里的数字（分钟数、代数）每次都在变，不算「摘要变了」
pub(crate) fn same_summary(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.chars().filter(|c| !c.is_ascii_digit()).collect::<String>();
    a == b || norm(a) == norm(b)
}

/// 持续提醒的间隔：续费类与流量额度每天一次，其它只有严重告警每 4 小时一次
pub(crate) fn repeat_interval_s(rule: &str, severity: &str) -> Option<i64> {
    if slow(rule) {
        Some(REPEAT_SLOW_S)
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
    let silences = Silences::load(&st.db).await?;
    // ipintel_ 开头的规则由 ipintel.rs 自己开和关（一次性异常，24 小时后恢复），central_ 开头的（中控自身）由 selfmon.rs 开和关，
    // router_ 开头的（软路由）由 routers/alerts.rs 开和关，这里都不碰，否则会被当成「原因消失」恢复掉
    let mut open: HashMap<(String, Uuid), Open> = sqlx::query_as::<_, Open>(&format!(
        "select {OPEN_COLS} from incident where state = 'firing' and rule not like 'ipintel!_%' escape '!' and rule not like 'central!_%' escape '!'
           and rule not like 'router!_%' escape '!'"
    ))
    .fetch_all(&st.db)
    .await?
    .into_iter()
    .map(|o| ((o.rule.clone(), o.subject_id), o))
    .collect();

    let open_ids: HashSet<Uuid> = open.values().map(|o| o.id).collect();
    let mut current: HashSet<(String, Uuid)> = HashSet::new();
    let mut changed = false;
    let mut touch: Vec<(Uuid, String)> = vec![];
    let mass_offline = agents_unreachable(&ms).is_some();
    for m in ms.iter() {
        for r in &m.health.reasons {
            // info 级只用于界面提示（如访问策略暂停新连接），不开告警
            if r.severity == "info" {
                continue;
            }
            let k = (r.key.to_string(), m.id);
            // 全网同时失联由中控侧的一条告警报（selfmon.rs）：不新开单机的 agent 离线（已开着的照旧）；
            // 那条恢复后还没回来的，去抖从那时算起，再按单机报
            if mass_offline && r.key == "agent_offline" && !open.contains_key(&k) {
                continue;
            }
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
                // 已在告警：同步摘要；严重度只升不降（流量额度超限后上限被调高、回到吃紧，仍按严重算到恢复：
                // 收到过「严重」触发的渠道也要收到恢复）。严重度变了或摘要实质变了马上写、通知前端刷新；
                // 摘要只有数字（分钟数、代数）在变的随 5 分钟一次的「最近仍在」刷新一起写，不每轮重写一遍
                state.clean_since.remove(&inc.id);
                let escalated = rank(r.severity) > rank(&inc.severity);
                let sev = if escalated { r.severity } else { inc.severity.as_str() };
                if escalated || !same_summary(&inc.summary, &summary) {
                    // 升级时清掉之前的确认（同复发重开）：确认的是吃紧，超限是新情况
                    sqlx::query(
                        "update incident set last_seen_at = now(), summary = $2, severity = $3::severity,
                             ack_at = case when $4 then null else ack_at end, ack_by = case when $4 then null else ack_by end
                         where id = $1",
                    )
                    .bind(inc.id)
                    .bind(&summary)
                    .bind(sev)
                    .bind(escalated)
                    .execute(&st.db)
                    .await?;
                    changed = true;
                } else {
                    touch.push((inc.id, summary));
                }
                // 持续提醒；复发后已报过恢复的，满限流时长再发一次「触发」；升成严重（流量额度从吃紧到超限）不等提醒间隔，马上再发一次「触发」
                let kind = if escalated { Some(Kind::Firing) } else { due(&inc, sev, now) };
                if let Some(kind) = kind.filter(|_| !silences.matches(r.key, m.id)) {
                    notify::enqueue(st, Notice { severity: sev.to_string(), ..notice(kind, inc.id, inc.opened_at) }).await;
                }
                continue;
            }

            let immediate = slow(r.key);
            if (!immediate && (now - since).whole_seconds() < debounce_s(r.key)) || silences.matches(r.key, m.id) {
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
    // 原因没变的在告警只在 last_seen_at 旧了 5 分钟以上才刷新（摘要里的数字随之更新）：每 30 秒把所有在告警的行都重写一遍
    // 只会产生死元组，界面上的「最近仍在」、摘要里的分钟数差几分钟无所谓
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

    // 原因消失的告警恢复：只处理 active 的机器。其它生命周期 evaluate 不给原因，不能当成「恢复」——
    // 退役中的在发起退役时已经关掉（closed_by = retire，见 api/machines.rs retire）。
    // `open` 里剩下的就是这一轮没有对应原因的告警。
    let active: HashMap<Uuid, (&str, bool, bool)> =
        ms.iter().filter(|m| m.lifecycle == "active").map(|m| (m.id, (m.name.as_str(), m.health.probe_unknown, m.maintenance))).collect();
    for (_, inc) in open {
        let Some(&(name, probe_unknown, maintenance)) = active.get(&inc.subject_id) else { continue };
        // 维护中 evaluate 不给原因：不是恢复，保持原状，维护结束后按那时的状态再判（同探测过期）
        if maintenance {
            state.clean_since.remove(&inc.id);
            continue;
        }
        // 探测结果过期（线路机 agent 离线、探测停了）：出口 / TCP 的原因消失不等于恢复，保持原状，
        // 等有新探测再判（去抖从那时重新算），否则线路机一离线这些告警就成批假恢复、补传后又重开
        if probe_unknown && matches!(inc.rule.as_str(), "egress_fail" | "tcp_fail") {
            state.clean_since.remove(&inc.id);
            continue;
        }
        let since = *state.clean_since.entry(inc.id).or_insert(now);
        if !slow(&inc.rule) && (now - since).whole_seconds() < debounce_s(&inc.rule) {
            continue;
        }
        state.clean_since.remove(&inc.id);
        sqlx::query("update incident set state = 'resolved', resolved_at = now() where id = $1").bind(inc.id).execute(&st.db).await?;
        changed = true;
        // 恢复通知：上次恢复通知之后发过触发/提醒、且当前没被静默时才发
        if !silences.matches(&inc.rule, inc.subject_id) && told_firing(&inc) {
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
        // 开告警时没能写进发送队列（库出错）：下一轮补发「触发」，警告也补
        let never = Open { told_resolved_at: None, ..inc(None, ago(600)) };
        assert!(matches!(due(&never, "warning", now), Some(Kind::Firing)));
        // 入队过、没到提醒间隔：不发
        let queued = Open { told_resolved_at: None, ..inc(Some(ago(60)), ago(600)) };
        assert!(due(&queued, "critical", now).is_none());
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
        assert_eq!(repeat_interval_s("renewal_due", "critical"), Some(24 * 3600));
        assert_eq!(repeat_interval_s("traffic_quota_high", "warning"), Some(24 * 3600));
        assert_eq!(repeat_interval_s("traffic_quota_high", "critical"), Some(24 * 3600));
    }
}
