//! 探测点模式：`fleet-agent probe -c /etc/fleet/probe.toml`。
//!
//! 放在靠近客户的地方（如香港）的一台小机器，替中控测线路机的客户入口通不通（为什么见 DESIGN.md「探测点」，中控那边见 fleet-server 的 probe_points/）。
//! 每 30 秒一轮（间隔由中控给）：
//! 1. `GET /agent/v1/probe/targets` 取目标：各线路机的 public_host + 在用入口端口、对照目标；
//! 2. 每台线路机连它第 `turn % 端口数` 个端口（每轮换下一个，同中控的 reach.rs），同时连对照目标；只建连，记通不通与内核量的往返时间
//!    （同线路机 → 节点的 TCP 探测，`probe::tcp_probe`，超时 10 秒）；
//! 3. 对照目标都连不上、或三台以上线路机全部连不上：是探测点自己的网络有问题，这一轮作废（`fleet_proto::keep_round`），只报心跳；
//! 4. `POST /agent/v1/probe/report` 上报。发不出去就丢掉（不排队补发：迟到的结果没有用，下一轮照常）。
//!
//! 中控要它升级时（`ProbeTargets::upgrade`）：下载 `/agent/v1/probe/binary`，sha256 对上、`--version` 跑得起来才换，
//! 旧的留成同目录的 `fleet-agent.prev`，然后退出（退出码 0），由服务管理器（supervise-daemon / systemd）拉起新的。
//!
//! 省内存：不缓冲、不落盘，一轮的结果用完就丢；程序本身的 sha256 启动时边读边算。常驻约几 MB。
//! 日志只在出问题时写，同一个问题 10 分钟最多一条（探测点的磁盘小，OpenRC 下日志是一个不轮转的文件）。

use crate::probe::{tcp_probe, TCP_TIMEOUT};
use anyhow::{bail, Context, Result};
use fleet_proto::{keep_round, ProbeReport, ProbeResult, ProbeTargets, ProbeUpgrade};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const DEFAULT_CONFIG: &str = "/etc/fleet/probe.toml";
/// 中控没给间隔、取目标失败时的间隔
const DEFAULT_INTERVAL: Duration = Duration::from_secs(30);
/// 同一个问题的日志最多这么久一条
const LOG_EVERY: Duration = Duration::from_secs(600);

/// `/etc/fleet/probe.toml`（中控安装时写，0600）
#[derive(Debug, Clone, Deserialize)]
pub struct ProbeConfig {
    pub server_url: String,
    /// 上报令牌（探测点自己的，与机器的 agent 令牌是两套）
    pub token: String,
    /// 只用于日志
    #[serde(default)]
    pub name: String,
}

impl ProbeConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(path).with_context(|| format!("读取配置 {}", path.display()))?;
        let c: Self = toml::from_str(&s).with_context(|| format!("解析配置 {}", path.display()))?;
        anyhow::ensure!(!c.server_url.trim().is_empty() && !c.token.trim().is_empty(), "配置 {} 缺 server_url 或 token", path.display());
        Ok(c)
    }
}

/// 一轮的问题
#[derive(Debug)]
enum RoundErr {
    /// 令牌无效 / 被撤销（探测点被删除或重装过）
    Revoked,
    Other(String),
}

impl<E: std::fmt::Display> From<E> for RoundErr {
    fn from(e: E) -> Self { RoundErr::Other(e.to_string()) }
}

/// 同一个问题 10 分钟最多一条日志；好了记一条「恢复」
#[derive(Default)]
struct Throttle {
    last: Option<(String, Instant)>,
}

impl Throttle {
    fn warn(&mut self, msg: &str) {
        if self.last.as_ref().is_none_or(|(m, t)| m != msg || t.elapsed() >= LOG_EVERY) {
            tracing::warn!("{msg}");
            self.last = Some((msg.to_string(), Instant::now()));
        }
    }
    fn ok(&mut self) {
        if self.last.take().is_some() {
            tracing::info!("恢复正常");
        }
    }
}

struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Client {
    fn new(cfg: &ProbeConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(format!("fleet-agent/{} (probe)", fleet_proto::build_version()))
            .build()?;
        Ok(Self { http, base: cfg.server_url.trim().trim_end_matches('/').to_string(), token: cfg.token.trim().to_string() })
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response, RoundErr> {
        let r = req.bearer_auth(&self.token).send().await.map_err(|e| RoundErr::Other(format!("连不上中控：{}", crate::util::error_chain(&e).0)))?;
        match r.status().as_u16() {
            200..=299 => Ok(r),
            401 => Err(RoundErr::Revoked),
            s => Err(RoundErr::Other(format!("中控回 HTTP {s}：{}", r.text().await.unwrap_or_default().chars().take(200).collect::<String>()))),
        }
    }

    async fn targets(&self) -> Result<ProbeTargets, RoundErr> {
        let r = self.send(self.http.get(format!("{}/agent/v1/probe/targets", self.base)).timeout(Duration::from_secs(15))).await?;
        r.json().await.map_err(|e| RoundErr::Other(format!("目标解析失败：{e}")))
    }

    async fn report(&self, rep: &ProbeReport) -> Result<(), RoundErr> {
        match self.send(self.http.post(format!("{}/agent/v1/probe/report", self.base)).timeout(Duration::from_secs(15)).json(rep)).await {
            // 429：离上一份太近（刚重启），这一份不要了
            Err(RoundErr::Other(m)) if m.contains("HTTP 429") => Ok(()),
            r => r.map(drop),
        }
    }
}

/// 常驻：每轮取目标、测、上报，直到收到 SIGTERM / SIGINT，或升级换好了程序（退出让服务管理器拉起新的）
pub async fn run(path: &Path) -> Result<()> {
    let cfg = ProbeConfig::load(path)?;
    anyhow::ensure!(
        !crate::server::plain_http_over_public(&cfg.server_url),
        "中控地址 {} 是明文 http:// 且不在内网/回环，拒绝连接：请改用 https://",
        cfg.server_url
    );
    let c = Client::new(&cfg)?;
    let own = tokio::task::spawn_blocking(exe_sha256).await?.ok();
    tracing::info!(name = %cfg.name, version = fleet_proto::build_version(), server = %c.base, "fleet-agent 探测点启动");
    let (stop_tx, mut stop) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        crate::wait_for_signal().await;
        let _ = stop_tx.send(true);
    });
    let mut log = Throttle::default();
    let mut turn = 0usize;
    loop {
        let start = tokio::time::Instant::now();
        let wait = tokio::select! {
            _ = stop.changed() => break,
            r = round(&c, turn, own.as_deref()) => match r {
                Ok(Next::Sleep(d, discarded)) => {
                    match discarded {
                        Some(why) => log.warn(&format!("这一轮作废：{why}（探测点自己的网络像是有问题）")),
                        None => log.ok(),
                    }
                    d
                }
                Ok(Next::Upgraded(v)) => {
                    tracing::info!("已换成 {v}，退出让服务管理器拉起新版本");
                    return Ok(());
                }
                Err(RoundErr::Revoked) => {
                    log.warn("中控说令牌无效（探测点被删除或重装过）：10 分钟后再试");
                    crate::server::REVOKED_SLEEP
                }
                Err(RoundErr::Other(m)) => {
                    log.warn(&m);
                    DEFAULT_INTERVAL
                }
            }
        };
        turn = turn.wrapping_add(1);
        tokio::select! {
            _ = stop.changed() => break,
            _ = tokio::time::sleep_until(start + wait) => {}
        }
    }
    tracing::info!("收到退出信号，退出");
    Ok(())
}

enum Next {
    /// 这一轮做完，下一轮从这一轮开始算起等这么久；Some = 这一轮作废的原因
    Sleep(Duration, Option<String>),
    /// 换好了程序（新版本号）
    Upgraded(String),
}

async fn round(c: &Client, turn: usize, own: Option<&str>) -> Result<Next, RoundErr> {
    let t = c.targets().await?;
    if let Some(u) = t.upgrade.as_ref().filter(|u| own != Some(u.sha256.as_str())) {
        upgrade(c, u).await.map_err(|e| RoundErr::Other(format!("升级失败（照旧用现在的版本）：{e:#}")))?;
        return Ok(Next::Upgraded(u.version.clone()));
    }
    let (results, discarded) = measure(&t, turn).await;
    let rep = ProbeReport { version: fleet_proto::build_version().into(), sha256: own.map(String::from), discarded: discarded.clone(), results };
    c.report(&rep).await?;
    Ok(Next::Sleep(Duration::from_secs(u64::from(t.interval_s.clamp(10, 300))), discarded))
}

