//! relayd 观察模式：只读 Xray 统计。
//!
//! - `StatsService.QueryStats`：gRPC over h2c 连 3x-ui 的 api 入站（127.0.0.1），请求体是**空消息**，
//!   即 `pattern=""`、`reset=false`（proto3 缺省值不编码），所以从协议上就不可能清零计数。
//!   这里只实现 QueryStats 一个方法，不包含任何会改动 Xray 的调用。
//! - 进程：扫描 /proc/*/cmdline 找 xray（argv[0] 文件名匹配且命令行含 `proc_match`）和 x-ui。
//! - 连接：/proc/net/tcp{,6} 里状态 ESTABLISHED、本地端口属于入站端口的条数。
//! - 增量：按核心纪元 core_id = boot_id:pid:starttime 求差；纪元变化或计数回退视为重启（增量 = 当前值），
//!   规则见 `delta_for`。
//! - 累计值是流量的真实来源：可用 x-ui.db 导出的种子起算（`seed_file`），之后逐次加增量，跨 Xray 重启保持；
//!   累计值、原始值、基线随每批写进状态目录（`xray.json`），relayd 重启不丢增量。
use crate::config::XrayCfg;
use crate::runner::Extension;
use crate::util::{self, now_f64};
use crate::{info, warn};
use bytes::Bytes;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufRead;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const QUERY_STATS_PATH: &str = "/xray.app.stats.command.StatsService/QueryStats";

// ---------- protobuf / gRPC 帧 ----------

fn varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let mut x: u64 = 0;
    let mut s = 0;
    while *i < b.len() && s < 64 {
        let c = b[*i];
        *i += 1;
        x |= ((c & 0x7f) as u64) << s;
        if c & 0x80 == 0 {
            return Some(x);
        }
        s += 7;
    }
    None
}

fn skip(b: &[u8], i: &mut usize, wt: u64) -> Option<()> {
    match wt {
        0 => varint(b, i).map(|_| ()),
        1 => {
            *i += 8;
            (*i <= b.len()).then_some(())
        }
        2 => {
            let n = varint(b, i)? as usize;
            *i = i.checked_add(n)?;
            (*i <= b.len()).then_some(())
        }
        5 => {
            *i += 4;
            (*i <= b.len()).then_some(())
        }
        _ => None,
    }
}

/// 解码 QueryStatsResponse { repeated Stat stat = 1; }，Stat { string name = 1; int64 value = 2; }
pub fn decode_query_stats(msg: &[u8]) -> Result<Vec<(String, i64)>, String> {
    let mut out = vec![];
    let mut i = 0;
    while i < msg.len() {
        let tag = varint(msg, &mut i).ok_or("坏的 tag")?;
        let (f, wt) = (tag >> 3, tag & 7);
        if f == 1 && wt == 2 {
            let n = varint(msg, &mut i).ok_or("坏的长度")? as usize;
            let end = i.checked_add(n).filter(|&e| e <= msg.len()).ok_or("越界")?;
            let sb = &msg[i..end];
            i = end;
            let (mut name, mut value) = (String::new(), 0i64);
            let mut j = 0;
            while j < sb.len() {
                let t = varint(sb, &mut j).ok_or("坏的 Stat tag")?;
                match (t >> 3, t & 7) {
                    (1, 2) => {
                        let n = varint(sb, &mut j).ok_or("坏的名字长度")? as usize;
                        let e = j.checked_add(n).filter(|&e| e <= sb.len()).ok_or("名字越界")?;
                        name = String::from_utf8_lossy(&sb[j..e]).into_owned();
                        j = e;
                    }
                    (2, 0) => value = varint(sb, &mut j).ok_or("坏的值")? as i64,
                    (_, w) => skip(sb, &mut j, w).ok_or("无法跳过字段")?,
                }
            }
            out.push((name, value));
        } else {
            skip(msg, &mut i, wt).ok_or("无法跳过字段")?;
        }
    }
    Ok(out)
}

/// 取出 gRPC 长度前缀帧里的第一条消息
pub fn grpc_unframe(buf: &[u8]) -> Result<&[u8], String> {
    if buf.len() < 5 {
        return Err(format!("响应过短（{} 字节）", buf.len()));
    }
    if buf[0] != 0 {
        return Err("响应被压缩（不支持）".into());
    }
    let n = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    buf.get(5..5 + n).ok_or_else(|| "响应截断".to_string())
}

/// 一次 QueryStats（reset=false）。每次新建连接（`host:port` 或 `unix:/path`），3 s 超时。
pub fn query_stats(rt: &tokio::runtime::Runtime, addr: &str) -> Result<Vec<(String, i64)>, String> {
    // 空的 QueryStatsRequest：pattern=""，reset=false
    decode_query_stats(&grpc_unary(rt, addr, QUERY_STATS_PATH, Vec::new())?)
}

/// 复用的 h2 连接（每 5 s 一次 QueryStats：省掉每次的 TCP + h2 握手）。当前线程运行时只在 block_on 期间驱动连接，
/// 连接在两次调用之间被对端关掉时，下一次 ready() 失败 → 丢掉重连一次。
#[derive(Default)]
pub struct StatsConn {
    client: Option<h2::client::SendRequest<Bytes>>,
    driver: Option<tokio::task::JoinHandle<()>>,
    authority: String,
    pub reconnects: u64,
}

impl Drop for StatsConn {
    fn drop(&mut self) {
        if let Some(d) = self.driver.take() {
            d.abort();
        }
    }
}

