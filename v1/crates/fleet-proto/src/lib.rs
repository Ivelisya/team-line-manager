//! fleet-agent ⇄ fleet-server 协议（`/agent/v1/*`）。
//!
//! - 鉴权：`Authorization: Bearer <token>`；令牌只绑定 machine_id，不含名字。
//! - 上报：`POST /agent/v1/report`，`Report.seq` 单调递增；服务端按 (machine_id, seq) 去重，回 `ReportAck`。
//! - 下发（仅线路机）：`GET /agent/v1/desired?after=<generation>` 长轮询，返回比 `after` 新的 `DesiredState`，
//!   无变化时 30 s 后回 204；对齐后 `POST /agent/v1/applied` 回报 `ApplyReport`。
//! - 放置矩阵（仅线路机）：`GET /agent/v1/rtt` 回 `RttConfig`，agent 每轮测之前取一次。
//! - 探测点（`fleet-agent probe`）：`/agent/v1/probe/*`，令牌是探测点自己的，见下面「探测点」一节。
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// 线路机上 AnyTLS 外壳（sing-box）的状态；旧 agent 不报，旧中控忽略
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub singbox: Option<SingboxStatus>,
    /// Agent 自身健康（累计值，自进程启动起算）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_health: Option<AgentHealth>,
    /// 三网 PING（线路机往外测，`DesiredState::ping` 给的目标）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ping: Vec<PingSample>,
    /// 线路机 → 各 VPS 节点的网络往返（`GET /agent/v1/rtt` 给的节点；放置矩阵，见 fleet-server 的 placement.rs）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rtt: Vec<RttSample>,
    /// 这个 agent 会的东西（[`CAP_REVERSE`] 等）。中控据此判断能不能下发它不认识就会整份解析失败的期望状态：
    /// agent 版本号只是包版本 + 提交哈希，比不出先后。旧 agent 不带（空）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caps: Vec<String>,
    /// 线路机：穿透入口里隧道没挂上的（Xray 里没有 `rvs-<入口>` 出站，连续 [`TUNNEL_DOWN_AFTER_S`] 秒以上）。
    /// 中控据此让节点重启 bridge（`ReportAck::bridge_kick`），见 fleet-agent 的 xray/render.rs「穿透」
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tunnels_down: Vec<Uuid>,
}

/// agent 会穿透入口：线路机渲染 `Outbound::Reverse`、上报 `tunnels_down`；节点按 `ReportAck::bridge_kick` 重启 bridge
pub const CAP_REVERSE: &str = "reverse";
/// agent 认 `RelayParams::reality_public`（线路机只留 AnyTLS 时把 Reality 入站改听回环）。旧 agent 忽略这个字段、Reality 照样对公网开着：
/// 中控只在线路机 agent 报了它时才允许把线路机的入口协议改成只 AnyTLS
pub const CAP_REALITY_PRIVATE: &str = "reality_private";
/// agent 在只留 AnyTLS 的线路机上让穿透入口的 Reality 入站照旧听公网、并用 nft 表 `inet fleet_reality_guard` 只放行这条入口的节点出站 IP
/// （`DesiredEntry::expect_egress_ip`，bridge 从它连过来）。旧 agent 把穿透入口也改听回环、bridge 连不上：中控只在线路机 agent 报了它时
/// 才允许有穿透入口的线路机改成只 AnyTLS、或在只 AnyTLS 的线路机上开穿透
pub const CAP_REALITY_GUARD: &str = "reality_guard";
/// 本 agent 的全部能力（随每份上报带上）
pub const CAPS: &[&str] = &[CAP_REVERSE, CAP_REALITY_PRIVATE, CAP_REALITY_GUARD];
/// 穿透入口的隧道在线路机 Xray 里缺席这么久才报 `tunnels_down`：bridge 断线重连约 2 秒，正常切换、重启都到不了这么久
pub const TUNNEL_DOWN_AFTER_S: u64 = 30;

