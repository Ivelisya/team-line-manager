//! 软路由的告警（规则 `router_` 开头，对象是路由：incident.subject_id = router.id）。采集每轮之后评估一次，
//! 同 selfmon.rs 走 incident.rs：此刻成立的问题对齐到 incident 表，开、关、通知走 notify::enqueue（渠道的最低级别、严重的每 4 小时提醒、
//! 静默与维护都同机器的告警：可以给单台路由开静默（全部或某条规则）、维护窗口，维护中开着的保持、不开新的、不发通知，见 incident.rs）。
//! 与「降级算正常、故障才是异常」的口径一致：
//!
//! | 规则 | 级别 | 开 |
//! |---|---|---|
//! | router_offline | 严重 | 接入过（换了这把隧道密钥之后连上过）、此刻连不上超过 2 分钟（中控收不到上报的时段不算，同 agent 离线，见 report_gaps.rs） |
//! | router_offline | 警告 | 接入过的路由重新接入（重新生成接入命令、重新部署）之后 30 分钟还没连上 |
//! | router_collect_error | 警告 | 连得上，但采集脚本出错、输出解析不了、超时（满 60 秒） |
//! | router_openclash_down | 严重 | OpenClash 启用了、核心没在跑（满 60 秒） |
//! | router_exits_down | 警告；全部 fleet 出口都不通升严重 | 有 fleet 出口（对上了 fleet 入口的）连续两次延迟测试不通（每 5 分钟测一次）；客户自己的出口不算。摘要附上不通的出口指向的节点 / 线路机此刻在维护或告警中的（「其中 2 个指向的团队2-03 正在维护」）；不通的出口指向的都在维护中时不开（维护期间预期不通，开着的保持） |
//! | router_wan_down | 严重 | WAN 口（逻辑接口 wan）断开（满 60 秒） |
//! | router_resource_high | 警告 | CPU > 90% 持续 10 分钟、内存 > 90%、存储 > 90%、连接表 > 80%（后三项满 60 秒） |
//!
//! 恢复：原因消失满 60 秒（出口延迟每 5 分钟才测一轮：一轮测通就算原因消失，这 60 秒里不会再测）。连不上、采集出错的路由其余几条无从判断，保持原状（不开也不关）。路由删掉时它的告警直接关掉、不发恢复。
//! 重新接入后（等待接入）开着的离线告警直接关掉、不发恢复（路由还没连上，不能说「已恢复」），30 分钟还没连上再开上面那条警告。
//! 严重度只升不降（升到严重马上再发一次「触发」），同 alerts.rs。

use super::collect::{Exit, Snapshot};
use crate::incident::{self, Close, Engine, Want};
use crate::notify;
use crate::silence::Silences;
use crate::state::AppState;
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

/// 重新接入后这么久还没连上，开离线警告（接入命令的有效期）
const REBOOTSTRAP_S: i64 = super::BOOTSTRAP_MINUTES * 60;
/// 开（不带时长的条件）、关的去抖
const DEBOUNCE_S: i64 = 60;
/// CPU 持续偏高满这么久
const CPU_SUSTAIN_S: i64 = 600;
/// 时长判定的余量：采集每 30 秒一轮、每轮耗时不一，「满 60 秒」实际量到的是 59.x 秒，差这几秒不该再多等一轮
const SLACK_S: i64 = 5;

/// 从 `since` 到 `now` 已经满 `secs` 秒（含余量）
fn held(since: OffsetDateTime, now: OffsetDateTime, secs: i64) -> bool { (now - since).whole_seconds() + SLACK_S >= secs }

/// 评估之间要记住的：各问题第一次出现的时间、各告警原因消失的时间、CPU 从什么时候起一直偏高
#[derive(Default)]
pub struct AlertState {
    first_seen: HashMap<(&'static str, Uuid), OffsetDateTime>,
    clean_since: HashMap<Uuid, OffsetDateTime>,
    cpu_high_since: HashMap<Uuid, OffsetDateTime>,
}

/// 一台路由此刻的样子
pub struct View {
    pub id: Uuid,
    pub name: String,
    pub state: &'static str,
    /// 离线时是连不上的原因；在线时有值 = 连得上但采集出错
    pub error: Option<String>,
    pub last_ok_at: Option<OffsetDateTime>,
    /// 连不上满 2 分钟了（扣掉中控收不到上报的时段）
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
    /// 有机器告警在告警的
    pub firing: HashSet<Uuid>,
}

impl Targets {
    /// 这个出口为什么可能不通：指向的节点或线路机在维护（true）/ 告警（false），给出它的名字。维护优先、节点优先
    fn why<'a>(&self, e: &'a Exit) -> Option<(&'a str, bool)> {
        let er = e.entry.as_ref()?;
        let pick = |set: &HashSet<Uuid>| {
            if set.contains(&er.node_id) {
                Some(er.node_name.as_str())
            } else if set.contains(&er.relay_id) {
                Some(er.relay_name.as_str())
            } else {
                None
            }
        };
        pick(&self.maint).map(|n| (n, true)).or_else(|| pick(&self.firing).map(|n| (n, false)))
    }
}

