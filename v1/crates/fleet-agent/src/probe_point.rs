//! 探测点模式：`fleet-agent probe -c /etc/fleet/probe.toml`。
//!
//! 放在靠近客户的地方（如香港）的一台小机器，替中控测线路机的客户入口通不通（为什么见 DESIGN.md「探测点」，中控那边见 fleet-server 的 probe_points/）。
//! 每 30 秒一轮（间隔由中控给）：
//! 1. `GET /agent/v1/probe/targets` 取目标：各线路机的 public_host + 在用入口端口、对照目标；
//! 2. 每台线路机连它第 `turn % 端口数` 个端口（每轮换下一个，同中控的 reach.rs），同时连对照目标；只建连，记通不通与内核量的往返时间
//!    （同线路机 → 节点的 TCP 探测，`probe::tcp_probe`，超时 10 秒）；
//! 3. 对照目标都连不上：是探测点自己的网络有问题，这一轮作废，只报心跳（带作废原因）；
//!    对照目标通、三台以上线路机却全部连不上：照常带结果上报，标上 `all_down`（`fleet_proto::all_down`：多半是线路机那边一起出了问题，中控照记）；
//! 4. `POST /agent/v1/probe/report` 上报。发不出去就丢掉（不排队补发：迟到的结果没有用，下一轮照常）。
//!
//! 中控要它升级时（`ProbeTargets::upgrade`）：下载 `/agent/v1/probe/binary`，sha256 对上、`--version` 跑得起来才换，
//! 旧的留成同目录的 `fleet-agent.prev`，写下试运行标记（[`TRIAL`]），然后退出（退出码 0），由服务管理器（supervise-daemon / systemd）拉起新的。
//! 新的启动后看到标记就是在试运行：[`TRIAL_WINDOW`] 内有一次上报成功就删掉标记；到时候一次都没成功（起来了但连不上中控、
//! 读不了配置等启动就失败被反复拉起）就把 `.prev` 换回来、记下这个版本的 sha256（[`BAD`]，之后中控再要它升级到这个版本时不换），
//! 以非 0 退出，由服务管理器拉起旧版本。标记在启动的第一步（读配置之前）看，所以任何启动失败都算在试运行里；
//! 是否到时只在每轮结束和启动时看：一轮卡住不返回不会回滚（取目标、上报各有 15 秒超时，建连 10 秒，实际卡不住）。
//! 试运行期间不接受新的升级（`.prev` 留着已知能用的那个版本）。
//! 中控那边也防反复：下载过程序、过了试运行的时长还没报上新版本的 sha256，就算这次升级失败，不再下发（probe_points/agent.rs）。
//!
//! 省内存：不缓冲、不落盘，一轮的结果用完就丢；程序本身的 sha256 启动时边读边算。常驻约几 MB。
//! 日志只在出问题时写，同一个问题 10 分钟最多一条（探测点的磁盘小，OpenRC 下日志是一个不轮转的文件）。

use crate::probe::{tcp_probe, TCP_TIMEOUT};
use anyhow::{bail, Context, Result};
use fleet_proto::{all_down, ProbeReport, ProbeResult, ProbeTargets, ProbeUpgrade};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const DEFAULT_CONFIG: &str = "/etc/fleet/probe.toml";
/// 中控没给间隔、取目标失败时的间隔
const DEFAULT_INTERVAL: Duration = Duration::from_secs(30);
/// 同一个问题的日志最多这么久一条
const LOG_EVERY: Duration = Duration::from_secs(600);
/// 升级后试运行的标记（程序同目录）：换上新程序时写，新程序第一次上报成功时删
const TRIAL: &str = ".fleet-agent.trial";
/// 换回旧版本时记下的新程序 sha256（程序同目录）：中控再要升级到它时不换
const BAD: &str = ".fleet-agent.bad-sha256";
/// 升级后这么久一次上报都没成功就换回旧版本（每 30 秒一轮，10 轮；中控短暂不可用也不至于误判）
const TRIAL_WINDOW: Duration = Duration::from_secs(300);

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
pub async fn run(path: &Path) -> Result<()> { run_as(path, &std::env::current_exe()?).await }

