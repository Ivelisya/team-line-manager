//! /etc/fleet-edge/agent.toml
//!
//! ```toml
//! endpoint = "https://38.58.57.22"        # 上报到 {endpoint}/v2/ingest/{node|relay}
//! host_id = "团队1-24"
//! token_file = "/etc/fleet-edge/token"     # 0600，属主为服务用户
//! # state_dir = "/var/lib/fleet-nodeagent" # 缺省取 $STATE_DIRECTORY
//! # sample_interval_s = 5                  # 采样间隔（relayd 的 Xray 统计同频）
//! # batch_samples = 2                      # 每 2 个样本（10 s）一批
//! # outbox_max_bytes = 2097152
//!
//! [xray]                                   # 只有 relayd 读
//! config = "/etc/fleet/xray/config.json"   # 入站/用户/api 地址/探测对象都从这里读（只读）
//! # api = "127.0.0.1:62789"                # 缺省从 config 的 api 入站推出；也可 "unix:/run/xray/api.sock"
//! bin = "xray"                             # argv[0] 文件名
//! proc_match = "/etc/fleet/xray/config.json"
//! seed_file = "/etc/fleet-edge/xray-seed.json"
//!
//! [probe]                                  # relayd 的 L1/L2 探测，缺省开启
//! # identities = "/etc/komari-relay-probe/config.json"
//! ```
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub endpoint: String,
    pub host_id: String,
    pub token_file: PathBuf,
    #[serde(default)]
    pub state_dir: Option<PathBuf>,
    #[serde(default = "d_interval")]
    pub sample_interval_s: u64,
    #[serde(default = "d_batch")]
    pub batch_samples: u32,
    #[serde(default = "d_outbox")]
    pub outbox_max_bytes: u64,
    #[serde(default = "d_timeout")]
    pub http_timeout_s: u64,
    #[serde(default)]
    pub xray: Option<XrayCfg>,
    /// relayd 的 L1/L2 探测（fleet-probe）；缺省开启
    #[serde(default)]
    pub probe: Option<ProbeCfg>,
    /// relayd 管理模式（fleet.relayspec.v1）；缺省开启，fleetd 未提供接口（404）或 generation 0 时什么都不做
    #[serde(default)]
    pub manage: Option<ManageCfg>,
    /// relayd 访问日志上报（fleet.access.v1）；缺省开启
    #[serde(default)]
    pub access: Option<AccessCfg>,
    /// 线路机自身：主节点入口自检（egress）与互探（tcp）
    #[serde(default)]
    pub relay: Option<RelaySelfCfg>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct RelaySelfCfg {
    /// 线路机自己的资产 uuid（结果行的 node）
    pub asset_uuid: String,
    /// 期望出口（本机公网 IP）
    pub public_ip: String,
    /// 主节点入口端口；缺省取路由到 direct/freedom 的那个 vless+reality 入口
    #[serde(default)]
    pub master_port: Option<u16>,
    /// 互探对象："<资产 uuid>@<ip>:<端口>"（其他线路机的主节点入口）
    #[serde(default)]
    pub peers: Vec<String>,
    #[serde(default = "d_peer_iv")]
    pub peer_interval_s: u64,
}

fn d_peer_iv() -> u64 {
    30
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct ManageCfg {
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default = "d_mconfig")]
    pub config: String,
    #[serde(default = "d_lock")]
    pub lock: String,
    /// 缺省用运行中的 xray 进程的 argv[0]
    #[serde(default)]
    pub xray_bin: Option<String>,
    /// XRAY_LOCATION_ASSET；缺省为 xray 可执行文件所在目录（与 fleet-xray.service 一致）
    #[serde(default)]
    pub asset_dir: Option<String>,
    #[serde(default = "d_systemctl")]
    pub systemctl: Vec<String>,
    #[serde(default = "d_unit")]
    pub unit: String,
    #[serde(default = "d_poll")]
    pub poll_s: u64,
}

impl Default for ManageCfg {
    fn default() -> Self {
        ManageCfg { enabled: true, config: d_mconfig(), lock: d_lock(), xray_bin: None, asset_dir: None, systemctl: d_systemctl(), unit: d_unit(), poll_s: d_poll() }
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct AccessCfg {
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// 缺省取 Xray 配置的 log.access
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default = "d_amax")]
    pub max_events: usize,
}

impl Default for AccessCfg {
    fn default() -> Self {
        AccessCfg { enabled: true, path: None, max_events: d_amax() }
    }
}

