//! fleet-agent ⇄ fleet-server 协议（`/agent/v1/*`）。
//!
//! - 鉴权：`Authorization: Bearer <token>`；令牌只绑定 machine_id，不含名字。
//! - 上报：`POST /agent/v1/report`，`Report.seq` 单调递增；服务端按 (machine_id, seq) 去重，回 `ReportAck`。
//! - 下发（仅线路机）：`GET /agent/v1/desired?after=<generation>` 长轮询，返回比 `after` 新的 `DesiredState`，
//!   无变化时 30 s 后回 204；对齐后 `POST /agent/v1/applied` 回报 `ApplyReport`。
//!
//! 时间戳统一为 Unix 毫秒。

use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::sync::LazyLock;
use uuid::Uuid;

/// agent 与中控的版本号：编译时带了环境变量 FLEET_GIT_HASH（lab/deploy-server.sh 传本机的 git 短哈希）就是
/// `0.1.0+<哈希>`，否则只有包版本。包版本一直是 0.1.0，靠哈希才看得出机器上跑的是哪一次提交
pub fn build_version() -> &'static str {
    static V: LazyLock<String> = LazyLock::new(|| match option_env!("FLEET_GIT_HASH") {
        Some(h) => format!("{}+{h}", env!("CARGO_PKG_VERSION")),
        None => env!("CARGO_PKG_VERSION").into(),
    });
    &V
}

// ── 上报 ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub seq: u64,
    pub boot_id: String,
    pub agent_version: String,
    pub sent_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<SystemInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host: Vec<HostSample>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<EntrySample>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub probes: Vec<ProbeSample>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub access: Vec<AccessEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xray: Option<XrayStatus>,
    /// Agent 自身健康（累计值，自进程启动起算）；旧 Agent 不带
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_health: Option<AgentHealth>,
}

/// Agent 自身的健康数据：离线队列、访问日志尾读、最近一次应用失败、写盘失败。服务端存进 agent_state.info。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AgentHealth {
    /// 离线队列里待发送的报告数
    #[serde(default)]
    pub queue_len: u32,
    /// 因超过字节/时间上限被丢弃的报告数
    #[serde(default)]
    pub queue_dropped: u64,
    /// 被移入 dead/ 的报告数：只有服务端永久拒绝（400/413/422 等 4xx）的才进 dead/，5xx 一直重试
    #[serde(default)]
    pub queue_dead: u64,
    /// 访问日志里解析不了的行
    #[serde(default)]
    pub access_unparsed: u64,
    /// 访问日志里被丢弃的行（超长、主机名含非法字符、轮转后旧文件末尾的残行）
    #[serde(default)]
    pub access_dropped_lines: u64,
    /// 轮转后找不到旧文件、可能漏掉一段
    #[serde(default)]
    pub access_rotation_gaps: u64,
    #[serde(default)]
    pub access_truncations: u64,
    /// 最近一次应用失败的错误（成功后清空）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_apply_error: Option<String>,
    /// 这份报告写不进磁盘队列（磁盘满等）、是直接从内存发来的：写盘的错误。写得进时不带；旧 agent 不带，旧中控忽略
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_write_error: Option<String>,
}

/// 开机后上报一次、变化时再报。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SystemInfo {
    pub hostname: String,
    pub kernel: String,
    pub os: String,
    pub cpu_cores: u32,
    pub arch: String,
    #[serde(default)]
    pub ipv4: Vec<IpAddr>,
    #[serde(default)]
    pub ipv6: Vec<IpAddr>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostSample {
    pub ts_ms: i64,
    pub cpu_pct: f32,
    /// 1 分钟负载；容器里拿不到本机负载（/proc/loadavg 是宿主机的）时为 None
    #[serde(default)]
    pub load1: Option<f32>,
    pub mem_used: u64,
    pub mem_total: u64,
    pub disk_used: u64,
    pub disk_total: u64,
    /// agent 进程内的累计计数器：从 0 起，只累加选中网卡（不含 lo 等虚拟网卡）各自的增量；
    /// agent 重启归零，服务端按计数器重置处理
    pub net_rx_bytes: u64,
    pub net_tx_bytes: u64,
    pub tcp_conns: u32,
}