/// 测一轮：每台线路机连它第 `turn % 端口数` 个端口，同时连对照目标。返回 (结果, 作废原因)；作废时结果为空
pub async fn measure(t: &ProbeTargets, turn: usize) -> (Vec<ProbeResult>, Option<String>) {
    let mut set = tokio::task::JoinSet::new();
    for (i, r) in t.relays.iter().enumerate().filter(|(_, r)| !r.ports.is_empty()) {
        let (id, host, port) = (r.relay_id, r.host.clone(), r.ports[turn % r.ports.len()]);
        set.spawn(async move { (i, port, tcp_probe(id, &host, port, TCP_TIMEOUT, 0).await) });
    }
    let mut ctl = tokio::task::JoinSet::new();
    for a in &t.controls {
        let a = a.clone();
        ctl.spawn(async move {
            let Some((h, p)) = a.rsplit_once(':').and_then(|(h, p)| Some((h.to_string(), p.parse::<u16>().ok()?))) else { return false };
            tcp_probe(uuid::Uuid::nil(), &h, p, TCP_TIMEOUT, 0).await.ok
        });
    }
    let mut res = Vec::with_capacity(t.relays.len());
    while let Some(Ok(x)) = set.join_next().await {
        res.push(x);
    }
    res.sort_by_key(|x| x.0);
    let mut control_ok = t.controls.is_empty();
    while let Some(r) = ctl.join_next().await {
        control_ok |= r.unwrap_or(false);
    }
    let oks: Vec<bool> = res.iter().map(|x| x.2.ok).collect();
    if !keep_round(&oks, control_ok) {
        let why = if control_ok { format!("{} 台线路机全部连不上", oks.len()) } else { format!("对照目标（{}）都连不上", t.controls.join("、")) };
        return (vec![], Some(why));
    }
    let out = res
        .into_iter()
        .map(|(i, port, s)| ProbeResult { relay_id: t.relays[i].relay_id, port, ok: s.ok, rtt_ms: s.latency_ms.filter(|_| s.ok), detail: s.detail })
        .collect();
    (out, None)
}

/// 正在跑的程序的 sha256（边读边算，不整份读进内存）
fn exe_sha256() -> Result<String> {
    use sha2::Digest;
    let mut f = std::fs::File::open(std::env::current_exe()?)?;
    let mut h = sha2::Sha256::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(hex::encode(h.finalize()))
}

/// 下载、核对、试跑、替换。旧的留成 `<程序>.prev`
async fn upgrade(c: &Client, u: &ProbeUpgrade) -> Result<()> {
    use sha2::Digest;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::AsyncWriteExt;
    let exe = std::env::current_exe()?;
    let dir = exe.parent().context("程序所在目录")?.to_path_buf();
    let new = dir.join(".fleet-agent.new");
    let mut r = match c.send(c.http.get(format!("{}/agent/v1/probe/binary", c.base)).timeout(Duration::from_secs(300))).await {
        Ok(r) => r,
        Err(RoundErr::Revoked) => bail!("令牌无效"),
        Err(RoundErr::Other(m)) => bail!(m),
    };
    let mut f = tokio::fs::File::create(&new).await.with_context(|| format!("写 {}", new.display()))?;
    let mut h = sha2::Sha256::new();
    while let Some(chunk) = r.chunk().await.context("下载中断")? {
        h.update(&chunk);
        f.write_all(&chunk).await?;
    }
    f.sync_all().await?;
    drop(f);
    let got = hex::encode(h.finalize());
    let cleanup = |e: anyhow::Error| {
        let _ = std::fs::remove_file(&new);
        e
    };
    if got != u.sha256 {
        return Err(cleanup(anyhow::anyhow!("下载的程序 sha256 是 {got}，中控说的是 {}", u.sha256)));
    }
    std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755)).map_err(|e| cleanup(e.into()))?;
    let ok = tokio::process::Command::new(&new).arg("--version").output().await.map(|o| o.status.success()).unwrap_or(false);
    if !ok {
        return Err(cleanup(anyhow::anyhow!("下载的程序跑不起来（--version 失败）")));
    }
    swap(&exe, &new).map_err(cleanup)
}

