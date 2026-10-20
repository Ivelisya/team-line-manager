//! fleetd-ingest：接收 nodeagent / relayd 的上报，并提供只读查询与原生告警钩子。
//!
//! 挂载方式（由 fleetd 接线，见 WIRING.md）：
//! - `api::ingest_routes(ing)`：`POST /v2/ingest/{node,relay}`，Bearer 令牌鉴权，**不能**套 fleetd 的管理员 guard；
//! - `api::read_routes(ing)`：`GET /v2/api/v1/hosts…`、`/v2/api/v1/relays/{id}/xray`、`/v2/api/v1/ingest/alerts`，
//!   合并进 fleetd 的 router（在 guard 之内）；
//! - `Ingest::evaluate(now)`：原生告警观测，交给 fleetd 告警引擎；
//! - `Ingest::spawn_maintenance()`：每小时清理过期数据；
//! - `tokens::cli`：`fleetd agent-token issue|revoke|list`。
pub mod api;
pub mod tokens;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const SCHEMA: &str = include_str!("schema.sql");

#[derive(Clone, Debug)]
pub struct IngestCfg {
    pub db_path: PathBuf,
    pub body_max: usize,
    /// 每台主机的令牌桶：每秒补充、桶容量（补传时允许突发）
    pub rate_per_s: f64,
    pub burst: f64,
    pub rollup_days: i64,
    pub xray_days: i64,
    pub seen_days: i64,
    /// 样本时间早于 now − max_past_s 或晚于 now + max_future_s 的不进汇总（批次照常确认）
    pub max_past_s: f64,
    pub max_future_s: f64,
    pub offline_s: f64,
    pub resource_pct: f64,
    pub resource_for_s: f64,
    pub cpu_pct: f64,
    pub cpu_for_s: f64,
}

impl Default for IngestCfg {
    fn default() -> Self {
        IngestCfg {
            db_path: PathBuf::from("/var/lib/fleetd/ingest.db"),
            body_max: 1024 * 1024,
            rate_per_s: 2.0,
            burst: 60.0,
            rollup_days: 90,
            xray_days: 35,
            seen_days: 7,
            max_past_s: 7.0 * 86400.0,
            max_future_s: 300.0,
            offline_s: 120.0,
            resource_pct: 90.0,
            resource_for_s: 120.0,
            cpu_pct: 15.0,
            cpu_for_s: 600.0,
        }
    }
}

pub struct Ingest {
    pub cfg: IngestCfg,
    /// 每接收一个新批次（非重复）就通知一次：fleetd 据此立即刷新主机快照 / 线路机快照并推送 SSE
    pub accepted: tokio::sync::Notify,
    /// 有人在看（fleetd 的 SSE 有连接）：接收响应带 "live": true，relayd 随后 90 s 内每 60 s 全量巡检
    pub live: std::sync::atomic::AtomicBool,
    /// 接收批次（鉴权 + 解析 + 写库）累计的线程 CPU：(批次数, 毫秒)
    pub cpu: Mutex<(u64, f64)>,
    db: Mutex<Connection>,
    buckets: Mutex<HashMap<String, (f64, f64)>>,
    authfail: Mutex<(f64, f64)>,
}

pub type Shared = Arc<Ingest>;

/// 汇总指标：名字、在样本 JSON 里的取值、放大倍数
pub const METRICS: &[&str] = &["cpu", "steal", "iowait", "load1", "mem", "swap", "disk", "rx", "tx", "tcp", "udp", "procs"];

pub fn metric_scale(m: &str) -> f64 {
    match m {
        "cpu" | "steal" | "iowait" | "load1" | "mem" | "swap" | "disk" => 100.0,
        _ => 1.0,
    }
}

fn extract(s: &Value) -> [Option<f64>; 12] {
    let f = |v: &Value| v.as_f64();
    [
        f(&s["cpu"]["pct"]),
        f(&s["cpu"]["steal"]),
        f(&s["cpu"]["iowait"]),
        f(&s["load"][0]),
        f(&s["mem"]["pct"]),
        f(&s["swap"]["pct"]),
        f(&s["disk"]["pct"]),
        f(&s["net"]["rx_bps"]),
        f(&s["net"]["tx_bps"]),
        f(&s["sock"]["tcp"]),
        f(&s["sock"]["udp"]),
        f(&s["procs"]),
    ]
}

pub fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

#[derive(Debug, PartialEq)]
pub enum Accepted {
    Ok { samples: usize, skipped: usize },
    Duplicate,
}

#[derive(Debug)]
pub struct Reject {
    pub status: u16,
    pub msg: String,
    pub retry_after: Option<u64>,
}

fn rej(status: u16, msg: impl Into<String>) -> Reject {
    Reject { status, msg: msg.into(), retry_after: None }
}

/// 原生告警观测（fleetd 转成 alerts::Observation，见 WIRING.md）
#[derive(Debug, Clone, serde::Serialize)]
pub struct NativeAlert {
    pub rule: &'static str,
    pub host_id: String,
    pub kind: String,
    pub severity: &'static str,
    pub summary: String,
    pub evidence: Value,
    /// 条件开始成立的时刻（跨 fleetd 重启计算 for）
    pub active_since: f64,
    pub for_s: f64,
    /// 按本模块自己的 for 判断是否已满足（引擎不做 for 时可直接用）
    pub firing: bool,
}

