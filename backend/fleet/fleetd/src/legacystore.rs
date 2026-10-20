//! 原生旧库（/var/lib/fleetd/legacy.db）：旧系统 operations.db 同构的库，由 fleetd 自己维护。
//!
//! 1. `fleetd legacy snapshot`（root）：对旧库做一致性拷贝（VACUUM INTO），连同旧进程配置里不在库中的几项
//!    （backend.json 的线路机名单、provision 公钥、软件包清单）写进 meta `fleetd:compat`，交给 fleetd 用户。
//! 2. fleetd 起一个喂数循环（10 s）从拷贝时刻接着写：
//!    - 流量账本 minutes/hours：relayd 的 xray_1m（按入口→资产映射；同一分钟整分钟覆盖，幂等、无缺口、不重复）；
//!    - relays 快照：relayd 的计数器/连接数/Xray 状态 + fleetd 观测到的线路机配置 + 过渡期转发的旧探测状态文件，
//!      速率、归属、重置判定按旧 store.ingest 逐行移植；
//!    - activity：relayd 的 fleet.access.v1 事件（id = `<线路机>:<inode>:<offset>`，与旧 bridge 同一编号，天然去重）；
//!    - hosts：旧 Komari 插件同步的形状；指标来自 nodeagent/relayd，只有连通性探针的主机按 relayd 的 TCP/出口验证判在线，ping 汇总来自 relayd 探测；
//!    - collector 心跳；monitor / behavior 的写路径见 compat::monitor_eval / compat::behavior_write。
//! 3. 读：compat::respond 在这份库上渲染旧接口（逐字节同构，见 `fleetd legacy verify`）。
use crate::app::App;
use crate::clock::Clock;
use crate::compat::{self, py, pyjson, CompatCfg, Ctx};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const DEFAULT_PATH: &str = "/var/lib/fleetd/legacy.db";

#[derive(Default, Clone, Debug, serde::Serialize)]
pub struct FeedStatus {
    pub imported: bool,
    pub snapshot_at: Option<f64>,
    pub ticks: u64,
    pub last_tick_at: f64,
    pub last_tick_ms: f64,
    pub last_error: Option<String>,
    pub errors: u64,
    pub activity_inserted: u64,
    pub activity_seen: u64,
    pub ledger_rows: u64,
    pub relays: BTreeMap<String, String>,
    pub hosts_at: Option<f64>,
    pub hosts_error: Option<String>,
    pub hosts: usize,
    /// 各阶段耗时：最近一拍 / 累计（ms），用来找 CPU 热点
    pub stage_last_ms: BTreeMap<String, f64>,
    pub stage_total_ms: BTreeMap<String, f64>,
    /// 最近一次有人读默认 /behavior 或 /activity（行为缓存按需刷新）
    pub behavior_demand_at: f64,
}

/// 只读连接池里借出的连接（用完归还：省去每次打开库、解析表结构和预编译）
pub struct Reader<'a> {
    ls: &'a LegacyStore,
    ino: u64,
    c: Option<Connection>,
}

impl std::ops::Deref for Reader<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.c.as_ref().unwrap()
    }
}

impl Drop for Reader<'_> {
    fn drop(&mut self) {
        if let Some(c) = self.c.take() {
            let mut g = self.ls.readers.lock().unwrap_or_else(|p| p.into_inner());
            if g.len() < 4 {
                g.push((self.ino, c));
            }
        }
    }
}

pub struct LegacyStore {
    pub path: PathBuf,
    pub memo: Arc<compat::Memo>,
    /// 下一拍无论 30 s 节拍都刷新主机快照（推送刷新用）
    pub force_hosts: std::sync::atomic::AtomicBool,
    /// 喂数里的策略回执核对改了版本状态：本拍提交后推 SSE policies
    pub policies_changed: std::sync::atomic::AtomicBool,
    readers: Mutex<Vec<(u64, Connection)>>,
    /// 探测一小时汇总（ping 汇总用）：每 60 s 重算一次
    probe_agg: Mutex<ProbeWin>,
    /// 最近一次有人读默认 /behavior 或 /activity 的时刻：有人看时才在喂数里刷新行为缓存
    pub behavior_demand: Mutex<f64>,
    /// 最近一次有人读默认 /activity（聚合那一份）
    pub activity_demand: Mutex<f64>,
    w: Mutex<Option<(u64, Connection)>>,
    pub status: Mutex<FeedStatus>,
    /// 数据代：每拍喂数结束、每次写入后 +1。旧接口 GET 的渲染结果按（路径+查询, 代）缓存，
    /// 同一代内任意多个查看者共用一次渲染（请求路径上不再重算）
    pub gen: std::sync::atomic::AtomicU64,
    rcache: Mutex<HashMap<String, (u64, u16, Arc<str>, f64)>>,
    pub rcache_stats: Mutex<(u64, u64)>,
}

/// 渲染缓存上限：条目数与正文总字节
const RCACHE_MAX_ENTRIES: usize = 256;
const RCACHE_MAX_BYTES: usize = 16 << 20;

type RcacheEntry = (u64, u16, Arc<str>, f64);

/// 插入一条渲染结果：先丢掉用不上的旧代条目（旧代只有长窗口在 60 s 内可复用），
/// 仍超过条目数或字节上限就按最早渲染的先丢（LRU 近似：命中不更新时间，同代内同一路径只渲染一次）
fn rcache_insert(m: &mut HashMap<String, RcacheEntry>, key: String, e: RcacheEntry, g: u64, now: f64) {
    m.remove(&key);
    m.retain(|_, v| v.0 == g || now - v.3 < 60.0);
    let add = key.len() + e.2.len();
    let mut bytes: usize = m.iter().map(|(k, v)| k.len() + v.2.len()).sum();
    if m.len() >= RCACHE_MAX_ENTRIES || bytes + add > RCACHE_MAX_BYTES {
        let mut by_age: Vec<(f64, String, usize)> = m.iter().map(|(k, v)| (v.3, k.clone(), k.len() + v.2.len())).collect();
        by_age.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        for (_, k, n) in by_age {
            if m.len() < RCACHE_MAX_ENTRIES && bytes + add <= RCACHE_MAX_BYTES {
                break;
            }
            m.remove(&k);
            bytes -= n;
        }
    }
    if add <= RCACHE_MAX_BYTES {
        m.insert(key, e);
    }
}

fn inode(p: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).ok().map(|m| m.ino())
}

impl LegacyStore {
    pub fn new(path: PathBuf) -> Arc<LegacyStore> {
        Arc::new(LegacyStore { path, memo: Arc::new(compat::Memo::default()), force_hosts: std::sync::atomic::AtomicBool::new(false), policies_changed: Default::default(), readers: Mutex::new(vec![]), probe_agg: Mutex::new(ProbeWin::default()), behavior_demand: Mutex::new(0.0), activity_demand: Mutex::new(0.0), w: Mutex::new(None), status: Mutex::new(FeedStatus::default()), gen: Default::default(), rcache: Default::default(), rcache_stats: Default::default() })
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// 写连接（文件被 snapshot 原子替换后自动重开）
    pub fn with_w<R>(&self, f: impl FnOnce(&Connection) -> R) -> Option<R> {
        let ino = inode(&self.path)?;
        let mut g = self.w.lock().unwrap_or_else(|p| p.into_inner());
        if g.as_ref().map(|(i, _)| *i) != Some(ino) {
            let c = Connection::open(&self.path).ok()?;
            c.set_prepared_statement_cache_capacity(256);
            let _ = c.busy_timeout(std::time::Duration::from_secs(10));
            // mmap：读页走内存映射（干净的文件页），不再逐页 pread 拷贝；页缓存 16 MB（写入要改的页；原 32 MB 会被每小时的保留期删除填满、常驻堆里）
            let _ = c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA mmap_size=2147418112; PRAGMA cache_size=-16384;");
            *g = Some((ino, c));
        }
        g.as_ref().map(|(_, c)| f(c))
    }

    /// 只读连接（每次新开：渲染与喂数并行，WAL 下互不阻塞）
    pub fn reader(&self) -> Option<Reader<'_>> {
        let ino = inode(&self.path)?;
        let pooled = {
            let mut g = self.readers.lock().unwrap_or_else(|p| p.into_inner());
            g.retain(|(i, _)| *i == ino);
            g.pop()
        };
        let c = match pooled {
            Some((_, c)) => c,
            None => {
                let c = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()?;
                let _ = c.busy_timeout(std::time::Duration::from_secs(10));
                let _ = c.execute_batch("PRAGMA mmap_size=2147418112; PRAGMA cache_size=-16384;");
                c.set_prepared_statement_cache_capacity(128);
                c
            }
        };
        Some(Reader { ls: self, ino, c: Some(c) })
    }

    pub fn compat_cfg(c: &Connection) -> CompatCfg {
        let v = py::meta(c, "fleetd:compat", Value::Null).unwrap_or(Value::Null);
        if v.is_null() {
            return CompatCfg::stage_defaults(c);
        }
        CompatCfg {
            config_relays: v["config_relays"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default(),
            public_key: v["public_key"].as_str().unwrap_or("").to_string(),
            packages: v.get("packages").cloned().unwrap_or(json!({})),
            cache_max_age: Some(180.0),
            memo: None,
            redact_internal: true,
            events_split: true,
            domain_collapse: true,
            path_fresh_s: Some(240.0),
            egress_ip_check: false,
        }
    }

    /// 在原生旧库上渲染旧接口（阻塞；在 spawn_blocking 里调用）
    pub fn bump(&self) {
        self.gen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.memo.data_gen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// 内存读数：渲染缓存条目 / 字节 / 旧代条目，连接池
    pub fn mem_stats(&self) -> Value {
        let g = self.gen.load(std::sync::atomic::Ordering::Relaxed);
        let m = self.rcache.lock().unwrap();
        let bytes: usize = m.iter().map(|(k, v)| k.len() + v.2.len()).sum();
        let stale = m.values().filter(|v| v.0 != g).count();
        let mut top: Vec<(usize, &String)> = m.iter().map(|(k, v)| (v.2.len(), k)).collect();
        top.sort_by(|a, b| b.0.cmp(&a.0));
        json!({"rcache_entries": m.len(), "rcache_bytes": bytes, "rcache_stale": stale,
            "rcache_top": top.iter().take(8).map(|(n, k)| json!([k, n])).collect::<Vec<_>>(),
            "readers_pooled": self.readers.lock().unwrap_or_else(|p| p.into_inner()).len(), "memo": self.memo.mem_stats()})
    }

    pub fn drop_caches(&self, what: &str) {
        match what {
            "rcache" => self.rcache.lock().unwrap().clear(),
            "conns" => {
                self.readers.lock().unwrap_or_else(|p| p.into_inner()).clear();
                *self.w.lock().unwrap_or_else(|p| p.into_inner()) = None;
            }
            other => self.memo.drop_caches(other),
        }
    }

    /// render + 按数据代缓存（只缓存 200；同一代内同一路径+查询只渲染一次）
    pub fn render_cached(&self, path_q: &str, now: f64) -> Option<(u16, Arc<str>)> {
        let g = self.gen.load(std::sync::atomic::Ordering::Relaxed);
        let p = path_q.split('?').next().unwrap_or(path_q);
        if p == "/behavior" {
            *self.behavior_demand.lock().unwrap() = now;
        } else if p == "/activity" {
            *self.activity_demand.lock().unwrap() = now;
        }
        // 长窗口（seconds > 1 天，7 / 14 天视图）允许 60 s 内的旧结果跨代复用：分钟级新鲜度足够，避免每拍重算
        let long = path_q.contains("seconds=") && compat::split_query(path_q).1.get("seconds").and_then(|x| x.parse::<f64>().ok()).is_some_and(|x| x > 86400.0);
        if let Some((cg, st, body, at)) = self.rcache.lock().unwrap().get(path_q).cloned() {
            if cg == g || (long && now - at < 60.0) {
                self.rcache_stats.lock().unwrap().0 += 1;
                return Some((st, body));
            }
        }
        let (st, body) = self.render(path_q, now)?;
        let body: Arc<str> = Arc::from(body);
        self.rcache_stats.lock().unwrap().1 += 1;
        if st == 200 {
            let mut m = self.rcache.lock().unwrap();
            rcache_insert(&mut m, path_q.to_string(), (g, st, body.clone(), now), g, now);
        }
        Some((st, body))
    }

    pub fn render(&self, path_q: &str, now: f64) -> Option<(u16, String)> {
        let c = self.reader()?;
        let mut cfg = Self::compat_cfg(&c);
        cfg.memo = Some(self.memo.clone());
        let (p, q) = compat::split_query(path_q);
        if p == "/behavior" {
            *self.behavior_demand.lock().unwrap() = now;
        } else if p == "/activity" {
            *self.activity_demand.lock().unwrap() = now;
        }
        Some(compat::respond(&Ctx { db: &c, now, cfg: &cfg }, &p, &q))
    }
}

// ─── snapshot（root CLI）────────────────────────────────────────────────────────────────────────────

fn lookup_ids(user: &str) -> Option<(u32, u32)> {
    let t = std::fs::read_to_string("/etc/passwd").ok()?;
    t.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() > 3 && f[0] == user).then(|| (f[2].parse().ok(), f[3].parse().ok())).and_then(|(a, b)| Some((a?, b?)))
    })
}

