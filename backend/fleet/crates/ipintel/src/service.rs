//! IpIntel：库 + 状态库 + 检测器。所有写入都是同步的 SQLite 事务，调用方在 spawn_blocking 里调用。
use crate::lookup::{self, Dbs, Intel, MemCache};
use crate::store;
/// 热路径（每条访问事件）用缓存的预编译语句
trait CachedExt {
    fn prepare_cached_execute<P: rusqlite::Params>(&self, sql: &str, p: P) -> rusqlite::Result<usize>;
    fn prepare_cached_query_row<T, P: rusqlite::Params, F: FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>>(&self, sql: &str, p: P, f: F) -> rusqlite::Result<T>;
}
impl CachedExt for rusqlite::Connection {
    fn prepare_cached_execute<P: rusqlite::Params>(&self, sql: &str, p: P) -> rusqlite::Result<usize> {
        self.prepare_cached(sql)?.execute(p)
    }
    fn prepare_cached_query_row<T, P: rusqlite::Params, F: FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>>(&self, sql: &str, p: P, f: F) -> rusqlite::Result<T> {
        self.prepare_cached(sql)?.query_row(p, f)
    }
}

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

// ─── 配置 ──────────────────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub enabled: bool,
    /// 四个离线库所在目录（fetch-dbs.sh 的 DEST）
    pub db_dir: PathBuf,
    /// 状态库（本 crate 独占）
    pub state_db: PathBuf,
    /// 情报缓存有效期
    pub cache_ttl_days: u32,
    /// 节点学习期：节点第一条访问事件之后这么久内只学习、不报“新运营商 / 新城市”（按事件时间算，回填历史也成立）
    pub learn_s: f64,
    /// 一次性异常（新运营商 / 新城市 / 机房来源）打开后多久自动关闭（告警随之结束，不发恢复通知）
    pub alert_ttl_s: f64,
    /// 一号多地：窗口。学习期内只记录；学习期内同一标识已经用过的城市组合算既有用法
    pub shared_window_s: f64,
    /// 一号多地：两地相距至少多少公里（两边都有坐标时）；缺坐标时退回“省不同”
    pub shared_min_km: f64,
    /// 不参与一号多地判定的客户端标识（例如多人共用的探测账号）
    pub shared_ignore_clients: Vec<String>,
    /// 内部标识前缀（线路机互联、中转、探测账号）：一号多地不看这些
    pub shared_internal_prefixes: Vec<String>,
    /// 一号多地的“既有用法”回看天数：这个标识在此之前（窗口外）已经从这两个城市都接入过就不报
    pub shared_known_days: f64,
    /// 出口 IP 变更：新地址要连续被测到几次才确认（egress / ingress 探测的不同 measured_at 各算一次）
    pub egress_confirm: u32,
    /// 小时分布与来源明细保留天数
    pub retention_days: u32,
    /// 额外的内部地址（线路机、中控、运维出口）；落地节点和线路机的出口 IP 会从 health 自动加入
    pub internal_ips: Vec<String>,
    /// 各规则的级别：info / warning / critical
    pub severity: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        let mut sev = BTreeMap::new();
        sev.insert("ipintel_new_isp".into(), "warning".into());
        sev.insert("ipintel_new_city".into(), "warning".into());
        // 团队现场常有多条宽带 / 5G 同时出网：一号多地只进日报（info），不单独推送
        sev.insert("ipintel_shared_identity".into(), "info".into());
        sev.insert("ipintel_datacenter_source".into(), "warning".into());
        sev.insert("ipintel_egress_ip_changed".into(), "critical".into());
        Config {
            enabled: true,
            db_dir: "/var/lib/fleetd/ipintel".into(),
            state_db: "/var/lib/fleetd/ipintel.db".into(),
            cache_ttl_days: 30,
            learn_s: 7.0 * 86400.0,
            alert_ttl_s: 86400.0,
            shared_window_s: 600.0,
            shared_min_km: 150.0,
            shared_ignore_clients: vec![],
            shared_internal_prefixes: vec!["relay-".into(), "zhongzhuan".into(), "probe".into(), "fleet-".into(), "komari".into(), "node-00".into()],
            shared_known_days: 14.0,
            egress_confirm: 2,
            retention_days: 35,
            internal_ips: vec![],
            severity: sev,
        }
    }
}

// ─── 输入 ──────────────────────────────────────────────────────────────────────────────────────────

/// 一条访问事件（旧系统 /activity 行，或以后 relayd 访问日志）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceEvent {
    /// 去重用的自然键（旧 /activity 的 id；relayd 用 file_gen:offset）
    pub id: Option<String>,
    pub ts: f64,
    /// 节点键：资产 uuid（与 nodes.node 相同）
    pub node: String,
    pub relay: Option<String>,
    pub team: Option<String>,
    pub source_ip: IpAddr,
    /// 客户端标识（3x-ui email / VLESS uuid）
    pub client: Option<String>,
}

