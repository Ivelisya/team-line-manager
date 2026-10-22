//! 软路由的告警（规则 `router_` 开头，对象是路由：incident.subject_id = router.id）。采集每轮之后评估一次，
//! 写法同 selfmon.rs：此刻成立的问题对齐到 incident 表，开、关、通知走 notify::enqueue（渠道的最低级别、严重的每 4 小时提醒、
//! 全局静默与维护都同机器的告警；路由本身的静默第二期再做）。与「降级算正常、故障才是异常」的口径一致：
//!
//! | 规则 | 级别 | 开 |
//! |---|---|---|
//! | router_offline | 严重 | 接入过（换了这把隧道密钥之后连上过）、此刻连不上超过 2 分钟 |
//! | router_openclash_down | 严重 | OpenClash 启用了、核心没在跑（满 60 秒） |
//! | router_exits_down | 警告；全部出口都不通升严重 | 有出口连续两次延迟测试不通（每 5 分钟测一次） |
//! | router_wan_down | 严重 | WAN 口（逻辑接口 wan）断开（满 60 秒） |
//! | router_resource_high | 警告 | CPU > 90% 持续 10 分钟、内存 > 90%、存储 > 90%、连接表 > 80%（后三项满 60 秒） |
//!
//! 恢复：原因消失满 60 秒。连不上的路由其余几条无从判断，保持原状（不开也不关）。路由删掉时它的告警直接关掉、不发恢复。
//! 严重度只升不降（升到严重马上再发一次「触发」），同 alerts.rs。

use super::collect::Snapshot;
use crate::alerts::{repeat_interval_s, same_summary};
use crate::notify::{self, Kind, Notice};
use crate::silence::Silences;
use crate::state::{AppState, Event};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use time::OffsetDateTime;
use uuid::Uuid;

pub const OFFLINE: &str = "router_offline";
pub const OPENCLASH_DOWN: &str = "router_openclash_down";
pub const EXITS_DOWN: &str = "router_exits_down";
pub const WAN_DOWN: &str = "router_wan_down";
pub const RESOURCE_HIGH: &str = "router_resource_high";

/// 连不上满这么久才开离线告警
const OFFLINE_S: i64 = 120;
/// 开（不带时长的条件）、关的去抖
const DEBOUNCE_S: i64 = 60;
/// CPU 持续偏高满这么久
const CPU_SUSTAIN_S: i64 = 600;
/// 时长判定的余量：采集每 30 秒一轮、每轮耗时不一，「满 60 秒」实际量到的是 59.x 秒，差这几秒不该再多等一轮
const SLACK_S: i64 = 5;

/// 从 `since` 到 `now` 已经满 `secs` 秒（含余量）
fn held(since: OffsetDateTime, now: OffsetDateTime, secs: i64) -> bool { (now - since).whole_seconds() + SLACK_S >= secs }

pub fn is_router(rule: &str) -> bool { rule.starts_with("router_") }

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
    pub error: Option<String>,
    pub last_ok_at: Option<OffsetDateTime>,
    pub snap: Option<Snapshot>,
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