/// 旧的改名成 `.prev`，新的换上；第二步失败时换回去
fn swap(exe: &Path, new: &Path) -> Result<()> {
    let prev: PathBuf = exe.with_file_name(format!("{}.prev", exe.file_name().and_then(|n| n.to_str()).unwrap_or("fleet-agent")));
    std::fs::rename(exe, &prev).with_context(|| format!("把 {} 改名成 {}", exe.display(), prev.display()))?;
    if let Err(e) = std::fs::rename(new, exe) {
        let _ = std::fs::rename(&prev, exe);
        bail!("换上新程序失败：{e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_proto::ProbeTarget;
    use uuid::Uuid;

    fn target(host: &str, ports: Vec<u16>) -> ProbeTarget { ProbeTarget { relay_id: Uuid::now_v7(), host: host.into(), ports } }

    #[test]
    fn config_needs_url_and_token() {
        let d = std::env::temp_dir().join(format!("fa-probe-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("probe.toml");
        std::fs::write(&p, "server_url = \"https://fleet.test\"\ntoken = \"abc\"\nname = \"香港\"\n").unwrap();
        let c = ProbeConfig::load(&p).unwrap();
        assert_eq!((c.server_url.as_str(), c.token.as_str(), c.name.as_str()), ("https://fleet.test", "abc", "香港"));
        std::fs::write(&p, "server_url = \"https://fleet.test\"\ntoken = \"\"\n").unwrap();
        assert!(ProbeConfig::load(&p).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 轮流连端口、记往返时间；对照目标都不通整轮作废；三台以上全不通整轮作废
    #[tokio::test]
    async fn measure_rotates_ports_and_discards() {
        let open = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let op = open.local_addr().unwrap().port();
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let ctl = format!("127.0.0.1:{op}");
        let t = ProbeTargets { interval_s: 30, controls: vec![format!("127.0.0.1:{closed}"), ctl.clone()], relays: vec![target("127.0.0.1", vec![op, closed])], upgrade: None };
        let (r, why) = measure(&t, 0).await;
        assert!(why.is_none());
        assert_eq!((r.len(), r[0].port, r[0].ok), (1, op, true));
        assert!(r[0].rtt_ms.is_some() && r[0].detail.is_none(), "{r:?}");
        let (r, _) = measure(&t, 1).await;
        assert_eq!((r[0].port, r[0].ok, r[0].detail.as_deref(), r[0].rtt_ms), (closed, false, Some("连接被拒绝"), None));
        // 对照目标都不通
        let bad = ProbeTargets { controls: vec![format!("127.0.0.1:{closed}")], ..t.clone() };
        let (r, why) = measure(&bad, 0).await;
        assert!(r.is_empty() && why.unwrap().contains("对照目标"));
        // 三台全不通
        let three = ProbeTargets { controls: vec![ctl], relays: (0..3).map(|_| target("127.0.0.1", vec![closed])).collect(), ..t.clone() };
        let (r, why) = measure(&three, 0).await;
        assert!(r.is_empty() && why.unwrap().contains("3 台线路机全部连不上"));
    }

    #[test]
    fn swap_keeps_prev() {
        let d = std::env::temp_dir().join(format!("fa-swap-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        let (exe, new) = (d.join("fleet-agent"), d.join(".fleet-agent.new"));
        std::fs::write(&exe, "old").unwrap();
        std::fs::write(&new, "new").unwrap();
        swap(&exe, &new).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(d.join("fleet-agent.prev")).unwrap(), "old");
        assert!(!new.exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn throttle_once_per_problem() {
        let mut t = Throttle::default();
        t.warn("a");
        let first = t.last.as_ref().unwrap().1;
        t.warn("a");
        assert_eq!(t.last.as_ref().unwrap().1, first, "同一个问题不重复记");
        t.warn("b");
        assert_eq!(t.last.as_ref().unwrap().0, "b");
        t.ok();
        assert!(t.last.is_none());
    }
}