/// 一个节点一轮：`ok` = 这一轮 ICMP 有回应；`rtt_ms` 是这一轮的往返时间（几次回应的中位数），不通为 None。
/// 线路机上系统 ping 用不了时不出样本
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RttSample {
    pub node_id: Uuid,
    pub ts_ms: i64,
    pub ok: bool,
    #[serde(default)]
    pub rtt_ms: Option<f32>,
}

/// 一个目标一轮（发 `sent` 次，丢 `lost` 次，`rtt_ms` 是通了的那几次的中位数，全丢为 None）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PingSample {
    pub slot: u8,
    pub ts_ms: i64,
    /// 实际测的目标（同 `PingTarget::target`）
    pub target: String,
    /// 实际用的方式：tcp / icmp（要 ICMP 但系统 ping 用不了时退回 tcp）
    pub mode: String,
    pub sent: u16,
    pub lost: u16,
    #[serde(default)]
    pub rtt_ms: Option<f32>,
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
    /// 这份报告写不进磁盘队列（磁盘满等）、是直接从内存发来的：写盘的错误。写得进时不带
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
    /// 流量计量用的网卡（默认路由所在网卡 ∪ 有公网地址或同机 IP 节点出站地址的物理网卡，或 agent.toml 的 nic_ifaces）
    #[serde(default)]
    pub nic_ifaces: Vec<String>,
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
    /// 选中网卡（`SystemInfo::nic_ifaces`）各自增量的持久累计值：自 agent 首次安装起单调，跨 agent 重启、开机接续
    /// （data_dir/nic.json）；状态文件丢失时从 0 重来。服务端：变小 = 重置、读数即增量
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
    /// 不含 1 s / 3 s 的重发等待；丢包另记在这里
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

/// 线路机上 sing-box（只做 AnyTLS 外壳，见 fleet-agent 的 singbox.rs）的状态。中控存进 agent_state.info.singbox；
/// 入口的 AnyTLS 链接、订阅里的 AnyTLS 条目只在 `running` 时才给（没在跑时链接是坏的）
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SingboxStatus {
    /// 线路机上有 sing-box 程序（没有 = 未安装，agent 跳过 AnyTLS，Xray 那一侧照常）
    pub installed: bool,
    /// fleet-singbox 单元在跑
    pub running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// 最近一次应用 sing-box 配置的错误（成功后清空）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Agent 的上报间隔（秒）。固定值，回执里不带（旧回执里的 `interval_s` 字段解析时忽略）
pub const REPORT_INTERVAL_S: u32 = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportAck {
    /// 服务端已持久化的最大 seq；agent 据此丢弃本地缓冲
    pub accepted_seq: u64,
    /// 节点：要它重启穿透的 bridge（fleet-xray-bridge）。值是中控决定重启的时刻（Unix 毫秒），同一个值只重启一次
    /// （agent 记在 data_dir）；没有要重启的为 None。旧 agent 不认识，忽略
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge_kick: Option<i64>,
}

// ── 下发（线路机）────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DesiredState {
    pub generation: u64,
    pub relay: RelayParams,
    pub entries: Vec<DesiredEntry>,
    /// 三网 PING 的目标（中控按全局 + 这台的覆盖合成）；没有目标为 None
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ping: Option<PingConfig>,
}