async fn stats_call(sc: &mut StatsConn, addr: &str, path: &'static str, frame: &[u8]) -> Result<Vec<u8>, String> {
    if sc.client.is_none() {
        let mut b = h2::client::Builder::new();
        b.initial_window_size(4 << 20).initial_connection_window_size(4 << 20);
        let (client, conn, authority) = if let Some(p) = addr.strip_prefix("unix:") {
            let s = tokio::net::UnixStream::connect(p).await.map_err(|e| format!("连接 {addr} 失败：{e}"))?;
            let (c, conn) = b.handshake::<_, Bytes>(s).await.map_err(|e| format!("h2 握手失败：{e}"))?;
            (c, tokio::spawn(async move {
                let _ = conn.await;
            }), "localhost".to_string())
        } else {
            let s = tokio::net::TcpStream::connect(addr).await.map_err(|e| format!("连接 {addr} 失败：{e}"))?;
            let _ = s.set_nodelay(true);
            let (c, conn) = b.handshake::<_, Bytes>(s).await.map_err(|e| format!("h2 握手失败：{e}"))?;
            (c, tokio::spawn(async move {
                let _ = conn.await;
            }), addr.to_string())
        };
        sc.client = Some(client);
        sc.driver = Some(conn);
        sc.authority = authority;
        sc.reconnects += 1;
    }
    let client = sc.client.clone().unwrap();
    let mut client = client.ready().await.map_err(|e| e.to_string())?;
    let req = http::Request::builder()
        .method("POST")
        .uri(format!("http://{}{path}", sc.authority))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(())
        .map_err(|e| e.to_string())?;
    let (resp, mut stream) = client.send_request(req, false).map_err(|e| e.to_string())?;
    stream.send_data(Bytes::copy_from_slice(frame), true).map_err(|e| e.to_string())?;
    let resp = resp.await.map_err(|e| format!("gRPC 响应失败：{e}"))?;
    let (parts, mut body) = resp.into_parts();
    if parts.status != 200 {
        return Err(format!("HTTP {}", parts.status));
    }
    if let Some(gs) = parts.headers.get("grpc-status") {
        if gs != "0" {
            return Err(format!("grpc-status {}", gs.to_str().unwrap_or("?")));
        }
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = body.data().await {
        let c = chunk.map_err(|e| e.to_string())?;
        let _ = body.flow_control().release_capacity(c.len());
        buf.extend_from_slice(&c);
        if buf.len() > 16 << 20 {
            return Err("响应过大".into());
        }
    }
    if let Ok(Some(tr)) = body.trailers().await {
        if let Some(gs) = tr.get("grpc-status") {
            if gs != "0" {
                return Err(format!("grpc-status {}", gs.to_str().unwrap_or("?")));
            }
        }
    }
    grpc_unframe(&buf).map(|m| m.to_vec())
}

/// QueryStats（reset=false，空请求体）走复用连接；失败时丢掉连接重连重试一次
pub fn query_stats_conn(rt: &tokio::runtime::Runtime, sc: &mut StatsConn, addr: &str) -> Result<Vec<(String, i64)>, String> {
    const EMPTY: [u8; 5] = [0, 0, 0, 0, 0];
    let mut last = String::new();
    for _ in 0..2 {
        let r = rt.block_on(async { tokio::time::timeout(Duration::from_secs(3), stats_call(sc, addr, QUERY_STATS_PATH, &EMPTY)).await });
        match r {
            Ok(Ok(m)) => return decode_query_stats(&m),
            Ok(Err(e)) => last = e,
            Err(_) => last = "Xray API 超时（3 s）".into(),
        }
        sc.client = None;
        if let Some(d) = sc.driver.take() {
            d.abort();
        }
    }
    Err(last)
}

pub const GET_STATS_PATH: &str = "/xray.app.stats.command.StatsService/GetStats";

/// GetStats(name, reset=true)：**只**由管理模式的「重置入口流量」操作调用，一次一个计数；返回清零前的值
pub fn get_stats_reset(rt: &tokio::runtime::Runtime, addr: &str, name: &str) -> Result<i64, String> {
    let mut req = vec![0x0a];
    let mut n = name.len();
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            req.push(b);
            break;
        }
        req.push(b | 0x80);
    }
    req.extend_from_slice(name.as_bytes());
    req.extend_from_slice(&[0x10, 0x01]);
    let r = decode_query_stats(&grpc_unary(rt, addr, GET_STATS_PATH, req)?)?;
    Ok(r.into_iter().find(|(n, _)| n == name).map(|x| x.1).unwrap_or(0))
}

/// 一次 gRPC 一元调用，返回去掉长度前缀后的响应消息
pub fn grpc_unary(rt: &tokio::runtime::Runtime, addr: &str, path: &'static str, msg: Vec<u8>) -> Result<Vec<u8>, String> {
    let addr = addr.to_string();
    let mut frame = vec![0u8];
    frame.extend_from_slice(&(msg.len() as u32).to_be_bytes());
    frame.extend_from_slice(&msg);
    rt.block_on(async move {
        let fut = async {
            let mut b = h2::client::Builder::new();
            b.initial_window_size(4 << 20).initial_connection_window_size(4 << 20);
            let (client, driver, authority) = if let Some(path) = addr.strip_prefix("unix:") {
                let s = tokio::net::UnixStream::connect(path).await.map_err(|e| format!("连接 {addr} 失败：{e}"))?;
                let (client, conn) = b.handshake::<_, Bytes>(s).await.map_err(|e| format!("h2 握手失败：{e}"))?;
                (client, tokio::spawn(async move {
                    let _ = conn.await;
                }), "localhost".to_string())
            } else {
                let s = tokio::net::TcpStream::connect(&addr).await.map_err(|e| format!("连接 {addr} 失败：{e}"))?;
                let _ = s.set_nodelay(true);
                let (client, conn) = b.handshake::<_, Bytes>(s).await.map_err(|e| format!("h2 握手失败：{e}"))?;
                (client, tokio::spawn(async move {
                    let _ = conn.await;
                }), addr.clone())
            };
            let res = async {
                let mut client = client.ready().await.map_err(|e| e.to_string())?;
                let req = http::Request::builder()
                    .method("POST")
                    .uri(format!("http://{authority}{path}"))
                    .header("content-type", "application/grpc")
                    .header("te", "trailers")
                    .body(())
                    .map_err(|e| e.to_string())?;
                let (resp, mut stream) = client.send_request(req, false).map_err(|e| e.to_string())?;
                stream.send_data(Bytes::from(frame), true).map_err(|e| e.to_string())?;
                let resp = resp.await.map_err(|e| format!("gRPC 响应失败：{e}"))?;
                let (parts, mut body) = resp.into_parts();
                if parts.status != 200 {
                    return Err(format!("HTTP {}", parts.status));
                }
                if let Some(gs) = parts.headers.get("grpc-status") {
                    if gs != "0" {
                        let m = parts.headers.get("grpc-message").and_then(|v| v.to_str().ok()).unwrap_or("");
                        return Err(format!("grpc-status {} {m}", gs.to_str().unwrap_or("?")));
                    }
                }
                let mut buf: Vec<u8> = Vec::new();
                while let Some(chunk) = body.data().await {
                    let c = chunk.map_err(|e| e.to_string())?;
                    let _ = body.flow_control().release_capacity(c.len());
                    buf.extend_from_slice(&c);
                    if buf.len() > 16 << 20 {
                        return Err("响应过大".into());
                    }
                }
                if let Ok(Some(tr)) = body.trailers().await {
                    if let Some(gs) = tr.get("grpc-status") {
                        if gs != "0" {
                            let m = tr.get("grpc-message").and_then(|v| v.to_str().ok()).unwrap_or("");
                            return Err(format!("grpc-status {} {m}", gs.to_str().unwrap_or("?")));
                        }
                    }
                }
                grpc_unframe(&buf).map(|m| m.to_vec())
            }
            .await;
            driver.abort();
            res
        };
        match tokio::time::timeout(Duration::from_secs(3), fut).await {
            Ok(r) => r,
            Err(_) => Err("Xray API 超时（3 s）".into()),
        }
    })
}

