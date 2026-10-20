//! SSH：系统 ssh（ControlMaster 复用）、严格主机公钥（每台节点一个已钉住的 known_hosts 文件）、
//! 密码引导走 SSH_ASKPASS（密码只从 0600 文件读，不上命令行、不进日志）。

use crate::model::{ErrClass, StepErr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct Remote {
    pub ssh_bin: String,
    pub key: PathBuf,
    pub known_hosts: PathBuf,
    pub host: String,
    pub port: u16,
    pub user: String,
    /// None = 不复用连接（密码引导、DD 前后）。
    pub control_dir: Option<PathBuf>,
}

#[derive(Debug)]
pub struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
}

pub fn control_dir() -> PathBuf {
    // unix 套接字路径 ≤ 104 字节，所以不放 work_dir。
    let uid = std::env::var("USER").unwrap_or_else(|_| "u".into());
    let d = PathBuf::from(format!("/tmp/fonb-cm-{uid}"));
    let _ = std::fs::create_dir_all(&d);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700));
    }
    d
}

fn base_args(r: &Remote) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-p".into(), r.port.to_string(),
        "-o".into(), "StrictHostKeyChecking=yes".into(),
        "-o".into(), format!("UserKnownHostsFile={}", r.known_hosts.display()),
        "-o".into(), "GlobalKnownHostsFile=/dev/null".into(),
        "-o".into(), "ConnectTimeout=10".into(),
        "-o".into(), "ServerAliveInterval=5".into(),
        "-o".into(), "ServerAliveCountMax=3".into(),
        "-o".into(), "LogLevel=ERROR".into(),
    ];
    match &r.control_dir {
        Some(d) => a.extend([
            "-o".into(), "ControlMaster=auto".into(),
            "-o".into(), format!("ControlPath={}/%C", d.display()),
            "-o".into(), "ControlPersist=60".into(),
        ]),
        None => a.extend(["-o".into(), "ControlMaster=no".into(), "-o".into(), "ControlPath=none".into()]),
    }
    a
}

fn classify(out: &Out) -> Option<StepErr> {
    let s = &out.stderr;
    if s.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") || s.contains("Host key verification failed") {
        return Some(StepErr { class: ErrClass::Identity, msg: "主机公钥和已钉住的不一致（可能被别的机器占了 IP，或已经重装）".into() });
    }
    if out.code == 255 {
        let tail: String = s.lines().last().unwrap_or("").chars().take(200).collect();
        if s.contains("Permission denied") {
            return Some(StepErr { class: ErrClass::Input, msg: format!("SSH 认证被拒：{tail}") });
        }
        return Some(StepErr { class: ErrClass::Transient, msg: format!("SSH 连接失败：{tail}") });
    }
    None
}

async fn exec(mut cmd: Command, stdin: &[u8], timeout: Duration) -> Result<Out, StepErr> {
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| StepErr { class: ErrClass::Bug, msg: format!("起 ssh 失败：{e}") })?;
    let mut si = child.stdin.take().unwrap();
    let data = stdin.to_vec();
    let w = tokio::spawn(async move {
        let _ = si.write_all(&data).await;
        let _ = si.shutdown().await;
    });
    let res = tokio::time::timeout(timeout, child.wait_with_output()).await;
    w.abort();
    match res {
        Err(_) => Err(StepErr { class: ErrClass::Transient, msg: format!("远端命令超时（{} 秒）", timeout.as_secs()) }),
        Ok(Err(e)) => Err(StepErr { class: ErrClass::Transient, msg: format!("ssh 进程出错：{e}") }),
        Ok(Ok(o)) => Ok(Out {
            code: o.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }),
    }
}

impl Remote {
    /// 用上架密钥执行 bash 脚本（脚本从 stdin 进，不上命令行）。连接层错误映射成分类错误；脚本非零退出码原样返回。
    pub async fn run(&self, script: &str, timeout: Duration) -> Result<Out, StepErr> {
        let mut cmd = Command::new(&self.ssh_bin);
        cmd.args(base_args(self))
            .args(["-i", &self.key.display().to_string(), "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes"])
            .arg(format!("{}@{}", self.user, self.host))
            .arg("bash -s");
        let out = exec(cmd, script.as_bytes(), timeout).await?;
        if let Some(e) = classify(&out) {
            return Err(e);
        }
        Ok(out)
    }

    /// 跑脚本，非零退出当作错误（带 stderr 尾部）。
    pub async fn run_ok(&self, script: &str, timeout: Duration, class: ErrClass) -> Result<String, StepErr> {
        let o = self.run(script, timeout).await?;
        if !o.ok() {
            let tail: String = o.stderr.trim().chars().rev().take(600).collect::<String>().chars().rev().collect();
            let otail: String = o.stdout.trim().chars().rev().take(300).collect::<String>().chars().rev().collect();
            return Err(StepErr { class, msg: format!("远端命令失败（rc={}）：{} {}", o.code, tail, otail) });
        }
        Ok(o.stdout)
    }

