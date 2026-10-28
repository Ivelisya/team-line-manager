//! 告警：用 health::evaluate 的结果对齐 incident 表（同中控、软路由、探测点、来源画像，走 incident.rs 的 `reconcile`），并在触发/恢复时发通知。
//! 只看 active 的机器（其它生命周期 evaluate 不给原因），所以退役/上架中的机器天然不告警。
//!
//! 什么时候评估：每 30 秒一轮；另外这些时候马上评估一次（`AppState::alerts_wake`，读刚补过指标的视图，两次至少隔 2 秒）：
//! 上报里某入口的探测结果翻转（失败转成功后还在确认恢复的也算）、agent 从离线回来、Xray 起停（agent_api.rs，`report_matters`），
//! 线路机连通性的结论变了（reach.rs），静默 / 维护变了（api/incidents.rs）。
//!
//! 去抖：
//! - 开：同一 (规则, 机器) 连续出现满去抖时长才开告警（出口不通 150 s、TCP 不通 90 s、其余 60 s；量到的时长留 5 秒余量，见 `held`）；续费类与流量额度立即开。被静默的不开。
//!   折成探测次数：出口探测每 60 s 一次，约连续 3 次失败才开；TCP 每 30 s 一次，约 4 次。relay_unreachable 的原因本身要连续 3 次连不上（reach.rs），
//!   再叠 60 s：整台不通约连续 5 轮（2.5 分钟）才开，摘要写「连续 3 次以上」。
//! - 关：出口 / TCP 不通的，当初失败的入口最近都连续 2 次探测成功就恢复（agent 在结果翻转后约 10 秒补探一次、马上上报，失败期间出口探测改 30 秒一次：
//!   出口恢复后约半分钟告警就恢复）；否则（入口删了、线路机的 Xray 停了这类）同其余规则，原因连续消失满去抖时长才恢复。
//!   续费类与流量额度立即恢复（按日期、小时汇总判定，不会抖）。
//! - 流量异常（非业务流量偏高、业务流量突增）：按小时汇总判定，开、关要几个连续小时由 anomaly.rs 算好，这里不再去抖。
//! - 持续握手丢包（tcp_loss_high）：30 分钟窗口与恢复滞回在 tcp_loss.rs 里算好，这里照普通规则去抖 60 秒；判不出（中控刚启动、样本不够）时开着的保持。
//! - 复发：恢复后 30 分钟内同一 (规则, 机器) 又满足开的条件，重开原告警（`reopen_count` +1，清掉确认），不新建。
//!
//! 通知（见 notify.rs）：新告警发「触发」；严重度升到严重（流量额度从吃紧到超限）马上再发一次「触发」；
//! 仍在触发的严重告警每 4 小时提醒一次，续费类与流量额度每天一次；
//! 原因消失恢复发「恢复」，只在上次恢复通知之后发过触发/提醒时才发（复发后没再通知过的不重复报恢复）；
//! 某个渠道没收到这条的触发（发送放弃了、渠道是后来加的），发送时不给它发恢复（notify.rs `got_firing`）。
//! 复发重开：已报过恢复的，距那条恢复通知满 5 分钟就立即发「触发」，不满则等满 5 分钟仍在触发再发，
//! 所以反复抖动的告警每 5 分钟最多一对触发/恢复，真故障最多晚 5 分钟报。被静默（含维护）的不发任何通知。
//! 「发过」= 写进了库里的发送队列（notify_outbox，`notified_at` 同一事务刷新）：之后由发送循环重试到送达或放弃，中控重启也不丢。

use crate::health::Status;
use crate::incident::{self, Close, Engine, Want};
use crate::notify::Kind;
use crate::selfmon;
use crate::state::AppState;
use crate::views::{self, MachineView};
use futures::FutureExt;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

/// 出口探测失败满这么久才开 egress_fail（出口探测周期长）；软路由出口告警同它（routers/alerts.rs 的 `EXITS_DOWN_S`）
pub(crate) const EGRESS_FAIL_S: i64 = 150;

/// 开、关去抖时长：探测类结果要等下一轮探测才会变，单次偶发失败也常见，拉长一些
/// （出口探测周期长，`EGRESS_FAIL_S`；TCP 探测 90 s）；其余规则 60 s
fn debounce_s(rule: &str) -> i64 {
    match rule {
        "egress_fail" => EGRESS_FAIL_S,
        "tcp_fail" => 90,
        _ => 60,
    }
}
/// 时长判定的余量（同 routers/alerts.rs）：两次取时间有毫秒抖动，「满 150 秒」量到的常是 149.x 秒，差这一点不该再多等一轮
const SLACK_S: i64 = 5;

