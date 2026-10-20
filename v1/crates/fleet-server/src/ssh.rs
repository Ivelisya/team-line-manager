//! 用系统 ssh 登录机器执行脚本（脚本经 stdin 传入，不出现在命令行）。
//!
//! 主机公钥：上架预检时 keyscan 一次并写入中控自己的 known_hosts（TOFU），之后一律 StrictHostKeyChecking=yes。

use anyhow::{bail, Context};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Clone)]
pub struct Target {
    pub ip: String,
    pub port: u16,
    pub key: PathBuf,
    pub known_hosts: PathBuf,
    /// 只在引导阶段用：root 密码（经环境变量交给 sshpass，不上命令行）
    pub password: Option<String>,
}

#[derive(Debug)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool { self.code == 0 }
    /// 解析脚本里 `key=value` 形式的输出行
    pub fn kv(&self, k: &str) -> Option<&str> {
        self.stdout.lines().rev().find_map(|l| l.strip_prefix(k).and_then(|r| r.strip_prefix('=')))
    }
}

fn host_spec(ip: &str, port: u16) -> String {
    if port == 22 { ip.to_string() } else { format!("[{ip}]:{port}") }
}

impl Target {
    fn base(&self) -> Command {
        let mut c = if self.password.is_some() {
            let mut c = Command::new("sshpass");
            c.arg("-e").arg("ssh");
            c.env("SSHPASS", self.password.clone().unwrap_or_default());
            c.args(["-o", "PubkeyAuthentication=no", "-o", "PreferredAuthentications=password,keyboard-interactive"]);
            c
        } else {
            let mut c = Command::new("ssh");
            c.args(["-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes", "-i"]).arg(&self.key);
            c
        };
        c.args(["-o", "StrictHostKeyChecking=yes", "-o", "ConnectTimeout=10", "-o", "ServerAliveInterval=10", "-o", "LogLevel=ERROR"]);
        c.arg("-o").arg(format!("UserKnownHostsFile={}", self.known_hosts.display()));
        c.arg("-p").arg(self.port.to_string());
        c.arg(format!("root@{}", self.ip));
        c
    }

    /// 执行 bash 脚本（经 stdin），可选再追加一段二进制 stdin（用于推文件时就别用这个，用 push）
    pub async fn run(&self, script: &str, timeout: Duration) -> anyhow::Result<Output> {
        let mut c = self.base();
        c.arg("bash -s");
        c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let mut child = c.spawn().context("启动 ssh")?;
        let mut stdin = child.stdin.take().unwrap();
        let body = script.to_string();
        tokio::spawn(async move {
            let _ = stdin.write_all(body.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });
        let out = tokio::time::timeout(timeout, child.wait_with_output()).await.context("ssh 超时")??;
        Ok(Output {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    /// 推文件：sha256 相同则跳过；否则写临时文件再原子替换。
    pub async fn push(&self, local: &Path, remote: &str, mode: &str, timeout: Duration) -> anyhow::Result<bool> {
        let data = tokio::fs::read(local).await.with_context(|| format!("读取 {}", local.display()))?;
        let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&data));
        let chk = self.run(&format!("sha256sum '{remote}' 2>/dev/null | cut -d' ' -f1"), Duration::from_secs(20)).await?;
        if chk.stdout.trim() == sha {
            return Ok(false);
        }
        let mut c = self.base();
        c.arg(format!(
            "set -e; d=$(dirname '{remote}'); mkdir -p \"$d\"; t=\"$d/.push.$$\"; cat > \"$t\"; \
             [ \"$(sha256sum \"$t\" | cut -d' ' -f1)\" = '{sha}' ] || {{ rm -f \"$t\"; echo SHA_MISMATCH >&2; exit 3; }}; \
             chmod {mode} \"$t\"; mv -f \"$t\" '{remote}'"
        ));
        c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let mut child = c.spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        tokio::spawn(async move {
            let _ = stdin.write_all(&data).await;
            let _ = stdin.shutdown().await;
        });
        let out = tokio::time::timeout(timeout, child.wait_with_output()).await.context("推文件超时")??;
        if !out.status.success() {
            bail!("推送 {remote} 失败：{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(true)
    }
}

/// keyscan 拿 ed25519 公钥（两次，必须一致），写入 known_hosts（先删同主机旧条目）。返回指纹行。
pub async fn pin_host_key(known_hosts: &Path, ip: &str, port: u16) -> anyhow::Result<String> {
    let mut seen: Vec<String> = vec![];
    for i in 0..2 {
        if i > 0 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let out = Command::new("ssh-keyscan")
            .args(["-T", "5", "-t", "ed25519", "-p", &port.to_string(), ip])
            .output()
            .await
            .context("ssh-keyscan")?;
        let s = String::from_utf8_lossy(&out.stdout);
        for l in s.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
            // 「host type key」→ 只取 type key
            let parts: Vec<&str> = l.split_whitespace().collect();
            if parts.len() >= 3 {
                let k = format!("{} {}", parts[1], parts[2]);
                if !seen.contains(&k) {
                    seen.push(k);
                }
            }
        }
    }
    match seen.len() {
        0 => bail!("拿不到 {ip}:{port} 的主机公钥（SSH 没开或被防火墙挡）"),
        1 => {}
        _ => bail!("{ip}:{port} 两次返回了不同的主机公钥：可能 IP 冲突或被冒用，停止"),
    }
    forget_host(known_hosts, ip, port).await?;
    let line = format!("{} {}\n", host_spec(ip, port), seen[0]);
    let mut f = tokio::fs::OpenOptions::new().create(true).append(true).open(known_hosts).await?;
    f.write_all(line.as_bytes()).await?;
    Ok(seen[0].clone())
}

/// 从 known_hosts 删除这台主机（退役、彻底删除时调用）
pub async fn forget_host(known_hosts: &Path, ip: &str, port: u16) -> anyhow::Result<()> {
    if !known_hosts.exists() {
        return Ok(());
    }
    let _ = Command::new("ssh-keygen").arg("-f").arg(known_hosts).arg("-R").arg(host_spec(ip, port)).output().await?;
    Ok(())
}

/// TCP 可达性
pub async fn tcp_ok(ip: &str, port: u16, timeout: Duration) -> bool {
    tokio::time::timeout(timeout, tokio::net::TcpStream::connect((ip, port))).await.map(|r| r.is_ok()).unwrap_or(false)
}
