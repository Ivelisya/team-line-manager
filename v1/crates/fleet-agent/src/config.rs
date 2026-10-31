//! `/etc/fleet-agent/agent.toml`

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Node,
    Relay,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server_url: String,
    #[serde(default = "d_token_file")]
    pub token_file: PathBuf,
    pub role: Role,
    /// 仅用于日志，身份由令牌决定
    #[serde(default)]
    pub machine_id: String,
    #[serde(default = "d_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "d_xray_bin")]
    pub xray_bin: PathBuf,
    #[serde(default = "d_xray_config")]
    pub xray_config: PathBuf,
    #[serde(default = "d_xray_service")]
    pub xray_service: String,
    #[serde(default = "d_access_log")]
    pub access_log: PathBuf,
    /// geoip.dat / geosite.dat 所在目录（XRAY_LOCATION_ASSET）
    #[serde(default = "d_asset_dir")]
    pub xray_asset_dir: PathBuf,
    /// sing-box（线路机的 AnyTLS 外壳）：程序不在就不管 AnyTLS，只报「未安装」
    #[serde(default = "d_singbox_bin")]
    pub singbox_bin: PathBuf,
    /// sing-box 的配置、自签证书所在目录（root 0700；fleet-agent 单元把它列进 ReadWritePaths）
    #[serde(default = "d_singbox_dir")]
    pub singbox_dir: PathBuf,
    #[serde(default = "d_singbox_service")]
    pub singbox_service: String,
    /// 流量计量只计这些网卡（特殊机型手动指定）；不写则按默认路由 / 公网地址自动挑选
    #[serde(default)]
    pub nic_ifaces: Vec<String>,
    /// 不认识的键：只警告、不拒绝启动。中控生成的 agent.toml 比 agent 新（多了新版才有的键）时，
    /// 旧 agent 拒绝启动就再也收不到升级；拼写错误也只是该项回落到默认值，日志里看得到
    #[serde(flatten)]
    unknown: toml::Table,
}

fn d_token_file() -> PathBuf { "/etc/fleet-agent/token".into() }
fn d_data_dir() -> PathBuf { "/var/lib/fleet-agent".into() }
fn d_xray_bin() -> PathBuf { "/usr/local/bin/xray".into() }
fn d_xray_config() -> PathBuf { "/etc/fleet-agent/xray.json".into() }
fn d_xray_service() -> String { "fleet-xray.service".into() }
fn d_access_log() -> PathBuf { "/var/log/fleet-xray/access.log".into() }
fn d_asset_dir() -> PathBuf { "/usr/local/share/xray".into() }
fn d_singbox_bin() -> PathBuf { "/usr/local/bin/sing-box".into() }
fn d_singbox_dir() -> PathBuf { "/etc/fleet-singbox".into() }
fn d_singbox_service() -> String { "fleet-singbox.service".into() }

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(path).with_context(|| format!("读取配置 {}", path.display()))?;
        let c: Config = toml::from_str(&s).with_context(|| format!("解析配置 {}", path.display()))?;
        for k in c.unknown.keys() {
            tracing::warn!("配置 {} 里有不认识的项 {k}，已忽略（拼写错误，或配置比 agent 版本新）", path.display());
        }
        Ok(c)
    }

    /// 本地诊断子命令（render/apply）在没有配置文件时使用的默认值
    pub fn relay_default() -> Self {
        toml::from_str("role = \"relay\"").expect("默认配置")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_unknown_fields() {
        let c = Config::relay_default();
        assert_eq!(c.role, Role::Relay);
        assert_eq!(c.xray_service, "fleet-xray.service");
        // 旧版中控生成的 xray_api 已不认识：只告警，不拒绝启动
        let c = toml::from_str::<Config>("role = \"node\"\nxray_api = \"127.0.0.1:62789\"\ndata_dir = \"/x\"").unwrap();
        assert_eq!((c.role, c.data_dir.to_str()), (Role::Node, Some("/x")));
        assert_eq!(c.unknown.keys().collect::<Vec<_>>(), ["xray_api"]);
    }
}
