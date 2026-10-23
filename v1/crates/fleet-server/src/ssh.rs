//! 用系统 ssh 登录机器执行脚本（脚本经 stdin 传入，不出现在命令行）。
//!
//! 主机公钥：上架预检时 keyscan 一次并写入中控自己的 known_hosts（TOFU），之后一律 StrictHostKeyChecking=yes。
//! 已有记录的主机指纹变化时预检失败（除非作业明确允许重置）。known_hosts 是共用文件，读写由全局锁串行化。
//! 退役、彻底删除时把记录从 known_hosts 删掉（`forget_host`）；退役不可恢复，同一 IP 再上架按新机器重新首次信任。
//! 连接类的瞬时错误（超时、拒绝、被重置…）在 `run` / `push` 里自动重试 3 次；用户命令走 `run_once`，不重试。

use anyhow::{bail, Context};
use base64::Engine;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Mutex;

/// 步骤失败的类别（写进 job_step.error_kind，前端据此提示）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrKind {
    Network,
    Auth,
    HostKey,
    Remote,
    Timeout,
    Internal,
}

impl ErrKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrKind::Network => "network",
            ErrKind::Auth => "auth",
            ErrKind::HostKey => "host_key",
            ErrKind::Remote => "remote",
            ErrKind::Timeout => "timeout",
            ErrKind::Internal => "internal",
        }
    }
}

/// 带类别的步骤错误；包进 anyhow 后用 `kind_of` 取回类别
#[derive(Debug)]
pub struct StepError {
    pub kind: ErrKind,
    pub msg: String,
}

impl std::fmt::Display for StepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.msg) }
}

impl std::error::Error for StepError {}

impl StepError {
    pub fn err(kind: ErrKind, msg: impl Into<String>) -> anyhow::Error { anyhow::Error::new(StepError { kind, msg: msg.into() }) }
}

/// 错误的类别；没标过的一律算中控自身（internal）
pub fn kind_of(e: &anyhow::Error) -> ErrKind { e.downcast_ref::<StepError>().map(|s| s.kind).unwrap_or(ErrKind::Internal) }

/// 瞬时网络错误的重试间隔（共重试 3 次）
const RETRY_BACKOFF: [Duration; 3] = [Duration::from_secs(2), Duration::from_secs(5), Duration::from_secs(10)];

/// known_hosts 是所有 worker 共用的一个文件：读-改-写必须串行
static KNOWN_HOSTS_LOCK: Mutex<()> = Mutex::const_new(());

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

/// 按 ssh 的错误输出判断类别（只对 ssh 自己失败，即退出码 255 的情形调用）
fn classify_ssh_stderr(stderr: &str) -> ErrKind {
    let s = stderr.to_lowercase();
    if s.contains("host key verification failed") || s.contains("remote host identification has changed") || s.contains("no matching host key") {
        ErrKind::HostKey
    } else if s.contains("permission denied") || s.contains("authentication failed") || s.contains("too many authentication failures") {
        ErrKind::Auth
    } else {
        // 连接超时 / 拒绝 / 无路由 / kex 被重置 / 解析失败 …
        ErrKind::Network
    }
}

impl Output {
    pub fn ok(&self) -> bool { self.code == 0 }
    /// 解析脚本里 `key=value` 形式的输出行
    pub fn kv(&self, k: &str) -> Option<&str> {
        self.stdout.lines().rev().find_map(|l| l.strip_prefix(k).and_then(|r| r.strip_prefix('=')))
    }
    /// ssh 本身失败（连不上、登录被拒、指纹不符）时的类别；脚本自己退出非 0 返回 None
    pub fn ssh_kind(&self) -> Option<ErrKind> { (self.code == 255).then(|| classify_ssh_stderr(&self.stderr)) }
    /// 把失败的输出变成带类别的错误：`what：stderr`
    pub fn fail(&self, what: &str) -> anyhow::Error {
        let kind = self.ssh_kind().unwrap_or(ErrKind::Remote);
        let detail = if self.stderr.trim().is_empty() { self.stdout.trim() } else { self.stderr.trim() };
        StepError::err(kind, format!("{what}：{detail}"))
    }
}

/// 单路输出最多保留 1 MiB（远程执行回给用户的上限是 256 KiB，作业脚本的输出只有几 KB）
const OUT_CAP: usize = 1024 * 1024;

