use anyhow::Context;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "d_listen")]
    pub listen: String,
    pub database_url: String,
    /// 32 字节随机数（二进制文件），用于加密库里的秘密字段
    pub secret_key_file: PathBuf,
    /// agent 访问中控用的地址（写进节点和线路机的 agent 配置）
    pub public_url: String,
    /// 前端构建产物目录；不存在则只提供 API
    #[serde(default)]
    pub web_dir: Option<PathBuf>,
    /// 中控登录机器用的私钥（上架、退役）
    pub ssh_key: PathBuf,
    /// 中控自己维护的 known_hosts（上架预检时记录指纹，之后严格校验）
    pub known_hosts: PathBuf,
    /// 推送到机器上的二进制：fleet-agent、ssserver、xray
    pub dist_dir: PathBuf,
    /// 位于反向代理之后时信任 X-Forwarded-For
    #[serde(default)]
    pub trust_forwarded_for: bool,
    /// 会话 Cookie 是否带 Secure；不填则按 public_url 是否 https 判断
    #[serde(default)]
    pub cookie_secure: Option<bool>,
    /// 组织时区（IANA 名）：账单时区、供应商时区没填时按它，通知、审计、fleetctl 里的时间也按它写。默认北京时间
    #[serde(default = "d_org_tz")]
    pub org_timezone: String,
    #[serde(default)]
    pub retention: Retention,
    #[serde(default)]
    pub numbering: Numbering,
    #[serde(default = "d_workers")]
    pub job_workers: usize,
    /// 访问监控 24h / 7d / 30d 聚合结果的缓存秒数（这些窗口读小时汇总表，每轮汇总后缓存整体作废，所以可以和汇总间隔一样长；
    /// 单台机器的结果最多 30 秒）；0 = 不缓存
    #[serde(default = "d_access_cache")]
    pub access_cache_secs: u64,
    /// 来源 IP 画像（离线库，只查本地）；整段不写也能跑
    #[serde(default)]
    pub ipintel: IpintelConfig,
    #[serde(default)]
    pub notify: NotifyConfig,
    /// 中控自监控（死人开关、备份检查，见 selfmon.rs）；整段不写也能跑
    #[serde(default)]
    pub selfmon: SelfmonConfig,
    /// 软路由的反向隧道（见 routers/mod.rs）；整段不写也能跑
    #[serde(default)]
    pub routers: RoutersConfig,
}

/// `[routers]`：路由经反向 SSH 隧道连中控 sshd（账号 fleet-tunnel），中控经隧道登录路由采集
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RoutersConfig {
    /// 路由连中控 sshd 用的地址：不填取 public_url 的主机名
    pub ssh_host: Option<String>,
    /// 中控 sshd 的端口
    pub ssh_port: u16,
    /// 写给 sshd 的隧道公钥清单（每行「端口 ssh-ed25519 <base64>」），由 /usr/local/lib/fleet/tunnel-keys 读取；
    /// 那个脚本只认默认路径，改了要同步改脚本
    pub tunnel_keys_file: PathBuf,
    /// 中控 sshd 的 ed25519 主机公钥：写进接入脚本，路由按它核对中控（HostKeyAlias=fleet-central）
    pub sshd_host_key: PathBuf,
    /// 经隧道登录路由时用的 known_hosts（每行「router-<id> 公钥」，由库里的 router.host_key 整份重写）
    pub known_hosts: PathBuf,
    /// OpenSSH ControlMaster 的套接字目录（每台路由一个，复用连接，30 秒一次的采集不每次握手）
    pub control_dir: PathBuf,
    /// 隧道端口段，必须在 23000–23999 之内（库里的检查约束与 tunnel-keys 脚本都按这一段）
    pub port_min: u16,
    pub port_max: u16,
}
impl Default for RoutersConfig {
    fn default() -> Self {
        Self {
            ssh_host: None,
            ssh_port: 22,
            tunnel_keys_file: PathBuf::from("/var/lib/fleet/tunnel-keys"),
            sshd_host_key: PathBuf::from("/etc/ssh/ssh_host_ed25519_key.pub"),
            known_hosts: PathBuf::from("/var/lib/fleet/router_known_hosts"),
            control_dir: PathBuf::from("/var/lib/fleet/ssh-ctl"),
            port_min: 23000,
            port_max: 23999,
        }
    }
}

