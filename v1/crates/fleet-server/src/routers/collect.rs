//! 采集：每 30 秒经隧道登录每台路由（并发最多 8 台，一台最多 25 秒），跑一段只读的采集脚本（collect.uc，经 stdin 交给 `sh -s`），
//! 把它回的 JSON 拼成快照（接口的 detail）存进 router_status，指标进 router_metric / router_exit_metric（30 秒一点）。
//!
//! - 速率：WAN 等接口按 /proc/net/dev 两次读数之差、代理总流量按 mihomo 的 uploadTotal / downloadTotal 两次之差，在中控算
//!   （前一次读数只在内存里，中控重启后第一轮没有速率）；各出口的流量由路由上按连接累加（见 collect.uc），中控除以间隔。
//! - 延迟：每 5 分钟让 mihomo 经每个出口请求一次 generate_204（/group/GLOBAL/delay），这是客户现场经出口的真实可用性；
//!   结果进快照与那一点的 router_exit_metric，连续失败次数（fail_streak）随快照存着，告警 router_exits_down 看它。
//! - 出口与 fleet 入口：配置文件的 sha256 变了才取回来解析（见 clash.rs），解析结果只在内存里。
//! - 主机公钥：还没记录时（刚接入）先经隧道 keyscan 记下（只有拿到隧道私钥的才能占住这个端口），之后严格核对。

use super::clash::{self, Entry, EntryRef, Proxy};
use super::tunnel;
use crate::state::{AppState, Event};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

const PARALLEL: usize = 8;
/// 一台路由一轮最多等这么久（测延迟的那一轮 mihomo 要等最慢的出口 5 秒）
const TIMEOUT: Duration = Duration::from_secs(25);
/// 多久测一次全部出口的延迟
const DELAY_EVERY_S: i64 = 300;
pub const DELAY_URL: &str = "https://www.gstatic.com/generate_204";

/// 交给 `sh -s` 的采集脚本：整段包在函数里（sh 读完整个函数才开始执行，命令读 stdin 吃不到后面的脚本），ucode 程序写到 tmpfs 再跑
const WRAPPER: &str = r#"
main() {
umask 077
mkdir -p /tmp/fleet-collect && chmod 700 /tmp/fleet-collect || exit 1
if ! command -v ucode >/dev/null 2>&1; then echo '{"v":1,"error":"路由上没有 ucode（OpenWrt 23.05 以后都有），采集不了"}'; exit 0; fi
cat > /tmp/fleet-collect/collect.uc <<'FLEET_UC'
@@PROGRAM@@
FLEET_UC
FLEET_DELAY="$DELAY" FLEET_DELAY_URL="$DELAY_URL" exec ucode /tmp/fleet-collect/collect.uc
}
main </dev/null
"#;

pub fn script(delay: bool) -> String {
    crate::jobs::scripts::vars(&[("DELAY", if delay { "1" } else { "0" }), ("DELAY_URL", &delay_url_param())]) + &WRAPPER.replace("@@PROGRAM@@", include_str!("collect.uc").trim_end())
}