/// 从 `since` 到 `now` 已经满 `secs` 秒（含余量）
fn held(since: OffsetDateTime, now: OffsetDateTime, secs: i64) -> bool { (now - since).whole_seconds() + SLACK_S >= secs }

/// 出口 / TCP 告警：当初失败的入口都连续这么多次探测成功就恢复（视图里最多数到 3，见 views::Probe::streak）
const PROBE_OK_TO_RESOLVE: u8 = 2;
/// 被唤醒的评估之间至少隔这么久：一阵上报接连唤醒时并成一次
const WAKE_GAP: Duration = Duration::from_secs(2);
/// 恢复后这么久内复发算同一条告警（incident.rs 的通用对齐同此）
pub(crate) const REOPEN_WINDOW_S: i64 = 30 * 60;
/// 报过恢复后这么久内复发不再报触发（抖动限流）
const REOPEN_QUIET_S: i64 = 5 * 60;
/// 严重告警持续触发的提醒间隔
const REPEAT_CRITICAL_S: i64 = 4 * 3600;
/// 续费类与流量额度的提醒间隔（每天最多一次）
const REPEAT_SLOW_S: i64 = 24 * 3600;

/// 严重度的高低（同库里 severity 枚举的顺序）
pub(crate) fn rank(severity: &str) -> u8 {
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

/// 有自己 agent 的在用机器（不含维护中的、同机 IP 节点、闲置备用——放生的机器全挂了不是「全网失联」）里同时离线的超过一半、且至少 `MASS_OFFLINE_MIN` 台时给出 (离线, 总数)：
/// 多半是 agent 到不了中控（边缘机、Caddy、DNS），由 selfmon.rs 开一条中控侧的告警，不按单机各报一条 agent 离线
pub fn agents_unreachable(ms: &[views::MachineView]) -> Option<(usize, usize)> {
    let online: Vec<bool> = ms
        .iter()
        .filter(|m| m.lifecycle == "active" && !m.maintenance && m.host_machine.is_none() && !m.idle_spare)
        .filter_map(|m| m.agent.as_ref().map(|a| a.online))
        .collect();
    let off = online.iter().filter(|o| !**o).count();
    (off >= MASS_OFFLINE_MIN && off * 2 > online.len()).then_some((off, online.len()))
}

/// 「未知」的原因（P-17）：`stale`：线路机在线、Xray 在跑、不在维护，这条在用入口却 15 分钟内没有可用探测结果（探测停了）——节点自己开 `health_unknown`；
/// `relays`：经它探测的线路机离线或 Xray 未运行（线路机自己有告警，节点不重复开）。线路机在维护中是预期的，不算
#[derive(Debug, Default, PartialEq)]
pub struct UnknownCauses {
    pub stale: Vec<String>,
    pub relays: Vec<Uuid>,
}

pub fn unknown_causes(m: &MachineView) -> UnknownCauses {
    let mut c = UnknownCauses::default();
    // 与 health.rs 判「未知」看同一批入口：线路机只看直出入口，其余看全部在用入口
    for e in m.entries.iter().filter(|e| e.lifecycle == "active" && (m.kind != "relay" || e.direct)) {
        if e.relay_maint {
            continue;
        }
        if e.relay_offline || e.relay_xray_down {
            if !c.relays.contains(&e.relay.id) {
                c.relays.push(e.relay.id);
            }
        } else if e.egress.is_none() {
            c.stale.push(format!("{}:{}", e.relay.name, e.port));
        }
    }
    c
}

/// 「未知」告警（P-17）：探测停了满 `GAP_S` 秒，警告级
pub const UNKNOWN: &str = "health_unknown";
/// 故障 / 未知持续这么久还没有告警盖着，自检算矛盾；也是 `health_unknown` 的去抖
pub const GAP_S: i64 = 300;

/// 已过期的备用机（P-06）：备用团队里、订阅已过到期日（等回收）：不开故障类告警（出口 / TCP / agent 离线 / 未知…），开着的关掉。
/// 还被软路由出口指着也一样（客户侧由软路由的出口告警报）；续费提醒（renewal_due，health.rs 只给被扣住的）照发——要续费还是放生由人定
pub fn expired_spare(m: &MachineView) -> bool { m.spare && m.lifecycle == "active" && m.subscription.as_ref().is_some_and(|s| s.due_days < 0) }

/// 不变量自检（P-17）：在用、不在维护、不在备用团队，状态故障或未知满 `GAP_S` 秒的机器，必须有告警盖着——自己开着的机器告警；
/// 全网同时失联时由中控那一条（central_ingress）盖；未知只因线路机离线 / Xray 未运行的，由那几台线路机开着的告警盖。返回没盖住的「名字（状态）」
pub fn gaps(ms: &[MachineView], firing: &HashSet<Uuid>, mass_offline: bool, bad_since: &HashMap<Uuid, OffsetDateTime>, now: OffsetDateTime) -> Vec<String> {
    ms.iter()
        .filter(|m| m.lifecycle == "active" && !m.maintenance && !m.spare && bad(m))
        .filter(|m| bad_since.get(&m.id).is_some_and(|t| held(*t, now, GAP_S)))
        .filter(|m| {
            let down = m.health.status == Status::Down;
            let covered = firing.contains(&m.id)
                || (mass_offline && m.agent.as_ref().is_some_and(|a| !a.online))
                || (!down && {
                    let c = unknown_causes(m);
                    c.stale.is_empty() && c.relays.iter().all(|r| firing.contains(r))
                });
            !covered
        })
        .map(|m| format!("{}（{}）", m.name, if m.health.status == Status::Down { "故障" } else { "未知" }))
        .collect()
}

fn bad(m: &MachineView) -> bool { matches!(m.health.status, Status::Down | Status::Unknown) }

/// 评估循环在两次之间要记住的东西
#[derive(Default)]
pub struct AlertState {
    first_seen: HashMap<(String, Uuid), OffsetDateTime>,
    /// incident id → 原因从什么时候起一直没有
    clean_since: HashMap<Uuid, OffsetDateTime>,
    /// 出口 / TCP 告警最近一次有原因时是哪几条入口失败的（所在线路机）：原因消失时只看它们判不判得出（health::Health::probe_undecided）、
    /// 是不是都连续探测成功了（`probes_recovered`）
    probe_failing: HashMap<(String, Uuid), Vec<Uuid>>,
    /// 机器从什么时候起一直故障 / 未知（不变量自检 `gaps` 用）
    bad_since: HashMap<Uuid, OffsetDateTime>,
}

/// 每 30 秒：记下中控还在跑（report_gaps::ReportGaps::alive），评估机器的告警，成功了 ping 死人开关；再检查中控自身（selfmon.rs）。
/// 被唤醒时（`AppState::alerts_wake`）只评估机器的告警
pub async fn run_forever(st: AppState) {
    let mut state = AlertState::default();
    let mut heartbeat = selfmon::Heartbeat::default();
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last: Option<tokio::time::Instant> = None;
    loop {
        let woken = tokio::select! {
            _ = tick.tick() => false,
            _ = st.alerts_wake.notified() => true,
        };
        if woken {
            if let Some(t) = last {
                tokio::time::sleep_until(t + WAKE_GAP).await;
            }
            // 等的这段时间里又来的唤醒并进这一次（评估期间来的留着，评估完再来一次）
            let _ = st.alerts_wake.notified().now_or_never();
            last = Some(tokio::time::Instant::now());
            if let Err(e) = evaluate(&st, &mut state, OffsetDateTime::now_utc(), true).await {
                tracing::error!("告警评估失败：{e:#}");
            }
            continue;
        }
        let now = OffsetDateTime::now_utc();
        if let Err(e) = st.report_gaps.alive(&st.db, now).await {
            tracing::warn!("记录中控存活时刻失败：{e}");
        }
        last = Some(tokio::time::Instant::now());
        match evaluate(&st, &mut state, now, false).await {
            Ok(()) => heartbeat.beat(&st),
            Err(e) => tracing::error!("告警评估失败：{e:#}"),
        }
        if let Err(e) = selfmon::check_once(&st, now).await {
            tracing::error!("中控自检失败：{e:#}");
        }
    }
}

/// 这份上报会不会改变机器告警的判断（入库提交之后调，是就唤醒评估）：agent 之前离线（`was_offline`），Xray 运行状态和视图里的不同，
/// 或某入口的探测结果和视图里最新一条不同、视图里还没有，或这次成功、视图里那条才连续成功不到 2 次（恢复确认中）。
/// 连续失败不唤醒：开告警要满去抖时长，由 30 秒一轮的评估接住（失败的入口多了，几乎每次上报都带着「不满 3 次」的，会一直唤醒）。
/// `probes`：(入口, tcp / egress, 成功)
pub fn report_matters(st: &AppState, machine: Uuid, was_offline: bool, xray: Option<bool>, probes: &[(Uuid, &str, bool)]) -> bool {
    if was_offline {
        return true;
    }
    if xray.is_none() && probes.is_empty() {
        return false;
    }
    let ms = st.live.latest();
    let Some(m) = ms.as_ref().and_then(|ms| ms.iter().find(|m| m.id == machine)) else { return true };
    if xray.is_some() && m.agent.as_ref().and_then(|a| a.xray_running) != xray {
        return true;
    }
    probes.iter().any(|(id, kind, ok)| {
        let cur = m.entries.iter().find(|e| e.id == *id).and_then(|e| if *kind == "tcp" { e.tcp.as_ref() } else { e.egress.as_ref() });
        cur.is_none_or(|p| p.ok != *ok || (*ok && p.streak < PROBE_OK_TO_RESOLVE))
    })
}

/// 出口 / TCP 告警当初失败的入口（`was`：所在线路机；中控重启后不知道是哪几条，按整台的入口算）最近都连续 `PROBE_OK_TO_RESOLVE` 次探测成功了。
/// 直出入口没有 TCP 探测，不算；一条也没有（入口删了、停了）为 false，交给时长去抖
fn probes_recovered(m: &MachineView, rule: &str, was: Option<&Vec<Uuid>>) -> bool {
    let tcp = rule == "tcp_fail";
    let mut es = m
        .entries
        .iter()
        .filter(|e| e.lifecycle == "active" && (m.kind != "relay" || e.direct) && !(tcp && e.direct) && was.is_none_or(|w| w.contains(&e.relay.id)))
        .peekable();
    es.peek().is_some() && es.all(|e| (if tcp { &e.tcp } else { &e.egress }).as_ref().is_some_and(|p| p.ok && p.streak >= PROBE_OK_TO_RESOLVE))
}

/// 最近一条送达的通知是「恢复」时它的发送时间（`incident` 表上的标量子查询；机器与 incident.rs 的告警共用）
pub(crate) const TOLD_RESOLVED_SQL: &str = "(select case when l.kind = 'resolved' then l.ts end from notify_log l
     where l.incident_id = incident.id and l.ok and l.kind in ('firing', 'repeat', 'resolved') order by l.ts desc, l.id desc limit 1)";

