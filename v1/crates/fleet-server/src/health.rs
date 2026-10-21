//! 健康判定：界面状态和告警引擎共用这一处。
//!
//! 只对 `active` 的机器判定；其它生命周期直接返回对应状态，不参与告警、统计。

use crate::views::{EntryView, MachineView};
use serde::Serialize;
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Degraded,
    Down,
    #[default]
    Unknown,
    Maintenance,
    /// 非 active：上架中、退役中、已退役、失败
    Inactive,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Health {
    pub status: Status,
    /// 机器可读的原因键（告警规则名同名）
    pub reasons: Vec<Reason>,
    /// 有在用入口 15 分钟内没有探测结果（线路机 agent 离线、探测停了）：出口 / TCP 判不出好坏，
    /// 告警引擎不能把这两条规则的原因消失当成恢复（只给告警用，不对外输出）
    #[serde(skip)]
    pub probe_unknown: bool,
}

#[derive(Debug, Clone)]
pub struct Reason {
    pub key: &'static str,
    pub severity: &'static str,
    pub text: String,
}

/// 序列化时带上规则中文名 `rule_name`（同通知、告警列表）
impl Serialize for Reason {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut x = s.serialize_struct("Reason", 4)?;
        x.serialize_field("key", self.key)?;
        x.serialize_field("rule_name", crate::notify::rule_name(self.key))?;
        x.serialize_field("severity", self.severity)?;
        x.serialize_field("text", &self.text)?;
        x.end()
    }
}

/// 线路机期望状态未应用超过这么久算异常
const APPLY_LAG_S: i64 = 120;

/// 持续高负载阈值（%）：最近 5 分钟的每个样本都不低于它才报
pub const CPU_HIGH_PCT: f32 = 90.0;
pub const MEM_HIGH_PCT: f32 = 90.0;
pub const DISK_HIGH_PCT: f32 = 85.0;
/// 样本至少覆盖这么多秒才判定持续高负载（窗口 5 分钟，留出一次上报间隔的余量）
const SUSTAIN_MIN_SPAN_S: i64 = 270;
/// agent 时钟偏差超过这么久（毫秒）报警告：样本按 agent 的时间入库，当前读数只看最近 15 分钟、持续负载看最近 5 分钟
const CLOCK_SKEW_MAX_MS: i64 = 60_000;

/// 在用入口的探测结果 → egress_fail / tcp_fail。
/// 同一条规则一台机器只开一个告警：几个入口同时失败合成一条原因，文字列出全部入口，
/// 其中一个恢复时摘要随之变化，不会悄悄换成另一个入口
fn probe_reasons(live: &[&EntryView], rs: &mut Vec<Reason>, unknown: &mut bool, probe_unknown: &mut bool) {
    let (mut egress, mut details, mut tcp) = (vec![], vec![], vec![]);
    for e in live {
        // 线路机的 Xray 没在运行时，经它的探测必然失败：不在节点上重复报严重告警（线路机自己有
        // relay_xray_down），这条入口按「未知」处理。
        if e.relay_xray_down {
            *unknown = true;
            continue;
        }
        // 15 分钟内没有探测结果（视图只取这么久的）：这条入口通不通不知道。直出入口本来就没有 TCP 探测
        if e.egress.is_none() || (e.tcp.is_none() && !e.direct) {
            *probe_unknown = true;
        }
        match &e.egress {
            Some(p) if !p.ok => {
                egress.push(format!("{}:{}", e.relay.name, e.port));
                let d = p.detail.clone().unwrap_or_default();
                if !d.is_empty() && !details.contains(&d) {
                    details.push(d);
                }
            }
            None => *unknown = true,
            _ => {}
        }
        if e.tcp.as_ref().is_some_and(|p| !p.ok) {
            tcp.push(e.relay.name.clone());
        }
    }
    if !egress.is_empty() {
        rs.push(Reason { key: "egress_fail", severity: "critical", text: format!("{} 出口不通：{}", egress.join("、"), details.join("；")) });
    }
    if !tcp.is_empty() {
        rs.push(Reason { key: "tcp_fail", severity: "critical", text: format!("{} 到节点 TCP 不通", tcp.join("、")) });
    }
}