/// 测延迟的地址按查询参数编码（mihomo 的 `?url=`）：替换节点后测出口延迟也用它（routers::replace）
pub fn delay_url_param() -> String {
    DELAY_URL.bytes().map(|b| if b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_' { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

// ── 快照（接口的 detail，存在 router_status.data）──────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub system: System,
    pub interfaces: Vec<Iface>,
    pub openclash: OpenClash,
    pub exits: Vec<Exit>,
    pub clients: Vec<Client>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub collected_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct System {
    pub os: Option<String>,
    pub kernel: Option<String>,
    pub cpu_model: Option<String>,
    pub cores: Option<i64>,
    pub cpu_pct: Option<f64>,
    /// 1 / 5 / 15 分钟负载
    pub load: Vec<Option<f64>>,
    pub procs: Option<i64>,
    pub uptime_s: Option<i64>,
    pub mem_used: Option<i64>,
    pub mem_total: Option<i64>,
    /// /overlay（可写层），没有就是 /
    pub disk_path: Option<String>,
    pub disk_used: Option<i64>,
    pub disk_total: Option<i64>,
    pub conntrack: Option<i64>,
    pub conntrack_max: Option<i64>,
    pub temp_c: Option<f64>,
    /// 中控时间 − 路由时间（秒），正数 = 路由时钟慢（同机器的 clock_skew_ms）
    pub clock_skew_s: Option<i64>,
    /// OpenClash 核心进程的常驻内存（字节）
    pub clash_rss: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Iface {
    pub name: String,
    pub device: Option<String>,
    pub up: bool,
    pub proto: Option<String>,
    pub ip: Option<String>,
    pub uptime_s: Option<i64>,
    pub rx_bps: Option<i64>,
    pub tx_bps: Option<i64>,
    pub rx_bytes: Option<i64>,
    pub tx_bytes: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenClash {
    pub enabled: bool,
    pub running: bool,
    pub version: Option<String>,
    pub mode: Option<String>,
    pub config_path: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub config_mtime: Option<OffsetDateTime>,
    pub conns: Option<i64>,
    pub up_bps: Option<i64>,
    pub down_bps: Option<i64>,
    /// 核心在跑但 API 调不通时的原因
    pub error: Option<String>,
    /// 配置文件取不回来、解析不了时的原因（出口就对不上入口）
    pub config_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Exit {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub server: Option<String>,
    pub port: Option<u16>,
    pub entry: Option<EntryRef>,
    /// ok / drift / unmanaged
    pub sync: String,
    pub alive: Option<bool>,
    pub delay_ms: Option<i64>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub delay_at: Option<OffsetDateTime>,
    /// 连续几次延迟测试不通（两次起告警）
    pub fail_streak: u32,
    pub conns: i64,
    pub up_bps: Option<i64>,
    pub down_bps: Option<i64>,
    pub clients: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Client {
    pub ip: String,
    pub mac: Option<String>,
    pub hostname: Option<String>,
    pub online: bool,
    pub conns: i64,
    /// 经 OpenClash 的速率（bit/s，按 mihomo 连接的来源 IP 汇总，取差同出口）：没有连接为 0；路由上第一次采集（没有上一次的读数）、mihomo API 调不通时算不出为 None（上一次的读数存在路由上，中控重启不影响）
    pub down_bps: Option<i64>,
    pub up_bps: Option<i64>,
    pub exits: Vec<ClientExit>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientExit {
    pub name: String,
    pub conns: i64,
}

/// 出口算不算可用：没测过、测通，或只测不通 1 次（同告警，连续两次不通才算不可用，见 routers/alerts.rs）
pub fn usable(e: &Exit) -> bool { e.alive != Some(false) || e.fail_streak < 2 }

/// 列表里的摘要（由快照算，不另存）
pub fn summary(s: &Snapshot) -> Value {
    let wan = s.interfaces.iter().find(|i| i.name == "wan");
    let y = &s.system;
    serde_json::json!({
        "uptime_s": y.uptime_s, "cpu_pct": y.cpu_pct, "load1": y.load.first().copied().flatten(), "cores": y.cores,
        "mem_used": y.mem_used, "mem_total": y.mem_total, "disk_used": y.disk_used, "disk_total": y.disk_total,
        "conntrack": y.conntrack, "conntrack_max": y.conntrack_max, "temp_c": y.temp_c,
        "wan": wan.map(|w| serde_json::json!({"up": w.up, "ip": w.ip, "rx_bps": w.rx_bps, "tx_bps": w.tx_bps})),
        "openclash": {
            "enabled": s.openclash.enabled, "running": s.openclash.running, "version": s.openclash.version, "mode": s.openclash.mode,
            "conns": s.openclash.conns, "up_bps": s.openclash.up_bps, "down_bps": s.openclash.down_bps,
        },
        "exits": {
            "total": s.exits.len(),
            "alive": s.exits.iter().filter(|e| usable(e)).count(),
            "managed": s.exits.iter().filter(|e| e.sync != "unmanaged").count(),
            "drift": s.exits.iter().filter(|e| e.sync == "drift").count(),
        },
        "clients": s.clients.iter().filter(|c| c.online).count(),
    })
}

// ── 由采集脚本的输出拼快照 ──────────────────────────────────────────────

/// 上一次的计数器读数（只在内存里）：算速率用
#[derive(Debug, Clone, Default)]
pub struct Prev {
    pub at: Option<OffsetDateTime>,
    pub dev: HashMap<String, (f64, f64)>,
    pub totals: Option<(f64, f64)>,
}

/// 一行 router_metric
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MetricRow {
    pub cpu_pct: Option<f64>,
    pub load1: Option<f64>,
    pub mem_used: Option<i64>,
    pub mem_total: Option<i64>,
    pub disk_used: Option<i64>,
    pub disk_total: Option<i64>,
    pub conntrack: Option<i64>,
    pub conntrack_max: Option<i64>,
    pub temp_c: Option<f64>,
    pub wan_rx_bps: Option<i64>,
    pub wan_tx_bps: Option<i64>,
    pub proxy_up_bps: Option<i64>,
    pub proxy_down_bps: Option<i64>,
    pub conns: Option<i64>,
    pub clients: i64,
    pub exits_total: i64,
    pub exits_alive: i64,
}

/// 一行 router_exit_metric
#[derive(Debug, Clone, PartialEq)]
pub struct ExitRow {
    pub exit: String,
    pub alive: Option<bool>,
    pub delay_ms: Option<i64>,
    pub conns: i64,
    pub up_bps: Option<i64>,
    pub down_bps: Option<i64>,
    pub clients: i64,
}

fn f(v: &Value, k: &str) -> Option<f64> { v.get(k).and_then(Value::as_f64).filter(|x| x.is_finite()) }
fn i(v: &Value, k: &str) -> Option<i64> { f(v, k).map(|x| x.round() as i64) }
fn s(v: &Value, k: &str) -> Option<String> { v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(String::from) }
fn ts(v: &Value, k: &str) -> Option<OffsetDateTime> {
    match v.get(k)? {
        Value::String(t) => OffsetDateTime::parse(t, &time::format_description::well_known::Rfc3339).ok(),
        n => n.as_f64().and_then(|x| OffsetDateTime::from_unix_timestamp(x as i64).ok()),
    }
}

/// 两次读数之差 → bit/s；计数器变小（重启、回绕）或间隔不对时为 None
fn rate(cur: Option<f64>, prev: Option<f64>, dt: f64) -> Option<i64> {
    let (c, p) = (cur?, prev?);
    (c >= p && dt > 0.5).then(|| ((c - p) * 8.0 / dt).round() as i64)
}

/// 这一轮拼出来的东西
pub struct Built {
    pub snap: Snapshot,
    pub prev: Prev,
    pub metric: MetricRow,
    pub exits: Vec<ExitRow>,
}

/// 由采集脚本的输出（`raw`）、上一次的计数器与快照、配置里的出口、fleet 入口拼出这一轮的快照与指标。`now` 是中控收到的时刻
pub fn build(raw: &Value, prev: &Prev, last: Option<&Snapshot>, config: Option<&[Proxy]>, entries: &[Entry], now: OffsetDateTime) -> Built {
    let dt = prev.at.map(|t| (now - t).as_seconds_f64()).unwrap_or(0.0);
    // 系统
    let mem_total = i(raw, "mem_total");
    let disk = raw.get("disk").cloned().unwrap_or(Value::Null);
    let oc = raw.get("oc").cloned().unwrap_or(Value::Null);
    let (du, da) = (i(&disk, "used"), i(&disk, "avail"));
    let system = System {
        os: s(raw, "os"),
        kernel: s(raw, "kernel"),
        cpu_model: s(raw, "cpu_model"),
        cores: i(raw, "cores").filter(|c| *c > 0),
        cpu_pct: f(raw, "cpu_pct").map(|x| x.clamp(0.0, 100.0)),
        load: raw.get("load").and_then(Value::as_array).map(|a| a.iter().map(|x| x.as_f64()).collect()).unwrap_or_default(),
        procs: i(raw, "procs"),
        uptime_s: i(raw, "uptime"),
        mem_used: mem_total.zip(i(raw, "mem_avail")).map(|(t, a)| (t - a).max(0)),
        mem_total,
        disk_path: s(&disk, "path"),
        disk_used: du,
        disk_total: du.zip(da).map(|(u, a)| u + a),
        conntrack: i(raw, "conntrack"),
        conntrack_max: i(raw, "conntrack_max"),
        temp_c: f(raw, "temp_c"),
        clock_skew_s: i(raw, "now").map(|t| now.unix_timestamp() - t),
        clash_rss: i(&oc, "rss"),
    };
    // 接口：按设备的计数器算速率
    let dev: HashMap<String, (f64, f64)> = raw
        .get("dev")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    let a = v.as_array()?;
                    Some((k.clone(), (a.first()?.as_f64()?, a.get(1)?.as_f64()?)))
                })
                .collect()
        })
        .unwrap_or_default();
    let interfaces: Vec<Iface> = raw
        .get("ifaces")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|x| {
            // 回环接口不是网络口，不列
            let name = s(x, "name").filter(|n| n != "loopback")?;
            let device = s(x, "device");
            let cur = device.as_ref().and_then(|d| dev.get(d)).copied();
            let old = device.as_ref().and_then(|d| prev.dev.get(d)).copied();
            Some(Iface {
                name,
                up: x.get("up").and_then(Value::as_bool).unwrap_or(false),
                proto: s(x, "proto"),
                ip: s(x, "ip"),
                uptime_s: i(x, "uptime"),
                rx_bps: rate(cur.map(|c| c.0), old.map(|o| o.0), dt),
                tx_bps: rate(cur.map(|c| c.1), old.map(|o| o.1), dt),
                rx_bytes: cur.map(|c| c.0 as i64),
                tx_bytes: cur.map(|c| c.1 as i64),
                device,
            })
        })
        .collect();
    // OpenClash 与代理总流量
    let totals = f(&oc, "up_total").zip(f(&oc, "down_total"));
    let openclash = OpenClash {
        enabled: oc.get("enabled").and_then(Value::as_bool).unwrap_or(false),
        running: oc.get("running").and_then(Value::as_bool).unwrap_or(false),
        version: s(&oc, "version"),
        mode: s(&oc, "mode"),
        config_path: s(&oc, "config_path"),
        config_mtime: ts(&oc, "config_mtime"),
        conns: i(&oc, "conns"),
        up_bps: rate(totals.map(|t| t.0), prev.totals.map(|t| t.0), dt),
        down_bps: rate(totals.map(|t| t.1), prev.totals.map(|t| t.1), dt),
        error: s(&oc, "api_error").map(|e| format!("mihomo 的 API 调不通：{e}")),
        config_error: None,
    };
    let exits = exits(&oc, last, config, entries, now);
    let clients = clients(raw, &oc, &interfaces);
    let wan = interfaces.iter().find(|w| w.name == "wan");
    let metric = MetricRow {
        cpu_pct: system.cpu_pct,
        load1: system.load.first().copied().flatten(),
        mem_used: system.mem_used,
        mem_total: system.mem_total,
        disk_used: system.disk_used,
        disk_total: system.disk_total,
        conntrack: system.conntrack,
        conntrack_max: system.conntrack_max,
        temp_c: system.temp_c,
        wan_rx_bps: wan.and_then(|w| w.rx_bps),
        wan_tx_bps: wan.and_then(|w| w.tx_bps),
        proxy_up_bps: openclash.up_bps,
        proxy_down_bps: openclash.down_bps,
        conns: openclash.conns,
        clients: clients.iter().filter(|c| c.online).count() as i64,
        exits_total: exits.len() as i64,
        exits_alive: exits.iter().filter(|e| usable(e)).count() as i64,
    };
    let tested = oc.pointer("/delay/ok").and_then(Value::as_bool) == Some(true);
    let exit_rows = exits
        .iter()
        .map(|e| ExitRow {
            exit: e.name.clone(),
            alive: if tested { e.alive } else { None },
            delay_ms: if tested { e.delay_ms } else { None },
            conns: e.conns,
            up_bps: e.up_bps,
            down_bps: e.down_bps,
            clients: e.clients,
        })
        .collect();
    let snap = Snapshot { system, interfaces, openclash, exits, clients, collected_at: Some(now) };
    Built { snap, prev: Prev { at: Some(now), dev, totals }, metric, exits: exit_rows }
}

/// 出口：配置里的（按配置的顺序）加上 mihomo /proxies 里另有的叶子出口（按名字）。
/// 这一轮测了延迟的按测的结果更新 alive / delay_ms / 连续失败次数，没测的沿用上一份快照（没有上一份时用 mihomo 自己记的）
fn exits(oc: &Value, last: Option<&Snapshot>, config: Option<&[Proxy]>, entries: &[Entry], now: OffsetDateTime) -> Vec<Exit> {
    let api: Vec<&Value> = oc.get("proxies").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
    let api_by: HashMap<&str, &Value> = api.iter().filter_map(|p| Some((p.get("name")?.as_str()?, *p))).collect();
    let mut names: Vec<String> = config.unwrap_or_default().iter().map(|p| p.name.clone()).collect();
    let mut extra: Vec<String> = api_by.keys().filter(|n| !names.iter().any(|m| m == *n)).map(|n| n.to_string()).collect();
    extra.sort();
    names.extend(extra);
    let mut seen = HashSet::new();
    names.retain(|n| seen.insert(n.clone()));
    let cfg_by: HashMap<&str, &Proxy> = config.unwrap_or_default().iter().map(|p| (p.name.as_str(), p)).collect();
    let last_by: HashMap<&str, &Exit> = last.map(|l| l.exits.iter().map(|e| (e.name.as_str(), e)).collect()).unwrap_or_default();
    let delay = oc.get("delay").filter(|d| d.get("ok").and_then(Value::as_bool) == Some(true));
    let results = delay.and_then(|d| d.get("results"));
    let dt = f(oc, "conn_dt").filter(|x| *x > 0.0);
    names
        .into_iter()
        .map(|name| {
            let cfg = cfg_by.get(name.as_str()).copied();
            let api = api_by.get(name.as_str()).copied();
            let prev = last_by.get(name.as_str()).copied();
            let (entry, sync) = match cfg {
                Some(p) => clash::match_entry(p, entries),
                None => (None, "unmanaged"),
            };
            let (alive, delay_ms, delay_at, fail_streak) = match results {
                Some(r) => match r.get(&name).and_then(Value::as_i64).filter(|d| *d > 0) {
                    Some(d) => (Some(true), Some(d), Some(now), 0),
                    None => (Some(false), None, Some(now), prev.map_or(0, |p| p.fail_streak) + 1),
                },
                None => match prev {
                    Some(p) => (p.alive, p.delay_ms, p.delay_at, p.fail_streak),
                    None => (
                        api.and_then(|a| a.get("alive")).and_then(Value::as_bool),
                        api.and_then(|a| a.get("delay")).and_then(Value::as_i64).filter(|d| *d > 0),
                        api.and_then(|a| ts(a, "at")),
                        0,
                    ),
                },
            };
            let st = oc.get("exits").and_then(|e| e.get(&name)).cloned().unwrap_or(Value::Null);
            let bps = |k: &str| dt.and_then(|dt| f(&st, k).map(|b| (b * 8.0 / dt).round() as i64));
            Exit {
                kind: api.and_then(|a| s(a, "type")).or_else(|| cfg.and_then(|c| c.kind.clone())),
                server: cfg.and_then(|c| c.server.clone()),
                port: cfg.and_then(|c| c.port),
                entry,
                sync: sync.to_string(),
                alive,
                delay_ms,
                delay_at,
                fail_streak,
                conns: i(&st, "conns").unwrap_or(0),
                up_bps: bps("up"),
                down_bps: bps("down"),
                clients: i(&st, "clients").unwrap_or(0),
                name,
            }
        })
        .collect()
}

/// 客户端：DHCP 租约（主机名、MAC）+ 邻居表（REACHABLE / STALE / DELAY / PROBE 算在线；WAN 口那一侧的不算：旁路由的 WAN 就是 br-lan，
/// 整个家里的局域网都在邻居表里，它的客户端靠下一条）+ mihomo 连接里的来源 IP（有连接即在线，速率由它算）。在线的在前，按连接数
fn clients(raw: &Value, oc: &Value, ifaces: &[Iface]) -> Vec<Client> {
    let wan_dev: HashSet<&str> = ifaces.iter().filter(|i| i.name == "wan" || i.name == "wan6").filter_map(|i| i.device.as_deref()).collect();
    let mut by: BTreeMap<String, Client> = BTreeMap::new();
    let str_at = |a: &Value, n: usize| a.get(n).and_then(Value::as_str).map(str::to_string).filter(|s| !s.is_empty());
    for l in raw.get("leases").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
        let Some(ip) = str_at(l, 0) else { continue };
        let c = by.entry(ip.clone()).or_insert_with(|| Client { ip, ..Client::default() });
        c.mac = str_at(l, 1);
        c.hostname = str_at(l, 2);
    }
    for n in raw.get("neigh").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
        let (Some(ip), dev) = (str_at(n, 0), str_at(n, 1)) else { continue };
        if dev.as_deref().is_some_and(|d| wan_dev.contains(d)) {
            continue;
        }
        let state = str_at(n, 3).unwrap_or_default();
        if !matches!(state.as_str(), "REACHABLE" | "STALE" | "DELAY" | "PROBE" | "PERMANENT") && !by.contains_key(&ip) {
            continue;
        }
        let c = by.entry(ip.clone()).or_insert_with(|| Client { ip, ..Client::default() });
        if c.mac.is_none() {
            c.mac = str_at(n, 2);
        }
        c.online |= matches!(state.as_str(), "REACHABLE" | "STALE" | "DELAY" | "PROBE" | "PERMANENT");
    }
    let dt = f(oc, "conn_dt").filter(|x| *x > 0.0);
    let bps = |v: &Value, k: &str| dt.and_then(|dt| f(v, k).map(|b| (b * 8.0 / dt).round() as i64));
    if let Some(m) = oc.get("clients").and_then(Value::as_object) {
        for (ip, v) in m {
            if ip.starts_with("127.") || ip == "::1" {
                continue;
            }
            let c = by.entry(ip.clone()).or_insert_with(|| Client { ip: ip.clone(), ..Client::default() });
            c.conns = i(v, "conns").unwrap_or(0);
            c.online |= c.conns > 0;
            c.down_bps = bps(v, "down");
            c.up_bps = bps(v, "up");
            let mut ex: Vec<ClientExit> = v
                .get("exits")
                .and_then(Value::as_object)
                .map(|e| e.iter().map(|(n, x)| ClientExit { name: n.clone(), conns: x.as_i64().unwrap_or(0) }).collect())
                .unwrap_or_default();
            ex.sort_by(|a, b| b.conns.cmp(&a.conns).then_with(|| a.name.cmp(&b.name)));
            c.exits = ex;
        }
    }
    let mut v: Vec<Client> = by.into_values().collect();
    for c in v.iter_mut().filter(|c| c.down_bps.is_none()) {
        (c.down_bps, c.up_bps) = (dt.map(|_| 0), dt.map(|_| 0));
    }
    v.sort_by(|a, b| b.online.cmp(&a.online).then_with(|| b.conns.cmp(&a.conns)).then_with(|| ip_key(&a.ip).cmp(&ip_key(&b.ip))));
    v.truncate(500);
    v
}

