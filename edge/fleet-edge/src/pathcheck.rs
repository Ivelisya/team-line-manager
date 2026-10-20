//! 原生全链路检查（check = "path"），取代旧 team-ops-path-probe（tests/relay_e2e.live_paths）。
//!
//! 每个周期（缺省 60 s，与旧定时器相同）：
//! 1. 读 Xray 配置（只读），取所有 `vless` + `reality` 入站（与旧实现同一筛选），每个入站取第一个用户的 id/flow、
//!    `serverNames[0]`、`shortIds[0]`，由 `privateKey` 算出公钥（x25519）。
//! 2. 生成一份临时客户端配置：每个入站一个本机 SOCKS 入站 → VLESS+REALITY 出站连 `127.0.0.1:<入口端口>`（fingerprint chrome）；
//!    用线上同一个 xray 可执行文件跑起来。**不改受管 Xray 配置**；来源恒为 127.0.0.1，访问日志里按回环记，不算客户活动。
//! 3. 每个入口最多 2 次尝试（每次 ≤ 18 s、整个周期 ≤ 140 s、并发 3），经 SOCKS 访问 `https://www.cloudflare.com/cdn-cgi/trace`，
//!    rc / phase 与旧 curl 口径一致；healthy = rc 0 且拿到 ip 且（有期望 IP 时）等于期望 IP。
//! 4. 结果写进 probe.db（fleet.probe.v1，check = path，细节在 `path` 字段），随批次上报。
//!
//! 客户端进程的回收：relayd 的 seccomp 禁止 kill 系列系统调用，所以客户端在一个短命线程里 fork，带 `PR_SET_PDEATHSIG(SIGKILL)`；
//! 周期结束该线程退出，内核向客户端发 SIGKILL（不经 kill 系统调用），再由探测线程 wait 回收。
use crate::{info, warn};
use base64::Engine;
use fleet_probe::http::{self, Phase, Transport};
use fleet_probe::targets;
use fleet_probe::types::{CheckKind, PathAttempt, PathDetail, ProbeResult, Status};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const HOST: &str = "www.cloudflare.com";
pub const URL_PATH: &str = "/cdn-cgi/trace";

#[derive(Clone, Debug)]
pub struct Entry {
    pub tag: String,
    pub port: u16,
    pub uuid: String,
    pub flow: String,
    pub sni: String,
    pub sid: String,
    pub pbk: String,
    pub network: String,
    /// 用于结果行的节点 id（旧资产 uuid，按出口 tag 从身份映射；没有映射时用入站 tag）
    pub node: String,
    pub expected_ip: Option<String>,
    /// 路由出口 tag 与其协议（freedom = 主节点入口，经本机直连出网）
    pub out_tag: String,
    pub out_protocol: String,
}

pub fn x25519_public(private_b64: &str) -> Option<String> {
    let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let raw = e.decode(private_b64.trim_end_matches('=')).ok()?;
    let k: [u8; 32] = raw.try_into().ok()?;
    let pubk = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(k));
    Some(e.encode(pubk.as_bytes()))
}

