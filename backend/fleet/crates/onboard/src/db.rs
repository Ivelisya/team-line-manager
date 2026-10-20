//! SQLite：saga 日志（rows / steps / step_attempts）、事件、线路机批次、分配、机密、资产、主机公钥。
//! 单文件、WAL、synchronous=FULL；所有写都是短事务。进程崩了以后，库里的状态就是恢复的起点。

use crate::intake::RowSpec;
use crate::model::{cell, row as rs, STEPS};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS batches(
  id TEXT PRIMARY KEY, title TEXT NOT NULL, csv_path TEXT, csv_text TEXT NOT NULL,
  options TEXT NOT NULL, plan_hash TEXT NOT NULL, status TEXT NOT NULL, operator TEXT NOT NULL,
  created_at INTEGER NOT NULL, started_at INTEGER, finished_at INTEGER,
  lease_owner TEXT, lease_until INTEGER);
CREATE TABLE IF NOT EXISTS rows(
  id TEXT PRIMARY KEY, batch_id TEXT NOT NULL REFERENCES batches(id), seq INTEGER NOT NULL, spec TEXT NOT NULL,
  name TEXT NOT NULL, ip TEXT NOT NULL, ssh_port INTEGER NOT NULL, relay TEXT,
  state TEXT NOT NULL, current_step TEXT NOT NULL, reason TEXT, is_canary INTEGER NOT NULL DEFAULT 0,
  ready_since INTEGER, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, finished_at INTEGER);
-- 同一台机器（ip, ssh_port）同时只能有一个进行中的运行；failed 不释放（要人处置）。
CREATE UNIQUE INDEX IF NOT EXISTS rows_live_ip ON rows(ip, ssh_port) WHERE state NOT IN ('active','cancelled');
CREATE TABLE IF NOT EXISTS steps(
  row_id TEXT NOT NULL, step TEXT NOT NULL, status TEXT NOT NULL, attempt INTEGER NOT NULL DEFAULT 0,
  started_at INTEGER, finished_at INTEGER, outputs TEXT, error_class TEXT, detail TEXT,
  PRIMARY KEY(row_id, step));
CREATE TABLE IF NOT EXISTS step_attempts(
  row_id TEXT NOT NULL, step TEXT NOT NULL, attempt INTEGER NOT NULL, started_at INTEGER NOT NULL,
  finished_at INTEGER, result TEXT, error_class TEXT, detail TEXT, PRIMARY KEY(row_id, step, attempt));