/// 不通的 fleet 出口（连续两次延迟测试不通；客户自己的出口不算）
fn down_exits(s: &Snapshot) -> impl Iterator<Item = &Exit> { s.exits.iter().filter(|e| e.sync != "unmanaged" && e.fail_streak >= 2) }

/// 不通的出口指向的都在维护中：这一轮 router_exits_down 不开，开着的保持（同机器告警的维护：冻结）
fn exits_paused(s: &Snapshot, tg: &Targets) -> bool {
    let mut down = down_exits(s).peekable();
    down.peek().is_some() && down.all(|e| tg.why(e).is_some_and(|(_, m)| m))
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
                });
            }
        }
        // 接入过的、重新接入之后一直没连上（命令过期没人执行、部署失败、取了脚本没跑成）
        if let Some(since) = v.pending_since.filter(|t| v.state == "pending" && v.last_ok_at.is_some() && held(*t, now, REBOOTSTRAP_S)) {
            out.push(Problem { rule: OFFLINE, severity: "warning", text: format!("重新接入后还没连上（已 {}）", notify::fmt_duration((now - since).whole_seconds())), hold_s: 0 });
        }
        return (out, false);
    }
    if let Some(e) = &v.error {
        out.push(Problem { rule: COLLECT_ERROR, severity: "warning", text: format!("连得上，但没读到状态：{e}"), hold_s: DEBOUNCE_S });
        return (out, false);
    }
    let Some(s) = &v.snap else { return (out, false) };
    let oc = &s.openclash;
    if oc.enabled && !oc.running {
        out.push(Problem { rule: OPENCLASH_DOWN, severity: "critical", text: "OpenClash 已启用，但核心（clash）没在运行：客户端走不了代理".into(), hold_s: DEBOUNCE_S });
    }
    // 只看 fleet 出口：客户自己的出口（订阅、机场节点）不通不归我们管，也不算进「全部」
    let fleet = s.exits.iter().filter(|e| e.sync != "unmanaged").count();
    let down: Vec<&Exit> = down_exits(s).collect();
    if !down.is_empty() && !exits_paused(s, tg) {
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
        let mut why: Vec<((&str, bool), usize)> = vec![];
        for w in down.iter().filter_map(|e| tg.why(e)) {
            match why.iter_mut().find(|x| x.0 == w) {
                Some(x) => x.1 += 1,
                None => why.push((w, 1)),
            }
        }
        let notes: Vec<String> = why.iter().map(|((name, m), n)| format!("{n} 个指向的{name} 正在{}", if *m { "维护" } else { "告警" })).collect();
        if !notes.is_empty() {
            text.push_str(&format!("（其中 {}）", notes.join("，")));
        }
        out.push(Problem { rule: EXITS_DOWN, severity: if all { "critical" } else { "warning" }, text, hold_s: 0 });
    }
    if let Some(w) = s.interfaces.iter().find(|i| i.name == "wan" && !i.up) {
        out.push(Problem {
            rule: WAN_DOWN,
            severity: "critical",
            text: format!("WAN 口断开（wan{}）", w.device.as_deref().map(|d| format!(" · {d}")).unwrap_or_default()),
            hold_s: DEBOUNCE_S,
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
        out.push(Problem { rule: RESOURCE_HIGH, severity: "warning", text: format!("资源吃紧：{}", res.join("；")), hold_s: DEBOUNCE_S });
    }
    (out, true)
}

/// 评估一轮，把路由的告警对齐到 incident 表（incident.rs）。`now` 由调用方给（测试可以快进）
pub async fn evaluate(st: &AppState, state: &mut AlertState, now: OffsetDateTime) -> anyhow::Result<()> {
    let rows = super::load(&st.db, None).await?;
    let views: Vec<View> = rows
        .iter()
        .map(|r| View {
            id: r.id,
            name: r.name.clone(),
            state: r.state(),
            error: r.error.clone(),
            last_ok_at: r.last_ok_at,
            // 余量同 held：往前挪 SLACK_S 秒再按 agent 的口径判
            offline_held: r.last_ok_at.is_some_and(|ok| !st.report_gaps.online(ok - time::Duration::seconds(SLACK_S), now)),
            pending_since: r.pending_since,
            snap: r.snapshot(),
        })
        .collect();
    let tg = targets(&st.db, &views).await?;
    let open = incident::firing(&st.db, Engine::Router).await?;
    let open_ids: HashSet<Uuid> = open.iter().map(|o| o.id).collect();
    let mut seen: HashSet<(&'static str, Uuid)> = HashSet::new();
    let mut known: HashSet<Uuid> = HashSet::new();
    // 不通的出口指向的都在维护中的路由：出口告警冻结
    let mut paused: HashSet<Uuid> = HashSet::new();
    let mut want = vec![];
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
            if v.snap.as_ref().is_some_and(|s| exits_paused(s, &tg)) {
                paused.insert(v.id);
            }
        }
        for p in ps {
            let key = (p.rule, v.id);
            seen.insert(key);
            let since = *state.first_seen.entry(key).or_insert(now);
            let ready = p.hold_s == 0 || held(since, now, p.hold_s);
            want.push(Want { rule: p.rule, subject: v.id, name: &v.name, severity: p.severity, text: p.text, detail: Some(json!({"name": v.name, "kind": "router"})), ready });
        }
    }
    state.first_seen.retain(|k, _| seen.contains(k));
    // 问题还在的：恢复的去抖从头算
    for o in &open {
        if seen.contains(&(o.rule.as_str(), o.subject_id)) {
            state.clean_since.remove(&o.id);
        }
    }
    let names: HashMap<Uuid, (&str, &str)> = views.iter().map(|v| (v.id, (v.name.as_str(), v.state))).collect();
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
        let since = *clean_since.entry(inc.id).or_insert(now);
        if !held(since, now, DEBOUNCE_S) {
            return Close::Keep;
        }
        clean_since.remove(&inc.id);
        Close::Resolve(name)
    })
    .await?;
    state.clean_since.retain(|id, _| open_ids.contains(id));
    let ids: HashSet<Uuid> = views.iter().map(|v| v.id).collect();
    state.cpu_high_since.retain(|id, _| ids.contains(id));
    Ok(())
}