/// 从 Xray 配置取待测入口（vless + reality，与旧 live_paths 相同的筛选）
pub fn entries(cfg: &Value, ids: &BTreeMap<String, targets::Identity>) -> Vec<Entry> {
    let rules = cfg["routing"]["rules"].as_array().cloned().unwrap_or_default();
    let mut out = vec![];
    for i in cfg["inbounds"].as_array().into_iter().flatten() {
        let ss = &i["streamSettings"];
        if i["protocol"] != "vless" || ss["security"] != "reality" {
            continue;
        }
        let (Some(tag), Some(port)) = (i["tag"].as_str(), i["port"].as_u64()) else { continue };
        let rs = &ss["realitySettings"];
        let Some(user) = i["settings"]["clients"].as_array().and_then(|c| c.first()) else { continue };
        let Some(pbk) = rs["privateKey"].as_str().and_then(x25519_public) else { continue };
        // 出口：只按 inboundTag 匹配的第一条规则的 outboundTag
        let out_tag = rules
            .iter()
            .find(|r| r["inboundTag"].as_array().is_some_and(|a| a.iter().any(|t| t == tag)) && r.get("domain").is_none() && r.get("ip").is_none())
            .and_then(|r| r["outboundTag"].as_str())
            .unwrap_or("");
        let out_protocol = cfg["outbounds"].as_array().into_iter().flatten().find(|o| o["tag"] == out_tag).and_then(|o| o["protocol"].as_str()).unwrap_or("").to_string();
        let id = ids.get(out_tag);
        let expected = id.map(|x| x.expected_ip.clone()).filter(|s| !s.is_empty()).or_else(|| targets::ip_from_tag(out_tag));
        out.push(Entry {
            tag: tag.to_string(),
            port: port as u16,
            uuid: user["id"].as_str().unwrap_or("").to_string(),
            flow: user["flow"].as_str().unwrap_or("").to_string(),
            sni: rs["serverNames"][0].as_str().unwrap_or("").to_string(),
            sid: rs["shortIds"][0].as_str().unwrap_or("").to_string(),
            pbk,
            network: ss["network"].as_str().unwrap_or("tcp").to_string(),
            node: id.map(|x| x.node.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| tag.to_string()),
            expected_ip: expected,
            out_tag: out_tag.to_string(),
            out_protocol,
        });
    }
    out
}

fn free_port() -> Option<u16> {
    std::net::TcpListener::bind("127.0.0.1:0").ok()?.local_addr().ok().map(|a| a.port())
}

pub fn client_config(es: &[Entry], ports: &[u16]) -> Value {
    let mut ins = vec![];
    let mut outs = vec![];
    let mut rules = vec![];
    for (n, (e, p)) in es.iter().zip(ports).enumerate() {
        ins.push(json!({"tag": format!("pc-in-{n}"), "listen": "127.0.0.1", "port": p, "protocol": "socks", "settings": {"auth": "noauth", "udp": false}}));
        outs.push(json!({"tag": format!("pc-out-{n}"), "protocol": "vless",
            "settings": {"vnext": [{"address": "127.0.0.1", "port": e.port, "users": [{"id": e.uuid, "encryption": "none", "flow": e.flow}]}]},
            "streamSettings": {"network": e.network, "security": "reality",
                "realitySettings": {"fingerprint": "chrome", "serverName": e.sni, "publicKey": e.pbk, "shortId": e.sid}}}));
        rules.push(json!({"type": "field", "inboundTag": [format!("pc-in-{n}")], "outboundTag": format!("pc-out-{n}")}));
    }
    json!({"log": {"loglevel": "none", "access": "none", "error": "none"}, "inbounds": ins, "outbounds": outs, "routing": {"rules": rules}})
}