    /// 把本地文件推到远端：写临时文件 → 校验 sha256 → chmod → 原子改名。
    pub async fn push_file(&self, local: &Path, remote: &str, mode: &str, sha256: &str) -> Result<(), StepErr> {
        let data = std::fs::read(local).map_err(|e| StepErr { class: ErrClass::Bug, msg: format!("读 {}：{e}", local.display()) })?;
        let tmp = format!("{remote}.fleet-tmp");
        let script = format!("set -e; umask 077; mkdir -p \"$(dirname '{remote}')\"; cat > '{tmp}'; echo \"{sha256}  {tmp}\" | sha256sum -c --status; chmod {mode} '{tmp}'; mv -f '{tmp}' '{remote}'");
        let mut cmd = Command::new(&self.ssh_bin);
        cmd.args(base_args(self))
            .args(["-i", &self.key.display().to_string(), "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes"])
            .arg(format!("{}@{}", self.user, self.host))
            .arg(script);
        let out = exec(cmd, &data, Duration::from_secs(180)).await?;
        if let Some(e) = classify(&out) {
            return Err(e);
        }
        if !out.ok() {
            return Err(StepErr { class: ErrClass::Transient, msg: format!("推送 {remote} 失败（rc={}）：{}", out.code, out.stderr.trim()) });
        }
        Ok(())
    }

    /// 用密码（文件）登录执行脚本。只对已钉住的主机公钥发送（StrictHostKeyChecking=yes）。
    pub async fn run_password(&self, askpass: &Path, password_file: &Path, script: &str, timeout: Duration) -> Result<Out, StepErr> {
        let mut r = self.clone();
        r.control_dir = None;
        // setsid：脱离控制终端，旧 OpenSSH（< 8.4，无 SSH_ASKPASS_REQUIRE）也会走 askpass。
        let mut cmd = Command::new("setsid");
        cmd.arg("-w")
            .arg(&self.ssh_bin)
            .args(base_args(&r))
            .args([
                "-o", "BatchMode=no", "-o", "PubkeyAuthentication=no",
                "-o", "PreferredAuthentications=password,keyboard-interactive", "-o", "NumberOfPasswordPrompts=1",
            ])
            .arg(format!("{}@{}", self.user, self.host))
            .arg("bash -s")
            .env("SSH_ASKPASS", askpass)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env("DISPLAY", ":0")
            .env("FLEET_ONB_PWFILE", password_file);
        let out = exec(cmd, script.as_bytes(), timeout).await?;
        if let Some(e) = classify(&out) {
            return Err(e);
        }
        Ok(out)
    }

    pub async fn close_master(&self) {
        if let Some(d) = &self.control_dir {
            let mut cmd = Command::new(&self.ssh_bin);
            cmd.args(["-o", &format!("ControlPath={}/%C", d.display()), "-p", &self.port.to_string(), "-O", "exit"])
                .arg(format!("{}@{}", self.user, self.host));
            let _ = exec(cmd, b"", Duration::from_secs(5)).await;
        }
    }
}

/// ssh-keyscan 一次：返回 (ed25519 公钥行里的 "类型 公钥" 部分, banner)。
pub async fn keyscan(bin: &str, host: &str, port: u16) -> Result<(Vec<String>, String), StepErr> {
    let mut cmd = Command::new(bin);
    cmd.args(["-T", "5", "-p", &port.to_string(), "-t", "ed25519", host]);
    let o = exec(cmd, b"", Duration::from_secs(15)).await?;
    let keys: Vec<String> = o
        .stdout
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let _host = it.next()?;
            Some(format!("{} {}", it.next()?, it.next()?))
        })
        .collect();
    let banner = o.stderr.lines().chain(o.stdout.lines()).find(|l| l.contains("SSH-2.0")).map(|l| l.split_whitespace().last().unwrap_or("").to_string()).unwrap_or_default();
    Ok((keys, banner))
}

pub fn known_hosts_line(host: &str, port: u16, key: &str) -> String {
    if port == 22 {
        format!("{host} {key}\n")
    } else {
        format!("[{host}]:{port} {key}\n")
    }
}

pub fn ensure_askpass(work_dir: &Path) -> std::io::Result<PathBuf> {
    let p = work_dir.join("askpass.sh");
    let body = "#!/bin/sh\nexec cat \"$FLEET_ONB_PWFILE\"\n";
    if std::fs::read_to_string(&p).ok().as_deref() != Some(body) {
        std::fs::write(&p, body)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(p)
}

/// shell 单引号转义。
pub fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