/// 提交组提交里未提交的批次（连接不在事务里就什么都不做）
pub fn commit_pending(c: &Connection) {
    if !c.is_autocommit() {
        if let Err(e) = c.execute_batch("COMMIT") {
            tracing::warn!(error = %e, "ingest 组提交失败");
        }
    }
}

fn init(conn: &Connection) -> Result<(), String> {
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000; PRAGMA mmap_size=1073741824; PRAGMA cache_size=-16384;")
        .map_err(|e| format!("初始化 PRAGMA 失败：{e}"))?;
    conn.execute_batch(SCHEMA).map_err(|e| format!("建表失败：{e}"))?;
    Ok(())
}

impl Ingest {
    pub fn open(cfg: IngestCfg) -> Result<Shared, String> {
        if let Some(d) = cfg.db_path.parent() {
            if !d.as_os_str().is_empty() {
                std::fs::create_dir_all(d).map_err(|e| format!("创建目录失败：{e}"))?;
            }
        }
        let conn = Connection::open(&cfg.db_path).map_err(|e| format!("打开 {} 失败：{e}", cfg.db_path.display()))?;
        conn.set_prepared_statement_cache_capacity(64);
        init(&conn)?;
        Ok(Arc::new(Ingest { cfg, accepted: tokio::sync::Notify::new(), live: Default::default(), cpu: Mutex::new((0, 0.0)), db: Mutex::new(conn), buckets: Mutex::new(HashMap::new()), authfail: Mutex::new((40.0, now())) }))
    }

    pub fn open_memory(mut cfg: IngestCfg) -> Shared {
        cfg.db_path = PathBuf::from(":memory:");
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        Arc::new(Ingest { cfg, accepted: tokio::sync::Notify::new(), live: Default::default(), cpu: Mutex::new((0, 0.0)), db: Mutex::new(conn), buckets: Mutex::new(HashMap::new()), authfail: Mutex::new((40.0, now())) })
    }

    pub fn db_path(&self) -> &Path {
        &self.cfg.db_path
    }