/// `fleetd legacy snapshot`：旧库一致性拷贝 + 旧进程配置；返回写入的 meta
pub fn snapshot(from: &Path, to: &Path, backend: Option<&Path>, owner: Option<&str>, now: f64) -> Result<Value, String> {
    let src = Connection::open_with_flags(from, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e| format!("打开 {}：{e}", from.display()))?;
    src.busy_timeout(std::time::Duration::from_secs(60)).ok();
    let tmp = to.with_extension("db.tmp");
    let _ = std::fs::remove_file(&tmp);
    let t0 = std::time::Instant::now();
    src.execute("VACUUM INTO ?1", [tmp.display().to_string()]).map_err(|e| format!("VACUUM INTO：{e}"))?;
    let copy_s = t0.elapsed().as_secs_f64();
    let base = from.parent().unwrap_or(Path::new("/"));
    // 旧进程配置
    let mut config_relays: Vec<String> = vec![];
    if let Some(b) = backend {
        let t = std::fs::read_to_string(b).map_err(|e| format!("读 {}：{e}", b.display()))?;
        let v: Value = serde_json::from_str(&t).map_err(|e| format!("{}：{e}", b.display()))?;
        config_relays = v["relays"].as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
    }
    let public_key = std::fs::read_to_string(base.join("provision.key.pub")).unwrap_or_default().trim().to_string();
    let mut packages = Map::new();
    if let Ok(t) = std::fs::read_to_string(base.join("packages/packages.json")) {
        if let Ok(Value::Object(o)) = serde_json::from_str::<Value>(&t) {
            for (k, v) in o {
                let file = v["file"].as_str().unwrap_or("");
                if !file.is_empty() && base.join("packages").join(file).exists() {
                    let mut m = v.as_object().cloned().unwrap_or_default();
                    m.shift_remove("file");
                    packages.insert(k, Value::Object(m));
                }
            }
        }
    }
    let dst = Connection::open(&tmp).map_err(|e| e.to_string())?;
    dst.execute_batch("PRAGMA journal_mode=WAL;").map_err(|e| e.to_string())?;
    let integrity: String = dst.query_row("PRAGMA quick_check", [], |r| r.get(0)).map_err(|e| e.to_string())?;
    if integrity != "ok" {
        return Err(format!("拷贝校验失败：{integrity}"));
    }
    let compat = json!({"config_relays": config_relays, "public_key": public_key, "packages": packages});
    let max_act: Option<f64> = dst.query_row("SELECT max(ts) FROM activity", [], |r| r.get(0)).ok().flatten();
    let snap = json!({"at": now, "source": from.display().to_string(), "copy_s": (copy_s * 10.0).round() / 10.0, "activity_max_ts": max_act});
    for (k, v) in [("fleetd:compat", &compat), ("fleetd:snapshot", &snap)] {
        dst.execute("INSERT INTO meta VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![k, pyjson::dumps(v)]).map_err(|e| e.to_string())?;
    }
    dst.execute("DELETE FROM meta WHERE key='fleetd:feed'", []).map_err(|e| e.to_string())?;
    drop(dst);
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    for ext in ["db-wal", "db-shm"] {
        let side = tmp.with_extension(ext);
        let _ = std::fs::remove_file(&side);
    }
    if let Some(u) = owner {
        let (uid, gid) = lookup_ids(u).ok_or(format!("没有用户 {u}"))?;
        std::os::unix::fs::chown(&tmp, Some(uid), Some(gid)).map_err(|e| format!("chown：{e}"))?;
    }
    for ext in ["db-wal", "db-shm"] {
        let _ = std::fs::remove_file(to.with_extension(ext));
    }
    std::fs::rename(&tmp, to).map_err(|e| format!("替换 {}：{e}", to.display()))?;
    Ok(json!({"compat": compat, "snapshot": snap}))
}

/// `fleetd legacy catchup`（root，下线时、旧后端已停止后）：把快照之后用户在旧系统里做的写入并入原生旧库。
/// 系统自己产生的数据（流量、访问、健康、行为）原生喂数一直在写，不动；只并入人的操作：
/// 资产（含档案/续费后的新 revision）、续费记录、API 密钥与调用记录、策略草稿/版本、接入任务、fleet 表、
/// 非 system 的审计、故障确认与行为确认。
pub fn catchup(from: &Path, to: &Path, owner: Option<&str>) -> Result<Value, String> {
    let snap_at: f64 = {
        let c = Connection::open_with_flags(to, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| e.to_string())?;
        meta_get(&c, "fleetd:snapshot")["at"].as_f64().ok_or("原生旧库没有快照记录（先 fleetd legacy snapshot）")?
    };
    let c = Connection::open(to).map_err(|e| e.to_string())?;
    c.busy_timeout(std::time::Duration::from_secs(30)).ok();
    c.execute("ATTACH DATABASE ?1 AS old", [format!("file:{}?mode=ro", from.display())]).map_err(|e| format!("ATTACH：{e}"))?;
    let tx = c.unchecked_transaction().map_err(|e| e.to_string())?;
    let mut out = Map::new();
    let mut n = |k: &str, sql: &str, p: &[&dyn rusqlite::ToSql]| -> Result<(), String> {
        let x = tx.execute(sql, p).map_err(|e| format!("{k}：{e}"))?;
        out.insert(k.into(), json!(x));
        Ok(())
    };
    // 资产：旧库里 revision 更新的覆盖原生的（原生在下线前不改资产）
    n("assets", "INSERT INTO main.assets(uuid,data) SELECT o.uuid,o.data FROM old.assets o LEFT JOIN main.assets m ON m.uuid=o.uuid
        WHERE m.uuid IS NULL OR coalesce(json_extract(o.data,'$.revision'),1) > coalesce(json_extract(m.data,'$.revision'),1)
           OR (json_extract(o.data,'$.revision') IS json_extract(m.data,'$.revision') AND o.data<>m.data AND json_extract(o.data,'$.updated_at') > ?1)
        ON CONFLICT(uuid) DO UPDATE SET data=excluded.data", &[&snap_at])?;
    n("renewals", "INSERT OR IGNORE INTO main.renewals SELECT * FROM old.renewals WHERE ts>?1", &[&snap_at])?;
    n("api_keys", "INSERT OR REPLACE INTO main.api_keys SELECT * FROM old.api_keys", &[])?;
    n("api_key_usage", "INSERT INTO main.api_key_usage SELECT * FROM old.api_key_usage WHERE ts>?1
        AND NOT EXISTS(SELECT 1 FROM main.api_key_usage m WHERE m.ts=old.api_key_usage.ts AND m.key_id=old.api_key_usage.key_id)", &[&snap_at])?;
    n("drafts", "INSERT OR REPLACE INTO main.drafts SELECT * FROM old.drafts", &[])?;
    n("drafts_removed", "DELETE FROM main.drafts WHERE scope NOT IN (SELECT scope FROM old.drafts)", &[])?;
    n("versions", "INSERT OR REPLACE INTO main.versions SELECT * FROM old.versions", &[])?;
    n("jobs", "INSERT OR REPLACE INTO main.jobs SELECT * FROM old.jobs WHERE created_at>?1 OR coalesce(finished_at,0)>?1", &[&snap_at])?;
    n("fleet", "INSERT OR REPLACE INTO main.fleet SELECT * FROM old.fleet", &[])?;
    n("audit_user", "INSERT INTO main.audit(ts,actor,action,target,detail) SELECT ts,actor,action,target,detail FROM old.audit WHERE ts>?1 AND actor<>'system' ORDER BY id", &[&snap_at])?;
    n("incident_acks", "UPDATE main.incidents SET ack_at=o.ack_at, ack_actor=o.ack_actor, note=o.note FROM old.incidents o
        WHERE o.ack_at>?1 AND main.incidents.asset=o.asset AND main.incidents.key=o.key AND abs(main.incidents.opened_at-o.opened_at)<=120 AND main.incidents.ack_at IS NULL", &[&snap_at])?;
    n("behavior_acks", "UPDATE main.behavior_events SET ack_at=o.ack_at FROM old.behavior_events o
        WHERE o.ack_at>?1 AND main.behavior_events.asset=o.asset AND main.behavior_events.kind=o.kind AND main.behavior_events.subject=o.subject
          AND main.behavior_events.window=o.window AND main.behavior_events.ack_at IS NULL", &[&snap_at])?;
    n("meta_policies", "INSERT OR REPLACE INTO main.meta SELECT key,value FROM old.meta WHERE key='applied_policies'", &[])?;
    let now = crate::clock::SystemClock.now();
    meta_set(&tx, "fleetd:catchup", &json!({"at": now, "source": from.display().to_string(), "snapshot_at": snap_at, "counts": out.clone()})).map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    c.execute("DETACH DATABASE old", []).map_err(|e| e.to_string())?;
    c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
    drop(c);
    if let Some(u) = owner {
        let (uid, gid) = lookup_ids(u).ok_or(format!("没有用户 {u}"))?;
        for ext in ["", "-wal", "-shm"] {
            let p = PathBuf::from(format!("{}{ext}", to.display()));
            if p.exists() {
                std::os::unix::fs::chown(&p, Some(uid), Some(gid)).map_err(|e| format!("chown {}：{e}", p.display()))?;
            }
        }
    }
    Ok(Value::Object(out))
}

// ─── 喂数 ───────────────────────────────────────────────────────────────────────────────────────────

fn meta_get(c: &Connection, k: &str) -> Value {
    py::meta(c, k, Value::Null).unwrap_or(Value::Null)
}

fn meta_set(c: &Connection, k: &str, v: &Value) -> rusqlite::Result<()> {
    c.execute("INSERT INTO meta VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![k, pyjson::dumps(v)])?;
    Ok(())
}

fn assets(c: &Connection) -> Vec<Value> {
    let mut st = match c.prepare("SELECT data FROM assets") {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    st.query_map([], |r| r.get::<_, String>(0)).map(|it| it.flatten().filter_map(|s| serde_json::from_str(&s).ok()).collect()).unwrap_or_default()
}

fn relay_snapshot(c: &Connection, relay: &str) -> Option<Value> {
    c.query_row("SELECT data FROM relays WHERE relay=?", [relay], |r| r.get::<_, String>(0)).optional().ok().flatten().and_then(|s| serde_json::from_str(&s).ok())
}

/// 入口端口 → 资产（旧 store.ingest 的 by_port：同线路机按 relay_port，后者覆盖前者；主入口归线路机资产）
fn port_map(all: &[Value], relay: &str, masters: &[i64]) -> BTreeMap<i64, Value> {
    let mut m = BTreeMap::new();
    for a in all.iter().filter(|a| a["relay"].as_str() == Some(relay)) {
        if let Some(p) = a["relay_port"].as_i64() {
            m.insert(p, a.clone());
        }
    }
    if !masters.is_empty() {
        if let Some(master) = all.iter().find(|a| a["team"] == "线路机" && a["name"].as_str() == Some(relay)) {
            for p in masters {
                m.insert(*p, master.clone());
            }
        }
    }
    m
}

fn masters_of(snap: &Value) -> Vec<i64> {
    snap["inbounds"].as_array().map(|v| v.iter().filter(|i| !py::truthy(&i["outbound_tag"])).filter_map(|i| i["port"].as_i64()).collect()).unwrap_or_default()
}

fn asset_uuid(map: &BTreeMap<i64, Value>, relay: &str, port: i64) -> String {
    map.get(&port).and_then(|a| a["uuid"].as_str().map(String::from)).unwrap_or_else(|| format!("direct:{relay}:{port}"))
}

/// relayd 看到的一台线路机
struct RelaydView {
    received_at: f64,
    sample: Value,
    xray: Value,
    /// tag → (cum_up, cum_down, raw_up, raw_down, rate_up, rate_down, ts)
    latest: BTreeMap<String, (i64, i64, i64, i64, f64, f64, f64)>,
    access_source: Option<Value>,
    legacy_probe: Option<Value>,
    legacy_paths: Option<Value>,
    /// relayd 自己的 path 探测拼成的旧 paths 形状（edge.native_paths 打开时）
    native_paths: Option<Value>,
    /// relayd 自己的 tcp/egress 探测拼成的旧 probe 形状（edge.native_probe 打开时）
    native_probe: Option<Value>,
}

fn relayd_view(app: &App, host: &str) -> Option<RelaydView> {
    let h = host.to_string();
    app.ingest.with_db(move |c| {
        let (received_at, sample, xray): (f64, String, Option<String>) =
            c.query_row("SELECT received_at, sample, xray FROM host_latest WHERE host_id=?1 AND kind='relay'", [&h], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional().ok()??;
        let mut latest = BTreeMap::new();
        if let Ok(mut st) = c.prepare("SELECT tag, cum_up, cum_down, raw_up, raw_down, rate_up, rate_down, ts FROM xray_latest WHERE host_id=?1 AND kind='inbound'") {
            if let Ok(rows) = st.query_map([&h], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))) {
                for (t, v) in rows.flatten() {
                    latest.insert(t, v);
                }
            }
        }
        let access_source: Option<Value> = c.query_row("SELECT source FROM access_state WHERE host_id=?1", [&h], |r| r.get::<_, String>(0)).optional().ok().flatten().and_then(|s| serde_json::from_str(&s).ok());
        let lp: Option<(Option<String>, Option<String>)> = c.query_row("SELECT probe, paths FROM legacy_probe WHERE host_id=?1", [&h], |r| Ok((r.get(0)?, r.get(1)?))).optional().ok().flatten();
        let (legacy_probe, legacy_paths) = match lp {
            Some((a, b)) => (a.and_then(|s| serde_json::from_str(&s).ok()), b.and_then(|s| serde_json::from_str(&s).ok())),
            None => (None, None),
        };
        Some(RelaydView {
            native_paths: None,
            native_probe: None,
            received_at,
            sample: serde_json::from_str(&sample).unwrap_or(Value::Null),
            xray: xray.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null),
            latest,
            access_source,
            legacy_probe,
            legacy_paths,
        })
    })
}

/// relayd 的 path 探测（最近 5 分钟、每节点最新一条）→ 旧 path-status 形状；没有结果的节点沿用旧文件里的那一行
fn native_paths(app: &App, host: &str, now: f64, legacy: Option<&Value>) -> Option<Value> {
    let h = host.to_string();
    let rows: Vec<(String, String)> = app.ingest.with_db(move |c| {
        let mut out = vec![];
        if let Ok(mut st) = c.prepare("SELECT node, doc, max(measured_at) FROM probe_land WHERE relay=?1 AND check_kind='path' AND measured_at>=?2 GROUP BY node") {
            if let Ok(it) = st.query_map(rusqlite::params![h, now - 300.0], |r| Ok((r.get(0)?, r.get(1)?))) {
                out = it.flatten().collect();
            }
        }
        out
    });
    if rows.is_empty() {
        return None;
    }
    let mut latest: BTreeMap<String, Value> = BTreeMap::new();
    for (node, doc) in rows {
        if let Ok(d) = serde_json::from_str::<Value>(&doc) {
            latest.insert(node, d);
        }
    }
    let mut assets: Vec<Value> = legacy.and_then(|l| l["assets"].as_array().cloned()).unwrap_or_default();
    let mut updated = 0.0f64;
    for (node, d) in &latest {
        if node.starts_with("inbound-") {
            continue; // 没有资产身份的入口（直连主入口）：旧文件里也没有
        }
        let p = &d["path"];
        let at = d["measured_at"].clone();
        updated = updated.max(at.as_f64().unwrap_or(0.0));
        let row = json!({"uuid": node, "healthy": p["healthy"].as_bool().unwrap_or(false) || probe_ok(d), "measured_at": at, "tcp_at": at, "egress_at": at,
            "actual_ip": d["actual_ip"], "expected_ip": d["expected_ip"], "egress_ms": p.get("ms").cloned().unwrap_or(d["value_ms"].clone()),
            "rc": p.get("rc").cloned().unwrap_or(d["legacy_rc"].clone()), "phase": p["phase"], "attempts": p["attempts"]});
        match assets.iter_mut().find(|a| a["uuid"].as_str() == Some(node.as_str())) {
            Some(a) => *a = row,
            None => assets.push(row),
        }
    }
    Some(json!({"updated_at": py::f(updated), "assets": assets}))
}

/// relayd 的 tcp / egress 探测 → 旧 komari-relay-probe status.json 的行（每节点：最近一次 tcp、最近一次出口验证尝试）。
/// 节点清单与名称取 relayd 上报的 probe_targets；publishes_presence 沿用旧文件（没有则 false）。
fn native_probe(app: &App, host: &str, now: f64, legacy: Option<&Value>) -> Option<Value> {
    let h = host.to_string();
    let (targets, rows): (Vec<Value>, Vec<(String, String, String)>) = app.ingest.with_db(move |c| {
        let t: Vec<Value> = c
            .query_row("SELECT targets FROM probe_targets WHERE relay=?1", [&h], |r| r.get::<_, String>(0))
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        let mut out = vec![];
        if let Ok(mut st) = c.prepare("SELECT node, check_kind, doc, max(measured_at) FROM probe_land WHERE relay=?1 AND check_kind IN ('tcp','egress') AND measured_at>=?2 GROUP BY node, check_kind") {
            if let Ok(it) = st.query_map(rusqlite::params![h, now - 900.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))) {
                out = it.flatten().collect();
            }
        }
        (t, out)
    });
    if targets.is_empty() {
        return None;
    }
    let mut last: BTreeMap<(String, String), Value> = BTreeMap::new();
    for (node, kind, doc) in rows {
        if let Ok(d) = serde_json::from_str::<Value>(&doc) {
            last.insert((node, kind), d);
        }
    }
    let presence = |u: &str| -> Value {
        legacy.and_then(|l| l["assets"].as_array().and_then(|a| a.iter().find(|x| x["uuid"].as_str() == Some(u)).map(|x| x["publishes_presence"].clone()))).unwrap_or(json!(false))
    };
    let mut updated = legacy.and_then(|l| l["updated_at"].as_f64()).unwrap_or(0.0);
    // relayd 不测的节点（例如 SOCKS5 落地）沿用旧文件那一行；relayd 测的节点用原生结果替换
    let mut assets: Vec<Value> = legacy.and_then(|l| l["assets"].as_array().cloned()).unwrap_or_default();
    for t in &targets {
        let Some(u) = t["node"].as_str() else { continue };
        let tcp = last.get(&(u.to_string(), "tcp".to_string()));
        let eg = last.get(&(u.to_string(), "egress".to_string()));
        for d in [tcp, eg].into_iter().flatten() {
            updated = updated.max(d["measured_at"].as_f64().unwrap_or(0.0));
        }
        let tcp_ok = tcp.is_some_and(|d| d["status"] == "ok");
        let eg_ok = eg.is_some_and(|d| probe_ok(d));
        let eg_final = eg.is_some_and(|d| d["final"] != false);
        let ms = |d: Option<&Value>| d.and_then(|d| d["value_ms"].as_f64()).map(|x| json!(x.round().max(1.0) as i64));
        let attempt = eg.map(|d| if !eg_ok && eg_final { json!(3) } else { d["attempt"].clone() }).unwrap_or(Value::Null);
        let row = json!({
            "name": t["name"], "uuid": u, "target": format!("{}:{}", t["addr"].as_str().unwrap_or(""), t["port"].as_i64().unwrap_or(0)),
            "expected_ip": t["expected_ip"],
            "tcp_ms": if tcp_ok { ms(tcp).unwrap_or(Value::Null) } else if tcp.is_some() { json!(-1) } else { Value::Null },
            "egress_ms": if eg_ok { ms(eg).unwrap_or(Value::Null) } else { Value::Null },
            "tcp_at": tcp.map(|d| d["measured_at"].clone()).unwrap_or(Value::Null),
            "egress_at": eg.map(|d| d["measured_at"].clone()).unwrap_or(Value::Null),
            "actual_ip": eg.map(|d| d["actual_ip"].clone()).unwrap_or(Value::Null),
            "egress_rc": eg.map(|d| d["legacy_rc"].clone()).unwrap_or(Value::Null),
            "egress_phase": Value::Null,
            "egress_attempt": attempt,
            "tcp_rc": tcp.map(|d| d["legacy_rc"].clone()).unwrap_or(Value::Null),
            "tcp_phase": "connect",
            "tcp_error": if tcp.is_some() && !tcp_ok { tcp.map(|d| d["reason_code"].clone()).unwrap_or(Value::Null) } else { Value::Null },
            "ingress_ms": Value::Null,
            "ingress_error": Value::Null,
            "egress_error": if eg.is_some() && !eg_ok { eg.map(|d| d["reason_code"].clone()).unwrap_or(Value::Null) } else { Value::Null },
            "healthy": tcp_ok && eg_ok,
            "source_valid": {"valid": true, "reason": null},
            "publishes_presence": presence(u),
        });
        match assets.iter_mut().find(|a| a["uuid"].as_str() == Some(u)) {
            Some(a) => *a = row,
            None => assets.push(row),
        }
    }
    Some(json!({"updated_at": py::f(updated), "assets": assets}))
}

/// 线路机自身的旧 probe 形状行（没有 relayd 的线路机自检结果时 None）
fn relay_self_row(app: &App, host: &str, ra: &Value, now: f64) -> Option<Value> {
    let uuid = ra["uuid"].as_str()?.to_string();
    let h = host.to_string();
    let (tcp, eg): (Option<Value>, Option<Value>) = app.ingest.with_db(move |c| {
        let one = |sql: &str, args: &[&dyn rusqlite::ToSql]| -> Option<Value> {
            c.query_row(sql, args, |r| r.get::<_, String>(0)).ok().and_then(|s| serde_json::from_str(&s).ok())
        };
        let since = now - 900.0;
        (
            one("SELECT doc FROM probe_land WHERE node=?1 AND check_kind='tcp' AND measured_at>=?2 ORDER BY measured_at DESC LIMIT 1", &[&uuid, &since]),
            one("SELECT doc FROM probe_land WHERE node=?1 AND relay=?2 AND check_kind='egress' AND measured_at>=?3 ORDER BY measured_at DESC LIMIT 1", &[&uuid, &h, &since]),
        )
    });
    if tcp.is_none() && eg.is_none() {
        return None;
    }
    let ok = |d: &Option<Value>| d.as_ref().is_some_and(probe_ok);
    let ms = |d: &Option<Value>| d.as_ref().and_then(|d| d["value_ms"].as_f64()).map(|x| json!(x.round().max(1.0) as i64)).unwrap_or(Value::Null);
    let g = |d: &Option<Value>, k: &str| d.as_ref().map(|d| d[k].clone()).unwrap_or(Value::Null);
    let eg_final = eg.as_ref().is_some_and(|d| d["final"] != false);
    Some(json!({
        "name": ra["name"], "uuid": ra["uuid"], "target": g(&tcp, "target"), "expected_ip": ra["ip"],
        "tcp_ms": if ok(&tcp) { ms(&tcp) } else if tcp.is_some() { json!(-1) } else { Value::Null },
        "egress_ms": if ok(&eg) { ms(&eg) } else { Value::Null },
        "tcp_at": g(&tcp, "measured_at"), "egress_at": g(&eg, "measured_at"), "actual_ip": g(&eg, "actual_ip"),
        "egress_rc": g(&eg, "legacy_rc"), "egress_phase": Value::Null,
        "egress_attempt": if eg.is_some() && !ok(&eg) && eg_final { json!(3) } else { g(&eg, "attempt") },
        "tcp_rc": g(&tcp, "legacy_rc"), "tcp_phase": "connect",
        "tcp_error": if tcp.is_some() && !ok(&tcp) { g(&tcp, "reason_code") } else { Value::Null },
        "ingress_ms": Value::Null, "ingress_error": Value::Null,
        "egress_error": if eg.is_some() && !ok(&eg) { g(&eg, "reason_code") } else { Value::Null },
        "healthy": ok(&tcp) && ok(&eg), "source_valid": {"valid": true, "reason": null}, "publishes_presence": false,
        "check_source": {"tcp": g(&tcp, "relay"), "egress": host}
    }))
}

fn counter_delta(prev: Option<i64>, cur: i64, old_core: &Value, core: &Value) -> i64 {
    match prev {
        None => 0,
        Some(p) => {
            if old_core != core || cur < p {
                cur.max(0)
            } else {
                (cur - p).max(0)
            }
        }
    }
}

fn describe_new(ib_cfg: &Value, managed: &Value) -> (Value, Value, Value, Value) {
    let tag = ib_cfg["tag"].as_str().unwrap_or("");
    let rule = managed["routing_rules"].as_array().and_then(|rs| {
        rs.iter().find(|r| r["inboundTag"].as_array().is_some_and(|t| t.iter().any(|x| x == tag)) && ["domain", "ip", "source", "port", "network", "protocol"].iter().all(|k| !py::truthy(&r[*k])))
    });
    let Some(ot) = rule.and_then(|r| r["outboundTag"].as_str()) else { return (Value::Null, Value::Null, Value::Null, Value::Null) };
    let ob = managed["outbounds"].as_array().and_then(|o| o.iter().find(|x| x["tag"] == ot)).cloned().unwrap_or(Value::Null);
    let peer = ob["settings"]["servers"].get(0).or_else(|| ob["settings"]["vnext"].get(0)).cloned().unwrap_or(Value::Null);
    (json!(ot), peer["address"].clone(), peer["port"].clone(), ob["protocol"].clone())
}

/// 用 relayd 的数据生成一份旧格式的线路机快照（静态字段取上一份快照，按观测到的配置更新）
fn build_snapshot(prev: &Value, relay: &str, v: &RelaydView, managed: &Value, remarks: &BTreeMap<i64, String>, cum: &mut Map<String, Value>) -> Value {
    let mut snap = prev.clone();
    let ts = v.sample["ts"].as_f64().unwrap_or(v.received_at);
    let conns = &v.xray["conns"];
    let srcs = &v.xray["srcs"];
    let mut by_port: BTreeMap<i64, Value> = BTreeMap::new();
    for ib in prev["inbounds"].as_array().cloned().unwrap_or_default() {
        if let Some(p) = ib["port"].as_i64() {
            by_port.insert(p, ib);
        }
    }
    let cfg_inbounds: Vec<Value> = managed["inbounds"].as_array().cloned().unwrap_or_default();
    let cfg_ports: BTreeMap<i64, Value> = cfg_inbounds.iter().filter_map(|i| i["port"].as_i64().map(|p| (p, i.clone()))).collect();
    let mut next_id = by_port.values().filter_map(|i| i["id"].as_i64()).max().unwrap_or(0) + 1;
    for (p, ibc) in &cfg_ports {
        if !by_port.contains_key(p) {
            let (ot, tgt, tp, proto) = describe_new(ibc, managed);
            let mut m = Map::new();
            m.insert("id".into(), json!(next_id));
            next_id += 1;
            for (k, val) in [("tag", ibc["tag"].clone()), ("port", json!(p)), ("remark", Value::String(String::new())), ("enable", json!(1)), ("up", json!(0)), ("down", json!(0)),
                ("total", json!(0)), ("expiry_time", json!(0)), ("security", Value::Null), ("reality", Value::Null), ("inbound_protocol", Value::Null),
                ("outbound_tag", ot), ("target", tgt), ("target_port", tp), ("protocol", proto), ("supported", json!(false)), ("client_uuid", Value::Null),
                ("clients", json!([])), ("counter_up", Value::Null), ("counter_down", Value::Null), ("tcp_connections", json!(0)), ("source_ips", json!([]))] {
                m.insert(k.into(), val);
            }
            by_port.insert(*p, Value::Object(m));
        }
    }
    let mut out = vec![];
    for (p, mut ib) in by_port {
        let tag = ib["tag"].as_str().unwrap_or("").to_string();
        let m = ib.as_object_mut().unwrap();
        if let Some(c) = cfg_ports.get(&p) {
            m.insert("enable".into(), json!(1));
            // 出站（落地地址 / 端口 / 协议）按当前观测到的配置刷新：落地换 IP、外部改动被采纳后，固定出口绑定跟着变
            let (mut ot, mut tgt, mut tp, mut proto) = describe_new(c, managed);
            if ot.is_null() {
                // 节点入口的路由规则不在受管部分：按快照里已有的出站 tag 找受管出站
                if let Some(t) = m.get("outbound_tag").and_then(|v| v.as_str()).map(String::from) {
                    if let Some(ob) = managed["outbounds"].as_array().and_then(|o| o.iter().find(|x| x["tag"] == t.as_str())) {
                        let peer = ob["settings"]["servers"].get(0).or_else(|| ob["settings"]["vnext"].get(0)).cloned().unwrap_or(Value::Null);
                        (ot, tgt, tp, proto) = (json!(t), peer["address"].clone(), peer["port"].clone(), ob["protocol"].clone());
                    }
                }
            }
            if !ot.is_null() && !tgt.is_null() {
                m.insert("outbound_tag".into(), ot);
                m.insert("target".into(), tgt);
                m.insert("target_port".into(), tp);
                m.insert("protocol".into(), proto);
            }
            let ss = &c["streamSettings"];
            m.insert("security".into(), ss["security"].clone());
            let rs = &ss["realitySettings"];
            let dest = if rs.get("target").is_some() { rs["target"].clone() } else { rs["dest"].clone() };
            m.insert("reality".into(), json!({"dest": dest, "sni": rs["serverNames"].get(0).cloned().unwrap_or(Value::Null)}));
            m.insert("inbound_protocol".into(), c["protocol"].clone());
            let clients = c["settings"]["clients"].as_array().cloned().unwrap_or_default();
            m.insert("client_uuid".into(), clients.first().map(|x| x["id"].clone()).unwrap_or(Value::Null));
            let old: Vec<Value> = m.get("clients").and_then(|v| v.as_array().cloned()).unwrap_or_default();
            let cl: Vec<Value> = clients
                .iter()
                .map(|x| {
                    let label: String = x["email"].as_str().unwrap_or("").chars().take(120).collect();
                    let o = old.iter().find(|o| o["label"].as_str() == Some(label.as_str()));
                    json!({"label": label, "enabled": o.map(|o| o["enabled"].clone()).unwrap_or(json!(true)),
                        "quota_bytes": o.map(|o| o["quota_bytes"].clone()).unwrap_or(json!(0)), "expires_ms": o.map(|o| o["expires_ms"].clone()).unwrap_or(json!(0))})
                })
                .collect();
            m.insert("clients".into(), Value::Array(cl));
            let sup = m["inbound_protocol"] == "vless" && m["security"] == "reality";
            m.insert("supported".into(), json!(sup));
        } else if !cfg_ports.is_empty() {
            // fleetd 停用的入口（配置里已没有）：旧 x-ui 里是 enable=0
            m.insert("enable".into(), json!(0));
        }
        if let Some(r) = remarks.get(&p) {
            m.insert("remark".into(), json!(r));
        }
        // 计数器：Xray 原始值；x-ui 口径的累计 up/down = 接管时的值 + relayd 累计增量
        match v.latest.get(&tag) {
            Some(&(cu, cd, ru, rd, _, _, _)) => {
                m.insert("counter_up".into(), json!(ru));
                m.insert("counter_down".into(), json!(rd));
                let anchor = cum.entry(tag.clone()).or_insert_with(|| json!({"cum_up": cu, "cum_down": cd, "up": m.get("up").cloned().unwrap_or(json!(0)), "down": m.get("down").cloned().unwrap_or(json!(0))}));
                if anchor["cum_up"].as_i64().unwrap_or(0) > cu || anchor["cum_down"].as_i64().unwrap_or(0) > cd {
                    *anchor = json!({"cum_up": cu, "cum_down": cd, "up": m.get("up").cloned().unwrap_or(json!(0)), "down": m.get("down").cloned().unwrap_or(json!(0))});
                }
                let up = anchor["up"].as_i64().unwrap_or(0) + cu - anchor["cum_up"].as_i64().unwrap_or(0);
                let dn = anchor["down"].as_i64().unwrap_or(0) + cd - anchor["cum_down"].as_i64().unwrap_or(0);
                m.insert("up".into(), json!(up));
                m.insert("down".into(), json!(dn));
            }
            None => {
                m.insert("counter_up".into(), Value::Null);
                m.insert("counter_down".into(), Value::Null);
            }
        }
        m.insert("tcp_connections".into(), conns.get(p.to_string()).cloned().filter(|x| x.is_i64()).unwrap_or(json!(0)));
        m.insert("source_ips".into(), srcs.get(p.to_string()).cloned().filter(|x| x.is_array()).unwrap_or(json!([])));
        out.push(ib);
    }
    let so = snap.as_object_mut().unwrap();
    so.insert("relay".into(), json!(relay));
    so.insert("ts".into(), py::f(ts));
    so.insert("core_id".into(), v.xray["core_id"].clone());
    so.insert("inbounds".into(), Value::Array(out));
    if let Some(src) = &v.access_source {
        let inode = src["inode"].as_i64().map(|x| x.to_string()).or_else(|| src["inode"].as_str().map(String::from));
        so.insert("cursor".into(), json!({"inode": inode, "offset": src["offset_to"], "backlog_bytes": 0}));
        so.insert("log_backlog_bytes".into(), json!(0));
        if let Some(o) = src["offset_to"].as_i64() {
            so.insert("log_size_bytes".into(), json!(o));
        }
    }
    so.insert("event_gap".into(), json!(0));
    if let Some(p) = v.native_probe.as_ref().or(v.legacy_probe.as_ref()) {
        so.insert("probe".into(), p.clone());
    }
    if let Some(p) = &v.native_paths {
        so.insert("paths".into(), p.clone());
    } else if let Some(p) = &v.legacy_paths {
        so.insert("paths".into(), p.clone());
    }
    let x = &v.xray["xray"];
    let onlines: Vec<Value> = so["inbounds"]
        .as_array()
        .map(|a| a.iter().filter(|i| v.latest.get(i["tag"].as_str().unwrap_or("")).is_some_and(|l| l.4 + l.5 > 0.0)).filter_map(|i| i["clients"].get(0).map(|c| c["label"].clone())).collect())
        .unwrap_or_default();
    so.insert("panel".into(), json!({"ok": true, "error": null, "version": "fleet-relayd",
        "xray_version": x["version"], "xray_running": x["running"], "cpu": v.sample["cpu"]["pct"], "mem_used": v.sample["mem"]["used"],
        "mem_total": v.sample["mem"]["total"], "onlines": onlines, "checked_at": v.sample["ts"]}));
    so.shift_remove("received_at");
    so.shift_remove("error");
    so.shift_remove("failed_at");
    snap
}

/// 旧 store.ingest 的逐入口计算（归属、速率、重置、基线），不含账本（账本走 xray_1m）
fn ingest_snapshot(c: &Connection, relay: &str, mut snap: Value, all: &[Value], now_wall: f64) -> rusqlite::Result<bool> {
    let ts = snap["ts"].as_f64().unwrap_or(0.0);
    let old = meta_get(c, &format!("baseline:{relay}"));
    let old = if old.is_object() { old } else { json!({}) };
    if let Some(ot) = old["ts"].as_f64() {
        if ts < ot || ts == ot {
            return Ok(false);
        }
    }
    let core = snap["core_id"].clone();
    let master_ports: Vec<i64> = snap["inbounds"].as_array().map(|v| v.iter().filter(|i| !py::truthy(&i["outbound_tag"])).filter_map(|i| i["port"].as_i64()).collect()).unwrap_or_default();
    let map = port_map(all, relay, &master_ports);
    let mut base_in = Map::new();
    let old_ts = old.get("ts").cloned();
    let old_core = old.get("core_id").cloned().unwrap_or(Value::Null);
    for i in snap["inbounds"].as_array_mut().unwrap() {
        let port = i["port"].as_i64().unwrap_or(0);
        let tag = i["tag"].as_str().unwrap_or("").to_string();
        let a = map.get(&port);
        let uuid = asset_uuid(&map, relay, port);
        let unassigned = if i["protocol"] == "freedom" { "直连" } else { "未归属转发" };
        let before = old["inbounds"].get(&tag).cloned();
        let valid = ["counter_up", "counter_down"].iter().all(|k| i[*k].is_i64() && i[*k].as_i64().unwrap_or(-1) >= 0);
        let b_ts = before.as_ref().and_then(|b| b.get("ts").cloned()).or_else(|| old_ts.clone()).unwrap_or(py::f(ts));
        let elapsed = ts - py::num(&b_ts).unwrap_or(ts);
        let oc = before.as_ref().and_then(|b| b.get("core_id").cloned()).unwrap_or(old_core.clone());
        let reset = valid
            && before.is_some()
            && (oc != core || ["counter_up", "counter_down"].iter().any(|k| i[*k].as_i64().unwrap_or(0) < before.as_ref().unwrap()[*k].as_i64().unwrap_or(0)));
        let deltas: Vec<i64> = if valid {
            ["counter_up", "counter_down"].iter().map(|k| counter_delta(before.as_ref().and_then(|b| b[*k].as_i64()), i[*k].as_i64().unwrap_or(0), &oc, &core)).collect()
        } else {
            vec![0, 0]
        };
        let rate_valid = valid && before.is_some() && elapsed > 0.0 && elapsed <= 60.0 && !reset;
        let last_active = if deltas[0] + deltas[1] > 0 { py::f(ts) } else { before.as_ref().map(|b| b["last_active"].clone()).unwrap_or(Value::Null) };
        let m = i.as_object_mut().unwrap();
        m.insert("kind".into(), json!(if master_ports.contains(&port) { "master" } else { "relay" }));
        m.insert("uuid".into(), json!(uuid));
        m.insert("team".into(), a.map(|a| a["team"].clone()).unwrap_or(json!(if unassigned == "直连" { "未分配直连" } else { "未归属入口" })));
        m.insert("name".into(), a.map(|a| a["name"].clone()).unwrap_or(json!(format!("{relay} · {unassigned} :{port}"))));
        m.insert("rate_up".into(), if rate_valid { py::f(deltas[0] as f64 / elapsed) } else { Value::Null });
        m.insert("rate_down".into(), if rate_valid { py::f(deltas[1] as f64 / elapsed) } else { Value::Null });
        m.insert("last_active".into(), last_active.clone());
        m.insert("sample_gap".into(), json!(before.is_some() && elapsed > 60.0));
        m.insert("stats_valid".into(), json!(valid));
        m.insert("counter_reset".into(), json!(reset));
        m.insert("traffic_source".into(), json!("xray_inbound"));
        if valid {
            base_in.insert(tag, json!({"counter_up": m["counter_up"], "counter_down": m["counter_down"], "last_active": last_active, "ts": py::f(ts), "core_id": core}));
        } else if let Some(b) = before {
            let mut bb = b.as_object().cloned().unwrap_or_default();
            bb.insert("ts".into(), b.get("ts").cloned().or_else(|| old_ts.clone()).unwrap_or(Value::Null));
            bb.insert("core_id".into(), oc);
            base_in.insert(tag, Value::Object(bb));
        }
    }
    let so = snap.as_object_mut().unwrap();
    so.insert("received_at".into(), py::f(now_wall));
    so.insert("error".into(), Value::Null);
    let cursor = so.get("cursor").cloned().unwrap_or(json!({}));
    c.execute("INSERT INTO relays VALUES(?1,?2,?3) ON CONFLICT(relay) DO UPDATE SET ts=excluded.ts,data=excluded.data", params![relay, ts, pyjson::dumps(&snap)])?;
    meta_set(c, &format!("baseline:{relay}"), &json!({"ts": py::f(ts), "core_id": core, "inbounds": base_in}))?;
    meta_set(c, &format!("cursor:{relay}"), &cursor)?;
    Ok(true)
}

/// 旧 store.failed：保留上一份快照，只标错误
fn mark_failed(c: &Connection, relay: &str, msg: &str, now: f64) -> rusqlite::Result<()> {
    let prev = relay_snapshot(c, relay).unwrap_or(json!({"ts": 0, "inbounds": []}));
    let mut d = prev.as_object().cloned().unwrap_or_default();
    if d.get("error").and_then(|e| e.as_str()) == Some(msg) {
        return Ok(());
    }
    d.insert("error".into(), json!(msg));
    d.insert("failed_at".into(), py::f(now));
    let ts = d.get("ts").cloned().unwrap_or(json!(0));
    c.execute("INSERT INTO relays VALUES(?1,?2,?3) ON CONFLICT(relay) DO UPDATE SET data=excluded.data", params![relay, py::num(&ts).unwrap_or(0.0), pyjson::dumps(&Value::Object(d))])?;
    Ok(())
}

/// fleetd.db 的一个独立连接（WAL；与进程内 Store 连接并发，靠 busy_timeout 排队）——在阻塞线程里同时要两库时用
pub fn fleetd_conn(p: &Path) -> Result<Connection, String> {
    let c = Connection::open(p).map_err(|e| e.to_string())?;
    c.busy_timeout(std::time::Duration::from_secs(10)).map_err(|e| e.to_string())?;
    c.execute_batch("PRAGMA synchronous=FULL;").map_err(|e| e.to_string())?;
    Ok(c)
}

fn feed_state(c: &Connection) -> Map<String, Value> {
    meta_get(c, "fleetd:feed").as_object().cloned().unwrap_or_default()
}

/// 账本：xray_1m → minutes（整分钟覆盖）→ hours（受影响小时重算）
fn feed_ledger(app: &App, c: &Connection, st: &mut Map<String, Value>, now: f64) -> rusqlite::Result<u64> {
    let snap_at = meta_get(c, "fleetd:snapshot")["at"].as_f64().unwrap_or(now);
    let c0 = st.get("ledger_c0").and_then(|v| v.as_i64()).unwrap_or_else(|| (snap_at as i64).div_euclid(60) * 60);
    st.insert("ledger_c0".into(), json!(c0));
    // 平时只重看最近 5 分钟（relayd 批次最多晚到几十秒）；每 5 分钟整看 30 分钟（补积压的批次）
    let last_full = st.get("ledger_full_at").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let window = if now - last_full >= 300.0 {
        st.insert("ledger_full_at".into(), json!(now));
        1800
    } else {
        300
    };
    let since = c0.max(now as i64 - window);
    let all = assets(c);
    let mut n = 0u64;
    let mut touched_hours: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for (host, relay) in &app.cfg.edge.relay_hosts {
        let Some(snap) = relay_snapshot(c, relay) else { continue };
        let map = port_map(&all, relay, &masters_of(&snap));
        let ports: Vec<i64> = snap["inbounds"].as_array().map(|v| v.iter().filter_map(|i| i["port"].as_i64()).collect()).unwrap_or_default();
        let h = host.clone();
        // relayd 在这些分钟有样本（旧系统每次采样都给全部入口写一行，零也写）
        let (alive, rows): (Vec<i64>, Vec<(String, i64, i64, i64)>) = app.ingest.with_db(move |ic| {
            let mut alive = vec![];
            if let Ok(mut s) = ic.prepare("SELECT minute FROM host_1m WHERE host_id=?1 AND minute>=?2 ORDER BY minute") {
                if let Ok(r) = s.query_map(params![h, since], |r| r.get(0)) {
                    alive = r.flatten().collect();
                }
            }
            let mut rows = vec![];
            if let Ok(mut s) = ic.prepare("SELECT tag, minute, up, down FROM xray_1m WHERE host_id=?1 AND kind='inbound' AND minute>=?2") {
                if let Ok(r) = s.query_map(params![h, since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))) {
                    rows = r.flatten().collect();
                }
            }
            (alive, rows)
        });
        let mut agg: BTreeMap<(String, i64), (i64, i64)> = BTreeMap::new();
        for m in &alive {
            if *m >= now as i64 - 60 {
                continue; // 当前分钟未完：下个周期再写整分钟
            }
            for p in &ports {
                agg.entry((asset_uuid(&map, relay, *p), *m)).or_default();
            }
        }
        for (tag, m, u, d) in rows {
            if m >= now as i64 - 60 {
                continue;
            }
            let Some(p) = tag.strip_prefix("inbound-").and_then(|x| x.parse::<i64>().ok()) else { continue };
            let e = agg.entry((asset_uuid(&map, relay, p), m)).or_default();
            e.0 += u;
            e.1 += d;
        }
        // 只写有变化的分钟（大多数分钟和上一拍写的一样）
        let mut have: std::collections::HashMap<(String, i64), (i64, i64)> = std::collections::HashMap::new();
        {
            let mut q = c.prepare_cached("SELECT asset, bucket, up, down FROM minutes WHERE bucket>=?1")?;
            for r in q.query_map([since], |r| Ok(((r.get::<_, String>(0)?, r.get::<_, i64>(1)?), (r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))))?.flatten() {
                have.insert(r.0, r.1);
            }
        }
        for ((uuid, m), (u, d)) in agg {
            if have.get(&(uuid.clone(), m)) == Some(&(u, d)) {
                continue;
            }
            c.execute("INSERT INTO minutes VALUES(?1,?2,?3,?4) ON CONFLICT(asset,bucket) DO UPDATE SET up=excluded.up,down=excluded.down", params![uuid, m, u, d])?;
            n += 1;
            let hrs = touched_hours.entry(uuid).or_default();
            let hb = m.div_euclid(3600) * 3600;
            if !hrs.contains(&hb) {
                hrs.push(hb);
            }
        }
    }
    for (uuid, hrs) in touched_hours {
        for hb in hrs {
            c.execute(
                "INSERT INTO hours(asset,bucket,up,down) SELECT ?1,?2,coalesce(sum(up),0),coalesce(sum(down),0) FROM minutes WHERE asset=?1 AND bucket>=?2 AND bucket<?3
                 ON CONFLICT(asset,bucket) DO UPDATE SET up=excluded.up,down=excluded.down",
                params![uuid, hb, hb + 3600],
            )?;
        }
    }
    Ok(n)
}

fn feed_relays(app: &App, c: &Connection, st: &mut Map<String, Value>, now: f64) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let all = assets(c);
    for (host, relay) in &app.cfg.edge.relay_hosts {
        let Some(prev) = relay_snapshot(c, relay) else {
            out.insert(relay.clone(), "旧库里没有这台线路机的快照".into());
            continue;
        };
        let Some(v) = relayd_view(app, host) else {
            let _ = mark_failed(c, relay, "relayd 没有上报", now);
            out.insert(relay.clone(), "relayd 没有上报".into());
            continue;
        };
        let mut v = v;
        let sample_ts = v.sample["ts"].as_f64().unwrap_or(v.received_at);
        if prev["ts"].as_f64() == Some(sample_ts) && now - v.received_at <= 60.0 {
            out.insert(relay.clone(), "无新样本".into());
            continue;
        }
        if app.cfg.edge.native_paths {
            v.native_paths = native_paths(app, host, now, v.legacy_paths.as_ref());
        }
        if app.cfg.edge.native_probe {
            v.native_probe = native_probe(app, host, now, v.legacy_probe.as_ref());
            // 线路机自己：对端测的 TCP 心跳 + 自己经主节点的出口验证（relayd 上报 node = 线路机资产 uuid）
            if let Some(ra) = all.iter().find(|a| a["team"] == "线路机" && a["name"].as_str() == Some(relay.as_str())) {
                if let Some(row) = relay_self_row(app, host, ra, now) {
                    let p = v.native_probe.get_or_insert_with(|| json!({"updated_at": py::f(now), "assets": []}));
                    let arr = p["assets"].as_array_mut().unwrap();
                    match arr.iter_mut().find(|x| x["uuid"] == ra["uuid"]) {
                        Some(x) => *x = row,
                        None => arr.push(row),
                    }
                }
            }
        }
        if now - v.received_at > 60.0 {
            let _ = mark_failed(c, relay, "relayd 超过 60 秒没有上报", now);
            out.insert(relay.clone(), format!("relayd 上报已过期 {:.0} s", now - v.received_at));
            continue;
        }
        let h2 = host.clone();
        let (managed, remarks): (Value, BTreeMap<i64, String>) = app.store.call_blocking(move |fc| {
            let m = crate::relayspec::observed(fc, &h2).map(|o| o.2).unwrap_or(Value::Null);
            let mut r = BTreeMap::new();
            if let Ok(mut s) = fc.prepare("SELECT port, remark FROM entry_meta WHERE relay=? AND remark IS NOT NULL AND remark<>''") {
                if let Ok(rows) = s.query_map([&h2], |x| Ok((x.get::<_, i64>(0)?, x.get::<_, String>(1)?))) {
                    r = rows.flatten().collect();
                }
            }
            (m, r)
        });
        let key = format!("cum:{relay}");
        let mut cum = st.get(&key).and_then(|x| x.as_object().cloned()).unwrap_or_default();
        let snap = build_snapshot(&prev, relay, &v, &managed, &remarks, &mut cum);
        st.insert(key, Value::Object(cum));
        match ingest_snapshot(c, relay, snap, &all, now) {
            Ok(true) => out.insert(relay.clone(), "ok".into()),
            Ok(false) => out.insert(relay.clone(), "无新样本".into()),
            Err(e) => out.insert(relay.clone(), format!("写入失败：{e}")),
        };
    }
    out
}

fn normalized_ip(s: &str) -> String {
    match s.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(v6)) => v6.to_ipv4_mapped().map(|v4| v4.to_string()).unwrap_or_else(|| v6.to_string()),
        Ok(ip) => ip.to_string(),
        Err(_) => s.to_string(),
    }
}