/// 把子进程的一路输出读完：前 OUT_CAP 字节留下，之后继续读但丢掉，让子进程不会因为管道满而卡住。
/// 读缓冲放在堆上：跨 await 的数组会整个算进 future，每个跑 ssh 的 future（作业步骤、远程执行…）都跟着大 32 KB，
/// 调试构建里层层嵌套的轮询栈帧再各留一份，升级回滚这样深的路径会撑爆 2 MB 的线程栈
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut r: R) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut kept = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => return kept,
            Ok(n) => {
                let room = OUT_CAP.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
}

/// 跑一条已拼好的 ssh 命令：脚本经 stdin 交给它，超过 `timeout` 杀掉（软路由的采集与一键部署也走这里）。
/// 边读边限量：超过 OUT_CAP 的部分只排空不保存（`cat` 几 GB 的日志不会把中控撑爆）；多余的在调用方按各自上限再截
pub(crate) async fn exec(mut c: Command, script: &str, timeout: Duration) -> anyhow::Result<Output> {
    c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = c.spawn().context("启动 ssh")?;
    let mut stdin = child.stdin.take().unwrap();
    let body = script.to_string();
    tokio::spawn(async move {
        let _ = stdin.write_all(body.as_bytes()).await;
        let _ = stdin.shutdown().await;
    });
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let run = async {
        let (o, e, status) = tokio::join!(read_capped(stdout), read_capped(stderr), child.wait());
        (o, e, status)
    };
    let (stdout, stderr, status) = tokio::time::timeout(timeout, run)
        .await
        .map_err(|_| StepError::err(ErrKind::Timeout, format!("ssh 执行超过 {} 秒", timeout.as_secs())))?;
    Ok(Output {
        code: status?.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
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
            self.with_key()
        };
        self.common(&mut c);
        c.arg(format!("root@{}", self.ip));
        c
    }

    /// 只用中控密钥登录
    fn with_key(&self) -> Command {
        let mut c = Command::new("ssh");
        c.args(["-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes", "-i"]).arg(&self.key);
        c
    }

    /// 登录方式以外的选项（不含目标主机）
    fn common(&self, c: &mut Command) {
        // 不读 /etc/ssh/ssh_config 与 ~/.ssh/config（ProxyCommand、ForwardAgent、SendEnv、ControlMaster…都不能带进来）：
        // 要用的选项全部在这里给出
        c.args(["-F", "/dev/null"]);
        c.args(["-o", "StrictHostKeyChecking=yes", "-o", "UpdateHostKeys=no", "-o", "ConnectTimeout=10", "-o", "ServerAliveInterval=10", "-o", "ServerAliveCountMax=3"]);
        c.args(["-o", "LogLevel=ERROR"]);
        // 只认中控自己的 known_hosts，不读系统级的
        c.args(["-o", "GlobalKnownHostsFile=/dev/null"]);
        c.arg("-o").arg(format!("UserKnownHostsFile={}", self.known_hosts.display()));
        c.arg("-p").arg(self.port.to_string());
    }

    /// 网页终端的交互式登录（由调用方放进 PTY）：选项同上，只用中控密钥（`password` 不看）；`-tt` 要远端分配终端，不带命令即登录 shell。
    /// `-e none` 关掉转义字符：`~C` 能在会话里临时加端口转发（`-R` 让机器经中控访问中控本机与内网），`~^Z` 会把 ssh 挂起
    pub fn interactive(&self) -> Command {
        let mut c = self.with_key();
        self.common(&mut c);
        c.args(["-o", "ClearAllForwardings=yes", "-e", "none", "-tt"]);
        c.arg(format!("root@{}", self.ip));
        c
    }

    /// sshpass 自己的退出码（5 密码错、6 主机指纹）折算成 ssh 的 255 + 对应错误文字，统一分类
    fn normalize(&self, mut o: Output) -> Output {
        if self.password.is_some() {
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

    /// 执行 bash 脚本（经 stdin），只跑一次、不重试。用户的命令（远程执行、查日志）走这里：
    /// 退出码 255 不一定是没连上——命令自己 exit 255、执行中断开连接（reboot）也是 255，重试会把命令再跑一遍
    pub async fn run_once(&self, script: &str, timeout: Duration) -> anyhow::Result<Output> {
        let mut c = self.base();
        c.arg("bash -s");
        Ok(self.normalize(exec(c, script, timeout).await?))
    }

    /// 同 `run_once`，但用 sh 跑（POSIX sh 脚本，给没有 bash 的机器：Alpine 上的探测点，见 probe_points/install.rs）
    pub async fn run_sh(&self, script: &str, timeout: Duration) -> anyhow::Result<Output> {
        let mut c = self.base();
        c.arg("sh -s");
        Ok(self.normalize(exec(c, script, timeout).await?))
    }

    /// 执行 bash 脚本（经 stdin）。连接类的瞬时错误自动重试（2s、5s、10s 退避）；脚本自己失败、登录被拒不重试。
    /// 只用于重跑无害的脚本（作业步骤、预检）；用户命令用 `run_once`。
    pub async fn run(&self, script: &str, timeout: Duration) -> anyhow::Result<Output> {
        let mut attempt = 0;
        loop {
            let out = self.run_once(script, timeout).await?;
            if out.ssh_kind() == Some(ErrKind::Network) && attempt < RETRY_BACKOFF.len() {
                tracing::warn!(ip = %self.ip, attempt = attempt + 1, "ssh 连接失败，稍后重试：{}", out.stderr.trim());
                tokio::time::sleep(RETRY_BACKOFF[attempt]).await;
                attempt += 1;
                continue;
            }
            return Ok(out);
        }
    }

    /// 推文件：sha256 相同则跳过；否则写临时文件再原子替换。连接类的瞬时错误同样重试。
    /// 文件只读进内存一次，每次尝试交给写 stdin 的任务的只是引用计数（不再整份复制）
    pub async fn push(&self, local: &Path, remote: &str, mode: &str, timeout: Duration) -> anyhow::Result<bool> {
        let data = tokio::fs::read(local).await.with_context(|| format!("读取 {}", local.display()))?;
        self.push_as(data, remote, mode, false, timeout).await
    }

    /// 同 `push`（内容由调用方读好：升级 agent 要先核对推的是哪个程序），替换之前把旧文件留成同目录的 `<remote>.prev`
    /// （硬链接，不另占空间；上一次留的被覆盖）：升级 agent 失败时换回去。
    /// 远端已经是新文件时不动 .prev（推送中途断线、重试时前一次其实已经换上了），免得旧的被新的顶掉
    pub async fn push_keep_prev(&self, data: Vec<u8>, remote: &str, mode: &str, timeout: Duration) -> anyhow::Result<bool> {
        self.push_as(data, remote, mode, true, timeout).await
    }

    async fn push_as(&self, data: Vec<u8>, remote: &str, mode: &str, keep_prev: bool, timeout: Duration) -> anyhow::Result<bool> {
        let data: std::sync::Arc<[u8]> = data.into();
        let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&data));
        let chk = self.run(&format!("sha256sum '{remote}' 2>/dev/null | cut -d' ' -f1"), Duration::from_secs(20)).await?;
        if chk.stdout.trim() == sha {
            return Ok(false);
        }
        let prev = if keep_prev {
            format!("if [ -f '{remote}' ] && [ \"$(sha256sum '{remote}' | cut -d' ' -f1)\" != '{sha}' ]; then ln -f '{remote}' '{remote}.prev'; fi; ")
        } else {
            String::new()
        };
        let mut attempt = 0;
        loop {
            let mut c = self.base();
            c.arg(format!(
                "set -e; d=$(dirname '{remote}'); mkdir -p \"$d\"; t=\"$d/.push.$$\"; cat > \"$t\"; \
                 [ \"$(sha256sum \"$t\" | cut -d' ' -f1)\" = '{sha}' ] || {{ rm -f \"$t\"; echo SHA_MISMATCH >&2; exit 3; }}; \
                 chmod {mode} \"$t\"; {prev}mv -f \"$t\" '{remote}'"
            ));
            c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
            let mut child = c.spawn().context("启动 ssh")?;
            let mut stdin = child.stdin.take().unwrap();
            let body = data.clone();
            tokio::spawn(async move {
                let _ = stdin.write_all(&body).await;
                let _ = stdin.shutdown().await;
            });
            let out = tokio::time::timeout(timeout, child.wait_with_output())
                .await
                .map_err(|_| StepError::err(ErrKind::Timeout, format!("推送 {remote} 超过 {} 秒", timeout.as_secs())))??;
            if out.status.success() {
                return Ok(true);
            }
            let o = self.normalize(Output {
                code: out.status.code().unwrap_or(-1),
                stdout: String::new(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
            if o.ssh_kind() == Some(ErrKind::Network) && attempt < RETRY_BACKOFF.len() {
                tracing::warn!(ip = %self.ip, attempt = attempt + 1, "推送 {remote} 连接失败，稍后重试：{}", o.stderr.trim());
                tokio::time::sleep(RETRY_BACKOFF[attempt]).await;
                attempt += 1;
                continue;
            }
            return Err(o.fail(&format!("推送 {remote} 失败")));
        }
    }
}

/// 中控公钥全文（`ssh-ed25519 AAAA… 注释`）：配置 `ssh_key` 旁边的 `<ssh_key>.pub`（ssh-keygen 的命名）。读不到或是空的为 None
pub async fn public_key(key: &Path) -> Option<String> {
    let mut p = key.as_os_str().to_owned();
    p.push(".pub");
    tokio::fs::read_to_string(PathBuf::from(p)).await.ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// 「录入中控公钥」这条出路的说明（上架检查、作业预检报中控密钥登录不了时带上）：公钥全文放在最后，整段选中末尾复制即可；
/// 读不到公钥时说它在中控的哪个文件
pub async fn add_key_hint(key: &Path) -> String {
    const WHERE: &str = "在供应商面板（或机器上 root 的 ~/.ssh/authorized_keys）录入中控公钥";
    match public_key(key).await {
        Some(k) => format!("{WHERE}：{k}"),
        None => format!("{WHERE}（中控上的 {}.pub，现在读不到）", key.display()),
    }
}

/// 上架检查用中控密钥试登录一次、跑只读脚本 `script`（读 /etc/os-release），不重试；`timeout` 是整个 ssh 进程的时限。
/// 只用密钥（BatchMode，不会停下来问密码），不读 ssh 配置文件，同 `Target`。
///
/// 主机公钥不核对、不记录（StrictHostKeyChecking=no + UserKnownHostsFile=/dev/null），而不是写进中控的 known_hosts 或先 keyscan 进临时文件：
/// - 记录指纹是上架作业第一步的事（两次 keyscan 比对、指纹变了要明确允许重置才覆盖）；检查不落库、可以随便点，
///   在这里写进正式的 known_hosts，等于没经过那些核对就信任了这把公钥；
/// - 临时 known_hosts 里只放这一次 keyscan 拿到的公钥，和不核对是一回事（同样是首次信任），只是多一次连接；
/// - 被冒充时的代价小：只做公钥认证（签名绑定本次会话，冒充者拿去登录不了真机器），不发密码、不转发 agent，
///   脚本只读 /etc/os-release，结果只决定检查报不报「登录不了」与发行版警告。真正上架时作业照常按 known_hosts 严格核对
pub async fn key_login_probe(ip: &str, port: u16, key: &Path, script: &str, timeout: Duration) -> anyhow::Result<Output> {
    let mut c = Command::new("ssh");
    c.args(["-F", "/dev/null", "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes", "-o", "PreferredAuthentications=publickey", "-i"]).arg(key);
    c.args(["-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o", "GlobalKnownHostsFile=/dev/null", "-o", "UpdateHostKeys=no"]);
    c.args(["-o", "ConnectTimeout=5", "-o", "LogLevel=ERROR", "-o", "ClearAllForwardings=yes", "-p"]).arg(port.to_string());
    c.arg(format!("root@{ip}")).arg("bash -s");
    exec(c, script, timeout).await
}

/// 公钥（`ssh-ed25519 AAAA…`）的 SHA256 指纹，仅用于展示
pub fn fingerprint(key: &str) -> String {
    let b64 = key.split_whitespace().nth(1).unwrap_or(key);
    match base64::engine::general_purpose::STANDARD.decode(b64) {
        Ok(raw) => format!("SHA256:{}", base64::engine::general_purpose::STANDARD_NO_PAD.encode(<sha2::Sha256 as sha2::Digest>::digest(&raw))),
        Err(_) => "（无法解析）".into(),
    }
}

/// known_hosts 文本里这台主机（host_spec）的记录行
fn lines_of<'a>(text: &'a str, spec: &str) -> Vec<&'a str> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter(|l| {
            let p: Vec<&str> = l.split_whitespace().collect();
            p.len() >= 3 && p[0].split(',').any(|h| h == spec)
        })
        .collect()
}