// ---------- 进程 ----------

#[derive(Clone, Debug, PartialEq)]
pub struct Proc {
    pub pid: u32,
    /// /proc/<pid>/stat 第 22 列（开机后的 clock ticks）
    pub start_ticks: u64,
    /// argv[0]（取版本时执行它；fleet-xray 的路径随版本变化）
    pub exe: String,
}

fn clk_tck() -> f64 {
    let t = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if t > 0 {
        t as f64
    } else {
        100.0
    }
}

fn btime() -> f64 {
    std::fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("btime ").and_then(|v| v.trim().parse::<f64>().ok())))
        .unwrap_or(0.0)
}

fn start_ticks(pid: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &s[s.rfind(')')? + 2..];
    // rest 从第 3 列（state）开始，starttime 是第 22 列
    rest.split_whitespace().nth(19)?.parse().ok()
}

fn cmdline(pid: &str) -> Option<Vec<String>> {
    let b = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if b.is_empty() {
        return None;
    }
    Some(b.split(|c| *c == 0).filter(|x| !x.is_empty()).map(|x| String::from_utf8_lossy(x).into_owned()).collect())
}

fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// 找 xray 与 x-ui 进程（一次 /proc 扫描）
fn scan(bin_name: &str, proc_match: &str, xui_name: &str) -> (Option<Proc>, Option<Proc>) {
    let (mut xr, mut xu) = (None, None);
    let Ok(rd) = std::fs::read_dir("/proc") else { return (None, None) };
    for e in rd.flatten() {
        let name = e.file_name();
        let n = name.to_string_lossy();
        if !n.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
            continue;
        }
        let Some(args) = cmdline(&n) else { continue };
        let a0 = basename(&args[0]);
        let pid: u32 = n.parse().unwrap_or(0);
        if xr.is_none() && a0 == bin_name && args.join(" ").contains(proc_match) {
            if let Some(st) = start_ticks(pid) {
                xr = Some(Proc { pid, start_ticks: st, exe: args[0].clone() });
            }
        } else if xu.is_none() && a0 == xui_name {
            if let Some(st) = start_ticks(pid) {
                xu = Some(Proc { pid, start_ticks: st, exe: args[0].clone() });
            }
        }
        if xr.is_some() && xu.is_some() {
            break;
        }
    }
    (xr, xu)
}

/// 运行中的 xray 的 argv[0]（绝对路径）
pub fn find_xray_exe(bin_name: &str, proc_match: &str) -> Option<String> {
    scan(bin_name, proc_match, "\0").0.map(|p| p.exe).filter(|e| e.starts_with('/'))
}

fn still_alive(p: &Option<Proc>) -> bool {
    p.as_ref().is_some_and(|p| start_ticks(p.pid) == Some(p.start_ticks))
}

fn exec_version(bin: &str) -> Option<String> {
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("version");
    // 超时由 PDEATHSIG 杀掉并回收（relayd 的 seccomp 禁止 kill）
    let r = crate::child::run(cmd, Duration::from_secs(5)).ok()?;
    let out = String::from_utf8_lossy(&r.stdout).to_string();
    let first = out.lines().next()?;
    // "Xray 26.7.28 (Xray, Penetrates Everything.) 5ca6f4b (go1.26.5 linux/amd64)"
    let mut it = first.split_whitespace();
    let prod = it.next()?;
    let ver = it.next()?;
    (prod.eq_ignore_ascii_case("xray")).then(|| ver.to_string())
}

// ---------- 连接数 ----------

/// 统计 ESTABLISHED 且本地端口在 `ports` 里的 TCP 连接（v4 + v6）
/// /proc/net/tcp{,6} 的地址字段（主机字节序的十六进制）→ IP；IPv4 映射的 IPv6 归一成 IPv4（与旧 bridge 的 normalized_ip 相同）
pub fn proc_ip(hex: &str) -> Option<std::net::IpAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    match hex.len() {
        8 => Some(IpAddr::V4(Ipv4Addr::from(u32::from_str_radix(hex, 16).ok()?.to_le_bytes()))),
        32 => {
            let mut b = [0u8; 16];
            for i in 0..4 {
                let w = u32::from_str_radix(&hex[i * 8..i * 8 + 8], 16).ok()?;
                b[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
            }
            let v6 = Ipv6Addr::from(b);
            Some(v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)))
        }
        _ => None,
    }
}

