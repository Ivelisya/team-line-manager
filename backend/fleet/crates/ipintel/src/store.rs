//! SQLite 结构（独立文件 ipintel.db，不碰 fleetd 的 control.db）。
//!
//! | 表 | 作用 |
//! |---|---|
//! | `ip_intel` | 情报缓存：ip → JSON，带库版本与写入时刻；TTL 30 天或库换版即重算 |
//! | `nodes` | 节点目录：资产 uuid（主键）、短名 t1-01、全名、线路机、团队、首条访问事件时刻（学习期起点） |
//! | `node_sources` | 每节点每来源 IP：首次 / 最近 / 次数 |
//! | `node_source_hours` | 每节点每来源每小时计数（按窗口算分布用；保留 35 天） |
//! | `node_dims` | 每节点见过的维度值（isp / city / province / country / asn / type）：首次 / 最近 / 次数；“新运营商 / 新城市”据此判定 |
//! | `client_sources` | （线路机, 客户端标识）× 来源 IP：首次 / 最近 / 次数 / 最近节点 / 城市键；“一号多地”据此判定（学习期内见过的城市组合算基线） |
//! | `egress_history` | 每节点每地址族的出口 IP 观测：首次 / 最近 / 次数 / 来源（egress / ingress 探测） |
//! | `egress_state` | 每节点每地址族：当前出口、从何时起、待确认的新地址与确认次数、最后一次测量时刻（去重） |
//! | `anomalies` | 异常事件：open / closed；egress_ip_changed 为 sticky，只能人工关闭 |
use rusqlite::Connection;

pub const SCHEMA_VERSION: i64 = 1;

pub const SCHEMA: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA foreign_keys=OFF;
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS ip_intel(
  ip TEXT PRIMARY KEY,
  intel TEXT NOT NULL,
  db_version TEXT NOT NULL,
  fetched_at REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS ip_intel_fetched ON ip_intel(fetched_at);
CREATE TABLE IF NOT EXISTS nodes(
  node TEXT PRIMARY KEY,
  short TEXT,
  name TEXT,
  relay TEXT,
  team TEXT,
  egress_ip TEXT,
  first_event REAL,
  updated_at REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS nodes_short ON nodes(short);
CREATE TABLE IF NOT EXISTS node_sources(
  node TEXT NOT NULL,
  ip TEXT NOT NULL,
  first_seen REAL NOT NULL,
  last_seen REAL NOT NULL,
  count INTEGER NOT NULL,
  PRIMARY KEY(node, ip)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS node_sources_ip ON node_sources(ip);
CREATE TABLE IF NOT EXISTS node_source_hours(
  node TEXT NOT NULL,
  hour INTEGER NOT NULL,
  ip TEXT NOT NULL,
  count INTEGER NOT NULL,
  PRIMARY KEY(node, hour, ip)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS node_source_hours_hour ON node_source_hours(hour);
CREATE TABLE IF NOT EXISTS node_dims(
  node TEXT NOT NULL,
  dim TEXT NOT NULL,
  value TEXT NOT NULL,
  first_seen REAL NOT NULL,
  last_seen REAL NOT NULL,
  count INTEGER NOT NULL,
  first_ip TEXT,
  PRIMARY KEY(node, dim, value)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS client_sources(
  relay TEXT NOT NULL,
  client TEXT NOT NULL,
  ip TEXT NOT NULL,
  first_seen REAL NOT NULL,
  last_seen REAL NOT NULL,
  count INTEGER NOT NULL,
  node TEXT,
  city TEXT,
  PRIMARY KEY(relay, client, ip)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS egress_history(
  node TEXT NOT NULL,
  family INTEGER NOT NULL,
  ip TEXT NOT NULL,
  first_seen REAL NOT NULL,
  last_seen REAL NOT NULL,
  count INTEGER NOT NULL,
  via TEXT,
  PRIMARY KEY(node, family, ip)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS egress_state(
  node TEXT NOT NULL,
  family INTEGER NOT NULL,
  current_ip TEXT NOT NULL,
  current_since REAL NOT NULL,
  candidate_ip TEXT,
  candidate_n INTEGER NOT NULL DEFAULT 0,
  candidate_since REAL,
  last_measured REAL NOT NULL,
  PRIMARY KEY(node, family)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS anomalies(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL,
  severity TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  status TEXT NOT NULL,
  sticky INTEGER NOT NULL DEFAULT 0,
  node TEXT,
  node_short TEXT,
  node_name TEXT,
  relay TEXT,
  team TEXT,
  subject TEXT,
  summary TEXT NOT NULL,
  evidence TEXT NOT NULL,
  first_at REAL NOT NULL,
  last_at REAL NOT NULL,
  count INTEGER NOT NULL DEFAULT 1,
  closed_at REAL,
  closed_by TEXT,
  close_note TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS anomalies_open_fp ON anomalies(fingerprint) WHERE status='open';
CREATE INDEX IF NOT EXISTS anomalies_status ON anomalies(status, last_at);
CREATE INDEX IF NOT EXISTS anomalies_node ON anomalies(node, last_at);
"#;

pub fn open(path: &std::path::Path) -> Result<Connection, String> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
    }
    let c = Connection::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    c.set_prepared_statement_cache_capacity(64);
    init(&c)?;
    Ok(c)
}

pub fn init(c: &Connection) -> Result<(), String> {
    c.busy_timeout(std::time::Duration::from_secs(5)).map_err(|e| e.to_string())?;
    c.execute_batch(SCHEMA).map_err(|e| format!("ipintel schema: {e}"))?;
    c.execute("INSERT OR REPLACE INTO meta(key,value) VALUES('schema_version', ?1)", [SCHEMA_VERSION.to_string()])
        .map_err(|e| e.to_string())?;
    Ok(())
}
