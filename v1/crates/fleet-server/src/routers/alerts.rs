//! 软路由的告警（规则 `router_` 开头，对象是路由：incident.subject_id = router.id）。采集每轮之后评估一次，
//! 同 selfmon.rs 走 incident.rs：此刻成立的问题对齐到 incident 表，开、关、通知走 notify::enqueue（渠道的最低级别、严重的每 4 小时提醒、
//! 静默与维护都同机器的告警：可以给单台路由开静默（全部或某条规则）、维护窗口，维护中开着的保持、不开新的、不发通知，见 incident.rs）。
//! 与「降级算正常、故障才是异常」的口径一致：
//!
//! | 规则 | 级别 | 开 |
//! |---|---|---|
//! | router_offline | 严重 | 接入过（换了这把隧道密钥之后连上过）、此刻连不上满 3 分钟（中控收不到上报的时段不算，同 agent 离线，见 report_gaps.rs） |
//! | router_offline | 警告 | 接入过的路由重新接入（重新生成接入命令、重新部署）之后 30 分钟还没连上 |
//! | router_collect_error | 警告 | 连得上，但采集脚本出错、输出解析不了、超时（满 60 秒） |
//! | router_openclash_down | 严重 | OpenClash 启用了、核心没在跑（满 60 秒） |
//! | router_exits_down | 警告；全部 fleet 出口都不通升严重 | 有 fleet 出口（对上了 fleet 入口的）连续两次延迟测试不通（每 5 分钟测一次），且从第一次不通起满 180 秒（同机器 egress_fail 的 150 秒再多一轮，让机器告警先开，几十秒到几分钟的出口抖动不报）；客户自己的出口不算。摘要附上不通的出口指向的节点 / 线路机此刻在维护或严重告警中的（「其中 2 个指向的团队2-03 正在告警」）；不通的出口指向的都在维护中时不开（维护期间预期不通，开着的保持）；不通的出口指向的都已有机器的严重告警（出口不通、TCP 不通、agent 离线）在报时，告警照开、挂在路由上，但不单独通知（一个节点挂了不再每台路由各报一条），原因单独留下来时补发；节点 / 线路机的维护刚结束（不满 180 秒）也按这样处理——维护期间机器告警的计时被清，结束后要重攒 150 秒才会开，这段时间不抢先通知 |
//! | router_wan_down | 严重 | WAN 口（逻辑接口 wan）断开（满 60 秒） |
//! | router_resource_high | 警告 | CPU > 90% 持续 10 分钟、内存 > 90%、存储 > 90%、连接表 > 80%（后三项满 60 秒） |
//! | router_exit_drift | 警告 | 有出口对上了 fleet 入口、但参数和入口现在的不一致（凭据轮换后没同步、入口已删除 / 停用，见 clash.rs），持续两轮采集（满 30 秒）。抽屉与告警里可以一键同步（作业 router_sync） |
//!
//! 恢复：离线的连上就恢复（隧道一回来中控马上采一次，见 collect.rs `watch_tunnels`）；出口不通的，不通的出口连续两次测通就恢复
//! （不通、还在确认的出口每轮采集单独重测，不等 5 分钟一轮；确认期间又不通仍算连续不通，告警不关）；其余原因消失满 60 秒。连不上、采集出错的路由其余几条无从判断，保持原状（不开也不关）。
//! 路由删掉时它的告警直接关掉、不发恢复。
//! 什么时候评估：每轮采集之后（30 秒一轮，隧道连上 / 断开时马上采的那一轮也算），连不上的路由到满 3 分钟的那一刻再评估一次（`evaluate` 的返回值）。
//! 重新接入后（等待接入）开着的离线告警直接关掉、不发恢复（路由还没连上，不能说「已恢复」），30 分钟还没连上再开上面那条警告。
//! 严重度只升不降（升到严重马上再发一次「触发」），同 alerts.rs。

use super::collect::{Exit, Snapshot};
use crate::incident::{self, Close, Engine, Want};
use crate::notify;
use crate::silence::Silences;
use crate::state::{AppState, Event};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use time::OffsetDateTime;
use uuid::Uuid;

pub const OFFLINE: &str = "router_offline";
pub const COLLECT_ERROR: &str = "router_collect_error";
pub const OPENCLASH_DOWN: &str = "router_openclash_down";
pub const EXITS_DOWN: &str = "router_exits_down";
pub const WAN_DOWN: &str = "router_wan_down";
pub const RESOURCE_HIGH: &str = "router_resource_high";
pub const EXIT_DRIFT: &str = "router_exit_drift";

/// 重新接入后这么久还没连上，开离线警告（接入命令的有效期）
const REBOOTSTRAP_S: i64 = super::BOOTSTRAP_MINUTES * 60;
/// 开（不带时长的条件）、关的去抖
const DEBOUNCE_S: i64 = 60;
/// 连不上满这么久开离线告警（扣掉中控收不到上报的时段），同出口不通的 180 秒：隧道偶尔没连上一两分钟（7 天 8 次、5 次不到 1 分钟）不报
const OFFLINE_S: i64 = EXITS_DOWN_S;
/// CPU 持续偏高满这么久
const CPU_SUSTAIN_S: i64 = 600;
/// 采集 / 评估的间隔（秒）：路由采集和机器告警评估都是每 30 秒一轮
const EVAL_TICK_S: i64 = 30;
/// 出口连续不通：从第一次不通的测试起满这么久才开 = 同机器 egress_fail 的去抖（alerts.rs `EGRESS_FAIL_S`）再多一轮。
/// 路由出口不通通常和指向的节点 / 线路机的 egress_fail 同时发生；两边都在 150 秒开的话，路由告警可能先入库，
/// 那一轮机器告警还没开，quiet 判不出来就会单独通知。晚一轮，机器告警先开，路由告警判 quiet 时看得到。
/// 采集每 30 秒重测不通的出口，所以到这时已经测了六次左右
const EXITS_DOWN_S: i64 = crate::alerts::EGRESS_FAIL_S + EVAL_TICK_S;
/// 出口参数不一致持续这么久（两轮采集）才开：替换、改出口指向的作业改配置的那一会儿不报
const DRIFT_HOLD_S: i64 = 30;
/// 时长判定的余量：采集每 30 秒一轮、每轮耗时不一，「满 60 秒」实际量到的是 59.x 秒，差这几秒不该再多等一轮
const SLACK_S: i64 = 5;

