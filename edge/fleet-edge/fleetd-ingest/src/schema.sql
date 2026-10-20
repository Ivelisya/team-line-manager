-- fleetd-ingest：边缘代理上报（独立数据库文件，缺省 /var/lib/fleetd/ingest.db，WAL + synchronous=FULL）
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;

-- 每台主机的上报令牌：只存 sha256（十六进制）。撤销后保留行（审计），revoked_at 非空即失效。
CREATE TABLE IF NOT EXISTS agent_tokens(
  id INTEGER PRIMARY KEY,
  host_id TEXT NOT NULL,
  name TEXT NOT NULL DEFAULT 'default',
  sha256 TEXT NOT NULL UNIQUE,
  created_at REAL NOT NULL,
  revoked_at REAL
) STRICT;
CREATE INDEX IF NOT EXISTS agent_tokens_host ON agent_tokens(host_id);

-- 去重：同一 (host, boot_id, seq) 只接收一次；保留 7 天
CREATE TABLE IF NOT EXISTS ingest_seen(
  host_id TEXT NOT NULL,
  boot_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  received_at REAL NOT NULL,
  PRIMARY KEY(host_id, boot_id, seq)
) STRICT, WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS ingest_seen_ts ON ingest_seen(received_at);

-- 每台主机最新状态（received_at 用于离线判定；sample 只在更新的样本到达时替换，补传的旧批次不回退）
CREATE TABLE IF NOT EXISTS host_latest(
  host_id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  received_at REAL NOT NULL,
  first_seen REAL NOT NULL,
  sample_ts REAL NOT NULL,
  boot_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  agent TEXT NOT NULL,
  meta TEXT NOT NULL,
  outbox TEXT NOT NULL,
  sample TEXT NOT NULL,
  xray TEXT,
  batches INTEGER NOT NULL,
  samples INTEGER NOT NULL
) STRICT;

-- 最近 15 分钟的原始样本（只存告警要用的字段）
CREATE TABLE IF NOT EXISTS host_recent(
  host_id TEXT NOT NULL,
  ts REAL NOT NULL,
  mem REAL, disk REAL, cpu REAL, steal REAL, iowait REAL,
  PRIMARY KEY(host_id, ts)
) STRICT, WITHOUT ROWID;

-- 1 分钟汇总（按样本时间落分钟，跨批次累加）：每个指标 n（样本数）、s（和）、x（最大值）。
-- 百分比与 load 放大 100 倍存整数；字节/秒、连接数、进程数存整数。保留 90 天。
CREATE TABLE IF NOT EXISTS host_1m(
  host_id TEXT NOT NULL,
  minute INTEGER NOT NULL,
  cpu_n INTEGER NOT NULL DEFAULT 0, cpu_s INTEGER NOT NULL DEFAULT 0, cpu_x INTEGER,
  steal_n INTEGER NOT NULL DEFAULT 0, steal_s INTEGER NOT NULL DEFAULT 0, steal_x INTEGER,
  iowait_n INTEGER NOT NULL DEFAULT 0, iowait_s INTEGER NOT NULL DEFAULT 0, iowait_x INTEGER,
  load1_n INTEGER NOT NULL DEFAULT 0, load1_s INTEGER NOT NULL DEFAULT 0, load1_x INTEGER,
  mem_n INTEGER NOT NULL DEFAULT 0, mem_s INTEGER NOT NULL DEFAULT 0, mem_x INTEGER,
  swap_n INTEGER NOT NULL DEFAULT 0, swap_s INTEGER NOT NULL DEFAULT 0, swap_x INTEGER,
  disk_n INTEGER NOT NULL DEFAULT 0, disk_s INTEGER NOT NULL DEFAULT 0, disk_x INTEGER,
  rx_n INTEGER NOT NULL DEFAULT 0, rx_s INTEGER NOT NULL DEFAULT 0, rx_x INTEGER,
  tx_n INTEGER NOT NULL DEFAULT 0, tx_s INTEGER NOT NULL DEFAULT 0, tx_x INTEGER,
  tcp_n INTEGER NOT NULL DEFAULT 0, tcp_s INTEGER NOT NULL DEFAULT 0, tcp_x INTEGER,
  udp_n INTEGER NOT NULL DEFAULT 0, udp_s INTEGER NOT NULL DEFAULT 0, udp_x INTEGER,
  procs_n INTEGER NOT NULL DEFAULT 0, procs_s INTEGER NOT NULL DEFAULT 0, procs_x INTEGER,
  PRIMARY KEY(host_id, minute)
) STRICT, WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS host_1m_minute ON host_1m(minute);

