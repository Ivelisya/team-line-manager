//! SQLite：WAL、synchronous=FULL，一个专用写线程（所有读写都排队进这个线程，1x 规模下足够）。
//! 保留期：已恢复告警 90 d、notify_log 90 d、审计 365 d。
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

#[derive(Clone)]
pub struct Store {
    tx: mpsc::Sender<Job>,
    pub path: PathBuf,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
CREATE TABLE IF NOT EXISTS plans(id TEXT PRIMARY KEY, canonical_name TEXT NOT NULL, provider TEXT, category TEXT,
  data TEXT NOT NULL, imported_at REAL NOT NULL) STRICT;
CREATE TABLE IF NOT EXISTS assets(name TEXT PRIMARY KEY, short_name TEXT, team TEXT, provider TEXT, plan_id TEXT NOT NULL,
  service_id TEXT, price TEXT, data TEXT NOT NULL, imported_at REAL NOT NULL) STRICT;
CREATE TABLE IF NOT EXISTS alerts(id INTEGER PRIMARY KEY, fingerprint TEXT NOT NULL, rule TEXT NOT NULL, severity TEXT NOT NULL,
  state TEXT NOT NULL, labels TEXT NOT NULL, summary TEXT NOT NULL, evidence TEXT NOT NULL, pending_since REAL NOT NULL,
  fired_at REAL, resolved_at REAL, last_seen REAL NOT NULL, ack_at REAL, ack_by TEXT, suppressed TEXT, group_key TEXT NOT NULL,
  notified_state TEXT, notify_resolved INTEGER NOT NULL, digest_done INTEGER NOT NULL) STRICT;
CREATE INDEX IF NOT EXISTS alerts_state ON alerts(state, resolved_at);
CREATE INDEX IF NOT EXISTS alerts_fp ON alerts(fingerprint);
CREATE TABLE IF NOT EXISTS alert_groups(key TEXT PRIMARY KEY, data TEXT NOT NULL) STRICT;
CREATE TABLE IF NOT EXISTS notify_log(id INTEGER PRIMARY KEY, ts REAL NOT NULL, channel TEXT NOT NULL, status TEXT NOT NULL,
  kind TEXT NOT NULL, severity TEXT NOT NULL, group_key TEXT, title TEXT NOT NULL, body TEXT NOT NULL, alert_ids TEXT NOT NULL,
  error TEXT, latency_ms INTEGER) STRICT;
CREATE INDEX IF NOT EXISTS notify_log_ts ON notify_log(ts);
CREATE TABLE IF NOT EXISTS silences(id INTEGER PRIMARY KEY, kind TEXT NOT NULL, matchers TEXT NOT NULL, starts_at REAL NOT NULL,
  ends_at REAL NOT NULL, tz TEXT NOT NULL, comment TEXT NOT NULL, created_by TEXT NOT NULL, created_at REAL NOT NULL,
  deleted_at REAL, deleted_by TEXT) STRICT;
CREATE TABLE IF NOT EXISTS audit(id INTEGER PRIMARY KEY, ts REAL NOT NULL, actor TEXT NOT NULL, action TEXT NOT NULL,
  target TEXT NOT NULL, detail TEXT NOT NULL, prev_hash TEXT NOT NULL, hash TEXT NOT NULL) STRICT;
CREATE INDEX IF NOT EXISTS audit_ts ON audit(ts);
"#;

impl Store {
    pub fn open(path: &Path) -> Result<Store, String> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir).map_err(|e| format!("创建数据目录失败：{e}"))?;
            }
        }
        let mut conn = Connection::open(path).map_err(|e| format!("打开数据库失败：{e}"))?;
        init(&mut conn)?;
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("fleetd-db".into())
            .spawn(move || {
                let mut conn = conn;
                while let Ok(job) = rx.recv() {
                    job(&mut conn);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Store { tx, path: path.to_path_buf() })
    }

    pub fn open_memory() -> Store {
        let mut conn = Connection::open_in_memory().unwrap();
        init(&mut conn).unwrap();
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::spawn(move || {
            let mut conn = conn;
            while let Ok(job) = rx.recv() {
                let _span = crate::cpu::Span::new("store.thread");
                job(&mut conn);
            }
        });
        Store { tx, path: PathBuf::from(":memory:") }
    }

    /// 在写线程上执行（异步等待结果）。
    pub async fn call<R, F>(&self, f: F) -> R
    where
        R: Send + 'static,
        F: FnOnce(&mut Connection) -> R + Send + 'static,
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = self.tx.send(Box::new(move |c: &mut Connection| {
            let _ = tx.send(f(c));
        }));
        rx.await.expect("db thread gone")
    }

    /// 同步版本（CLI、测试）。
    pub fn call_blocking<R, F>(&self, f: F) -> R
    where
        R: Send + 'static,
        F: FnOnce(&mut Connection) -> R + Send + 'static,
    {
        let (tx, rx) = mpsc::sync_channel(1);
        let _ = self.tx.send(Box::new(move |c: &mut Connection| {
            let _ = tx.send(f(c));
        }));
        rx.recv().expect("db thread gone")
    }

    pub fn size_bytes(&self) -> u64 {
        let mut total = 0;
        for suffix in ["", "-wal", "-shm"] {
            let p = PathBuf::from(format!("{}{}", self.path.display(), suffix));
            total += std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        }
        total
    }
}