pub fn established_by_port(ports: &HashSet<u16>) -> BTreeMap<u16, u64> {
    established_with_sources(ports).0
}

/// 已建立连接数 + 每个端口的来源 IP（去重、按首次出现顺序、每端口最多 100 个），与旧 bridge 的 get_connections 同口径
pub fn established_with_sources(ports: &HashSet<u16>) -> (BTreeMap<u16, u64>, BTreeMap<u16, Vec<String>>) {
    let mut m: BTreeMap<u16, u64> = ports.iter().map(|p| (*p, 0)).collect();
    let mut srcs: BTreeMap<u16, Vec<String>> = ports.iter().map(|p| (*p, vec![])).collect();
    if ports.is_empty() {
        return (m, srcs);
    }
    let mut line = String::new();
    for p in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(f) = std::fs::File::open(p) else { continue };
        let mut r = std::io::BufReader::with_capacity(32 * 1024, f);
        line.clear();
        let _ = r.read_line(&mut line); // 表头
        loop {
            line.clear();
            match r.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let mut it = line.split_whitespace();
            let _sl = it.next();
            let Some(local) = it.next() else { continue };
            let rem = it.next();
            if it.next() != Some("01") {
                continue;
            }
            let Some(ph) = local.rsplit(':').next() else { continue };
            if let Ok(port) = u16::from_str_radix(ph, 16) {
                if let Some(c) = m.get_mut(&port) {
                    *c += 1;
                    if let Some(ip) = rem.and_then(|r| r.split(':').next()).and_then(proc_ip) {
                        let v = srcs.get_mut(&port).unwrap();
                        let ip = ip.to_string();
                        if v.len() < 100 && !v.contains(&ip) {
                            v.push(ip);
                        }
                    }
                }
            }
        }
    }
    (m, srcs)
}

// ---------- 采集器 ----------

/// 计数名 → (kind, tag, 是否上行)
fn split_name(n: &str) -> Option<(&str, &str, bool)> {
    // inbound>>>TAG>>>traffic>>>uplink / user>>>EMAIL>>>traffic>>>downlink / outbound>>>TAG>>>traffic>>>…
    let mut it = n.split(">>>");
    let kind = it.next()?;
    let tag = it.next()?;
    if it.next()? != "traffic" {
        return None;
    }
    let up = match it.next()? {
        "uplink" => true,
        "downlink" => false,
        _ => return None,
    };
    Some((kind, tag, up))
}

fn counter_name(kind: &str, tag: &str, up: bool) -> String {
    format!("{kind}>>>{tag}>>>traffic>>>{}", if up { "uplink" } else { "downlink" })
}

/// 每条计数的增量规则（纯函数，便于测试）：
/// - 核心纪元变了（Xray 重启）：计数从 0 重新开始，增量 = 当前值
/// - 同一纪元，计数回退：视为重置，增量 = 当前值
/// - 同一纪元，新出现的计数（运行中新增的入站/用户，从 0 开始）：增量 = 当前值
/// - 本进程还没有任何基线（首次启动且没有种子）：只记基线，增量 0
pub fn delta_for(prev: Option<i64>, cur: i64, core_changed: bool, have_baseline: bool, full: bool) -> (i64, bool) {
    let cur = cur.max(0);
    if full {
        return (cur, false);
    }
    if !have_baseline {
        return (0, false);
    }
    if core_changed {
        return (cur, true);
    }
    match prev {
        Some(p) if cur >= p => (cur - p, false),
        Some(_) => (cur, true),
        None => (cur, false),
    }
}

