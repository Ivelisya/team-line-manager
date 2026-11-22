//! dist 里 agent 程序（`dist/fleet-agent`）的 sha256 与版本号，按文件的 (mtime, 大小, inode) 缓存。
//! 「全部升级」每个作业开始前都要核对 sha256（jobs::halted，每台机器一次），探测点升级、安装也要它；
//! 每次都把整个程序读进来算一遍、再起一个 `--version` 进程不划算，文件没变就用上次的结果。
//!
//! 刚写过的文件不缓存：文件时间戳的精度是内核的时钟节拍（几毫秒），同一节拍里原地改写成同样大小的内容，
//! (mtime, 大小, inode) 都不变（同 git 的 racy-clean）。mtime 离现在不到 [`SETTLE`] 的每次都重算。

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

/// mtime 离现在不到这么久的不缓存（见模块说明）
const SETTLE: Duration = Duration::from_secs(2);
/// `--version` 最多等这么久
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistAgent {
    /// 程序内容的 sha256（十六进制）
    pub sha256: String,
    /// `--version` 的第二段（`fleet-agent 0.1.0+abc` → `0.1.0+abc`）；跑不起来、没有输出为 None
    pub version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Key {
    mtime: SystemTime,
    size: u64,
    ino: u64,
}

fn key(m: &std::fs::Metadata) -> std::io::Result<Key> { Ok(Key { mtime: m.modified()?, size: m.len(), ino: m.ino() }) }

fn cache() -> &'static Mutex<HashMap<PathBuf, (Key, DistAgent)>> {
    static C: OnceLock<Mutex<HashMap<PathBuf, (Key, DistAgent)>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// dist 目录 `dist_dir` 里的 fleet-agent。没有这个文件、读不出来时报错；读得出但版本号读不出时 `version` 为 None
pub async fn get(dist_dir: &Path) -> std::io::Result<DistAgent> {
    let path = dist_dir.join("fleet-agent");
    let k = key(&tokio::fs::metadata(&path).await?)?;
    let hit = crate::lock(cache()).get(&path).filter(|(ck, _)| *ck == k).map(|(_, d)| d.clone());
    let d = match hit {
        Some(d) if d.version.is_some() => return Ok(d),
        // sha256 还能用；版本号上次没读出来（超时之类），再试一次
        Some(d) => DistAgent { version: run_version(&path).await, ..d },
        None => {
            let p = path.clone();
            let sha256 = tokio::task::spawn_blocking(move || std::fs::read(p).map(|b| hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&b))))
                .await
                .map_err(std::io::Error::other)??;
            DistAgent { sha256, version: run_version(&path).await }
        }
    };
    // 算的过程中文件被换了（前后的 key 不一样），或刚写过：这次的结果照用，不缓存
    let after = tokio::fs::metadata(&path).await.ok().and_then(|m| key(&m).ok());
    let settled = SystemTime::now().duration_since(k.mtime).is_ok_and(|age| age >= SETTLE);
    if after == Some(k) && settled {
        crate::lock(cache()).insert(path, (k, d.clone()));
    }
    Ok(d)
}

async fn run_version(path: &Path) -> Option<String> {
    let out = tokio::time::timeout(VERSION_TIMEOUT, tokio::process::Command::new(path).arg("--version").kill_on_drop(true).output()).await.ok()?.ok()?;
    if !out.status.success() {
        return None;
    }
    parse_version(&String::from_utf8_lossy(&out.stdout))
}

/// `--version` 输出的第二段
fn parse_version(stdout: &str) -> Option<String> { stdout.split_whitespace().nth(1).map(String::from) }

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, FileTimes};

    fn write_at(p: &Path, body: &[u8], mtime: SystemTime) {
        std::fs::write(p, body).unwrap();
        File::options().write(true).open(p).unwrap().set_times(FileTimes::new().set_modified(mtime)).unwrap();
    }

    fn sha(b: &[u8]) -> String { hex::encode(<sha2::Sha256 as sha2::Digest>::digest(b)) }

    #[test]
    fn parses_second_field() {
        assert_eq!(parse_version("fleet-agent 0.1.0+abc\n").as_deref(), Some("0.1.0+abc"));
        assert_eq!(parse_version("fleet-agent\n"), None);
        assert_eq!(parse_version(""), None);
    }

    /// 同一个 key 用缓存（这里故意原地改写成同样大小、再把 mtime 改回去，读到的仍是旧值：说明没有重读）；
    /// mtime 或大小变了就重算；刚写过的（mtime 在 SETTLE 之内）每次都重算
    #[tokio::test]
    async fn cache_follows_mtime_and_size() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("fleet-agent");
        let t1 = SystemTime::now() - Duration::from_secs(3600);
        // 没有执行权限：--version 起不来，version 为 None，sha256 照常
        write_at(&p, b"aaaa", t1);
        let d = get(dir.path()).await.unwrap();
        assert_eq!((d.sha256.as_str(), d.version.as_deref()), (sha(b"aaaa").as_str(), None));

        write_at(&p, b"bbbb", t1);
        assert_eq!(get(dir.path()).await.unwrap().sha256, sha(b"aaaa"), "key 没变：用缓存");

        write_at(&p, b"bbbb", t1 + Duration::from_secs(1));
        assert_eq!(get(dir.path()).await.unwrap().sha256, sha(b"bbbb"), "mtime 变了：重算");

        write_at(&p, b"ccccc", t1 + Duration::from_secs(1));
        assert_eq!(get(dir.path()).await.unwrap().sha256, sha(b"ccccc"), "大小变了：重算");

        // 刚写过：不缓存，同一节拍里原地改写成同样大小也读得到新的
        let now = SystemTime::now();
        write_at(&p, b"dddd", now);
        assert_eq!(get(dir.path()).await.unwrap().sha256, sha(b"dddd"));
        write_at(&p, b"eeee", now);
        assert_eq!(get(dir.path()).await.unwrap().sha256, sha(b"eeee"), "刚写过的不缓存");

        std::fs::remove_file(&p).unwrap();
        assert!(get(dir.path()).await.is_err(), "文件没了报错，不拿缓存顶");
    }
}