fn init(conn: &mut Connection) -> Result<(), String> {
    conn.pragma_update(None, "journal_mode", "WAL").map_err(|e| e.to_string())?;
    // 告警与审计状态要求“写入即持久”。
    conn.pragma_update(None, "synchronous", "FULL").map_err(|e| e.to_string())?;
    conn.pragma_update(None, "busy_timeout", 5000).map_err(|e| e.to_string())?;
    conn.pragma_update(None, "foreign_keys", "ON").map_err(|e| e.to_string())?;
    let _ = conn.execute_batch("PRAGMA mmap_size=268435456; PRAGMA cache_size=-16384;");
    conn.execute_batch(SCHEMA).map_err(|e| format!("建表失败：{e}"))?;
    conn.execute_batch(crate::probes::SCHEMA).map_err(|e| format!("建表失败：{e}"))?;
    conn.execute_batch(crate::accounts::SCHEMA).map_err(|e| format!("建表失败：{e}"))?;
    conn.execute_batch(crate::relayspec::SCHEMA).map_err(|e| format!("建表失败：{e}"))?;
    Ok(())
}

// ─── meta ───────────────────────────────────────────────────────────────────────────────────────────

pub fn meta_get(c: &Connection, key: &str) -> Option<String> {
    c.query_row("SELECT value FROM meta WHERE key=?", [key], |r| r.get(0)).optional().ok().flatten()
}

pub fn meta_set(c: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value],
    )?;
    Ok(())
}

// ─── audit：只追加，带哈希链 ────────────────────────────────────────────────────────────────────────

pub fn audit(c: &Connection, ts: f64, actor: &str, action: &str, target: &str, detail: &Value) -> rusqlite::Result<i64> {
    let prev: String = c
        .query_row("SELECT hash FROM audit ORDER BY id DESC LIMIT 1", [], |r| r.get(0))
        .optional()?
        .unwrap_or_else(|| "0".repeat(64));
    let detail_s = serde_json::to_string(detail).unwrap_or_else(|_| "{}".into());
    let mut h = Sha256::new();
    h.update(prev.as_bytes());
    h.update(b"\n");
    h.update(format!("{ts:.6}\n{actor}\n{action}\n{target}\n{detail_s}").as_bytes());
    let hash = hex::encode(h.finalize());
    c.execute(
        "INSERT INTO audit(ts,actor,action,target,detail,prev_hash,hash) VALUES(?,?,?,?,?,?,?)",
        params![ts, actor, action, target, detail_s, prev, hash],
    )?;
    Ok(c.last_insert_rowid())
}

/// 校验整条哈希链；返回第一处断点的 id。
pub fn audit_verify(c: &Connection) -> rusqlite::Result<Option<i64>> {
    let mut stmt = c.prepare("SELECT id,ts,actor,action,target,detail,prev_hash,hash FROM audit ORDER BY id")?;
    let mut rows = stmt.query([])?;
    // 保留期从左边缘截断过：从保留下来的第一条的 prev_hash 起算
    let mut prev: Option<String> = None;
    while let Some(r) = rows.next()? {
        let id: i64 = r.get(0)?;
        let ts: f64 = r.get(1)?;
        let (actor, action, target, detail, p, hash): (String, String, String, String, String, String) =
            (r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?);
        let mut h = Sha256::new();
        h.update(p.as_bytes());
        h.update(b"\n");
        h.update(format!("{ts:.6}\n{actor}\n{action}\n{target}\n{detail}").as_bytes());
        if prev.as_deref().map(|x| x != p).unwrap_or(false) || hex::encode(h.finalize()) != hash {
            return Ok(Some(id));
        }
        prev = Some(hash);
    }
    Ok(None)
}

pub fn audit_list(c: &Connection, limit: i64) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = c.prepare("SELECT id,ts,actor,action,target,detail FROM audit ORDER BY id DESC LIMIT ?")?;
    let rows = stmt.query_map([limit], |r| {
        let detail: String = r.get(5)?;
        Ok(serde_json::json!({"id": r.get::<_, i64>(0)?, "ts": r.get::<_, f64>(1)?, "actor": r.get::<_, String>(2)?,
            "action": r.get::<_, String>(3)?, "target": r.get::<_, String>(4)?,
            "detail": serde_json::from_str::<Value>(&detail).unwrap_or(Value::Null)}))
    })?;
    rows.collect()
}

