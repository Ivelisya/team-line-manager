//! 小工具：时间、原子写文件、JSON 状态文件

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 错误连同 source 链拼成一行，另给最里层的那个（reqwest 的顶层只有「error sending request」，真正原因在 source 里）
pub fn error_chain(e: &dyn std::error::Error) -> (String, String) {
    let (mut s, mut inner) = (e.to_string(), e.to_string());
    let mut src = e.source();
    while let Some(x) = src {
        inner = x.to_string();
        s.push_str(": ");
        s.push_str(&inner);
        src = x.source();
    }
    (s, inner)
}

/// 同目录临时文件 → fsync → rename → fsync 目录；权限 0600。
/// 写入 / fsync / 改名任一步失败都删掉临时文件（磁盘满时不留下半截文件占空间）
pub fn write_atomic(path: &Path, data: &[u8], fsync: bool) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.tmp"));
    if let Err(e) = write_then_rename(&tmp, path, data, fsync) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if fsync {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

fn write_then_rename(tmp: &Path, path: &Path, data: &[u8], fsync: bool) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(tmp)
        .with_context(|| format!("创建 {}", tmp.display()))?;
    f.write_all(data).with_context(|| format!("写入 {}", tmp.display()))?;
    if fsync {
        f.sync_all().with_context(|| format!("fsync {}", tmp.display()))?;
    }
    drop(f);
    std::fs::rename(tmp, path).with_context(|| format!("rename → {}", path.display()))
}

pub fn load_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let s = std::fs::read(path).ok()?;
    match serde_json::from_slice(&s) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!("状态文件 {} 损坏，忽略：{e}", path.display());
            None
        }
    }
}

pub fn save_json<T: Serialize>(path: &Path, v: &T, fsync: bool) -> Result<()> {
    write_atomic(path, &serde_json::to_vec(v)?, fsync)
}

/// 带抖动的指数退避：base·2^n，封顶 cap，再乘 [0.5,1.5)
pub fn backoff(fails: u32, base_s: u64, cap_s: u64) -> std::time::Duration {
    use rand::Rng;
    let d = (base_s << fails.min(10)).min(cap_s) as f64;
    let j: f64 = rand::thread_rng().gen_range(0.5..1.5);
    std::time::Duration::from_secs_f64(d * j)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_roundtrip() {
        let d = std::env::temp_dir().join(format!("fa-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("x.json");
        save_json(&p, &serde_json::json!({"a":1}), true).unwrap();
        let v: serde_json::Value = load_json(&p).unwrap();
        assert_eq!(v["a"], 1);
        // 改名失败（目标是目录）：临时文件要删掉，不能每失败一次留一个
        let target = d.join("dir.json");
        std::fs::create_dir_all(target.join("sub")).unwrap();
        assert!(write_atomic(&target, b"{}", true).is_err());
        assert!(!d.join(".dir.json.tmp").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn backoff_bounded() {
        for n in 0..20 {
            assert!(backoff(n, 5, 60).as_secs_f64() < 90.0);
        }
    }
}