/// known_hosts 里这台主机已记录的「类型 公钥」
async fn pinned_key(known_hosts: &Path, spec: &str) -> Option<String> {
    let text = tokio::fs::read_to_string(known_hosts).await.ok()?;
    lines_of(&text, spec).first().map(|l| {
        let p: Vec<&str> = l.split_whitespace().collect();
        format!("{} {}", p[1], p[2])
    })
}

/// keyscan 拿 ed25519 公钥（两次，必须一致）并写入 known_hosts。返回「类型 公钥」。
/// 该主机已有记录时：一致就什么都不改；指纹变了则报「主机指纹变化」，`allow_reset` 为真才覆盖。
pub async fn pin_host_key(known_hosts: &Path, ip: &str, port: u16, allow_reset: bool) -> anyhow::Result<String> {
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
        0 => return Err(StepError::err(ErrKind::Network, format!("拿不到 {ip}:{port} 的主机公钥（SSH 没开或被防火墙挡）"))),
        1 => {}
        _ => return Err(StepError::err(ErrKind::HostKey, format!("{ip}:{port} 两次返回了不同的主机公钥：可能 IP 冲突或被冒用，停止"))),
    }
    let spec = host_spec(ip, port);
    let _g = KNOWN_HOSTS_LOCK.lock().await;
    if let Some(old) = pinned_key(known_hosts, &spec).await {
        if old == seen[0] {
            return Ok(seen[0].clone());
        }
        if !allow_reset {
            bail!(StepError {
                kind: ErrKind::HostKey,
                msg: format!(
                    "主机指纹变化：{ip}:{port} 原来记录的是 {}，现在是 {}。确认是同一台机器重装过，再在网页作业的「重试」里勾选「重置主机指纹」，或 fleetctl retry <作业> --reset-host-key；否则可能是 IP 被冒用",
                    fingerprint(&old),
                    fingerprint(&seen[0])
                )
            });
        }
        forget_locked(known_hosts, ip, port).await?;
    }
    let line = format!("{spec} {}\n", seen[0]);
    let mut f = tokio::fs::OpenOptions::new().create(true).append(true).open(known_hosts).await?;
    f.write_all(line.as_bytes()).await?;
    f.flush().await?;
    Ok(seen[0].clone())
}