/// 访问事件 → 旧 activity 行（旧 bridge.parse_access + store.ingest 的事件部分）
pub fn event_row(relay: &str, ev: &Value, map: &BTreeMap<i64, Value>) -> Option<Map<String, Value>> {
    let inbound = ev["inbound"].as_str()?;
    let port_s = inbound.strip_prefix("inbound-")?;
    if !(2..=5).contains(&port_s.len()) || !port_s.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let iport: i64 = port_s.parse().ok()?;
    let proto = ev["network"].as_str().unwrap_or("");
    if proto != "tcp" && proto != "udp" {
        return None;
    }
    let host: String = ev["host"].as_str().unwrap_or("").trim_matches(|c| c == '[' || c == ']').to_lowercase().chars().take(253).collect();
    let port = ev["port"].as_i64()?;
    if host.is_empty() || !(1..=65535).contains(&port) {
        return None;
    }
    let src = normalized_ip(ev["source_ip"].as_str()?);
    if src.parse::<std::net::IpAddr>().map(|i| i.is_loopback()).unwrap_or(false) {
        return None;
    }
    let outbound: String = ev["outbound"].as_str().unwrap_or("").chars().take(120).collect();
    let status = ev["status"].as_str();
    let blocked = outbound == "blocked" || status == Some("rejected") || (status.is_none() && ev["action"] == "blocked" && outbound == "blocked");
    let kind = if host.parse::<std::net::IpAddr>().is_ok() { "ip" } else { "domain" };
    let a = map.get(&iport);
    let uuid = a.and_then(|a| a["uuid"].as_str().map(String::from)).unwrap_or_else(|| format!("direct:{relay}:{iport}"));
    let team = a.map(|a| a["team"].clone()).unwrap_or(json!(if outbound == "direct" { "未分配直连" } else { "未归属入口" }));
    let ts = ev["ts"].as_f64()?.floor();
    let mut m = Map::new();
    m.insert("id".into(), json!(format!("{relay}:{}", ev["id"].as_str()?)));
    m.insert("ts".into(), py::f(ts));
    m.insert("asset".into(), json!(uuid));
    m.insert("team".into(), team);
    m.insert("relay".into(), json!(relay));
    m.insert("source_ip".into(), json!(src));
    m.insert("host".into(), json!(host));
    m.insert("port".into(), json!(port));
    m.insert("protocol".into(), json!(proto));
    m.insert("action".into(), json!(if blocked { "blocked" } else { "routed" }));
    m.insert("client".into(), json!(ev["client"].as_str().unwrap_or("").chars().take(120).collect::<String>()));
    m.insert("kind".into(), json!(kind));
    Some(m)
}