/// 从 `since` 到 `now` 已经满 `secs` 秒（含余量）
fn held(since: OffsetDateTime, now: OffsetDateTime, secs: i64) -> bool { (now - since).whole_seconds() + SLACK_S >= secs }

/// 评估之间要记住的：各问题第一次出现的时间、上一轮是 quiet 的问题、quiet 解除的时刻、各告警原因消失的时间、CPU 从什么时候起一直偏高、上一轮在维护中的路由
#[derive(Default)]
pub struct AlertState {
    first_seen: HashMap<(&'static str, Uuid), OffsetDateTime>,
    quiet: HashSet<(&'static str, Uuid)>,
    /// quiet 解除（机器告警先恢复了）的时刻：这台路由的快照要晚于它才算「解除之后重新测过」，才判断要不要补发「触发」
    unquiet_at: HashMap<(&'static str, Uuid), OffsetDateTime>,
    clean_since: HashMap<Uuid, OffsetDateTime>,
    cpu_high_since: HashMap<Uuid, OffsetDateTime>,
    maintenance: HashSet<Uuid>,
}

impl AlertState {
    /// 中控重启后从库里补回 quiet：还在告警、上次恢复之后没报过「触发」（`!told_firing`：从没通知过，或通知时刻不晚于最近一条恢复通知——
    /// 恢复后重开、重开时 quiet 的也在内）、没被静默的出口告警，一律先当 quiet。
    /// 这些里除了真 quiet 的（开的时候机器告警在报），还有开告警时通知入队失败、所以也没有通知记录的：同样被当成 quiet，
    /// 下一轮评估发现它其实不 quiet 时走「quiet 解除」的补发（等这台路由重新采集、出口还不通才发「触发」），只是晚一轮
    pub async fn load(db: &sqlx::PgPool) -> sqlx::Result<Self> {
        let ids: Vec<Uuid> = sqlx::query_scalar(&format!(
            "select subject_id from incident where state = 'firing' and engine = 'router' and rule = $1
               and (notified_at is null or notified_at <= {})",
            crate::alerts::TOLD_RESOLVED_SQL
        ))
        .bind(EXITS_DOWN)
        .fetch_all(db)
        .await?;
        let silences = Silences::load(db).await?;
        Ok(Self { quiet: ids.into_iter().filter(|id| !silences.matches(EXITS_DOWN, *id)).map(|id| (EXITS_DOWN, id)).collect(), ..Self::default() })
    }

    /// 这条出口告警此刻是 quiet（开的时候机器告警在报，还没单独通知过）
    pub fn is_quiet(&self, router: Uuid) -> bool { self.quiet.contains(&(EXITS_DOWN, router)) }
}

/// 一台路由此刻的样子
pub struct View {
    pub id: Uuid,
    pub name: String,
    pub state: &'static str,
    /// 离线时是连不上的原因；在线时有值 = 连得上但采集出错
    pub error: Option<String>,
    pub last_ok_at: Option<OffsetDateTime>,
    /// 连不上满 `OFFLINE_S`（3 分钟）了（扣掉中控收不到上报的时段）
    pub offline_held: bool,
    /// 等待接入从什么时候起（重新生成接入命令、取走接入脚本、一键部署的时刻）
    pub pending_since: Option<OffsetDateTime>,
    pub snap: Option<Snapshot>,
}

/// 出口指向的节点 / 线路机此刻的样子（router_exits_down 的附注；都在维护中的不开）
#[derive(Default)]
pub struct Targets {
    /// 在维护中的（同机 IP 节点的宿主机在维护也算）
    pub maint: HashSet<Uuid>,
    /// 有机器的严重告警（出口不通、TCP 不通、agent 离线，见 `targets`）在报的
    pub firing: HashSet<Uuid>,
    /// 同机 IP 节点 → (宿主机 id, 宿主机名)：这种节点没有自己的 agent，agent 离线只由宿主机报
    pub hosts: HashMap<Uuid, (Uuid, String)>,
    /// 其中 agent 离线（严重）在报的宿主机：它的同机 IP 节点也算在告警（文案写宿主机名）。宿主机的出口不通 / TCP 不通不算——
    /// IP 节点有自己的 IP 和入口，出口 / TCP 探测各报各的（health.rs），宿主机那条不代表它
    pub host_down: HashSet<Uuid>,
    /// 维护刚结束不满 `EXITS_DOWN_S` 的（同 `maint`，同机 IP 节点看宿主机）：维护期间机器告警的计时被清掉，结束后要重攒 150 秒才会开，
    /// 而出口的不通计时一直在走、早就满了。这段时间机器告警还没来得及开，按「原因已经有人在报」处理，免得路由告警抢在机器告警前单独通知
    pub settled: HashSet<Uuid>,
}

/// 出口为什么可能不通：指向的节点 / 线路机在维护、有机器告警、或维护刚结束（机器告警还没来得及开）
#[derive(Clone, Copy, Debug, PartialEq)]
enum Why {
    Maint,
    Alert,
    Settled,
}

impl Targets {
    /// 这个出口为什么可能不通：指向的节点或线路机在维护 / 告警 / 刚结束维护，给出它的名字。维护优先，其次告警，节点优先
    fn why<'a>(&'a self, e: &'a Exit) -> Option<(&'a str, Why)> {
        let er = e.entry.as_ref()?;
        let pick = |set: &HashSet<Uuid>, host_set: &HashSet<Uuid>| {
            if set.contains(&er.node_id) {
                Some(er.node_name.as_str())
            } else if let Some((_, name)) = self.hosts.get(&er.node_id).filter(|(h, _)| host_set.contains(h)) {
                Some(name.as_str())
            } else if set.contains(&er.relay_id) {
                Some(er.relay_name.as_str())
            } else {
                None
            }
        };
        pick(&self.maint, &self.maint)
            .map(|n| (n, Why::Maint))
            .or_else(|| pick(&self.firing, &self.host_down).map(|n| (n, Why::Alert)))
            .or_else(|| pick(&self.settled, &self.settled).map(|n| (n, Why::Settled)))
    }
}

/// 不通的 fleet 出口（连续两次延迟测试不通；客户自己的出口不算）
fn down_exits(s: &Snapshot) -> impl Iterator<Item = &Exit> { s.exits.iter().filter(|e| e.sync != "unmanaged" && e.fail_streak >= 2) }

/// 不通满 `EXITS_DOWN_S` 的 fleet 出口（第一次不通的测试起算；连续不通两次以上的一定有起点）
fn held_down(s: &Snapshot, now: OffsetDateTime) -> impl Iterator<Item = &Exit> { down_exits(s).filter(move |e| e.fail_since.is_some_and(|t| held(t, now, EXITS_DOWN_S))) }

/// 不通的出口指向的都在维护中：这一轮 router_exits_down 不开，开着的保持（同机器告警的维护：冻结）
fn exits_paused(s: &Snapshot, tg: &Targets, now: OffsetDateTime) -> bool {
    let mut down = held_down(s, now).peekable();
    down.peek().is_some() && down.all(|e| tg.why(e).is_some_and(|(_, w)| w == Why::Maint))
}

/// 出口参数为什么和入口对不上：入口已删除 / 停用，否则是凭据（uuid、SNI、公钥、short-id、flow）和入口现在的不一样
pub fn drift_why(lifecycle: &str) -> &'static str {
    match lifecycle {
        "removed" | "removing" => "入口已删除",
        "disabled" => "入口已停用",
        _ => "凭据和入口现在的不一样",
    }
}

/// 不通的出口都有对应入口、且全经同一台线路机时，这台线路机的名字
fn common_relay<'a>(down: &[&'a Exit]) -> Option<&'a str> {
    let first = down.first()?.entry.as_ref()?;
    down.iter().all(|e| e.entry.as_ref().is_some_and(|er| er.relay_id == first.relay_id)).then_some(first.relay_name.as_str())
}

