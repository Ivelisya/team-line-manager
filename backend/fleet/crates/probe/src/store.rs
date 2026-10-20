//! 边缘 SQLite：`probe_raw` 保存每次尝试（24 h），`kv.probe_push_acked` 记推送游标。
//!
//! relayd 可以把这两张表建在自己的 edge.db 里（同一个连接），也可以单独一个文件；表名都带 probe 前缀，不与别的表冲突。
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use crate::types::{CheckKind, ProbeBatch, ProbeResult, TargetInfo, SCHEMA};

pub const RETENTION_S: f64 = 86_400.0;

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS probe_raw(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  node TEXT NOT NULL,
  check_kind TEXT NOT NULL,
  status TEXT NOT NULL,
  measured_at REAL NOT NULL,
  reason_code TEXT,
  value_ms REAL,
  actual_ip TEXT,
  attempt INTEGER,
  final INTEGER,
  doc TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS probe_raw_at ON probe_raw(measured_at);
CREATE INDEX IF NOT EXISTS probe_raw_node ON probe_raw(node, check_kind, id);
CREATE TABLE IF NOT EXISTS probe_kv(k TEXT PRIMARY KEY, v TEXT NOT NULL);
";

pub struct ProbeStore {
    c: Connection,
}

impl ProbeStore {
    pub fn open(path: &Path) -> rusqlite::Result<ProbeStore> {
        let c = Connection::open(path)?;
        c.pragma_update(None, "journal_mode", "WAL")?;
        c.pragma_update(None, "synchronous", "NORMAL")?;
        Self::with_connection(c)
    }

    /// 用调用方已经打开的连接（例如 relayd 的 edge.db）
    pub fn with_connection(c: Connection) -> rusqlite::Result<ProbeStore> {
        c.execute_batch(SCHEMA_SQL)?;
        Ok(ProbeStore { c })
    }

    /// 一个事务写入，回填 id
    pub fn insert(&mut self, rs: &mut [ProbeResult]) -> rusqlite::Result<()> {
        let tx = self.c.transaction()?;
        {
            let mut st = tx.prepare_cached(
                "INSERT INTO probe_raw(node, check_kind, status, measured_at, reason_code, value_ms, actual_ip, attempt, final, doc)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            )?;
            for r in rs.iter_mut() {
                r.id = None;
                let doc = serde_json::to_string(&*r).expect("ProbeResult 可序列化");
                st.execute(params![
                    r.node,
                    r.check.as_str(),
                    r.status.as_str(),
                    r.measured_at,
                    r.reason_code,
                    r.value_ms,
                    r.actual_ip,
                    r.attempt,
                    r.final_attempt,
                    doc
                ])?;
                r.id = Some(tx.last_insert_rowid());
            }
        }
        tx.commit()
    }

    /// 删掉 24 h 以前的行，返回删除行数
    pub fn prune(&self, now: f64) -> rusqlite::Result<usize> {
        self.c.execute("DELETE FROM probe_raw WHERE measured_at < ?1", params![now - RETENTION_S])
    }

    pub fn acked(&self) -> rusqlite::Result<i64> {
        Ok(self
            .c
            .query_row("SELECT v FROM probe_kv WHERE k='probe_push_acked'", [], |r| r.get::<_, String>(0))
            .optional()?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0))
    }

    /// 取游标之后最多 `limit` 条，组成推送批次；没有新数据返回 None
    pub fn pending(&self, relay: &str, limit: usize, targets: Vec<TargetInfo>, now: f64) -> rusqlite::Result<Option<ProbeBatch>> {
        let after = self.acked()?;
        let mut st = self.c.prepare_cached("SELECT id, doc FROM probe_raw WHERE id > ?1 ORDER BY id LIMIT ?2")?;
        let rows = st.query_map(params![after, limit as i64], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut results = Vec::new();
        for row in rows {
            let (id, doc) = row?;
            if let Ok(mut p) = serde_json::from_str::<ProbeResult>(&doc) {
                p.id = Some(id);
                results.push(p);
            }
        }
        if results.is_empty() {
            return Ok(None);
        }
        Ok(Some(ProbeBatch {
            schema: SCHEMA.into(),
            relay: relay.into(),
            generated_at: now,
            first_id: results.first().and_then(|r| r.id).unwrap_or(0),
            last_id: results.last().and_then(|r| r.id).unwrap_or(0),
            results,
            targets,
        }))
    }

    /// 中控确认后前移游标
    pub fn ack(&self, last_id: i64) -> rusqlite::Result<()> {
        self.c.execute(
            "INSERT INTO probe_kv(k, v) VALUES('probe_push_acked', ?1) ON CONFLICT(k) DO UPDATE SET v=excluded.v
             WHERE CAST(excluded.v AS INTEGER) > CAST(probe_kv.v AS INTEGER)",
            params![last_id.to_string()],
        )?;
        Ok(())
    }

    pub fn latest(&self, node: &str, check: CheckKind) -> rusqlite::Result<Option<ProbeResult>> {
        let doc: Option<(i64, String)> = self
            .c
            .query_row(
                "SELECT id, doc FROM probe_raw WHERE node=?1 AND check_kind=?2 ORDER BY id DESC LIMIT 1",
                params![node, check.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(doc.and_then(|(id, d)| {
            let mut p: ProbeResult = serde_json::from_str(&d).ok()?;
            p.id = Some(id);
            Some(p)
        }))
    }

    pub fn count(&self) -> rusqlite::Result<i64> {
        self.c.query_row("SELECT COUNT(*) FROM probe_raw", [], |r| r.get(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Status;

    #[test]
    fn insert_batch_ack_prune() {
        let mut s = ProbeStore::with_connection(Connection::open_in_memory().unwrap()).unwrap();
        let mut rs = vec![
            ProbeResult::new("n1", CheckKind::Tcp, Status::Ok, 1000.0),
            ProbeResult::new("n1", CheckKind::Egress, Status::Fail, 200_000.0),
        ];
        s.insert(&mut rs).unwrap();
        assert_eq!(rs[1].id, Some(2));
        let b = s.pending("line3", 10, vec![], 200_001.0).unwrap().unwrap();
        assert_eq!((b.first_id, b.last_id, b.results.len()), (1, 2, 2));
        s.ack(1).unwrap();
        s.ack(0).unwrap(); // 游标不回退
        assert_eq!(s.pending("line3", 10, vec![], 0.0).unwrap().unwrap().results.len(), 1);
        assert_eq!(s.prune(200_001.0).unwrap(), 1);
        assert_eq!(s.latest("n1", CheckKind::Egress).unwrap().unwrap().status, Status::Fail);
        let json = serde_json::to_value(&b).unwrap();
        assert_eq!(json["schema"], "fleet.probe.v1");
        assert_eq!(json["results"][0]["check"], "tcp");
    }
}