fn feed_activity(app: &App, c: &Connection, st: &mut Map<String, Value>, now: f64) -> Result<(u64, u64), String> {
    let cursor = st.get("access_rid").and_then(|v| v.as_i64()).unwrap_or(0);
    let snap_at = meta_get(c, "fleetd:snapshot")["at"].as_f64().unwrap_or(0.0);
    let rows: Vec<(i64, String, String)> = app.ingest.with_db(move |ic| {
        let mut out = vec![];
        if let Ok(mut s) = ic.prepare("SELECT rid, host_id, doc FROM access_raw WHERE rid>?1 ORDER BY rid LIMIT 5000") {
            if let Ok(r) = s.query_map([cursor], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))) {
                out = r.flatten().collect();
            }
        }
        out
    });
    if rows.is_empty() {
        return Ok((0, 0));
    }
    let cfg = LegacyStore::compat_cfg(c);
    let cx = Ctx { db: c, now, cfg: &cfg };
    let all = assets(c);
    let mut maps: BTreeMap<String, BTreeMap<i64, Value>> = BTreeMap::new();
    let (mut seen, mut ins) = (0u64, 0u64);
    let mut last = cursor;
    for (rid, host, doc) in rows {
        last = rid;
        let Some(relay) = app.cfg.edge.relay_hosts.get(&host) else { continue };
        let Ok(ev) = serde_json::from_str::<Value>(&doc) else { continue };
        // 快照之前的事件旧库里已经有（同一 id），INSERT OR IGNORE 去重；更早的也不必再看
        if ev["ts"].as_f64().unwrap_or(0.0) < snap_at - 3600.0 {
            continue;
        }
        let map = maps.entry(relay.clone()).or_insert_with(|| {
            let snap = relay_snapshot(c, relay).unwrap_or(Value::Null);
            port_map(&all, relay, &masters_of(&snap))
        });
        let Some(row) = event_row(relay, &ev, map) else { continue };
        seen += 1;
        // 旧 bridge 事件形状（behavior.ingest 读 ts/host/source_ip/kind/action）
        let event = json!({"event_id": ev["id"], "ts": row["ts"], "inbound": ev["inbound"], "outbound": ev["outbound"], "source_ip": row["source_ip"],
            "host": row["host"], "port": row["port"], "protocol": row["protocol"], "kind": row["kind"], "action": row["action"], "client": row["client"]});
        let asset = row["asset"].as_str().unwrap_or("").to_string();
        let team = row["team"].as_str().unwrap_or("").to_string();
        match compat::behavior_write::insert_activity(&cx, relay, &asset, &team, &event) {
            Ok(true) => ins += 1,
            Ok(false) => {}
            Err(e) => return Err(format!("{e:?}")),
        }
    }
    st.insert("access_rid".into(), json!(last));
    Ok((seen, ins))
}

