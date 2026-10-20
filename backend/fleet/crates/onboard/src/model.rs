//! 词表：16 步（与原型 onboarding.js / snapshot demo_batch 的 steps、phases 完全同形）、步骤状态、行状态、失败分类。

use serde::{Deserialize, Serialize};

/// (code, key, 中文名, 估算) —— 与 prototype/data/snapshot.json `onboarding.demo_batch.steps` 逐项一致；
/// 例外：P11 已由 komari-agent 改为 nodeagent（Komari 已停用），原型的静态快照仍是旧名。
pub const STEPS: [(&str, &str, &str, &str); 16] = [
    ("P1", "preflight", "预检", "30–45 s"),
    ("P2", "bootstrap_key", "推密钥", "5–10 s"),
    ("P3", "identify", "身份采集", "≈5 s"),
    ("P4", "dd_prepare", "重装准备", "20–40 s"),
    ("P5", "dd_execute", "重装执行", "30–90 s"),
    ("P6", "dd_verify", "重装验收", "5–10 分"),
    ("P7", "harden", "系统基线", "20–40 s"),
    ("P8", "firewall", "防火墙", "10–20 s"),
    ("P9", "ss", "Shadowsocks", "10–20 s"),
    ("P10", "register", "登记", "< 3 s"),
    ("P11", "agent", "nodeagent", "20–60 s"),
    ("P12", "outbound_check", "出站校验", "2–5 s"),
    ("P13", "relay_attach", "线路机接入", "每批 30–60 s"),
    ("P14", "probe_attach", "探针与监测", "10–20 s"),
    ("P15", "e2e_verify", "端到端验收", "20–40 s"),
    ("P16", "activate", "上线交付", "< 5 s"),
];

/// 阶段分组（UI 的「阶段」列模式）。
pub const PHASES: [(&str, &str, &[&str]); 7] = [
    ("preflight", "预检", &["P1"]),
    ("keys", "密钥", &["P2", "P3"]),
    ("reinstall", "重装", &["P4", "P5", "P6"]),
    ("system", "系统与 SS", &["P7", "P8", "P9"]),
    ("register", "登记与 agent", &["P10", "P11", "P12"]),
    ("relay", "线路机", &["P13", "P14"]),
    ("deliver", "验收交付", &["P15", "P16"]),
];

pub fn step_index(code: &str) -> usize {
    STEPS.iter().position(|s| s.0 == code).unwrap_or(0)
}
pub fn step_name(code: &str) -> &'static str {
    STEPS.iter().find(|s| s.0 == code).map(|s| s.2).unwrap_or("?")
}
pub fn next_step(code: &str) -> Option<&'static str> {
    STEPS.get(step_index(code) + 1).map(|s| s.0)
}

/// 步骤格状态（UI 词表：queued running succeeded failed skipped waiting_input blocked）。
pub mod cell {
    pub const QUEUED: &str = "queued";
    pub const RUNNING: &str = "running";
    pub const SUCCEEDED: &str = "succeeded";
    pub const FAILED: &str = "failed";
    pub const SKIPPED: &str = "skipped";
    pub const WAITING_INPUT: &str = "waiting_input";
    pub const BLOCKED: &str = "blocked";
    pub const COMPENSATED: &str = "compensated";
}

/// 行状态（UI 的 row_state）。
pub mod row {
    pub const QUEUED: &str = "queued";
    pub const RUNNING: &str = "running";
    pub const WAITING_GATE: &str = "waiting_gate";
    pub const READY_FOR_RELAY: &str = "ready_for_relay";
    pub const IN_RELAY_BATCH: &str = "in_relay_batch";
    pub const WAITING_INPUT: &str = "waiting_input";
    pub const BLOCKED: &str = "blocked";
    pub const FAILED: &str = "failed";
    pub const ACTIVE: &str = "active";
    pub const CANCELLED: &str = "cancelled";

    /// 还会被调度器推进的状态。
    pub fn runnable(s: &str) -> bool {
        matches!(s, QUEUED | RUNNING | WAITING_GATE)
    }
    /// 停下来等人（或终态）。
    pub fn stopped(s: &str) -> bool {
        matches!(s, WAITING_INPUT | BLOCKED | FAILED | ACTIVE | CANCELLED)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrClass {
    Transient,
    Input,
    Identity,
    Provider,
    Conflict,
    Relay,
    Bug,
}

impl ErrClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrClass::Transient => "transient",
            ErrClass::Input => "input",
            ErrClass::Identity => "identity",
            ErrClass::Provider => "provider",
            ErrClass::Conflict => "conflict",
            ErrClass::Relay => "relay",
            ErrClass::Bug => "bug",
        }
    }
}

/// 一步的结果。
#[derive(Debug)]
pub enum Outcome {
    /// 目标状态已达成（含「本来就是」）；outputs 不含机密。
    Done(serde_json::Value),
    Skipped(String),
    /// 需要人：密码被拒、DD 人工闸门……
    WaitInput(String),
    /// 风险停止：身份不符、冲突。不自动重试。
    Blocked(ErrClass, String),
    Fail(ErrClass, String),
}

pub fn err<T>(class: ErrClass, msg: impl Into<String>) -> Result<T, StepErr> {
    Err(StepErr { class, msg: msg.into() })
}

#[derive(Debug, Clone)]
pub struct StepErr {
    pub class: ErrClass,
    pub msg: String,
}

impl std::fmt::Display for StepErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.class.as_str(), self.msg)
    }
}

impl From<StepErr> for Outcome {
    fn from(e: StepErr) -> Self {
        match e.class {
            ErrClass::Identity | ErrClass::Conflict => Outcome::Blocked(e.class, e.msg),
            ErrClass::Input => Outcome::WaitInput(e.msg),
            c => Outcome::Fail(c, e.msg),
        }
    }
}