impl RoutersConfig {
    /// 路由连中控用的主机名：配置的 ssh_host，否则 public_url 的主机名。只许主机名 / IP 的字符（它会写进路由上的启动命令）
    pub fn ssh_host_of(&self, public_url: &str) -> anyhow::Result<String> {
        let h = match &self.ssh_host {
            Some(h) => h.trim().to_string(),
            None => reqwest::Url::parse(public_url)
                .ok()
                .and_then(|u| u.host_str().map(|h| h.trim_start_matches('[').trim_end_matches(']').to_string()))
                .context("public_url 里取不到主机名，[routers] ssh_host 要写上")?,
        };
        if h.is_empty() || h.len() > 253 || !h.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':')) {
            anyhow::bail!("[routers] ssh_host 不对：{h}（只能是主机名或 IP）");
        }
        Ok(h)
    }

    fn check(&self, public_url: &str) -> anyhow::Result<()> {
        if !(23000..=23999).contains(&self.port_min) || !(23000..=23999).contains(&self.port_max) || self.port_min > self.port_max {
            anyhow::bail!("[routers] port_min / port_max 要在 23000–23999 之内且 port_min ≤ port_max");
        }
        self.ssh_host_of(public_url).map(|_| ())
    }
}

/// `[notify]`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    /// 允许通知渠道指向内网 / 环回 / 保留地址。默认不允许：渠道地址由有 write 权限的人填，不能借它探中控所在的内网；
    /// 只有测试或自建在内网的 webhook 才打开
    pub allow_private: bool,
}

/// `[selfmon]`
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SelfmonConfig {
    /// 死人开关：告警评估每轮成功后，最多每分钟 GET 一次这个地址（healthchecks.io 之类的 ping URL）。
    /// 中控挂了、卡住、告警循环一直失败时那边收不到 ping，由它报警；不填不发
    pub heartbeat_url: Option<String>,
    /// 每日备份所在的目录（deploy/backup/fleet-backup.sh 的 D）：最新一份 fleet-*.tar（.tar.age）超过 26 小时开告警；
    /// 目录不存在（没装备份，如实验室）不检查
    pub backup_dir: PathBuf,
}
impl Default for SelfmonConfig {
    fn default() -> Self { Self { heartbeat_url: None, backup_dir: PathBuf::from("/var/backups/fleet") } }
}

