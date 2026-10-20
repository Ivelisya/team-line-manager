//! 健康判定：界面状态和告警引擎共用这一处。
//!
//! 只对 `active` 的机器判定；其它生命周期直接返回对应状态，不参与告警、统计。

use crate::views::{MachineView, AGENT_OFFLINE_S};
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
    /// 非 active：上架中、下线中、已退役、失败
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

pub fn evaluate(m: &MachineView, now: OffsetDateTime) -> Health {
    if m.lifecycle != "active" {
        return Health { status: Status::Inactive, reasons: vec![] };
    }
    if m.maintenance {
        return Health { status: Status::Maintenance, reasons: vec![] };
    }
    let mut rs: Vec<Reason> = vec![];
    let mut unknown = false;

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

    match m.kind.as_str() {
        "relay" => {
            if let Some(r) = &m.relay {
                let lag = r.generation > r.applied_generation
                    && r.applied_at.map(|t| (now - t).whole_seconds() > APPLY_LAG_S).unwrap_or(true);
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
            if let Some(s) = &m.subscription {
                if s.due_days < 0 {
                    rs.push(Reason { key: "renewal_overdue", severity: "critical", text: format!("已过期 {} 天", -s.due_days) });
                } else if s.due_days <= s.remind_days as i64 && !s.auto_renew {
                    rs.push(Reason { key: "renewal_due", severity: "warning", text: format!("{} 天后到期", s.due_days) });
                }
            }
        }
    }
    let _ = AGENT_OFFLINE_S;
    let status = if rs.iter().any(|r| r.severity == "critical") {
        Status::Down
    } else if !rs.is_empty() {
        Status::Degraded
    } else if unknown {
        Status::Unknown
    } else {
        Status::Ok
    };
    Health { status, reasons: rs }
}