/// 一次出口 IP 观测（旧 /health 的 egress / ingress 检查的 actual_ip）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EgressObs {
    pub node: String,
    /// 探测时刻 measured_at（同一时刻重复读到只算一次）
    pub ts: f64,
    pub ip: IpAddr,
    /// egress | ingress
    pub via: String,
    pub expected: Option<IpAddr>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeMeta {
    pub node: String,
    pub short: String,
    pub name: String,
    pub relay: Option<String>,
    pub team: Option<String>,
    pub egress_ip: Option<IpAddr>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct IngestReport {
    pub events: usize,
    pub duplicates: usize,
    pub new_pairs: usize,
    pub anomalies_opened: Vec<i64>,
    pub anomalies_bumped: usize,
}

// ─── 告警输出（与 fleetd alerts::Observation 字段一一对应，见 WIRING.md） ──────────────────────────────

#[derive(Clone, Debug, Serialize)]
pub struct AlertObs {
    pub rule: String,
    pub labels: BTreeMap<String, String>,
    /// info | warning | critical
    pub severity: String,
    pub summary: String,
    pub evidence: Value,
    pub active_since: Option<f64>,
    pub for_s: f64,
    pub notify_resolved: bool,
}

pub const RULES: &[&str] = &[
    "ipintel_new_isp",
    "ipintel_new_city",
    "ipintel_shared_identity",
    "ipintel_datacenter_source",
    "ipintel_egress_ip_changed",
];

pub fn rule_title(rule: &str) -> Option<&'static str> {
    Some(match rule {
        "ipintel_new_isp" => "来源出现新运营商",
        "ipintel_new_city" => "来源出现新城市",
        "ipintel_shared_identity" => "同一账号多地同时接入",
        "ipintel_datacenter_source" => "机房 / VPN 来源",
        "ipintel_egress_ip_changed" => "出口 IP 变更",
        _ => return None,
    })
}

fn kind_of(rule: &str) -> &str {
    rule.strip_prefix("ipintel_").unwrap_or(rule)
}

// ─── 服务 ──────────────────────────────────────────────────────────────────────────────────────────

pub struct IpIntel {
    pub cfg: Config,
    dbs: RwLock<Arc<Dbs>>,
    conn: Mutex<Connection>,
    mem: Mutex<MemCache>,
    internal: RwLock<HashMap<IpAddr, String>>,
    seen_ids: Mutex<(HashSet<String>, VecDeque<String>)>,
    pub loaded_at: RwLock<f64>,
}

const SEEN_CAP: usize = 50_000;

fn now_unix() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn fam(ip: &IpAddr) -> i64 {
    if ip.is_ipv4() {
        4
    } else {
        6
    }
}

impl IpIntel {
    pub fn open(cfg: Config) -> Result<Arc<IpIntel>, String> {
        let conn = store::open(&cfg.state_db)?;
        Self::with_conn(cfg, conn)
    }

    /// 测试用：给定连接（例如内存库）。
    pub fn with_conn(cfg: Config, conn: Connection) -> Result<Arc<IpIntel>, String> {
        store::init(&conn)?;
        let dbs = Dbs::open(&cfg.db_dir);
        for e in &dbs.errors {
            tracing::warn!(error = %e, "ipintel: database not loaded");
        }
        let mut internal = HashMap::new();
        for s in &cfg.internal_ips {
            if let Ok(ip) = s.parse::<IpAddr>() {
                internal.insert(lookup::canonical(ip), "内部地址".to_string());
            }
        }
        let me = IpIntel {
            dbs: RwLock::new(Arc::new(dbs)),
            conn: Mutex::new(conn),
            mem: Mutex::new(MemCache::new(20_000)),
            internal: RwLock::new(internal),
            seen_ids: Mutex::new((HashSet::new(), VecDeque::new())),
            loaded_at: RwLock::new(now_unix()),
            cfg,
        };
        // 节点出口 IP 也是内部地址
        {
            let c = me.conn.lock().unwrap();
            let mut st = c.prepare("SELECT egress_ip, COALESCE(short, name, node) FROM nodes WHERE egress_ip IS NOT NULL").map_err(|e| e.to_string())?;
            let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map_err(|e| e.to_string())?;
            let mut m = me.internal.write().unwrap();
            for (ip, who) in rows.flatten() {
                if let Ok(ip) = ip.parse::<IpAddr>() {
                    m.entry(lookup::canonical(ip)).or_insert(who);
                }
            }
        }
        Ok(Arc::new(me))
    }

    pub fn dbs(&self) -> Arc<Dbs> {
        self.dbs.read().unwrap().clone()
    }

    /// 重新打开库目录（月度更新后调用）。只有新库至少加载成功一个时才替换。
    pub fn reload(&self) -> Result<String, String> {
        let d = Dbs::open(&self.cfg.db_dir);
        if d.loaded() == 0 {
            return Err(format!("没有可用的库：{}", d.errors.join("；")));
        }
        let v = d.version.clone();
        *self.dbs.write().unwrap() = Arc::new(d);
        self.mem.lock().unwrap().clear();
        *self.loaded_at.write().unwrap() = now_unix();
        Ok(v)
    }

    /// 库文件的 mtime 变了就 reload（后台每 10 min 调一次）。
    pub fn reload_if_changed(&self) -> Option<Result<String, String>> {
        let cur = self.dbs();
        let changed = cur.files.iter().any(|f| {
            std::fs::metadata(&f.path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64 != f.mtime)
                .unwrap_or(false)
        }) || cur.loaded() < 5 && Dbs::open(&self.cfg.db_dir).loaded() > cur.loaded();
        if changed {
            Some(self.reload())
        } else {
            None
        }
    }

    pub fn set_internal(&self, ip: IpAddr, who: &str) {
        self.internal.write().unwrap().insert(lookup::canonical(ip), who.to_string());
    }

    fn internal_of(&self, ip: &IpAddr) -> Option<String> {
        self.internal.read().unwrap().get(&lookup::canonical(*ip)).cloned()
    }

    /// 查情报：进程内缓存 → SQLite 缓存（TTL + 库版本）→ 现查并写回。
    pub fn intel(&self, ip: IpAddr, now: f64) -> Arc<Intel> {
        let ip = lookup::canonical(ip);
        let internal = self.internal_of(&ip);
        if let Some(x) = self.mem.lock().unwrap().get(&ip) {
            if x.internal == internal {
                return x;
            }
        }
        let c = self.conn.lock().unwrap();
        self.intel_with(&c, ip, now)
    }

