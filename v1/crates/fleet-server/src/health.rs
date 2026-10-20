//! 健康判定：界面状态和告警引擎共用这一处。
//!
//! 只对 `active` 的机器判定；其它生命周期直接返回对应状态，不参与告警、统计。

use crate::views::MachineView;
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
}

#[derive(Debug, Clone, Serialize)]
pub struct Reason {
    pub key: &'static str,
    pub severity: &'static str,
    pub text: String,
}

/// 线路机期望状态未应用超过这么久算异常
const APPLY_LAG_S: i64 = 120;

/// 持续高负载阈值（%）：最近 5 分钟的每个样本都不低于它才报
pub const CPU_HIGH_PCT: f32 = 90.0;
pub const MEM_HIGH_PCT: f32 = 90.0;
pub const DISK_HIGH_PCT: f32 = 85.0;
/// 样本至少覆盖这么多秒才判定持续高负载（窗口 5 分钟，留出一次上报间隔的余量）
const SUSTAIN_MIN_SPAN_S: i64 = 270;

pub fn evaluate(m: &MachineView, now: OffsetDateTime) -> Health {
    if m.lifecycle != "active" {
        return Health { status: Status::Inactive, reasons: vec![] };
    }
    if m.maintenance {
        return Health { status: Status::Maintenance, reasons: vec![] };
    }
    let mut rs: Vec<Reason> = vec![];
    let mut unknown = false;
    let agent_online = m.agent.as_ref().is_some_and(|a| a.online);

    if let Some(a) = &m.agent {
        if !a.online {
            let ago = a.last_report_at.map(|t| (now - t).whole_minutes());
            rs.push(Reason {
                key: "agent_offline",
                severity: "critical",
                text: match ago {
                    Some(m) => format!("{m} 分钟没有上报"),
                    None => "从未上报".into(),
                },
            });
        }
    }

    // 持续高负载：只在 agent 在线时判定（离线本身已经是严重告警）
    if agent_online && m.sustained.span_s >= SUSTAIN_MIN_SPAN_S {
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
            if let Some(r) = &m.relay {
                // 从期望代数最近一次变化起算（没有记录就退回上次应用时间）
                let since = r.generation_at.or(r.applied_at);
                let lag = r.generation > r.applied_generation && since.map(|t| (now - t).whole_seconds() > APPLY_LAG_S).unwrap_or(true);
                if lag {
                    rs.push(Reason {
                        key: "relay_apply_lag",
                        severity: "warning",
                        text: format!("配置未应用（期望 {}，已应用 {}）", r.generation, r.applied_generation),
                    });
                }
                if let Some(e) = &r.apply_error {
                    rs.push(Reason { key: "relay_apply_error", severity: "critical", text: format!("应用配置失败：{e}") });
                }
            }
        }
        _ => {
            let live: Vec<_> = m.entries.iter().filter(|e| e.lifecycle == "active").collect();
            if live.is_empty() {
                rs.push(Reason { key: "no_entry", severity: "warning", text: "没有在用的入口".into() });
            }
            for e in live {
                // 线路机的 Xray 没在运行时，经它的探测必然失败：不在节点上重复报严重告警（线路机自己有
                // relay_xray_down），这条入口按「未知」处理。
                if e.relay_xray_down {
                    unknown = true;
                    continue;
                }
                match &e.egress {
                    Some(p) if !p.ok => rs.push(Reason {
                        key: "egress_fail",
                        severity: "critical",
                        text: format!("{}:{} 出口不通：{}", e.relay.name, e.port, p.detail.clone().unwrap_or_default()),
                    }),
                    None => unknown = true,
                    _ => {}
                }
                if let Some(p) = &e.tcp {
                    if !p.ok {
                        rs.push(Reason {
                            key: "tcp_fail",
                            severity: "critical",
                            text: format!("{} 到节点 TCP 不通", e.relay.name),
                        });
                    }
                }
            }
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
            rs.push(Reason { key: "renewal_due", severity: "warning", text: format!("{} 天后到期", s.due_days) });
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
    Health { status, reasons: rs }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::{Agent, EntryView, Probe, Ref, RelayInfo, Subscription, Sustained, Traffic};
    use time::Duration;
    use uuid::Uuid;

    fn base(kind: &str) -> MachineView {
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
            agent: Some(Agent { version: None, last_report_at: Some(now), online: true, xray_running: Some(true) }),
            host: None,
            entries: vec![],
            removed_traffic: Default::default(),
            health: Health::default(),
            active_job: None,
            open_incidents: 0,
            maintenance: false,
            relay: None,
            sustained: Sustained::default(),
            policy_blocked: false,
        }
    }

    fn keys(h: &Health) -> Vec<&'static str> { h.reasons.iter().map(|r| r.key).collect() }

    fn sub(due_days: i64) -> Subscription {
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