    pub fn with_db<R>(&self, f: impl FnOnce(&mut Connection) -> R) -> R {
        let mut g = self.db.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut g)
    }

    /// 组提交：接收的批次写在一个敞开的事务里（每批一个 SAVEPOINT），由 flush() 每秒提交一次。
    /// 同一连接上的读看得到未提交的数据；别的连接 / 进程最多晚 1 s 看到。进程崩溃最多丢最后 1 s 已回 200 的批次
    ///（边缘代理按 seq 去重，下次重启后照常上报）。
    pub fn flush(&self) {
        self.with_db(|c| commit_pending(c));
    }

    // ---------- 令牌 ----------

    /// 返回令牌对应的 host_id（未撤销）
    pub fn auth(&self, token: &str) -> Option<String> {
        let h = sha256_hex(token);
        self.with_db(|c| {
            c.query_row("SELECT host_id FROM agent_tokens WHERE sha256=?1 AND revoked_at IS NULL", [h], |r| r.get::<_, String>(0))
                .optional()
                .ok()
                .flatten()
        })
    }

    /// 全局的鉴权失败限速（防止拿随机令牌打库）：20/s，容量 40
    pub fn authfail_allow(&self, now: f64) -> bool {
        let mut g = self.authfail.lock().unwrap_or_else(|p| p.into_inner());
        let (tok, last) = *g;
        let tok = (tok + (now - last).max(0.0) * 20.0).min(40.0);
        if tok < 1.0 {
            *g = (tok, now);
            return false;
        }
        *g = (tok - 1.0, now);
        true
    }

    /// 每台主机的令牌桶；不允许时返回建议等待秒数
    pub fn rate_check(&self, host: &str, now: f64) -> Result<(), u64> {
        let mut g = self.buckets.lock().unwrap_or_else(|p| p.into_inner());
        let e = g.entry(host.to_string()).or_insert((self.cfg.burst, now));
        let tok = (e.0 + (now - e.1).max(0.0) * self.cfg.rate_per_s).min(self.cfg.burst);
        if tok < 1.0 {
            *e = (tok, now);
            return Err(((1.0 - tok) / self.cfg.rate_per_s).ceil().max(1.0) as u64);
        }
        *e = (tok - 1.0, now);
        Ok(())
    }

    // ---------- 接收 ----------

    /// 处理一批（已鉴权：`host` 来自令牌）。整批在一个事务里：去重表 + 汇总 + 最新值，恰好一次。
    pub fn accept(&self, host: &str, kind: &str, b: &Value, now: f64) -> Result<Accepted, Reject> {
        if b["v"].as_u64() != Some(1) {
            return Err(rej(400, "不支持的批次版本"));
        }
        if b["kind"].as_str() != Some(kind) {
            return Err(rej(400, "kind 与接口不一致"));
        }
        if b["host_id"].as_str() != Some(host) {
            return Err(rej(403, "host_id 与令牌不一致"));
        }
        let boot = b["boot_id"].as_str().filter(|s| !s.is_empty() && s.len() <= 64).ok_or_else(|| rej(400, "boot_id 无效"))?;
        let seq = b["seq"].as_u64().filter(|s| *s > 0 && *s < i64::MAX as u64).ok_or_else(|| rej(400, "seq 无效"))? as i64;
        let samples = b["samples"].as_array().ok_or_else(|| rej(400, "缺少 samples"))?;
        if samples.len() > 360 {
            return Err(rej(413, "样本过多"));
        }
        let agent: String = b["agent"].as_str().unwrap_or("").chars().take(64).collect();
        let cfg = &self.cfg;

        self.with_db(|c| {
            if c.is_autocommit() {
                c.execute_batch("BEGIN IMMEDIATE").map_err(|e| rej(503, format!("数据库忙：{e}")))?;
            }
            let tx = c.savepoint().map_err(|e| rej(503, format!("数据库忙：{e}")))?;
            let n = tx
                .execute("INSERT OR IGNORE INTO ingest_seen(host_id, boot_id, seq, received_at) VALUES(?1,?2,?3,?4)", params![host, boot, seq, now])
                .map_err(|e| rej(503, e.to_string()))?;
            if n == 0 {
                // 已收过：只刷新在线时间
                tx.execute("UPDATE host_latest SET received_at=MAX(received_at, ?2) WHERE host_id=?1", params![host, now]).ok();
                tx.commit().map_err(|e| rej(503, e.to_string()))?;
                return Ok(Accepted::Duplicate);
            }
            // 1 分钟汇总：先在内存里按分钟聚合
            let mut minutes: BTreeMap<i64, [(i64, i64, Option<i64>); 12]> = BTreeMap::new();
            let mut xr: BTreeMap<(i64, String, String), (i64, i64)> = BTreeMap::new();
            let (mut ok, mut skipped) = (0usize, 0usize);
            let mut newest: Option<&Value> = None;
            for s in samples {
                let Some(ts) = s["ts"].as_f64() else {
                    skipped += 1;
                    continue;
                };
                if ts < now - cfg.max_past_s || ts > now + cfg.max_future_s {
                    skipped += 1;
                    continue;
                }
                ok += 1;
                if newest.is_none_or(|n| n["ts"].as_f64().unwrap_or(0.0) <= ts) {
                    newest = Some(s);
                }
                let minute = (ts as i64).div_euclid(60) * 60;
                let vals = extract(s);
                let row = minutes.entry(minute).or_insert([(0, 0, None); 12]);
                for (i, v) in vals.iter().enumerate() {
                    if let Some(v) = v {
                        let x = (v * metric_scale(METRICS[i])).round() as i64;
                        row[i].0 += 1;
                        row[i].1 += x;
                        row[i].2 = Some(row[i].2.map_or(x, |m: i64| m.max(x)));
                    }
                }
                if ts >= now - 900.0 {
                    tx.execute(
                        "INSERT OR REPLACE INTO host_recent(host_id, ts, mem, disk, cpu, steal, iowait) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                        params![host, ts, vals[4], vals[6], vals[0], vals[1], vals[2]],
                    )
                    .map_err(|e| rej(503, e.to_string()))?;
                }
                if kind == "relay" {
                    if let Some(d) = s["xray"]["d"].as_array() {
                        for e in d {
                            let (Some(k), Some(t)) = (e[0].as_str(), e[1].as_str()) else { continue };
                            let (u, dn) = (e[2].as_i64().unwrap_or(0).max(0), e[3].as_i64().unwrap_or(0).max(0));
                            if u == 0 && dn == 0 {
                                continue;
                            }
                            let x = xr.entry((minute, k.chars().take(16).collect(), t.chars().take(128).collect())).or_default();
                            x.0 += u;
                            x.1 += dn;
                        }
                    }
                }
            }
            let mut sql = String::from("INSERT INTO host_1m(host_id, minute");
            for m in METRICS {
                sql += &format!(", {m}_n, {m}_s, {m}_x");
            }
            sql += ") VALUES(?1, ?2";
            for i in 0..METRICS.len() {
                sql += &format!(", ?{}, ?{}, ?{}", 3 + i * 3, 4 + i * 3, 5 + i * 3);
            }
            sql += ") ON CONFLICT(host_id, minute) DO UPDATE SET ";
            sql += &METRICS
                .iter()
                .map(|m| {
                    format!("{m}_n={m}_n+excluded.{m}_n, {m}_s={m}_s+excluded.{m}_s, {m}_x=MAX(COALESCE({m}_x, excluded.{m}_x), COALESCE(excluded.{m}_x, {m}_x))")
                })
                .collect::<Vec<_>>()
                .join(", ");
            {
                let mut st = tx.prepare_cached(&sql).map_err(|e| rej(500, e.to_string()))?;
                for (minute, row) in &minutes {
                    let mut p: Vec<rusqlite::types::Value> = vec![host.to_string().into(), (*minute).into()];
                    for (n, s, x) in row {
                        p.push((*n).into());
                        p.push((*s).into());
                        p.push(match x {
                            Some(x) => (*x).into(),
                            None => rusqlite::types::Value::Null,
                        });
                    }
                    st.execute(rusqlite::params_from_iter(p)).map_err(|e| rej(503, e.to_string()))?;
                }
                let mut sx = tx
                    .prepare_cached(
                        "INSERT INTO xray_1m(host_id, kind, tag, minute, up, down) VALUES(?1,?2,?3,?4,?5,?6)
                         ON CONFLICT(host_id, kind, tag, minute) DO UPDATE SET up=up+excluded.up, down=down+excluded.down",
                    )
                    .map_err(|e| rej(500, e.to_string()))?;
                for ((minute, k, t), (u, d)) in &xr {
                    sx.execute(params![host, k, t, minute, u, d]).map_err(|e| rej(503, e.to_string()))?;
                }
            }
            tx.execute("DELETE FROM host_recent WHERE host_id=?1 AND ts < ?2", params![host, now - 900.0]).ok();

            // 最新值
            let created = b["created_at"].as_f64().unwrap_or(now);
            let prev_ts: Option<f64> =
                tx.query_row("SELECT sample_ts FROM host_latest WHERE host_id=?1", [host], |r| r.get(0)).optional().map_err(|e| rej(503, e.to_string()))?;
            let newest_ts = newest.and_then(|n| n["ts"].as_f64());
            let is_newer = match (prev_ts, newest_ts) {
                (None, _) => true,
                (Some(p), Some(n)) => n >= p,
                (Some(_), None) => false,
            };
            let mut xinfo = Value::Null;
            if kind == "relay" && b["xray"].is_object() {
                xinfo = b["xray"].clone();
                if let Some(o) = xinfo.as_object_mut() {
                    o.remove("entries");
                }
                if let Some(n) = newest {
                    let lx = &n["xray"];
                    xinfo["conns"] = lx["conns"].clone();
                    xinfo["last_query"] = json!({"ok": lx["ok"], "err": lx["err"], "latency_ms": lx["latency_ms"], "ts": n["ts"]});
                }
            }
            if prev_ts.is_none() {
                tx.execute(
                    "INSERT INTO host_latest(host_id, kind, received_at, first_seen, sample_ts, boot_id, seq, agent, meta, outbox, sample, xray, batches, samples)
                     VALUES(?1,?2,?3,?3,?4,?5,?6,?7,?8,?9,?10,?11,1,?12)",
                    params![
                        host,
                        kind,
                        now,
                        newest_ts.unwrap_or(0.0),
                        boot,
                        seq,
                        agent,
                        b["meta"].to_string(),
                        b["outbox"].to_string(),
                        newest.cloned().unwrap_or(Value::Null).to_string(),
                        if xinfo.is_null() { None } else { Some(xinfo.to_string()) },
                        ok as i64
                    ],
                )
                .map_err(|e| rej(503, e.to_string()))?;
            } else if is_newer {
                tx.execute(
                    "UPDATE host_latest SET kind=?2, received_at=MAX(received_at, ?3), sample_ts=?4, boot_id=?5, seq=?6, agent=?7, meta=?8, outbox=?9,
                       sample=?10, xray=COALESCE(?11, xray), batches=batches+1, samples=samples+?12 WHERE host_id=?1",
                    params![
                        host,
                        kind,
                        now,
                        newest_ts.unwrap_or(0.0),
                        boot,
                        seq,
                        agent,
                        b["meta"].to_string(),
                        b["outbox"].to_string(),
                        newest.cloned().unwrap_or(Value::Null).to_string(),
                        if xinfo.is_null() { None } else { Some(xinfo.to_string()) },
                        ok as i64
                    ],
                )
                .map_err(|e| rej(503, e.to_string()))?;
            } else {
                tx.execute(
                    "UPDATE host_latest SET received_at=MAX(received_at, ?2), batches=batches+1, samples=samples+?3 WHERE host_id=?1",
                    params![host, now, ok as i64],
                )
                .map_err(|e| rej(503, e.to_string()))?;
            }
            // Xray 条目：累计值 / 原始值 / 速率（按批次创建时间只进不退）
            if kind == "relay" {
                if let Some(es) = b["xray"]["entries"].as_array() {
                    let mut st = tx
                        .prepare_cached(
                            "INSERT INTO xray_latest(host_id, kind, tag, cum_up, cum_down, raw_up, raw_down, rate_up, rate_down, ts)
                             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
                             ON CONFLICT(host_id, kind, tag) DO UPDATE SET cum_up=excluded.cum_up, cum_down=excluded.cum_down,
                               raw_up=excluded.raw_up, raw_down=excluded.raw_down, rate_up=excluded.rate_up, rate_down=excluded.rate_down, ts=excluded.ts
                             WHERE excluded.ts >= xray_latest.ts",
                        )
                        .map_err(|e| rej(500, e.to_string()))?;
                    for e in es.iter().take(20000) {
                        let (Some(k), Some(t)) = (e[0].as_str(), e[1].as_str()) else { continue };
                        let g = |i: usize| e[i].as_i64().unwrap_or(0).max(0);
                        let gf = |i: usize| e[i].as_f64().unwrap_or(0.0).max(0.0);
                        st.execute(params![
                            host,
                            k.chars().take(16).collect::<String>(),
                            t.chars().take(128).collect::<String>(),
                            g(2),
                            g(3),
                            g(4),
                            g(5),
                            gf(6),
                            gf(7),
                            created
                        ])
                        .map_err(|e| rej(503, e.to_string()))?;
                    }
                }
            }
            // 探测结果（fleet.probe.v1）：按自然键幂等写入
            if kind == "relay" {
                if let Some(pb) = b.get("probe").filter(|p| p["schema"].as_str() == Some("fleet.probe.v1")) {
                    let mut st = tx
                        .prepare_cached(
                            "INSERT OR IGNORE INTO probe_land(relay, node, check_kind, measured_at, attempt, status, doc) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                        )
                        .map_err(|e| rej(500, e.to_string()))?;
                    for r in pb["results"].as_array().into_iter().flatten().take(5000) {
                        let (Some(node), Some(check), Some(at), Some(status)) =
                            (r["node"].as_str(), r["check"].as_str(), r["measured_at"].as_f64(), r["status"].as_str())
                        else {
                            continue;
                        };
                        st.execute(params![host, node, check, at, r["attempt"].as_i64().unwrap_or(0), status, r.to_string()])
                            .map_err(|e| rej(503, e.to_string()))?;
                    }
                    if pb["targets"].is_array() {
                        tx.execute(
                            "INSERT INTO probe_targets(relay, targets, last_id, updated_at) VALUES(?1,?2,?3,?4)
                             ON CONFLICT(relay) DO UPDATE SET targets=excluded.targets, last_id=MAX(last_id, excluded.last_id), updated_at=excluded.updated_at",
                            params![host, pb["targets"].to_string(), pb["last_id"].as_i64().unwrap_or(0), now],
                        )
                        .map_err(|e| rej(503, e.to_string()))?;
                    }
                }
            }
            // 访问日志（fleet.access.v1）：按 (host, id) 幂等暂存，由 fleetd 的旧库喂数消费
            if kind == "relay" {
                if let Some(ac) = b.get("access").filter(|a| a["schema"].as_str() == Some("fleet.access.v1")) {
                    let mut st = tx
                        .prepare_cached("INSERT OR IGNORE INTO access_raw(host_id, id, ts, doc, received_at) VALUES(?1,?2,?3,?4,?5)")
                        .map_err(|e| rej(500, e.to_string()))?;
                    for e in ac["events"].as_array().into_iter().flatten().take(5000) {
                        let (Some(id), Some(ts)) = (e["id"].as_str(), e["ts"].as_f64()) else { continue };
                        st.execute(params![host, id.chars().take(96).collect::<String>(), ts, e.to_string(), now]).map_err(|e| rej(503, e.to_string()))?;
                    }
                    tx.execute(
                        "INSERT INTO access_state(host_id, source, counters, dropped, updated_at) VALUES(?1,?2,?3,?4,?5)
                         ON CONFLICT(host_id) DO UPDATE SET source=excluded.source, counters=excluded.counters, dropped=access_state.dropped+excluded.dropped, updated_at=excluded.updated_at",
                        params![host, ac["source"].to_string(), ac.get("counters").map(|c| c.to_string()), ac["dropped"].as_i64().unwrap_or(0), now],
                    )
                    .map_err(|e| rej(503, e.to_string()))?;
                }
            }
            if kind == "relay" {
                if let Some(lp) = b.get("legacy_probe").filter(|v| v.is_object()) {
                    tx.execute("INSERT INTO legacy_probe(host_id, updated_at) VALUES(?1,?2) ON CONFLICT(host_id) DO UPDATE SET updated_at=excluded.updated_at", params![host, now])
                        .map_err(|e| rej(503, e.to_string()))?;
                    for (k, m) in [("probe", "probe_mtime"), ("paths", "paths_mtime")] {
                        if lp[k].is_object() {
                            tx.execute(&format!("UPDATE legacy_probe SET {k}=?2, {m}=?3 WHERE host_id=?1"), params![host, lp[k].to_string(), lp[m].as_f64()])
                                .map_err(|e| rej(503, e.to_string()))?;
                        }
                    }
                }
            }
            tx.commit().map_err(|e| rej(503, e.to_string()))?;
            Ok(Accepted::Ok { samples: ok, skipped })
        })
    }

    // ---------- 查询 ----------

    pub fn hosts(&self, now: f64) -> Value {
        let offline = self.cfg.offline_s;
        self.with_db(|c| {
            let mut st = c
                .prepare("SELECT host_id, kind, received_at, first_seen, sample_ts, boot_id, seq, agent, meta, outbox, sample, xray, batches, samples FROM host_latest ORDER BY host_id")
                .unwrap();
            let rows = st
                .query_map([], |r| {
                    let recv: f64 = r.get(2)?;
                    let sample: Value = serde_json::from_str(&r.get::<_, String>(10)?).unwrap_or(Value::Null);
                    let xray: Option<String> = r.get(11)?;
                    let s = &sample;
                    let mut o = json!({
                        "host_id": r.get::<_, String>(0)?,
                        "kind": r.get::<_, String>(1)?,
                        "online": now - recv < offline,
                        "received_at": recv,
                        "age_s": ((now - recv) * 10.0).round() / 10.0,
                        "first_seen": r.get::<_, f64>(3)?,
                        "sample_ts": r.get::<_, f64>(4)?,
                        "boot_id": r.get::<_, String>(5)?,
                        "seq": r.get::<_, i64>(6)?,
                        "agent": r.get::<_, String>(7)?,
                        "meta": serde_json::from_str::<Value>(&r.get::<_, String>(8)?).unwrap_or(Value::Null),
                        "outbox": serde_json::from_str::<Value>(&r.get::<_, String>(9)?).unwrap_or(Value::Null),
                        "batches": r.get::<_, i64>(12)?,
                        "samples": r.get::<_, i64>(13)?,
                        "latest": {
                            "cpu": s["cpu"]["pct"], "steal": s["cpu"]["steal"], "iowait": s["cpu"]["iowait"],
                            "load": s["load"], "mem": s["mem"], "swap": s["swap"], "disk": s["disk"],
                            "net": s["net"], "sock": s["sock"], "procs": s["procs"], "uptime": s["uptime"],
                        },
                    });
                    if let Some(x) = xray.and_then(|x| serde_json::from_str::<Value>(&x).ok()) {
                        o["xray"] = json!({
                            "running": x["xray"]["running"], "version": x["xray"]["version"], "pid": x["xray"]["pid"],
                            "started_at": x["xray"]["started_at"], "xui_running": x["xui"]["running"],
                            "last_query": x["last_query"], "accum_since": x["accum_since"],
                        });
                    }
                    Ok(o)
                })
                .unwrap();
            Value::Array(rows.filter_map(|r| r.ok()).collect())
        })
    }

    /// 1 分钟汇总的时间序列。`metrics` 逗号分隔；返回 {metric: [[t, avg, max], …]}
    pub fn series(&self, host: &str, metrics: &str, range_s: i64, step_s: Option<i64>, now: f64) -> Result<Value, String> {
        let range_s = range_s.clamp(60, self.cfg.rollup_days * 86400);
        let step = step_s.unwrap_or_else(|| ((range_s / 720 + 59) / 60 * 60).max(60)).max(60);
        let from = (now as i64 - range_s).div_euclid(60) * 60;
        let mut out = serde_json::Map::new();
        for m in metrics.split(',').map(|x| x.trim()).filter(|x| !x.is_empty()) {
            if !METRICS.contains(&m) {
                return Err(format!("未知指标 {m}；可用：{}", METRICS.join(",")));
            }
            let sc = metric_scale(m);
            let sql = format!(
                "SELECT (minute/?3)*?3 AS b, SUM({m}_s), SUM({m}_n), MAX({m}_x) FROM host_1m WHERE host_id=?1 AND minute>=?2 AND {m}_n>0 GROUP BY b ORDER BY b"
            );
            let pts = self.with_db(|c| -> Result<Vec<Value>, String> {
                let mut st = c.prepare_cached(&sql).map_err(|e| e.to_string())?;
                let rows = st
                    .query_map(params![host, from, step], |r| {
                        let (b, s, n, x): (i64, i64, i64, Option<i64>) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
                        let avg = (s as f64 / n.max(1) as f64 / sc * 100.0).round() / 100.0;
                        Ok(json!([b, avg, x.map(|x| x as f64 / sc)]))
                    })
                    .map_err(|e| e.to_string())?;
                Ok(rows.filter_map(|r| r.ok()).collect())
            })?;
            out.insert(m.to_string(), Value::Array(pts));
        }
        Ok(json!({"host_id": host, "range_s": range_s, "step_s": step, "from": from, "series": out}))
    }

    /// 线路机 Xray：进程、连接、每个条目的累计值与速率，以及 range 内的增量合计；给了 kind+tag 时附分钟序列
    pub fn relay_xray(&self, host: &str, range_s: i64, kind: Option<&str>, tag: Option<&str>, now: f64) -> Option<Value> {
        let range_s = range_s.clamp(60, self.cfg.xray_days * 86400);
        let from = (now as i64 - range_s).div_euclid(60) * 60;
        self.with_db(|c| {
            let (recv, xray): (f64, Option<String>) =
                c.query_row("SELECT received_at, xray FROM host_latest WHERE host_id=?1", [host], |r| Ok((r.get(0)?, r.get(1)?))).optional().ok()??;
            let x: Value = xray.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
            let mut win: HashMap<(String, String), (i64, i64)> = HashMap::new();
            {
                let mut st = c.prepare_cached("SELECT kind, tag, SUM(up), SUM(down) FROM xray_1m WHERE host_id=?1 AND minute>=?2 GROUP BY kind, tag").ok()?;
                let rows = st.query_map(params![host, from], |r| Ok(((r.get::<_, String>(0)?, r.get::<_, String>(1)?), (r.get(2)?, r.get(3)?)))).ok()?;
                for r in rows.flatten() {
                    win.insert(r.0, r.1);
                }
            }
            let conns = &x["conns"];
            let mut entries = vec![];
            {
                let mut st = c
                    .prepare_cached("SELECT kind, tag, cum_up, cum_down, raw_up, raw_down, rate_up, rate_down, ts FROM xray_latest WHERE host_id=?1 ORDER BY kind, tag")
                    .ok()?;
                let rows = st
                    .query_map([host], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, i64>(4)?,
                            r.get::<_, i64>(5)?,
                            r.get::<_, f64>(6)?,
                            r.get::<_, f64>(7)?,
                            r.get::<_, f64>(8)?,
                        ))
                    })
                    .ok()?;
                for (k, t, cu, cd, ru, rd, rtu, rtd, ts) in rows.flatten() {
                    let w = win.get(&(k.clone(), t.clone())).copied().unwrap_or((0, 0));
                    let port = if k == "inbound" { t.strip_prefix("inbound-").and_then(|p| p.parse::<u16>().ok()) } else { None };
                    let mut e = json!({
                        "kind": k, "tag": t, "cum_up": cu, "cum_down": cd, "raw_up": ru, "raw_down": rd,
                        "rate_up_bps": rtu, "rate_down_bps": rtd, "window_up": w.0, "window_down": w.1, "ts": ts,
                    });
                    if let Some(p) = port {
                        e["port"] = json!(p);
                        e["conns"] = conns[p.to_string()].clone();
                    }
                    entries.push(e);
                }
            }
            let mut out = json!({
                "host_id": host,
                "received_at": recv,
                "age_s": ((now - recv) * 10.0).round() / 10.0,
                "online": now - recv < self.cfg.offline_s,
                "range_s": range_s,
                "process": {"xray": x["xray"], "xui": x["xui"], "core_id": x["core_id"], "api": x["api"], "last_ok": x["last_ok"], "errors": x["errors"], "last_query": x["last_query"]},
                "accum_since": x["accum_since"],
                "seed": x["seed"],
                "conns": conns,
                "entries": entries,
            });
            if let (Some(k), Some(t)) = (kind, tag) {
                let mut st = c.prepare_cached("SELECT minute, up, down FROM xray_1m WHERE host_id=?1 AND kind=?2 AND tag=?3 AND minute>=?4 ORDER BY minute").ok()?;
                let pts: Vec<Value> =
                    st.query_map(params![host, k, t, from], |r| Ok(json!([r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?]))).ok()?.flatten().collect();
                out["series"] = json!({"kind": k, "tag": t, "points": pts});
            }
            Some(out)
        })
    }

    /// 线路机探测：在测节点 + 每个 (节点, 检查) 的最新结果（L2 取最新的 final 尝试）+ range 内的计数
    pub fn relay_probes(&self, relay: &str, range_s: i64, now: f64) -> Option<Value> {
        let from = now - range_s.clamp(60, 14 * 86400) as f64;
        self.with_db(|c| {
            let (targets, last_id, upd): (String, i64, f64) = c
                .query_row("SELECT targets, last_id, updated_at FROM probe_targets WHERE relay=?1", [relay], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()
                .ok()??;
            let mut latest = vec![];
            let mut st = c
                .prepare_cached(
                    "SELECT p.doc FROM probe_land p JOIN (
                       SELECT node, check_kind, MAX(measured_at) m FROM probe_land
                       WHERE relay=?1 AND measured_at>=?2 AND (check_kind!='egress' OR json_extract(doc,'$.final')=1)
                       GROUP BY node, check_kind) x
                     ON p.node=x.node AND p.check_kind=x.check_kind AND p.measured_at=x.m WHERE p.relay=?1 ORDER BY p.node, p.check_kind",
                )
                .ok()?;
            for d in st.query_map(params![relay, from], |r| r.get::<_, String>(0)).ok()?.flatten() {
                if let Ok(v) = serde_json::from_str::<Value>(&d) {
                    latest.push(v);
                }
            }
            let mut counts = vec![];
            let mut st = c
                .prepare_cached("SELECT node, check_kind, status, COUNT(*) FROM probe_land WHERE relay=?1 AND measured_at>=?2 GROUP BY node, check_kind, status ORDER BY node, check_kind")
                .ok()?;
            for r in st
                .query_map(params![relay, from], |r| Ok(json!([r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?])))
                .ok()?
                .flatten()
            {
                counts.push(r);
            }
            Some(json!({
                "relay": relay, "updated_at": upd, "last_id": last_id, "range_s": range_s,
                "targets": serde_json::from_str::<Value>(&targets).unwrap_or(Value::Null),
                "latest": latest, "counts": counts,
            }))
        })
    }

    // ---------- 告警 ----------

    /// 原生告警：native_host_offline、native_host_resource、native_cpu_steal、native_cpu_iowait。
    /// 只对有未撤销令牌的主机评估（撤销令牌 = 下线主机，不再告警）。
    pub fn evaluate(&self, now: f64) -> Vec<NativeAlert> {
        let cfg = self.cfg.clone();
        self.with_db(|c| {
            let mut out = vec![];
            let hosts: Vec<(String, String, f64, f64)> = {
                let mut st = c
                    .prepare_cached(
                        "SELECT h.host_id, h.kind, h.received_at, h.sample_ts FROM host_latest h
                         WHERE EXISTS(SELECT 1 FROM agent_tokens t WHERE t.host_id=h.host_id AND t.revoked_at IS NULL)",
                    )
                    .unwrap();
                st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap().flatten().collect()
            };
            for (host, kind, recv, _sts) in hosts {
                let age = now - recv;
                if age >= cfg.offline_s {
                    out.push(NativeAlert {
                        rule: "native_host_offline",
                        host_id: host.clone(),
                        kind: kind.clone(),
                        severity: if kind == "relay" { "critical" } else { "warning" },
                        summary: format!("{host} 的 {} 已 {:.0} 秒没有上报", if kind == "relay" { "relayd" } else { "nodeagent" }, age),
                        evidence: json!({"last_received": recv, "age_s": age.round()}),
                        active_since: recv,
                        for_s: cfg.offline_s,
                        firing: true,
                    });
                    continue; // 离线时不评估资源类规则
                }
                // 资源：内存或磁盘 ≥ 阈值，按原始样本连续成立的时长
                let recent: Vec<(f64, Option<f64>, Option<f64>)> = {
                    let mut st = c.prepare_cached("SELECT ts, mem, disk FROM host_recent WHERE host_id=?1 ORDER BY ts DESC").unwrap();
                    st.query_map([&host], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().flatten().collect()
                };
                let hot = |m: Option<f64>, d: Option<f64>| m.is_some_and(|m| m >= cfg.resource_pct) || d.is_some_and(|d| d >= cfg.resource_pct);
                if let Some(&(latest_ts, lm, ld)) = recent.first() {
                    if hot(lm, ld) {
                        let mut since = latest_ts;
                        for &(ts, m, d) in &recent {
                            if hot(m, d) {
                                since = ts;
                            } else {
                                break;
                            }
                        }
                        let what = match (lm.is_some_and(|m| m >= cfg.resource_pct), ld.is_some_and(|d| d >= cfg.resource_pct)) {
                            (true, true) => "内存和磁盘",
                            (true, false) => "内存",
                            _ => "磁盘",
                        };
                        out.push(NativeAlert {
                            rule: "native_host_resource",
                            host_id: host.clone(),
                            kind: kind.clone(),
                            severity: "warning",
                            summary: format!("{host} {what}使用率 ≥ {:.0}%（内存 {:.1}%，磁盘 {:.1}%）", cfg.resource_pct, lm.unwrap_or(0.0), ld.unwrap_or(0.0)),
                            evidence: json!({"mem_pct": lm, "disk_pct": ld, "since": since, "held_s": (latest_ts - since).round()}),
                            active_since: since,
                            for_s: cfg.resource_for_s,
                            firing: latest_ts - since >= cfg.resource_for_s,
                        });
                    }
                }
                // steal / iowait：按分钟平均值连续 ≥ 阈值
                let mins: Vec<(i64, i64, i64, i64, i64)> = {
                    let mut st = c
                        .prepare_cached("SELECT minute, steal_s, steal_n, iowait_s, iowait_n FROM host_1m WHERE host_id=?1 AND minute>=?2 ORDER BY minute DESC")
                        .unwrap();
                    st.query_map(params![&host, now as i64 - cfg.cpu_for_s as i64 - 300], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
                        .unwrap()
                        .flatten()
                        .collect()
                };
                for (rule, idx, label) in [("native_cpu_steal", 0usize, "steal"), ("native_cpu_iowait", 1usize, "iowait")] {
                    let avg = |r: &(i64, i64, i64, i64, i64)| {
                        let (s, n) = if idx == 0 { (r.1, r.2) } else { (r.3, r.4) };
                        (n > 0).then(|| s as f64 / n as f64 / 100.0)
                    };
                    let Some(first) = mins.first() else { continue };
                    // 最新一分钟必须在最近 3 分钟内
                    if (now as i64 - first.0) > 180 {
                        continue;
                    }
                    if !avg(first).is_some_and(|a| a >= cfg.cpu_pct) {
                        continue;
                    }
                    let mut since = first.0;
                    let mut prev = first.0;
                    let mut vals = vec![];
                    for r in &mins {
                        if prev - r.0 > 60 {
                            break; // 缺分钟 = 断开
                        }
                        match avg(r) {
                            Some(a) if a >= cfg.cpu_pct => {
                                since = r.0;
                                prev = r.0;
                                vals.push(a);
                            }
                            _ => break,
                        }
                    }
                    let held = (first.0 + 60 - since) as f64;
                    let mean = vals.iter().sum::<f64>() / vals.len().max(1) as f64;
                    out.push(NativeAlert {
                        rule,
                        host_id: host.clone(),
                        kind: kind.clone(),
                        severity: "warning",
                        summary: format!("{host} CPU {label} 连续 {:.0} 分钟 ≥ {:.0}%（平均 {:.1}%）", held / 60.0, cfg.cpu_pct, mean),
                        evidence: json!({"minutes": vals.len(), "mean_pct": (mean * 10.0).round() / 10.0, "since": since}),
                        active_since: since as f64,
                        for_s: cfg.cpu_for_s,
                        firing: held >= cfg.cpu_for_s,
                    });
                }
            }
            out
        })
    }

    // ---------- 维护 ----------

    pub fn purge(&self, now: f64) -> (usize, usize, usize) {
        let cfg = self.cfg.clone();
        self.with_db(|c| {
            let a = c.execute("DELETE FROM host_1m WHERE minute < ?1", [now as i64 - cfg.rollup_days * 86400]).unwrap_or(0);
            let b = c.execute("DELETE FROM xray_1m WHERE minute < ?1", [now as i64 - cfg.xray_days * 86400]).unwrap_or(0);
            let d = c.execute("DELETE FROM ingest_seen WHERE received_at < ?1", [now - cfg.seen_days as f64 * 86400.0]).unwrap_or(0);
            c.execute("DELETE FROM host_recent WHERE ts < ?1", [now - 900.0]).ok();
            c.execute("DELETE FROM probe_land WHERE measured_at < ?1", [now - 14.0 * 86400.0]).ok();
            c.execute("DELETE FROM access_raw WHERE received_at < ?1", [now - 3.0 * 86400.0]).ok();
            let _ = c.execute_batch("PRAGMA optimize;");
            (a, b, d)
        })
    }

    /// 每小时清理一次（在 tokio 运行时里调用）
    pub fn spawn_maintenance(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                iv.tick().await;
                let m = me.clone();
                let r = tokio::task::spawn_blocking(move || m.purge(now())).await;
                if let Ok((a, b, d)) = r {
                    tracing::info!(host_1m = a, xray_1m = b, seen = d, "ingest purge");
                }
            }
        });
    }
}
