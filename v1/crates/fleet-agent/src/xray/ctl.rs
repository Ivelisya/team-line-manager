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

/// (start_ticks, 启动时刻 unix ms)
pub fn proc_start(pid: u32) -> Option<(u64, i64)> {
    let ticks = parse_stat_starttime(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)?;
    let btime: i64 = std::fs::read_to_string("/proc/stat").ok()?.lines().find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok())?;
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as i64;
    Some((ticks, btime * 1000 + ticks as i64 * 1000 / hz))
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