fn ip_key(ip: &str) -> (u8, u128) {
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(a)) => (4, u32::from(a) as u128),
        Ok(std::net::IpAddr::V6(a)) => (6, u128::from(a)),
        Err(_) => (9, 0),
    }
}

// ── 采集循环 ──────────────────────────────────────────────────────────────

/// 每台路由在两轮之间要记住的
#[derive(Default)]
pub struct Mem {
    prev: Prev,
    last: Option<Snapshot>,
    /// 配置文件的 sha256 与解析出的出口（解析不了时是原因）
    config: Option<(String, Result<Vec<Proxy>, String>)>,
    last_delay: Option<OffsetDateTime>,
}

#[derive(sqlx::FromRow)]
struct Target {
    id: Uuid,
    tunnel_port: i32,
    has_key: bool,
    host_key: Option<String>,
    data: Option<Value>,
}

enum Outcome {
    /// 还没发隧道密钥（刚建、刚重新生成接入命令）：不去连
    Pending,
    Ok(Box<Built>),
    Fail(String),
}

/// 每个进程只做一次的收尾：上次没跑完的一键部署（中控重启、部署中断）标为失败
static STARTED: AtomicBool = AtomicBool::new(false);

/// 请采集循环马上采某几台路由：作业刚改完（或换回）它们的配置，快照别等下一轮 30 秒——紧接着发起的预览要按新配置算出口。
/// 请求带序号，循环每轮开始时把排着的一起取走，这一轮做完记下做到了哪个序号（`wait` 等它）
pub struct Refresh {
    due: std::sync::Mutex<(u64, HashSet<Uuid>)>,
    wake: tokio::sync::Notify,
    served: tokio::sync::watch::Sender<u64>,
    /// 这个 AppState 的采集循环在跑（测试里一般不跑：不等）
    running: AtomicBool,
}