/// `[ipintel]`：全部有默认值，老配置文件不用改
#[derive(Debug, Clone, Deserialize)]
pub struct IpintelConfig {
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// 离线库目录（deploy/ipintel/fetch-dbs.sh 的 DEST）；缺失或不全时降级，不影响服务
    #[serde(default = "d_ipintel_dir")]
    pub db_dir: PathBuf,
    /// 可选的 ASN 分类覆盖文件（见 crates/fleet-ipintel/data/asn-lists.toml 的写法）；文件不存在就只用内置默认
    #[serde(default = "d_ipintel_overrides")]
    pub overrides: Option<PathBuf>,
    /// 额外视为内部地址的 IP（运维出口等）。线路机和节点的 IP 自动算内部，不用写
    #[serde(default)]
    pub internal_ips: Vec<String>,
    /// 学习期天数：从节点第一条访问记录算起，期间只学习不告警
    #[serde(default = "d_learn_days")]
    pub learn_days: u32,
    /// 一次性异常（新运营商 / 新城市 / 机房来源）最后一次出现后多少小时自动恢复
    #[serde(default = "d_alert_ttl_hours")]
    pub alert_ttl_hours: u32,
}
impl Default for IpintelConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            db_dir: d_ipintel_dir(),
            overrides: d_ipintel_overrides(),
            internal_ips: vec![],
            learn_days: d_learn_days(),
            alert_ttl_hours: d_alert_ttl_hours(),
        }
    }
}
fn d_true() -> bool { true }
fn d_ipintel_dir() -> PathBuf { PathBuf::from("/var/lib/fleet/ipintel") }
fn d_ipintel_overrides() -> Option<PathBuf> { Some(PathBuf::from("/etc/fleet/ipintel-overrides.toml")) }
fn d_learn_days() -> u32 { 7 }
fn d_alert_ttl_hours() -> u32 { 24 }

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Retention {
    /// 原始时序（主机指标、入口流量、探测）保留天数。读原始表的地方最多看 24 小时（7d / 30d 走小时汇总表），
    /// agent 离线队列 6 小时就淘汰，3 天足够补算迟到的小时汇总。上报接收窗口、分区维护、彻底删除的时间窗都由它推出
    pub raw_days: u32,
    /// 访问记录（原始）保留天数的默认值：访问记录的保留期在面板里设（settings.rs），没设过时用它
    pub access_days: u32,
}
impl Default for Retention {
    fn default() -> Self { Self { raw_days: 3, access_days: 7 } }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Numbering {
    /// 节点退役后自动把该团队编号压缩成 01..N（编号连续，数量一眼可见）
    pub auto_compact: bool,
}
impl Default for Numbering {
    fn default() -> Self { Self { auto_compact: true } }
}

fn d_org_tz() -> String { crate::meta::DEFAULT_ORG_TZ.into() }
fn d_listen() -> String { "127.0.0.1:8080".into() }
fn d_workers() -> usize { 8 }
fn d_access_cache() -> u64 { 300 }

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path).with_context(|| format!("读取配置 {}", path.display()))?;
        let c: Config = toml::from_str(&s).with_context(|| format!("解析配置 {}", path.display()))?;
        c.routers.check(&c.public_url)?;
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_tables_keep_defaults() {
        let r: Retention = toml::from_str("raw_days = 30").unwrap();
        assert_eq!((r.raw_days, r.access_days), (30, 7));
        let n: Numbering = toml::from_str("").unwrap();
        assert!(n.auto_compact);
        let s: SelfmonConfig = toml::from_str("heartbeat_url = \"https://hc-ping.com/x\"").unwrap();
        assert_eq!((s.heartbeat_url.as_deref(), s.backup_dir), (Some("https://hc-ping.com/x"), PathBuf::from("/var/backups/fleet")));
        let r: RoutersConfig = toml::from_str("ssh_port = 2222").unwrap();
        assert_eq!((r.ssh_port, r.port_min, r.port_max), (2222, 23000, 23999));
        assert_eq!(r.tunnel_keys_file, PathBuf::from("/var/lib/fleet/tunnel-keys"));
    }

    #[test]
    fn router_ssh_host_defaults_to_public_url() {
        let r = RoutersConfig::default();
        assert_eq!(r.ssh_host_of("https://fleet.kismet613.com").unwrap(), "fleet.kismet613.com");
        assert_eq!(r.ssh_host_of("http://192.168.31.60:8080/").unwrap(), "192.168.31.60");
        assert_eq!(r.ssh_host_of("http://[2001:db8::1]:8080").unwrap(), "2001:db8::1");
        let bad = RoutersConfig { ssh_host: Some("a b; rm -rf /".into()), ..RoutersConfig::default() };
        assert!(bad.ssh_host_of("https://x.test").is_err());
        assert!(RoutersConfig { port_min: 22000, ..RoutersConfig::default() }.check("https://x.test").is_err());
        assert!(RoutersConfig { port_min: 23500, port_max: 23400, ..RoutersConfig::default() }.check("https://x.test").is_err());
    }
}