pub fn evaluate(m: &MachineView, now: OffsetDateTime) -> Health {
    if m.lifecycle != "active" {
        return Health { status: Status::Inactive, ..Default::default() };
    }
    if m.maintenance {
        return Health { status: Status::Maintenance, ..Default::default() };
    }
    let mut rs: Vec<Reason> = vec![];
    let mut unknown = false;
    let mut probe_unknown = false;
    let agent_online = m.agent.as_ref().is_some_and(|a| a.online);
    // 同机 IP 节点的 agent 与主机读数是宿主机的：agent 离线、时钟、持续高负载只由宿主机报，入口和续费照常自己报
    let own_agent = m.host_machine.is_none();

    if let Some(a) = m.agent.as_ref().filter(|_| own_agent) {
        if !a.online {
            let ago = a.last_report_at.map(|t| (now - t).whole_seconds());
            rs.push(Reason {
                key: "agent_offline",
                severity: "critical",
                text: match ago {
                    Some(s) => format!("{}没有上报", crate::notify::fmt_duration(s)),
                    None => "从未上报".into(),
                },
            });
        }
        // 同持续高负载，只在 agent 在线时判定
        if let Some(ms) = a.clock_skew_ms.filter(|ms| a.online && ms.abs() > CLOCK_SKEW_MAX_MS) {
            rs.push(Reason {
                key: "clock_skew",
                severity: "warning",
                text: format!("agent 时钟{} {}：读数可能缺失或冻结，请校时", if ms > 0 { "慢" } else { "快" }, crate::notify::fmt_duration(ms.abs() / 1000)),
            });
        }
    }

    // 持续高负载：只在 agent 在线时判定（离线本身已经是严重告警）
    if own_agent && agent_online && m.sustained.span_s >= SUSTAIN_MIN_SPAN_S {
        let s = &m.sustained;
        for (key, label, min, limit) in [
            ("host_cpu_high", "CPU", s.cpu_min, CPU_HIGH_PCT),
            ("host_mem_high", "内存", s.mem_min, MEM_HIGH_PCT),
            ("host_disk_high", "磁盘", s.disk_min, DISK_HIGH_PCT),
        ] {
            if let Some(v) = min.filter(|v| *v >= limit) {
                rs.push(Reason { key, severity: "warning", text: format!("{label} 持续 5 分钟高于 {limit:.0}%（最低 {v:.0}%）") });
            }
        }
    }

    match m.kind.as_str() {
        "relay" => {
            if agent_online && m.agent.as_ref().and_then(|a| a.xray_running) == Some(false) {
                rs.push(Reason { key: "relay_xray_down", severity: "critical", text: "Xray 未运行".into() });
            }
            // 直出入口（节点就是这台线路机）：出口不通照节点入口报 egress_fail；没有 TCP 探测，也不报 no_entry。
            // 挂在它上面的节点入口仍由各自的节点报
            let direct: Vec<&EntryView> = m.entries.iter().filter(|e| e.direct && e.lifecycle == "active").collect();
            probe_reasons(&direct, &mut rs, &mut unknown, &mut probe_unknown);
            if let Some(r) = &m.relay {
                // 从期望代数最近一次变化起算（没有记录就退回上次应用时间）
                let since = r.generation_at.or(r.applied_at);
                let lag = r.generation > r.applied_generation && since.map(|t| (now - t).whole_seconds() > APPLY_LAG_S).unwrap_or(true);
                if lag {
                    rs.push(Reason {
                        key: "relay_apply_lag",
                        severity: "warning",
                        text: format!("配置未应用（配置代数 {} · 已应用 {}）", r.generation, r.applied_generation),
                    });
                }
                if let Some(e) = &r.apply_error {
                    rs.push(Reason { key: "relay_apply_error", severity: "critical", text: format!("应用失败：{e}") });
                }
            }
        }
        _ => {
            let live: Vec<_> = m.entries.iter().filter(|e| e.lifecycle == "active").collect();
            if live.is_empty() {
                rs.push(Reason { key: "no_entry", severity: "warning", text: "没有在用的入口".into() });
            }
            probe_reasons(&live, &mut rs, &mut unknown, &mut probe_unknown);
        }
    }

    // 访问策略暂停了新连接：提示一下为什么没有流量。info 级别：不算故障，也不产生告警
    if m.policy_blocked {
        rs.push(Reason { key: "policy_blocked", severity: "info", text: "访问策略：已暂停新连接".into() });
    }

    // 续费提醒：线路机和节点都有账单
    if let Some(s) = &m.subscription {
        if s.due_days < 0 {
            rs.push(Reason { key: "renewal_overdue", severity: "critical", text: format!("已过期 {} 天", -s.due_days) });
        } else if s.due_days <= s.remind_days as i64 && !s.auto_renew {
            let text = if s.due_days == 0 { "今天到期".into() } else { format!("{} 天后到期", s.due_days) };
            rs.push(Reason { key: "renewal_due", severity: "warning", text });
        }
    }

    let status = if rs.iter().any(|r| r.severity == "critical") {
        Status::Down
    } else if rs.iter().any(|r| r.severity != "info") {
        Status::Degraded
    } else if unknown {
        Status::Unknown
    } else {
        Status::Ok
    };
    Health { status, reasons: rs, probe_unknown }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::views::{Agent, EntryView, Probe, Quota, Ref, RelayInfo, Subscription, Sustained, Traffic};
    use time::Duration;
    use uuid::Uuid;

    pub(crate) fn base(kind: &str) -> MachineView {
        let now = OffsetDateTime::now_utc();
        MachineView {
            id: Uuid::now_v7(),
            kind: kind.into(),
            name: "m".into(),
            team: None,
            number: None,
            label: None,
            ip: "192.0.2.1".into(),
            ssh_port: 22,
            host_machine: None,
            ip_nodes: vec![],
            provider: None,
            provider_code: None,
            plan: None,
            service_ref: None,
            lifecycle: "active".into(),
            lifecycle_reason: None,
            note: String::new(),
            revision: 1,
            created_at: now,
            activated_at: Some(now),
            retired_at: None,
            subscription: None,
            agent: Some(Agent { version: None, last_report_at: Some(now), online: true, xray_running: Some(true), cores: None, health: None, clock_skew_ms: None }),
            host: None,
            entries: vec![],
            removed_traffic: Default::default(),
            quota: Quota { used: 0, period_start: now.date() },
            health: Health::default(),
            active_job: None,
            failed_job: None,
            open_incidents: 0,
            maintenance: false,
            relay: None,
            sustained: Sustained::default(),
            policy_blocked: false,
        }
    }

    fn keys(h: &Health) -> Vec<&'static str> { h.reasons.iter().map(|r| r.key).collect() }

    pub(crate) fn sub(due_days: i64) -> Subscription {
        Subscription {
            price_cents: 100,
            currency: "USD".into(),
            cycle: "monthly".into(),
            due_date: time::macros::date!(2030 - 01 - 01),
            due_days,
            billing_tz: "Asia/Shanghai".into(),
            auto_renew: false,
            remind_days: 7,
            traffic_gb: None,
            traffic_reset_day: None,
        }
    }

    fn relay_info(generation: i64, applied: i64, generation_at: Option<OffsetDateTime>, applied_at: Option<OffsetDateTime>) -> RelayInfo {
        RelayInfo { public_host: "h".into(), generation, applied_generation: applied, applied_at, generation_at, apply_error: None, port_min: 20000, port_max: 29999 }
    }

    fn entry(relay_xray_down: bool, egress_ok: bool) -> EntryView {
        let ts = OffsetDateTime::now_utc();
        EntryView {
            id: Uuid::now_v7(),
            relay: Ref { id: Uuid::now_v7(), name: "线路机1".into() },
            node: Ref { id: Uuid::now_v7(), name: "n".into() },
            direct: false,
            port: 20000,
            lifecycle: "active".into(),
            applied: true,
            apply_error: None,
            relay_xray_down,
            tcp: Some(Probe { ok: egress_ok, latency_ms: None, detail: None, ts }),
            egress: Some(Probe { ok: egress_ok, latency_ms: None, detail: Some("timeout".into()), ts }),
            traffic: Traffic::default(),
        }
    }

    #[test]
    fn relay_gets_renewal_alerts() {
        let mut m = base("relay");
        m.subscription = Some(sub(3));
        assert_eq!(keys(&evaluate(&m, OffsetDateTime::now_utc())), ["renewal_due"]);
        assert_eq!(evaluate(&m, OffsetDateTime::now_utc()).reasons[0].text, "3 天后到期");
        m.subscription = Some(sub(0));
        assert_eq!(evaluate(&m, OffsetDateTime::now_utc()).reasons[0].text, "今天到期", "和前端的“今天”一致");
        m.subscription = Some(sub(-2));
        let h = evaluate(&m, OffsetDateTime::now_utc());
        assert_eq!(keys(&h), ["renewal_overdue"]);
        assert_eq!(h.status, Status::Down);
    }

    #[test]
    fn relay_xray_down_is_critical_and_suppresses_node_probe_alerts() {
        let mut r = base("relay");
        r.agent.as_mut().unwrap().xray_running = Some(false);
        assert_eq!(keys(&evaluate(&r, OffsetDateTime::now_utc())), ["relay_xray_down"]);
        // agent 离线时不再另报 xray（离线本身已是严重告警）
        r.agent.as_mut().unwrap().online = false;
        assert_eq!(keys(&evaluate(&r, OffsetDateTime::now_utc())), ["agent_offline"]);

        let mut n = base("vps");
        n.entries = vec![entry(true, false)];
        let h = evaluate(&n, OffsetDateTime::now_utc());
        assert!(h.reasons.is_empty(), "线路机 Xray 挂了，节点不重复报：{:?}", keys(&h));
        assert_eq!(h.status, Status::Unknown);
        n.entries = vec![entry(false, false)];
        assert_eq!(keys(&evaluate(&n, OffsetDateTime::now_utc())), ["egress_fail", "tcp_fail"]);
    }

    #[test]
    fn same_rule_failures_merge_into_one_reason() {
        let now = OffsetDateTime::now_utc();
        let mut n = base("vps");
        let (mut a, mut b) = (entry(false, false), entry(false, false));
        (a.relay.name, a.port, b.relay.name, b.port) = ("线路机A".into(), 20001, "线路机B".into(), 20003);
        b.egress.as_mut().unwrap().detail = Some("refused".into());
        n.entries = vec![a, b];
        let h = evaluate(&n, now);
        assert_eq!(keys(&h), ["egress_fail", "tcp_fail"], "每条规则只一条原因");
        assert_eq!(h.reasons[0].text, "线路机A:20001、线路机B:20003 出口不通：timeout；refused");
        assert_eq!(h.reasons[1].text, "线路机A、线路机B 到节点 TCP 不通");
        // 恢复一个：摘要只剩另一个
        n.entries[0] = entry(false, true);
        assert_eq!(evaluate(&n, now).reasons[0].text, "线路机B:20003 出口不通：refused");
    }

    #[test]
    fn missing_probes_mark_probe_unknown() {
        let now = OffsetDateTime::now_utc();
        let mut n = base("vps");
        n.entries = vec![entry(false, true)];
        assert!(!evaluate(&n, now).probe_unknown);
        n.entries[0].egress = None;
        let h = evaluate(&n, now);
        assert!(h.probe_unknown && h.status == Status::Unknown);
        assert!(serde_json::to_value(&h).unwrap().get("probe_unknown").is_none(), "不对外输出");
        // 一条入口失败、另一条没有 TCP 探测：照常报，同时标记（告警不能因另一条入口判不出而恢复）
        n.entries = vec![entry(false, false), entry(false, true)];
        n.entries[1].tcp = None;
        let h = evaluate(&n, now);
        assert_eq!(keys(&h), ["egress_fail", "tcp_fail"]);
        assert!(h.probe_unknown);
        // 线路机 Xray 没在运行的入口交给线路机的告警，不算探测过期
        n.entries = vec![entry(true, false)];
        assert!(!evaluate(&n, now).probe_unknown);
    }

    #[test]
    fn relay_direct_entry_alerts_on_egress_only() {
        let now = OffsetDateTime::now_utc();
        let mut r = base("relay");
        let direct = |ok: bool| {
            let mut e = entry(false, ok);
            (e.direct, e.tcp, e.relay.name) = (true, None, "线路机2".into());
            e
        };
        // 挂在线路机上的节点入口失败：由节点报，线路机自己不报；线路机也不报 no_entry
        r.entries = vec![entry(false, false)];
        let h = evaluate(&r, now);
        assert!(h.reasons.is_empty() && h.status == Status::Ok && !h.probe_unknown, "{:?}", keys(&h));
        // 直出入口通：没有 TCP 探测不算「判不出」
        r.entries = vec![direct(true), entry(false, false)];
        let h = evaluate(&r, now);
        assert!(h.reasons.is_empty() && h.status == Status::Ok && !h.probe_unknown, "{:?}", keys(&h));
        // 直出入口出口不通：线路机报 egress_fail，不报 tcp_fail
        r.entries = vec![direct(false)];
        let h = evaluate(&r, now);
        assert_eq!(keys(&h), ["egress_fail"]);
        assert_eq!(h.reasons[0].text, "线路机2:20000 出口不通：timeout");
        assert_eq!(h.status, Status::Down);
        // 还没有出口探测结果：未知，告警不能因此恢复
        r.entries[0].egress = None;
        let h = evaluate(&r, now);
        assert!(h.reasons.is_empty() && h.status == Status::Unknown && h.probe_unknown);
        // Xray 没在运行：只报 relay_xray_down
        r.entries = vec![direct(false)];
        r.entries[0].relay_xray_down = true;
        r.agent.as_mut().unwrap().xray_running = Some(false);
        assert_eq!(keys(&evaluate(&r, now)), ["relay_xray_down"]);
        // 停用的直出入口不判
        r = base("relay");
        r.entries = vec![direct(false)];
        r.entries[0].lifecycle = "disabled".into();
        assert!(evaluate(&r, now).reasons.is_empty());
    }

    #[test]
    fn offline_duration_is_human() {
        let now = OffsetDateTime::now_utc();
        let mut m = base("vps");
        m.entries = vec![entry(false, true)];
        let a = m.agent.as_mut().unwrap();
        (a.online, a.last_report_at) = (false, Some(now - Duration::days(3)));
        assert_eq!(evaluate(&m, now).reasons[0].text, "3 天 0 小时没有上报");
    }

    #[test]
    fn clock_skew_over_a_minute_warns() {
        let now = OffsetDateTime::now_utc();
        let mut m = base("vps");
        m.entries = vec![entry(false, true)];
        m.agent.as_mut().unwrap().clock_skew_ms = Some(59_000);
        assert!(keys(&evaluate(&m, now)).is_empty(), "一分钟以内不报");
        m.agent.as_mut().unwrap().clock_skew_ms = Some(17 * 60_000 + 500);
        let h = evaluate(&m, now);
        assert_eq!((keys(&h), h.status), (vec!["clock_skew"], Status::Degraded));
        assert_eq!(h.reasons[0].text, "agent 时钟慢 17 分钟：读数可能缺失或冻结，请校时");
        m.agent.as_mut().unwrap().clock_skew_ms = Some(-2 * 3_600_000);
        assert_eq!(evaluate(&m, now).reasons[0].text, "agent 时钟快 2 小时 0 分钟：读数可能缺失或冻结，请校时");
        // 离线时只报离线
        m.agent.as_mut().unwrap().online = false;
        assert_eq!(keys(&evaluate(&m, now)), ["agent_offline"]);
    }

    #[test]
    fn apply_lag_counts_from_generation_change() {
        let now = OffsetDateTime::now_utc();
        let mut r = base("relay");
        // 很久以前应用过，但刚刚才有新代数：不报
        r.relay = Some(relay_info(5, 4, Some(now - Duration::seconds(30)), Some(now - Duration::hours(3))));
        assert!(keys(&evaluate(&r, now)).is_empty());
        // 新代数已经 3 分钟没应用：报
        r.relay = Some(relay_info(5, 4, Some(now - Duration::minutes(3)), Some(now - Duration::hours(3))));
        assert_eq!(keys(&evaluate(&r, now)), ["relay_apply_lag"]);
        // generation_at 为空：退回 applied_at
        r.relay = Some(relay_info(5, 4, None, Some(now - Duration::minutes(3))));
        assert_eq!(keys(&evaluate(&r, now)), ["relay_apply_lag"]);
    }

    #[test]
    fn policy_blocked_is_informational_only() {
        let now = OffsetDateTime::now_utc();
        let mut n = base("vps");
        n.entries = vec![entry(false, true)];
        n.policy_blocked = true;
        let h = evaluate(&n, now);
        assert_eq!(keys(&h), ["policy_blocked"]);
        assert_eq!(h.reasons[0].severity, "info");
        assert_eq!(h.status, Status::Ok, "info 不降级");
        // 同时有真故障时照常降级
        n.entries = vec![entry(false, false)];
        assert_eq!(evaluate(&n, now).status, Status::Down);
    }

    #[test]
    fn ip_node_leaves_agent_and_host_reasons_to_its_host() {
        let now = OffsetDateTime::now_utc();
        let mut x = base("vps");
        x.host_machine = Some(Ref { id: Uuid::now_v7(), name: "团队1-03".into() });
        x.entries = vec![entry(false, true)];
        // 借来的宿主机 agent 离线、时钟偏、持续高负载：都由宿主机自己报
        let a = x.agent.as_mut().unwrap();
        (a.online, a.last_report_at, a.clock_skew_ms) = (false, Some(now - Duration::hours(1)), Some(10 * 60_000));
        x.sustained = Sustained { span_s: 290, cpu_min: Some(99.0), mem_min: Some(99.0), disk_min: Some(99.0) };
        let h = evaluate(&x, now);
        assert!(h.reasons.is_empty() && h.status == Status::Ok, "{:?}", keys(&h));
        x.agent.as_mut().unwrap().online = true;
        assert!(keys(&evaluate(&x, now)).is_empty());
        // 自己的入口、续费照常报
        x.entries = vec![entry(false, false)];
        x.subscription = Some(sub(2));
        assert_eq!(keys(&evaluate(&x, now)), ["egress_fail", "tcp_fail", "renewal_due"]);
        x.entries.clear();
        assert_eq!(keys(&evaluate(&x, now))[0], "no_entry");
    }

    #[test]
    fn sustained_load_thresholds() {
        let now = OffsetDateTime::now_utc();
        let mut m = base("vps");
        m.entries = vec![entry(false, true)];
        m.sustained = Sustained { span_s: 290, cpu_min: Some(95.0), mem_min: Some(50.0), disk_min: Some(86.0) };
        assert_eq!(keys(&evaluate(&m, now)), ["host_cpu_high", "host_disk_high"]);
        // 样本覆盖不足 5 分钟：不判定
        m.sustained.span_s = 60;
        assert!(keys(&evaluate(&m, now)).is_empty());
        // 内存 89% 没到 90%
        m.sustained = Sustained { span_s: 290, cpu_min: None, mem_min: Some(89.0), disk_min: Some(84.9) };
        assert!(keys(&evaluate(&m, now)).is_empty());
    }
}