    fn intel_with(&self, c: &Connection, ip: IpAddr, now: f64) -> Arc<Intel> {
        let ip = lookup::canonical(ip);
        let internal = self.internal_of(&ip);
        if let Some(x) = self.mem.lock().unwrap().get(&ip) {
            if x.internal == internal {
                return x;
            }
        }
        let dbs = self.dbs();
        let ttl = self.cfg.cache_ttl_days as f64 * 86400.0;
        let cached: Option<Intel> = c
            .prepare_cached_query_row("SELECT intel FROM ip_intel WHERE ip=?1 AND db_version=?2 AND fetched_at>?3", params![ip.to_string(), dbs.version, now - ttl], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .filter(|x: &Intel| x.internal == internal);
        let x = match cached {
            Some(x) => x,
            None => {
                let x = dbs.lookup(ip, internal.as_deref());
                let _ = c.prepare_cached_execute(
                    "INSERT INTO ip_intel(ip,intel,db_version,fetched_at) VALUES(?1,?2,?3,?4)
                     ON CONFLICT(ip) DO UPDATE SET intel=excluded.intel, db_version=excluded.db_version, fetched_at=excluded.fetched_at",
                    params![ip.to_string(), serde_json::to_string(&x).unwrap(), dbs.version, now],
                );
                x
            }
        };
        let a = Arc::new(x);
        self.mem.lock().unwrap().put(ip, a.clone());
        a
    }

    // ─── 节点目录 ──────────────────────────────────────────────────────────────────────────────────

    pub fn upsert_nodes<I: IntoIterator<Item = NodeMeta>>(&self, nodes: I, now: f64) -> Result<usize, String> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction().map_err(|e| e.to_string())?;
        let mut n = 0;
        for m in nodes {
            tx.execute(
                "INSERT INTO nodes(node,short,name,relay,team,egress_ip,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7)
                 ON CONFLICT(node) DO UPDATE SET short=excluded.short, name=excluded.name, relay=COALESCE(excluded.relay, relay),
                   team=COALESCE(excluded.team, team), egress_ip=COALESCE(excluded.egress_ip, egress_ip), updated_at=excluded.updated_at",
                params![m.node, m.short, m.name, m.relay, m.team, m.egress_ip.map(|i| i.to_string()), now],
            )
            .map_err(|e| e.to_string())?;
            if let Some(ip) = m.egress_ip {
                self.set_internal(ip, &m.short);
            }
            n += 1;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(n)
    }

    // ─── 来源事件 ──────────────────────────────────────────────────────────────────────────────────

    /// 吃一批访问事件（任意顺序；内部按时间排序、按 id 去重），更新画像并运行来源类检测器。
    pub fn ingest_sources<I: IntoIterator<Item = SourceEvent>>(&self, events: I, now: f64) -> Result<IngestReport, String> {
        let mut evs: Vec<SourceEvent> = events.into_iter().collect();
        evs.sort_by(|a, b| a.ts.partial_cmp(&b.ts).unwrap_or(std::cmp::Ordering::Equal));
        let mut rep = IngestReport::default();
        {
            let mut seen = self.seen_ids.lock().unwrap();
            evs.retain(|e| {
                let Some(id) = &e.id else { return true };
                if seen.0.contains(id) {
                    rep.duplicates += 1;
                    return false;
                }
                seen.0.insert(id.clone());
                seen.1.push_back(id.clone());
                if seen.1.len() > SEEN_CAP {
                    if let Some(old) = seen.1.pop_front() {
                        seen.0.remove(&old);
                    }
                }
                true
            });
        }
        // 分块提交（每块 300 条）：块之间放开连接锁，查询（来源画像、IP 详情）最多等一个块
        for chunk in evs.chunks(300) {
            let mut c = self.conn.lock().unwrap();
            let tx = c.transaction().map_err(|e| e.to_string())?;
            for e in chunk {
                self.ingest_one(&tx, e, now, &mut rep).map_err(|er| format!("ingest {}: {er}", e.source_ip))?;
                rep.events += 1;
            }
            tx.commit().map_err(|e| e.to_string())?;
            drop(c);
            std::thread::yield_now();
        }
        Ok(rep)
    }

    /// 批量查 IP 画像（缓存优先）：{ip: {isp, asn, as_org, country, region, city, place, type, type_word, …, internal}}
    pub fn lookup_many(&self, ips: &[IpAddr], now: f64) -> Value {
        let mut out = serde_json::Map::new();
        for ip in ips {
            let x = self.intel(*ip, now);
            let mut b = brief(&x);
            b["internal"] = json!(x.internal);
            if let Some(name) = &x.internal {
                // 内部地址（线路机 / 中控）：flags 带 internal，relay 给出名字
                if let Some(f) = b["flags"].as_array_mut() {
                    if !f.iter().any(|v| v == "internal") {
                        f.push(json!("internal"));
                    }
                }
                b["relay"] = json!(name);
            }
            out.insert(ip.to_string(), b);
        }
        Value::Object(out)
    }

    fn ingest_one(&self, tx: &Transaction, e: &SourceEvent, now: f64, rep: &mut IngestReport) -> rusqlite::Result<()> {
        let ip = lookup::canonical(e.source_ip);
        let ips = ip.to_string();
        let x = self.intel_with(tx, ip, now);
        let skip_alerts = x.internal.is_some() || x.flags.iter().any(|f| f == "private");
        // 节点（首条事件时刻 = 学习期起点，按事件时间）
        tx.prepare_cached_execute(
            "INSERT INTO nodes(node,relay,team,first_event,updated_at) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(node) DO UPDATE SET first_event=MIN(COALESCE(first_event, excluded.first_event), excluded.first_event),
               relay=COALESCE(relay, excluded.relay), team=COALESCE(team, excluded.team)",
            params![e.node, e.relay, e.team, e.ts, now],
        )?;
        let (first_event, nshort, nname, nrelay, nteam): (f64, Option<String>, Option<String>, Option<String>, Option<String>) = tx.prepare_cached_query_row(
            "SELECT first_event, short, name, relay, team FROM nodes WHERE node=?1",
            [&e.node],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        let learned = e.ts >= first_event + self.cfg.learn_s;
        let node_ctx = NodeCtx {
            node: e.node.clone(),
            short: nshort.clone().unwrap_or_else(|| e.node.chars().take(8).collect()),
            name: nname.clone().or(nshort.clone()).unwrap_or_else(|| e.node.chars().take(8).collect()),
            relay: nrelay.or(e.relay.clone()),
            team: nteam.or(e.team.clone()),
        };
        // 节点 × 来源
        let pair_new = tx
            .prepare_cached_query_row("SELECT 1 FROM node_sources WHERE node=?1 AND ip=?2", params![e.node, ips], |_| Ok(()))
            .optional()?
            .is_none();
        tx.prepare_cached_execute(
            "INSERT INTO node_sources(node,ip,first_seen,last_seen,count) VALUES(?1,?2,?3,?3,1)
             ON CONFLICT(node,ip) DO UPDATE SET first_seen=MIN(first_seen, excluded.first_seen), last_seen=MAX(last_seen, excluded.last_seen), count=count+1",
            params![e.node, ips, e.ts],
        )?;
        let hour = (e.ts / 3600.0).floor() as i64;
        tx.prepare_cached_execute(
            "INSERT INTO node_source_hours(node,hour,ip,count) VALUES(?1,?2,?3,1) ON CONFLICT(node,hour,ip) DO UPDATE SET count=count+1",
            params![e.node, hour, ips],
        )?;
        if pair_new {
            rep.new_pairs += 1;
        }
        // 维度
        for (dim, val) in lookup::dims(&x) {
            let is_new = tx
                .query_row("SELECT 1 FROM node_dims WHERE node=?1 AND dim=?2 AND value=?3", params![e.node, dim, val], |_| Ok(()))
                .optional()?
                .is_none();
            tx.prepare_cached_execute(
                "INSERT INTO node_dims(node,dim,value,first_seen,last_seen,count,first_ip) VALUES(?1,?2,?3,?4,?4,1,?5)
                 ON CONFLICT(node,dim,value) DO UPDATE SET first_seen=MIN(first_seen, excluded.first_seen), last_seen=MAX(last_seen, excluded.last_seen), count=count+1",
                params![e.node, dim, val, e.ts, ips],
            )?;
            if is_new && learned && !skip_alerts && (dim == "isp" || dim == "city") {
                let (rule, summary, subject) = if dim == "isp" {
                    (
                        "ipintel_new_isp",
                        format!("{} 出现新运营商：{}（{}，来源 {}）", node_ctx.name, x.isp, place_text(&x), ips),
                        x.isp.clone(),
                    )
                } else {
                    ("ipintel_new_city", format!("{} 出现新城市：{}（{}，来源 {}）", node_ctx.name, place_text(&x), x.isp, ips), val.clone())
                };
                let ev = json!({"source_ip": ips, "client": e.client, "dim": dim, "value": val, "intel": brief(&x),
                    "node_first_event": first_event, "learned_since": first_event + self.cfg.learn_s});
                self.open_anomaly(tx, rule, &node_ctx, &subject, &format!("{}|{}|{}", rule, e.node, val), summary, ev, e.ts, false, false, rep)?;
            }
        }
        // 机房 / VPN 来源（每个节点 × 来源只报一次）
        if pair_new && !skip_alerts && matches!(x.conn_type, crate::classify::ConnType::Datacenter | crate::classify::ConnType::Vpn) {
            let rule = "ipintel_datacenter_source";
            let summary = format!(
                "{} 有{}来源 {}（{}{}，{}）",
                node_ctx.name,
                x.type_word,
                ips,
                x.asn.map(|a| format!("AS{a} ")).unwrap_or_default(),
                x.as_org.clone().unwrap_or_else(|| x.isp.clone()),
                x.type_reason
            );
            let ev = json!({"source_ip": ips, "client": e.client, "intel": brief(&x)});
            self.open_anomaly(tx, rule, &node_ctx, &ips, &format!("{}|{}|{}", rule, e.node, ips), summary, ev, e.ts, false, false, rep)?;
        }
        // 客户端 × 来源 + 一号多地
        if let (Some(client), Some(relay)) = (e.client.as_deref().filter(|s| !s.is_empty()), e.relay.as_deref().or(node_ctx.relay.as_deref())) {
            tx.prepare_cached_execute(
                "INSERT INTO client_sources(relay,client,ip,first_seen,last_seen,count,node,city) VALUES(?1,?2,?3,?4,?4,1,?5,?6)
                 ON CONFLICT(relay,client,ip) DO UPDATE SET first_seen=MIN(first_seen, excluded.first_seen),
                   last_seen=MAX(last_seen, excluded.last_seen), count=count+1, node=excluded.node, city=excluded.city",
                params![relay, client, ips, e.ts, e.node, lookup::city_dim(&x)],
            )?;
            if learned && !skip_alerts && !self.shared_exempt(client) {
                self.check_shared(tx, relay, client, e, &x, &node_ctx, first_event + self.cfg.learn_s, now, rep)?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    /// 一号多地：同一（线路机, 客户端标识）在窗口内从两个相距足够远的城市接入。
    /// 基线：学习期结束（cutoff）之前这个标识已经从这两个城市都接入过 → 视为既有用法，不报。
    /// 内部标识（线路机互联、中转、探测）与显式忽略的标识不参与一号多地判定
    pub fn shared_exempt(&self, client: &str) -> bool {
        self.cfg.shared_ignore_clients.iter().any(|c| c == client) || self.cfg.shared_internal_prefixes.iter().any(|p| !p.is_empty() && client.starts_with(p.as_str()))
    }

    fn check_shared(
        &self,
        tx: &Transaction,
        relay: &str,
        client: &str,
        e: &SourceEvent,
        x: &Intel,
        ctx: &NodeCtx,
        cutoff: f64,
        now: f64,
        rep: &mut IngestReport,
    ) -> rusqlite::Result<()> {
        let w = self.cfg.shared_window_s;
        let ips = x.ip.clone();
        let others: Vec<(String, f64, Option<String>)> = {
            let mut st = tx.prepare_cached(
                "SELECT ip, last_seen, node FROM client_sources WHERE relay=?1 AND client=?2 AND ip<>?3 AND last_seen>=?4 AND last_seen<=?5",
            )?;
            let rows = st.query_map(params![relay, client, ips, e.ts - w, e.ts + w], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        let mut hits = Vec::new();
        for (oip, olast, onode) in others {
            let Ok(o) = oip.parse::<IpAddr>() else { continue };
            let y = self.intel_with(tx, o, now);
            if y.internal.is_some() || y.flags.iter().any(|f| f == "private") {
                continue;
            }
            let (Some(a), Some(b)) = (lookup::city_dim(x), lookup::city_dim(&y)) else { continue };
            if a == b {
                continue;
            }
            let km = match (x.lat.zip(x.lon), y.lat.zip(y.lon)) {
                (Some(p), Some(q)) => Some(lookup::distance_km(p, q)),
                _ => None,
            };
            // 既有用法：学习期内、或最近 N 天窗口之外，这个标识已经从这两个城市都接入过
            let known_since = e.ts - self.cfg.shared_known_days * 86400.0;
            let known: i64 = tx.prepare_cached_query_row(
                "SELECT COUNT(DISTINCT city) FROM client_sources WHERE relay=?1 AND client=?2 AND city IN (?4, ?5)
                   AND (first_seen<?3 OR (last_seen>=?6 AND first_seen<?7))",
                params![relay, client, cutoff, a, b, known_since, e.ts - w],
                |r| r.get(0),
            )?;
            if known >= 2 {
                continue;
            }
            let far = match km {
                Some(k) => k >= self.cfg.shared_min_km,
                None => x.region != y.region || x.country != y.country,
            };
            if far {
                hits.push(json!({"ip": oip, "last_seen": olast, "node": onode, "km": km.map(|k| k.round()), "intel": brief(&y)}));
            }
        }
        if hits.is_empty() {
            return Ok(());
        }
        let rule = "ipintel_shared_identity";
        let max_km = hits.iter().filter_map(|h| h["km"].as_f64()).fold(None, |m: Option<f64>, k| Some(m.map_or(k, |m| m.max(k))));
        let first = &hits[0];
        let summary = format!(
            "客户端 {}（{}）{} 分钟内从{}两地接入：{} {} / {} {}",
            client,
            relay,
            (w / 60.0).round(),
            max_km.map(|k| format!("相距 {k:.0} km 的")).unwrap_or_default(),
            place_text(x),
            x.isp,
            first["intel"]["place"].as_str().unwrap_or(""),
            first["intel"]["isp"].as_str().unwrap_or("")
        );
        let ev = json!({"relay": relay, "client": client, "window_s": w, "source": {"ip": ips, "ts": e.ts, "node": e.node, "intel": brief(x)},
            "others": hits, "max_km": max_km.map(|k| k.round())});
        let mut ictx = ctx.clone();
        ictx.relay = Some(relay.to_string());
        self.open_anomaly(tx, rule, &ictx, client, &format!("{}|{}|{}", rule, relay, client), summary, ev, e.ts, false, true, rep)
    }

    #[allow(clippy::too_many_arguments)]
    fn open_anomaly(
        &self,
        tx: &Transaction,
        rule: &str,
        ctx: &NodeCtx,
        subject: &str,
        fp: &str,
        summary: String,
        evidence: Value,
        ts: f64,
        sticky: bool,
        bump: bool,
        rep: &mut IngestReport,
    ) -> rusqlite::Result<()> {
        let open: Option<i64> = tx.query_row("SELECT id FROM anomalies WHERE fingerprint=?1 AND status='open'", [fp], |r| r.get(0)).optional()?;
        if let Some(id) = open {
            if bump {
                tx.execute(
                    "UPDATE anomalies SET last_at=MAX(last_at, ?2), count=count+1, summary=?3, evidence=?4 WHERE id=?1",
                    params![id, ts, summary, evidence.to_string()],
                )?;
            } else {
                tx.execute("UPDATE anomalies SET count=count+1 WHERE id=?1", [id])?;
            }
            rep.anomalies_bumped += 1;
            return Ok(());
        }
        let sev = self.cfg.severity.get(rule).cloned().unwrap_or_else(|| "warning".into());
        tx.execute(
            "INSERT INTO anomalies(kind,severity,fingerprint,status,sticky,node,node_short,node_name,relay,team,subject,summary,evidence,first_at,last_at,count)
             VALUES(?1,?2,?3,'open',?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?13,1)",
            params![kind_of(rule), sev, fp, sticky as i64, ctx.node, ctx.short, ctx.name, ctx.relay, ctx.team, subject, summary, evidence.to_string(), ts],
        )?;
        rep.anomalies_opened.push(tx.last_insert_rowid());
        Ok(())
    }

    // ─── 出口 IP ───────────────────────────────────────────────────────────────────────────────────

    pub fn ingest_egress<I: IntoIterator<Item = EgressObs>>(&self, obs: I, now: f64) -> Result<IngestReport, String> {
        let mut v: Vec<EgressObs> = obs.into_iter().collect();
        v.sort_by(|a, b| a.ts.partial_cmp(&b.ts).unwrap_or(std::cmp::Ordering::Equal));
        let mut rep = IngestReport::default();
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction().map_err(|e| e.to_string())?;
        for o in &v {
            match self.egress_one(&tx, o, now, &mut rep) {
                Ok(true) => rep.events += 1,
                Ok(false) => rep.duplicates += 1,
                Err(e) => return Err(format!("egress {}: {e}", o.node)),
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(rep)
    }

    fn egress_one(&self, tx: &Transaction, o: &EgressObs, now: f64, rep: &mut IngestReport) -> rusqlite::Result<bool> {
        let ip = lookup::canonical(o.ip);
        let f = fam(&ip);
        let ips = ip.to_string();
        type St = (String, f64, Option<String>, i64, Option<f64>, f64);
        let st: Option<St> = tx
            .query_row(
                "SELECT current_ip, current_since, candidate_ip, candidate_n, candidate_since, last_measured FROM egress_state WHERE node=?1 AND family=?2",
                params![o.node, f],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .optional()?;
        // 同一个 measured_at（或更早）重复读到：不重复计数
        if let Some(s) = &st {
            if o.ts <= s.5 {
                return Ok(false);
            }
        }
        tx.execute(
            "INSERT INTO egress_history(node,family,ip,first_seen,last_seen,count,via) VALUES(?1,?2,?3,?4,?4,1,?5)
             ON CONFLICT(node,family,ip) DO UPDATE SET first_seen=MIN(first_seen, excluded.first_seen), last_seen=MAX(last_seen, excluded.last_seen),
               count=count+1, via=excluded.via",
            params![o.node, f, ips, o.ts, o.via],
        )?;
        match st {
            None => {
                tx.execute(
                    "INSERT INTO egress_state(node,family,current_ip,current_since,candidate_n,last_measured) VALUES(?1,?2,?3,?4,0,?4)",
                    params![o.node, f, ips, o.ts],
                )?;
            }
            Some((cur, _since, cand, n, cand_since, _)) => {
                if cur == ips {
                    tx.execute(
                        "UPDATE egress_state SET candidate_ip=NULL, candidate_n=0, candidate_since=NULL, last_measured=?3 WHERE node=?1 AND family=?2",
                        params![o.node, f, o.ts],
                    )?;
                } else {
                    let (n2, cs) = if cand.as_deref() == Some(ips.as_str()) { (n + 1, cand_since.unwrap_or(o.ts)) } else { (1, o.ts) };
                    if n2 >= self.cfg.egress_confirm.max(1) as i64 {
                        let ctx = self.node_ctx(tx, &o.node)?;
                        let old_ip: Option<IpAddr> = cur.parse().ok();
                        let old = old_ip.map(|i| self.intel_with(tx, i, now));
                        let new = self.intel_with(tx, ip, now);
                        let same_asn = old.as_ref().map(|x| x.asn == new.asn).unwrap_or(false);
                        let summary = format!(
                            "{} 出口 IP 变更：{} → {}（{}{}，{}{}）",
                            ctx.name,
                            cur,
                            ips,
                            new.asn.map(|a| format!("AS{a} ")).unwrap_or_default(),
                            new.isp,
                            place_text(&new),
                            if same_asn { "，同一 ASN" } else { "，ASN 也变了" }
                        );
                        let ev = json!({"family": f, "old_ip": cur, "new_ip": ips, "first_seen_new": cs, "confirmations": n2,
                            "via": o.via, "expected_ip": o.expected.map(|e| e.to_string()),
                            "matches_expected": o.expected.map(|e| lookup::canonical(e) == ip),
                            "old": old.as_deref().map(brief), "new": brief(&new), "same_asn": same_asn});
                        let rule = "ipintel_egress_ip_changed";
                        self.open_anomaly(tx, rule, &ctx, &ips, &format!("{}|{}|{}|{}", rule, o.node, cur, ips), summary, ev, cs, true, false, rep)?;
                        tx.execute(
                            "UPDATE egress_state SET current_ip=?3, current_since=?4, candidate_ip=NULL, candidate_n=0, candidate_since=NULL, last_measured=?5
                             WHERE node=?1 AND family=?2",
                            params![o.node, f, ips, cs, o.ts],
                        )?;
                    } else {
                        tx.execute(
                            "UPDATE egress_state SET candidate_ip=?3, candidate_n=?4, candidate_since=?5, last_measured=?6 WHERE node=?1 AND family=?2",
                            params![o.node, f, ips, n2, cs, o.ts],
                        )?;
                    }
                }
            }
        }
        Ok(true)
    }

    fn node_ctx(&self, tx: &Connection, node: &str) -> rusqlite::Result<NodeCtx> {
        type Row = (Option<String>, Option<String>, Option<String>, Option<String>);
        let r: Option<Row> = tx
            .query_row("SELECT short,name,relay,team FROM nodes WHERE node=?1", [node], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .optional()?;
        let (short, name, relay, team) = r.unwrap_or((None, None, None, None));
        let fallback: String = node.chars().take(8).collect();
        Ok(NodeCtx {
            node: node.into(),
            short: short.clone().unwrap_or_else(|| fallback.clone()),
            name: name.or(short).unwrap_or(fallback),
            relay,
            team,
        })
    }

    // ─── 生命周期 ──────────────────────────────────────────────────────────────────────────────────

    /// 定时（每 60 s）：一次性异常到期自动关闭；过期数据清理。
    pub fn maintain(&self, now: f64) -> Result<Value, String> {
        let c = self.conn.lock().unwrap();
        let closed = c
            .execute(
                "UPDATE anomalies SET status='closed', closed_at=?1, closed_by='auto' WHERE status='open' AND sticky=0 AND last_at < ?2",
                params![now, now - self.cfg.alert_ttl_s],
            )
            .map_err(|e| e.to_string())?;
        let ret = self.cfg.retention_days as f64 * 86400.0;
        let hours = c.execute("DELETE FROM node_source_hours WHERE hour < ?1", [((now - ret) / 3600.0) as i64]).map_err(|e| e.to_string())?;
        let cache = c
            .execute("DELETE FROM ip_intel WHERE fetched_at < ?1", [now - self.cfg.cache_ttl_days as f64 * 86400.0])
            .map_err(|e| e.to_string())?;
        let old = c
            .execute("DELETE FROM anomalies WHERE status='closed' AND closed_at < ?1", [now - 180.0 * 86400.0])
            .map_err(|e| e.to_string())?;
        let cs = c.execute("DELETE FROM client_sources WHERE last_seen < ?1", [now - ret]).map_err(|e| e.to_string())?;
        // 一号多地的级别按当前配置（旧行是 warning）
        let sev = self.cfg.severity.get("ipintel_shared_identity").cloned().unwrap_or_else(|| "info".into());
        let _ = c.execute("UPDATE anomalies SET severity=?1 WHERE status='open' AND kind='shared_identity' AND severity<>?1", [&sev]);
        // 一号多地：内部标识、或两地都是既有用法的，自动关闭（规则收紧后清理旧的误报）
        let mut shared_closed = 0usize;
        let open: Vec<(i64, String, Option<String>, String, f64)> = {
            let mut st = c.prepare("SELECT id, subject, relay, evidence, first_at FROM anomalies WHERE status='open' AND kind='shared_identity'").map_err(|e| e.to_string())?;
            let v = st.query_map([], |r| Ok((r.get(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default(), r.get(2)?, r.get(3)?, r.get(4)?)))
                .map_err(|e| e.to_string())?.flatten().collect();
            v
        };
        for (id, client, relay, ev, first_at) in open {
            let ev: Value = serde_json::from_str(&ev).unwrap_or(Value::Null);
            let why = if self.shared_exempt(&client) {
                Some("内部标识（线路机互联 / 中转 / 探测），不参与一号多地")
            } else {
                // 与 lookup::city_dim 同一口径（brief 里有 country/region/city）
                let city = |i: &Value| -> Option<String> {
                    let g = |k: &str| i[k].as_str().map(String::from);
                    if g("city").is_none() && g("region").is_none() {
                        return None;
                    }
                    Some(format!("{}|{}|{}", g("country").unwrap_or_default(), g("region").unwrap_or_default(), g("city").unwrap_or_default()))
                };
                let mut cities: Vec<String> = vec![];
                if let Some(cy) = city(&ev["source"]["intel"]) {
                    cities.push(cy);
                }
                for o in ev["others"].as_array().into_iter().flatten() {
                    if let Some(cy) = city(&o["intel"]) {
                        cities.push(cy);
                    }
                }
                cities.sort();
                cities.dedup();
                let w = ev["window_s"].as_f64().unwrap_or(self.cfg.shared_window_s);
                let known_since = first_at - self.cfg.shared_known_days * 86400.0;
                let known = cities.iter().filter(|cy| {
                    c.query_row(
                        "SELECT 1 FROM client_sources WHERE relay IS ?1 AND client=?2 AND city=?3 AND last_seen>=?4 AND first_seen<?5 LIMIT 1",
                        params![relay, client, cy, known_since, first_at - w],
                        |_| Ok(()),
                    ).is_ok()
                }).count();
                (cities.len() >= 2 && known == cities.len()).then_some("两地在此前 14 天都用过（既有多线路 / 移动网络用法）")
            };
            if let Some(note) = why {
                let _ = c.execute(
                    "UPDATE anomalies SET status='closed', closed_at=?2, closed_by='auto', close_note=?3 WHERE id=?1 AND status='open'",
                    params![id, now, note],
                );
                shared_closed += 1;
            }
        }
        Ok(json!({"auto_closed": closed, "shared_closed": shared_closed, "pruned_hours": hours, "pruned_cache": cache, "pruned_anomalies": old, "pruned_client_sources": cs}))
    }

    /// 人工关闭（出口 IP 变更只能这样结束）。
    pub fn close_anomaly(&self, id: i64, actor: &str, note: Option<&str>, now: f64) -> Result<Value, String> {
        let c = self.conn.lock().unwrap();
        let n = c
            .execute(
                "UPDATE anomalies SET status='closed', closed_at=?2, closed_by=?3, close_note=?4 WHERE id=?1 AND status='open'",
                params![id, now, actor, note],
            )
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("异常不存在或已关闭".into());
        }
        anomaly_by_id(&c, id).map_err(|e| e.to_string())?.ok_or_else(|| "异常不存在".into())
    }

    /// 当前成立的告警条件（open 的异常）。fleetd 每轮评估调一次，scope = RULES。
    pub fn observations(&self, _now: f64) -> Result<Vec<AlertObs>, String> {
        let c = self.conn.lock().unwrap();
        let mut st = c
            .prepare("SELECT id,kind,severity,node_short,node_name,relay,team,subject,summary,evidence,first_at,last_at,count,sticky FROM anomalies WHERE status='open' AND kind NOT IN ('egress_ip_changed','ipintel_egress_ip_changed') ORDER BY id LIMIT 1000")
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, String>(8)?,
                    r.get::<_, String>(9)?,
                    r.get::<_, f64>(10)?,
                    r.get::<_, f64>(11)?,
                    r.get::<_, i64>(12)?,
                    r.get::<_, i64>(13)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            let (id, kind, sev, short, name, relay, team, subject, summary, evidence, first_at, last_at, count, sticky) = row.map_err(|e| e.to_string())?;
            let rule = format!("ipintel_{kind}");
            let mut l = BTreeMap::new();
            if kind == "shared_identity" {
                l.insert("kind".to_string(), "identity".to_string());
                l.insert("client".to_string(), subject.clone().unwrap_or_default());
            } else {
                l.insert("kind".to_string(), "node".to_string());
                if let Some(s) = &short {
                    l.insert("node".to_string(), s.clone());
                }
                if let Some(n) = &name {
                    l.insert("node_name".to_string(), n.clone());
                }
                l.insert("subject".to_string(), subject.clone().unwrap_or_default());
            }
            if let Some(r) = &relay {
                l.insert("relay".to_string(), r.clone());
            }
            if let Some(t) = &team {
                l.insert("team".to_string(), t.clone());
            }
            l.insert("cause".to_string(), rule.clone());
            l.insert("anomaly_id".to_string(), id.to_string());
            let mut ev: Value = serde_json::from_str(&evidence).unwrap_or(Value::Null);
            if let Value::Object(m) = &mut ev {
                m.insert("anomaly_id".into(), json!(id));
                m.insert("first_at".into(), json!(first_at));
                m.insert("last_at".into(), json!(last_at));
                m.insert("count".into(), json!(count));
                m.insert("sticky".into(), json!(sticky != 0));
            }
            out.push(AlertObs {
                rule,
                labels: l,
                severity: sev,
                summary,
                evidence: ev,
                active_since: Some(first_at),
                for_s: 0.0,
                // 一次性异常自动到期不是“恢复”，不发恢复通知；出口变更要人工关闭，关闭时发一条恢复
                notify_resolved: sticky != 0,
            });
        }
        Ok(out)
    }

    pub(crate) fn with_conn_ref<T>(&self, f: impl FnOnce(&Connection) -> T) -> T {
        let c = self.conn.lock().unwrap();
        f(&c)
    }
}

#[derive(Clone, Debug)]
struct NodeCtx {
    node: String,
    short: String,
    name: String,
    relay: Option<String>,
    team: Option<String>,
}

pub fn place_text(x: &Intel) -> String {
    let mut v: Vec<&str> = Vec::new();
    if x.country.as_deref() != Some("CN") {
        if let Some(c) = x.country_name.as_deref().or(x.country.as_deref()) {
            v.push(c);
        }
    }
    if let Some(r) = x.region.as_deref() {
        v.push(r);
    }
    if let Some(c) = x.city.as_deref() {
        if Some(c) != x.region.as_deref() {
            v.push(c);
        }
    }
    if v.is_empty() {
        "地点未知".into()
    } else {
        v.join(" ")
    }
}

/// 告警证据里用的精简情报。
pub fn brief(x: &Intel) -> Value {
    json!({"ip": x.ip, "isp": x.isp, "asn": x.asn, "as_org": x.as_org, "country": x.country, "region": x.region, "city": x.city,
           "place": place_text(x), "type": x.conn_type, "type_word": x.type_word, "type_confidence": x.type_confidence,
           "lat": x.lat, "lon": x.lon, "flags": x.flags})
}

pub(crate) fn anomaly_json(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    let kind: String = r.get("kind")?;
    let ev: String = r.get("evidence")?;
    Ok(json!({
        "id": r.get::<_, i64>("id")?,
        "rule": format!("ipintel_{kind}"),
        "kind": kind,
        "title": rule_title(&format!("ipintel_{kind}")),
        "severity": r.get::<_, String>("severity")?,
        "status": r.get::<_, String>("status")?,
        "sticky": r.get::<_, i64>("sticky")? != 0,
        "node": r.get::<_, Option<String>>("node_short")?,
        "node_uuid": r.get::<_, Option<String>>("node")?,
        "node_name": r.get::<_, Option<String>>("node_name")?,
        "relay": r.get::<_, Option<String>>("relay")?,
        "team": r.get::<_, Option<String>>("team")?,
        "subject": r.get::<_, Option<String>>("subject")?,
        "summary": r.get::<_, String>("summary")?,
        "evidence": serde_json::from_str::<Value>(&ev).unwrap_or(Value::Null),
        "first_at": r.get::<_, f64>("first_at")?,
        "last_at": r.get::<_, f64>("last_at")?,
        "count": r.get::<_, i64>("count")?,
        "closed_at": r.get::<_, Option<f64>>("closed_at")?,
        "closed_by": r.get::<_, Option<String>>("closed_by")?,
        "close_note": r.get::<_, Option<String>>("close_note")?,
    }))
}

pub(crate) fn anomaly_by_id(c: &Connection, id: i64) -> rusqlite::Result<Option<Value>> {
    c.query_row("SELECT * FROM anomalies WHERE id=?1", [id], anomaly_json).optional()
}
