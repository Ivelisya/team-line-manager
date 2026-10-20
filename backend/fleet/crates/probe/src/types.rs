//! 探测结果与推送契约（`fleet.probe.v1`）。本模块不依赖运行时。
//!
//! 一条 [`ProbeResult`] 是一次尝试的原始证据，不是判定：综合状态只在中控（fleet-health）算。
use serde::{Deserialize, Serialize};

/// 推送批次的 schema 名（写在 [`ProbeBatch::schema`]）
pub const SCHEMA: &str = "fleet.probe.v1";

/// 控制路径（线路机不经 SS 直接访问公共来源）结果的 `node` 字段
pub const CONTROL_NODE: &str = "_relay";

/// 检查种类
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    /// L1：线路机 → 落地 `ip:ss_port` 的 TCP connect
    Tcp,
    /// L2：线路机 → SS → 落地 → 按域名访问公共来源，核对出口 IP
    Egress,
    /// 控制路径：线路机直接访问同一公共来源（判 `probe_target_down` 的佐证）
    Control,
    /// UDP：经 SS UDP relay 向公共 STUN 发 Binding，核对映射地址
    Udp,
    /// IPv6：经 SS 访问只有 AAAA 的回显端点
    Ipv6,
    /// 全链路（relayd 原生，取代旧 team-ops-path-probe）：本机 xray 客户端 → 127.0.0.1:<入口端口>（VLESS+REALITY，
    /// 入口第一个用户）→ 路由 → 落地 → `https://www.cloudflare.com/cdn-cgi/trace`；细节在 [`ProbeResult::path`]
    Path,
}

impl CheckKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckKind::Tcp => "tcp",
            CheckKind::Egress => "egress",
            CheckKind::Control => "control",
            CheckKind::Udp => "udp",
            CheckKind::Ipv6 => "ipv6",
            CheckKind::Path => "path",
        }
    }
    pub fn parse(s: &str) -> Option<CheckKind> {
        Some(match s {
            "tcp" => CheckKind::Tcp,
            "egress" => CheckKind::Egress,
            "control" => CheckKind::Control,
            "udp" => CheckKind::Udp,
            "ipv6" => CheckKind::Ipv6,
            "path" => CheckKind::Path,
            _ => return None,
        })
    }
}

/// 单次尝试的结果
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Fail,
    Timeout,
    /// 无法下结论（例如控制路径也失败 → `probe_target_down`；UDP 无应答）
    Unknown,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Fail => "fail",
            Status::Timeout => "timeout",
            Status::Unknown => "unknown",
        }
    }
    pub fn is_ok(self) -> bool {
        self == Status::Ok
    }
}

/// 原因码（字符串常量，与旧 `monitor.py` 的 REASONS 口径一致，外加 v1.2 新增项）
pub mod reason {
    /// 超时（旧 curl rc 28）
    pub const TIMEOUT: &str = "timeout";
    /// 隧道建立后握手被断开：SS 密码/加密方式不对，或节点连不上目标（旧 curl rc 35/52/55/56）
    pub const AUTH: &str = "auth";
    /// 连不上落地 SS 端口（旧 curl rc 7/97；TCP 检查失败）
    pub const UPSTREAM_DOWN: &str = "upstream_down";
    /// 出口 IP 与期望不符
    pub const EGRESS_MISMATCH: &str = "egress_mismatch";
    /// 控制路径也失败：公共来源本身故障，本轮不计入可用率
    pub const PROBE_TARGET_DOWN: &str = "probe_target_down";
    /// 证书校验失败（经隧道看到的证书不可信，疑似劫持）
    pub const TLS_INVALID: &str = "tls_invalid";
    /// HTTP 状态码非 200
    pub const HTTP_ERROR: &str = "http_error";
    /// 响应里解析不出 IP
    pub const BAD_RESPONSE: &str = "bad_response";
    /// 不支持的加密方式（探测器没法说话，不代表节点坏）
    pub const UNSUPPORTED_CIPHER: &str = "unsupported_cipher";
    /// UDP：STUN 映射地址 ≠ 期望出口
    pub const UDP_EGRESS_MISMATCH: &str = "udp_egress_mismatch";
    /// UDP：多个来源给出不同的映射地址
    pub const UDP_EGRESS_UNSTABLE: &str = "udp_egress_unstable";
    /// UDP：所有 STUN 来源都没应答（节点可能没开 UDP）
    pub const UDP_NO_REPLY: &str = "udp_no_reply";
    /// IPv6：经 SS 能连通只有 AAAA 的端点
    pub const IPV6_EGRESS_PRESENT: &str = "ipv6_egress_present";
    /// IPv6：连不通（正常情况）
    pub const IPV6_ABSENT: &str = "ipv6_absent";
}

/// 旧系统兼容：把原因码映射回 curl 退出码（`monitor.py` 用 REASONS[rc] 还原原因）。
/// 0 = 成功；-1 = 无对应（旧系统显示 unknown / egress_ip_mismatch 也是 -1）。
pub fn legacy_rc(status: Status, reason_code: Option<&str>) -> i32 {
    if status.is_ok() {
        return 0;
    }
    match reason_code {
        Some(reason::TIMEOUT) => 28,
        Some(reason::AUTH) => 35,
        Some(reason::UPSTREAM_DOWN) => 7,
        _ => -1,
    }
}