impl Default for Refresh {
    fn default() -> Self {
        Refresh { due: Default::default(), wake: Default::default(), served: tokio::sync::watch::channel(0).0, running: AtomicBool::new(false) }
    }
}

impl Refresh {
    /// 马上采这几台，等这一轮做完（最多一台的采集时限多一点）；采集循环没在跑就不等
    pub async fn now(&self, ids: impl IntoIterator<Item = Uuid>) {
        let seq = {
            let mut d = self.due.lock().unwrap();
            d.0 += 1;
            d.1.extend(ids);
            d.0
        };
        self.wake.notify_one();
        if self.running.load(Ordering::Relaxed) {
            let mut rx = self.served.subscribe();
            let _ = tokio::time::timeout(TIMEOUT + Duration::from_secs(5), rx.wait_for(|s| *s >= seq)).await;
        }
    }

    fn take(&self) -> (u64, Vec<Uuid>) {
        let mut d = self.due.lock().unwrap();
        (d.0, d.1.drain().collect())
    }
}

pub async fn run_forever(st: AppState) {
    if !STARTED.swap(true, Ordering::Relaxed) {
        if let Err(e) = super::deploy::mark_interrupted(&st).await {
            tracing::warn!("标记中断的一键部署失败：{e:#}");
        }
    }
    if let Err(e) = tokio::fs::create_dir_all(&st.cfg.routers.control_dir).await {
        tracing::warn!("建 {} 失败：{e}", st.cfg.routers.control_dir.display());
    }
    let mut mem: HashMap<Uuid, Mem> = HashMap::new();
    let mut alerts = super::alerts::AlertState::default();
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let refresh = &st.routers_refresh;
    refresh.running.store(true, Ordering::Relaxed);
    loop {
        let full = tokio::select! {
            _ = tick.tick() => true,
            _ = refresh.wake.notified() => false,
        };
        // 排着的立即采集一起取走：整轮会采到它们，否则只采它们
        let (seq, due) = refresh.take();
        if full {
            // 文件被删、被改、上次写失败：每轮按库对齐一次（内容没变不写）
            if let Err(e) = super::sync_files(&st).await {
                tracing::warn!("对齐隧道公钥清单 / router_known_hosts 失败：{e:#}");
            }
        }
        if full || !due.is_empty() {
            if let Err(e) = round_of(&st, &mut mem, (!full).then_some(due.as_slice())).await {
                tracing::error!("软路由采集失败：{e:#}");
            }
        }
        refresh.served.send_replace(seq);
        if full {
            if let Err(e) = super::alerts::evaluate(&st, &mut alerts, OffsetDateTime::now_utc()).await {
                tracing::error!("软路由告警评估失败：{e:#}");
            }
        }
    }
}

