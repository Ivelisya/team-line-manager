//! 进程管理：systemctl、/proc 里的 Xray 进程信息

use anyhow::{bail, Context, Result};
use std::time::Duration;
use tokio::process::Command;

pub async fn systemctl(args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("systemctl");
    cmd.args(args).stdin(std::process::Stdio::null()).kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(60), cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("systemctl {} 超时", args.join(" ")))?
        .context("启动 systemctl")?;
    if !out.status.success() {
        bail!("systemctl {} 失败（{}）：{}", args.join(" "), out.status, String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

pub async fn main_pid(service: &str) -> Option<u32> {
    let out = systemctl(&["show", "-p", "MainPID", "--value", service]).await.ok()?;
    out.trim().parse::<u32>().ok().filter(|p| *p > 0)
}

/// /proc/<pid>/stat 第 22 列 starttime（comm 可能含空格/括号，从最后一个 ')' 之后数）
pub fn parse_stat_starttime(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    // ')' 之后第一个字段是第 3 列 state，starttime 是第 22 列 → 下标 19
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// 只读 /proc/<pid>/stat 的 starttime：和 pid 一起标识 Xray 进程（流量计数纪元），也用来廉价地确认缓存的 MainPID
/// 还是同一个进程（进程没了 / pid 被复用都会对不上）
pub fn proc_start_ticks(pid: u32) -> Option<u64> {
    parse_stat_starttime(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starttime_with_weird_comm() {
        let s = "123 (xr ay) x) S 1 123 123 0 -1 4194560 100 0 0 0 5 3 0 0 20 0 9 0 98765 1000 2 18446744073709551615";
        assert_eq!(parse_stat_starttime(s), Some(98765));
    }
}