/// 仍在触发的告警这一轮该发的通知：报过恢复的，距那条恢复通知满 REOPEN_QUIET_S 发「触发」；还没入队过（开告警时入队失败）的补发「触发」；
/// 否则按持续提醒间隔发「提醒」（已确认的 `acked` 不再提醒：确认就是知道了，想继续催只能靠升级 / 复发重开清掉确认）。notify_log 在送达后才写（渠道失败重试时更晚），`notified_at` 在入队时就更新：
/// 比那条恢复通知新，说明「触发」已经在发了，按提醒间隔算。机器与 incident.rs 的告警共用
pub(crate) fn due(rule: &str, severity: &str, notified_at: Option<OffsetDateTime>, told_resolved_at: Option<OffsetDateTime>, acked: bool, now: OffsetDateTime) -> Option<Kind> {
    match (told_resolved_at.filter(|t| notified_at.is_none_or(|n| n < *t)), notified_at) {
        (Some(t), _) => ((now - t).whole_seconds() >= REOPEN_QUIET_S).then_some(Kind::Firing),
        (None, None) => Some(Kind::Firing),
        (None, Some(_)) if acked => None,
        (None, Some(n)) => {
            let every = repeat_interval_s(rule, severity)?;
            ((now - n).whole_seconds() >= every).then_some(Kind::Repeat)
        }
    }
}