// ─── notify_log ─────────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct NotifyRow {
    pub ts: f64,
    pub channel: String,
    pub status: String,
    pub kind: String,
    pub severity: String,
    pub group_key: Option<String>,
    pub title: String,
    pub body: String,
    pub alert_ids: Vec<i64>,
    pub error: Option<String>,
    pub latency_ms: Option<i64>,
}

pub fn notify_insert(c: &Connection, n: &NotifyRow) -> rusqlite::Result<i64> {
    c.execute(
        "INSERT INTO notify_log(ts,channel,status,kind,severity,group_key,title,body,alert_ids,error,latency_ms) VALUES(?,?,?,?,?,?,?,?,?,?,?)",
        params![n.ts, n.channel, n.status, n.kind, n.severity, n.group_key, n.title, n.body,
            serde_json::to_string(&n.alert_ids).unwrap(), n.error, n.latency_ms],
    )?;
    Ok(c.last_insert_rowid())
}

pub fn notify_list(c: &Connection, limit: i64, status: Option<&str>, channel: Option<&str>) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = c.prepare(
        "SELECT id,ts,channel,status,kind,severity,group_key,title,body,alert_ids,error,latency_ms FROM notify_log
         WHERE (?1 IS NULL OR status=?1) AND (?2 IS NULL OR channel=?2) ORDER BY id DESC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![status, channel, limit], |r| {
        let ids: String = r.get(9)?;
        Ok(serde_json::json!({"id": r.get::<_, i64>(0)?, "ts": r.get::<_, f64>(1)?, "channel": r.get::<_, String>(2)?,
            "status": r.get::<_, String>(3)?, "kind": r.get::<_, String>(4)?, "severity": r.get::<_, String>(5)?,
            "group_key": r.get::<_, Option<String>>(6)?, "title": r.get::<_, String>(7)?, "body": r.get::<_, String>(8)?,
            "alert_ids": serde_json::from_str::<Value>(&ids).unwrap_or(Value::Null),
            "error": r.get::<_, Option<String>>(10)?, "latency_ms": r.get::<_, Option<i64>>(11)?}))
    })?;
    rows.collect()
}

// ─── retention ──────────────────────────────────────────────────────────────────────────────────────

pub fn retention(c: &Connection, now: f64) -> rusqlite::Result<(usize, usize, usize)> {
    let day = 86400.0;
    let a = c.execute("DELETE FROM alerts WHERE state='resolved' AND resolved_at < ?", [now - 90.0 * day])?;
    let n = c.execute("DELETE FROM notify_log WHERE ts < ?", [now - 90.0 * day])?;
    // 审计链从左边缘截断：保留剩余部分的链接关系（第一条的 prev_hash 指向已删除的行，校验时从保留的第一条起算）。
    let au = c.execute("DELETE FROM audit WHERE ts < ?", [now - 365.0 * day])?;
    c.execute("DELETE FROM silences WHERE ends_at < ?", [now - 90.0 * day])?;
    Ok((a, n, au))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_chain() {
        let s = Store::open_memory();
        s.call_blocking(|c| {
            audit(c, 1.0, "admin", "alert.ack", "alert:1", &serde_json::json!({"note": "x"})).unwrap();
            audit(c, 2.0, "admin", "silence.create", "silence:1", &serde_json::json!({})).unwrap();
            assert_eq!(audit_verify(c).unwrap(), None);
            c.execute("UPDATE audit SET actor='evil' WHERE id=1", []).unwrap();
            assert_eq!(audit_verify(c).unwrap(), Some(1));
        });
    }

    #[test]
    fn retention_windows() {
        let s = Store::open_memory();
        s.call_blocking(|c| {
            let now = 100.0 * 86400.0 * 10.0;
            let row = |ts: f64| NotifyRow { ts, channel: "shadow".into(), status: "shadow".into(), kind: "group".into(),
                severity: "warning".into(), group_key: None, title: "t".into(), body: "b".into(), alert_ids: vec![],
                error: None, latency_ms: None };
            notify_insert(c, &row(now - 91.0 * 86400.0)).unwrap();
            notify_insert(c, &row(now - 89.0 * 86400.0)).unwrap();
            audit(c, now - 366.0 * 86400.0, "a", "x", "t", &Value::Null).unwrap();
            audit(c, now - 300.0 * 86400.0, "a", "x", "t", &Value::Null).unwrap();
            let (_, n, au) = retention(c, now).unwrap();
            assert_eq!((n, au), (1, 1));
        });
    }
}