/// 此刻成立的一个问题
#[derive(Debug, PartialEq)]
pub struct Problem {
    pub rule: &'static str,
    pub severity: &'static str,
    /// 不带「路由名：」前缀
    pub text: String,
    /// 要持续这么久才开（条件自己带了时长的为 0）
    pub hold_s: i64,
    /// 开着但不单独通知（原因已经由机器告警在报）
    pub quiet: bool,
}

fn pct(a: Option<i64>, b: Option<i64>) -> Option<f64> {
    let (a, b) = (a?, b?);
    (b > 0).then(|| a as f64 * 100.0 / b as f64)
}

/// 这台路由此刻的问题；连不上、采集出错时只判离线 / 采集出错，其余几条返回 None（无从判断，保持原状）。`tg`：出口指向的节点 / 线路机此刻在不在维护、告警
pub fn problems(v: &View, cpu_high_since: Option<OffsetDateTime>, tg: &Targets, now: OffsetDateTime) -> (Vec<Problem>, bool) {
    let mut out = vec![];
    if v.state != "online" {
        if v.state == "offline" {
            if let Some(ok) = v.last_ok_at.filter(|_| v.offline_held) {
                out.push(Problem {
                    rule: OFFLINE,
                    severity: "critical",
                    text: format!("连不上已 {}：{}", notify::fmt_duration((now - ok).whole_seconds()), v.error.as_deref().unwrap_or("原因不明")),
                    hold_s: 0,
                    quiet: false,
                });
            }
        }
        // 接入过的、重新接入之后一直没连上（命令过期没人执行、部署失败、取了脚本没跑成）
        if let Some(since) = v.pending_since.filter(|t| v.state == "pending" && v.last_ok_at.is_some() && held(*t, now, REBOOTSTRAP_S)) {
            out.push(Problem { rule: OFFLINE, severity: "warning", text: format!("重新接入后还没连上（已 {}）", notify::fmt_duration((now - since).whole_seconds())), hold_s: 0, quiet: false });
        }
        return (out, false);
    }
    if let Some(e) = &v.error {
        out.push(Problem { rule: COLLECT_ERROR, severity: "warning", text: format!("连得上，但没读到状态：{e}"), hold_s: DEBOUNCE_S, quiet: false });
        return (out, false);
    }
    let Some(s) = &v.snap else { return (out, false) };
    let oc = &s.openclash;
    if oc.enabled && !oc.running {
        out.push(Problem { rule: OPENCLASH_DOWN, severity: "critical", text: "OpenClash 已启用，但核心（clash）没在运行：客户端走不了代理".into(), hold_s: DEBOUNCE_S, quiet: false });
    }
    // 只看 fleet 出口：客户自己的出口（订阅、机场节点）不通不归我们管，也不算进「全部」
    let fleet = s.exits.iter().filter(|e| e.sync != "unmanaged").count();
    let down: Vec<&Exit> = held_down(s, now).collect();
    if !down.is_empty() && !exits_paused(s, tg, now) {
        let all = down.len() == fleet;
        let mut names = down.iter().take(8).map(|e| e.name.as_str()).collect::<Vec<_>>().join("、");
        if down.len() > 8 {
            names.push_str(&format!(" 等 {} 个", down.len()));
        }
        let mut text = if all {
            format!("对应面板入口的 {} 个出口都连续两次延迟测试不通：{names}", down.len())
        } else {
            format!("{} 个出口连续两次延迟测试不通：{names}", down.len())
        };
        // 指向的节点 / 线路机此刻在维护、告警中的：按对象数一数，看的人先去看它
        let mut why: Vec<((&str, Why), usize)> = vec![];
        for w in down.iter().filter_map(|e| tg.why(e)) {
            match why.iter_mut().find(|x| x.0 == w) {
                Some(x) => x.1 += 1,
                None => why.push((w, 1)),
            }
        }
        let notes: Vec<String> = why
            .iter()
            .map(|((name, w), n)| format!("{n} 个指向的{name} {}", match w { Why::Maint => "正在维护", Why::Alert => "正在告警", Why::Settled => "刚结束维护" }))
            .collect();
        if !notes.is_empty() {
            text.push_str(&format!("（其中 {}）", notes.join("，")));
        } else if let Some(relay) = common_relay(&down) {
            // 没有机器告警、维护能解释：不通的出口全经同一台线路机时点出来（线路机在国内被挡，它自己的探测不一定看得出），先去查它
            text.push_str(&format!("（{}经 {relay}）", if down.len() > 1 { "都" } else { "" }));
        }
        // 不通的出口指向的节点 / 线路机都已经在告警（或维护）：机器告警在报原因，这条挂在路由上、不另发通知
        let quiet = down.iter().all(|e| tg.why(e).is_some());
        out.push(Problem { rule: EXITS_DOWN, severity: if all { "critical" } else { "warning" }, text, hold_s: 0, quiet });
    }
    if let Some(w) = s.interfaces.iter().find(|i| i.name == "wan" && !i.up) {
        out.push(Problem {
            rule: WAN_DOWN,
            severity: "critical",
            text: format!("WAN 口断开（wan{}）", w.device.as_deref().map(|d| format!(" · {d}")).unwrap_or_default()),
            hold_s: DEBOUNCE_S,
            quiet: false,
        });
    }
    let y = &s.system;
    let mut res = vec![];
    if let Some(since) = cpu_high_since.filter(|t| held(*t, now, CPU_SUSTAIN_S)) {
        res.push(format!("CPU {:.0}%（超过 90% 已 {}）", y.cpu_pct.unwrap_or(0.0), notify::fmt_duration((now - since).whole_seconds())));
    }
    if let Some(p) = pct(y.mem_used, y.mem_total).filter(|p| *p > 90.0) {
        res.push(format!("内存 {p:.0}%（{} / {}）", notify::fmt_bytes(y.mem_used.unwrap_or(0) as f64), notify::fmt_bytes(y.mem_total.unwrap_or(0) as f64)));
    }
    if let Some(p) = pct(y.disk_used, y.disk_total).filter(|p| *p > 90.0) {
        res.push(format!("存储 {p:.0}%（{} {} / {}）", y.disk_path.as_deref().unwrap_or("/"), notify::fmt_bytes(y.disk_used.unwrap_or(0) as f64), notify::fmt_bytes(y.disk_total.unwrap_or(0) as f64)));
    }
    if let Some(p) = pct(y.conntrack, y.conntrack_max).filter(|p| *p > 80.0) {
        res.push(format!("连接表 {p:.0}%（{} / {}）", y.conntrack.unwrap_or(0), y.conntrack_max.unwrap_or(0)));
    }
    if !res.is_empty() {
        out.push(Problem { rule: RESOURCE_HIGH, severity: "warning", text: format!("资源吃紧：{}", res.join("；")), hold_s: DEBOUNCE_S, quiet: false });
    }
    let drift: Vec<String> = s.exits.iter().filter(|e| e.sync == "drift").filter_map(|e| e.entry.as_ref().map(|er| format!("{}（{}，{}{}）", e.name, er.name, drift_why(&er.lifecycle), if e.linked { "" } else { "；从未对上过，未自动同步" }))).collect();
    if !drift.is_empty() {
        out.push(Problem { rule: EXIT_DRIFT, severity: "warning", text: format!("{} 个出口的参数和对应的面板入口不一致：{}", drift.len(), drift.join("、")), hold_s: DRIFT_HOLD_S, quiet: false });
    }
    (out, true)
}