fn d_mconfig() -> String {
    "/etc/fleet/xray/config.json".into()
}
fn d_lock() -> String {
    "/run/lock/fleet-xray-config.lock".into()
}
fn d_systemctl() -> Vec<String> {
    vec!["systemctl".into()]
}
fn d_unit() -> String {
    "fleet-xray.service".into()
}
fn d_poll() -> u64 {
    10
}
fn d_amax() -> usize {
    400
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct XrayCfg {
    /// Xray 的 api 入站（tunnel，127.0.0.1），明文 gRPC；缺省从 `config` 里的 api 入站推出，再缺省 127.0.0.1:62789
    #[serde(default)]
    pub api: Option<String>,
    /// 用来识别进程（argv[0] 的文件名）；进程换代时用进程自己的 argv[0]（绝对路径）执行 `version`
    #[serde(default = "d_bin")]
    pub bin: String,
    /// 命令行里必须包含的片段，用来排除 komari-relay-probe 那个隔离 xray
    #[serde(default = "d_match")]
    pub proc_match: String,
    /// x-ui 面板进程的 argv[0] 文件名
    #[serde(default = "d_xui")]
    pub xui_proc_name: String,
    /// 额外统计已建立连接数的入站端口；缺省从 `inbound-<端口>` 标签推出
    #[serde(default)]
    pub inbound_ports: Vec<u16>,
    /// 是否执行 `xray version` 取版本（只在 xray 进程换代时执行一次）
    #[serde(default = "d_true")]
    pub version_exec: bool,
    /// 累计流量的种子（切换时从 x-ui.db 导出，见 deploy/xui-seed.py）；内容变化才重新应用
    #[serde(default = "d_seed")]
    pub seed_file: Option<String>,
    /// 运行中的 Xray 配置（只读，每 60 s 按 mtime 重读）：入站 tag→端口、用户、api 地址；也是探测对象的来源
    #[serde(default = "d_xcfg")]
    pub config: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct ProbeCfg {
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// SS 出站来源（Xray JSON，只读）；缺省用 [xray].config
    #[serde(default)]
    pub xray_configs: Vec<String>,
    /// 节点身份（tag → uuid/名字/期望 IP），旧 komari-relay-probe 的 config.json；不存在就只用 tag
    #[serde(default = "d_ids")]
    pub identities: Option<String>,
    /// 每批最多带多少条探测结果（控制请求体 < 256 KB）
    #[serde(default = "d_probe_limit")]
    pub batch_limit: usize,
    #[serde(default = "d_true")]
    pub udp: bool,
    #[serde(default = "d_true")]
    pub ipv6: bool,
    /// 原生全链路检查（check = path，取代旧 team-ops-path-probe）
    #[serde(default = "d_true")]
    pub path: bool,
    #[serde(default = "d_path_iv")]
    pub path_interval_s: u64,
}

fn d_path_iv() -> u64 {
    60
}

impl Default for ProbeCfg {
    fn default() -> Self {
        ProbeCfg { enabled: true, xray_configs: vec![], identities: d_ids(), batch_limit: d_probe_limit(), udp: true, ipv6: true, path: true, path_interval_s: d_path_iv() }
    }
}

fn d_xcfg() -> Option<String> {
    Some("/etc/fleet/xray/config.json".into())
}
fn d_ids() -> Option<String> {
    Some("/etc/komari-relay-probe/config.json".into())
}
fn d_probe_limit() -> usize {
    200
}

fn d_interval() -> u64 {
    5
}
fn d_batch() -> u32 {
    2
}
fn d_outbox() -> u64 {
    2 * 1024 * 1024
}
fn d_timeout() -> u64 {
    15
}
fn d_bin() -> String {
    "xray".into()
}
fn d_match() -> String {
    "/etc/fleet/xray/config.json".into()
}
fn d_xui() -> String {
    "x-ui".into()
}
fn d_seed() -> Option<String> {
    Some("/etc/fleet-edge/xray-seed.json".into())
}
fn d_true() -> bool {
    true
}

impl Config {
    pub fn load(path: &str) -> Result<Config, String> {
        let s = std::fs::read_to_string(path).map_err(|e| format!("读取 {path} 失败：{e}"))?;
        let c: Config = toml::from_str(&s).map_err(|e| format!("解析 {path} 失败：{e}"))?;
        c.validate()?;
        Ok(c)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.host_id.trim().is_empty() || self.host_id.len() > 64 {
            return Err("host_id 为空或过长".into());
        }
        let ep = self.endpoint.trim_end_matches('/');
        if let Some(rest) = ep.strip_prefix("http://") {
            // 明文只允许 loopback（测试服）；生产必须 https 并正常校验证书
            let host = rest.split('/').next().unwrap_or("");
            let h = host.rsplit_once(':').map(|x| x.0).unwrap_or(host);
            if !(h == "127.0.0.1" || h == "localhost" || h == "[::1]") {
                return Err("endpoint 必须是 https://（http:// 只允许 127.0.0.1 测试）".into());
            }
        } else if !ep.starts_with("https://") {
            return Err("endpoint 必须以 https:// 开头".into());
        }
        if !(2..=300).contains(&self.sample_interval_s) {
            return Err("sample_interval_s 取值 2–300".into());
        }
        if !(1..=60).contains(&self.batch_samples) {
            return Err("batch_samples 取值 1–60".into());
        }
        if !(64 * 1024..=64 * 1024 * 1024).contains(&self.outbox_max_bytes) {
            return Err("outbox_max_bytes 取值 64 KiB–64 MiB".into());
        }
        Ok(())
    }

    pub fn url(&self, kind: &str) -> String {
        format!("{}/v2/ingest/{kind}", self.endpoint.trim_end_matches('/'))
    }

    pub fn state_dir(&self) -> PathBuf {
        if let Some(d) = &self.state_dir {
            return d.clone();
        }
        if let Some(d) = std::env::var_os("STATE_DIRECTORY") {
            // 可能是冒号分隔的多个目录，取第一个
            let s = d.to_string_lossy().to_string();
            return PathBuf::from(s.split(':').next().unwrap_or(&s));
        }
        PathBuf::from("/var/lib/fleet-edge")
    }

    pub fn read_token(&self) -> Result<String, String> {
        use std::os::unix::fs::MetadataExt;
        let md = std::fs::metadata(&self.token_file).map_err(|e| format!("读取令牌文件 {} 失败：{e}", self.token_file.display()))?;
        if md.mode() & 0o077 != 0 {
            return Err(format!("令牌文件 {} 权限过宽（应为 0600）", self.token_file.display()));
        }
        let t = std::fs::read_to_string(&self.token_file).map_err(|e| format!("读取令牌失败：{e}"))?;
        let t = t.trim().to_string();
        if t.len() < 20 || !t.bytes().all(|b| b.is_ascii_graphic()) {
            return Err("令牌格式不对".into());
        }
        Ok(t)
    }
}