CREATE TABLE IF NOT EXISTS events(
  id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, batch_id TEXT, row_id TEXT, type TEXT NOT NULL, data TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS events_batch ON events(batch_id, id);
CREATE TABLE IF NOT EXISTS relay_batches(
  id TEXT PRIMARY KEY, batch_id TEXT NOT NULL, relay TEXT NOT NULL, batch_no INTEGER NOT NULL, status TEXT NOT NULL,
  members TEXT NOT NULL, report TEXT, error TEXT, created_at INTEGER NOT NULL, finished_at INTEGER);
CREATE TABLE IF NOT EXISTS allocations(
  kind TEXT NOT NULL, scope TEXT NOT NULL, value INTEGER NOT NULL, status TEXT NOT NULL, row_id TEXT,
  created_at INTEGER NOT NULL, PRIMARY KEY(kind, scope, value));
CREATE TABLE IF NOT EXISTS secrets(
  row_id TEXT NOT NULL, kind TEXT NOT NULL, value TEXT NOT NULL, created_at INTEGER NOT NULL, PRIMARY KEY(row_id, kind));
CREATE TABLE IF NOT EXISTS assets(
  uuid TEXT PRIMARY KEY, row_id TEXT UNIQUE, name TEXT NOT NULL, short_name TEXT NOT NULL, team TEXT NOT NULL, number INTEGER NOT NULL,
  provider TEXT NOT NULL, provider_code TEXT NOT NULL, plan_id TEXT NOT NULL, price TEXT, currency TEXT, service_id TEXT,
  ip TEXT NOT NULL, ssh_port INTEGER NOT NULL, ss_public_port INTEGER NOT NULL, nat INTEGER NOT NULL,
  relay TEXT NOT NULL, relay_port INTEGER NOT NULL, inbound_tag TEXT NOT NULL, outbound_tag TEXT NOT NULL, rule_tag TEXT NOT NULL,
  email TEXT NOT NULL, ss_impl TEXT NOT NULL, ss_method TEXT NOT NULL, lifecycle TEXT NOT NULL,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE UNIQUE INDEX IF NOT EXISTS assets_team_number ON assets(team, number) WHERE lifecycle <> 'retired';
CREATE UNIQUE INDEX IF NOT EXISTS assets_relay_port ON assets(relay, relay_port) WHERE lifecycle <> 'retired';
CREATE TABLE IF NOT EXISTS host_keys(
  row_id TEXT NOT NULL, phase TEXT NOT NULL, source TEXT NOT NULL, key TEXT NOT NULL, samples INTEGER NOT NULL, ts INTEGER NOT NULL,
  PRIMARY KEY(row_id, phase, source, key));
"#;

#[derive(Clone)]
pub struct Db {
    c: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone)]
pub struct Batch {
    pub id: String,
    pub title: String,
    pub csv_path: Option<String>,
    pub csv_text: String,
    pub options: Value,
    pub plan_hash: String,
    pub status: String,
    pub operator: String,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub lease_owner: Option<String>,
    pub lease_until: Option<i64>,
    /// 批次级错误（启动失败等），给界面显示
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub id: String,
    pub batch_id: String,
    pub seq: i64,
    pub spec: RowSpec,
    pub relay: String,
    pub state: String,
    pub current_step: String,
    pub reason: Option<String>,
    pub is_canary: bool,
    pub ready_since: Option<i64>,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct StepRec {
    pub step: String,
    pub status: String,
    pub attempt: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub outputs: Value,
    pub error_class: Option<String>,
    pub detail: Option<String>,
}

type R<T> = Result<T, String>;
fn e(x: rusqlite::Error) -> String {
    format!("数据库：{x}")
}

impl Db {
    pub fn open(path: &std::path::Path) -> R<Db> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d).map_err(|x| format!("建目录 {}：{x}", d.display()))?;
        }
        let c = Connection::open(path).map_err(e)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        c.busy_timeout(std::time::Duration::from_secs(10)).map_err(e)?;
        c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;").map_err(e)?;
        c.execute_batch(SCHEMA).map_err(e)?;
        let has_err: bool = c.prepare("SELECT 1 FROM pragma_table_info('batches') WHERE name='error'").and_then(|mut s| s.exists([])).map_err(e)?;
        if !has_err {
            c.execute_batch("ALTER TABLE batches ADD COLUMN error TEXT").map_err(e)?;
        }
        let has_cancel: bool = c.prepare("SELECT 1 FROM pragma_table_info('batches') WHERE name='cancel_req'").and_then(|mut s| s.exists([])).map_err(e)?;
        if !has_cancel {
            c.execute_batch("ALTER TABLE batches ADD COLUMN cancel_req TEXT").map_err(e)?;
        }
        Ok(Db { c: Arc::new(Mutex::new(c)) })
    }

    fn with<T>(&self, f: impl FnOnce(&mut Connection) -> rusqlite::Result<T>) -> R<T> {
        let mut g = self.c.lock().unwrap();
        f(&mut g).map_err(e)
    }

    // ── 批次 ──────────────────────────────────────────────────────────────
    pub fn create_batch(&self, b: &Batch, rows: &[(RowSpec, String)]) -> R<()> {
        self.with(|c| {
            let tx = c.transaction()?;
            tx.execute(
                "INSERT INTO batches(id,title,csv_path,csv_text,options,plan_hash,status,operator,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![b.id, b.title, b.csv_path, b.csv_text, b.options.to_string(), b.plan_hash, b.status, b.operator, b.created_at],
            )?;
            for (i, (spec, relay)) in rows.iter().enumerate() {
                let rid = format!("{}-{}", b.id, spec.seq);
                let canary = b.options["canary"].as_bool().unwrap_or(false) && i == 0;
                tx.execute(
                    "INSERT INTO rows(id,batch_id,seq,spec,name,ip,ssh_port,relay,state,current_step,is_canary,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'P1',?10,?11,?11)",
                    params![rid, b.id, spec.seq as i64, serde_json::to_string(spec).unwrap(), spec.name, spec.ip, spec.ssh_port, relay, rs::QUEUED, canary as i64, b.created_at],
                )?;
                for s in STEPS.iter() {
                    tx.execute("INSERT INTO steps(row_id,step,status) VALUES(?1,?2,?3)", params![rid, s.0, cell::QUEUED])?;
                }
            }
            tx.commit()
        })
    }

    fn batch_from(r: &rusqlite::Row) -> rusqlite::Result<Batch> {
        Ok(Batch {
            id: r.get(0)?,
            title: r.get(1)?,
            csv_path: r.get(2)?,
            csv_text: r.get(3)?,
            options: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or(json!({})),
            plan_hash: r.get(5)?,
            status: r.get(6)?,
            operator: r.get(7)?,
            created_at: r.get(8)?,
            started_at: r.get(9)?,
            finished_at: r.get(10)?,
            lease_owner: r.get(11)?,
            lease_until: r.get(12)?,
            error: r.get(13)?,
        })
    }
    const BCOLS: &'static str = "id,title,csv_path,csv_text,options,plan_hash,status,operator,created_at,started_at,finished_at,lease_owner,lease_until,error";

    pub fn batch(&self, id: &str) -> R<Option<Batch>> {
        self.with(|c| c.query_row(&format!("SELECT {} FROM batches WHERE id=?1", Self::BCOLS), [id], Self::batch_from).optional())
    }
    pub fn batches(&self) -> R<Vec<Batch>> {
        self.with(|c| {
            let mut s = c.prepare(&format!("SELECT {} FROM batches ORDER BY created_at DESC", Self::BCOLS))?;
            let v = s.query_map([], Self::batch_from)?.collect::<rusqlite::Result<Vec<_>>>();
            v
        })
    }
    /// 运行中的取消请求：JSON 数组，元素是行名，"*" 表示整批
    pub fn cancel_requests(&self, id: &str) -> R<Vec<String>> {
        self.with(|c| {
            let v: Option<String> = c.query_row("SELECT cancel_req FROM batches WHERE id=?1", [id], |r| r.get(0)).optional()?.flatten();
            Ok(v.and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok()).unwrap_or_default())
        })
    }
    pub fn add_cancel_request(&self, id: &str, what: &str) -> R<()> {
        let mut cur = self.cancel_requests(id)?;
        if !cur.iter().any(|x| x == what) {
            cur.push(what.to_string());
        }
        let j = serde_json::to_string(&cur).unwrap_or_default();
        self.with(|c| c.execute("UPDATE batches SET cancel_req=?2 WHERE id=?1", params![id, j]).map(|_| ()))
    }
    pub fn take_cancel_request(&self, id: &str, what: &str) -> R<()> {
        let cur: Vec<String> = self.cancel_requests(id)?.into_iter().filter(|x| x != what).collect();
        let j = if cur.is_empty() { None } else { Some(serde_json::to_string(&cur).unwrap_or_default()) };
        self.with(|c| c.execute("UPDATE batches SET cancel_req=?2 WHERE id=?1", params![id, j]).map(|_| ()))
    }
    pub fn set_batch_error(&self, id: &str, error: Option<&str>) -> R<()> {
        self.with(|c| c.execute("UPDATE batches SET error=?2 WHERE id=?1", params![id, error]).map(|_| ()))
    }
    pub fn set_batch_status(&self, id: &str, status: &str) -> R<()> {
        let now = now_ms();
        self.with(|c| {
            c.execute(
                "UPDATE batches SET status=?2, started_at=CASE WHEN ?2='running' AND started_at IS NULL THEN ?3 ELSE started_at END, finished_at=CASE WHEN ?2 IN ('finished','cancelled','attention') THEN ?3 ELSE NULL END WHERE id=?1",
                params![id, status, now],
            )
            .map(|_| ())
        })
    }
    /// 批次尚未开始（validated）时，选项变了连同计划哈希一起更新；已开始的批次只改选项
    pub fn set_batch_options_and_hash(&self, id: &str, options: &Value, plan_hash: &str) -> R<()> {
        self.with(|c| {
            c.execute("UPDATE batches SET options=?2, plan_hash=CASE WHEN status='validated' THEN ?3 ELSE plan_hash END WHERE id=?1",
                params![id, options.to_string(), plan_hash])
                .map(|_| ())
        })
    }

    /// 每个批次最新的事件 id（fleetd 据此推 SSE）
    pub fn event_heads(&self) -> R<Vec<(String, i64)>> {
        self.with(|c| {
            let mut s = c.prepare("SELECT batch_id, MAX(id) FROM events GROUP BY batch_id")?;
            let v = s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>();
            v
        })
    }

    pub fn set_batch_options(&self, id: &str, options: &Value) -> R<()> {
        self.with(|c| c.execute("UPDATE batches SET options=?2 WHERE id=?1", params![id, options.to_string()]).map(|_| ()))
    }

    /// 批次租约：同一时间只允许一个执行者；过期（崩溃）后 resume 可以接管。
    pub fn acquire_lease(&self, id: &str, owner: &str, ttl_ms: i64, force: bool) -> R<Result<(), String>> {
        let now = now_ms();
        self.with(|c| {
            let cur: Option<(Option<String>, Option<i64>)> =
                c.query_row("SELECT lease_owner, lease_until FROM batches WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let Some((o, u)) = cur else { return Ok(Err("批次不存在".into())) };
            if let (Some(o), Some(u)) = (&o, u) {
                if o != owner && u > now && !force {
                    return Ok(Err(format!("批次正由 {o} 执行（租约还剩 {} 秒）", (u - now) / 1000)));
                }
            }
            c.execute("UPDATE batches SET lease_owner=?2, lease_until=?3 WHERE id=?1", params![id, owner, now + ttl_ms])?;
            Ok(Ok(()))
        })
    }
    pub fn renew_lease(&self, id: &str, owner: &str, ttl_ms: i64) -> R<bool> {
        let now = now_ms();
        self.with(|c| Ok(c.execute("UPDATE batches SET lease_until=?3 WHERE id=?1 AND lease_owner=?2", params![id, owner, now + ttl_ms])? == 1))
    }
    pub fn release_lease(&self, id: &str, owner: &str) -> R<()> {
        self.with(|c| c.execute("UPDATE batches SET lease_owner=NULL, lease_until=NULL WHERE id=?1 AND lease_owner=?2", params![id, owner]).map(|_| ()))
    }

    // ── 行 ────────────────────────────────────────────────────────────────
    fn row_from(r: &rusqlite::Row) -> rusqlite::Result<Row> {
        let spec: String = r.get(3)?;
        Ok(Row {
            id: r.get(0)?,
            batch_id: r.get(1)?,
            seq: r.get(2)?,
            spec: serde_json::from_str(&spec).expect("row spec"),
            relay: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
            state: r.get(5)?,
            current_step: r.get(6)?,
            reason: r.get(7)?,
            is_canary: r.get::<_, i64>(8)? != 0,
            ready_since: r.get(9)?,
            updated_at: r.get(10)?,
        })
    }
    const RCOLS: &'static str = "id,batch_id,seq,spec,relay,state,current_step,reason,is_canary,ready_since,updated_at";

    pub fn rows(&self, batch: &str) -> R<Vec<Row>> {
        self.with(|c| {
            let mut s = c.prepare(&format!("SELECT {} FROM rows WHERE batch_id=?1 ORDER BY seq", Self::RCOLS))?;
            let v = s.query_map([batch], Self::row_from)?.collect::<rusqlite::Result<Vec<_>>>();
            v
        })
    }
    pub fn row(&self, id: &str) -> R<Option<Row>> {
        self.with(|c| c.query_row(&format!("SELECT {} FROM rows WHERE id=?1", Self::RCOLS), [id], Self::row_from).optional())
    }
    pub fn set_row(&self, id: &str, state: &str, step: &str, reason: Option<&str>) -> R<()> {
        let now = now_ms();
        self.with(|c| {
            c.execute(
                "UPDATE rows SET state=?2, current_step=?3, reason=?4, updated_at=?5,
                   ready_since=CASE WHEN ?2='ready_for_relay' THEN COALESCE(ready_since, ?5) WHEN ?2='in_relay_batch' THEN ready_since ELSE NULL END,
                   finished_at=CASE WHEN ?2 IN ('active','cancelled') THEN ?5 ELSE NULL END WHERE id=?1",
                params![id, state, step, reason, now],
            )
            .map(|_| ())
        })
    }
    pub fn set_row_spec(&self, id: &str, spec: &RowSpec) -> R<()> {
        self.with(|c| c.execute("UPDATE rows SET spec=?2 WHERE id=?1", params![id, serde_json::to_string(spec).unwrap()]).map(|_| ()))
    }
    pub fn set_row_relay(&self, id: &str, relay: &str) -> R<()> {
        self.with(|c| c.execute("UPDATE rows SET relay=?2 WHERE id=?1", params![id, relay]).map(|_| ()))
    }

    // ── 步骤 ──────────────────────────────────────────────────────────────
    pub fn step_start(&self, row: &str, step: &str) -> R<i64> {
        let now = now_ms();
        self.with(|c| {
            let tx = c.transaction()?;
            tx.execute(
                "UPDATE steps SET status='running', attempt=attempt+1, started_at=?3, finished_at=NULL, error_class=NULL, detail=NULL WHERE row_id=?1 AND step=?2",
                params![row, step, now],
            )?;
            let a: i64 = tx.query_row("SELECT attempt FROM steps WHERE row_id=?1 AND step=?2", params![row, step], |r| r.get(0))?;
            tx.execute(
                "INSERT OR REPLACE INTO step_attempts(row_id,step,attempt,started_at) VALUES(?1,?2,?3,?4)",
                params![row, step, a, now],
            )?;
            tx.commit()?;
            Ok(a)
        })
    }
    pub fn step_finish(&self, row: &str, step: &str, status: &str, outputs: Option<&Value>, class: Option<&str>, detail: Option<&str>) -> R<()> {
        let now = now_ms();
        self.with(|c| {
            let tx = c.transaction()?;
            let out = outputs.map(|o| o.to_string());
            tx.execute(
                "UPDATE steps SET status=?3, finished_at=?4, outputs=COALESCE(?5, outputs), error_class=?6, detail=?7 WHERE row_id=?1 AND step=?2",
                params![row, step, status, now, out, class, detail],
            )?;
            tx.execute(
                "UPDATE step_attempts SET finished_at=?3, result=?4, error_class=?5, detail=?6 WHERE row_id=?1 AND step=?2 AND attempt=(SELECT attempt FROM steps WHERE row_id=?1 AND step=?2)",
                params![row, step, now, status, class, detail],
            )?;
            tx.commit()
        })
    }
    /// 不经执行直接设置格子（跳过、人工闸门确认、补偿）。
    pub fn step_set(&self, row: &str, step: &str, status: &str, detail: Option<&str>) -> R<()> {
        let now = now_ms();
        self.with(|c| {
            c.execute(
                "UPDATE steps SET status=?3, detail=?4, finished_at=CASE WHEN ?3 IN ('queued') THEN NULL ELSE ?5 END, started_at=CASE WHEN ?3='queued' THEN NULL ELSE COALESCE(started_at, ?5) END WHERE row_id=?1 AND step=?2",
                params![row, step, status, detail, now],
            )
            .map(|_| ())
        })
    }
    pub fn step_outputs_merge(&self, row: &str, step: &str, patch: &Value) -> R<()> {
        let cur = self.step(row, step)?.map(|s| s.outputs).unwrap_or(json!({}));
        let mut m = if cur.is_object() { cur } else { json!({}) };
        for (k, v) in patch.as_object().into_iter().flatten() {
            m[k] = v.clone();
        }
        self.with(|c| c.execute("UPDATE steps SET outputs=?3 WHERE row_id=?1 AND step=?2", params![row, step, m.to_string()]).map(|_| ()))
    }
    fn step_from(r: &rusqlite::Row) -> rusqlite::Result<StepRec> {
        Ok(StepRec {
            step: r.get(0)?,
            status: r.get(1)?,
            attempt: r.get(2)?,
            started_at: r.get(3)?,
            finished_at: r.get(4)?,
            outputs: r.get::<_, Option<String>>(5)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({})),
            error_class: r.get(6)?,
            detail: r.get(7)?,
        })
    }
    pub fn steps(&self, row: &str) -> R<Vec<StepRec>> {
        self.with(|c| {
            let mut s = c.prepare("SELECT step,status,attempt,started_at,finished_at,outputs,error_class,detail FROM steps WHERE row_id=?1")?;
            let mut v = s.query_map([row], Self::step_from)?.collect::<rusqlite::Result<Vec<_>>>()?;
            v.sort_by_key(|s| crate::model::step_index(&s.step));
            Ok(v)
        })
    }
    pub fn step(&self, row: &str, step: &str) -> R<Option<StepRec>> {
        self.with(|c| {
            c.query_row(
                "SELECT step,status,attempt,started_at,finished_at,outputs,error_class,detail FROM steps WHERE row_id=?1 AND step=?2",
                params![row, step],
                Self::step_from,
            )
            .optional()
        })
    }

    // ── 事件 ──────────────────────────────────────────────────────────────
    pub fn event(&self, batch: &str, row: Option<&str>, kind: &str, data: Value) -> R<i64> {
        let now = now_ms();
        self.with(|c| {
            c.execute("INSERT INTO events(ts,batch_id,row_id,type,data) VALUES(?1,?2,?3,?4,?5)", params![now, batch, row, kind, data.to_string()])?;
            Ok(c.last_insert_rowid())
        })
    }
    pub fn events(&self, batch: &str, after: i64, limit: i64) -> R<Vec<Value>> {
        self.with(|c| {
            let mut s = c.prepare("SELECT id,ts,row_id,type,data FROM events WHERE batch_id=?1 AND id>?2 ORDER BY id LIMIT ?3")?;
            let v = s
                .query_map(params![batch, after, limit], |r| {
                    Ok(json!({"id": r.get::<_, i64>(0)?, "ts": r.get::<_, i64>(1)?, "batch_id": batch, "node_id": r.get::<_, Option<String>>(2)?,
                              "type": r.get::<_, String>(3)?, "data": serde_json::from_str::<Value>(&r.get::<_, String>(4)?).unwrap_or(Value::Null)}))
                })?
                .collect::<rusqlite::Result<Vec<_>>>();
            v
        })
    }

    // ── 机密（今晚：明文存本库，库文件 0600；保险库接入见 WIRING.md）───────────
    pub fn secret(&self, row: &str, kind: &str) -> R<Option<String>> {
        self.with(|c| c.query_row("SELECT value FROM secrets WHERE row_id=?1 AND kind=?2", params![row, kind], |r| r.get(0)).optional())
    }
    /// 已有就返回已有的（幂等：重跑不会换密码/换密钥）。
    pub fn secret_or_insert(&self, row: &str, kind: &str, gen: impl FnOnce() -> String) -> R<String> {
        if let Some(v) = self.secret(row, kind)? {
            return Ok(v);
        }
        let v = gen();
        let now = now_ms();
        self.with(|c| c.execute("INSERT OR IGNORE INTO secrets(row_id,kind,value,created_at) VALUES(?1,?2,?3,?4)", params![row, kind, v, now]).map(|_| ()))?;
        Ok(self.secret(row, kind)?.unwrap())
    }
    pub fn secret_set(&self, row: &str, kind: &str, v: &str) -> R<()> {
        let now = now_ms();
        self.with(|c| c.execute("INSERT OR REPLACE INTO secrets(row_id,kind,value,created_at) VALUES(?1,?2,?3,?4)", params![row, kind, v, now]).map(|_| ()))
    }

    // ── 分配 ──────────────────────────────────────────────────────────────
    /// 线路机端口：本行已有预留就复用；否则取 max(配置里在用, 库里分配过, lo-1)+1。
    /// 用 max+1 而不是「最小空闲」：不复用刚被删掉的端口，避免旧分享链接误连到新节点。
    pub fn allocate_port(&self, relay: &str, row: &str, in_use: &[u16], lo: u16, hi: u16) -> R<Result<u16, String>> {
        let now = now_ms();
        self.with(|c| {
            let tx = c.transaction()?;
            let mine: Option<i64> = tx
                .query_row("SELECT value FROM allocations WHERE kind='relay_port' AND scope=?1 AND row_id=?2 AND status IN ('reserved','active')", params![relay, row], |r| r.get(0))
                .optional()?;
            if let Some(p) = mine {
                tx.commit()?;
                return Ok(Ok(p as u16));
            }
            let db_max: Option<i64> = tx.query_row("SELECT MAX(value) FROM allocations WHERE kind='relay_port' AND scope=?1", [relay], |r| r.get(0))?;
            let cfg_max = in_use.iter().copied().filter(|p| *p >= lo && *p <= hi).max().map(|p| p as i64);
            let next = db_max.unwrap_or(lo as i64 - 1).max(cfg_max.unwrap_or(lo as i64 - 1)) + 1;
            if next > hi as i64 {
                return Ok(Err(format!("{relay} 的端口段 {lo}–{hi} 用完了")));
            }
            tx.execute(
                "INSERT INTO allocations(kind,scope,value,status,row_id,created_at) VALUES('relay_port',?1,?2,'reserved',?3,?4)",
                params![relay, next, row, now],
            )?;
            tx.commit()?;
            Ok(Ok(next as u16))
        })
    }
    pub fn set_allocation(&self, row: &str, status: &str) -> R<()> {
        self.with(|c| c.execute("UPDATE allocations SET status=?2 WHERE row_id=?1", params![row, status]).map(|_| ()))
    }
    /// 端口冲突时作废本行的预留（标 retired，不复用），下次分配拿新端口。
    pub fn drop_allocation(&self, row: &str) -> R<()> {
        self.with(|c| c.execute("UPDATE allocations SET status='retired', row_id=NULL WHERE row_id=?1 AND status='reserved'", [row]).map(|_| ()))
    }

    // ── 资产 ──────────────────────────────────────────────────────────────
    pub fn upsert_asset(&self, a: &Value) -> R<()> {
        let now = now_ms();
        self.with(|c| {
            c.execute(
                "INSERT INTO assets(uuid,row_id,name,short_name,team,number,provider,provider_code,plan_id,price,currency,service_id,ip,ssh_port,ss_public_port,nat,relay,relay_port,inbound_tag,outbound_tag,rule_tag,email,ss_impl,ss_method,lifecycle,created_at,updated_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?26)
                 ON CONFLICT(row_id) DO UPDATE SET relay=excluded.relay, relay_port=excluded.relay_port, inbound_tag=excluded.inbound_tag, rule_tag=excluded.rule_tag, updated_at=excluded.updated_at",
                params![
                    a["uuid"].as_str(), a["row_id"].as_str(), a["name"].as_str(), a["short_name"].as_str(), a["team"].as_str(), a["number"].as_i64(),
                    a["provider"].as_str(), a["provider_code"].as_str(), a["plan_id"].as_str(), a["price"].as_str(), a["currency"].as_str(), a["service_id"].as_str(),
                    a["ip"].as_str(), a["ssh_port"].as_i64(), a["ss_public_port"].as_i64(), a["nat"].as_bool().unwrap_or(false) as i64,
                    a["relay"].as_str(), a["relay_port"].as_i64(), a["inbound_tag"].as_str(), a["outbound_tag"].as_str(), a["rule_tag"].as_str(),
                    a["email"].as_str(), a["ss_impl"].as_str(), a["ss_method"].as_str(), a["lifecycle"].as_str(), now
                ],
            )
            .map(|_| ())
        })
    }
    pub fn asset_by_row(&self, row: &str) -> R<Option<Value>> {
        self.with(|c| {
            c.query_row(
                "SELECT uuid,name,short_name,team,number,provider,plan_id,ip,ss_public_port,relay,relay_port,inbound_tag,outbound_tag,rule_tag,email,lifecycle FROM assets WHERE row_id=?1",
                [row],
                |r| {
                    Ok(json!({"uuid": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?, "short_name": r.get::<_, String>(2)?, "team": r.get::<_, String>(3)?,
                              "number": r.get::<_, i64>(4)?, "provider": r.get::<_, String>(5)?, "plan_id": r.get::<_, String>(6)?, "ip": r.get::<_, String>(7)?,
                              "ss_public_port": r.get::<_, i64>(8)?, "relay": r.get::<_, String>(9)?, "relay_port": r.get::<_, i64>(10)?,
                              "inbound_tag": r.get::<_, String>(11)?, "outbound_tag": r.get::<_, String>(12)?, "rule_tag": r.get::<_, String>(13)?,
                              "email": r.get::<_, String>(14)?, "lifecycle": r.get::<_, String>(15)?}))
                },
            )
            .optional()
        })
    }
    pub fn set_asset_lifecycle(&self, row: &str, lc: &str) -> R<()> {
        let now = now_ms();
        self.with(|c| c.execute("UPDATE assets SET lifecycle=?2, updated_at=?3 WHERE row_id=?1", params![row, lc, now]).map(|_| ()))
    }
    pub fn delete_asset(&self, row: &str) -> R<()> {
        self.with(|c| c.execute("DELETE FROM assets WHERE row_id=?1", [row]).map(|_| ()))
    }
    pub fn assets(&self) -> R<Vec<Value>> {
        self.with(|c| {
            let mut s = c.prepare("SELECT row_id FROM assets ORDER BY created_at")?;
            let v = s.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>();
            v
        })
        .and_then(|ids| ids.iter().filter_map(|i| self.asset_by_row(i).transpose()).collect())
    }
    pub fn taken_numbers(&self) -> R<Vec<(String, i64, String)>> {
        self.with(|c| {
            let mut s = c.prepare("SELECT team, number, COALESCE(row_id,'') FROM assets WHERE lifecycle <> 'retired'")?;
            let v = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<Vec<_>>>();
            v
        })
    }

    // ── 主机公钥 ───────────────────────────────────────────────────────────
    pub fn host_key(&self, row: &str, phase: &str, source: &str, key: &str, samples: i64) -> R<()> {
        let now = now_ms();
        self.with(|c| {
            c.execute(
                "INSERT INTO host_keys(row_id,phase,source,key,samples,ts) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(row_id,phase,source,key) DO UPDATE SET samples=samples+excluded.samples, ts=excluded.ts",
                params![row, phase, source, key, samples, now],
            )
            .map(|_| ())
        })
    }

    // ── 线路机批次 ─────────────────────────────────────────────────────────
    pub fn relay_batch_create(&self, batch: &str, relay: &str, members: &[String]) -> R<(String, i64)> {
        let now = now_ms();
        self.with(|c| {
            let no: i64 = c.query_row("SELECT COALESCE(MAX(batch_no),0)+1 FROM relay_batches WHERE batch_id=?1 AND relay=?2", params![batch, relay], |r| r.get(0))?;
            let id = format!("{batch}-{relay}-{no}");
            c.execute(
                "INSERT INTO relay_batches(id,batch_id,relay,batch_no,status,members,created_at) VALUES(?1,?2,?3,?4,'merging',?5,?6)",
                params![id, batch, relay, no, json!(members).to_string(), now],
            )?;
            Ok((id, no))
        })
    }
    pub fn relay_batch_finish(&self, id: &str, status: &str, members: &[String], report: &Value, error: Option<&str>) -> R<()> {
        let now = now_ms();
        self.with(|c| {
            c.execute(
                "UPDATE relay_batches SET status=?2, members=?3, report=?4, error=?5, finished_at=?6 WHERE id=?1",
                params![id, status, json!(members).to_string(), report.to_string(), error, now],
            )
            .map(|_| ())
        })
    }
    pub fn relay_batches(&self, batch: &str) -> R<Vec<Value>> {
        self.with(|c| {
            let mut s = c.prepare("SELECT id,relay,batch_no,status,members,report,error,created_at,finished_at FROM relay_batches WHERE batch_id=?1 ORDER BY created_at")?;
            let v = s
                .query_map([batch], |r| {
                    Ok(json!({"id": r.get::<_, String>(0)?, "relay": r.get::<_, String>(1)?, "batch_no": r.get::<_, i64>(2)?, "status": r.get::<_, String>(3)?,
                              "members": serde_json::from_str::<Value>(&r.get::<_, String>(4)?).unwrap_or(json!([])),
                              "report": r.get::<_, Option<String>>(5)?.and_then(|s| serde_json::from_str::<Value>(&s).ok()).unwrap_or(Value::Null),
                              "error": r.get::<_, Option<String>>(6)?, "created_at": r.get::<_, i64>(7)?, "finished_at": r.get::<_, Option<i64>>(8)?}))
                })?
                .collect::<rusqlite::Result<Vec<_>>>();
            v
        })
    }
    /// 上次崩溃时停在 merging 的线路机批次：标记为 interrupted（成员会由 resume 重新组批，接入本身幂等）。
    pub fn relay_batches_interrupt(&self, batch: &str) -> R<usize> {
        let now = now_ms();
        self.with(|c| c.execute("UPDATE relay_batches SET status='interrupted', finished_at=?2 WHERE batch_id=?1 AND status='merging'", params![batch, now]))
    }
}