/// 一种检查在一个节点上最近一小时的汇总
#[allow(dead_code)]
#[derive(Clone)]
struct ProbeAgg {
    n: i64,
    ok: i64,
    ok_with_value: i64,
    avg: Option<f64>,
    min: Option<f64>,
    max: Option<f64>,
    last_at: f64,
    last_ok: bool,
    last_value: Option<f64>,
}

/// probe_land 最近一小时的内存窗口（按 land_id 游标增量读）：(node, kind) 的 tcp / egress（只取周期最终结果）/ path，
/// 与线路机控制路径（按 relay）。每格 (measured_at, ok, value_ms)。
#[derive(Default)]
struct ProbeWin {
    cursor: i64,
    loaded: bool,
    by_node: HashMap<(String, String), std::collections::VecDeque<(f64, bool, Option<f64>)>>,
    control: HashMap<String, std::collections::VecDeque<(f64, bool, Option<f64>)>>,
}

impl ProbeWin {
    fn sync(&mut self, ic: &Connection, now: f64) {
        let (sql, arg) = if self.loaded {
            ("SELECT land_id, relay, node, check_kind, measured_at, status, json_extract(doc,'$.value_ms'), json_extract(doc,'$.final'), json_extract(doc,'$.reason_code'), json_extract(doc,'$.actual_ip') FROM probe_land WHERE land_id>?1 ORDER BY land_id",
                rusqlite::types::Value::Integer(self.cursor))
        } else {
            ("SELECT land_id, relay, node, check_kind, measured_at, status, json_extract(doc,'$.value_ms'), json_extract(doc,'$.final'), json_extract(doc,'$.reason_code'), json_extract(doc,'$.actual_ip') FROM probe_land WHERE measured_at>=?1 ORDER BY land_id",
                rusqlite::types::Value::Real(now - 3600.0))
        };
        if let Ok(mut st) = ic.prepare_cached(sql) {
            if let Ok(mut q) = st.query([arg]) {
                while let Ok(Some(r)) = q.next() {
                    let id: i64 = r.get(0).unwrap_or(0);
                    self.cursor = self.cursor.max(id);
                    let (relay, node, kind): (String, String, String) = (r.get(1).unwrap_or_default(), r.get(2).unwrap_or_default(), r.get(3).unwrap_or_default());
                    let ts: f64 = r.get(4).unwrap_or(0.0);
                    let reason: Option<String> = r.get::<_, Option<String>>(8).ok().flatten();
                    let actual: Option<String> = r.get::<_, Option<String>>(9).ok().flatten();
                    // 只看连通性：取到出口 IP（即使与记录不同）也算通过
                    let ok = r.get::<_, String>(5).map(|x| x == "ok").unwrap_or(false) || (reason.as_deref() == Some("egress_mismatch") && actual.is_some());
                    let v: Option<f64> = r.get::<_, Option<f64>>(6).ok().flatten();
                    let fin: Option<i64> = r.get::<_, Option<i64>>(7).ok().flatten();
                    match kind.as_str() {
                        "control" => self.control.entry(relay).or_default().push_back((ts, ok, v)),
                        "egress" if fin == Some(0) => {}
                        "tcp" | "egress" | "path" => self.by_node.entry((node, kind)).or_default().push_back((ts, ok, v)),
                        _ => {}
                    }
                }
            }
        }
        if !self.loaded {
            self.cursor = self.cursor.max(ic.query_row("SELECT coalesce(max(land_id),0) FROM probe_land", [], |r| r.get(0)).unwrap_or(0));
            self.loaded = true;
        }
        let cut = now - 3600.0;
        for q in self.by_node.values_mut().chain(self.control.values_mut()) {
            while q.front().is_some_and(|x| x.0 < cut) {
                q.pop_front();
            }
        }
        self.by_node.retain(|_, q| !q.is_empty());
        self.control.retain(|_, q| !q.is_empty());
    }