/// `run`，程序路径由调用方给（测试用临时目录里的假程序）
async fn run_as(path: &Path, exe: &Path) -> Result<()> {
    // 升级后的试运行：最先看（读配置、建客户端之前），新版本读不了现有配置之类启动就失败的，也会在到时后换回。
    // 标记写了太久还在（之前几次启动都没能上报成功，或一启动就失败被反复拉起），直接换回旧版本
    let exe = exe.to_path_buf();
    let mut trial = Trial::load(&exe);
    if let Some(t) = trial.as_ref().filter(|t| t.expired()) {
        return t.roll_back(&exe);
    }
    let cfg = ProbeConfig::load(path)?;
    anyhow::ensure!(
        !crate::server::plain_http_over_public(&cfg.server_url),
        "中控地址 {} 是明文 http:// 且不在内网/回环，拒绝连接：请改用 https://",
        cfg.server_url
    );
    let c = Client::new(&cfg)?;
    let own = tokio::task::spawn_blocking({
        let exe = exe.clone();
        move || exe_sha256_of(&exe)
    })
    .await?
    .ok();
    tracing::info!(name = %cfg.name, version = fleet_proto::build_version(), server = %c.base, trial = trial.is_some(), "fleet-agent 探测点启动");
    let (stop_tx, mut stop) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        crate::wait_for_signal().await;
        let _ = stop_tx.send(true);
    });
    let mut log = Throttle::default();
    let mut turn = 0usize;
    loop {
        let start = tokio::time::Instant::now();
        let r = tokio::select! {
            _ = stop.changed() => break,
            r = round(&c, turn, own.as_deref(), &exe, trial.is_some()) => r,
        };
        // 试运行中：上报成功（Sleep）就算过了；到时候还没成功就换回旧版本
        if let Some(t) = trial.take() {
            if matches!(r, Ok(Next::Sleep(..))) {
                t.passed();
                tracing::info!("升级后首次上报成功");
            } else if t.expired() {
                return t.roll_back(&exe);
            } else {
                trial = Some(t);
            }
        }
        let wait = match r {
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

/// `in_trial`：自己还在升级后的试运行里（还没上报成功过）：这时不再升级，免得 `.prev` 被这个没验证过的版本覆盖，等上报成功之后的下一轮再换
async fn round(c: &Client, turn: usize, own: Option<&str>, exe: &Path, in_trial: bool) -> Result<Next, RoundErr> {
    let t = c.targets().await?;
    // 上次换成它之后没能上报、已经换回来了的版本（`BAD`）不再换：中控那边也会在试运行时长过后记成升级失败、不再下发
    if let Some(u) = t.upgrade.as_ref().filter(|u| !in_trial && wants(own, bad_sha(exe).as_deref(), &u.sha256)) {
        upgrade(c, u, exe).await.map_err(|e| RoundErr::Other(format!("升级失败（照旧用现在的版本）：{e:#}")))?;
        return Ok(Next::Upgraded(u.version.clone()));
    }
    let m = measure(&t, turn).await;
    let rep = ProbeReport { version: fleet_proto::build_version().into(), sha256: own.map(String::from), discarded: m.discarded.clone(), all_down: m.all_down, results: m.results };
    c.report(&rep).await?;
    Ok(Next::Sleep(Duration::from_secs(u64::from(t.interval_s.clamp(10, 300))), m.discarded))
}

/// 中控要换成 `target`：不是正在跑的这个、也不是换上过又换回来的那个（`bad`）才换
fn wants(own: Option<&str>, bad: Option<&str>, target: &str) -> bool { own != Some(target) && bad != Some(target) }

/// 一轮的结果
#[derive(Debug)]
pub struct Measured {
    /// 作废时为空
    pub results: Vec<ProbeResult>,
    /// 整轮作废的原因（对照目标都连不上）
    pub discarded: Option<String>,
    /// 对照目标通、三台以上线路机全部连不上（照记，见 `fleet_proto::all_down`）
    pub all_down: bool,
}

/// 测一轮：每台线路机连它第 `turn % 端口数` 个端口，同时连对照目标
pub async fn measure(t: &ProbeTargets, turn: usize) -> Measured {
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
    if !control_ok {
        return Measured { results: vec![], discarded: Some(format!("对照目标（{}）都连不上", t.controls.join("、"))), all_down: false };
    }
    let oks: Vec<bool> = res.iter().map(|x| x.2.ok).collect();
    let results = res
        .into_iter()
        .map(|(i, port, s)| ProbeResult { relay_id: t.relays[i].relay_id, port, ok: s.ok, rtt_ms: s.latency_ms.filter(|_| s.ok), detail: s.detail })
        .collect();
    Measured { results, discarded: None, all_down: all_down(&oks) }
}

/// 程序的 sha256（边读边算，不整份读进内存）
fn exe_sha256_of(path: &Path) -> Result<String> {
    use sha2::Digest;
    let mut f = std::fs::File::open(path)?;
    let mut h = sha2::Sha256::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(hex::encode(h.finalize()))
}

/// 下载、核对、试跑、替换。旧的留成 `<程序>.prev`
async fn upgrade(c: &Client, u: &ProbeUpgrade, exe: &Path) -> Result<()> {
    use sha2::Digest;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::AsyncWriteExt;
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
    swap(exe, &new).map_err(cleanup)
}

fn prev_of(exe: &Path) -> PathBuf { exe.with_file_name(format!("{}.prev", exe.file_name().and_then(|n| n.to_str()).unwrap_or("fleet-agent"))) }

/// 旧的改名成 `.prev`，新的换上，写下试运行标记（[`Trial`]）；换上失败时换回去
fn swap(exe: &Path, new: &Path) -> Result<()> {
    let prev = prev_of(exe);
    std::fs::rename(exe, &prev).with_context(|| format!("把 {} 改名成 {}", exe.display(), prev.display()))?;
    if let Err(e) = std::fs::rename(new, exe) {
        let _ = std::fs::rename(&prev, exe);
        bail!("换上新程序失败：{e}");
    }
    // 写不了标记（磁盘满）也照样用新的：只是少了运行期回滚
    if let Err(e) = std::fs::write(exe.with_file_name(TRIAL), b"") {
        tracing::warn!("写升级试运行标记失败（换上后起不来不会自动换回）：{e}");
    }
    Ok(())
}

/// 换回旧版本时记下的新程序 sha256
fn bad_sha(exe: &Path) -> Option<String> { std::fs::read_to_string(exe.with_file_name(BAD)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) }

/// 升级后的试运行：标记文件的修改时间就是换上新程序的时刻（跨多次重启都按它算）
struct Trial {
    path: PathBuf,
    since: std::time::SystemTime,
}

impl Trial {
    fn load(exe: &Path) -> Option<Self> {
        let path = exe.with_file_name(TRIAL);
        let since = std::fs::metadata(&path).ok()?.modified().ok()?;
        Some(Self { path, since })
    }

    fn expired(&self) -> bool { self.since.elapsed().is_ok_and(|d| d >= TRIAL_WINDOW) }

    fn passed(self) {
        let _ = std::fs::remove_file(&self.path);
    }

    /// 换回 `.prev`，记下这个版本的 sha256，删掉标记，返回错误（非 0 退出，服务管理器拉起旧版本）。没有 `.prev` 就只删标记、照旧跑
    fn roll_back(&self, exe: &Path) -> Result<()> {
        let prev = prev_of(exe);
        let _ = std::fs::remove_file(&self.path);
        if !prev.exists() {
            tracing::warn!("升级后 {} 秒内没能上报，但没有 {} 可换回：照旧用现在的版本", TRIAL_WINDOW.as_secs(), prev.display());
            return Ok(());
        }
        if let Ok(sha) = exe_sha256_of(exe) {
            let _ = std::fs::write(exe.with_file_name(BAD), sha);
        }
        std::fs::rename(&prev, exe).with_context(|| format!("把 {} 换回 {}", prev.display(), exe.display()))?;
        bail!("升级后 {} 秒内一次都没能上报成功：已换回旧版本，退出让服务管理器拉起它", TRIAL_WINDOW.as_secs())
    }
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

    /// 轮流连端口、记往返时间；对照目标都不通整轮作废；三台以上全不通照报、标上 all_down
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
        let m = measure(&t, 0).await;
        let r = &m.results;
        assert!(m.discarded.is_none() && !m.all_down);
        assert_eq!((r.len(), r[0].port, r[0].ok), (1, op, true));
        assert!(r[0].rtt_ms.is_some() && r[0].detail.is_none(), "{r:?}");
        let r = measure(&t, 1).await.results;
        assert_eq!((r[0].port, r[0].ok, r[0].detail.as_deref(), r[0].rtt_ms), (closed, false, Some("连接被拒绝"), None));
        // 对照目标都不通
        let bad = ProbeTargets { controls: vec![format!("127.0.0.1:{closed}")], ..t.clone() };
        let m = measure(&bad, 0).await;
        assert!(m.results.is_empty() && m.discarded.unwrap().contains("对照目标") && !m.all_down);
        // 三台全不通、对照目标通：照报，标上 all_down
        let three = ProbeTargets { controls: vec![ctl], relays: (0..3).map(|_| target("127.0.0.1", vec![closed])).collect(), ..t.clone() };
        let m = measure(&three, 0).await;
        assert!(m.discarded.is_none() && m.all_down, "{m:?}");
        assert_eq!(m.results.iter().filter(|r| !r.ok).count(), 3);
    }

    /// 升级后试运行：换上时写标记；到时候一次都没上报成功就换回 .prev、记下坏版本的 sha256、删掉标记；成功了只删标记
    #[test]
    fn trial_rolls_back_when_no_report() {
        let d = std::env::temp_dir().join(format!("fa-trial-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        let (exe, new) = (d.join("fleet-agent"), d.join(".fleet-agent.new"));
        std::fs::write(&exe, "old").unwrap();
        std::fs::write(&new, "new").unwrap();
        swap(&exe, &new).unwrap();
        let t = Trial::load(&exe).expect("换上新程序时写了标记");
        assert!(!t.expired());
        // 标记写了超过试运行时长：换回旧的
        std::fs::File::options().write(true).open(d.join(TRIAL)).unwrap().set_modified(std::time::SystemTime::now() - TRIAL_WINDOW).unwrap();
        let t = Trial::load(&exe).unwrap();
        assert!(t.expired());
        let e = t.roll_back(&exe).unwrap_err();
        assert!(e.to_string().contains("已换回旧版本"), "{e}");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
        assert!(!d.join("fleet-agent.prev").exists() && Trial::load(&exe).is_none());
        assert_eq!(bad_sha(&exe).as_deref(), Some(hex::encode(<sha2::Sha256 as sha2::Digest>::digest(b"new")).as_str()));
        // 再升级、这次上报成功：只删标记，新程序留着
        std::fs::write(&new, "new2").unwrap();
        swap(&exe, &new).unwrap();
        Trial::load(&exe).unwrap().passed();
        assert!(Trial::load(&exe).is_none());
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new2");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 试运行在读配置之前看：新版本读不了配置（启动就失败）时，到时照样换回旧版本；没在试运行的照常报配置错误
    #[tokio::test]
    async fn trial_checked_before_config() {
        let d = std::env::temp_dir().join(format!("fa-trial-cfg-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        let (exe, new) = (d.join("fleet-agent"), d.join(".fleet-agent.new"));
        let missing = d.join("probe.toml");
        std::fs::write(&exe, "old").unwrap();
        let e = run_as(&missing, &exe).await.unwrap_err();
        assert!(format!("{e:#}").contains("读取配置"), "{e:#}");
        std::fs::write(&new, "new").unwrap();
        swap(&exe, &new).unwrap();
        // 还没到时：照常因为配置失败退出（服务管理器再拉起），不换回
        assert!(format!("{:#}", run_as(&missing, &exe).await.unwrap_err()).contains("读取配置"));
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
        std::fs::File::options().write(true).open(d.join(TRIAL)).unwrap().set_modified(std::time::SystemTime::now() - TRIAL_WINDOW).unwrap();
        let e = run_as(&missing, &exe).await.unwrap_err();
        assert!(e.to_string().contains("已换回旧版本"), "{e:#}");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 要不要换成中控给的版本：正在跑的、换回来过的（BAD）都不换
    #[test]
    fn upgrade_target_filter() {
        assert!(wants(Some("a"), None, "b"));
        assert!(wants(None, Some("c"), "b"));
        assert!(!wants(Some("b"), None, "b"));
        assert!(!wants(Some("a"), Some("b"), "b"));
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