-- 线路机 Xray：每分钟每条目的增量（只存非零），保留 35 天
CREATE TABLE IF NOT EXISTS xray_1m(
  host_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  tag TEXT NOT NULL,
  minute INTEGER NOT NULL,
  up INTEGER NOT NULL,
  down INTEGER NOT NULL,
  PRIMARY KEY(host_id, kind, tag, minute)
) STRICT, WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS xray_1m_minute ON xray_1m(minute);

-- 线路机 Xray：每条目的累计值（真实来源，relayd 维护）、Xray 原始值、最近速率
CREATE TABLE IF NOT EXISTS xray_latest(
  host_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  tag TEXT NOT NULL,
  cum_up INTEGER NOT NULL,
  cum_down INTEGER NOT NULL,
  raw_up INTEGER NOT NULL,
  raw_down INTEGER NOT NULL,
  rate_up REAL NOT NULL,
  rate_down REAL NOT NULL,
  ts REAL NOT NULL,
  PRIMARY KEY(host_id, kind, tag)
) STRICT, WITHOUT ROWID;

-- relayd 探测结果（fleet.probe.v1，见 fleet/crates/probe/WIRING.md）：按自然键幂等，和批次去重同一事务；保留 14 天
CREATE TABLE IF NOT EXISTS probe_land(
  land_id INTEGER PRIMARY KEY AUTOINCREMENT,
  relay TEXT NOT NULL,
  node TEXT NOT NULL,
  check_kind TEXT NOT NULL,
  measured_at REAL NOT NULL,
  attempt INTEGER NOT NULL DEFAULT 0,
  status TEXT NOT NULL,
  doc TEXT NOT NULL,
  UNIQUE(node, check_kind, measured_at, attempt)
) STRICT;
CREATE INDEX IF NOT EXISTS probe_land_relay ON probe_land(relay, measured_at);
CREATE INDEX IF NOT EXISTS probe_land_at ON probe_land(measured_at);

-- 每台线路机当前在测的节点（批次里的 targets，不含密码）与游标区间
CREATE TABLE IF NOT EXISTS probe_targets(
  relay TEXT PRIMARY KEY,
  targets TEXT NOT NULL,
  last_id INTEGER NOT NULL,
  updated_at REAL NOT NULL
) STRICT;

-- relayd 访问日志事件（fleet.access.v1，API.md §11）：按 (host, id) 幂等；fleetd 的旧库喂数按 rowid 游标消费；保留 3 天
CREATE TABLE IF NOT EXISTS access_raw(
  rid INTEGER PRIMARY KEY AUTOINCREMENT,
  host_id TEXT NOT NULL,
  id TEXT NOT NULL,
  ts REAL NOT NULL,
  doc TEXT NOT NULL,
  received_at REAL NOT NULL,
  UNIQUE(host_id, id)
) STRICT;
CREATE INDEX IF NOT EXISTS access_raw_ts ON access_raw(received_at);
-- 每批的访问日志游标/计数（最新一批），供缺口核对
CREATE TABLE IF NOT EXISTS access_state(
  host_id TEXT PRIMARY KEY,
  source TEXT NOT NULL,
  counters TEXT,
  dropped INTEGER NOT NULL DEFAULT 0,
  updated_at REAL NOT NULL
) STRICT;
-- 过渡期：relayd 原样转发线路机上旧探测的状态文件（komari-relay-probe status.json / path-status.json），供原生旧库快照使用
CREATE TABLE IF NOT EXISTS legacy_probe(
  host_id TEXT PRIMARY KEY,
  probe TEXT,
  probe_mtime REAL,
  paths TEXT,
  paths_mtime REAL,
  updated_at REAL NOT NULL
) STRICT;