    fn agg(q: &std::collections::VecDeque<(f64, bool, Option<f64>)>, now: f64) -> ProbeAgg {
        let cut = now - 3600.0;
        let mut a = ProbeAgg { n: 0, ok: 0, ok_with_value: 0, avg: None, min: None, max: None, last_at: 0.0, last_ok: false, last_value: None };
        let mut sum = 0.0;
        let mut last: Option<(f64, bool, Option<f64>)> = None;
        for &(t, ok, v) in q.iter().filter(|x| x.0 >= cut) {
            a.n += 1;
            if ok {
                a.ok += 1;
                if let Some(x) = v {
                    a.ok_with_value += 1;
                    sum += x;
                    a.min = Some(a.min.map_or(x, |m: f64| m.min(x)));
                    a.max = Some(a.max.map_or(x, |m: f64| m.max(x)));
                }
            }
            if last.is_none_or(|l| t >= l.0) {
                last = Some((t, ok, v));
            }
        }
        if a.ok_with_value > 0 {
            a.avg = Some(sum / a.ok_with_value as f64);
        }
        if let Some((t, ok, v)) = last {
            a.last_at = t;
            // 最新一条只认 5 分钟内的
            if t >= now - 300.0 {
                a.last_ok = ok;
                a.last_value = v;
            }
        }
        a
    }

    fn probes(&self, now: f64) -> BTreeMap<(String, String), ProbeAgg> {
        self.by_node.iter().map(|(k, q)| {
            let mut a = Self::agg(q, now);
            // 原先 last_at 只来自 5 分钟内的最新一条
            if a.last_at < now - 300.0 {
                a.last_at = 0.0;
            }
            (k.clone(), a)
        }).collect()
    }

    fn control(&self, now: f64) -> BTreeMap<String, ProbeAgg> {
        self.control.iter().map(|(k, q)| (k.clone(), Self::agg(q, now))).collect()
    }
}

/// 节点出口 IP 变了就更新资产（不告警）：最近两次最终出口验证都成功、IP 相同、与资产 IP 不同、不是任何线路机的 IP
///（那是泄漏，由 egress_route_leak 报警），且都在 30 分钟内。
fn auto_egress_ip(app: &App, cx: &Ctx, now: f64) -> compat::py::R<usize> {
    let assets = compat::store::raw_assets(cx)?;
    let relay_ips: Vec<String> = assets.iter().filter(|a| a["team"] == "线路机").filter_map(|a| a["ip"].as_str().map(String::from)).collect();
    let mut n = 0;
    for a in assets.iter().filter(|a| a["team"] != "线路机") {
        let (Some(uuid), Some(ip)) = (a["uuid"].as_str(), a["ip"].as_str()) else { continue };
        let docs: Vec<Value> = app.ingest.with_db(|c| {
            c.prepare_cached("SELECT doc FROM probe_land WHERE node=?1 AND check_kind='egress' AND measured_at>=?2 ORDER BY measured_at DESC LIMIT 8")
                .and_then(|mut st| st.query_map(rusqlite::params![uuid, now - 1800.0], |r| r.get::<_, String>(0)).map(|it| it.flatten().collect::<Vec<_>>()))
                .unwrap_or_default()
                .into_iter()
                .filter_map(|s| serde_json::from_str::<Value>(&s).ok())
                .filter(|d| d["final"] != false)
                .take(2)
                .collect()
        });
        if docs.len() < 2 || !docs.iter().all(probe_ok) {
            continue;
        }
        let (Some(a0), Some(a1)) = (docs[0]["actual_ip"].as_str(), docs[1]["actual_ip"].as_str()) else { continue };
        if a0 != a1 || a0 == ip || relay_ips.iter().any(|r| r == a0) || a0.parse::<std::net::IpAddr>().is_err() {
            continue;
        }
        if compat::writes::set_asset_ip(cx, uuid, a0, "fleetd", "出口 IP 自动更新", false)?.is_some() {
            tracing::info!(asset = uuid, from = ip, to = a0, "出口 IP 自动更新");
            n += 1;
        }
    }
    // 出站地址同步：线路机上该入口（同一出站 tag）实际连的落地地址 / 端口与资产记录不同 → 以线路机为准
    let ov = compat::store::overview(cx)?;
    for a in ov["assets"].as_array().cloned().unwrap_or_default() {
        let ch = &a["channel"];
        let (Some(uuid), Some(tag)) = (a["uuid"].as_str(), a["outbound_tag"].as_str()) else { continue };
        if a["team"] == "线路机" || ch["outbound_tag"].as_str() != Some(tag) || !ch["target"].is_string() || ch["target_port"].is_null() {
            continue;
        }
        if ch["target"] != a["target"] || !compat::store::py_eq(&ch["target_port"], &a["target_port"]) {
            if compat::writes::set_asset_target(cx, uuid, &ch["target"], &ch["target_port"], "fleetd", "出站地址自动同步（以线路机实际配置为准）")? {
                tracing::info!(asset = uuid, target = %ch["target"], "出站地址自动同步");
                n += 1;
            }
        }
    }
    Ok(n)
}

/// relayd 探测行是否算通过：只看连通性——出口 / 路径取到了 IP，就算与记录不同（reason_code = egress_mismatch）也算通过
fn probe_ok(d: &Value) -> bool {
    d["status"] == "ok" || (d["reason_code"] == "egress_mismatch" && d["actual_ip"].is_string())
}

/// ISO 时间（Komari 的格式：纳秒、Z 结尾）
fn iso_ns(ts: f64) -> String {
    let secs = ts.floor() as i64;
    let nanos = ((ts - ts.floor()) * 1e9).round().min(999_999_999.0) as u32;
    chrono::DateTime::from_timestamp(secs, nanos).map(|d| d.format("%Y-%m-%dT%H:%M:%S%.9fZ").to_string()).unwrap_or_default()
}