/// 这台路由此刻的问题；连不上时只判离线，其余几条返回 None（无从判断，保持原状）
pub fn problems(v: &View, cpu_high_since: Option<OffsetDateTime>, now: OffsetDateTime) -> (Vec<Problem>, bool) {
    let mut out = vec![];
    if v.state != "online" {
        if v.state == "offline" {
            if let Some(ok) = v.last_ok_at.filter(|ok| held(*ok, now, OFFLINE_S)) {
                out.push(Problem {
                    rule: OFFLINE,
                    severity: "critical",
                    text: format!("连不上已 {}：{}", notify::fmt_duration((now - ok).whole_seconds()), v.error.as_deref().unwrap_or("原因不明")),
                    hold_s: 0,
                });
            }
        }
        return (out, false);
    }
    let Some(s) = &v.snap else { return (out, false) };
    let oc = &s.openclash;
    if oc.enabled && !oc.running {
        out.push(Problem { rule: OPENCLASH_DOWN, severity: "critical", text: "OpenClash 已启用，但核心（clash）没在运行：客户端走不了代理".into(), hold_s: DEBOUNCE_S });
    }
    let down: Vec<&str> = s.exits.iter().filter(|e| e.fail_streak >= 2).map(|e| e.name.as_str()).collect();
    if !down.is_empty() {
        let all = down.len() == s.exits.len();
        let mut names = down.iter().take(8).copied().collect::<Vec<_>>().join("、");
        if down.len() > 8 {
            names.push_str(&format!(" 等 {} 个", down.len()));
        }
        let text = if all {
            format!("全部 {} 个出口连续两次延迟测试不通：{names}", down.len())
        } else {
            format!("{} 个出口连续两次延迟测试不通：{names}", down.len())
        };
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

fn rank(s: &str) -> u8 {
    match s {
        "critical" => 2,
        "warning" => 1,
        _ => 0,
    }
}

/// 评估一轮，把路由的告警对齐到 incident 表。`now` 由调用方给（测试可以快进）
pub async fn evaluate(st: &AppState, state: &mut AlertState, now: OffsetDateTime) -> anyhow::Result<()> {
    let rows = super::load(&st.db, None).await?;
    let views: Vec<View> = rows
        .iter()
        .map(|r| View { id: r.id, name: r.name.clone(), state: r.state(), error: r.error.clone(), last_ok_at: r.last_ok_at, snap: r.snapshot() })
        .collect();
    let open: Vec<Open> = sqlx::query_as(
        "select id, rule, subject_id, severity::text severity, summary, opened_at, notified_at from incident
         where state = 'firing' and rule like 'router!_%' escape '!'",
    )
    .fetch_all(&st.db)
    .await?;
    let silences = Silences::load(&st.db).await?;
    let mut open: HashMap<(String, Uuid), Open> = open.into_iter().map(|o| ((o.rule.clone(), o.subject_id), o)).collect();
    let open_ids: HashSet<Uuid> = open.values().map(|o| o.id).collect();
    let mut seen: HashSet<(&'static str, Uuid)> = HashSet::new();
    let mut known: HashSet<Uuid> = HashSet::new();
    let mut changed = false;
    for v in &views {
        // CPU 从什么时候起一直超过 90%（连不上的这一轮不动它）
        if v.state == "online" {
            match v.snap.as_ref().and_then(|s| s.system.cpu_pct) {
                Some(c) if c > 90.0 => {
                    state.cpu_high_since.entry(v.id).or_insert(now);
                }
                _ => {
                    state.cpu_high_since.remove(&v.id);
                }
            }
        }
        let (ps, judged) = problems(v, state.cpu_high_since.get(&v.id).copied(), now);
        if judged {
            known.insert(v.id);
        }
        let notice = |kind, id, rule: &str, severity: &str, text: &str, opened_at| Notice {
            kind,
            incident_id: Some(id),
            rule: rule.into(),
            severity: severity.into(),
            machine: v.name.clone(),
            summary: text.into(),
            at: now,
            opened_at: Some(opened_at),
        };
        for p in ps {
            let key = (p.rule, v.id);
            seen.insert(key);
            let since = *state.first_seen.entry(key).or_insert(now);
            let summary = format!("{}：{}", v.name, p.text);
            let silenced = silences.matches(p.rule, v.id);
            if let Some(inc) = open.remove(&(p.rule.to_string(), v.id)) {
                state.clean_since.remove(&inc.id);
                let escalated = rank(p.severity) > rank(&inc.severity);
                let sev = if escalated { p.severity } else { inc.severity.as_str() };
                if escalated || !same_summary(&inc.summary, &summary) {
                    sqlx::query("update incident set summary = $2, severity = $3::severity, last_seen_at = now() where id = $1")
                        .bind(inc.id)
                        .bind(&summary)
                        .bind(sev)
                        .execute(&st.db)
                        .await?;
                    changed = true;
                }
                // 开的时候没能写进发送队列的补发「触发」；升到严重马上再发一次；严重的每 4 小时提醒一次
                let kind = match inc.notified_at {
                    _ if escalated => Some(Kind::Firing),
                    None => Some(Kind::Firing),
                    Some(n) => repeat_interval_s(p.rule, sev).filter(|every| (now - n).whole_seconds() >= *every).map(|_| Kind::Repeat),
                };
                if let Some(kind) = kind.filter(|_| !silenced) {
                    notify::enqueue(st, notice(kind, inc.id, p.rule, sev, &p.text, inc.opened_at)).await;
                }
                continue;
            }
            if (p.hold_s > 0 && !held(since, now, p.hold_s)) || silenced {
                continue;
            }
            let id = crate::db::new_id();
            let res = sqlx::query(
                "insert into incident (id, rule, subject_id, severity, summary, detail)
                 select $1, $2, $3, $4::severity, $5, $6 where exists (select 1 from router where id = $3)
                 on conflict (rule, subject_id) where state = 'firing' do nothing",
            )
            .bind(id)
            .bind(p.rule)
            .bind(v.id)
            .bind(p.severity)
            .bind(&summary)
            .bind(json!({"name": v.name, "kind": "router"}))
            .execute(&st.db)
            .await?;
            if res.rows_affected() > 0 {
                changed = true;
                notify::enqueue(st, notice(Kind::Firing, id, p.rule, p.severity, &p.text, now)).await;
            }
        }
    }
    state.first_seen.retain(|k, _| seen.contains(k));
    let names: HashMap<Uuid, &str> = views.iter().map(|v| (v.id, v.name.as_str())).collect();
    for (_, inc) in open {
        let Some(name) = names.get(&inc.subject_id).copied() else {
            // 路由已经删了（删除接口会关掉，这里兜底）：关掉、不发恢复
            close(st, inc.id, Some("router_deleted")).await?;
            changed = true;
            continue;
        };
        // 连不上的路由：除离线外无从判断，保持原状
        if inc.rule != OFFLINE && !known.contains(&inc.subject_id) {
            state.clean_since.remove(&inc.id);
            continue;
        }
        let since = *state.clean_since.entry(inc.id).or_insert(now);
        if !held(since, now, DEBOUNCE_S) {
            continue;
        }
        state.clean_since.remove(&inc.id);
        close(st, inc.id, None).await?;
        changed = true;
        if inc.notified_at.is_some() && !silences.matches(&inc.rule, inc.subject_id) {
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
    state.clean_since.retain(|id, _| open_ids.contains(id));
    let ids: HashSet<Uuid> = views.iter().map(|v| v.id).collect();
    state.cpu_high_since.retain(|id, _| ids.contains(id));
    if changed {
        st.emit(Event::Incidents);
    }
    Ok(())
}

async fn close(st: &AppState, id: Uuid, closed_by: Option<&str>) -> sqlx::Result<()> {
    let detail = closed_by.map(|c| json!({"closed_by": c})).unwrap_or_else(|| json!({}));
    sqlx::query("update incident set state = 'resolved', resolved_at = now(), detail = detail || $2 where id = $1 and state = 'firing'")
        .bind(id)
        .bind(detail)
        .execute(&st.db)
        .await
        .map(|_| ())
}

/// 删除路由时（同一事务）关掉它在告警中的告警：不发恢复
pub async fn close_all(tx: &mut crate::db::Tx<'_>, router: Uuid) -> sqlx::Result<u64> {
    Ok(sqlx::query(
        "update incident set state = 'resolved', resolved_at = now(), detail = detail || '{\"closed_by\": \"router_deleted\"}'
         where subject_id = $1 and state = 'firing' and rule like 'router!_%' escape '!'",
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
    use crate::routers::collect::{Exit, Iface, OpenClash, System};

    fn view(state: &'static str, snap: Option<Snapshot>, last_ok_s: i64, now: OffsetDateTime) -> View {
        View { id: Uuid::nil(), name: "团队2-路由A".into(), state, error: Some("经隧道连不上路由：Connection refused".into()), last_ok_at: Some(now - time::Duration::seconds(last_ok_s)), snap }
    }

    fn healthy() -> Snapshot {
        Snapshot {
            system: System { cpu_pct: Some(10.0), mem_used: Some(1), mem_total: Some(4), disk_used: Some(1), disk_total: Some(10), conntrack: Some(10), conntrack_max: Some(100), ..System::default() },
            interfaces: vec![Iface { name: "wan".into(), device: Some("eth0".into()), up: true, ..Iface::default() }],
            openclash: OpenClash { enabled: true, running: true, ..OpenClash::default() },
            exits: (1..=3).map(|i| Exit { name: format!("节点0{i}"), ..Exit::default() }).collect(),
            ..Snapshot::default()
        }
    }

    #[test]
    fn offline_after_two_minutes_only() {
        let now = OffsetDateTime::now_utc();
        assert!(problems(&view("offline", None, 90, now), None, now).0.is_empty(), "不到 2 分钟");
        let (p, judged) = problems(&view("offline", Some(healthy()), 300, now), None, now);
        assert!(!judged, "连不上时其余几条无从判断");
        assert_eq!((p.len(), p[0].rule, p[0].severity), (1, OFFLINE, "critical"));
        assert_eq!(p[0].text, "连不上已 5 分钟：经隧道连不上路由：Connection refused");
        assert!(problems(&view("pending", None, 3000, now), None, now).0.is_empty(), "还没接入的不报离线");
    }

    #[test]
    fn thresholds() {
        let now = OffsetDateTime::now_utc();
        let ok = view("online", Some(healthy()), 0, now);
        assert_eq!(problems(&ok, None, now), (vec![], true));
        let mut s = healthy();
        s.openclash.running = false;
        s.interfaces[0].up = false;
        s.exits[1].fail_streak = 2;
        s.exits[2].fail_streak = 1;
        s.system.mem_used = Some(368);
        s.system.mem_total = Some(400);
        s.system.conntrack = Some(81);
        s.system.cpu_pct = Some(95.0);
        let (p, _) = problems(&view("online", Some(s.clone()), 0, now), Some(now - time::Duration::seconds(700)), now);
        let rules: Vec<&str> = p.iter().map(|p| p.rule).collect();
        assert_eq!(rules, [OPENCLASH_DOWN, EXITS_DOWN, WAN_DOWN, RESOURCE_HIGH]);
        assert_eq!((p[1].severity, p[1].text.as_str()), ("warning", "1 个出口连续两次延迟测试不通：节点02"));
        assert_eq!(p[2].text, "WAN 口断开（wan · eth0）");
        assert_eq!(p[3].text, "资源吃紧：CPU 95%（超过 90% 已 11 分钟）；内存 92%（368 B / 400 B）；连接表 81%（81 / 100）");
        // CPU 高还不到 10 分钟、内存 90% 整、连接表 80% 整都不算
        s.system.mem_used = Some(360);
        s.system.conntrack = Some(80);
        let (p, _) = problems(&view("online", Some(s.clone()), 0, now), Some(now - time::Duration::seconds(300)), now);
        assert!(!p.iter().any(|p| p.rule == RESOURCE_HIGH), "{p:?}");
        // 全部出口都不通：严重
        for e in &mut s.exits {
            e.fail_streak = 3;
        }
        let (p, _) = problems(&view("online", Some(s), 0, now), None, now);
        let ex = p.iter().find(|p| p.rule == EXITS_DOWN).unwrap();
        assert_eq!((ex.severity, ex.text.as_str()), ("critical", "全部 3 个出口连续两次延迟测试不通：节点01、节点02、节点03"));
    }

    #[test]
    fn rules_have_names() {
        for r in [OFFLINE, OPENCLASH_DOWN, EXITS_DOWN, WAN_DOWN, RESOURCE_HIGH] {
            assert!(is_router(r));
            assert_ne!(notify::rule_name(r), r, "{r} 没有中文名");
        }
        assert!(!is_router("agent_offline"));
        assert_eq!(subject(Uuid::nil(), None)["name"], "（已删除）");
    }
}