pub struct Collector {
    cfg: XrayCfg,
    rt: tokio::runtime::Runtime,
    bin_name: String,
    state_path: Option<PathBuf>,
    boot_id: String,
    btime: f64,
    tck: f64,
    xray: Option<Proc>,
    xui: Option<Proc>,
    version: Option<String>,
    version_for: Option<Proc>,
    core_id: Option<String>,
    /// 上次读到的原始计数（Xray 进程内累计值）
    prev: HashMap<String, i64>,
    prev_at: Option<Instant>,
    /// 累计值（真实来源）：种子 + 之后按增量累加，跨 Xray 重启保持
    cum: BTreeMap<String, u64>,
    accum_since: f64,
    seed: Option<Value>,
    seed_mtime: Option<std::time::SystemTime>,
    /// 下一次成功读取时把当前值全部计为增量（种子之后才启动的 Xray）
    full_next: bool,
    last: Vec<(String, i64)>,
    /// 最近一次采样的速率（字节/秒），按 (kind, tag)
    rates: BTreeMap<(String, String), (f64, f64)>,
    last_ok: f64,
    last_err: Option<String>,
    ports: HashSet<u16>,
    conns: BTreeMap<u16, u64>,
    srcs: BTreeMap<u16, Vec<String>>,
    errors: u64,
    dirty: bool,
    last_scan: Option<Instant>,
    stats_conn: StatsConn,
    /// 从 Xray 配置读出的入站（tag → 端口、协议、用户 email）与 api 地址
    inbounds: BTreeMap<String, InboundInfo>,
    cfg_api: Option<String>,
    api_tag: String,
    xcfg_mtime: Option<std::time::SystemTime>,
    xcfg_mtime_f: Option<f64>,
    xcfg_checked: Option<Instant>,
    xcfg_err: Option<String>,
    /// 配置里的 log.access 与 blackhole 出站
    pub access_log: Option<String>,
    pub blackholes: Vec<String>,
    /// 两次样本之间（管理操作前的补采、重置）累积的增量，下一个样本一起报
    acc_d: BTreeMap<(String, String), (i64, i64)>,
    /// 已重置计数的账本备注（随批次上报，提交后清空）
    pub notes: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InboundInfo {
    pub port: u16,
    pub protocol: String,
    pub users: Vec<String>,
}

/// 解析 Xray 配置：非 api 入站的 tag → 端口/协议/用户，以及 api 入站地址
pub fn parse_xray_config(v: &Value) -> (BTreeMap<String, InboundInfo>, Option<String>) {
    let api_tag = v["api"]["tag"].as_str().unwrap_or("api");
    let mut m = BTreeMap::new();
    let mut api = None;
    for i in v["inbounds"].as_array().into_iter().flatten() {
        let tag = i["tag"].as_str().unwrap_or("");
        let port = i["port"].as_u64().filter(|p| *p > 0 && *p < 65536).map(|p| p as u16);
        if tag == api_tag {
            if let Some(p) = port {
                let l = i["listen"].as_str().unwrap_or("127.0.0.1");
                api = Some(if l.starts_with('/') { format!("unix:{l}") } else { format!("{}:{p}", if l == "0.0.0.0" || l.is_empty() { "127.0.0.1" } else { l }) });
            } else if let Some(l) = i["listen"].as_str().filter(|l| l.starts_with('/')) {
                api = Some(format!("unix:{l}"));
            }
            continue;
        }
        let (Some(port), false) = (port, tag.is_empty()) else { continue };
        let users = i["settings"]["clients"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c["email"].as_str().map(|e| e.to_string()))
            .collect();
        m.insert(tag.to_string(), InboundInfo { port, protocol: i["protocol"].as_str().unwrap_or("").to_string(), users });
    }
    (m, api)
}

impl Collector {
    pub fn new(cfg: XrayCfg, state_dir: Option<PathBuf>) -> Result<Collector, String> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_io().enable_time().build().map_err(|e| e.to_string())?;
        let bin_name = basename(&cfg.bin).to_string();
        let persist = state_dir.is_some();
        let mut c = Collector {
            ports: cfg.inbound_ports.iter().copied().collect(),
            cfg,
            rt,
            bin_name,
            state_path: state_dir.map(|d| d.join("xray.json")),
            boot_id: crate::hostmon::boot_id(),
            btime: btime(),
            tck: clk_tck(),
            xray: None,
            xui: None,
            version: None,
            version_for: None,
            core_id: None,
            prev: HashMap::new(),
            prev_at: None,
            cum: BTreeMap::new(),
            accum_since: util::round2(now_f64()),
            seed: None,
            seed_mtime: None,
            full_next: false,
            last: vec![],
            rates: BTreeMap::new(),
            last_ok: 0.0,
            last_err: None,
            conns: BTreeMap::new(),
            srcs: BTreeMap::new(),
            errors: 0,
            dirty: false,
            last_scan: None,
            stats_conn: StatsConn::default(),
            inbounds: BTreeMap::new(),
            cfg_api: None,
            api_tag: "api".into(),
            xcfg_mtime: None,
            xcfg_mtime_f: None,
            xcfg_checked: None,
            xcfg_err: None,
            access_log: None,
            blackholes: vec![],
            acc_d: BTreeMap::new(),
            notes: vec![],
        };
        c.reload_xray_config();
        c.load_state();
        if persist {
            c.refresh_procs();
            c.check_seed();
        }
        Ok(c)
    }

    fn load_state(&mut self) {
        let Some(p) = &self.state_path else { return };
        let Ok(s) = std::fs::read_to_string(p) else { return };
        let Ok(v) = serde_json::from_str::<Value>(&s) else {
            warn!("xray_state_corrupt", "path={}", crate::log::q(&p.display().to_string()));
            return;
        };
        if let (Some(core), Some(m)) = (v["core_id"].as_str(), v["counters"].as_object()) {
            self.core_id = Some(core.to_string());
            self.prev = m.iter().filter_map(|(k, v)| v.as_i64().map(|x| (k.clone(), x))).collect();
        }
        if let Some(m) = v["cum"].as_object() {
            self.cum = m.iter().filter_map(|(k, v)| v.as_u64().map(|x| (k.clone(), x))).collect();
        }
        if let Some(t) = v["accum_since"].as_f64() {
            self.accum_since = t;
        }
        if !v["seed"].is_null() {
            self.seed = Some(v["seed"].clone());
        }
        info!(
            "xray_state_loaded",
            "core_id={} counters={} cum_entries={} accum_since={} seeded={}",
            crate::log::q(self.core_id.as_deref().unwrap_or("")),
            self.prev.len(),
            self.cum.len(),
            self.accum_since,
            self.seed.is_some()
        );
    }

    fn save_state(&mut self) {
        let Some(p) = &self.state_path else { return };
        let prev: BTreeMap<&String, &i64> = self.prev.iter().collect();
        let v = json!({
            "core_id": self.core_id,
            "counters": prev,
            "cum": self.cum,
            "accum_since": self.accum_since,
            "seed": self.seed,
            "ts": util::round2(now_f64()),
        });
        if let Err(e) = util::write_atomic(p, v.to_string().as_bytes()) {
            warn!("xray_state_write_failed", "err={}", crate::log::q(&e.to_string()));
        }
        self.dirty = false;
    }