/// 评估一轮，把路由的告警对齐到 incident 表（incident.rs）。`now` 由调用方给（测试可以快进）。
/// 返回多久之后该再评估一次：有连不上、还没满 `OFFLINE_S` 的路由时，到它满的那一刻（不等下一轮采集）
pub async fn evaluate(st: &AppState, state: &mut AlertState, now: OffsetDateTime) -> anyhow::Result<Option<std::time::Duration>> {
    let rows = super::load(&st.db, None).await?;
    let views: Vec<View> = rows
        .iter()
        .map(|r| View {
            id: r.id,
            name: r.name.clone(),
            state: r.state(),
            error: r.error.clone(),
            last_ok_at: r.last_ok_at,
            offline_held: r.last_ok_at.is_some_and(|ok| st.report_gaps.silent_s(ok, now) + SLACK_S >= OFFLINE_S),
            pending_since: r.pending_since,
            snap: r.snapshot(),
        })
        .collect();
    let recheck = views
        .iter()
        .filter(|v| v.state == "offline" && !v.offline_held)
        .filter_map(|v| v.last_ok_at)
        .map(|ok| OFFLINE_S - SLACK_S - st.report_gaps.silent_s(ok, now))
        .min()
        .map(|s| std::time::Duration::from_secs(s.max(1) as u64));
    let tg = targets(&st.db, &views).await?;
    // 维护窗口自己开始 / 结束（预约的到点、到期）时没有人操作、也就没有事件：路由列表里的「维护」靠这里发 Routers 才跟上（新建 / 删除静默时 api/incidents.rs 已发）
    let maintenance: HashSet<Uuid> = {
        let silences = Silences::load(&st.db).await?;
        views.iter().filter(|v| silences.maintenance(v.id)).map(|v| v.id).collect()
    };
    if maintenance != state.maintenance {
        state.maintenance = maintenance;
        st.emit(Event::Routers);
    }
    let open = incident::firing(&st.db, Engine::Router).await?;
    let open_ids: HashSet<Uuid> = open.iter().map(|o| o.id).collect();
    let mut seen: HashSet<(&'static str, Uuid)> = HashSet::new();
    let mut known: HashSet<Uuid> = HashSet::new();
    // 不通的出口指向的都在维护中的路由：出口告警冻结
    let mut paused: HashSet<Uuid> = HashSet::new();
    let mut want = vec![];
    let mut quiet: HashSet<(&'static str, Uuid)> = HashSet::new();
    for v in &views {
        // CPU 从什么时候起一直超过 90%（连不上、采集出错的这一轮不动它）
        if v.state == "online" && v.error.is_none() {
            match v.snap.as_ref().and_then(|s| s.system.cpu_pct) {
                Some(c) if c > 90.0 => {
                    state.cpu_high_since.entry(v.id).or_insert(now);
                }
                _ => {
                    state.cpu_high_since.remove(&v.id);
                }
            }
        }
        let (ps, judged) = problems(v, state.cpu_high_since.get(&v.id).copied(), &tg, now);
        if judged {
            known.insert(v.id);
            if v.snap.as_ref().is_some_and(|s| exits_paused(s, &tg, now)) {
                paused.insert(v.id);
            }
        }
        for p in ps {
            let key = (p.rule, v.id);
            seen.insert(key);
            let since = *state.first_seen.entry(key).or_insert(now);
            let ready = p.hold_s == 0 || held(since, now, p.hold_s);
            // 上一轮还是 quiet、这一轮不是了（机器告警先恢复）：记下这一刻，等这台路由重新采集（快照晚于它）之后出口还不通才补发「触发」；
            // 等的是新数据而不是一轮：评估也跑在补采几台、到点复查时，这台的快照可能还是旧的。解除之后出口就好了的，既不发触发也不发恢复
            if p.quiet {
                quiet.insert(key);
                state.unquiet_at.remove(&key);
            } else if state.quiet.contains(&key) {
                state.unquiet_at.entry(key).or_insert(now);
            }
            let collected = v.snap.as_ref().and_then(|s| s.collected_at);
            let settling = match state.unquiet_at.get(&key) {
                Some(&at) if collected.is_some_and(|c| c > at) => {
                    state.unquiet_at.remove(&key);
                    false
                }
                Some(_) => true,
                None => false,
            };
            want.push(Want { rule: p.rule, subject: v.id, name: &v.name, severity: p.severity, text: p.text, detail: Some(json!({"name": v.name, "kind": "router"})), ready, quiet: p.quiet || settling });
        }
    }
    state.first_seen.retain(|k, _| seen.contains(k));
    state.unquiet_at.retain(|k, _| seen.contains(k));
    state.quiet = quiet;
    // 问题还在的：恢复的去抖从头算
    for o in &open {
        if seen.contains(&(o.rule.as_str(), o.subject_id)) {
            state.clean_since.remove(&o.id);
        }
    }
    let names: HashMap<Uuid, (&str, &str)> = views.iter().map(|v| (v.id, (v.name.as_str(), v.state))).collect();
    // 刚测通、还要再确认一次的 fleet 出口：出口告警先不关
    let confirming: HashSet<Uuid> = views.iter().filter(|v| v.snap.as_ref().is_some_and(|s| s.exits.iter().any(|e| e.sync != "unmanaged" && e.recovering))).map(|v| v.id).collect();
    let clean_since = &mut state.clean_since;
    incident::reconcile(st, Engine::Router, open, want, "exists (select 1 from router where id = $3)", now, |inc| {
        // 路由已经删了（删除接口会关掉，这里兜底）：关掉、不发恢复
        let Some(&(name, state)) = names.get(&inc.subject_id) else { return Close::Quiet("router_deleted") };
        // 重新接入、还没连上：离线告警关掉、不发恢复（30 分钟还没连上另开警告）
        if inc.rule == OFFLINE && state == "pending" {
            clean_since.remove(&inc.id);
            return Close::Quiet("router_rebootstrap");
        }
        // 连不上的路由：除离线外无从判断，保持原状；不通的出口指向的都在维护中：出口告警保持原状
        if (inc.rule != OFFLINE && !known.contains(&inc.subject_id)) || (inc.rule == EXITS_DOWN && paused.contains(&inc.subject_id)) {
            clean_since.remove(&inc.id);
            return Close::Keep;
        }
        // 离线的连上就恢复；出口不通的，不通的出口连续两次测通（没有还在确认的）就恢复；其余满去抖时长
        let since = *clean_since.entry(inc.id).or_insert(now);
        let ok = match inc.rule.as_str() {
            OFFLINE => state == "online",
            EXITS_DOWN => !confirming.contains(&inc.subject_id),
            _ => held(since, now, DEBOUNCE_S),
        };
        if !ok {
            return Close::Keep;
        }
        clean_since.remove(&inc.id);
        Close::Resolve(name)
    })
    .await?;
    state.clean_since.retain(|id, _| open_ids.contains(id));
    let ids: HashSet<Uuid> = views.iter().map(|v| v.id).collect();
    state.cpu_high_since.retain(|id, _| ids.contains(id));
    Ok(recheck)
}

/// 各路由不通的出口指向的节点、线路机里，此刻在维护（静默表的维护，同机 IP 节点看宿主机）、有机器的严重告警（出口不通、TCP 不通、agent 离线）在报的；
/// 同机 IP 节点的 agent 离线只由宿主机报，所以宿主机的 agent 离线一并查（宿主机自己的出口 / TCP 告警不算它的，见 `Targets::host_down`）。没有不通的出口时不查库
async fn targets(db: &sqlx::PgPool, views: &[View]) -> sqlx::Result<Targets> {
    let mut ids: Vec<Uuid> =
        views.iter().filter_map(|v| v.snap.as_ref()).flat_map(|s| down_exits(s).filter_map(|e| e.entry.as_ref()).flat_map(|er| [er.node_id, er.relay_id])).collect();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return Ok(Targets::default());
    }
    let silences = Silences::load(db).await?;
    let rows: Vec<(Uuid, Option<Uuid>, Option<String>)> = sqlx::query_as(
        "select m.id, m.host_id, display_name(h.kind::text, h.label, t.name, h.number)
         from machine m left join machine h on h.id = m.host_id left join team t on t.id = h.team_id where m.id = any($1)",
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;
    let maint = rows.iter().filter(|(id, host, _)| silences.maintenance(*id) || host.is_some_and(|h| silences.maintenance(h))).map(|(id, ..)| *id).collect();
    let ended = crate::silence::ended_maintenance(db, EXITS_DOWN_S).await?;
    let settled = rows.iter().filter(|(id, host, _)| ended.matches(*id) || host.is_some_and(|h| ended.matches(h))).map(|(id, ..)| *id).collect();
    let hosts: HashMap<Uuid, (Uuid, String)> = rows.into_iter().filter_map(|(id, host, name)| Some((id, (host?, name.unwrap_or_default())))).collect();
    let mut host_ids: Vec<Uuid> = hosts.values().map(|(h, _)| *h).collect();
    host_ids.sort();
    host_ids.dedup();
    let firing: Vec<(Uuid, String)> = sqlx::query_as(
        "select distinct subject_id, rule from incident where state = 'firing' and engine = 'machine' and severity = 'critical' and rule in ('egress_fail', 'tcp_fail', 'agent_offline')
           and (subject_id = any($1) or (rule = 'agent_offline' and subject_id = any($2)))",
    )
    .bind(&ids)
    .bind(&host_ids)
    .fetch_all(db)
    .await?;
    let host_down = firing.iter().filter(|(id, rule)| rule == "agent_offline" && host_ids.contains(id)).map(|(id, _)| *id).collect();
    Ok(Targets { maint, firing: firing.into_iter().map(|(id, _)| id).collect(), hosts, host_down, settled })
}

/// 删除路由时（同一事务）关掉它在告警中的告警：不发恢复
pub async fn close_all(tx: &mut crate::db::Tx<'_>, router: Uuid) -> sqlx::Result<u64> {
    Ok(sqlx::query(
        "update incident set state = 'resolved', resolved_at = now(), detail = detail || '{\"closed_by\": \"router_deleted\"}'
         where subject_id = $1 and state = 'firing' and engine = 'router'",
    )
    .bind(router)
    .execute(&mut **tx)
    .await?
    .rows_affected())
}

/// 告警列表、发送记录里路由告警的对象：`{id, name, kind: "router"}`（路由已删除时名字是「（已删除）」）
pub fn subject(id: Uuid, name: Option<&str>) -> Value { json!({"id": id, "name": name.unwrap_or("（已删除）"), "kind": "router"}) }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routers::clash::EntryRef;
    use crate::routers::collect::{Iface, OpenClash, System};

    fn view(state: &'static str, snap: Option<Snapshot>, last_ok_s: i64, now: OffsetDateTime) -> View {
        let ok = now - time::Duration::seconds(last_ok_s);
        View {
            id: Uuid::nil(),
            name: "团队2-路由A".into(),
            state,
            error: (state != "online").then(|| "经隧道连不上路由：Connection refused".into()),
            last_ok_at: Some(ok),
            offline_held: held(ok, now, OFFLINE_S),
            pending_since: None,
            snap,
        }
    }

    fn healthy() -> Snapshot {
        Snapshot {
            system: System { cpu_pct: Some(10.0), mem_used: Some(1), mem_total: Some(4), disk_used: Some(1), disk_total: Some(10), conntrack: Some(10), conntrack_max: Some(100), ..System::default() },
            interfaces: vec![Iface { name: "wan".into(), device: Some("eth0".into()), up: true, ..Iface::default() }],
            openclash: OpenClash { enabled: true, running: true, ..OpenClash::default() },
            exits: (1..=3).map(|i| Exit { name: format!("节点0{i}"), sync: "ok".into(), ..Exit::default() }).collect(),
            ..Snapshot::default()
        }
    }

    #[test]
    fn offline_after_three_minutes_only() {
        let now = OffsetDateTime::now_utc();
        // 同出口不通的 180 秒门槛（含 5 秒余量）：1 到 3 分钟的隧道没连上不报
        for secs in [40, 90, 170] {
            assert!(problems(&view("offline", None, secs, now), None, &Targets::default(), now).0.is_empty(), "{secs} 秒不到 3 分钟");
        }
        for secs in [175, 180] {
            assert_eq!(problems(&view("offline", Some(healthy()), secs, now), None, &Targets::default(), now).0.len(), 1, "{secs} 秒该报");
        }
        let (p, judged) = problems(&view("offline", Some(healthy()), 300, now), None, &Targets::default(), now);
        assert!(!judged, "连不上时其余几条无从判断");
        assert_eq!((p.len(), p[0].rule, p[0].severity), (1, OFFLINE, "critical"));
        assert_eq!(p[0].text, "连不上已 5 分钟：经隧道连不上路由：Connection refused");
        assert!(problems(&view("pending", None, 3000, now), None, &Targets::default(), now).0.is_empty(), "还没接入的不报离线");
    }

    /// 重新接入后：30 分钟内不报；之后接入过的报离线警告，从没连上过的不报
    #[test]
    fn rebootstrap_not_connected() {
        let now = OffsetDateTime::now_utc();
        let mut v = view("pending", None, 7200, now);
        v.pending_since = Some(now - time::Duration::minutes(20));
        assert!(problems(&v, None, &Targets::default(), now).0.is_empty());
        v.pending_since = Some(now - time::Duration::minutes(31));
        let p = problems(&v, None, &Targets::default(), now).0;
        assert_eq!((p.len(), p[0].rule, p[0].severity, p[0].text.as_str()), (1, OFFLINE, "warning", "重新接入后还没连上（已 31 分钟）"));
        v.last_ok_at = None;
        assert!(problems(&v, None, &Targets::default(), now).0.is_empty(), "从没连上过：还是新接入，不报");
    }

    /// 连得上但采集出错：只报采集出错（警告、满 60 秒），其余几条无从判断
    #[test]
    fn collect_error_is_not_offline() {
        let now = OffsetDateTime::now_utc();
        let mut s = healthy();
        s.openclash.running = false;
        let mut v = view("online", Some(s), 0, now);
        v.error = Some("采集脚本出错（退出码 1）：Syntax error".into());
        let (p, judged) = problems(&v, None, &Targets::default(), now);
        assert!(!judged);
        assert_eq!(p, vec![Problem { rule: COLLECT_ERROR, severity: "warning", text: "连得上，但没读到状态：采集脚本出错（退出码 1）：Syntax error".into(), hold_s: DEBOUNCE_S, quiet: false }]);
    }

    #[test]
    fn thresholds() {
        let now = OffsetDateTime::now_utc();
        let ok = view("online", Some(healthy()), 0, now);
        assert_eq!(problems(&ok, None, &Targets::default(), now), (vec![], true));
        let mut s = healthy();
        s.openclash.running = false;
        s.interfaces[0].up = false;
        s.exits[1].fail_streak = 2;
        s.exits[1].fail_since = Some(now - time::Duration::seconds(200));
        s.exits[2].fail_streak = 1;
        // 客户自己的出口不通不算
        s.exits.push(Exit { name: "机场".into(), sync: "unmanaged".into(), fail_streak: 5, ..Exit::default() });
        s.system.mem_used = Some(368);
        s.system.mem_total = Some(400);
        s.system.conntrack = Some(81);
        s.system.cpu_pct = Some(95.0);
        let (p, _) = problems(&view("online", Some(s.clone()), 0, now), Some(now - time::Duration::seconds(700)), &Targets::default(), now);
        let rules: Vec<&str> = p.iter().map(|p| p.rule).collect();
        assert_eq!(rules, [OPENCLASH_DOWN, EXITS_DOWN, WAN_DOWN, RESOURCE_HIGH]);
        assert_eq!((p[1].severity, p[1].text.as_str()), ("warning", "1 个出口连续两次延迟测试不通：节点02"));
        assert_eq!(p[2].text, "WAN 口断开（wan · eth0）");
        assert_eq!(p[3].text, "资源吃紧：CPU 95%（超过 90% 已 11 分钟）；内存 92%（368 B / 400 B）；连接表 81%（81 / 100）");
        // CPU 高还不到 10 分钟、内存 90% 整、连接表 80% 整都不算
        s.system.mem_used = Some(360);
        s.system.conntrack = Some(80);
        let (p, _) = problems(&view("online", Some(s.clone()), 0, now), Some(now - time::Duration::seconds(300)), &Targets::default(), now);
        assert!(!p.iter().any(|p| p.rule == RESOURCE_HIGH), "{p:?}");
        // 全部 fleet 出口都不通：严重（客户自己的出口通着也一样）
        for e in &mut s.exits {
            e.fail_streak = if e.sync == "unmanaged" { 0 } else { 3 };
            e.fail_since = Some(now - time::Duration::seconds(200));
        }
        let (p, _) = problems(&view("online", Some(s.clone()), 0, now), None, &Targets::default(), now);
        let ex = p.iter().find(|p| p.rule == EXITS_DOWN).unwrap();
        assert_eq!((ex.severity, ex.text.as_str()), ("critical", "对应面板入口的 3 个出口都连续两次延迟测试不通：节点01、节点02、节点03"));
        // 只有客户自己的出口不通：不报
        for e in &mut s.exits {
            e.fail_streak = if e.sync == "unmanaged" { 5 } else { 0 };
        }
        assert!(!problems(&view("online", Some(s), 0, now), None, &Targets::default(), now).0.iter().any(|p| p.rule == EXITS_DOWN));
    }

    /// 出口指向的节点 / 线路机在维护、告警中：摘要附上；不通的都指向维护中的：不开
    #[test]
    fn exits_down_notes_targets() {
        let now = OffsetDateTime::now_utc();
        let (n3, n4, relay) = (Uuid::from_u128(3), Uuid::from_u128(4), Uuid::from_u128(9));
        let er = |node: Uuid, name: &str| EntryRef { id: Uuid::now_v7(), name: String::new(), node_id: node, relay_id: relay, node_name: name.into(), relay_name: "线路机2".into(), port: 1, lifecycle: "active".into() };
        let mut s = healthy();
        for (i, (node, name)) in [(n3, "团队2-03"), (n3, "团队2-03"), (n4, "团队2-04")].into_iter().enumerate() {
            s.exits[i].entry = Some(er(node, name));
            s.exits[i].fail_streak = 2;
            s.exits[i].fail_since = Some(now - time::Duration::seconds(200));
        }
        s.exits.push(Exit { name: "节点05".into(), sync: "ok".into(), entry: Some(er(Uuid::from_u128(5), "团队2-05")), ..Exit::default() });
        let v = view("online", Some(s.clone()), 0, now);
        let ex = |tg: &Targets| problems(&v, None, tg, now).0.into_iter().find(|p| p.rule == EXITS_DOWN);
        // 没有机器告警 / 维护能解释、不通的出口全经同一台线路机：点出线路机名
        assert_eq!(ex(&Targets::default()).unwrap().text, "3 个出口连续两次延迟测试不通：节点01、节点02、节点03（都经 线路机2）");
        // 经不同线路机、或有的出口没对上入口：不点
        let mut other = s.clone();
        other.exits[2].entry = Some(EntryRef { relay_id: Uuid::from_u128(10), relay_name: "线路机3".into(), ..er(n4, "团队2-04") });
        let text = |s: &Snapshot| problems(&view("online", Some(s.clone()), 0, now), None, &Targets::default(), now).0.into_iter().find(|p| p.rule == EXITS_DOWN).unwrap().text;
        assert_eq!(text(&other), "3 个出口连续两次延迟测试不通：节点01、节点02、节点03");
        other.exits[2].entry = None;
        assert_eq!(text(&other), "3 个出口连续两次延迟测试不通：节点01、节点02、节点03");
        // 只有一个出口不通
        other.exits[0].fail_streak = 0;
        other.exits[1].fail_streak = 0;
        other.exits[2].entry = Some(er(n4, "团队2-04"));
        assert_eq!(text(&other), "1 个出口连续两次延迟测试不通：节点03（经 线路机2）");
        let tg = Targets { maint: [n3].into(), firing: [relay].into(), ..Targets::default() };
        assert_eq!(ex(&tg).unwrap().text, "3 个出口连续两次延迟测试不通：节点01、节点02、节点03（其中 2 个指向的团队2-03 正在维护，1 个指向的线路机2 正在告警）");
        assert!(!exits_paused(&s, &tg, now));
        // 不通的都指向维护中的（线路机在维护）：不开，评估时开着的保持
        let tg = Targets { maint: [relay].into(), firing: HashSet::new(), ..Targets::default() };
        assert!(ex(&tg).is_none() && exits_paused(&s, &tg, now));
    }

    /// 出口不通要从第一次不通的测试起满 180 秒（同机器 egress_fail 的 150 秒再多一轮）才开；只不通 1 次、几十秒就恢复的不报
    #[test]
    fn exits_down_needs_180s() {
        let now = OffsetDateTime::now_utc();
        let mut s = healthy();
        s.exits[1].fail_streak = 2;
        let ex = |s: &Snapshot| problems(&view("online", Some(s.clone()), 0, now), None, &Targets::default(), now).0.into_iter().find(|p| p.rule == EXITS_DOWN);
        for secs in [30, 60, 120, 150, 174] {
            s.exits[1].fail_since = Some(now - time::Duration::seconds(secs));
            assert!(ex(&s).is_none(), "{secs} 秒还不到");
        }
        // 满 180 秒（含 5 秒余量）开
        for secs in [175, 180, 600] {
            s.exits[1].fail_since = Some(now - time::Duration::seconds(secs));
            assert_eq!(ex(&s).unwrap().text, "1 个出口连续两次延迟测试不通：节点02", "{secs} 秒");
        }
        // 时间够了，但连续不通还不到两次：不开
        s.exits[1].fail_streak = 1;
        assert!(ex(&s).is_none());
        // 满与不满的混在一起：只算满的（摘要、「全部」都按满的数）
        s.exits[0].fail_streak = 2;
        s.exits[0].fail_since = Some(now - time::Duration::seconds(40));
        s.exits[1].fail_streak = 2;
        s.exits[1].fail_since = Some(now - time::Duration::seconds(200));
        assert_eq!(ex(&s).unwrap().text, "1 个出口连续两次延迟测试不通：节点02");
    }

    /// 不通的出口指向的节点 / 线路机都已有机器的严重告警（或在维护）：告警照开但不单独通知；有一个不是（原因不明）就照常通知
    #[test]
    fn exits_down_is_quiet_when_machine_alert_reports_it() {
        let now = OffsetDateTime::now_utc();
        let (n3, n4, relay) = (Uuid::from_u128(3), Uuid::from_u128(4), Uuid::from_u128(9));
        let er = |node: Uuid, name: &str| EntryRef { id: Uuid::now_v7(), name: String::new(), node_id: node, relay_id: relay, node_name: name.into(), relay_name: "线路机2".into(), port: 1, lifecycle: "active".into() };
        let mut s = healthy();
        for (i, (node, name)) in [(n3, "团队2-03"), (n4, "团队2-04")].into_iter().enumerate() {
            s.exits[i].entry = Some(er(node, name));
            s.exits[i].fail_streak = 3;
            s.exits[i].fail_since = Some(now - time::Duration::seconds(200));
        }
        let v = view("online", Some(s), 0, now);
        let quiet = |tg: &Targets| problems(&v, None, tg, now).0.into_iter().find(|p| p.rule == EXITS_DOWN).map(|p| p.quiet);
        assert_eq!(quiet(&Targets::default()), Some(false), "没有机器告警在报：照常通知");
        assert_eq!(quiet(&Targets { maint: HashSet::new(), firing: [n3].into(), ..Targets::default() }), Some(false), "只有一个出口的节点在告警：另一个原因不明，照常通知");
        assert_eq!(quiet(&Targets { maint: HashSet::new(), firing: [n3, n4].into(), ..Targets::default() }), Some(true));
        assert_eq!(quiet(&Targets { maint: HashSet::new(), firing: [relay].into(), ..Targets::default() }), Some(true), "线路机在告警：它下面的出口都算");
        assert_eq!(quiet(&Targets { maint: [n3].into(), firing: [n4].into(), ..Targets::default() }), Some(true));
        // 同机 IP 节点的 agent 离线只由宿主机报：宿主机在告警，节点也算，文案写宿主机名
        let host = Uuid::from_u128(7);
        let hosts: HashMap<Uuid, (Uuid, String)> = [(n3, (host, "团队2-07".to_string())), (n4, (host, "团队2-07".to_string()))].into();
        // 宿主机自己的出口不通 / TCP 不通（在 firing 里）不代表 IP 节点：IP 节点有自己的 IP 和探测，各报各的
        let tg = Targets { maint: HashSet::new(), firing: [host].into(), hosts: hosts.clone(), host_down: HashSet::new(), ..Targets::default() };
        assert_eq!(quiet(&tg), Some(false));
        let tg = Targets { maint: HashSet::new(), firing: [host].into(), hosts, host_down: [host].into(), ..Targets::default() };
        assert_eq!(quiet(&tg), Some(true));
        assert_eq!(problems(&v, None, &tg, now).0.into_iter().find(|p| p.rule == EXITS_DOWN).unwrap().text, "2 个出口连续两次延迟测试不通：节点01、节点02（其中 2 个指向的团队2-07 正在告警）");
        // 维护刚结束、机器告警还没来得及开：也算有人在报（quiet），摘要写明；机器告警开了之后以告警为准
        let tg = Targets { settled: [n3, n4].into(), ..Targets::default() };
        assert_eq!(quiet(&tg), Some(true));
        assert_eq!(problems(&v, None, &tg, now).0.into_iter().find(|p| p.rule == EXITS_DOWN).unwrap().text, "2 个出口连续两次延迟测试不通：节点01、节点02（其中 1 个指向的团队2-03 刚结束维护，1 个指向的团队2-04 刚结束维护）");
        assert_eq!(quiet(&Targets { settled: [n3].into(), ..Targets::default() }), Some(false), "只有一个出口的节点刚结束维护：另一个原因不明，照常通知");
        assert!(!exits_paused(v.snap.as_ref().unwrap(), &tg, now), "维护已经结束：不再冻结");
        let tg = Targets { settled: [n3, n4].into(), firing: [n3].into(), ..Targets::default() };
        assert!(problems(&v, None, &tg, now).0.into_iter().find(|p| p.rule == EXITS_DOWN).unwrap().text.contains("1 个指向的团队2-03 正在告警"));
        // 其他规则从不 quiet
        let mut bad = healthy();
        bad.interfaces[0].up = false;
        assert!(problems(&view("online", Some(bad), 0, now), None, &Targets::default(), now).0.iter().all(|p| !p.quiet));
    }

    #[test]
    fn rules_have_names() {
        for r in [OFFLINE, COLLECT_ERROR, OPENCLASH_DOWN, EXITS_DOWN, WAN_DOWN, RESOURCE_HIGH, EXIT_DRIFT] {
            assert_ne!(crate::meta::rule_label(r), r, "{r} 没有中文名");
        }
        assert_eq!(subject(Uuid::nil(), None)["name"], "（已删除）");
    }
}