async fn forget_locked(known_hosts: &Path, ip: &str, port: u16) -> anyhow::Result<()> {
    if !known_hosts.exists() {
        return Ok(());
    }
    let _ = Command::new("ssh-keygen").arg("-f").arg(known_hosts).arg("-R").arg(host_spec(ip, port)).output().await?;
    Ok(())
}

/// 从 known_hosts 删除这台主机（退役、彻底删除时调用）
pub async fn forget_host(known_hosts: &Path, ip: &str, port: u16) -> anyhow::Result<()> {
    let _g = KNOWN_HOSTS_LOCK.lock().await;
    forget_locked(known_hosts, ip, port).await
}

/// TCP 可达性
pub async fn tcp_ok(ip: &str, port: u16, timeout: Duration) -> bool { tcp_probe(ip, port, timeout).await.is_ok() }

/// TCP 可达性，连不上时给原因（给人看）：连接被拒绝、`timeout` 内没有响应，或别的连接错误
pub async fn tcp_probe(ip: &str, port: u16, timeout: Duration) -> Result<(), String> { connect(ip, port, timeout).await.map(drop) }

async fn connect(ip: &str, port: u16, timeout: Duration) -> Result<tokio::net::TcpStream, String> {
    match tokio::time::timeout(timeout, tokio::net::TcpStream::connect((ip, port))).await {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => Err("连接被拒绝：端口没有在监听，或防火墙拒绝了".into()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!("{} 秒没有响应", timeout.as_secs())),
    }
}

