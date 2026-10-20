//! /etc/fleetd/fleetd.toml。机密（旧后端 key、Komari API key、渠道 token）一律放在单独文件里，配置只写路径。
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "d_listen")]
    pub listen: String,
    #[serde(default = "d_data_dir")]
    pub data_dir: PathBuf,
    /// 套餐目录 catalog.json；启动时导入（幂等）。
    #[serde(default)]
    pub catalog: Option<PathBuf>,
    /// 通知里的链接前缀，例如 https://38.58.57.22/#/
    #[serde(default = "d_ui_base")]
    pub ui_base_url: String,
    #[serde(default)]
    pub legacy: LegacyCfg,
    #[serde(default)]
    pub komari: KomariCfg,
    #[serde(default)]
    pub auth: AuthCfg,
    #[serde(default)]
    pub poll: PollCfg,
    #[serde(default)]
    pub alerts: AlertCfg,
    #[serde(default)]
    pub notify: NotifyCfg,
    /// IP 情报（fleet/crates/ipintel）；整段不写也能跑
    #[serde(default)]
    pub ipintel: ipintel::Config,
    /// ipintel 启动回填：每个节点最近多少天的访问（按天取样，每天一页 500 条）
    #[serde(default = "d_backfill_days")]
    pub ipintel_backfill_days: u32,
    /// 边缘代理上报（fleet-edge：nodeagent / relayd → /v2/ingest/*）
    #[serde(default)]
    pub edge: EdgeCfg,
    /// 上架流水线（fleet/crates/onboard）配置文件；不写则 /v2/api/v1/onboard/* 返回“未启用”
    #[serde(default)]
    pub onboard_config: Option<PathBuf>,
    /// 旧接口数据来源："team-ops"（旧系统在运行，只读适配）| "native"（旧系统已下线，原生旧库 legacy.db 渲染）
    #[serde(default = "d_legacy_source")]
    pub legacy_source: String,
}