/// Komari 的 ping 汇总（latest/avg/min/max 取整毫秒，失败记 -1 / 计入 loss），窗口 1 小时；tail 沿用上一份
fn ping_summary(a: Option<&ProbeAgg>, prev: &Value) -> Value {
    let mut m = prev.as_object().cloned().unwrap_or_default();
    let Some(a) = a.filter(|a| a.n > 0) else { return Value::Object(m) };
    let latest = if a.last_ok { a.last_value.map(|x| x.round() as i64).unwrap_or(-1) } else { -1 };
    let loss = ((a.n - a.ok_with_value) as f64 * 100.0 / a.n as f64).round() as i64;
    let r = |x: Option<f64>| x.map(|v| v.round() as i64).unwrap_or(0);
    for (k, v) in [("latest", latest), ("avg", r(a.avg)), ("min", r(a.min)), ("max", r(a.max)), ("loss", loss)] {
        m.insert(k.into(), json!(v));
    }
    m.entry("tail").or_insert(json!(0));
    Value::Object(m)
}

/// hosts（旧 Komari 插件同步的 `common:getNodesLatestStatus` 形状）：有 nodeagent/relayd 的主机用其样本；
/// 只有连通性探针的主机（NodeMach 等）在线 = 最近的 TCP 与出口验证都通过；ping 汇总来自 relayd 探测。
fn feed_hosts_native(app: &App, c: &Connection, now: f64) -> rusqlite::Result<usize> {
    let prev = meta_get(c, "hosts");
    let mut hosts = prev.as_object().cloned().unwrap_or_default();
    let all = assets(c);
    let rev: BTreeMap<&str, &str> = app.cfg.edge.relay_hosts.iter().map(|(h, r)| (r.as_str(), h.as_str())).collect();
    let latest: BTreeMap<String, (f64, Value, String)> = app.ingest.with_db(|ic| {
        let mut m = BTreeMap::new();
        if let Ok(mut st) = ic.prepare("SELECT host_id, received_at, sample, boot_id FROM host_latest") {
            if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))) {
                for (h, t, s, b) in rows.flatten() {
                    m.insert(h, (t, serde_json::from_str(&s).unwrap_or(Value::Null), b));
                }
            }
        }
        m
    });
    // 每节点每种检查：最近一小时的汇总 + 最新一条（出口验证只看周期的最终结果）
    // 探测一小时汇总 + 最新一条：内存里按 land_id 游标增量读 probe_land（不再每次对一小时的行做 json_extract 聚合）
    let (probes, relay_control) = {
        let mut w = app.legacy_store.probe_agg.lock().unwrap();
        app.ingest.with_db(|ic| w.sync(ic, now));
        (w.probes(now), w.control(now))
    };
    let task_map: std::collections::HashMap<i64, (String, &'static str)> = all
        .iter()
        .flat_map(|a| {
            let u = a["uuid"].as_str().unwrap_or("").to_string();
            [("tcp_task_id", "tcp"), ("egress_task_id", "egress")].into_iter().filter_map(move |(f, k)| a[f].as_i64().map(|t| (t, (u.clone(), k))))
        })
        .collect();
    let mut n = 0;
    for a in &all {
        let Some(uuid) = a["uuid"].as_str() else { continue };
        let name = a["name"].as_str().unwrap_or("");
        let host_id = if a["team"] == "线路机" { rev.get(name).map(|s| s.to_string()) } else { Some(name.split(" · ").next().unwrap_or("").to_string()) };
        let mut h = hosts.get(uuid).and_then(|v| v.as_object().cloned()).unwrap_or_default();
        let presence_only = h.get("metrics_available") == Some(&json!(false));
        match host_id.as_ref().and_then(|id| latest.get(id)) {
            Some((recv, s, boot)) if !presence_only => {
                let set = |h: &mut Map<String, Value>, k: &str, v: Value| {
                    h.insert(k.into(), v);
                };
                set(&mut h, "client", json!(uuid));
                set(&mut h, "cpu", s["cpu"]["pct"].clone());
                set(&mut h, "ram", s["mem"]["used"].clone());
                set(&mut h, "ram_total", s["mem"]["total"].clone());
                set(&mut h, "swap", s["swap"]["used"].clone());
                set(&mut h, "swap_total", s["swap"]["total"].clone());
                set(&mut h, "disk", s["disk"]["used"].clone());
                set(&mut h, "disk_total", s["disk"]["total"].clone());
                set(&mut h, "net_in", json!(s["net"]["rx_bps"].as_f64().unwrap_or(0.0).round() as i64));
                set(&mut h, "net_out", json!(s["net"]["tx_bps"].as_f64().unwrap_or(0.0).round() as i64));
                // 本期累计（流量计：跨重启累加、线路机按账单周期清零），不是开机以来的网卡计数
                if let Some((up, down)) = crate::meter::step(c, a, s, boot, now)? {
                    set(&mut h, "net_total_down", json!(down.round() as i64));
                    set(&mut h, "net_total_up", json!(up.round() as i64));
                }
                set(&mut h, "load", s["load"][0].clone());
                set(&mut h, "load5", s["load"][1].clone());
                set(&mut h, "load15", s["load"][2].clone());
                set(&mut h, "connections", s["sock"]["tcp"].clone());
                set(&mut h, "connections_udp", s["sock"]["udp"].clone());
                set(&mut h, "process", s["procs"].clone());
                set(&mut h, "uptime", json!(s["uptime"].as_f64().unwrap_or(0.0) as i64));
                for (k, d) in [("gpu", json!(0)), ("temp", json!(0)), ("message", json!("")), ("metrics_available", json!(true))] {
                    h.entry(k).or_insert(d);
                }
                set(&mut h, "time", json!(iso_ns(s["ts"].as_f64().unwrap_or(*recv))));
                set(&mut h, "online", json!(now - recv < 60.0));
            }
            _ if presence_only => {
                let relay_host = a["relay"].as_str().and_then(|r| rev.get(r)).map(|s| s.to_string());
                let _ = relay_host;
                let tcp = probes.get(&(uuid.to_string(), "tcp".into()));
                let eg = probes.get(&(uuid.to_string(), "egress".into()));
                let path = probes.get(&(uuid.to_string(), "path".into()));
                // 在线 = 最近的 TCP 与出口验证都通过；relayd 不单独测 TCP/出口的节点用实际入口到出口验证
                let online = if tcp.is_some() || eg.is_some() {
                    tcp.is_some_and(|t| t.last_ok && now - t.last_at <= 120.0) && eg.is_some_and(|e| e.last_ok && now - e.last_at <= 300.0)
                } else {
                    path.is_some_and(|p| p.last_ok && now - p.last_at <= 300.0)
                };
                h.insert("online".into(), json!(online));
            }
            _ => {
                if h.contains_key("online") {
                    h.insert("online".into(), json!(false));
                }
            }
        }
        // 线路机自身：对端 TCP 心跳、经主节点的出口验证（ping["peer_tcp"] / ping["self_egress"]，20 格条用 /komari/pings/<键>/results）
        if a["team"] == "线路机" {
            let mut ping = h.get("ping").and_then(|p| p.as_object().cloned()).unwrap_or_default();
            for (key, kind, label) in [("peer_tcp", "tcp", "心跳 TCP（对端线路机 → 主节点）"), ("self_egress", "egress", "出口验证（经主节点）")] {
                if let Some(agg) = probes.get(&(uuid.to_string(), kind.to_string())) {
                    let prev = ping.get(key).cloned().unwrap_or(json!({"name": format!("{name} {label}")}));
                    ping.insert(key.into(), ping_summary(Some(agg), &prev));
                }
            }
            h.insert("ping".into(), Value::Object(ping));
        }
        // 线路机自己的控制路径（relayd 直连 cloudflare / ipify，不经落地）：挂在 ping["control"]，线路机卡片的延迟 · 丢包用它
        if a["team"] == "线路机" {
            if let Some(hid) = host_id.as_ref() {
                if let Some(agg) = relay_control.get(hid) {
                    let prev = h.get("ping").and_then(|p| p.get("control")).cloned().unwrap_or(json!({"name": format!("{name} 控制路径 · cloudflare")}));
                    let mut ping = h.get("ping").and_then(|p| p.as_object().cloned()).unwrap_or_default();
                    ping.insert("control".into(), ping_summary(Some(agg), &prev));
                    h.insert("ping".into(), Value::Object(ping));
                }
            }
        }
        // ping：Komari 任务号取资产上的 tcp_task_id / egress_task_id
        // 任何 Komari 任务号（包括挂在线路机上的、别的节点的 TCP 任务）都按任务号找到对应资产的原生探测；
        // 没有原生对应的旧任务（例如线路机之间的 SSH 检查）删掉，免得显示冻结的旧值
        if let Some(pm) = h.get("ping").and_then(|p| p.as_object()).cloned() {
            let mut np = serde_json::Map::new();
            for (k, prev_t) in &pm {
                match k.parse::<i64>().ok().and_then(|t| task_map.get(&t)) {
                    Some((node, kind)) => {
                        np.insert(k.clone(), ping_summary(probes.get(&(node.clone(), kind.to_string())), prev_t));
                    }
                    None if k.parse::<i64>().is_ok() => {}
                    None => {
                        np.insert(k.clone(), prev_t.clone());
                    }
                }
            }
            h.insert("ping".into(), Value::Object(np));
        }
        if !h.is_empty() {
            hosts.insert(uuid.to_string(), Value::Object(h));
            n += 1;
        }
    }
    let hv = Value::Object(hosts);
    if hv != prev {
        meta_set(c, "hosts", &hv)?;
    }
    meta_set(c, "hosts_at", &py::f(now))?;
    Ok(n)
}

/// 旧 store.maintenance 的保留期（每日备份见 backup_daily）
fn store_maintenance(c: &Connection, now: f64) -> rusqlite::Result<()> {
    c.execute("DELETE FROM activity WHERE ts<?", [now - 14.0 * 86400.0])?;
    c.execute("DELETE FROM minutes WHERE bucket<?", [now - 35.0 * 86400.0])?;
    c.execute("DELETE FROM hours WHERE bucket<?", [now - 400.0 * 86400.0])?;
    c.execute("DELETE FROM audit WHERE ts<?", [now - 365.0 * 86400.0])?;
    Ok(())
}

