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

fn d_listen() -> String { "127.0.0.1:8080".into() }
fn d_workers() -> usize { 8 }
fn d_access_cache() -> u64 { 300 }

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path).with_context(|| format!("读取配置 {}", path.display()))?;
        let c: Config = toml::from_str(&s).with_context(|| format!("解析配置 {}", path.display()))?;
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
    }
}