/// 采集一轮（全部路由并发，最多 8 台同时）。有路由的状态或摘要变了就发一次实时事件
pub async fn round(st: &AppState, mem: &mut HashMap<Uuid, Mem>) -> anyhow::Result<()> { round_of(st, mem, None).await }

/// 采集一轮：`only` 给了就只采这几台（立即采集，见 `Refresh`）
async fn round_of(st: &AppState, mem: &mut HashMap<Uuid, Mem>, only: Option<&[Uuid]>) -> anyhow::Result<()> {
    let targets: Vec<Target> = sqlx::query_as(
        "select r.id, r.tunnel_port, r.tunnel_pubkey is not null has_key, r.host_key, s.data
         from router r left join router_status s on s.router_id = r.id where $1::uuid[] is null or r.id = any($1) order by r.tunnel_port",
    )
    .bind(only)
    .fetch_all(&st.db)
    .await?;
    if only.is_none() {
        let alive: HashSet<Uuid> = targets.iter().map(|t| t.id).collect();
        mem.retain(|id, _| alive.contains(id));
    }
    if targets.is_empty() {
        return Ok(());
    }
    let entries: Arc<Vec<Entry>> = Arc::new(sqlx::query_as(clash::ENTRIES).fetch_all(&st.db).await?);
    let jobs: Vec<_> = targets
        .into_iter()
        .map(|t| {
            let mut m = mem.remove(&t.id).unwrap_or_default();
            if m.last.is_none() {
                m.last = t.data.clone().and_then(|d| serde_json::from_value(d).ok());
            }
            let (st, entries) = (st.clone(), entries.clone());
            async move {
                let id = t.id;
                let changed = match one(&st, &t, &mut m, &entries).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(router = %id, "写采集结果失败：{e:#}");
                        false
                    }
                };
                (id, m, changed)
            }
        })
        .collect();
    let done: Vec<(Uuid, Mem, bool)> = futures::stream::iter(jobs).buffer_unordered(PARALLEL).collect().await;
    let mut changed = false;
    for (id, m, c) in done {
        mem.insert(id, m);
        changed |= c;
    }
    if changed {
        st.emit(Event::Routers);
    }
    Ok(())
}

/// 采集一台并落库。返回状态或摘要是否变了
async fn one(st: &AppState, t: &Target, m: &mut Mem, entries: &[Entry]) -> anyhow::Result<bool> {
    let now = OffsetDateTime::now_utc();
    let outcome = match tokio::time::timeout(TIMEOUT, collect(st, t, m, entries, now)).await {
        Ok(o) => o,
        Err(_) => Outcome::Fail(format!("采集超过 {} 秒没有完成", TIMEOUT.as_secs())),
    };
    let before: Option<(bool, Option<String>)> =
        sqlx::query_as("select reachable, error from router_status where router_id = $1").bind(t.id).fetch_optional(&st.db).await?;
    match outcome {
        Outcome::Pending => {
            sqlx::query(
                "insert into router_status (router_id, ts, reachable, error) values ($1, now(), false, null)
                 on conflict (router_id) do update set ts = now(), reachable = false, error = null",
            )
            .bind(t.id)
            .execute(&st.db)
            .await?;
            Ok(before.is_none_or(|b| b.0 || b.1.is_some()))
        }
        Outcome::Fail(err) => {
            sqlx::query(
                "insert into router_status (router_id, ts, reachable, error) values ($1, now(), false, $2)
                 on conflict (router_id) do update set ts = now(), reachable = false, error = $2",
            )
            .bind(t.id)
            .bind(&err)
            .execute(&st.db)
            .await?;
            Ok(before.is_none_or(|b| b.0 || b.1.as_deref() != Some(err.as_str())))
        }
        Outcome::Ok(b) => {
            let mut tx = st.db.begin().await?;
            // 路由刚被删掉：外键挡住，什么都不写
            let exists: Option<i32> = sqlx::query_scalar("select 1 from router where id = $1 for share").bind(t.id).fetch_optional(&mut *tx).await?;
            if exists.is_none() {
                return Ok(false);
            }
            sqlx::query(
                "insert into router_status (router_id, ts, reachable, error, data, last_ok_at) values ($1, $2, true, null, $3, $2)
                 on conflict (router_id) do update set ts = $2, reachable = true, error = null, data = $3, last_ok_at = $2",
            )
            .bind(t.id)
            .bind(now)
            .bind(serde_json::to_value(&b.snap)?)
            .execute(&mut *tx)
            .await?;
            let r = &b.metric;
            sqlx::query(
                "insert into router_metric (router_id, ts, cpu_pct, load1, mem_used, mem_total, disk_used, disk_total, conntrack, conntrack_max, temp_c,
                    wan_rx_bps, wan_tx_bps, proxy_up_bps, proxy_down_bps, conns, clients, exits_total, exits_alive)
                 values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19) on conflict do nothing",
            )
            .bind(t.id)
            .bind(now)
            .bind(r.cpu_pct.map(|x| x as f32))
            .bind(r.load1.map(|x| x as f32))
            .bind(r.mem_used)
            .bind(r.mem_total)
            .bind(r.disk_used)
            .bind(r.disk_total)
            .bind(r.conntrack.map(|x| x.clamp(0, i32::MAX as i64) as i32))
            .bind(r.conntrack_max.map(|x| x.clamp(0, i32::MAX as i64) as i32))
            .bind(r.temp_c.map(|x| x as f32))
            .bind(r.wan_rx_bps)
            .bind(r.wan_tx_bps)
            .bind(r.proxy_up_bps)
            .bind(r.proxy_down_bps)
            .bind(r.conns.map(|x| x.clamp(0, i32::MAX as i64) as i32))
            .bind(r.clients as i32)
            .bind(r.exits_total as i32)
            .bind(r.exits_alive as i32)
            .execute(&mut *tx)
            .await?;
            if !b.exits.is_empty() {
                let col = |f: fn(&ExitRow) -> Option<i64>| b.exits.iter().map(f).collect::<Vec<_>>();
                sqlx::query(
                    "insert into router_exit_metric (router_id, ts, exit, alive, delay_ms, conns, up_bps, down_bps, clients)
                     select $1, $2, u.e, u.a, u.d::int, u.c::int, u.up, u.down, u.cl::int
                     from unnest($3::text[], $4::bool[], $5::bigint[], $6::bigint[], $7::bigint[], $8::bigint[], $9::bigint[]) u(e, a, d, c, up, down, cl)
                     on conflict do nothing",
                )
                .bind(t.id)
                .bind(now)
                .bind(b.exits.iter().map(|e| e.exit.clone()).collect::<Vec<_>>())
                .bind(b.exits.iter().map(|e| e.alive).collect::<Vec<_>>())
                .bind(col(|e| e.delay_ms.map(|d| d.min(i32::MAX as i64))))
                .bind(col(|e| Some(e.conns.min(i32::MAX as i64))))
                .bind(col(|e| e.up_bps))
                .bind(col(|e| e.down_bps))
                .bind(col(|e| Some(e.clients.min(i32::MAX as i64))))
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            m.prev = b.prev.clone();
            m.last = Some(b.snap.clone());
            Ok(true)
        }
    }
}