/// 一次喂数（阻塞）
pub fn tick(app: &App, ls: &LegacyStore, now: f64) -> Result<(), String> {
    let t0 = std::time::Instant::now();
    let mut stages: Vec<(&'static str, f64)> = vec![];
    let mut lap = std::time::Instant::now();
    let mut lap_cpu = crate::cpu::thread_ms();
    let mut mark = |name: &'static str, stages: &mut Vec<(&'static str, f64)>| {
        stages.push((name, lap.elapsed().as_secs_f64() * 1000.0));
        lap = std::time::Instant::now();
        let c = crate::cpu::thread_ms();
        crate::cpu::add(&format!("tick.{name}"), c - lap_cpu);
        lap_cpu = c;
    };
    let r = ls.with_w(|c| -> Result<(), String> {
        // 一拍一个事务：synchronous=FULL 下逐条自动提交每条都要 fsync
        let tx = c.unchecked_transaction().map_err(|e| e.to_string())?;
        let mut st = feed_state(c);
        if st.get("meter_schema").is_none() {
            c.execute_batch(crate::meter::SCHEMA).map_err(|e| e.to_string())?;
            st.insert("meter_schema".into(), json!(true));
        }
        if st.get("activity_idx2").is_none() {
            // 访问记录按 action / kind / source_ip 过滤时的逐条列表（ORDER BY ts DESC LIMIT）：没有这些索引就要扫整张表
            c.execute_batch(
                "CREATE INDEX IF NOT EXISTS activity_action_time ON activity(action,ts);
                 CREATE INDEX IF NOT EXISTS activity_kind_time ON activity(kind,ts);
                 CREATE INDEX IF NOT EXISTS activity_source_time ON activity(source_ip,ts);",
            )
            .map_err(|e| e.to_string())?;
            st.insert("activity_idx2".into(), json!(true));
        }
        if st.get("evidence_column").is_none() {
            compat::domains::ensure_column(c).map_err(|e| e.to_string())?;
            st.insert("evidence_column".into(), json!(true));
        }
        let relays = feed_relays(app, c, &mut st, now);
        mark("relays", &mut stages);
        // 账本按整分钟写、主机快照供 30 s 一次的评估：都每 30 s 一次
        let forced = ls.force_hosts.swap(false, std::sync::atomic::Ordering::Relaxed);
        let slow_due = now - st.get("slow_at").and_then(|v| v.as_f64()).unwrap_or(0.0) >= 29.5;
        let n = if slow_due { feed_ledger(app, c, &mut st, now).map_err(|e| format!("账本：{e}"))? } else { 0 };
        if n > 0 {
            ls.memo.bump();
        }
        // 这一拍改了快照 / 主机：overview 复用作废
        *ls.memo.overview.lock().unwrap() = None;
        mark("ledger", &mut stages);
        let (seen, ins) = feed_activity(app, c, &mut st, now).map_err(|e| format!("访问记录：{e}"))?;
        mark("activity", &mut stages);
        let hosts_n = if slow_due || forced {
            if slow_due {
                st.insert("slow_at".into(), json!(now));
            }
            feed_hosts_native(app, c, now).map_err(|e| format!("hosts：{e}"))?
        } else {
            ls.status.lock().unwrap().hosts
        };
        mark("hosts", &mut stages);
        let durations: Map<String, Value> = app.cfg.edge.relay_hosts.values().map(|r| (r.clone(), json!(0.0))).collect();
        meta_set(c, "collector", &json!({"workers": 8, "configured": app.cfg.edge.relay_hosts.len(), "running": 0, "waiting": 0,
            "oldest_wait_seconds": 0, "duration_seconds": durations, "checked_at": py::f(now)})).map_err(|e| e.to_string())?;
        // 旧 monitor_loop：每 10 s evaluate + behavior.detect；cache_loop：30 s refresh；每小时 maintenance
        let mut cfg = LegacyStore::compat_cfg(c);
        cfg.memo = Some(ls.memo.clone());
        let cx = Ctx { db: c, now, cfg: &cfg };
        if st.get("ensured").is_none() {
            compat::monitor_eval::ensure(&cx).map_err(|e| format!("monitor ensure：{e:?}"))?;
            compat::behavior_write::ensure(&cx).map_err(|e| format!("behavior ensure：{e:?}"))?;
            st.insert("ensured".into(), json!(true));
        }
        // 输入（线路机新样本）变了就评估；否则最多 30 s 评估一次（新鲜度随时间变化、每分钟一个可用率样本）
        // 每 30 s 评估一次：判定按“完成的测量周期”计数（不按评估次数），可用率每分钟一个样本；最多晚 30 s 发现
        let last_eval = st.get("monitor_at").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if now - last_eval >= 29.5 {
            compat::monitor_eval::evaluate(&cx, None).map_err(|e| format!("monitor：{e:?}"))?;
            st.insert("monitor_at".into(), json!(now));
        }
        mark("monitor", &mut stages);
        // 策略发布：看 relayd 回执，定版或回滚
        let fc = fleetd_conn(&app.cfg.db_path())?;
        if compat::policy_apply::reconcile(&cx, &fc).map_err(|e| format!("policy：{e:?}"))? > 0 {
            app.legacy.cache.purge_all();
            // 版本定版 / 回滚：这一拍提交、数据代加一之后再推 policies（run 里）
            ls.policies_changed.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        mark("policy", &mut stages);
        // 出口 IP 自动更新：节点最近两次出口验证都取到同一个新 IP → 改资产 IP（审计「出口 IP 自动更新」，不告警）
        let last_ipc = st.get("egress_ip_at").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if now - last_ipc >= 59.5 {
            if let Err(e) = auto_egress_ip(app, &cx, now) {
                tracing::warn!(error = ?e, "出口 IP 自动更新");
            }
            st.insert("egress_ip_at".into(), json!(now));
        }
        mark("egress_ip", &mut stages);
        // 行为突增按 5 分钟桶判定（INSERT OR IGNORE 按窗口去重）：每分钟看一次足够
        let last_detect = st.get("detect_at").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if now - last_detect >= 59.5 {
            compat::behavior_write::detect(&cx).map_err(|e| format!("behavior detect：{e:?}"))?;
            st.insert("detect_at".into(), json!(now));
        }
        mark("detect", &mut stages);
        let last_refresh = st.get("behavior_refresh_at").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let demand = *ls.behavior_demand.lock().unwrap();
        let act_demand = *ls.activity_demand.lock().unwrap();
        // 有人看才刷新（24 h 聚合在中控上要几秒）：默认 /behavior 与默认 /activity 各刷各的，最多每 120 s 一次
        let (want_s, want_a) = (now - demand <= 180.0, now - act_demand <= 180.0);
        // 默认 /behavior、/activity 已由增量窗口实时算（compat::window）：这里只保持窗口同步，不再重算 24 h 缓存
        let _ = ls.memo.with_window(c, now, |_| Ok(()));
        // 7 天 / 14 天的小时汇总：新访问记录并进表与内存镜像（首次按块回填）
        match compat::rollup::feed(c, now) {
            Ok((s, e, b)) => ls.memo.rollup_apply(s, e, &b, now),
            Err(e) => tracing::warn!(error = ?e, "rollup feed"),
        }
        mark("rollup", &mut stages);
        if cx.cfg.memo.is_none() && now - last_refresh >= 120.0 && (want_s || want_a) {
            compat::behavior_write::refresh_parts(&cx, want_s, want_a).map_err(|e| format!("behavior refresh：{e:?}"))?;
            st.insert("behavior_refresh_at".into(), json!(now));
        }
        mark("refresh", &mut stages);
        let last_maint = st.get("maintenance_at").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let maint_due = now - last_maint >= 3600.0;
        if maint_due {
            compat::monitor_eval::maintenance(&cx).map_err(|e| format!("monitor maintenance：{e:?}"))?;
            compat::behavior_write::maintenance(&cx).map_err(|e| format!("behavior maintenance：{e:?}"))?;
            store_maintenance(c, now).map_err(|e| format!("store maintenance：{e}"))?;
            let _ = compat::rollup::retention(c, now);
            st.insert("maintenance_at".into(), json!(now));
        }
        meta_set(c, "fleetd:feed", &Value::Object(st)).map_err(|e| e.to_string())?;
        mark("maintenance", &mut stages);
        tx.commit().map_err(|e| e.to_string())?;
        if maint_due {
            // 保留期删除把写连接的页缓存填满：提交后都是干净页，还给堆（下一拍要改的页再从 mmap 读）
            let _ = c.execute_batch("PRAGMA shrink_memory;");
        }
        mark("commit", &mut stages);
        let mut s = ls.status.lock().unwrap();
        s.behavior_demand_at = demand.max(act_demand);
        s.stage_last_ms.clear();
        for (k, v) in &stages {
            s.stage_last_ms.insert(k.to_string(), (v * 10.0).round() / 10.0);
            *s.stage_total_ms.entry(k.to_string()).or_default() += v;
        }
        s.relays = relays;
        s.ledger_rows += n;
        s.activity_seen += seen;
        s.activity_inserted += ins;
        if slow_due {
            s.hosts_at = Some(now);
        }
        s.hosts_error = None;
        s.hosts = hosts_n;
        Ok(())
    });
    let mut s = ls.status.lock().unwrap();
    s.ticks += 1;
    s.last_tick_at = now;
    s.last_tick_ms = t0.elapsed().as_secs_f64() * 1000.0;
    match r {
        None => {
            s.imported = false;
            Err("旧库尚未导入（fleetd legacy snapshot）".into())
        }
        Some(Err(e)) => {
            s.errors += 1;
            s.last_error = Some(e.clone());
            Err(e)
        }
        Some(Ok(())) => {
            s.imported = true;
            Ok(())
        }
    }
}

/// 后台循环：10 s 一次喂数，hosts 每 10 s 从 Komari 取一次
pub async fn run(app: Arc<App>, ls: Arc<LegacyStore>) {
    let mut iv = tokio::time::interval(std::time::Duration::from_secs(10));
    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_push = 0.0f64;
    loop {
        // 定时 10 s 一拍；有人看（SSE 客户端）时，边缘批次一到就（合并 push_debounce_s 后）刷新并推送
        let pushed = tokio::select! {
            _ = iv.tick() => false,
            _ = app.ingest.accepted.notified() => true,
        };
        if pushed {
            {
                let mut f = app.first_ingest_at.lock().unwrap();
                if f.is_none() {
                    *f = Some(app.clock.now());
                }
            }
            if !app.active() {
                continue;
            }
            let wait = app.cfg.poll.push_debounce_s.max((last_push + app.cfg.poll.push_min_gap_s) - app.clock.now()).max(0.0);
            tokio::time::sleep(std::time::Duration::from_secs_f64(wait)).await;
        }
        if !ls.exists() {
            ls.status.lock().unwrap().imported = false;
            continue;
        }
        let now = app.clock.now();
        if pushed {
            last_push = now;
            ls.force_hosts.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let before = ls.status.lock().unwrap().activity_inserted;
        let (a2, l2) = (app.clone(), ls.clone());
        let r = tokio::task::spawn_blocking(move || tick(&a2, &l2, now)).await;
        ls.bump();
        if ls.policies_changed.swap(false, std::sync::atomic::Ordering::Relaxed) && matches!(r, Ok(Ok(()))) {
            app.hub.publish(crate::legacyhttp::POLICIES, Some(crate::legacyhttp::POLICIES_PATH.into()), app.clock.now());
        }
        app.ingest.live.store(app.active(), std::sync::atomic::Ordering::Relaxed);
        if ls.status.lock().unwrap().activity_inserted > before {
            // 新访问记录：推 activity 变化，界面按游标取增量（GET /v2/api/v1/activity/delta?after=…）
            app.hub.publish("activity", Some("/v2/api/v1/activity/delta".into()), now);
        }
        if pushed {
            app.force_poll(&["overview", "panel"]);
        }
        if let Ok(Err(e)) = r {
            tracing::debug!(error = %e, "legacy feed");
        }
        if let Some(Some(at)) = ls.with_w(|c| meta_get(c, "fleetd:snapshot")["at"].as_f64()) {
            ls.status.lock().unwrap().snapshot_at = Some(at);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn events_map_like_bridge() {
        let mut map = BTreeMap::new();
        map.insert(20007, json!({"uuid": "u7", "team": "团队1"}));
        let ev = json!({"id": "516108:42", "ts": 1790190000.923, "inbound": "inbound-20007", "client": "node-31", "source_ip": "::ffff:1.2.3.4",
            "network": "tcp", "host": "Example.COM", "port": 443, "action": "accepted", "status": "accepted", "outbound": "ss-x"});
        let r = event_row("线路机3", &ev, &map).unwrap();
        assert_eq!(pyjson::dumps(&Value::Object(r)), r#"{"id":"线路机3:516108:42","ts":1790190000.0,"asset":"u7","team":"团队1","relay":"线路机3","source_ip":"1.2.3.4","host":"example.com","port":443,"protocol":"tcp","action":"routed","client":"node-31","kind":"domain"}"#);
        let ev2 = json!({"id": "1:2", "ts": 1.0, "inbound": "inbound-20009", "source_ip": "5.6.7.8", "network": "udp", "host": "8.8.8.8", "port": 53, "status": "rejected", "outbound": "direct"});
        let r2 = event_row("线路机3", &ev2, &map).unwrap();
        assert_eq!(r2["asset"], "direct:线路机3:20009");
        assert_eq!(r2["team"], "未分配直连");
        assert_eq!(r2["action"], "blocked");
        assert_eq!(r2["kind"], "ip");
        let lo = json!({"id": "1:3", "ts": 1.0, "inbound": "inbound-20009", "source_ip": "127.0.0.1", "network": "tcp", "host": "a.b", "port": 1, "outbound": "x"});
        assert!(event_row("r", &lo, &map).is_none());
        let other = json!({"id": "1:4", "ts": 1.0, "inbound": "in-socks", "source_ip": "1.1.1.1", "network": "tcp", "host": "a.b", "port": 1, "outbound": "x"});
        assert!(event_row("r", &other, &map).is_none());
    }
    #[test]
    fn rcache_bounded() {
        let mut m: HashMap<String, RcacheEntry> = HashMap::new();
        let body = |n: usize| -> Arc<str> { Arc::from("x".repeat(n)) };
        // 旧代条目：60 s 后丢掉；当前代留着
        rcache_insert(&mut m, "/a".into(), (1, 200, body(10), 0.0), 1, 0.0);
        rcache_insert(&mut m, "/b".into(), (2, 200, body(10), 30.0), 2, 30.0);
        assert!(m.contains_key("/a"), "60 s 内的旧代（长窗口可复用）");
        rcache_insert(&mut m, "/c".into(), (2, 200, body(10), 70.0), 2, 70.0);
        assert!(!m.contains_key("/a") && m.contains_key("/b") && m.contains_key("/c"));
        // 字节上限：最早渲染的先丢
        let big = RCACHE_MAX_BYTES / 3;
        for (i, k) in ["/p1", "/p2", "/p3", "/p4"].iter().enumerate() {
            rcache_insert(&mut m, k.to_string(), (2, 200, body(big), 100.0 + i as f64), 2, 100.0 + i as f64);
        }
        let total: usize = m.iter().map(|(k, v)| k.len() + v.2.len()).sum();
        assert!(total <= RCACHE_MAX_BYTES, "{total}");
        assert!(!m.contains_key("/p1") && m.contains_key("/p4"));
        // 条目上限
        for i in 0..(RCACHE_MAX_ENTRIES + 20) {
            rcache_insert(&mut m, format!("/h{i}"), (2, 200, body(1), 200.0 + i as f64), 2, 200.0 + i as f64);
        }
        assert!(m.len() <= RCACHE_MAX_ENTRIES);
        assert!(m.contains_key(&format!("/h{}", RCACHE_MAX_ENTRIES + 19)));
    }

    #[test]
    fn deltas() {
        let a = json!("a");
        let b = json!("b");
        assert_eq!(counter_delta(None, 10, &a, &a), 0);
        assert_eq!(counter_delta(Some(5), 10, &a, &a), 5);
        assert_eq!(counter_delta(Some(5), 3, &a, &a), 3);
        assert_eq!(counter_delta(Some(5), 10, &a, &b), 10);
    }
}