    /// 种子文件（切换时从 x-ui.db 导出）：
    /// `{"ts": <导出时刻 unix 秒>, "source": "...", "entries": [{"kind":"inbound","tag":"inbound-20001","up":N,"down":N}, {"kind":"user","tag":"<email>",...}]}`
    /// 内容（sha256）与已应用的不同才应用：累计值整体换成种子值，accum_since = 种子时刻。
    /// 当前 Xray 在种子时刻之后才启动 → 下一次读数全部计为增量（切换场景下精确）；
    /// 否则以当前读数为基线（种子时刻到现在之间的流量无法区分，记日志）。
    fn check_seed(&mut self) {
        let Some(path) = self.cfg.seed_file.clone() else { return };
        let Ok(md) = std::fs::metadata(&path) else { return };
        let mt = md.modified().ok();
        if mt.is_some() && mt == self.seed_mtime {
            return;
        }
        self.seed_mtime = mt;
        let Ok(raw) = std::fs::read(&path) else { return };
        let sha = sha256_hex(&raw);
        if self.seed.as_ref().and_then(|s| s["sha256"].as_str()) == Some(sha.as_str()) {
            return;
        }
        let v: Value = match serde_json::from_slice(&raw) {
            Ok(v) => v,
            Err(e) => {
                warn!("xray_seed_invalid", "path={} err={}", crate::log::q(&path), crate::log::q(&e.to_string()));
                return;
            }
        };
        let (Some(ts), Some(entries)) = (v["ts"].as_f64(), v["entries"].as_array()) else {
            warn!("xray_seed_invalid", "path={} err=missing_ts_or_entries", crate::log::q(&path));
            return;
        };
        let mut cum = BTreeMap::new();
        for e in entries {
            let (Some(k), Some(t)) = (e["kind"].as_str(), e["tag"].as_str()) else { continue };
            cum.insert(counter_name(k, t, true), e["up"].as_u64().unwrap_or(0));
            cum.insert(counter_name(k, t, false), e["down"].as_u64().unwrap_or(0));
        }
        let xray_start = self.xray.as_ref().map(|p| self.btime + p.start_ticks as f64 / self.tck);
        let after = xray_start.is_some_and(|s| s >= ts - 1.0);
        info!(
            "xray_seed_applied",
            "path={} sha256={} ts={ts} entries={} xray_started_after_seed={after} replaced_cum_entries={}",
            crate::log::q(&path),
            &sha[..16],
            entries.len(),
            self.cum.len()
        );
        if !after {
            warn!("xray_seed_gap", "note={}", crate::log::q("当前 Xray 早于种子时刻启动：以当前读数为基线，种子时刻到现在之间的流量不计入"));
        }
        self.cum = cum;
        self.accum_since = ts;
        self.full_next = after;
        if !after {
            // 以下一次读数为基线
            self.prev.clear();
            self.core_id = None;
        }
        self.seed = Some(json!({"sha256": sha, "ts": ts, "source": v["source"], "entries": entries.len(), "applied_at": util::round2(now_f64())}));
        self.save_state();
    }

    pub fn xray_exe(&self) -> Option<String> {
        self.xray.as_ref().map(|p| p.exe.clone()).filter(|e| e.starts_with('/'))
    }

    pub fn xray_running(&mut self) -> bool {
        self.refresh_procs();
        self.xray.is_some()
    }

    /// 管理操作改了配置之后立即重读
    pub fn force_reload_config(&mut self) {
        self.xcfg_checked = None;
        self.xcfg_mtime = None;
        self.reload_xray_config();
    }

    /// 管理操作前补采一次（增量进累计值，并留到下一个样本上报）
    pub fn fold_now(&mut self) -> Result<(), String> {
        let v = self.collect();
        if v["ok"].as_bool() == Some(true) {
            // 这次补采的增量不在任何样本里：放回 acc_d，随下一个样本上报
            for e in v["d"].as_array().into_iter().flatten() {
                if let (Some(k), Some(t)) = (e[0].as_str(), e[1].as_str()) {
                    let x = self.acc_d.entry((k.to_string(), t.to_string())).or_default();
                    x.0 += e[2].as_i64().unwrap_or(0);
                    x.1 += e[3].as_i64().unwrap_or(0);
                }
            }
            Ok(())
        } else {
            Err(v["err"].as_str().unwrap_or("Xray API 不可用").to_string())
        }
    }

    /// 重置指定计数（GetStats reset=true，一次一个）：先补采，再把清零前的值与基线之差计入累计值；累计值不清零。
    pub fn reset_counters(&mut self, names: &[String]) -> Result<Vec<Value>, String> {
        self.fold_now()?;
        let api = self.api();
        let mut out = vec![];
        for n in names {
            let v = get_stats_reset(&self.rt, &api, n)?;
            let prev = self.prev.get(n).copied().unwrap_or(0);
            let d = if v >= prev { v - prev } else { v.max(0) };
            let c = self.cum.entry(n.clone()).or_insert(0);
            *c = c.saturating_add(d as u64);
            if let Some((k, t, up)) = split_name(n) {
                let e = self.acc_d.entry((k.to_string(), t.to_string())).or_default();
                if up {
                    e.0 += d;
                } else {
                    e.1 += d;
                }
            }
            self.prev.insert(n.clone(), 0);
            out.push(json!({"name": n, "value_at_reset": v, "delta_folded": d, "cum": *c}));
        }
        self.dirty = true;
        self.save_state();
        Ok(out)
    }

    pub fn cum_of(&self, name: &str) -> u64 {
        self.cum.get(name).copied().unwrap_or(0)
    }

    pub fn inbound_users(&self, tag: &str) -> Vec<String> {
        self.inbounds.get(tag).map(|i| i.users.clone()).unwrap_or_default()
    }

    pub fn api(&self) -> String {
        self.cfg.api.clone().or_else(|| self.cfg_api.clone()).unwrap_or_else(|| "127.0.0.1:62789".into())
    }