/// 分段耗时（毫秒）：connect = 到落地（或来源）的 TCP；tls = TLS 握手完成；first_byte = 收到首个明文字节
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Phases {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_byte_ms: Option<f64>,
}

/// 一次探测尝试的原始证据（边缘 SQLite 与推送批次里都是这个形状）
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ProbeResult {
    /// 边缘 `probe_raw.id`（入库后才有；中控按 `(node, check, measured_at, attempt)` 幂等合并，不依赖它）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    /// 节点 uid（迁移期 = 旧系统资产 uuid）；控制路径为 `_relay`
    pub node: String,
    pub check: CheckKind,
    pub status: Status,
    /// 成功时的总耗时（毫秒）；失败、超时一律为 null，绝不当作延迟
    pub value_ms: Option<f64>,
    /// 公共来源（或 STUN）看到的出口地址
    pub actual_ip: Option<String>,
    /// 失败原因（成功为 null；IPv6/UDP 的结论性原因即使 ok 也会填）
    pub reason_code: Option<String>,
    /// 尝试结束时刻（unix 秒）
    pub measured_at: f64,
    /// L2：本周期第几次尝试（1..=3）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u8>,
    /// L2：是否是本周期的最后一次（HL-04：只有最后一次算证据）
    #[serde(rename = "final", default, skip_serializing_if = "Option::is_none")]
    pub final_attempt: Option<bool>,
    /// 命中的来源名（`cloudflare` / `ipify` / `stun.cloudflare` …）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phases: Option<Phases>,
    /// 旧系统兼容的 curl 退出码（见 [`legacy_rc`]）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_rc: Option<i32>,
    /// 错误摘要（≤ 200 字符，不含凭据）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// check = path 时：与旧 path-status.json 行同口径的字段
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathDetail>,
    /// 实际探测的地址 "ip:port"（线路机互探的 tcp 行、线路机自检 egress 行填写）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// 全链路检查的一次尝试（旧 relay_e2e.live_paths 的 attempts 元素）
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PathAttempt {
    /// curl 兼容的退出码（0 成功，7 连不上本机客户端，97 SOCKS 握手失败，35 TLS，28 超时，22 HTTP 错误，56 读失败）
    pub rc: i32,
    pub ms: Option<f64>,
    /// 失败阶段：null / connect / tls / http / read / deadline（与旧 curl write-out 推断一致）
    pub phase: Option<String>,
    pub actual_ip: Option<String>,
}

/// 全链路检查的结果细节（旧 path-status.json 一行：healthy、actual_ip、expected_ip、rc、phase、egress_ms、attempts）
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PathDetail {
    /// 入口端口（旧行按资产 uuid，资产 ↔ 端口由中控映射）
    pub entry_port: u16,
    /// 入站 tag
    pub entry_tag: String,
    pub healthy: bool,
    pub rc: i32,
    pub phase: Option<String>,
    /// 最后一次尝试的耗时（旧 egress_ms；失败时也有值，与旧系统一致）
    pub ms: Option<f64>,
    pub attempts: Vec<PathAttempt>,
}

impl ProbeResult {
    pub fn new(node: impl Into<String>, check: CheckKind, status: Status, measured_at: f64) -> ProbeResult {
        ProbeResult {
            id: None,
            node: node.into(),
            check,
            status,
            value_ms: None,
            actual_ip: None,
            reason_code: None,
            measured_at,
            attempt: None,
            final_attempt: None,
            source: None,
            expected_ip: None,
            phases: None,
            legacy_rc: None,
            detail: None,
            path: None,
            target: None,
        }
    }

    /// 本条是否算 L2 证据（非 egress 恒为 true）
    pub fn is_evidence(&self) -> bool {
        self.check != CheckKind::Egress || self.final_attempt.unwrap_or(true) || self.status.is_ok()
    }
}

/// 推送批次里的 `probe` 段（挂在 fleet-edge 遥测批次的顶层 `"probe"` 键上，见 WIRING.md）
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ProbeBatch {
    /// 恒为 [`SCHEMA`]
    pub schema: String,
    /// 线路机 host_id（与遥测批次同一个）
    pub relay: String,
    pub generated_at: f64,
    /// 本批覆盖的边缘行号区间（闭区间）；中控确认后边缘前移游标
    pub first_id: i64,
    pub last_id: i64,
    pub results: Vec<ProbeResult>,
    /// 本线路机当前在测的节点（中控据此把“没结果的节点”记 unknown，而不是当成已下线）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<TargetInfo>,
}

/// 批次里对在测节点的描述（不含凭据）
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TargetInfo {
    pub node: String,
    #[serde(default)]
    pub name: String,
    pub tag: String,
    pub addr: String,
    pub port: u16,
    pub method: String,
    pub expected_ip: String,
    #[serde(default)]
    pub udp: bool,
}