fn d_legacy_source() -> String {
    "team-ops".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeCfg {
    /// relayd 的 host_id → 旧系统线路机名
    #[serde(default = "d_relay_hosts")]
    pub relay_hosts: std::collections::BTreeMap<String, String>,
    /// 节点综合状态的来源：auto = 某台线路机的原生探测覆盖完整且持续 native_after_s 后改用原生判定；off = 一直用旧 /health
    #[serde(default = "d_native_mode")]
    pub native_health: String,
    #[serde(default = "d_600")]
    pub native_after_s: u64,
    /// 原生探测结果多久内算新鲜（tcp/egress）
    #[serde(default = "d_180")]
    pub native_fresh_s: u64,
    /// fleetd 启动后多久内不开 native_host_offline（agent 退避最长 90 s）
    #[serde(default = "d_180")]
    pub startup_hold_s: u64,
    /// relayd 报告 Xray API 连续不可达多久算 Xray 故障
    #[serde(default = "d_60")]
    pub xray_api_down_s: u64,
    /// 原生旧库的 relays.paths（实际入口到出口验证）来源：false = relayd 转发的旧 path-status.json；
    /// true = relayd 自己的 path 探测（fleet.probe.v1 check=path，最近 5 分钟有结果的节点），没有时仍退回旧文件
    #[serde(default)]
    pub native_paths: bool,
    /// 原生旧库的 relays.probe（线路到上游 TCP + 认证出口验证）来源：false = relayd 转发的旧 komari-relay-probe status.json；
    /// true = relayd 自己的 tcp/egress 探测（fleet.probe.v1）。退役 komari-relay-probe 之前必须先打开
    #[serde(default)]
    pub native_probe: bool,
}

impl Default for EdgeCfg {
    fn default() -> Self { toml::from_str("").unwrap() }
}

fn d_relay_hosts() -> std::collections::BTreeMap<String, String> {
    [("line1", "线路机1"), ("central", "线路机2"), ("line3", "线路机3")].iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
}
fn d_native_mode() -> String { "auto".into() }
fn d_600() -> u64 { 600 }
fn d_180() -> u64 { 180 }

/// 三台线路机的公网 IP：它们自己的探测连接（机房 ASN）不能算“机房来源”。
pub const RELAY_PUBLIC_IPS: &[&str] = &["38.58.57.22", "23.148.204.129", "38.58.57.69"];
fn d_backfill_days() -> u32 { 14 }

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyCfg {
    /// 旧后端地址（只在 legacy_source = "team-ops" 时连接；native 下不用，可不写）。缺省 http://127.0.0.1:25800；旧测试服 http://127.0.0.1:18397/api/team-ops
    #[serde(default = "d_legacy_base")]
    pub base_url: String,
    /// X-Ops-Key 所在文件（纯文本或 {"key": "..."}）。为空表示不发（测试服）。
    #[serde(default)]
    pub key_file: Option<PathBuf>,
    #[serde(default = "d_actor")]
    pub actor: String,
    #[serde(default = "d_two")]
    pub concurrency: usize,
    #[serde(default = "d_timeout")]
    pub timeout_s: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KomariCfg {
    /// false：fleetd 完全不再连 Komari（不做会话回退、不探活、komari/* 全部原生）——Komari 可以停掉
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default = "d_komari_base")]
    pub base_url: String,
    /// Komari「系统 API 密钥」文件（可选）。有它才能在没人打开页面时读 Komari；只调用白名单里的只读方法。
    #[serde(default)]
    pub api_key_file: Option<PathBuf>,
    #[serde(default = "d_timeout")]
    pub timeout_s: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMode {
    /// 只认 fleetd 自有账户（Cookie fleet_session）。Komari 下线后的生产值
    Fleetd,
    /// fleetd 自有账户；`[komari] enabled = true` 且 `auth.komari_fallback = true` 时另接受 Komari 管理员会话（下线前的过渡值）
    Komari,
    /// 只给测试服：本机直连（不经 Caddy）的请求直接放行
    DevAllowLocal,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthCfg {
    #[serde(default = "d_auth_mode")]
    pub mode: AuthMode,
    #[serde(default = "d_cookie")]
    pub cookie_name: String,
    #[serde(default = "d_30")]
    pub cache_ttl_s: u64,
    /// fleetd 自己的会话 Cookie 带 Secure（生产经 Caddy 是 https；测试服 http 可关）
    #[serde(default = "d_true")]
    pub cookie_secure: bool,
    /// Komari 下线前同时接受 Komari 管理员会话
    #[serde(default = "d_true")]
    pub komari_fallback: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollCfg {
    /// 有 SSE 客户端时
    #[serde(default = "d_10")]
    pub active_overview_s: u64,
    #[serde(default = "d_15")]
    pub active_panel_s: u64,
    #[serde(default = "d_60")]
    pub active_history_s: u64,
    #[serde(default = "d_300")]
    pub active_slow_s: u64,
    /// 没有客户端时，overview + health 的周期（够告警用）
    #[serde(default = "d_60")]
    pub idle_overview_s: u64,
    /// 按需代理的最短缓存
    #[serde(default = "d_15")]
    pub ondemand_ttl_s: u64,
    /// 有人看时：边缘批次到达后多久刷新主机 / 线路机快照并推送 overview 变化（合并这段时间里的批次）
    #[serde(default = "d_2f")]
    pub push_debounce_s: f64,
    /// 有人看时两次推送刷新的最短间隔（控制 CPU：每次推送 = 主机快照 + overview / panel 渲染）
    #[serde(default = "d_10f")]
    pub push_min_gap_s: f64,
}

fn d_10f() -> f64 {
    10.0
}

fn d_2f() -> f64 {
    2.0
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertCfg {
    #[serde(default = "d_10")]
    pub group_wait_critical_s: u64,
    #[serde(default = "d_30")]
    pub group_wait_s: u64,
    #[serde(default = "d_300")]
    pub group_interval_s: u64,
    #[serde(default = "d_1800")]
    pub repeat_critical_s: u64,
    #[serde(default = "d_14400")]
    pub repeat_warning_s: u64,
    /// 启动宽限：fleetd 启动后至少这么久不开「还没数据」类告警（主机离线、线路机宕机 / 管理通道中断、采集器 / 健康评估 / 主机数据同步停滞），
    /// 已有的这类告警保持原状（不因启动空窗恢复）。之后还要等数据确实是启动后的（收到第一批边缘上报、喂数与健康评估各跑过一轮），
    /// 最长到 startup_grace_max_s
    #[serde(default = "d_120")]
    pub startup_grace_s: u64,
    #[serde(default = "d_600")]
    pub startup_grace_max_s: u64,
    #[serde(default = "d_9")]
    pub digest_hour: u32,
    #[serde(default = "d_tz")]
    pub digest_tz: String,
    #[serde(default = "d_300")]
    pub degraded_for_s: u64,
    #[serde(default = "d_120")]
    pub host_offline_s: u64,
    #[serde(default = "d_60")]
    pub relay_for_s: u64,
    #[serde(default = "d_120")]
    pub correlate_window_s: u64,
    #[serde(default = "d_3")]
    pub correlate_min_nodes: usize,
    #[serde(default = "d_renewal")]
    pub renewal_days: Vec<i64>,
    #[serde(default = "d_80")]
    pub quota_start_pct: u32,
    #[serde(default = "d_5")]
    pub quota_step_pct: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotifyCfg {
    /// true：即使配了渠道也只写 notify_log（status=shadow）。没有任何渠道时总是影子模式。
    #[serde(default = "d_true")]
    pub shadow: bool,
    /// healthchecks.io ping 地址（可选，默认关）
    #[serde(default)]
    pub healthchecks_url: Option<String>,
    #[serde(default = "d_60")]
    pub healthchecks_interval_s: u64,
    #[serde(default)]
    pub channels: Vec<ChannelCfg>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelCfg {
    pub name: String,
    /// telegram | wecom | smtp | webhook
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default = "d_min_sev")]
    pub min_severity: String,
    // telegram
    #[serde(default)]
    pub bot_token_file: Option<PathBuf>,
    #[serde(default)]
    pub chat_id: Option<String>,
    // wecom / webhook
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub url_file: Option<PathBuf>,
    #[serde(default)]
    pub secret_file: Option<PathBuf>,
    // smtp
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    /// "starttls" | "tls" | "none"
    #[serde(default)]
    pub security: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password_file: Option<PathBuf>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Vec<String>,
}

fn d_listen() -> String { "127.0.0.1:25900".into() }
fn d_data_dir() -> PathBuf { PathBuf::from("/var/lib/fleetd") }
fn d_ui_base() -> String { "https://38.58.57.22/#/".into() }
fn d_legacy_base() -> String { "http://127.0.0.1:25800".into() }
fn d_komari_base() -> String { "http://127.0.0.1:25774".into() }
fn d_actor() -> String { "fleetd".into() }
fn d_two() -> usize { 2 }
fn d_timeout() -> u64 { 30 }
fn d_auth_mode() -> AuthMode { AuthMode::Komari }
fn d_cookie() -> String { "session_token".into() }
fn d_3() -> usize { 3 }
fn d_5() -> u32 { 5 }
fn d_9() -> u32 { 9 }
fn d_10() -> u64 { 10 }
fn d_15() -> u64 { 15 }
fn d_30() -> u64 { 30 }
fn d_60() -> u64 { 60 }
fn d_80() -> u32 { 80 }
fn d_120() -> u64 { 120 }
fn d_300() -> u64 { 300 }
fn d_1800() -> u64 { 1800 }
fn d_14400() -> u64 { 14400 }
fn d_tz() -> String { "Asia/Shanghai".into() }
fn d_true() -> bool { true }
fn d_min_sev() -> String { "warning".into() }
fn d_renewal() -> Vec<i64> { vec![7, 3, 1] }

impl Default for LegacyCfg {
    fn default() -> Self { toml::from_str("").unwrap() }
}
impl Default for KomariCfg {
    fn default() -> Self { toml::from_str("").unwrap() }
}
impl Default for AuthCfg {
    fn default() -> Self { toml::from_str("").unwrap() }
}
impl Default for PollCfg {
    fn default() -> Self { toml::from_str("").unwrap() }
}
impl Default for AlertCfg {
    fn default() -> Self { toml::from_str("").unwrap() }
}
impl Default for NotifyCfg {
    fn default() -> Self { toml::from_str("").unwrap() }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("读取配置 {} 失败：{e}", path.display()))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Config, String> {
        let mut cfg: Config = toml::from_str(text).map_err(|e| format!("配置格式错误：{e}"))?;
        for ip in RELAY_PUBLIC_IPS {
            if !cfg.ipintel.internal_ips.iter().any(|x| x == ip) {
                cfg.ipintel.internal_ips.push(ip.to_string());
            }
        }
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn listen_addr(&self) -> Result<SocketAddr, String> {
        self.listen.parse().map_err(|e| format!("listen 无效：{e}"))
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.legacy_source != "team-ops" && self.legacy_source != "native" {
            return Err(format!("legacy_source 只能是 \"team-ops\" 或 \"native\"（现在是 {:?}）", self.legacy_source));
        }
        let addr = self.listen_addr()?;
        if !addr.ip().is_loopback() {
            // fleetd 只在本机回环上监听，由 Caddy 转发；任何模式都不允许对外监听。
            return Err(format!("listen 必须是回环地址（现在是 {addr}）"));
        }
        if self.auth.mode == AuthMode::DevAllowLocal && !addr.ip().is_loopback() {
            return Err("auth.mode = dev-allow-local 只能配合回环 listen 使用".into());
        }
        if self.legacy.concurrency == 0 || self.legacy.concurrency > 4 {
            return Err("legacy.concurrency 须在 1..=4".into());
        }
        if self.poll.ondemand_ttl_s < 10 {
            return Err("poll.ondemand_ttl_s 不能小于 10".into());
        }
        if self.poll.idle_overview_s < self.poll.active_overview_s {
            return Err("poll.idle_overview_s 不能小于 active_overview_s".into());
        }
        self.alerts.digest_tz.parse::<chrono_tz::Tz>()
            .map_err(|_| format!("alerts.digest_tz 无效：{}", self.alerts.digest_tz))?;
        if self.alerts.startup_grace_max_s < self.alerts.startup_grace_s {
            return Err("alerts.startup_grace_max_s 不能小于 startup_grace_s".into());
        }
        if self.alerts.digest_hour > 23 {
            return Err("alerts.digest_hour 须在 0..=23".into());
        }
        let mut names = std::collections::HashSet::new();
        for c in &self.notify.channels {
            if !names.insert(c.name.clone()) {
                return Err(format!("渠道名重复：{}", c.name));
            }
            crate::alerts::Severity::parse(&c.min_severity).ok_or_else(|| format!("渠道 {} 的 min_severity 无效", c.name))?;
            match c.kind.as_str() {
                "telegram" => {
                    if c.bot_token_file.is_none() || c.chat_id.is_none() {
                        return Err(format!("渠道 {}：telegram 需要 bot_token_file 与 chat_id", c.name));
                    }
                }
                "wecom" => {
                    if c.url.is_none() && c.url_file.is_none() {
                        return Err(format!("渠道 {}：wecom 需要 url 或 url_file", c.name));
                    }
                }
                "webhook" => {
                    if c.url.is_none() && c.url_file.is_none() {
                        return Err(format!("渠道 {}：webhook 需要 url 或 url_file", c.name));
                    }
                    if c.secret_file.is_none() {
                        return Err(format!("渠道 {}：webhook 需要 secret_file（HMAC-SHA256 签名）", c.name));
                    }
                }
                "smtp" => {
                    if c.host.is_none() || c.from.is_none() || c.to.is_empty() {
                        return Err(format!("渠道 {}：smtp 需要 host、from、to", c.name));
                    }
                }
                other => return Err(format!("渠道 {}：未知类型 {other}", c.name)),
            }
        }
        if self.edge.native_health != "auto" && self.edge.native_health != "off" {
            return Err("edge.native_health 取值：auto | off".into());
        }
        if self.ipintel_backfill_days > 14 {
            return Err("ipintel_backfill_days 不能超过 14（旧系统只留 14 天访问记录）".into());
        }
        for ip in &self.ipintel.internal_ips {
            ip.parse::<std::net::IpAddr>().map_err(|_| format!("ipintel.internal_ips 里有无效地址：{ip}"))?;
        }
        if let Some(u) = &self.notify.healthchecks_url {
            if !u.starts_with("https://") {
                return Err("notify.healthchecks_url 必须是 https".into());
            }
        }
        Ok(())
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("fleetd.db")
    }

    /// 原生旧库（旧 operations.db 同构；`fleetd legacy snapshot` 导入）
    pub fn legacy_db_path(&self) -> PathBuf {
        self.data_dir.join("legacy.db")
    }
}

/// 读取机密文件：去掉首尾空白；允许 JSON {"key": "..."}（旧插件 backend-key.json 的格式）。
pub fn read_secret(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("读取 {} 失败：{e}", path.display()))?;
    let t = text.trim();
    if t.starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(t).map_err(|e| format!("{} 不是有效 JSON：{e}", path.display()))?;
        return v
            .get("key")
            .and_then(|k| k.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| format!("{} 缺少 key 字段", path.display()));
    }
    if t.is_empty() {
        return Err(format!("{} 为空", path.display()));
    }
    Ok(t.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.listen, "127.0.0.1:25900");
        assert_eq!(c.poll.active_overview_s, 10);
        assert_eq!(c.poll.idle_overview_s, 60);
        assert_eq!(c.auth.mode, AuthMode::Komari);
        assert!(c.notify.shadow);
        assert!(c.ipintel.enabled);
        for ip in RELAY_PUBLIC_IPS {
            assert!(c.ipintel.internal_ips.iter().any(|x| x == ip), "{ip}");
        }
        let c = Config::parse("[ipintel]\nenabled = false\ninternal_ips = [\"1.2.3.4\"]").unwrap();
        assert!(!c.ipintel.enabled);
        assert_eq!(c.ipintel.internal_ips.len(), 4);
    }

    #[test]
    fn refuses_public_listen() {
        assert!(Config::parse("listen = \"0.0.0.0:25900\"").is_err());
        let e = Config::parse("listen = \"10.0.0.1:18396\"\n[auth]\nmode = \"dev-allow-local\"").unwrap_err();
        assert!(e.contains("回环"), "{e}");
        assert!(Config::parse("listen = \"127.0.0.1:18396\"\n[auth]\nmode = \"dev-allow-local\"").is_ok());
    }

    #[test]
    fn auth_mode_fleetd_and_dead_keys_optional() {
        // 生产配置去掉 [legacy] base_url / [komari] base_url 后与写着默认值时完全相同
        let a = Config::parse("legacy_source = \"native\"\n[komari]\nenabled = false\n[auth]\nmode = \"fleetd\"").unwrap();
        let b = Config::parse("legacy_source = \"native\"\n[legacy]\nbase_url = \"http://127.0.0.1:25800\"\n[komari]\nenabled = false\nbase_url = \"http://127.0.0.1:25774\"\n[auth]\nmode = \"komari\"").unwrap();
        assert_eq!(a.auth.mode, AuthMode::Fleetd);
        assert_eq!(a.legacy.base_url, b.legacy.base_url);
        assert_eq!(a.komari.base_url, b.komari.base_url);
        assert_eq!(a.alerts.startup_grace_s, 120);
        assert!(Config::parse("[alerts]\nstartup_grace_s = 300\nstartup_grace_max_s = 200").is_err());
    }

    #[test]
    fn channel_validation() {
        let e = Config::parse("[[notify.channels]]\nname=\"x\"\ntype=\"webhook\"\nurl=\"https://h\"").unwrap_err();
        assert!(e.contains("secret_file"));
    }

    #[test]
    fn secret_json_or_raw() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("k");
        std::fs::write(&p, "{\"key\": \"abc\"}\n").unwrap();
        assert_eq!(read_secret(&p).unwrap(), "abc");
        std::fs::write(&p, "  raw-key\n").unwrap();
        assert_eq!(read_secret(&p).unwrap(), "raw-key");
    }
}
