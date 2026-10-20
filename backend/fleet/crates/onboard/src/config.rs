//! onboard.toml：执行环境（中控）上的配置。示例见 onboard.toml.example。

use serde::Deserialize;
use std::path::PathBuf;

fn d_concurrency() -> usize { 5 }
fn d_true() -> bool { true }
fn d_window() -> u64 { 30 }
fn d_batch_max() -> usize { 20 }
fn d_retry_max() -> u32 { 3 }
fn d_retry_base() -> u64 { 5000 }
fn d_samples() -> u32 { 5 }
fn d_interval() -> u64 { 3000 }
fn d_lease() -> u64 { 20 }
fn d_egress() -> String { "https://www.cloudflare.com/cdn-cgi/trace".into() }
fn d_ssh() -> String { "ssh".into() }
fn d_keyscan() -> String { "ssh-keyscan".into() }
fn d_dd_wait() -> u64 { 1800 }
fn d_canary_gate() -> String { "P16".into() }
fn d_method() -> String { "aes-256-gcm".into() }
fn d_ss_port() -> u16 { 8388 }
fn d_port_lo() -> u16 { 20001 }
fn d_port_hi() -> u16 { 20999 }
fn d_user() -> String { "root".into() }
fn d_22() -> u16 { 22 }
fn d_xray() -> String { "auto".into() }
fn d_asset() -> String { "auto".into() }
fn d_cfg() -> String { "/etc/fleet/xray/config.json".into() }
fn d_api() -> String { "127.0.0.1:62789".into() }
fn d_sni() -> String { "www.cloudflare.com".into() }
fn d_max_nodes() -> usize { 25 }

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub db: PathBuf,
    pub catalog: PathBuf,
    /// known_hosts、ControlMaster 套接字、askpass、解包后的 ssserver 都放这里（0700）。
    pub work_dir: PathBuf,
    /// 上架专用私钥（推到节点 root 的 authorized_keys）。
    pub ssh_key: PathBuf,
    pub ssh_pubkey: PathBuf,
    #[serde(default = "d_ssh")]
    pub ssh_bin: String,
    #[serde(default = "d_keyscan")]
    pub keyscan_bin: String,
    #[serde(default = "d_concurrency")]
    pub concurrency: usize,
    #[serde(default = "d_true")]
    pub canary: bool,
    /// 金丝雀要走到哪一步才放行其余行：P12（节点就绪）或 P16（全链路含线路机接入）。
    #[serde(default = "d_canary_gate")]
    pub canary_gate: String,
    #[serde(default = "d_window")]
    pub relay_window_s: u64,
    #[serde(default = "d_batch_max")]
    pub relay_batch_max: usize,
    #[serde(default = "d_retry_max")]
    pub retry_max: u32,
    #[serde(default = "d_retry_base")]
    pub retry_base_ms: u64,
    #[serde(default = "d_samples")]
    pub keyscan_samples: u32,
    #[serde(default = "d_interval")]
    pub keyscan_interval_ms: u64,
    #[serde(default = "d_lease")]
    pub lease_ttl_s: u64,
    /// 出口核对 URL：返回 cloudflare trace 格式（含 `ip=`）。
    #[serde(default = "d_egress")]
    pub egress_check_url: String,
    /// 中控本机做 P15 端到端验收用的 xray 客户端。
    pub client_xray: PathBuf,
    /// BBR 必须生效。只有容器演练才设 false（/proc/sys 只读）。
    #[serde(default = "d_true")]
    pub require_bbr: bool,
    #[serde(default = "d_dd_wait")]
    pub dd_wait_timeout_s: u64,
    pub ss: SsCfg,
    /// P11 装 nodeagent（fleet-edge）。没有这一节 = P11 跳过（不装）。
    #[serde(default)]
    pub agent: Option<AgentCfg>,
    #[serde(default)]
    pub providers: Vec<ProviderCfg>,
    pub relays: Vec<RelayCfg>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SsCfg {
    /// shadowsocks-rust 官方 musl 包（tar.xz），旁边要有 .sha256。
    pub tarball: PathBuf,
    #[serde(default = "d_method")]
    pub method: String,
    #[serde(default = "d_ss_port")]
    pub port: u16,
    #[serde(default = "d_port_lo")]
    pub relay_port_lo: u16,
    #[serde(default = "d_port_hi")]
    pub relay_port_hi: u16,
}

fn d_endpoint() -> String { "https://38.58.57.22".into() }
fn d_sample() -> u64 { 5 }
fn d_batch_samples() -> u32 { 2 }
fn d_first_batch() -> u64 { 90 }

