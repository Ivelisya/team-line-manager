//! 连接配置：环境变量 FLEET_URL / FLEET_TOKEN 优先，其次配置文件
//! （Linux/macOS：~/.config/fleetctl/config.toml；Windows：%APPDATA%\fleetctl\config.toml）。

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize, Clone, PartialEq)]
pub struct FileCfg {
    pub url: Option<String>,
    pub token: Option<String>,
}

pub fn config_path() -> anyhow::Result<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from).context("找不到 %APPDATA%")?
    } else {
        match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            Some(x) => PathBuf::from(x),
            None => PathBuf::from(std::env::var_os("HOME").context("找不到 $HOME")?).join(".config"),
        }
    };
    Ok(base.join("fleetctl").join("config.toml"))
}

fn non_empty(v: Option<String>) -> Option<String> { v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) }

/// 合并：环境变量优先于文件
pub fn merge(env_url: Option<String>, env_token: Option<String>, file: FileCfg) -> anyhow::Result<(String, String)> {
    let url = non_empty(env_url).or_else(|| non_empty(file.url));
    let token = non_empty(env_token).or_else(|| non_empty(file.token));
    match (url, token) {
        (Some(u), Some(t)) => Ok((u, t)),
        (u, t) => {
            let miss = [u.is_none().then_some("FLEET_URL"), t.is_none().then_some("FLEET_TOKEN")].into_iter().flatten().collect::<Vec<_>>().join("、");
            bail!("缺少配置：{miss}。设置环境变量，或运行 `fleetctl login --url <中控地址> --token <API 密钥>`")
        }
    }
}

pub fn load_file() -> FileCfg {
    config_path().ok().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
}

/// 只取中控地址（环境变量优先）
pub fn load_url() -> Option<String> {
    std::env::var("FLEET_URL").ok().filter(|s| !s.trim().is_empty()).or_else(|| load_file().url.filter(|s| !s.trim().is_empty()))
}

pub fn load() -> anyhow::Result<(String, String)> {
    merge(std::env::var("FLEET_URL").ok(), std::env::var("FLEET_TOKEN").ok(), load_file())
}

pub fn save(url: &str, token: &str) -> anyhow::Result<PathBuf> {
    let p = config_path()?;
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).with_context(|| format!("创建目录 {}", d.display()))?;
    }
    let body = toml::to_string(&FileCfg { url: Some(url.to_string()), token: Some(token.to_string()) })?;
    std::fs::write(&p, body).with_context(|| format!("写入 {}", p.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_beats_file() {
        let f = FileCfg { url: Some("http://file".into()), token: Some("ft".into()) };
        assert_eq!(merge(Some("http://env".into()), None, f.clone()).unwrap(), ("http://env".into(), "ft".into()));
        assert_eq!(merge(Some("  ".into()), Some("".into()), f).unwrap(), ("http://file".into(), "ft".into()));
    }

    #[test]
    fn missing_is_explained() {
        let e = merge(None, None, FileCfg::default()).unwrap_err().to_string();
        assert!(e.contains("FLEET_URL") && e.contains("FLEET_TOKEN") && e.contains("login"), "{e}");
    }

    #[test]
    fn file_roundtrip() {
        let c = FileCfg { url: Some("http://x:8080".into()), token: Some("flk_aa_bb".into()) };
        let s = toml::to_string(&c).unwrap();
        assert_eq!(toml::from_str::<FileCfg>(&s).unwrap(), c);
    }
}
