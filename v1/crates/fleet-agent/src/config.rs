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
#[serde(deny_unknown_fields)]
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
    #[serde(default = "d_xray_api")]
    pub xray_api: String,
    #[serde(default = "d_xray_service")]
    pub xray_service: String,
    #[serde(default = "d_access_log")]
    pub access_log: PathBuf,
    /// geoip.dat / geosite.dat 所在目录（XRAY_LOCATION_ASSET）
    #[serde(default = "d_asset_dir")]
    pub xray_asset_dir: PathBuf,
    /// 自签证书的中控可在这里放 CA（PEM）
    #[serde(default)]
    pub ca_file: Option<PathBuf>,
}

fn d_token_file() -> PathBuf { "/etc/fleet-agent/token".into() }
fn d_data_dir() -> PathBuf { "/var/lib/fleet-agent".into() }
fn d_xray_bin() -> PathBuf { "/usr/local/bin/xray".into() }
fn d_xray_config() -> PathBuf { "/etc/fleet-agent/xray.json".into() }
fn d_xray_api() -> String { "127.0.0.1:62789".into() }
fn d_xray_service() -> String { "fleet-xray.service".into() }
fn d_access_log() -> PathBuf { "/var/log/fleet-xray/access.log".into() }
fn d_asset_dir() -> PathBuf { "/usr/local/share/xray".into() }

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(path).with_context(|| format!("读取配置 {}", path.display()))?;
        let c: Config = toml::from_str(&s).with_context(|| format!("解析配置 {}", path.display()))?;
        Ok(c)
    }

    /// 本地诊断子命令（render/apply）在没有配置文件时使用的默认值
    pub fn relay_default() -> Self {
        toml::from_str("role = \"relay\"").expect("默认配置")
    }

    pub fn api_port(&self) -> u16 {
        self.xray_api.rsplit_once(':').and_then(|(_, p)| p.parse().ok()).unwrap_or(62789)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_unknown_fields() {
        let c = Config::relay_default();
        assert_eq!(c.role, Role::Relay);
        assert_eq!(c.api_port(), 62789);
        assert!(toml::from_str::<Config>("role = \"node\"\nbogus = 1").is_err());
    }
}