/// `[agent]`：P11 按 fleet-edge/deploy/rollout.sh 的方式装 nodeagent。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCfg {
    /// fleet-edge 发布目录（fleet-edge/dist/<版本>/ 的拷贝）：nodeagent、VERSION、SHA256SUMS。
    /// nodeagent 的 sha256 必须与 SHA256SUMS 里的一致，否则批次不开始。
    pub dist: PathBuf,
    /// 上报地址（rollout.sh 缺省 https://38.58.57.22；nodeagent 会 POST {endpoint}/v2/ingest/node）。
    #[serde(default = "d_endpoint")]
    pub endpoint: String,
    #[serde(default = "d_sample")]
    pub sample_interval_s: u64,
    #[serde(default = "d_batch_samples")]
    pub batch_samples: u32,
    /// 等第一批被中控接收的上限（秒）。
    #[serde(default = "d_first_batch")]
    pub first_batch_timeout_s: u64,
    /// systemd 单元；缺省用内置的一份（与 fleet-edge/deploy/fleet-nodeagent.service 逐字节相同，单元测试核对）。
    #[serde(default)]
    pub unit_file: Option<PathBuf>,
    /// 只在 fleetctl 独立运行时用（fleetd 里由进程内的 fleetd-ingest 签发）：签发 / 撤销令牌的命令，`{host_id}` 会替换成
    /// 单引号转义后的 host_id。以 fleetd 用户执行 fleetctl 时：`/opt/fleetd/current/fleetd agent-token issue {host_id} --name onboard`
    /// （以 root 执行时前面加 `runuser -u fleetd --`：fleetd agent-token 拒绝以 root 写别人的库）
    #[serde(default)]
    pub token_cmd: Option<String>,
    #[serde(default)]
    pub revoke_cmd: Option<String>,
}

impl AgentCfg {
    pub fn validate(&self) -> Result<(), String> {
        let ep = self.endpoint.trim_end_matches('/');
        // 与 nodeagent 自己的校验一致：生产必须 https；http 只允许回环（测试）
        if let Some(rest) = ep.strip_prefix("http://") {
            let host = rest.split('/').next().unwrap_or("");
            let h = host.rsplit_once(':').map(|x| x.0).unwrap_or(host);
            if !(h == "127.0.0.1" || h == "localhost" || h == "[::1]") {
                return Err("[agent].endpoint 必须是 https://（http:// 只允许 127.0.0.1 测试）".into());
            }
        } else if !ep.starts_with("https://") {
            return Err("[agent].endpoint 必须以 https:// 开头".into());
        }
        if !(2..=300).contains(&self.sample_interval_s) {
            return Err("[agent].sample_interval_s 取值 2–300".into());
        }
        if !(1..=60).contains(&self.batch_samples) {
            return Err("[agent].batch_samples 取值 1–60".into());
        }
        if !(10..=900).contains(&self.first_batch_timeout_s) {
            return Err("[agent].first_batch_timeout_s 取值 10–900".into());
        }
        for (k, v) in [("token_cmd", &self.token_cmd), ("revoke_cmd", &self.revoke_cmd)] {
            if v.as_deref().is_some_and(|c| !c.contains("{host_id}")) {
                return Err(format!("[agent].{k} 里要有 {{host_id}} 占位符"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderCfg {
    pub name: String,
    pub code: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RelayCfg {
    /// 与 CSV 的 relay 列一致，例如「线路机3」。
    pub name: String,
    /// SSH 管理地址。
    pub host: String,
    #[serde(default = "d_22")]
    pub ssh_port: u16,
    #[serde(default = "d_user")]
    pub ssh_user: String,
    pub ssh_key: PathBuf,
    /// 线路机主机公钥（已钉住，必填）。
    pub known_hosts: PathBuf,
    /// 客户端连接用的公网地址（分享链接里的 host）。
    pub public_host: String,
    /// 节点 nft 放行 8388 的来源（所属线路机 + 备用线路机的出口 IP）。
    pub allow_ips: Vec<String>,
    /// "auto"（默认）= 取正在运行的 Xray 进程的可执行文件（fleet-cutover 装在 /opt/fleet/xray/<sha16>/xray），
    /// 保证 -test / api 用的就是在跑的那个版本。
    #[serde(default = "d_xray")]
    pub xray_bin: String,
    /// "auto" = xray_bin 所在目录（geoip.dat / geosite.dat 与二进制放在一起）。
    #[serde(default = "d_asset")]
    pub asset_dir: String,
    #[serde(default = "d_cfg")]
    pub config: String,
    #[serde(default = "d_api")]
    pub api: String,
    #[serde(default = "d_sni")]
    pub sni: String,
    #[serde(default = "d_max_nodes")]
    pub max_nodes: usize,
}

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("读配置 {} 失败：{e}", path.display()))?;
        let c: Config = toml::from_str(&text).map_err(|e| format!("配置 {} 格式错误：{e}", path.display()))?;
        if c.relays.is_empty() {
            return Err("配置里至少要有一台 [[relays]]".into());
        }
        if !matches!(c.canary_gate.as_str(), "P12" | "P16") {
            return Err("canary_gate 只能是 P12 或 P16".into());
        }
        if let Some(a) = &c.agent {
            a.validate()?;
        }
        Ok(c)
    }
    pub fn relay(&self, name: &str) -> Option<&RelayCfg> {
        self.relays.iter().find(|r| r.name == name)
    }
    pub fn provider_code(&self, name: &str) -> String {
        if let Some(p) = self.providers.iter().find(|p| p.name.eq_ignore_ascii_case(name)) {
            return p.code.clone();
        }
        match name.to_ascii_lowercase().as_str() {
            "attvps" => "ATT".into(),
            "nodemach" => "NM".into(),
            "ohnoicu" => "MX".into(),
            _ => name.to_ascii_uppercase().chars().filter(|c| c.is_ascii_alphanumeric()).take(4).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn example_parses_with_agent() {
        let c: super::Config = toml::from_str(include_str!("../onboard.toml.example")).expect("onboard.toml.example 解析失败");
        let a = c.agent.expect("示例里应有 [agent]");
        a.validate().unwrap();
        assert_eq!(a.endpoint, "https://38.58.57.22");
        assert!(a.token_cmd.is_none());
    }
}
