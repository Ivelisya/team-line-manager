//! 中控登录路由的 ssh：经隧道（127.0.0.1:<tunnel_port>，OpenSSH ControlMaster 复用连接）或一键部署时直连（可带一次性密码）。
//! 路由上是 busybox ash，脚本一律交给 `sh -s`。主机公钥按 HostKeyAlias=router-<id> 在 `[routers] known_hosts` 里核对。

use crate::config::Config;
use crate::ssh::{ErrKind, Output};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;
use uuid::Uuid;

/// 不读 ssh_config（ProxyCommand、ForwardAgent、SendEnv… 都不能带进来），不读系统级 known_hosts，不更新主机公钥
fn common(c: &mut Command, known_hosts: &Path, id: Uuid) {
    c.args(["-F", "/dev/null", "-o", "StrictHostKeyChecking=yes", "-o", "UpdateHostKeys=no", "-o", "GlobalKnownHostsFile=/dev/null"]);
    c.arg("-o").arg(format!("UserKnownHostsFile={}", known_hosts.display()));
    c.arg("-o").arg(format!("HostKeyAlias=router-{id}"));
    c.args(["-o", "ConnectTimeout=10", "-o", "ServerAliveInterval=10", "-o", "ServerAliveCountMax=3", "-o", "LogLevel=ERROR"]);
}

fn control_path(cfg: &Config, id: Uuid) -> PathBuf { cfg.routers.control_dir.join(id.to_string()) }

/// 经隧道登录路由（root@127.0.0.1:<port>，中控密钥）。ControlPersist 5 分钟：30 秒一轮的采集复用同一条连接，不每次握手
pub fn via_tunnel(cfg: &Config, id: Uuid, port: i32) -> Command {
    let mut c = Command::new("ssh");
    c.args(["-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes", "-i"]).arg(&cfg.ssh_key);
    common(&mut c, &cfg.routers.known_hosts, id);
    c.args(["-o", "ControlMaster=auto", "-o", "ControlPersist=300"]);
    c.arg("-o").arg(format!("ControlPath={}", control_path(cfg, id).display()));
    c.arg("-p").arg(port.to_string()).arg("root@127.0.0.1").arg("sh -s");
    c
}

/// 让这台路由的 ControlMaster 退出：重新接入、删除、主机指纹对不上之后，下一次重新握手、重新核对主机公钥
pub async fn close_master(cfg: &Config, id: Uuid) {
    let path = control_path(cfg, id);
    if !path.exists() {
        return;
    }
    let mut c = Command::new("ssh");
    c.args(["-F", "/dev/null", "-o"]).arg(format!("ControlPath={}", path.display())).args(["-O", "exit", "root@127.0.0.1"]);
    c.kill_on_drop(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), c.output()).await;
}

/// 一键部署登录路由的目标：密码只在内存里，经环境变量 SSHPASS 交给 sshpass，不上命令行、不落库、不进日志
pub struct Login {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<String>,
}

impl std::fmt::Debug for Login {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}:{}（{}）", self.user, self.host, self.port, if self.password.is_some() { "密码" } else { "中控密钥" })
    }
}

/// 一键部署直连路由：有密码时 sshpass + 只用密码认证，没有时用中控密钥。主机公钥按 `known_hosts`（部署自己的临时文件）核对
pub fn direct(cfg: &Config, id: Uuid, l: &Login, known_hosts: &Path) -> Command {
    let mut c = match &l.password {
        Some(pw) => {
            let mut c = Command::new("sshpass");
            c.arg("-e").arg("ssh").env("SSHPASS", pw);
            c.args(["-o", "PubkeyAuthentication=no", "-o", "PreferredAuthentications=password,keyboard-interactive"]);
            c
        }
        None => {
            let mut c = Command::new("ssh");
            c.args(["-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes", "-i"]).arg(&cfg.ssh_key);
            c
        }
    };
    common(&mut c, known_hosts, id);
    c.arg("-p").arg(l.port.to_string()).arg(format!("{}@{}", l.user, l.host)).arg("sh -s");
    c
}

/// sshpass 自己的退出码（5 密码不对、6 主机公钥）折算成 ssh 的 255 + 对应错误文字（同 ssh.rs 的 Target::normalize）
pub fn normalize(mut o: Output, password: bool) -> Output {
    if password {
        match o.code {
            5 => {
                o.code = 255;
                o.stderr.push_str("\nPermission denied (password)");
            }
            6 => {
                o.code = 255;
                o.stderr.push_str("\nHost key verification failed.");
            }
            _ => {}
        }
    }
    o
}

/// keyscan 两次拿 ed25519 主机公钥（「ssh-ed25519 base64」），两次不一致报错；拿不到为 None
pub async fn keyscan(host: &str, port: u16) -> anyhow::Result<Option<String>> {
    let mut seen: Option<String> = None;
    for i in 0..2 {
        if i > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let out = Command::new("ssh-keyscan").args(["-T", "5", "-t", "ed25519", "-p", &port.to_string(), host]).kill_on_drop(true).output().await?;
        let s = String::from_utf8_lossy(&out.stdout);
        let got = s.lines().filter(|l| !l.starts_with('#')).find_map(|l| super::key_of(l.split_once(' ').map(|x| x.1).unwrap_or("")));
        match (&seen, got) {
            (_, None) => return Ok(None),
            (None, Some(k)) => seen = Some(k),
            (Some(a), Some(b)) if *a != b => anyhow::bail!("{host}:{port} 两次给出了不同的主机公钥：可能被冒用，停止"),
            _ => {}
        }
    }
    Ok(seen)
}

/// ssh 自己失败（退出码 255）时给人看的原因
pub fn explain(o: &Output, via_tunnel: bool) -> String {
    let detail = o.stderr.lines().map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("").to_string();
    match o.ssh_kind() {
        Some(ErrKind::HostKey) => format!(
            "路由的主机公钥和记录的不一样（{detail}）：{}",
            if via_tunnel { "隧道端口上的不是这台路由；确认路由重装过再「重新生成接入命令」，会清掉旧记录" } else { "可能连错了机器" }
        ),
        Some(ErrKind::Auth) => format!(
            "登录被拒（{detail}）：{}",
            if via_tunnel { "路由的 /etc/dropbear/authorized_keys 里没有中控的公钥，重新接入会加上" } else { "用户名或密码不对，或路由不收这个中控的密钥" }
        ),
        _ if via_tunnel => format!("经隧道连不上路由：{detail}"),
        _ => format!("SSH 连接失败：{detail}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(code: i32, stderr: &str) -> Output { Output { code, stdout: String::new(), stderr: stderr.into() } }

    #[test]
    fn explains_failures() {
        assert!(explain(&out(255, "Host key verification failed."), true).starts_with("路由的主机公钥和记录的不一样"));
        assert!(explain(&out(255, "root@127.0.0.1: Permission denied (publickey)."), true).contains("authorized_keys"));
        assert!(explain(&normalize(out(5, ""), true), false).contains("用户名或密码不对"));
        assert!(explain(&out(255, "ssh: connect to host 127.0.0.1 port 23001: Connection refused"), true).starts_with("经隧道连不上路由：ssh: connect"));
        assert_eq!(normalize(out(5, ""), false).code, 5, "没用密码时不折算");
    }

    #[test]
    fn login_debug_hides_password() {
        let l = Login { host: "10.0.0.1".into(), port: 22, user: "root".into(), password: Some("hunter2".into()) };
        assert_eq!(format!("{l:?}"), "root@10.0.0.1:22（密码）");
    }
}