/// 连上之后等 SSH 版本行的时长
const SSH_BANNER_WAIT: Duration = Duration::from_secs(5);

/// SSH 端口能用：TCP 连上（`timeout` 内），并且 5 秒内读到服务端的版本行（`SSH-` 开头；按 RFC 4253，之前可以有别的行）。
/// 只看 TCP 不够：中控出网经透明代理时，任何地址的 TCP 都「连得上」（代理先接住），不存在的机器也会被判成通。
/// 不通时给原因（给人看），接在「SSH ip:port」后面：「连不上（…）」「没有响应 SSH 握手」「回的不是 SSH 版本行」
pub async fn ssh_probe(ip: &str, port: u16, timeout: Duration) -> Result<(), String> {
    use tokio::io::AsyncReadExt;
    let mut s = connect(ip, port, timeout).await.map_err(|why| format!("连不上（{why}）"))?;
    let mut got = Vec::new();
    let read = async {
        let mut buf = [0u8; 256];
        // 只看前 1 KB：版本行之前的杂行不会多到这里
        while got.len() < 1024 {
            let n = s.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                return false;
            }
            got.extend_from_slice(&buf[..n]);
            if got.starts_with(b"SSH-") || got.windows(5).any(|w| w == b"\nSSH-") {
                return true;
            }
        }
        false
    };
    let r = tokio::time::timeout(SSH_BANNER_WAIT, read).await;
    match r {
        Ok(true) => Ok(()),
        // 说了话但不是 SSH（读满、断开或超时都算）
        _ if !got.is_empty() => Err("回的不是 SSH 版本行：端口上不是 SSH 服务".into()),
        Ok(false) => Err("没有响应 SSH 握手（连上后被对方断开）".into()),
        Err(_) => Err(format!("没有响应 SSH 握手（连上后 {} 秒没收到版本行）", SSH_BANNER_WAIT.as_secs())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify() {
        assert_eq!(classify_ssh_stderr("root@1.2.3.4: Permission denied (publickey)."), ErrKind::Auth);
        assert_eq!(classify_ssh_stderr("Host key verification failed."), ErrKind::HostKey);
        assert_eq!(classify_ssh_stderr("ssh: connect to host 1.2.3.4 port 22: Connection refused"), ErrKind::Network);
        assert_eq!(classify_ssh_stderr("kex_exchange_identification: read: Connection reset by peer"), ErrKind::Network);
    }

    #[test]
    fn kind_survives_context() {
        let e = StepError::err(ErrKind::Auth, "x").context("外层");
        assert_eq!(kind_of(&e), ErrKind::Auth);
        assert_eq!(kind_of(&anyhow::anyhow!("普通错误")), ErrKind::Internal);
    }

    #[test]
    fn spec_and_fingerprint() {
        assert_eq!(host_spec("1.2.3.4", 22), "1.2.3.4");
        assert_eq!(host_spec("1.2.3.4", 2222), "[1.2.3.4]:2222");
        assert!(fingerprint("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGWz").starts_with("SHA256:"));
    }

    /// SSH 探测：只接受连接不说话（透明代理接住了不存在的地址）判不通；先回杂行再回版本行判通；连接被拒、回的不是 SSH 判不通
    #[tokio::test]
    async fn ssh_probe_needs_banner() {
        use tokio::io::AsyncWriteExt;
        async fn serve(reply: &'static [u8]) -> u16 {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = l.local_addr().unwrap().port();
            tokio::spawn(async move {
                let (mut s, _) = l.accept().await.unwrap();
                s.write_all(reply).await.unwrap();
                // 连接留着，不主动断开
                tokio::time::sleep(Duration::from_secs(10)).await;
            });
            port
        }
        let t = Duration::from_secs(2);
        let silent = serve(b"").await;
        let e = ssh_probe("127.0.0.1", silent, t).await.unwrap_err();
        assert!(e.contains("没有响应 SSH 握手"), "{e}");
        assert!(tcp_probe("127.0.0.1", silent, t).await.is_ok(), "TCP 本身是通的");
        assert_eq!(ssh_probe("127.0.0.1", serve(b"SSH-2.0-OpenSSH_9.6\r\n").await, t).await, Ok(()));
        assert_eq!(ssh_probe("127.0.0.1", serve(b"hello\r\nSSH-2.0-x\r\n").await, t).await, Ok(()));
        let e = ssh_probe("127.0.0.1", serve(b"HTTP/1.1 400 Bad Request\r\n\r\n").await, t).await.unwrap_err();
        assert!(e.contains("不是 SSH"), "{e}");
        let (_held, closed) = crate::reach::refused_port();
        let e = ssh_probe("127.0.0.1", closed, t).await.unwrap_err();
        assert!(e.starts_with("连不上"), "{e}");
    }

    #[tokio::test]
    async fn pinned_key_matches_host_and_port() {
        let text = "# c\n[1.2.3.4]:2222 ssh-ed25519 AAAA1\n1.2.3.4 ssh-ed25519 AAAA2\n[1.2.3.40]:2222 ssh-ed25519 AAAA3\n";
        assert_eq!(lines_of(text, "[1.2.3.4]:2222"), ["[1.2.3.4]:2222 ssh-ed25519 AAAA1"]);
        let dir = std::env::temp_dir().join(format!("fleet-kh-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let kh = dir.join("known_hosts");
        assert_eq!(pinned_key(&kh, "1.2.3.4").await, None, "文件不存在");
        std::fs::write(&kh, text).unwrap();
        assert_eq!(pinned_key(&kh, "1.2.3.4").await.as_deref(), Some("ssh-ed25519 AAAA2"));
        assert_eq!(pinned_key(&kh, "[1.2.3.4]:2200").await, None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