/// 各路由不通的出口指向的节点、线路机里，此刻在维护（静默表的维护，同机 IP 节点看宿主机）、有机器告警在告警的。没有不通的出口时不查库
async fn targets(db: &sqlx::PgPool, views: &[View]) -> sqlx::Result<Targets> {
    let mut ids: Vec<Uuid> =
        views.iter().filter_map(|v| v.snap.as_ref()).flat_map(|s| down_exits(s).filter_map(|e| e.entry.as_ref()).flat_map(|er| [er.node_id, er.relay_id])).collect();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return Ok(Targets::default());
    }
    let silences = Silences::load(db).await?;
    let hosts: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as("select id, host_id from machine where id = any($1)").bind(&ids).fetch_all(db).await?;
    let maint = hosts.iter().filter(|(id, host)| silences.maintenance(*id) || host.is_some_and(|h| silences.maintenance(h))).map(|(id, _)| *id).collect();
    let firing: Vec<Uuid> =
        sqlx::query_scalar("select distinct subject_id from incident where state = 'firing' and engine = 'machine' and subject_id = any($1)").bind(&ids).fetch_all(db).await?;
    Ok(Targets { maint, firing: firing.into_iter().collect() })
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
            offline_held: held(ok, now, crate::views::AGENT_OFFLINE_S),
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
    fn offline_after_two_minutes_only() {
        let now = OffsetDateTime::now_utc();
        assert!(problems(&view("offline", None, 90, now), None, &Targets::default(), now).0.is_empty(), "不到 2 分钟");
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
        assert_eq!(p, vec![Problem { rule: COLLECT_ERROR, severity: "warning", text: "连得上，但没读到状态：采集脚本出错（退出码 1）：Syntax error".into(), hold_s: DEBOUNCE_S }]);
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
        }
        s.exits.push(Exit { name: "节点05".into(), sync: "ok".into(), entry: Some(er(Uuid::from_u128(5), "团队2-05")), ..Exit::default() });
        let v = view("online", Some(s.clone()), 0, now);
        let ex = |tg: &Targets| problems(&v, None, tg, now).0.into_iter().find(|p| p.rule == EXITS_DOWN);
        assert_eq!(ex(&Targets::default()).unwrap().text, "3 个出口连续两次延迟测试不通：节点01、节点02、节点03");
        let tg = Targets { maint: [n3].into(), firing: [relay].into() };
        assert_eq!(ex(&tg).unwrap().text, "3 个出口连续两次延迟测试不通：节点01、节点02、节点03（其中 2 个指向的团队2-03 正在维护，1 个指向的线路机2 正在告警）");
        assert!(!exits_paused(&s, &tg));
        // 不通的都指向维护中的（线路机在维护）：不开，评估时开着的保持
        let tg = Targets { maint: [relay].into(), firing: HashSet::new() };
        assert!(ex(&tg).is_none() && exits_paused(&s, &tg));
    }

    #[test]
    fn rules_have_names() {
        for r in [OFFLINE, COLLECT_ERROR, OPENCLASH_DOWN, EXITS_DOWN, WAN_DOWN, RESOURCE_HIGH] {
            assert_ne!(crate::meta::rule_label(r), r, "{r} 没有中文名");
        }
        assert_eq!(subject(Uuid::nil(), None)["name"], "（已删除）");
    }
}