/// 一次尝试：SOCKS5 CONNECT → TLS → GET trace。返回 (rc, phase, actual_ip, ms, 分段耗时)
async fn attempt(socks: u16, limit: Duration) -> (i32, Option<String>, Option<String>, f64, Option<fleet_probe::types::Phases>) {
    let t0 = Instant::now();
    let ms = |t: Instant| (t.elapsed().as_secs_f64() * 10000.0).round() / 10.0;
    // 阶段：connect（到本机客户端 + SOCKS）→ tls → http
    let stage = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
    let st = stage.clone();
    let phases = std::sync::Arc::new(std::sync::Mutex::new(fleet_probe::types::Phases::default()));
    let ph = phases.clone();
    let fut = async move {
        let mut s = match tokio::net::TcpStream::connect(("127.0.0.1", socks)).await {
            Ok(s) => s,
            Err(_) => return (7, Some("connect".to_string()), None),
        };
        // 小包（SOCKS 握手、TLS 记录）不能等 Nagle + 回环 delayed ACK，否则每次往返多出几十毫秒（curl 也设 TCP_NODELAY）
        let _ = s.set_nodelay(true);
        let mut req = vec![5u8, 1, 0];
        let mut b = [0u8; 2];
        if s.write_all(&req).await.is_err() || s.read_exact(&mut b).await.is_err() || b != [5, 0] {
            return (97, Some("connect".into()), None);
        }
        req = vec![5, 1, 0, 3, HOST.len() as u8];
        req.extend_from_slice(HOST.as_bytes());
        req.extend_from_slice(&443u16.to_be_bytes());
        let mut rep = [0u8; 10];
        if s.write_all(&req).await.is_err() || s.read_exact(&mut rep).await.is_err() || rep[1] != 0 {
            return (97, Some("connect".into()), None);
        }
        st.store(1, std::sync::atomic::Ordering::Relaxed);
        let socks_ms = ms(t0);
        ph.lock().unwrap().connect_ms = Some(socks_ms);
        let mut t = Transport::Plain(s);
        match http::get(&mut t, HOST, URL_PATH, t0).await {
            Ok(r) => {
                let mut g = ph.lock().unwrap();
                g.tls_ms = Some(r.tls_ms);
                g.first_byte_ms = Some(r.first_byte_ms);
                drop(g);
                if r.status != 200 {
                    return (22, Some("read".into()), None);
                }
                let body = String::from_utf8_lossy(&r.body);
                let ip = body.lines().find_map(|l| l.strip_prefix("ip=")).map(|x| x.trim().to_string());
                (0, None, ip)
            }
            Err(e) => match e.phase {
                Phase::Tls => (35, Some("tls".into()), None),
                Phase::Cert => (60, Some("tls".into()), None),
                Phase::Http => (56, Some("read".into()), None),
            },
        }
    };
    match tokio::time::timeout(limit, fut).await {
        Ok((rc, phase, ip)) => (rc, phase, ip, ms(t0), Some(phases.lock().unwrap().clone())),
        Err(_) => {
            let phase = match stage.load(std::sync::atomic::Ordering::Relaxed) {
                0 => "connect",
                _ => "tls",
            };
            (28, Some(phase.into()), None, ms(t0), Some(phases.lock().unwrap().clone()))
        }
    }
}

async fn check(e: Entry, socks: u16, deadline: Instant) -> ProbeResult {
    let mut attempts = vec![];
    let mut last_phases = None;
    let (mut rc, mut phase, mut actual, mut ms) = (28, Some("deadline".to_string()), None, None);
    let mut ok = false;
    for _ in 0..2 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining <= Duration::from_secs(1) {
            break;
        }
        let lim = Duration::from_secs(18).min(remaining - Duration::from_secs(1));
        let (r, p, a, m, phs) = attempt(socks, lim).await;
        last_phases = phs;
        rc = r;
        phase = p.clone();
        actual = a.clone();
        ms = Some(m);
        attempts.push(PathAttempt { rc: r, ms: Some(m), phase: p, actual_ip: a.clone() });
        // 只看能否经入口上网；出口 IP 只作数据（漏到线路机出口另由 mark_leaks 标记）
        ok = r == 0 && a.is_some();
        if ok {
            break;
        }
    }
    let now = crate::util::now_f64();
    let status = if ok {
        Status::Ok
    } else if rc == 28 {
        Status::Timeout
    } else {
        Status::Fail
    };
    let mut r = ProbeResult::new(e.node.clone(), CheckKind::Path, status, now);
    r.value_ms = if ok { ms } else { None };
    r.actual_ip = actual.clone();
    r.expected_ip = e.expected_ip.clone();
    r.legacy_rc = Some(rc);
    r.attempt = Some(attempts.len() as u8);
    r.final_attempt = Some(true);
    r.source = Some("cloudflare".into());
    r.reason_code = if ok {
        None
    } else {
        Some(match rc {
            28 => "timeout",
            7 | 97 => "entry_unreachable",
            35 | 60 => "tls",
            22 | 56 => "http_error",
            _ => "bad_response",
        }
        .into())
    };
    r.phases = last_phases;
    r.path = Some(PathDetail { entry_port: e.port, entry_tag: e.tag.clone(), healthy: ok, rc, phase, ms, attempts });
    r
}