    /// 每 60 s 检查一次 mtime，变了才重读（只读）
    fn reload_xray_config(&mut self) {
        let Some(path) = self.cfg.config.clone().filter(|p| !p.is_empty()) else { return };
        if self.xcfg_checked.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
            return;
        }
        self.xcfg_checked = Some(Instant::now());
        let md = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                let msg = format!("{path}: {e}");
                if self.xcfg_err.as_deref() != Some(&msg) {
                    warn!("xray_config_unreadable", "err={}", crate::log::q(&msg));
                }
                self.xcfg_err = Some(msg);
                return;
            }
        };
        let mt = md.modified().ok();
        if mt.is_some() && mt == self.xcfg_mtime {
            return;
        }
        match std::fs::read(&path).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice::<Value>(&b).map_err(|e| e.to_string())) {
            Ok(v) => {
                let (m, api) = parse_xray_config(&v);
                self.access_log = v["log"]["access"].as_str().map(|s| s.to_string());
                self.blackholes = v["outbounds"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|o| o["protocol"] == "blackhole")
                    .filter_map(|o| o["tag"].as_str().map(|t| t.to_string()))
                    .collect();
                self.api_tag = v["api"]["tag"].as_str().unwrap_or("api").to_string();
                for i in m.values() {
                    self.ports.insert(i.port);
                }
                info!("xray_config_loaded", "path={} inbounds={} api={}", crate::log::q(&path), m.len(), crate::log::q(api.as_deref().unwrap_or("")));
                self.inbounds = m;
                self.cfg_api = api;
                self.xcfg_mtime = mt;
                self.xcfg_mtime_f = mt.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as f64);
                self.xcfg_err = None;
            }
            Err(e) => {
                warn!("xray_config_invalid", "path={} err={}", crate::log::q(&path), crate::log::q(&e));
                self.xcfg_err = Some(e);
            }
        }
    }

    fn started_at(&self, p: &Proc) -> f64 {
        util::round2(self.btime + p.start_ticks as f64 / self.tck)
    }

    fn refresh_procs(&mut self) {
        // xray 不在/换代，或 x-ui 退出时立即重扫；x-ui 本来就不存在（切换后）时每 60 s 才扫一次
        let xui_due = match &self.xui {
            Some(_) => !still_alive(&self.xui),
            None => self.last_scan.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)),
        };
        if !still_alive(&self.xray) || xui_due {
            self.last_scan = Some(Instant::now());
            let (xr, xu) = scan(&self.bin_name, &self.cfg.proc_match, &self.cfg.xui_proc_name);
            if xr != self.xray {
                info!("xray_process", "pid={:?} prev={:?}", xr.as_ref().map(|p| p.pid), self.xray.as_ref().map(|p| p.pid));
            }
            self.xray = xr;
            self.xui = xu;
        }
        if self.cfg.version_exec && self.xray.is_some() && self.version_for != self.xray {
            self.version_for = self.xray.clone();
            let exe = self.xray.as_ref().map(|p| p.exe.clone()).filter(|e| e.starts_with('/')).unwrap_or_else(|| self.cfg.bin.clone());
            self.version = exec_version(&exe);
            info!("xray_version", "version={}", crate::log::q(self.version.as_deref().unwrap_or("unknown")));
        }
    }

    /// 一次采集：返回样本里的 xray 部分
    fn collect(&mut self) -> Value {
        self.refresh_procs();
        let t0 = Instant::now();
        self.reload_xray_config();
        let api = self.api();
        let r = query_stats_conn(&self.rt, &mut self.stats_conn, &api);
        let lat = util::round2(t0.elapsed().as_secs_f64() * 1000.0);
        let mut out = json!({"running": self.xray.is_some(), "latency_ms": lat});
        match r {
            Err(e) => {
                self.errors += 1;
                if self.last_err.as_deref() != Some(&e) {
                    warn!("xray_query_failed", "err={}", crate::log::q(&e));
                }
                self.last_err = Some(e.clone());
                out["ok"] = json!(false);
                out["err"] = json!(e);
            }
            Ok(stats) => {
                if self.last_err.take().is_some() {
                    info!("xray_query_recovered");
                }
                self.last_ok = now_f64();
                let now = Instant::now();
                let dt = self.prev_at.map(|t| now.duration_since(t).as_secs_f64()).unwrap_or(0.0);
                self.prev_at = Some(now);
                let core = self.xray.as_ref().map(|p| format!("{}:{}:{}", self.boot_id, p.pid, p.start_ticks)).unwrap_or_else(|| "unknown".into());
                let core_changed = self.core_id.as_deref() != Some(core.as_str());
                let have_baseline = self.core_id.is_some();
                let full = std::mem::take(&mut self.full_next);
                for (n, _) in &stats {
                    if let Some(("inbound", tag, _)) = split_name(n) {
                        if let Some(p) = tag.strip_prefix("inbound-").and_then(|x| x.parse::<u16>().ok()) {
                            self.ports.insert(p);
                        }
                    }
                }
                let mut deltas: BTreeMap<(String, String), (i64, i64)> = BTreeMap::new();
                let mut reset = false;
                let api_tag = self.api_tag.clone();
                for (n, v) in &stats {
                    let Some((k, tag, up)) = split_name(n) else { continue };
                    if k == "inbound" && tag == api_tag {
                        continue; // api 入站自己的流量（就是本进程的查询）
                    }
                    let (d, r) = delta_for(self.prev.get(n).copied(), *v, core_changed, have_baseline, full);
                    reset |= r;
                    let c = self.cum.entry(n.clone()).or_insert(0);
                    *c = c.saturating_add(d as u64);
                    let e = deltas.entry((k.to_string(), tag.to_string())).or_default();
                    if up {
                        e.0 += d;
                    } else {
                        e.1 += d;
                    }
                }
                if core_changed {
                    if have_baseline {
                        info!("xray_core_changed", "from={} to={}", crate::log::q(self.core_id.as_deref().unwrap_or("")), crate::log::q(&core));
                    }
                    self.core_id = Some(core);
                }
                for (k, (u, d)) in std::mem::take(&mut self.acc_d) {
                    let e = deltas.entry(k).or_default();
                    e.0 += u;
                    e.1 += d;
                }
                self.rates = if dt > 0.5 && have_baseline {
                    deltas.iter().map(|(k, (u, d))| (k.clone(), (util::round2(*u as f64 / dt), util::round2(*d as f64 / dt)))).collect()
                } else {
                    BTreeMap::new()
                };
                // 合并而不是替换：入口被停用（rmi）时计数可能暂时消失，重新出现时仍按原基线求差
                if core_changed {
                    self.prev.clear();
                }
                for (n, v) in &stats {
                    self.prev.insert(n.clone(), *v);
                }
                self.last = stats.into_iter().filter(|(n, _)| !matches!(split_name(n), Some(("inbound", t, _)) if t == api_tag)).collect();
                self.dirty = true;
                out["ok"] = json!(true);
                out["dt"] = json!(util::round2(dt));
                if !have_baseline && !full {
                    out["baseline"] = json!(true);
                }
                if reset {
                    out["reset"] = json!(true);
                }
                out["d"] = Value::Array(deltas.into_iter().filter(|(_, (u, d))| *u != 0 || *d != 0).map(|((k, t), (u, d))| json!([k, t, u, d])).collect());
            }
        }
        let (conns, srcs) = established_with_sources(&self.ports);
        self.conns = conns;
        self.srcs = srcs;
        out["conns"] = json!(self.conns.iter().map(|(p, n)| (p.to_string(), *n)).collect::<BTreeMap<_, _>>());
        out
    }
}