/// 放置矩阵（`GET /agent/v1/rtt`）：这台要测往返的全部在用 VPS 节点（不只是挂在它上面的），每 `interval_s` 秒一轮。
/// 不进期望状态：节点清单一变就要给线路机代数 +1，会让「重启 Xray 失败、暂停重试」中的线路机带着新代数再重启一次
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RttConfig {
    pub interval_s: u32,
    pub targets: Vec<RttTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RttTarget {
    pub node_id: Uuid,
    /// 节点的公网 IP（只发 ICMP，不连端口）
    pub host: String,
}

/// 三网 PING：每 `interval_s` 秒一轮，每个目标发 `count` 次。`mode` 是字符串而不是枚举：以后加新方式时旧 agent 不至于整份期望状态解析失败
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PingConfig {
    pub interval_s: u32,
    pub count: u32,
    /// tcp（建连，默认）/ icmp（系统 ping；用不了时退回 tcp）
    pub mode: String,
    pub targets: Vec<PingTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PingTarget {
    /// 槽位 0–7
    pub slot: u8,
    pub name: String,
    /// `host`、`host:port`、`[IPv6]:port` 或裸 IPv6；TCP 不写端口时连 80
    pub target: String,
}

/// TCP 方式不写端口时连的端口
pub const PING_TCP_DEFAULT_PORT: u16 = 80;

/// 线路机上 Xray API 的监听端口（127.0.0.1）。agent 渲染 xray.json、中控给内核保留本地端口（jobs/tune.rs）都用它：
/// 一边改了另一边没改，内核会把 Xray 正在监听的端口当本地端口发出去，Xray 重启时绑不上
pub const XRAY_API_PORT: u16 = 62789;
/// 每个启用入口的回环探测入站端口 = 入口端口 + 此偏移（agent 的 `probe_ports`；中控 tune 同样要保留这段端口）
pub const PROBE_PORT_OFFSET: u16 = 10000;
/// 入口的 AnyTLS 公网端口（sing-box 监听）= 入口端口 + 此偏移；默认端口池 20000–29999 落在 40000–49999
pub const ANYTLS_PORT_OFFSET: u16 = 20000;
/// 入口的 AnyTLS 回环桥接端口（Xray 的 VLESS 入站 `at-<入口>`，sing-box 连它）= 入口端口 + 此偏移；默认落在 50000–59999
pub const ANYTLS_BRIDGE_OFFSET: u16 = 30000;

/// 哪些入口有 AnyTLS：`entries` 是启用入口的 (id, 端口)，返回 入口 → AnyTLS 公网端口。
/// 公网端口、桥接端口都要不超过 65535，且在「全部入口端口、探测端口、Xray API 端口、全部 AnyTLS 公网 / 桥接端口」里只被它自己占用；
/// 撞了的两边都没有（结果只取决于入口集合，和顺序无关）。中控下发 `DesiredEntry::anytls_port`、算分享链接 / 订阅，
/// agent 渲染时都用它：两边同一条规则，链接里的端口就是线路机上真在监听的
pub fn anytls_ports(entries: &[(Uuid, u16)]) -> std::collections::HashMap<Uuid, u16> {
    let pair = |p: u16| Some((p.checked_add(ANYTLS_PORT_OFFSET)?, p.checked_add(ANYTLS_BRIDGE_OFFSET)?));
    let mut claims: std::collections::HashMap<u16, u32> = std::collections::HashMap::new();
    let mut claim = |p: u16| *claims.entry(p).or_default() += 1;
    claim(XRAY_API_PORT);
    for &(_, p) in entries {
        claim(p);
        if let Some(x) = p.checked_add(PROBE_PORT_OFFSET) {
            claim(x);
        }
        if let Some((a, b)) = pair(p) {
            claim(a);
            claim(b);
        }
    }
    entries
        .iter()
        .filter_map(|&(id, p)| {
            let (a, b) = pair(p)?;
            (claims[&a] == 1 && claims[&b] == 1).then_some((id, a))
        })
        .collect()
}

/// 拆 `target`：(主机, 端口)。`[v6]:port`、`[v6]`、裸 IPv6、`host:port`、`host`；端口不对为 None
pub fn split_target(t: &str) -> Option<(String, Option<u16>)> {
    let t = t.trim();
    if let Some(rest) = t.strip_prefix('[') {
        let (h, tail) = rest.split_once(']')?;
        return match tail.strip_prefix(':') {
            Some(p) => Some((h.to_string(), Some(p.parse().ok().filter(|p: &u16| *p > 0)?))),
            None if tail.is_empty() => Some((h.to_string(), None)),
            None => None,
        };
    }
    if t.matches(':').count() > 1 {
        return Some((t.to_string(), None));
    }
    match t.split_once(':') {
        Some((h, p)) => Some((h.to_string(), Some(p.parse().ok().filter(|p: &u16| *p > 0)?))),
        None => Some((t.to_string(), None)),
    }
}

/// 拆「必须带端口」的 `host:port`（探测的对照目标）：规则同 `split_target`，没写端口为 None。中控与探测点 agent 都用它，
/// 不要各自 `rsplit_once(':')`：`[::1]:443` 会被拆成 `[::1]` 和 443，主机名带着方括号解析不出来
pub fn host_port(t: &str) -> Option<(String, u16)> {
    match split_target(t)? {
        (h, Some(p)) => Some((h, p)),
        _ => None,
    }
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
    /// 入口的 VLESS REALITY 入站对公网开着（默认）。false = 线路机只留 AnyTLS：Reality 入站改听 127.0.0.1，别的都不变
    /// （tag、统计、策略规则、AnyTLS 桥接与探测入站照旧）；穿透入口例外，照旧听公网、只放行它的节点（见 [`CAP_REALITY_GUARD`]）。
    /// 为 true 时不序列化：旧 agent 收到的期望状态和以前一样
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub reality_public: bool,
}

fn yes() -> bool { true }
fn is_true(b: &bool) -> bool { *b }

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
    /// 访问策略（按域名黑白名单/暂停）。`None` = observe，不产生任何路由规则。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<EntryPolicy>,
    /// AnyTLS 公网端口（中控按 [`anytls_ports`] 算好下发；None = 这个入口没有 AnyTLS）。旧 agent 忽略，只有 Reality
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anytls_port: Option<u16>,
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
    /// 穿透：节点上的 bridge 用 `bridge_uuid` 主动连这个入口自己的 REALITY 入站（VLESS 反向代理），客户流量经这条隧道
    /// 交给节点、从节点自己的 IP 出去。线路机不向节点发起连接（供应商丢入站 SYN 的节点用）。
    /// 旧 agent 不认识，整份期望状态解析失败：中控只在线路机 agent 报了 [`CAP_REVERSE`] 时才下发
    Reverse { bridge_uuid: Uuid },
}

impl Outbound {
    /// 出站服务器地址 (host, port)；直出、穿透没有（不做线路机 → 节点的 TCP 探测：穿透时线路机根本不连节点）
    pub fn addr(&self) -> Option<(&str, u16)> {
        match self {
            Outbound::Shadowsocks { host, port, .. } | Outbound::Socks5 { host, port, .. } | Outbound::Http { host, port, .. } => Some((host, *port)),
            Outbound::Direct | Outbound::Reverse { .. } => None,
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

// ── 探测点（`fleet-agent probe`）─────────────────────────────────────────────
//
// 探测点是放在靠近客户的地方（如香港）的一台小机器，替中控测线路机的客户入口通不通（见 fleet-server 的 probe_points/）。
// 令牌是探测点自己的（probe_point.token_hash），与机器的 agent 令牌是两套：
// - `GET /agent/v1/probe/targets` → `ProbeTargets`：这一轮要连的线路机（客户连的地址 + 在用入口端口）与对照目标；
// - `POST /agent/v1/probe/report` ← `ProbeReport`：这一轮的结果；
// - `GET /agent/v1/probe/binary`：dist 里的 fleet-agent（安装命令下载、`ProbeTargets::upgrade` 时自己换）。

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProbeTargets {
    /// 每轮间隔（秒）
    pub interval_s: u32,
    /// 对照目标（`host:port`）：一个都连不上 = 探测点自己的网络有问题，这一轮作废
    pub controls: Vec<String>,
    pub relays: Vec<ProbeTarget>,
    /// 中控要它换成 dist 里的程序（`fleetctl probe upgrade`）：下载 `/agent/v1/probe/binary`，sha256 对上才换
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<ProbeUpgrade>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProbeTarget {
    pub relay_id: Uuid,
    /// 客户连的地址（relay.public_host）
    pub host: String,
    /// 在用入口端口（升序）：每轮连第 `turn % len` 个，N 个口 N 轮连一遍
    pub ports: Vec<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProbeUpgrade {
    pub sha256: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProbeReport {
    pub version: String,
    /// 正在跑的程序的 sha256（十六进制）：与 `ProbeTargets::upgrade` 的一致时中控清掉升级请求
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// 整轮作废的原因（对照目标都连不上）：这时 `results` 为空，只算心跳
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discarded: Option<String>,
    /// 对照目标是通的、三台以上线路机却全部连不上（[`all_down`]）：照常带结果上报并置 true，中控照记、告警摘要写明
    /// 「全部线路机同时连不上」（可能是供应商整段故障）
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all_down: bool,
    /// 换上后没能上报、自己换回了旧版本的程序 sha256（探测点的 BAD 标记）：中控要它升级到的正是这个时，它不会再换，
    /// 中控马上记成升级失败、不再下发
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bad_sha256: Option<String>,
    pub results: Vec<ProbeResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProbeResult {
    pub relay_id: Uuid,
    pub port: u16,
    pub ok: bool,
    /// 建连的往返时间（内核 TCP_INFO 量的，不含 SYN 重发等待；量不到为 None）
    #[serde(default)]
    pub rtt_ms: Option<f32>,
    /// 失败原因（中文）
    #[serde(default)]
    pub detail: Option<String>,
}

/// 三台以上线路机全部连不上（中控 reach.rs 与探测点同一条规则）。一轮记不记只看对照目标：一个都连不上是测的这一方自己的网络有问题，整轮作废。
/// 三台以上全不通以前也当成测的这一方的问题整轮作废；但对照目标是通的时候，更可能是线路机这边一起出了问题
/// （生产几台线路机在同一家供应商、同一个 /24，供应商整段故障时就是这样），这正是最该报的时候：照记，带上这个标记，
/// relay_unreachable 的摘要写明「全部线路机同时连不上」。两台同时不通不算（线路机少时太容易凑上）
pub fn all_down(oks: &[bool]) -> bool { oks.len() >= 3 && !oks.iter().any(|&o| o) }

/// Xray 里用到的标签：只由 entry_id 派生，和名字、端口都无关。
pub mod tags {
    use uuid::Uuid;
    pub fn inbound(id: Uuid) -> String { format!("in-{}", id.simple()) }
    pub fn outbound(id: Uuid) -> String { format!("out-{}", id.simple()) }
    pub fn rule(id: Uuid) -> String { format!("rule-{}", id.simple()) }
    /// 客户端在 Xray 统计里的 email
    pub fn email(id: Uuid) -> String { format!("e-{}", id.simple()) }
    /// AnyTLS 的回环桥接入站（和穿透入口的 bridge 无关）（Xray 里，sing-box 连它）：email 与主入站相同，流量、在线统计合并到入口
    pub fn anytls(id: Uuid) -> String { format!("at-{}", id.simple()) }
    /// 穿透入口：bridge 那个客户端的 email（不以 e- 开头：流量统计只取 `user>>>e-`，隧道流量不重复计）
    pub fn bridge_email(id: Uuid) -> String { format!("r-{}", id.simple()) }
    /// 穿透入口：bridge 连上后 Xray 自己建的反向出站（路由规则指向它；它不在配置里，agent 不增不删）
    pub fn reverse(id: Uuid) -> String { format!("rvs-{}", id.simple()) }
    /// 反向出站的前缀（`reverse`）：核对运行态时这些出站由 Xray 管，不算多出来的
    pub const REVERSE_PREFIX: &str = "rvs-";
    pub fn parse_entry_id(tag: &str) -> Option<Uuid> {
        let s = tag.split_once('-')?.1;
        Uuid::try_parse(s).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_port_needs_a_port_and_handles_ipv6() {
        let hp = |t| host_port(t);
        assert_eq!(hp("example.com:443"), Some(("example.com".into(), 443)));
        assert_eq!(hp("1.1.1.1:53"), Some(("1.1.1.1".into(), 53)));
        assert_eq!(hp("[::1]:443"), Some(("::1".into(), 443)));
        assert_eq!(hp("[2606:4700::1111]:853"), Some(("2606:4700::1111".into(), 853)));
        // 没写端口、端口不对、裸 IPv6（分不出端口）都不算
        for bad in ["example.com", "[::1]", "::1", "example.com:0", "example.com:70000", "[::1]:x", ""] {
            assert_eq!(hp(bad), None, "{bad}");
        }
    }

    #[test]
    fn probe_round_keep_rule() {
        assert!(!all_down(&[false, false]), "两台全不通：不算全部");
        assert!(all_down(&[false, false, false]), "三台以上全不通：全部线路机同时连不上");
        assert!(!all_down(&[false, true, false]) && !all_down(&[]));
    }

    #[test]
    fn anytls_ports_skip_overflow_and_conflicts() {
        let id = Uuid::from_u128;
        // 默认端口池：公网 = 入口 + 20000
        let p = anytls_ports(&[(id(1), 20001), (id(2), 20002)]);
        assert_eq!((p[&id(1)], p[&id(2)]), (40001, 40002));
        // 桥接端口溢出（40000 + 30000 > 65535）：没有
        assert!(anytls_ports(&[(id(1), 40000)]).is_empty());
        // 入口 2 的端口 = 入口 1 的公网端口：入口 1 没有；入口 2 自己的公网 / 桥接（60001 / 70001 溢出）也没有
        let p = anytls_ports(&[(id(1), 20001), (id(2), 40001)]);
        assert!(p.is_empty(), "{p:?}");
        // 入口 1 的桥接端口 = 入口 2 的公网端口（20001 + 30000 = 30001 + 20000）：两边都没有；不相干的入口 3 照常有
        let p = anytls_ports(&[(id(1), 20001), (id(2), 30001), (id(3), 21000)]);
        assert!(!p.contains_key(&id(1)) && !p.contains_key(&id(2)), "{p:?}");
        assert_eq!(p.get(&id(3)), Some(&41000));
    }

    #[test]
    fn outbound_is_tagged_by_protocol() {
        let o = Outbound::Socks5 { host: "h".into(), port: 1, username: None, password: None };
        let v = serde_json::to_value(&o).unwrap();
        assert_eq!(v["protocol"], "socks5");
        // 穿透：协议名 reverse，带 bridge 的 UUID；没有线路机 → 节点的地址
        let o = Outbound::Reverse { bridge_uuid: Uuid::from_u128(7) };
        let v = serde_json::to_value(&o).unwrap();
        assert_eq!(v, serde_json::json!({"protocol": "reverse", "bridge_uuid": Uuid::from_u128(7)}));
        assert_eq!(serde_json::from_value::<Outbound>(v).unwrap(), o);
        assert!(o.addr().is_none());
        assert_eq!(tags::reverse(Uuid::from_u128(7)), format!("rvs-{}", Uuid::from_u128(7).simple()));
        assert!(tags::reverse(Uuid::nil()).starts_with(tags::REVERSE_PREFIX) && !tags::bridge_email(Uuid::nil()).starts_with("e-"));
    }

    #[test]
    fn new_report_fields_are_optional_both_ways() {
        // 旧 agent 的上报没有 caps / tunnels_down；旧中控的回执没有 bridge_kick
        let r: Report = serde_json::from_str(r#"{"seq":1,"boot_id":"b","agent_version":"v","sent_at_ms":0}"#).unwrap();
        assert!(r.caps.is_empty() && r.tunnels_down.is_empty());
        let a: ReportAck = serde_json::from_str(r#"{"accepted_seq":3}"#).unwrap();
        assert_eq!(a.bridge_kick, None);
        // 旧中控的回执还带 interval_s：忽略
        let a: ReportAck = serde_json::from_str(r#"{"accepted_seq":3,"interval_s":10}"#).unwrap();
        assert_eq!(a.accepted_seq, 3);
        let s = serde_json::to_string(&ReportAck { accepted_seq: 1, bridge_kick: None }).unwrap();
        assert!(!s.contains("bridge_kick"), "{s}");
    }
}