/// 线路机：每个入口在一个采样间隔内的流量增量。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntrySample {
    pub entry_id: Uuid,
    pub ts_ms: i64,
    pub up_bytes: u64,
    pub down_bytes: u64,
    pub conns: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProbeKind {
    /// 线路机 → 节点出站地址的 TCP 握手
    Tcp,
    /// 经节点出站访问回显服务，核对出口 IP
    Egress,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeSample {
    pub entry_id: Uuid,
    pub ts_ms: i64,
    pub kind: ProbeKind,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<f32>,
    /// 失败原因，或 egress 探测看到的出口 IP
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// TCP 探测建连时 SYN 重发了几次（内核 TCP_INFO 的 total_retrans）。重发过的建连，`latency_ms` 仍是内核量的往返时间，
    /// 不含 1 s / 3 s 的重发等待；丢包另记在这里。旧 agent 不报（None），旧中控收到不认识的字段照常忽略
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrans: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessEvent {
    pub entry_id: Uuid,
    pub ts_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src_ip: Option<IpAddr>,
    pub dest_host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest_port: Option<u16>,
    pub network: String,
    #[serde(default)]
    pub blocked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XrayStatus {
    pub running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub applied_generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportAck {
    /// 服务端已持久化的最大 seq；agent 据此丢弃本地缓冲
    pub accepted_seq: u64,
    /// 服务端希望的上报间隔（秒）
    pub interval_s: u32,
}

// ── 下发（线路机）────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DesiredState {
    pub generation: u64,
    pub relay: RelayParams,
    pub entries: Vec<DesiredEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RelayParams {
    pub reality_dest: String,
    pub reality_sni: String,
    pub reality_private_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_client_ver: Option<String>,
    /// 出口回显服务（egress 探测用），返回纯文本 IP
    pub echo_url: String,
    pub probe_interval_s: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DesiredEntry {
    pub id: Uuid,
    pub port: u16,
    pub enabled: bool,
    /// 入站备注（节点显示名），只用于人看：agent 不读它，也不写进 xray.json。
    /// 中控改节点名 / 团队名 / 重排编号时代数仍会 +1，agent 收到后渲染结果不变，按幂等应用处理（不跑 xray -test、不动运行态）
    pub remark: String,
    pub client_uuid: Uuid,
    pub short_id: String,
    pub outbound: Outbound,
    /// egress 探测期望看到的出口 IP
    pub expect_egress_ip: IpAddr,
    /// 访问策略（按域名黑白名单/暂停）。`None` = observe，不产生任何路由规则；
    /// 旧 Agent 忽略这个字段，新 Agent 收不到字段也按 observe 处理。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<EntryPolicy>,
}

/// 入口的访问策略（中控已按「节点策略 > 团队策略 > observe」算出的有效值）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EntryPolicy {
    pub mode: PolicyMode,
    /// Xray 域名匹配串：`domain:example.com`（含子域）或 `full:example.com`（仅精确）。
    /// allowlist / denylist 必有至少一个；blocked 为空。
    #[serde(default)]
    pub domains: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    /// 只放行名单内域名，其余阻止
    Allowlist,
    /// 阻止名单内域名，其余放行
    Denylist,
    /// 暂停该入口的所有新连接
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "protocol", rename_all = "snake_case")]
pub enum Outbound {
    Shadowsocks { host: String, port: u16, method: String, password: String },
    Socks5 { host: String, port: u16, username: Option<String>, password: Option<String> },
    Http { host: String, port: u16, username: Option<String>, password: Option<String> },
    /// 线路机直出：从线路机自己的公网 IP 出去（Xray freedom），没有落地节点。
    /// 旧 agent 不认识这个值，整份期望状态解析失败、回报「agent 版本过旧」：先升级线路机 agent 再建直出入口
    Direct,
}

impl Outbound {
    /// 出站服务器地址 (host, port)；直出没有（不做线路机 → 节点的 TCP 探测）
    pub fn addr(&self) -> Option<(&str, u16)> {
        match self {
            Outbound::Shadowsocks { host, port, .. } | Outbound::Socks5 { host, port, .. } | Outbound::Http { host, port, .. } => Some((host, *port)),
            Outbound::Direct => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyReport {
    pub generation: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub entries: Vec<EntryApply>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryApply {
    pub id: Uuid,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Xray 里用到的标签：只由 entry_id 派生，和名字、端口都无关。
pub mod tags {
    use uuid::Uuid;
    pub fn inbound(id: Uuid) -> String { format!("in-{}", id.simple()) }
    pub fn outbound(id: Uuid) -> String { format!("out-{}", id.simple()) }
    pub fn rule(id: Uuid) -> String { format!("rule-{}", id.simple()) }
    /// 客户端在 Xray 统计里的 email
    pub fn email(id: Uuid) -> String { format!("e-{}", id.simple()) }
    pub fn parse_entry_id(tag: &str) -> Option<Uuid> {
        let s = tag.split_once('-')?.1;
        Uuid::try_parse(s).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_roundtrip() {
        let id = Uuid::now_v7();
        for t in [tags::inbound(id), tags::outbound(id), tags::rule(id), tags::email(id)] {
            assert_eq!(tags::parse_entry_id(&t), Some(id));
        }
        assert_eq!(tags::parse_entry_id("api"), None);
    }

    #[test]
    fn entry_policy_is_backward_compatible() {
        // 旧 JSON（没有 policy 字段）能解析成 None，且 None 不序列化出字段
        let id = Uuid::nil();
        let old = serde_json::json!({
            "id": id, "port": 20001, "enabled": true, "remark": "r", "client_uuid": id, "short_id": "ab",
            "outbound": {"protocol": "socks5", "host": "h", "port": 1, "username": null, "password": null},
            "expect_egress_ip": "203.0.113.9"
        });
        let e: DesiredEntry = serde_json::from_value(old).unwrap();
        assert!(e.policy.is_none());
        assert!(serde_json::to_value(&e).unwrap().get("policy").is_none());
        let mut e2 = e.clone();
        e2.policy = Some(EntryPolicy { mode: PolicyMode::Allowlist, domains: vec!["domain:a.com".into(), "full:b.com".into()] });
        let v = serde_json::to_value(&e2).unwrap();
        assert_eq!(v["policy"]["mode"], "allowlist");
        assert_eq!(serde_json::from_value::<DesiredEntry>(v).unwrap(), e2);
    }

    #[test]
    fn agent_health_disk_error_is_optional() {
        // 旧 agent 不带 disk_write_error：照样解析；没有错误时也不序列化出这个字段（旧中控存进库的形状不变）
        let h: AgentHealth = serde_json::from_value(serde_json::json!({"queue_len": 1, "queue_dropped": 0, "queue_dead": 0})).unwrap();
        assert_eq!(h.disk_write_error, None);
        assert!(serde_json::to_value(&h).unwrap().get("disk_write_error").is_none());
        let h = AgentHealth { disk_write_error: Some("No space left on device (os error 28)".into()), ..h };
        assert_eq!(serde_json::from_value::<AgentHealth>(serde_json::to_value(&h).unwrap()).unwrap(), h);
    }

    #[test]
    fn outbound_is_tagged_by_protocol() {
        let o = Outbound::Socks5 { host: "h".into(), port: 1, username: None, password: None };
        let v = serde_json::to_value(&o).unwrap();
        assert_eq!(v["protocol"], "socks5");
    }

    #[test]
    fn direct_outbound_serde() {
        // 直出只有协议名，没有地址
        let v = serde_json::to_value(&Outbound::Direct).unwrap();
        assert_eq!(v, serde_json::json!({"protocol": "direct"}));
        assert_eq!(serde_json::from_value::<Outbound>(v).unwrap(), Outbound::Direct);
        assert_eq!(Outbound::Direct.addr(), None);
        let ss = Outbound::Shadowsocks { host: "10.0.0.9".into(), port: 8388, method: "m".into(), password: "p".into() };
        assert_eq!(ss.addr(), Some(("10.0.0.9", 8388)));
        // 整条入口往返
        let id = Uuid::nil();
        let e = DesiredEntry {
            id,
            port: 20001,
            enabled: true,
            remark: "线路机2-直出".into(),
            client_uuid: id,
            short_id: "ab".into(),
            outbound: Outbound::Direct,
            expect_egress_ip: "198.51.100.2".parse().unwrap(),
            policy: None,
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["outbound"], serde_json::json!({"protocol": "direct"}));
        assert_eq!(serde_json::from_value::<DesiredEntry>(v).unwrap(), e);
    }
}