fn sha256_hex(b: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

impl Extension for Collector {
    fn on_sample(&mut self, _ts: f64) -> Option<Value> {
        Some(self.collect())
    }

    fn on_batch(&mut self, _pressure: f64) -> Vec<(&'static str, Value)> {
        self.check_seed();
        // 每批带一份每个条目的：累计值（真实来源）、Xray 原始值、最近速率
        let mut entries: BTreeMap<(String, String), [f64; 6]> = BTreeMap::new();
        for (n, v) in &self.cum {
            if let Some((k, t, up)) = split_name(n) {
                let e = entries.entry((k.to_string(), t.to_string())).or_default();
                e[if up { 0 } else { 1 }] = *v as f64;
            }
        }
        for (n, v) in &self.last {
            if let Some((k, t, up)) = split_name(n) {
                let e = entries.entry((k.to_string(), t.to_string())).or_default();
                e[if up { 2 } else { 3 }] = *v as f64;
            }
        }
        for ((k, t), (u, d)) in &self.rates {
            let e = entries.entry((k.clone(), t.clone())).or_default();
            e[4] = *u;
            e[5] = *d;
        }
        let proc_json = |p: &Option<Proc>| match p {
            Some(p) => json!({"running": true, "pid": p.pid, "started_at": self.started_at(p)}),
            None => json!({"running": false}),
        };
        let mut xr = proc_json(&self.xray);
        xr["version"] = json!(self.version);
        vec![("xray", json!({
            "srcs": self.srcs.iter().map(|(p, v)| (p.to_string(), json!(v))).collect::<serde_json::Map<String, Value>>(),
            "api": self.api(),
            "config": {"path": self.cfg.config, "mtime": self.xcfg_mtime_f, "error": self.xcfg_err, "inbounds": self.inbounds.iter().map(|(t, i)| json!({"tag": t, "port": i.port, "protocol": i.protocol, "users": i.users})).collect::<Vec<_>>()},
            "xray": xr,
            "xui": proc_json(&self.xui),
            "core_id": self.core_id,
            "last_ok": util::round2(self.last_ok),
            "errors": self.errors,
            "accum_since": self.accum_since,
            "seed": self.seed,
            "ports": self.ports.iter().copied().collect::<std::collections::BTreeSet<_>>(),
            // [kind, tag, cum_up, cum_down, raw_up, raw_down, rate_up_Bps, rate_down_Bps]
            "entries": entries.into_iter().map(|((k, t), e)| json!([k, t, e[0] as u64, e[1] as u64, e[2] as u64, e[3] as u64, e[4], e[5]])).collect::<Vec<_>>(),
        }))]
    }

    fn commit(&mut self) {
        if self.dirty {
            self.save_state();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc_varint(mut x: u64, out: &mut Vec<u8>) {
        loop {
            let b = (x & 0x7f) as u8;
            x >>= 7;
            if x == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
    }

    #[test]
    fn decode_roundtrip() {
        let mut msg = vec![];
        for (n, v) in [("inbound>>>inbound-20001>>>traffic>>>uplink", 12345u64), ("user>>>client-1>>>traffic>>>downlink", 7479566486)] {
            let mut s = vec![0x0a];
            enc_varint(n.len() as u64, &mut s);
            s.extend_from_slice(n.as_bytes());
            s.push(0x10);
            enc_varint(v, &mut s);
            msg.push(0x0a);
            enc_varint(s.len() as u64, &mut msg);
            msg.extend_from_slice(&s);
        }
        let got = decode_query_stats(&msg).unwrap();
        assert_eq!(got[0], ("inbound>>>inbound-20001>>>traffic>>>uplink".into(), 12345));
        assert_eq!(got[1].1, 7479566486);
        let mut framed = vec![0, 0, 0, 0, msg.len() as u8];
        framed.extend_from_slice(&msg);
        assert_eq!(grpc_unframe(&framed).unwrap(), &msg[..]);
    }

    #[test]
    fn delta_rules() {
        // 首次无基线：只记基线
        assert_eq!(delta_for(None, 100, true, false, false), (0, false));
        // 种子之后启动的 Xray：全部计入
        assert_eq!(delta_for(None, 100, true, false, true), (100, false));
        // 正常增长
        assert_eq!(delta_for(Some(100), 150, false, true, false), (50, false));
        // 回退 = 重置
        assert_eq!(delta_for(Some(100), 30, false, true, false), (30, true));
        // Xray 重启（纪元变化）
        assert_eq!(delta_for(Some(100), 500, true, true, false), (500, true));
        // 运行中新增的计数
        assert_eq!(delta_for(None, 7, false, true, false), (7, false));
    }

    #[test]
    fn proc_addrs() {
        assert_eq!(proc_ip("0100007F").unwrap().to_string(), "127.0.0.1");
        assert_eq!(proc_ip("0000000000000000FFFF00000900A8C0").unwrap().to_string(), "192.168.0.9");
        assert_eq!(proc_ip("B80D0120000000000000000001000000").unwrap().to_string(), "2001:db8::1");
    }

    #[test]
    fn names() {
        assert_eq!(split_name("user>>>a@b>>>traffic>>>uplink"), Some(("user", "a@b", true)));
        assert_eq!(split_name("user>>>a>>>online"), None);
    }
}