async fn collect(st: &AppState, t: &Target, m: &mut Mem, entries: &[Entry], now: OffsetDateTime) -> Outcome {
    if !t.has_key {
        return Outcome::Pending;
    }
    let port = t.tunnel_port;
    // 还没记过主机公钥：经隧道 keyscan 记下（之后严格核对）
    if t.host_key.is_none() {
        match tunnel::keyscan("127.0.0.1", port as u16).await {
            Ok(Some(k)) => {
                let n = sqlx::query("update router set host_key = $2 where id = $1 and host_key is null").bind(t.id).bind(&k).execute(&st.db).await;
                if let Err(e) = n {
                    return Outcome::Fail(format!("记录主机公钥失败：{e}"));
                }
                if let Err(e) = super::sync_files(st).await {
                    return Outcome::Fail(format!("写 router_known_hosts 失败：{e:#}"));
                }
                tracing::info!(router = %t.id, "经隧道记下路由的主机公钥 {}", crate::ssh::fingerprint(&k));
            }
            Ok(None) => {
                return Outcome::Fail(format!(
                    "隧道还没连上（中控 127.0.0.1:{port} 上没有路由的 SSH）：路由上的 fleet-tunnel 没在运行，或连不到中控的 SSH"
                ))
            }
            Err(e) => return Outcome::Fail(format!("{e:#}")),
        }
    }
    let delay = m.last_delay.is_none_or(|t| (now - t).whole_seconds() >= DELAY_EVERY_S);
    let out = match crate::ssh::exec(tunnel::via_tunnel(&st.cfg, t.id, port), &script(delay), TIMEOUT).await {
        Ok(o) => o,
        Err(e) => return Outcome::Fail(format!("{e:#}")),
    };
    if out.code == 255 {
        if out.ssh_kind() == Some(crate::ssh::ErrKind::HostKey) {
            tunnel::close_master(&st.cfg, t.id).await;
        }
        return Outcome::Fail(tunnel::explain(&out, true));
    }
    let raw: Value = match out.stdout.lines().rev().find(|l| l.trim_start().starts_with('{')).map(serde_json::from_str) {
        Some(Ok(v)) => v,
        _ => {
            let detail = if out.stderr.trim().is_empty() { out.stdout.trim() } else { out.stderr.trim() };
            return Outcome::Fail(format!("采集脚本出错（退出码 {}）：{}", out.code, detail.chars().take(300).collect::<String>()));
        }
    };
    if let Some(e) = s(&raw, "error").filter(|_| raw.get("now").is_none()) {
        return Outcome::Fail(e);
    }
    if delay && raw.pointer("/oc/delay").is_some() {
        m.last_delay = Some(now);
    }
    // 配置文件变了（或还没取过）才取回来解析
    let sha = raw.pointer("/oc/config_sha256").and_then(Value::as_str).map(String::from);
    let path = raw.pointer("/oc/config_path").and_then(Value::as_str).map(String::from);
    if let (Some(sha), Some(path)) = (sha.as_ref(), path.as_ref()) {
        if m.config.as_ref().is_none_or(|c| &c.0 != sha) {
            m.config = Some((sha.clone(), fetch_config(st, t, path).await));
        }
    }
    let (config, config_error) = match &m.config {
        Some((_, Ok(p))) => (Some(p.as_slice()), None),
        Some((_, Err(e))) => (None, Some(e.clone())),
        None => (None, None),
    };
    let mut b = build(&raw, &m.prev, m.last.as_ref(), config, entries, now);
    b.snap.openclash.config_error = config_error;
    Outcome::Ok(Box::new(b))
}

/// 经隧道取回 OpenClash 的源配置（顶层 `secret:` 行在路由上先删掉，不传回来），在内存里解析出口
async fn fetch_config(st: &AppState, t: &Target, path: &str) -> Result<Vec<Proxy>, String> {
    let script = crate::jobs::scripts::vars(&[("P", path)]) + &format!("sed '/^secret[[:space:]]*:/d' \"$P\" | head -c {}\n", clash::MAX_CONFIG + 1);
    let out = crate::ssh::exec(tunnel::via_tunnel(&st.cfg, t.id, t.tunnel_port), &script, Duration::from_secs(20)).await.map_err(|e| format!("取配置文件失败：{e:#}"))?;
    if out.code != 0 {
        return Err(format!("取配置文件 {path} 失败：{}", out.stderr.trim().chars().take(200).collect::<String>()));
    }
    if out.stdout.len() > clash::MAX_CONFIG {
        return Err(format!("配置文件 {path} 超过 1 MiB，不解析"));
    }
    clash::parse(&out.stdout)
}

// ── 小时汇总与保留期 ────────────────────────────────────────────────────

/// 小时汇总（随遥测的小时汇总每 5 分钟一轮）：当前小时与上一小时从原始点重算并覆盖，幂等
pub async fn rollup(db: &sqlx::PgPool) -> anyhow::Result<()> {
    let cur = crate::db::floor_hour(OffsetDateTime::now_utc());
    for h in [cur - time::Duration::hours(1), cur] {
        sqlx::query(
            "insert into router_metric_hourly (router_id, hour, samples, cpu_pct, cpu_pct_max, load1, mem_used, mem_total, disk_used, disk_total,
                conntrack, conntrack_max, temp_c, wan_rx_bps, wan_rx_bps_max, wan_tx_bps, wan_tx_bps_max, proxy_up_bps, proxy_up_bps_max,
                proxy_down_bps, proxy_down_bps_max, conns, clients, exits_total, exits_alive)
             select router_id, $1, count(*), avg(cpu_pct), max(cpu_pct), avg(load1), avg(mem_used)::bigint, max(mem_total), avg(disk_used)::bigint,
                max(disk_total), avg(conntrack), max(conntrack_max), avg(temp_c), avg(wan_rx_bps)::bigint, max(wan_rx_bps), avg(wan_tx_bps)::bigint,
                max(wan_tx_bps), avg(proxy_up_bps)::bigint, max(proxy_up_bps), avg(proxy_down_bps)::bigint, max(proxy_down_bps),
                avg(conns), avg(clients), avg(exits_total), avg(exits_alive)
             from router_metric where ts >= $1 and ts < $1 + interval '1 hour' group by router_id
             on conflict (router_id, hour) do update set samples = excluded.samples, cpu_pct = excluded.cpu_pct, cpu_pct_max = excluded.cpu_pct_max,
                load1 = excluded.load1, mem_used = excluded.mem_used, mem_total = excluded.mem_total, disk_used = excluded.disk_used,
                disk_total = excluded.disk_total, conntrack = excluded.conntrack, conntrack_max = excluded.conntrack_max, temp_c = excluded.temp_c,
                wan_rx_bps = excluded.wan_rx_bps, wan_rx_bps_max = excluded.wan_rx_bps_max, wan_tx_bps = excluded.wan_tx_bps,
                wan_tx_bps_max = excluded.wan_tx_bps_max, proxy_up_bps = excluded.proxy_up_bps, proxy_up_bps_max = excluded.proxy_up_bps_max,
                proxy_down_bps = excluded.proxy_down_bps, proxy_down_bps_max = excluded.proxy_down_bps_max, conns = excluded.conns,
                clients = excluded.clients, exits_total = excluded.exits_total, exits_alive = excluded.exits_alive",
        )
        .bind(h)
        .execute(db)
        .await?;
        sqlx::query(
            "insert into router_exit_metric_hourly (router_id, exit, hour, samples, tests, tests_ok, delay_ms, conns, up_bps, up_bps_max,
                down_bps, down_bps_max, clients)
             select router_id, exit, $1, count(*), count(alive), count(*) filter (where alive), avg(delay_ms) filter (where alive),
                avg(conns), avg(up_bps)::bigint, max(up_bps), avg(down_bps)::bigint, max(down_bps), avg(clients)
             from router_exit_metric where ts >= $1 and ts < $1 + interval '1 hour' group by router_id, exit
             on conflict (router_id, exit, hour) do update set samples = excluded.samples, tests = excluded.tests, tests_ok = excluded.tests_ok,
                delay_ms = excluded.delay_ms, conns = excluded.conns, up_bps = excluded.up_bps, up_bps_max = excluded.up_bps_max,
                down_bps = excluded.down_bps, down_bps_max = excluded.down_bps_max, clients = excluded.clients",
        )
        .bind(h)
        .execute(db)
        .await?;
    }
    Ok(())
}