fn spawn_client(exe: &str, cfg_path: &Path) -> std::io::Result<Child> {
    let mut c = Command::new(exe);
    c.args(["run", "-c"]).arg(cfg_path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    unsafe {
        c.pre_exec(|| {
            // 创建本进程的线程退出时，内核向它发 SIGKILL（relayd 自己不能调 kill）
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    c.spawn()
}

/// 常驻客户端：由一个“持有线程”fork（PDEATHSIG 绑在这个线程上），配置不变就一直复用；
/// 需要换配置时让持有线程退出 → 内核 SIGKILL 客户端 → 回收。省掉每个周期启动一次 xray（Go 运行时启动在慢 CPU 上约 0.3% 单核）。
struct Client {
    key: String,
    ports: Vec<u16>,
    pid: u32,
    stop: std::sync::mpsc::Sender<()>,
    holder: std::thread::JoinHandle<Option<Child>>,
}

impl Client {
    fn start(exe: &str, es: &[Entry], tmp_dir: &Path, key: String) -> Result<Client, String> {
        let ports: Vec<u16> = es.iter().map(|_| free_port()).collect::<Option<Vec<_>>>().ok_or("拿不到空闲端口")?;
        let cfg_path = tmp_dir.join("pathcheck-client.json");
        crate::util::write_atomic(&cfg_path, client_config(es, &ports).to_string().as_bytes()).map_err(|e| e.to_string())?;
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let (ptx, prx) = std::sync::mpsc::channel::<Result<u32, String>>();
        let exe = exe.to_string();
        let holder = std::thread::Builder::new()
            .name("path-client".into())
            .stack_size(128 * 1024)
            .spawn(move || -> Option<Child> {
                match spawn_client(&exe, &cfg_path) {
                    Ok(c) => {
                        let _ = ptx.send(Ok(c.id()));
                        let _ = rx.recv(); // 等停止信号（或发送端被丢弃）
                        Some(c) // 线程退出 → PDEATHSIG
                    }
                    Err(e) => {
                        let _ = ptx.send(Err(format!("起客户端 xray 失败：{e}")));
                        None
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let pid = prx.recv().map_err(|_| "客户端持有线程异常".to_string())??;
        Ok(Client { key, ports, pid, stop: tx, holder })
    }

    fn alive(&self) -> bool {
        std::fs::read(format!("/proc/{}/cmdline", self.pid)).map(|b| !b.is_empty()).unwrap_or(false)
            && std::fs::read_to_string(format!("/proc/{}/stat", self.pid)).map(|s| !s.contains(") Z ")).unwrap_or(false)
    }

    fn shutdown(self) {
        let _ = self.stop.send(());
        if let Ok(Some(mut c)) = self.holder.join() {
            // 持有线程已退出 → 内核已发 SIGKILL（不可忽略）：阻塞 wait 一定很快返回，且保证回收
            let _ = c.wait();
        }
    }
}

fn client_key(exe: &str, es: &[Entry]) -> String {
    let mut k = exe.to_string();
    for e in es {
        k += &format!("|{}:{}:{}:{}:{}:{}:{}:{}", e.tag, e.port, e.uuid, e.flow, e.sni, e.sid, e.pbk, e.network);
    }
    k
}

/// 线路机自检：主节点入口（出口是 freedom）的全链路结果改写成 egress 行（node = 线路机资产、期望出口 = 本机公网 IP），
/// 按 HL-04：第 1 次失败且有第 2 次时，第 1 次记 final=false
pub fn self_egress_rows(r: &ProbeResult, relay_uuid: &str, public_ip: &str) -> Vec<ProbeResult> {
    let Some(p) = &r.path else { return vec![] };
    let n = p.attempts.len();
    let mut out = vec![];
    for (i, a) in p.attempts.iter().enumerate() {
        let ok = a.rc == 0 && a.actual_ip.is_some();
        let status = if ok {
            Status::Ok
        } else if a.rc == 28 {
            Status::Timeout
        } else {
            Status::Fail
        };
        let mut e = ProbeResult::new(relay_uuid.to_string(), CheckKind::Egress, status, r.measured_at - (n - 1 - i) as f64 * 0.001);
        e.expected_ip = Some(public_ip.to_string());
        e.actual_ip = a.actual_ip.clone();
        e.value_ms = if ok { a.ms } else { None };
        e.source = Some("cloudflare".into());
        e.attempt = Some((i + 1) as u8);
        e.final_attempt = Some(i + 1 == n);
        e.legacy_rc = Some(if ok { 0 } else if a.rc == 0 { -1 } else { a.rc });
        e.reason_code = if ok {
            None
        } else {
            Some(match a.rc {
                28 => "timeout",
                7 | 97 => "upstream_down",
                35 | 60 => "auth",
                22 | 56 => "http_error",
                _ => "bad_response",
            }
            .into())
        };
        e.target = Some(format!("127.0.0.1:{}", p.entry_port));
        out.push(e);
    }
    out
}

/// 常驻线程：每 `interval` 一个周期，结果写 probe.db
#[allow(clippy::too_many_arguments)]
pub fn run_loop(
    interval: Duration,
    sources: Vec<String>,
    ids_path: Option<String>,
    xray_bin: String,
    proc_match: String,
    db: PathBuf,
    tmp_dir: PathBuf,
    relay_self: Option<crate::config::RelaySelfCfg>,
) {
    let mut store = match crate::probe::open_store_pub(&db) {
        Ok(s) => s,
        Err(e) => {
            warn!("path_store_open_failed", "err={}", crate::log::q(&e));
            return;
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            warn!("path_runtime_failed", "err={}", crate::log::q(&e.to_string()));
            return;
        }
    };
    let mut last_err: Option<String> = None;
    let mut exe_cache: Option<(String, Instant)> = None;
    let mut client: Option<Client> = None;
    // 自适应：每个周期（60 s）测主节点入口（线路机自检）和上次不健康的入口；全量 120 s 一次；
    // fleetd 提示有人在看时每个周期都全量
    let mut last_full: Option<Instant> = None;
    // 上一次结果不健康的入口（tag）：它们每个周期都测
    let mut bad: std::collections::BTreeSet<String> = Default::default();
    // 配置与身份文件按 mtime 缓存，不变就不重读、不重算公钥
    let mut cached: Option<(Option<std::time::SystemTime>, Option<std::time::SystemTime>, Vec<Entry>)> = None;
    let mtime = |p: &str| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    loop {
        let t0 = Instant::now();
        let r = (|| -> Result<(usize, usize), String> {
            let src = sources.first().ok_or("没有 Xray 配置来源")?;
            let (m1, m2) = (mtime(src), ids_path.as_deref().and_then(mtime));
            if cached.as_ref().is_none_or(|c| c.0 != m1 || c.1 != m2 || m1.is_none()) {
                let cfg: Value = serde_json::from_slice(&std::fs::read(src).map_err(|e| format!("读 {src}：{e}"))?).map_err(|e| e.to_string())?;
                let ids = match ids_path.as_deref() {
                    Some(p) if Path::new(p).exists() => targets::identities_from_legacy_assets(Path::new(p)).unwrap_or_default(),
                    _ => BTreeMap::new(),
                };
                cached = Some((m1, m2, entries(&cfg, &ids)));
            }
            let all = cached.as_ref().unwrap().2.clone();
            let live = crate::LIVE_UNTIL.load(std::sync::atomic::Ordering::Relaxed) > crate::util::now_f64() as u64;
            let full = live || last_full.is_none_or(|t| t.elapsed() >= interval * 2 - Duration::from_secs(5));
            let master_port = relay_self.as_ref().and_then(|rs| rs.master_port.or_else(|| all.iter().find(|e| e.out_protocol == "freedom").map(|e| e.port)));
            let es: Vec<Entry> = if full { all.clone() } else { all.iter().filter(|e| Some(e.port) == master_port || bad.contains(&e.tag)).cloned().collect() };
            if es.is_empty() {
                return Ok((0, 0));
            }
            // xray 可执行文件路径缓存 10 min（每次扫描要读所有进程的 cmdline）；文件不在了立即重扫
            if exe_cache.as_ref().is_none_or(|(p, t): &(String, Instant)| t.elapsed() > Duration::from_secs(600) || !Path::new(p).exists()) {
                exe_cache = crate::xray::find_xray_exe(&xray_bin, &proc_match).map(|p| (p, Instant::now()));
            }
            let exe = exe_cache.as_ref().map(|x| x.0.clone()).ok_or("找不到运行中的 xray 进程")?;
            // 客户端按全部入口起（只测主节点的周期也复用同一个客户端）
            let key = client_key(&exe, &all);
            let reuse = client.as_ref().is_some_and(|c| c.key == key && c.alive());
            if !reuse {
                if let Some(c) = client.take() {
                    c.shutdown();
                }
                let c = Client::start(&exe, &all, &tmp_dir, key)?;
                info!("path_client_started", "pid={} entries={}", c.pid, all.len());
                client = Some(c);
            }
            let all_ports = client.as_ref().unwrap().ports.clone();
            let ports: Vec<u16> = all.iter().zip(&all_ports).filter(|(e, _)| es.iter().any(|x| x.tag == e.tag)).map(|(_, p)| *p).collect();
            if es.is_empty() {
                return Ok((0, 0));
            }
            let fresh = !reuse;
            let master = master_port;
            let mut rs = rt.block_on(async {
                if fresh {
                    // 等客户端开始监听（最多 5 s），再等 0.8 s 让它完成初始化（与旧实现一致）
                    let st = Instant::now();
                    let last = *ports.last().unwrap();
                    let last = *all_ports.last().unwrap_or(&last);
                    while st.elapsed() < Duration::from_secs(5) {
                        if tokio::net::TcpStream::connect(("127.0.0.1", last)).await.is_ok() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    tokio::time::sleep(Duration::from_millis(800)).await;
                }
                let deadline = Instant::now() + Duration::from_secs(140);
                let sem = Arc::new(tokio::sync::Semaphore::new(3));
                let mut hs = vec![];
                for (e, p) in es.into_iter().zip(ports) {
                    let sem = sem.clone();
                    hs.push(tokio::spawn(async move {
                        let _g = sem.acquire().await;
                        check(e, p, deadline).await
                    }));
                }
                let mut out = vec![];
                for h in hs {
                    if let Ok(r) = h.await {
                        out.push(r);
                    }
                }
                out
            });
            let n = rs.len();
            let ok = rs.iter().filter(|r| r.status == Status::Ok).count();
            if full {
                last_full = Some(Instant::now());
            }
            for r in &rs {
                if let Some(p) = &r.path {
                    if p.healthy {
                        bad.remove(&p.entry_tag);
                    } else {
                        bad.insert(p.entry_tag.clone());
                    }
                }
            }
            if let (Some(sc), Some(mp)) = (relay_self.as_ref(), master) {
                let extra: Vec<ProbeResult> = rs
                    .iter()
                    .filter(|r| r.path.as_ref().is_some_and(|p| p.entry_port == mp))
                    .flat_map(|r| self_egress_rows(r, &sc.asset_uuid, &sc.public_ip))
                    .collect();
                rs.extend(extra);
            }
            let direct_ports: Vec<u16> = all.iter().filter(|e| e.out_protocol == "freedom").map(|e| e.port).collect();
            crate::probe::mark_leaks(&mut rs, &crate::probe::leak_ips_of(relay_self.as_ref()), &direct_ports);
            store.insert(&mut rs).map_err(|e| e.to_string())?;
            Ok((n, ok))
        })();
        match r {
            Ok((n, ok)) => {
                if last_err.take().is_some() || n != ok {
                    info!("path_cycle", "entries={n} healthy={ok} ms={}", t0.elapsed().as_millis());
                }
            }
            Err(e) => {
                if last_err.as_deref() != Some(&e) {
                    warn!("path_cycle_failed", "err={}", crate::log::q(&e));
                }
                last_err = Some(e);
                // 出错时丢掉客户端，下个周期重起
                if let Some(c) = client.take() {
                    c.shutdown();
                }
            }
        }
        std::thread::sleep(interval.saturating_sub(t0.elapsed()).max(Duration::from_secs(5)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x25519_known() {
        // RFC 7748 §6.1 Alice
        let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let sk = hex_decode("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        let pk = x25519_public(&e.encode(sk)).unwrap();
        assert_eq!(e.decode(pk).unwrap(), hex_decode("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a"));
    }

    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }
}
