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
            bail!("缺少配置：{miss}。设置环境变量，或运行 `fleetctl login --url <中控地址>`（再输入 API 密钥）")
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
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        b.create(d).with_context(|| format!("创建目录 {}", d.display()))?;
    }
    let body = toml::to_string(&FileCfg { url: Some(url.to_string()), token: Some(token.to_string()) })?;
    write_private(&p, &body).with_context(|| format!("写入 {}", p.display()))?;
    Ok(p)
}

/// 写含密钥的文件：先写同目录的临时文件（unix 上新建即 0600，不经过 umask 决定的宽权限），落盘后 rename 换上去。
/// 中途失败（磁盘满、被杀）原来的配置原样留着，不会只剩半个文件；换上去的文件总是 0600（原来宽的也随之收紧）
fn write_private(p: &std::path::Path, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    let dir = p.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let tmp = dir.join(format!(".{name}.tmp.{}.{nanos}", std::process::id()));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let written = o.open(&tmp).and_then(|mut f| {
        f.write_all(body.as_bytes())?;
        f.sync_all()
    });
    if let Err(e) = written.and_then(|_| std::fs::rename(&tmp, p)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // rename 本身落盘（目录项）；打不开目录的平台（Windows）跳过
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
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

    #[cfg(unix)]
    #[test]
    fn private_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("fleetctl-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let new = d.join("new.toml");
        write_private(&new, "a").unwrap();
        assert_eq!(mode(&new), 0o600);
        let old = d.join("old.toml");
        std::fs::write(&old, "x").unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&old, "b").unwrap();
        assert_eq!((mode(&old), std::fs::read_to_string(&old).unwrap().as_str()), (0o600, "b"));
        // 临时文件不留在目录里
        let left: Vec<_> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(left.iter().all(|n| !n.contains(".tmp.")), "{left:?}");
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// 写失败（目标是目录，rename 换不上去）时原来的东西不动、临时文件删掉
    #[test]
    fn failed_write_keeps_old_and_cleans_tmp() {
        let d = std::env::temp_dir().join(format!("fleetctl-cfg-fail-{}", std::process::id()));
        let target = d.join("config.toml");
        std::fs::create_dir_all(target.join("sub")).unwrap();
        assert!(write_private(&target, "x").is_err());
        assert!(target.join("sub").is_dir());
        let left: Vec<_> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(left, vec!["config.toml".to_string()]);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn rewrite_replaces_content() {
        let d = std::env::temp_dir().join(format!("fleetctl-cfg-rw-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("config.toml");
        write_private(&p, "url = \"a\"\n").unwrap();
        write_private(&p, "url = \"b\"\n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "url = \"b\"\n");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