/// 保留期（每小时随分区与保留期维护一轮）：原始点按 `[retention] raw_days`；小时汇总同机器的小时汇总，跟着路由记录走（删路由时级联删掉）
pub async fn cleanup(db: &sqlx::PgPool, raw_days: u32) -> anyhow::Result<()> {
    for t in ["router_metric", "router_exit_metric"] {
        let n = sqlx::query(&format!("delete from {t} where ts < now() - make_interval(days => $1)")).bind(raw_days as i32).execute(db).await?.rows_affected();
        if n > 0 {
            tracing::info!("保留期清理：{t} {n} 条");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 立即采集：采集循环没在跑时只排上、不等；在跑时等到这一轮（取走了这次请求的那一轮）做完
    #[tokio::test]
    async fn refresh_waits_for_its_round() {
        let r = Arc::new(Refresh::default());
        let a = Uuid::now_v7();
        r.now([a]).await;
        assert_eq!(r.take(), (1, vec![a]));
        r.running.store(true, Ordering::Relaxed);
        let h = tokio::spawn({
            let r = r.clone();
            async move { r.now([a]).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!h.is_finished(), "这一轮还没做完");
        let (seq, due) = r.take();
        assert_eq!((seq, due), (2, vec![a]));
        r.served.send_replace(seq);
        h.await.unwrap();
    }

    fn raw(now: i64, rx: u64, up_total: u64, delay: Option<Value>) -> Value {
        let mut oc = json!({
            "enabled": true, "running": true, "mode": "fake-ip-mix", "config_path": "/etc/openclash/config/x.yaml", "config_mtime": 1761000000,
            "config_sha256": "abc", "rss": 52428800, "version": "alpha-g99e68e9", "up_total": up_total, "down_total": up_total * 4, "conns": 3,
            "exits": {"节点01": {"conns": 2, "up": 3000, "down": 30000, "clients": 1}, "DIRECT": {"conns": 1, "up": 10, "down": 10, "clients": 1}},
            "clients": {"192.168.124.100": {"conns": 2, "exits": {"节点01": 2}, "up": 3000, "down": 30000}, "192.168.124.9": {"conns": 1, "exits": {"DIRECT": 1}, "up": 10, "down": 10}},
            "conn_dt": 30,
            "proxies": [
                {"name": "节点01", "type": "Vless", "alive": true, "delay": 120, "at": "2026-10-22T08:00:00Z"},
                {"name": "节点02", "type": "Vless", "alive": false, "delay": 0, "at": null}
            ]
        });
        if let Some(d) = delay {
            oc["delay"] = d;
        }
        json!({
            "v": 1, "now": now, "cpu_pct": 3.25, "load": [0.49, 0.5, 0.4], "uptime": 16862.53, "procs": 120, "cores": 4,
            "cpu_model": "Intel(R) Celeron(R) CPU J1900", "mem_total": 4_000_000_000u64, "mem_avail": 3_000_000_000u64,
            "disk": {"path": "/overlay", "used": 100, "avail": 900}, "temp_c": 38.0, "conntrack": 1887, "conntrack_max": 65535,
            "os": "iStoreOS 24.10.2", "kernel": "6.6.86",
            "ifaces": [
                {"name": "wan", "up": true, "proto": "dhcp", "device": "eth0", "ip": "192.168.1.2", "uptime": 300},
                {"name": "lan", "up": true, "proto": "static", "device": "br-lan", "ip": "192.168.124.1", "uptime": 300},
                {"name": "loopback", "up": true, "proto": "static", "device": "lo", "ip": "127.0.0.1", "uptime": 300}
            ],
            "dev": {"eth0": [rx, rx / 2], "br-lan": [5, 5]},
            "oc": oc,
            "leases": [["192.168.124.100", "aa:bb:cc:dd:ee:01", "laptop"], ["192.168.124.101", "aa:bb:cc:dd:ee:02", null]],
            "neigh": [["192.168.124.100", "br-lan", "aa:bb:cc:dd:ee:01", "REACHABLE"], ["192.168.1.1", "eth0", "aa:00:00:00:00:01", "REACHABLE"],
                      ["192.168.124.50", "br-lan", null, "FAILED"]]
        })
    }

    fn proxies() -> Vec<Proxy> {
        clash::parse("proxies:\n  - {name: 节点01, type: vless, server: 198.51.100.2, port: 20003}\n  - {name: 节点02, type: vless, server: 198.51.100.2, port: 20004}\n").unwrap()
    }

    #[test]
    fn first_sample_has_no_rates() {
        let now = OffsetDateTime::now_utc();
        let b = build(&raw(now.unix_timestamp() - 2, 1000, 500, None), &Prev::default(), None, Some(&proxies()), &[], now);
        let y = &b.snap.system;
        assert_eq!((y.mem_used, y.mem_total, y.disk_used, y.disk_total, y.cores), (Some(1_000_000_000), Some(4_000_000_000), Some(100), Some(1000), Some(4)));
        assert_eq!((y.clock_skew_s, y.clash_rss, y.uptime_s), (Some(2), Some(52428800), Some(16863)));
        assert_eq!(b.snap.interfaces[0].rx_bps, None);
        assert_eq!(b.snap.interfaces.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["wan", "lan"], "回环接口不列");
        assert_eq!(b.snap.openclash.up_bps, None);
        assert_eq!((b.metric.wan_rx_bps, b.metric.conns, b.metric.exits_total), (None, Some(3), 2));
        // 没测过延迟：用 mihomo 自己记的
        let e = &b.snap.exits;
        assert_eq!((e[0].name.as_str(), e[0].alive, e[0].delay_ms, e[0].port), ("节点01", Some(true), Some(120), Some(20003)));
        assert_eq!((e[1].alive, e[1].delay_ms), (Some(false), None));
        // 各出口的流量：路由给的是 30 秒的字节数
        assert_eq!((e[0].conns, e[0].clients, e[0].up_bps, e[0].down_bps), (2, 1, Some(800), Some(8000)));
        // 这一点没测延迟：exit 行里 alive / delay 为 null
        assert!(b.exits.iter().all(|r| r.alive.is_none() && r.delay_ms.is_none()));
    }

    #[test]
    fn rates_from_two_samples_and_counter_reset() {
        let t0 = OffsetDateTime::now_utc();
        let b0 = build(&raw(t0.unix_timestamp(), 1_000_000, 10_000, None), &Prev::default(), None, None, &[], t0);
        let t1 = t0 + time::Duration::seconds(30);
        let b1 = build(&raw(t1.unix_timestamp(), 4_750_000, 40_000, None), &b0.prev, Some(&b0.snap), None, &[], t1);
        // (4 750 000 − 1 000 000) × 8 / 30 = 1 000 000 bit/s；发送是接收的一半
        assert_eq!((b1.metric.wan_rx_bps, b1.metric.wan_tx_bps), (Some(1_000_000), Some(500_000)));
        assert_eq!((b1.snap.openclash.up_bps, b1.snap.openclash.down_bps), (Some(8000), Some(32000)));
        let summary = summary(&b1.snap);
        assert_eq!(summary["wan"], json!({"up": true, "ip": "192.168.1.2", "rx_bps": 1_000_000, "tx_bps": 500_000}));
        // 路由重启（计数器变小）：这一点没有速率
        let t2 = t1 + time::Duration::seconds(30);
        let b2 = build(&raw(t2.unix_timestamp(), 10, 5, None), &b1.prev, Some(&b1.snap), None, &[], t2);
        assert_eq!((b2.metric.wan_rx_bps, b2.metric.proxy_up_bps), (None, None));
    }

    #[test]
    fn delay_tests_update_streaks() {
        let t0 = OffsetDateTime::now_utc();
        let tested = |r: Value| Some(json!({"ok": true, "results": r}));
        let b0 = build(&raw(0, 0, 0, tested(json!({"节点01": 88, "GLOBAL": 88}))), &Prev::default(), None, Some(&proxies()), &[], t0);
        let e = &b0.snap.exits;
        assert_eq!((e[0].alive, e[0].delay_ms, e[0].fail_streak), (Some(true), Some(88), 0));
        assert_eq!((e[1].alive, e[1].delay_ms, e[1].fail_streak), (Some(false), None, 1));
        assert_eq!(b0.exits[1], ExitRow { exit: "节点02".into(), alive: Some(false), delay_ms: None, conns: 0, up_bps: None, down_bps: None, clients: 0 });
        // 没测的一轮沿用上一份
        let b1 = build(&raw(0, 0, 0, None), &b0.prev, Some(&b0.snap), Some(&proxies()), &[], t0 + time::Duration::seconds(30));
        assert_eq!((b1.snap.exits[1].alive, b1.snap.exits[1].fail_streak), (Some(false), 1));
        // 全部不通（mihomo 回 504，结果为空）：都 +1
        let b2 = build(&raw(0, 0, 0, tested(json!({}))), &b1.prev, Some(&b1.snap), Some(&proxies()), &[], t0 + time::Duration::seconds(300));
        assert_eq!(b2.snap.exits.iter().map(|e| e.fail_streak).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(summary(&b2.snap)["exits"], json!({"total": 2, "alive": 1, "managed": 0, "drift": 0}), "只测不通 1 次的还算可用");
        // 测试请求本身失败（API 出错）：不算出口不通
        let b3 = build(&raw(0, 0, 0, Some(json!({"ok": false, "error": "x"}))), &b2.prev, Some(&b2.snap), Some(&proxies()), &[], t0 + time::Duration::seconds(330));
        assert_eq!(b3.snap.exits.iter().map(|e| e.fail_streak).collect::<Vec<_>>(), [1, 2]);
    }

    #[test]
    fn clients_merge_leases_neighbors_and_connections() {
        let now = OffsetDateTime::now_utc();
        let b = build(&raw(0, 0, 0, None), &Prev::default(), None, None, &[], now);
        let c: Vec<(&str, bool, i64, Option<&str>)> = b.snap.clients.iter().map(|c| (c.ip.as_str(), c.online, c.conns, c.hostname.as_deref())).collect();
        // WAN 口那一侧的邻居（192.168.1.1）不算；FAILED 的邻居不算；只有租约的离线
        assert_eq!(c, [("192.168.124.100", true, 2, Some("laptop")), ("192.168.124.9", true, 1, None), ("192.168.124.101", false, 0, None)]);
        assert_eq!(b.snap.clients[0].exits[0].name, "节点01");
        assert_eq!((b.metric.clients, summary(&b.snap)["clients"].as_i64()), (2, Some(2)));
        // 速率：这段时间（conn_dt 30 秒）各自连接的字节数之差；没有连接的为 0；算不出间隔（第一轮）为 None
        let r: Vec<(Option<i64>, Option<i64>)> = b.snap.clients.iter().map(|c| (c.down_bps, c.up_bps)).collect();
        assert_eq!(r, [(Some(8000), Some(800)), (Some(3), Some(3)), (Some(0), Some(0))]);
        let mut first = raw(0, 0, 0, None);
        first["oc"]["conn_dt"] = Value::Null;
        let b = build(&first, &Prev::default(), None, None, &[], now);
        assert!(b.snap.clients.iter().all(|c| c.down_bps.is_none() && c.up_bps.is_none()));
    }

    #[test]
    fn script_carries_program_and_parameters() {
        let s = script(true);
        assert!(s.starts_with("DELAY='1'\nDELAY_URL='https%3A%2F%2Fwww.gstatic.com%2Fgenerate_204'\n"), "{}", &s[..120]);
        assert!(s.contains("'use strict';") && s.contains("\nFLEET_UC\n") && s.trim_end().ends_with("main </dev/null"));
        assert!(script(false).starts_with("DELAY='0'\n"));
        // 程序里不能有单独一行的结束标记
        assert!(!include_str!("collect.uc").lines().any(|l| l == "FLEET_UC"));
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let now = OffsetDateTime::now_utc();
        let b = build(&raw(0, 0, 0, None), &Prev::default(), None, Some(&proxies()), &[], now);
        let v = serde_json::to_value(&b.snap).unwrap();
        assert_eq!(v["exits"][0]["type"], "Vless");
        assert_eq!(v["exits"][0]["sync"], "unmanaged");
        let back: Snapshot = serde_json::from_value(v).unwrap();
        assert_eq!(back.exits.len(), 2);
        // 字段缺的旧快照也读得回来
        let old: Snapshot = serde_json::from_value(json!({"system": {"cores": 2}})).unwrap();
        assert_eq!(old.system.cores, Some(2));
    }
}