/// 恢复时要不要报「恢复」：上次恢复通知之后发过（入队过）触发/提醒才报（报过触发的恢复不看普通静默，否则群里只留一条触发没有下文；维护窗口照旧不发，见 incident.rs）。
/// 看入队时就刷新的 `notified_at`，不看 notify_log（送达后才写，触发还在队列里时查不到，恢复通知就被吞了）；
/// 同一渠道的队列先进先出，恢复排在触发之后，没收到触发的渠道发送时跳过。机器与 incident.rs 的告警共用
pub(crate) fn told_firing(notified_at: Option<OffsetDateTime>, told_resolved_at: Option<OffsetDateTime>) -> bool {
    notified_at.is_some_and(|n| told_resolved_at.is_none_or(|t| n > t))
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

/// 评估一轮（读最多旧 3 s 的视图）。`now` 由调用方给（测试可以快进去抖时间）
pub async fn evaluate_once(st: &AppState, state: &mut AlertState, now: OffsetDateTime) -> anyhow::Result<()> { evaluate(st, state, now, false).await }

/// 评估一轮。`fresh`：被唤醒的，读等到锁、刚补过指标的视图（不回上一份：它可能早于刚入库的上报）；否则与列表、实时指标共用在用机器视图（最多旧 3 s）
pub async fn evaluate(st: &AppState, state: &mut AlertState, now: OffsetDateTime, fresh: bool) -> anyhow::Result<()> {
    let ms = if fresh { views::live_synced(st).await? } else { views::live(st).await? };
    // 只读写机器引擎（incident::Engine::Machine）的告警：来源画像、中控自身、软路由、探测点由各自的引擎开和关，这里不碰，否则会被当成「原因消失」恢复掉
    let open = incident::firing(&st.db, Engine::Machine).await?;
    let open_ids: HashSet<Uuid> = open.iter().map(|o| o.id).collect();
    let open_keys: HashSet<(String, Uuid)> = open.iter().map(|o| (o.rule.clone(), o.subject_id)).collect();
    let mut current: HashSet<(String, Uuid)> = HashSet::new();
    let mut want = vec![];
    let mass_offline = agents_unreachable(&ms).is_some();
    for m in ms.iter() {
        // 闲置备用（备用团队、没有软路由出口指向、上面没有在用的同机 IP 节点，等到期放生）：供应商那边停了是预期，不开故障类告警；
        // 已开着的在下面悄悄关掉。健康状态照常算，卡片上看得到。只开着自动续费被扣住的闲置备用照常续费，续费提醒（renewal_due）照发（health.rs renewal_reason 只给被扣住的），其余原因不开
        for r in &m.health.reasons {
            // info 级只用于界面提示（如访问策略暂停新连接），不开告警
            if r.severity == "info" || (m.idle_spare && r.key != "renewal_due") || (expired_spare(m) && r.key != "renewal_due") {
                continue;
            }
            let k = (r.key.to_string(), m.id);
            // 全网同时失联由中控侧的一条告警报（selfmon.rs）：不新开单机的 agent 离线（已开着的照旧）；
            // 那条恢复后还没回来的，去抖从那时算起，再按单机报
            if mass_offline && r.key == "agent_offline" && !open_keys.contains(&k) {
                continue;
            }
            if matches!(r.key, "egress_fail" | "tcp_fail") {
                state.probe_failing.insert(k.clone(), m.health.probe_failing.iter().filter(|f| f.0 == r.key).map(|f| f.1).collect());
            }
            let since = *state.first_seen.entry(k.clone()).or_insert(now);
            let ready = slow(r.key) || crate::anomaly::hourly(r.key) || held(since, now, debounce_s(r.key));
            current.insert(k);
            want.push(Want { rule: r.key, subject: m.id, name: &m.name, severity: r.severity, text: r.text.clone(), detail: Some(json!({"name": m.name})), ready, quiet: false });
        }
        // 「未知」只因探测停了（线路机在线、Xray 在跑、不在维护）：节点自己报；线路机离线 / Xray 未运行由线路机的告警报，全网失联由中控那一条报
        let stale = unknown_causes(m).stale;
        if m.lifecycle == "active" && !m.maintenance && !m.idle_spare && !expired_spare(m) && !mass_offline && m.health.status == Status::Unknown && !stale.is_empty() {
            let k = (UNKNOWN.to_string(), m.id);
            let since = *state.first_seen.entry(k.clone()).or_insert(now);
            let text = format!("状态未知：{} 15 分钟内没有探测结果", stale.join("、"));
            want.push(Want { rule: UNKNOWN, subject: m.id, name: &m.name, severity: "warning", text, detail: Some(json!({"name": m.name})), ready: held(since, now, GAP_S), quiet: false });
            current.insert(k);
        }
    }
    state.first_seen.retain(|k, _| current.contains(k));
    // 原因还在的：恢复的去抖从头算
    for o in &open {
        if current.contains(&(o.rule.clone(), o.subject_id)) {
            state.clean_since.remove(&o.id);
        }
    }

    // 原因消失的告警恢复：只处理 active 的机器。其它生命周期 evaluate 不给原因，不能当成「恢复」——
    // 退役中的在发起退役时已经关掉（closed_by = retire，见 api/machines.rs retire）。
    // 重开与新建都要求机器此刻仍在用：视图是稍早取的，退役事务若在那之后提交（发起退役时已关掉它的告警），
    // 不能再给退役中的机器开一条永远不会恢复的告警。for share：退役事务还没提交时等它提交，再按新的 lifecycle 判断
    let active: HashMap<Uuid, &MachineView> = ms.iter().filter(|m| m.lifecycle == "active").map(|m| (m.id, m)).collect();
    let (clean_since, probe_failing) = (&mut state.clean_since, &state.probe_failing);
    let exists = "exists (select 1 from machine where id = $3 and lifecycle = 'active' for share)";
    incident::reconcile(st, Engine::Machine, open, want, exists, now, |inc| {
        let Some(m) = active.get(&inc.subject_id) else { return Close::Keep };
        // 退到备用、或被指着的备用取消了指向（或上面的 IP 节点都走了），现在没有出口指向了：关掉。报过触发的发恢复（摘要说明是转为闲置，不是原因消失）；之后若又有出口指向它，原因还在就按新告警开
        // 续费提醒只给被扣住的闲置备用（health.rs renewal_reason）：还被扣住的提醒消除（到期日推后、续费了）是普通恢复；
        // 变成不再续费（用户关掉了自动续费）才是转为等到期回收，不再提醒
        if expired_spare(m) && inc.rule != "renewal_due" {
            return Close::ResolveNoted { name: &m.name, by: "spare", note: "备用机订阅已过期（等回收），不再开故障类告警" };
        }
        if m.idle_spare && (inc.rule != "renewal_due" || m.spare_unbilled) {
            let note = if inc.rule == "renewal_due" { "不再续费（等到期回收），不再提醒续费" } else { "转为闲置备用（没有软路由出口指着、没有在用的同机 IP 节点），不再告警" };
            return Close::ResolveNoted { name: &m.name, by: "spare", note };
        }
        let probe = matches!(inc.rule.as_str(), "egress_fail" | "tcp_fail");
        let was = probe_failing.get(&(inc.rule.clone(), inc.subject_id));
        // 维护中 evaluate 不给原因：不是恢复，保持原状，维护结束后按那时的状态再判（同探测过期）。
        // 这一轮判不出（health::Health::undecided）：原因消失不等于恢复，保持原状，等判得出再判（去抖从那时重新算）。
        // 出口 / TCP：探测结果过期（线路机 agent 离线或维护中、探测停了）；持续高负载、时钟偏差：agent 离线（负载还要回来后攒够 5 分钟样本）；
        // 客户入口连不上：中控重启后新的检查不到 3 轮。否则一离线、一重启这些告警就成批假恢复、之后又重开。
        // 出口 / TCP 只看当初失败的那几条入口：另一条入口的线路机在维护、离线不挡这条的恢复（中控重启后不知道是哪几条，按整台算）
        let undecided = m.maintenance
            || match was {
                Some(was) if probe => was.iter().any(|r| m.health.probe_undecided.contains(r)),
                _ => m.health.undecided(&inc.rule),
            };
        // 流量额度的「预计 7 天内用满」在临界点附近会摆动（开、关又不去抖）：预测要超过 7 天 + 12 小时才算原因消失
        if undecided || (inc.rule == "traffic_quota_high" && crate::health::quota_forecast_lingers(m, now)) {
            clean_since.remove(&inc.id);
            return Close::Keep;
        }
        // 流量异常：连续 2 个完整小时都正常才恢复（anomaly.rs 已算好），原因消失（只是不再连续 2 小时异常）不算
        if let Some(clear) = crate::anomaly::cleared(m.anomaly.as_ref(), &inc.rule) {
            clean_since.remove(&inc.id);
            return if clear { Close::Resolve(m.name.as_str()) } else { Close::Keep };
        }
        let since = *clean_since.entry(inc.id).or_insert(now);
        if slow(&inc.rule) || (probe && probes_recovered(m, &inc.rule, was)) || held(since, now, debounce_s(&inc.rule)) {
            clean_since.remove(&inc.id);
            Close::Resolve(m.name.as_str())
        } else {
            Close::Keep
        }
    })
    .await?;
    // 已经不在告警的计数清掉
    state.clean_since.retain(|id, _| open_ids.contains(id));
    state.probe_failing.retain(|k, _| current.contains(k) || open_keys.contains(k));
    // 不变量自检：在用且故障 / 未知的必有告警盖着（selfmon.rs 照它开 central_alert_gap）
    state.bad_since.retain(|id, _| ms.iter().any(|m| m.id == *id && bad(m)));
    for m in ms.iter().filter(|m| bad(m)) {
        state.bad_since.entry(m.id).or_insert(now);
    }
    let firing: HashSet<Uuid> = incident::firing(&st.db, Engine::Machine).await?.iter().map(|o| o.subject_id).collect();
    let g = gaps(&ms, &firing, mass_offline, &state.bad_since, now);
    if !g.is_empty() {
        tracing::warn!(gaps = %g.join("、"), "在用且故障/未知的机器没有告警");
    }
    *st.alert_gaps.lock().unwrap() = g;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(f: impl FnOnce(&mut MachineView)) -> MachineView {
        let mut m = crate::health::tests::base("vps");
        m.entries = vec![crate::health::tests::entry(false, true)];
        f(&mut m);
        m.health = crate::health::evaluate(&m, OffsetDateTime::now_utc());
        m
    }

    #[test]
    fn down_or_unknown_without_alert_is_a_gap() {
        let now = OffsetDateTime::now_utc();
        let down = node(|m| m.entries = vec![crate::health::tests::entry(false, false)]);
        assert_eq!(down.health.status, crate::health::Status::Down);
        let since = |m: &MachineView, s: i64| HashMap::from([(m.id, now - time::Duration::seconds(s))]);
        assert_eq!(gaps(std::slice::from_ref(&down), &HashSet::new(), false, &since(&down, 301), now).len(), 1, "故障满 5 分钟没有告警：矛盾");
        assert!(gaps(std::slice::from_ref(&down), &HashSet::from([down.id]), false, &since(&down, 301), now).is_empty(), "有告警盖着");
        assert!(gaps(std::slice::from_ref(&down), &HashSet::new(), false, &since(&down, 30), now).is_empty(), "还在去抖");
        let spare = node(|m| {
            m.entries = vec![crate::health::tests::entry(false, false)];
            m.spare = true;
            m.idle_spare = true;
        });
        assert!(gaps(std::slice::from_ref(&spare), &HashSet::new(), false, &since(&spare, 301), now).is_empty(), "备用团队不在不变量里");
        // 未知：探测停了要自己有告警；只因线路机离线的由线路机的告警盖
        let stale = node(|m| m.entries[0].egress = None);
        assert_eq!(stale.health.status, crate::health::Status::Unknown);
        assert_eq!(gaps(std::slice::from_ref(&stale), &HashSet::new(), false, &since(&stale, 301), now).len(), 1);
        let via = node(|m| m.entries[0].relay_offline = true);
        let relay = via.entries[0].relay.id;
        assert_eq!(gaps(std::slice::from_ref(&via), &HashSet::new(), false, &since(&via, 301), now).len(), 1, "线路机也没告警：矛盾");
        assert!(gaps(std::slice::from_ref(&via), &HashSet::from([relay]), false, &since(&via, 301), now).is_empty());
        // 全网失联：由中控那一条盖
        let off = node(|m| m.agent.as_mut().unwrap().online = false);
        assert_eq!(off.health.status, crate::health::Status::Down);
        assert!(gaps(std::slice::from_ref(&off), &HashSet::new(), true, &since(&off, 301), now).is_empty());
    }

    #[test]
    fn unknown_causes_split_stale_probes_from_relay_problems() {
        let stale = node(|m| m.entries[0].egress = None);
        assert_eq!(unknown_causes(&stale).stale.len(), 1);
        let via = node(|m| m.entries[0].relay_offline = true);
        assert_eq!(unknown_causes(&via), UnknownCauses { stale: vec![], relays: vec![via.entries[0].relay.id] });
        let maint = node(|m| m.entries[0].relay_maint = true);
        assert_eq!(unknown_causes(&maint), UnknownCauses::default(), "线路机维护中是预期的");
    }

    #[test]
    fn expired_spare_is_detected_by_due_date() {
        let mk = |spare: bool, due: i64| node(|m| { m.spare = spare; m.subscription = Some(crate::health::tests::sub(due)); });
        assert!(expired_spare(&mk(true, -1)));
        assert!(!expired_spare(&mk(true, 3)), "没过期");
        assert!(!expired_spare(&mk(false, -1)), "不在备用团队");
    }

    #[test]
    fn summary_digits_do_not_count_as_change() {
        assert!(same_summary("团队1-01：5 分钟没有上报", "团队1-01：6 分钟没有上报"));
        assert!(same_summary("线路机2：配置未应用（配置版本 3，已生效 2）", "线路机2：配置未应用（配置版本 4，已生效 2）"));
        assert!(!same_summary("团队1-01：5 分钟没有上报", "团队1-01：从未上报"));
    }

    #[test]
    fn reopen_firing_not_resent_while_in_flight() {
        let now = OffsetDateTime::now_utc();
        let ago = |s: i64| now - time::Duration::seconds(s);
        let due = |notified, resolved, sev| due("relay_xray_down", sev, notified, resolved, false, now);
        // 恢复通知 10 分钟前，之后没再发过：发「触发」
        assert!(matches!(due(Some(ago(900)), Some(ago(600)), "critical"), Some(Kind::Firing)));
        // 「触发」30 s 前已开始发送、还没写进 notify_log（最近一条仍是恢复）：不重发
        assert!(due(Some(ago(30)), Some(ago(600)), "critical").is_none());
        // 开告警时没能写进发送队列（库出错）：下一轮补发「触发」，警告也补
        assert!(matches!(due(None, None, "warning"), Some(Kind::Firing)));
        // 入队过、没到提醒间隔：不发
        assert!(due(Some(ago(60)), None, "critical").is_none());
    }

    #[test]
    fn acked_incident_gets_no_repeat() {
        let now = OffsetDateTime::now_utc();
        let ago = |s: i64| now - time::Duration::seconds(s);
        // 严重告警 5 小时前通知过、没确认：到间隔发「提醒」
        assert!(matches!(due("relay_xray_down", "critical", Some(ago(5 * 3600)), None, false, now), Some(Kind::Repeat)));
        // 已确认：不再提醒（续费类每天一次的提醒同理）
        assert!(due("relay_xray_down", "critical", Some(ago(5 * 3600)), None, true, now).is_none());
        assert!(matches!(due("renewal_due", "warning", Some(ago(25 * 3600)), None, false, now), Some(Kind::Repeat)));
        assert!(due("renewal_due", "warning", Some(ago(25 * 3600)), None, true, now).is_none());
        // 确认不挡「触发」：开告警时没入队过的补发；报过恢复后复发（重开清掉确认，传进来的就是 false）也照常发
        assert!(matches!(due("relay_xray_down", "critical", None, None, true, now), Some(Kind::Firing)));
        assert!(matches!(due("relay_xray_down", "critical", Some(ago(900)), Some(ago(600)), true, now), Some(Kind::Firing)));
    }

    #[test]
    fn resolved_notice_follows_queued_firing() {
        let now = OffsetDateTime::now_utc();
        let ago = |s: i64| now - time::Duration::seconds(s);
        // 从没通知过（如开告警时被静默）：不报恢复
        assert!(!told_firing(None, None));
        // 触发已入队（notify_log 里可能还没有）：报恢复
        assert!(told_firing(Some(ago(30)), None));
        // 报过恢复后复发、又入队了触发：报恢复
        assert!(told_firing(Some(ago(30)), Some(ago(600))));
        // 最近一条是恢复、之后没再入队触发：不重复报
        assert!(!told_firing(Some(ago(900)), Some(ago(600))));
    }

    #[test]
    fn idle_spare_offline_is_not_mass_offline() {
        let m = |spare: bool, exits: &[&str], online: bool| {
            let mut m = crate::health::tests::base("vps");
            m.spare = spare;
            m.router_exits = exits.iter().map(|s| s.to_string()).collect();
            m.agent.as_mut().unwrap().online = online;
            m.settle_spare()
        };
        // 放生的备用机器全挂：不算全网失联（在用的 2 台都在线）
        let ms: Vec<_> = [m(true, &[], false), m(true, &[], false), m(true, &[], false), m(false, &[], true), m(false, &[], true)].into();
        assert_eq!(agents_unreachable(&ms), None);
        // 被软路由出口指向的备用机器照常算
        let ms: Vec<_> = [m(true, &["办公室2 · 美国"], false), m(true, &["办公室2 · 美国"], false), m(true, &["办公室2 · 美国"], false), m(false, &[], true)].into();
        assert_eq!(agents_unreachable(&ms), Some((3, 4)));
        // 上面还有在用同机 IP 节点的备用宿主机同样照常算
        let host = |online: bool| {
            let mut x = m(true, &[], online);
            x.live_ip_nodes = vec!["团队1-02".into()];
            x.settle_spare()
        };
        let ms: Vec<_> = [host(false), host(false), host(false), m(false, &[], true)].into();
        assert_eq!(agents_unreachable(&ms), Some((3, 4)));
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
