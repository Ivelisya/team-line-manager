//! 来源 IP 画像：运营商、城市、接入方式。离线库在进程内查询（crates/fleet-ipintel，只查本地、绝不外发 IP），
//! 这里负责三件事：
//! 1. 查询缓存：`ip_intel_cache`，按库版本失效；内部地址（线路机 / 节点 / 配置里的运维出口）打 internal 标。
//! 2. 节点来源画像：每 5 分钟从 `access_source_hourly`（访问监控的汇总表）增量维护 `node_source_seen`。
//! 3. 异常：学习期（节点第一条访问记录起 learn_days 天）之后出现新的运营商 / 新的城市 / 机房或 VPN 来源 IP，
//!    写进 incident 表（规则 ipintel_new_isp / ipintel_new_city / ipintel_datacenter_source，warning）并走 notify。
//!    一次性异常最后一次出现 alert_ttl_hours 之后自动恢复，不发恢复通知；alerts.rs 的恢复逻辑不碰 ipintel_ 开头的规则。
//!
//! 库缺失或部分缺失时降级：能查多少查多少，查不到的字段为 null，`/ipintel/status` 说明缺什么；一个库都没有时
//! 来源汇总不推进（等库装好后一次补上）。

use crate::access::{self, floor_hour, SourceRow, Window};
use crate::config::IpintelConfig;
use crate::error::{ApiError, ApiResult};
use crate::notify::{self, Kind, Notice};
use crate::state::{AppState, Event};
use fleet_ipintel::{canonical, is_global, ConnType, IpIntel};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::IpAddr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration as StdDuration, Instant};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

pub const RULE_NEW_ISP: &str = "ipintel_new_isp";
pub const RULE_NEW_CITY: &str = "ipintel_new_city";
pub const RULE_DC_SOURCE: &str = "ipintel_datacenter_source";

/// 节点来源汇总的水位 key
const WM_KEY: &str = "sources_wm";
/// 内部地址表多久重读一次 machine 表
const INTERNAL_TTL: StdDuration = StdDuration::from_secs(60);

fn rule_of_dim(dim: &str) -> Option<&'static str> {
    match dim {
        "isp" => Some(RULE_NEW_ISP),
        "city" => Some(RULE_NEW_CITY),
        "ip" => Some(RULE_DC_SOURCE),
        _ => None,
    }
}

/// 展示用地点：国内「河南省郑州市」，境外「US · Dallas」
pub fn place(country: Option<&str>, province: Option<&str>, city: Option<&str>) -> String {
    let (p, c) = (province.unwrap_or("").trim(), city.unwrap_or("").trim());
    match country {
        Some("CN") => {
            if p.is_empty() || p == c {
                c.to_string()
            } else {
                format!("{p}{c}")
            }
        }
        Some(cc) => {
            let part = if !c.is_empty() { c } else { p };
            if part.is_empty() {
                cc.to_string()
            } else {
                format!("{cc} · {part}")
            }
        }
        None => format!("{p}{c}"),
    }
}

/// 画像摘要：缓存里存的那部分 + 读取时现算的 internal / private。查不到的字段为 null，`/access` 来源条目的 `intel` 就是它。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Brief {
    pub isp: String,
    pub asn: Option<u32>,
    pub as_org: Option<String>,
    pub country: Option<String>,
    pub province: Option<String>,
    pub city: Option<String>,
    /// 展示用地点（见 `place`）
    pub place: String,
    /// residential | mobile | datacenter | vpn | unknown
    pub conn_type: String,
    pub conn_label: &'static str,
    /// high | medium | low
    pub confidence: String,
    pub reason: String,
    /// 我方线路机 / 节点 / 运维出口的名称
    pub internal: Option<String>,
    /// 非公网地址
    pub private: bool,
}

#[derive(sqlx::FromRow)]
struct CacheRow {
    ip: String,
    isp: String,
    asn: Option<i32>,
    as_org: Option<String>,
    country: Option<String>,
    province: Option<String>,
    city: Option<String>,
    conn_type: String,
    confidence: String,
    reason: String,
}

const CACHE_COLS: &str = "host(ip) ip, isp, asn, as_org, country, province, city, conn_type, confidence, reason";

impl CacheRow {
    fn from_intel(ip: IpAddr, i: &fleet_ipintel::Intel) -> CacheRow {
        CacheRow {
            ip: ip.to_string(),
            isp: i.isp.clone(),
            asn: i.asn.map(|a| a as i32),
            as_org: i.as_org.clone(),
            country: i.country.clone(),
            province: i.region.clone(),
            city: i.city.clone(),
            conn_type: i.conn_type.as_str().to_string(),
            confidence: i.type_confidence.clone(),
            reason: i.type_reason.clone(),
        }
    }

    fn brief(self, ip: IpAddr, internal: Option<String>) -> Brief {
        let ct = ConnType::parse(&self.conn_type);
        Brief {
            place: place(self.country.as_deref(), self.province.as_deref(), self.city.as_deref()),
            isp: self.isp,
            asn: self.asn.map(|a| a as u32),
            as_org: self.as_org,
            country: self.country,
            province: self.province,
            city: self.city,
            conn_type: ct.as_str().to_string(),
            conn_label: ct.word(),
            confidence: self.confidence,
            reason: self.reason,
            internal,
            private: !is_global(canonical(ip)),
        }
    }
}

/// 城市维度的值：国家|省|市
fn city_value(b: &Brief) -> Option<String> {
    if b.province.is_none() && b.city.is_none() {
        return None;
    }
    Some(format!("{}|{}|{}", b.country.clone().unwrap_or_default(), b.province.clone().unwrap_or_default(), b.city.clone().unwrap_or_default()))
}

/// 城市维度的值 → 展示名
fn city_label(v: &str) -> String {
    let mut it = v.splitn(3, '|');
    let (c, p, ci) = (it.next().unwrap_or(""), it.next().unwrap_or(""), it.next().unwrap_or(""));
    place((!c.is_empty()).then_some(c), (!p.is_empty()).then_some(p), (!ci.is_empty()).then_some(ci))
}

pub struct Service {
    pub intel: Arc<IpIntel>,
    pub enabled: bool,
    learn: Duration,
    ttl: Duration,
    extra_internal: Vec<IpAddr>,
    internal: RwLock<HashMap<IpAddr, String>>,
    refreshed: Mutex<Option<Instant>>,
}

impl Service {
    /// 同步打开离线库（几百毫秒量级）。库目录不存在不报错。
    pub fn new(cfg: &IpintelConfig) -> Service {
        let intel = if cfg.enabled {
            IpIntel::open(cfg.db_dir.clone(), cfg.overrides.clone())
        } else {
            IpIntel::open(std::path::PathBuf::new(), None)
        };
        let extra_internal = cfg
            .internal_ips
            .iter()
            .filter_map(|s| match s.trim().parse::<IpAddr>() {
                Ok(ip) => Some(canonical(ip)),
                Err(_) => {
                    tracing::warn!("ipintel.internal_ips 里的「{s}」不是有效 IP，已忽略");
                    None
                }
            })
            .collect();
        Service {
            intel: Arc::new(intel),
            enabled: cfg.enabled,
            learn: Duration::days(cfg.learn_days as i64),
            ttl: Duration::hours(cfg.alert_ttl_hours as i64),
            extra_internal,
            internal: RwLock::new(HashMap::new()),
            refreshed: Mutex::new(None),
        }
    }

    pub fn version(&self) -> String {
        self.intel.version()
    }

    /// 地址属于我方哪台机器（线路机 / 节点 / 配置里的运维出口）
    pub fn internal_name(&self, ip: IpAddr) -> Option<String> {
        self.internal.read().unwrap_or_else(|e| e.into_inner()).get(&canonical(ip)).cloned()
    }

    /// 重读 machine 表建内部地址表（线路机和节点的 primary_ip 自动算内部）。
    pub async fn refresh_internal(&self, db: &PgPool) {
        let rows: Result<Vec<(String, String)>, _> = sqlx::query_as(
            "select host(m.primary_ip), coalesce(case when m.kind = 'relay' then m.label
                 else t.name || '-' || case when m.number < 10 then '0' || m.number::text else m.number::text end end, '未命名')
             from machine m left join team t on t.id = m.team_id where m.lifecycle <> 'retired'",
        )
        .fetch_all(db)
        .await;
        let rows = match rows {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("ipintel 读取内部地址失败：{e}");
                return;
            }
        };
        let mut map: HashMap<IpAddr, String> = self.extra_internal.iter().map(|ip| (*ip, "运维 / 自定义内部地址".to_string())).collect();
        for (ip, name) in rows {
            if let Ok(ip) = ip.parse::<IpAddr>() {
                map.insert(canonical(ip), name);
            }
        }
        *self.internal.write().unwrap_or_else(|e| e.into_inner()) = map;
        *self.refreshed.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }

    /// 内部地址表超过一分钟没刷新就刷新（接口用；后台循环自己每轮刷新）
    pub async fn ensure_internal(&self, db: &PgPool) {
        let stale = self.refreshed.lock().unwrap_or_else(|e| e.into_inner()).is_none_or(|t| t.elapsed() > INTERNAL_TTL);
        if stale {
            self.refresh_internal(db).await;
        }
    }

    /// 库文件变了就重载（放到阻塞线程里：读盘、建索引）
    pub async fn reload(&self) -> bool {
        let i = self.intel.clone();
        tokio::task::spawn_blocking(move || i.reload_if_changed()).await.unwrap_or(false)
    }

    fn compute(&self, ip: IpAddr) -> CacheRow {
        CacheRow::from_intel(ip, &self.intel.lookup(ip, None))
    }

    /// 批量取画像：先读缓存（库版本一致才算命中），没命中的现查；`store` 为真时把现查的结果写回缓存。
    /// 一个库都没加载且缓存里也没有的地址不在返回里（查不到）。数据库出错时尽量返回已有的。
    pub async fn enrich(&self, db: &PgPool, ips: &[IpAddr], store: bool) -> HashMap<IpAddr, Brief> {
        let mut out: HashMap<IpAddr, Brief> = HashMap::new();
        let uniq: Vec<IpAddr> = ips.iter().copied().collect::<HashSet<_>>().into_iter().collect();
        let version = self.version();
        let mut missing: Vec<IpAddr> = vec![];
        for chunk in uniq.chunks(1000) {
            let keys: Vec<String> = chunk.iter().map(|i| i.to_string()).collect();
            let sql = format!("select {CACHE_COLS} from ip_intel_cache where ip = any($1::text[]::inet[]) and db_version = $2");
            let rows: Vec<CacheRow> = match sqlx::query_as(&sql).bind(&keys).bind(&version).fetch_all(db).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("ipintel 读取缓存失败：{e}");
                    vec![]
                }
            };
            let mut hit: HashSet<IpAddr> = HashSet::new();
            for r in rows {
                if let Ok(ip) = r.ip.parse::<IpAddr>() {
                    hit.insert(ip);
                    out.insert(ip, r.brief(ip, self.internal_name(ip)));
                }
            }
            missing.extend(chunk.iter().filter(|i| !hit.contains(i)));
        }
        if missing.is_empty() || self.intel.loaded() == 0 {
            return out;
        }
        let fresh: Vec<(IpAddr, CacheRow)> = missing.into_iter().map(|ip| (ip, self.compute(ip))).collect();
        if store {
            for chunk in fresh.chunks(1000) {
                if let Err(e) = store_rows(db, chunk.iter().map(|(_, r)| r), &version).await {
                    tracing::warn!("ipintel 写缓存失败：{e}");
                    break;
                }
            }
        }
        for (ip, row) in fresh {
            let internal = self.internal_name(ip);
            out.insert(ip, row.brief(ip, internal));
        }
        out
    }
}

async fn store_rows<'a>(db: &PgPool, rows: impl Iterator<Item = &'a CacheRow>, version: &str) -> sqlx::Result<()> {
    let rows: Vec<&CacheRow> = rows.collect();
    if rows.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "insert into ip_intel_cache (ip, isp, asn, as_org, country, province, city, conn_type, confidence, reason, db_version, updated_at)
         select u.ip::inet, u.isp, u.asn, u.as_org, u.country, u.province, u.city, u.conn_type, u.confidence, u.reason, $11, now()
         from unnest($1::text[], $2::text[], $3::int[], $4::text[], $5::text[], $6::text[], $7::text[], $8::text[], $9::text[], $10::text[])
              as u(ip, isp, asn, as_org, country, province, city, conn_type, confidence, reason)
         on conflict (ip) do update set isp = excluded.isp, asn = excluded.asn, as_org = excluded.as_org, country = excluded.country,
             province = excluded.province, city = excluded.city, conn_type = excluded.conn_type, confidence = excluded.confidence,
             reason = excluded.reason, db_version = excluded.db_version, updated_at = now()",
    )
    .bind(rows.iter().map(|r| r.ip.clone()).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.isp.clone()).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.asn).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.as_org.clone()).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.country.clone()).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.province.clone()).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.city.clone()).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.conn_type.clone()).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.confidence.clone()).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.reason.clone()).collect::<Vec<_>>())
    .bind(version)
    .execute(db)
    .await?;
    Ok(())
}

// ── 访问监控的来源条目带画像 ─────────────────────────────────────────────────

/// 给来源条目填 `intel`（查不到为 null）。尽力而为：出错只是没有画像。
pub async fn attach(st: &AppState, rows: &mut [SourceRow]) {
    if rows.is_empty() {
        return;
    }
    st.ipintel.ensure_internal(&st.db).await;
    let ips: Vec<IpAddr> = rows.iter().filter_map(|r| r.src_ip.parse().ok()).collect();
    let map = st.ipintel.enrich(&st.db, &ips, true).await;
    for r in rows {
        r.intel = r.src_ip.parse::<IpAddr>().ok().and_then(|ip| map.get(&ip).cloned());
    }
}

// ── 来源汇总与异常 ───────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, Serialize)]
pub struct SyncReport {
    /// 本轮扫到的 (节点, 来源 IP) 数
    pub pairs: usize,
    /// 写入（新插入或累加）的 node_source_seen 行数
    pub upserted: usize,
    pub opened: usize,
    pub updated: usize,
    pub resolved: usize,
    /// 因为没有任何离线库而跳过
    pub skipped_no_db: bool,
}

struct Agg {
    first: OffsetDateTime,
    last: OffsetDateTime,
    conns: i64,
}

/// 增量维护 node_source_seen。
/// - 小时汇总表最近两小时会被反复重算，所以连接数只累加已「关闭」的小时（早于 当前整点 - 1h），靠水位保证只加一次；
///   首次 / 最近一次出现用 min / max，重复扫描是幂等的。
/// - 学习期内出现的值不标记；学习期后第一次出现的 isp / city / 机房·VPN 来源 IP 标 open（最近一次出现已超过 ttl 的历史值直接标 done）。
pub async fn sync_sources(st: &AppState, now: OffsetDateTime) -> anyhow::Result<SyncReport> {
    let svc = &st.ipintel;
    let mut rep = SyncReport::default();
    if !svc.enabled {
        return Ok(rep);
    }
    if svc.intel.loaded() == 0 {
        // 没有库就没有画像可记：不推进水位，库装好后一次补上
        rep.skipped_no_db = true;
        return Ok(rep);
    }
    svc.ensure_internal(&st.db).await;
    let db = &st.db;
    let closed_upto = floor_hour(now) - Duration::hours(1);
    let wm: Option<OffsetDateTime> = sqlx::query_scalar("select ts from ipintel_state where key = $1").bind(WM_KEY).fetch_optional(db).await?;
    let scan_from = wm.unwrap_or(now - Duration::days(access::HOURLY_DAYS + 1));

    let rows: Vec<(Uuid, String, OffsetDateTime, OffsetDateTime, i64)> = sqlx::query_as(
        "select node_id, host(src_ip), min(hour), max(hour), coalesce(sum(conns) filter (where hour < $2), 0)::bigint
         from access_source_hourly where hour >= $1 group by node_id, src_ip",
    )
    .bind(scan_from)
    .bind(closed_upto)
    .fetch_all(db)
    .await?;
    rep.pairs = rows.len();

    let ips: Vec<IpAddr> = rows.iter().filter_map(|r| r.1.parse().ok()).collect();
    let briefs = svc.enrich(db, &ips, true).await;

    // (节点, 维度, 值) → 聚合
    let mut agg: BTreeMap<(Uuid, &'static str, String), Agg> = BTreeMap::new();
    for (node, ip, first, last, conns) in &rows {
        let Ok(ipa) = ip.parse::<IpAddr>() else { continue };
        let Some(b) = briefs.get(&ipa) else { continue };
        if b.private || b.internal.is_some() {
            continue;
        }
        let mut put = |dim: &'static str, value: String| {
            let e = agg.entry((*node, dim, value)).or_insert(Agg { first: *first, last: *last, conns: 0 });
            e.first = e.first.min(*first);
            e.last = e.last.max(*last);
            e.conns += *conns;
        };
        put("isp", b.isp.clone());
        if let Some(c) = city_value(b) {
            put("city", c);
        }
        put("conn_type", b.conn_type.clone());
        if ConnType::parse(&b.conn_type).is_suspicious() {
            put("ip", ip.clone());
        }
    }

    if !agg.is_empty() {
        // 每个节点的学习期起点 = 已记录的最早 first_seen 与本批最早的较小者
        let nodes: Vec<Uuid> = agg.keys().map(|k| k.0).collect::<HashSet<_>>().into_iter().collect();
        let known: Vec<(Uuid, OffsetDateTime)> =
            sqlx::query_as("select node_id, min(first_seen) from node_source_seen where node_id = any($1) group by node_id").bind(&nodes).fetch_all(db).await?;
        let mut start: HashMap<Uuid, OffsetDateTime> = known.into_iter().collect();
        for ((node, _, _), a) in &agg {
            let s = start.entry(*node).or_insert(a.first);
            *s = (*s).min(a.first);
        }
        let live_since = now - svc.ttl;
        let (mut n_node, mut n_dim, mut n_val, mut n_first, mut n_last, mut n_conns, mut n_flag) = (vec![], vec![], vec![], vec![], vec![], vec![], vec![]);
        for ((node, dim, value), a) in &agg {
            let flag: Option<&str> = if *dim != "conn_type" && a.first >= start[node] + svc.learn {
                Some(if a.last >= live_since { "open" } else { "done" })
            } else {
                None
            };
            n_node.push(*node);
            n_dim.push(dim.to_string());
            n_val.push(value.clone());
            n_first.push(a.first);
            n_last.push(a.last);
            n_conns.push(a.conns);
            n_flag.push(flag.map(str::to_string));
        }
        for lo in (0..n_node.len()).step_by(2000) {
            let hi = (lo + 2000).min(n_node.len());
            let r = sqlx::query(
                "insert into node_source_seen as n (node_id, dim, value, first_seen, last_seen, conns, flag)
                 select * from unnest($1::uuid[], $2::text[], $3::text[], $4::timestamptz[], $5::timestamptz[], $6::bigint[], $7::text[])
                 on conflict (node_id, dim, value) do update set
                    first_seen = least(n.first_seen, excluded.first_seen),
                    last_seen  = greatest(n.last_seen, excluded.last_seen),
                    conns      = n.conns + excluded.conns",
            )
            .bind(&n_node[lo..hi])
            .bind(&n_dim[lo..hi])
            .bind(&n_val[lo..hi])
            .bind(&n_first[lo..hi])
            .bind(&n_last[lo..hi])
            .bind(&n_conns[lo..hi])
            .bind(&n_flag[lo..hi])
            .execute(db)
            .await?;
            rep.upserted += r.rows_affected() as usize;
        }
    }
    sqlx::query(
        "insert into ipintel_state (key, ts) values ($1, $2) on conflict (key) do update set ts = greatest(ipintel_state.ts, excluded.ts)",
    )
    .bind(WM_KEY)
    .bind(closed_upto)
    .execute(db)
    .await?;
    Ok(rep)
}

struct OpenRow {
    value: String,
    conns: i64,
}

fn join_values(items: &[String], total: usize) -> String {
    let shown = items.join("、");
    if total > items.len() {
        format!("{shown} 等 {total} 项")
    } else {
        shown
    }
}

/// 把 flag = open 的值对齐到 incident 表：有就开 / 更新，没有了（最近一次出现已超过 ttl）就恢复。
pub async fn reconcile_incidents(st: &AppState, now: OffsetDateTime, rep: &mut SyncReport) -> anyhow::Result<()> {
    let db = &st.db;
    let svc = &st.ipintel;
    // 最近一次出现已超过 ttl 的，一次性异常结束
    sqlx::query("update node_source_seen set flag = 'done' where flag = 'open' and last_seen < $1").bind(now - svc.ttl).execute(db).await?;

    let rows: Vec<(Uuid, String, String, i64)> = sqlx::query_as(
        "select n.node_id, n.dim, n.value, n.conns from node_source_seen n join machine m on m.id = n.node_id
         where n.flag = 'open' and m.lifecycle = 'active' order by n.node_id, n.dim, n.conns desc, n.value",
    )
    .fetch_all(db)
    .await?;
    let mut want: BTreeMap<(&'static str, Uuid), Vec<OpenRow>> = BTreeMap::new();
    for (node, dim, value, conns) in rows {
        if let Some(rule) = rule_of_dim(&dim) {
            want.entry((rule, node)).or_default().push(OpenRow { value, conns });
        }
    }
    // 机房 / VPN 来源要带上运营商和接入方式
    let dc_ips: Vec<IpAddr> = want.iter().filter(|((r, _), _)| *r == RULE_DC_SOURCE).flat_map(|(_, v)| v.iter().filter_map(|o| o.value.parse().ok())).collect();
    let briefs = svc.enrich(db, &dc_ips, true).await;

    let ids: Vec<Uuid> = want.keys().map(|k| k.1).collect();
    let names: HashMap<Uuid, String> = sqlx::query_as::<_, (Uuid, String)>(
        "select m.id, coalesce(case when m.kind = 'relay' then m.label
             else t.name || '-' || case when m.number < 10 then '0' || m.number::text else m.number::text end end, '未命名')
         from machine m left join team t on t.id = m.team_id where m.id = any($1)",
    )
    .bind(&ids)
    .fetch_all(db)
    .await?
    .into_iter()
    .collect();

    #[derive(sqlx::FromRow)]
    struct Firing {
        id: Uuid,
        rule: String,
        subject_id: Uuid,
        summary: String,
    }
    let firing: Vec<Firing> = sqlx::query_as("select id, rule, subject_id, summary from incident where state = 'firing' and rule like 'ipintel!_%' escape '!'")
        .fetch_all(db)
        .await?;
    let mut have: HashMap<(String, Uuid), Firing> = firing.into_iter().map(|f| ((f.rule.clone(), f.subject_id), f)).collect();
    let silences: Vec<(Option<Uuid>, Option<String>)> =
        sqlx::query_as("select subject_id, rule from silence where deleted_at is null and now() between starts_at and ends_at").fetch_all(db).await?;
    let silenced = |rule: &str, id: Uuid| silences.iter().any(|(s, r)| s.is_none_or(|s| s == id) && r.as_deref().is_none_or(|r| r == rule));

    let mut changed = false;
    for ((rule, node), vals) in &want {
        let Some(name) = names.get(node) else { continue };
        let total = vals.len();
        let shown: Vec<String> = vals
            .iter()
            .take(3)
            .map(|o| match *rule {
                RULE_NEW_ISP => format!("{}（{} 次连接）", o.value, o.conns),
                RULE_NEW_CITY => format!("{}（{} 次连接）", city_label(&o.value), o.conns),
                _ => match o.value.parse::<IpAddr>().ok().and_then(|ip| briefs.get(&ip)) {
                    Some(b) => format!("{}（{} · {}，{} 次连接）", o.value, b.isp, b.conn_label, o.conns),
                    None => format!("{}（{} 次连接）", o.value, o.conns),
                },
            })
            .collect();
        let text = match *rule {
            RULE_NEW_ISP => format!("出现新的来源运营商：{}", join_values(&shown, total)),
            RULE_NEW_CITY => format!("出现新的来源城市：{}", join_values(&shown, total)),
            _ => format!("出现机房 / VPN 来源 IP：{}", join_values(&shown, total)),
        };
        let summary = format!("{name}：{text}");
        let detail = json!({"name": name, "values": vals.iter().map(|o| json!({"value": o.value, "conns": o.conns})).collect::<Vec<_>>()});
        if let Some(inc) = have.remove(&(rule.to_string(), *node)) {
            if inc.summary != summary {
                sqlx::query("update incident set summary = $2, detail = $3, last_seen_at = now() where id = $1").bind(inc.id).bind(&summary).bind(&detail).execute(db).await?;
                rep.updated += 1;
                changed = true;
            } else {
                sqlx::query("update incident set last_seen_at = now() where id = $1").bind(inc.id).execute(db).await?;
            }
            continue;
        }
        if silenced(rule, *node) {
            continue;
        }
        let id = crate::db::new_id();
        let res = sqlx::query(
            "insert into incident (id, rule, subject_id, severity, summary, detail) values ($1, $2, $3, 'warning'::severity, $4, $5)
             on conflict (rule, subject_id) where state = 'firing' do nothing",
        )
        .bind(id)
        .bind(rule)
        .bind(node)
        .bind(&summary)
        .bind(&detail)
        .execute(db)
        .await?;
        if res.rows_affected() > 0 {
            rep.opened += 1;
            changed = true;
            notify::spawn_dispatch(
                st,
                Notice {
                    kind: Kind::Firing,
                    incident_id: Some(id),
                    rule: rule.to_string(),
                    severity: "warning".into(),
                    machine: name.clone(),
                    summary: text,
                    at: now,
                    opened_at: Some(now),
                },
            );
        }
    }
    // 剩下的 firing = 已经没有 open 值（或节点不再 active）→ 恢复；一次性异常结束不发恢复通知
    for (_, inc) in have {
        sqlx::query(
            "update incident set state = 'resolved', resolved_at = now(), detail = detail || '{\"closed_by\":\"ipintel_ttl\"}' where id = $1 and state = 'firing'",
        )
        .bind(inc.id)
        .execute(db)
        .await?;
        rep.resolved += 1;
        changed = true;
    }
    if changed {
        st.emit(Event::Incidents);
    }
    Ok(())
}

/// 一轮维护：重载库 → 内部地址 → 来源汇总 → 异常对齐。`now` 由调用方给（测试可以快进时间）。
pub async fn maintain_once(st: &AppState, now: OffsetDateTime) -> anyhow::Result<SyncReport> {
    st.ipintel.reload().await;
    st.ipintel.refresh_internal(&st.db).await;
    let mut rep = sync_sources(st, now).await?;
    if st.ipintel.enabled {
        reconcile_incidents(st, now, &mut rep).await?;
    }
    Ok(rep)
}

/// 清理缓存：库换版后的旧行、久未更新的行
pub async fn prune_cache(st: &AppState) -> sqlx::Result<u64> {
    if !st.ipintel.enabled {
        return Ok(0);
    }
    let r = if st.ipintel.intel.loaded() > 0 {
        sqlx::query("delete from ip_intel_cache where updated_at < now() - interval '60 days' or (db_version <> $1 and updated_at < now() - interval '1 day')")
            .bind(st.ipintel.version())
            .execute(&st.db)
            .await?
    } else {
        sqlx::query("delete from ip_intel_cache where updated_at < now() - interval '60 days'").execute(&st.db).await?
    };
    Ok(r.rows_affected())
}

/// 后台循环：每 5 分钟一轮。启动后先等一会儿，让访问汇总的启动补算先跑完。
pub async fn run_forever(st: AppState) {
    if !st.ipintel.enabled {
        tracing::info!("ipintel 已在配置里关闭");
        return;
    }
    let st0 = st.ipintel.intel.status();
    tracing::info!("ipintel：加载 {}/{} 个库，版本 {}", st0["loaded"], st0["expected"], st0["version"]);
    tokio::time::sleep(StdDuration::from_secs(60)).await;
    let mut tick = tokio::time::interval(StdDuration::from_secs(300));
    let mut n = 0u32;
    loop {
        tick.tick().await;
        match maintain_once(&st, OffsetDateTime::now_utc()).await {
            Ok(r) if r.opened + r.resolved > 0 => tracing::info!("ipintel：新开 {} 条异常，恢复 {} 条", r.opened, r.resolved),
            Ok(_) => {}
            Err(e) => tracing::error!("ipintel 维护失败：{e:#}"),
        }
        if n.is_multiple_of(12) {
            if let Err(e) = prune_cache(&st).await {
                tracing::error!("ipintel 缓存清理失败：{e:#}");
            }
        }
        n = n.wrapping_add(1);
    }
}

// ── 查询：节点来源画像 ───────────────────────────────────────────────────────

fn pct(part: i64, total: i64) -> f64 {
    if total <= 0 {
        0.0
    } else {
        ((part as f64 / total as f64) * 1000.0).round() / 10.0
    }
}

#[derive(Default)]
struct DimAcc {
    conns: i64,
    sources: i64,
}

fn dist(m: HashMap<String, DimAcc>, total: i64, label: impl Fn(&str) -> String) -> Vec<Value> {
    let mut v: Vec<(String, DimAcc)> = m.into_iter().collect();
    v.sort_by(|a, b| b.1.conns.cmp(&a.1.conns).then_with(|| a.0.cmp(&b.0)));
    v.into_iter()
        .take(30)
        .map(|(value, a)| json!({"label": label(&value), "value": value, "conns": a.conns, "sources": a.sources, "pct": pct(a.conns, total)}))
        .collect()
}

pub fn conn_label(t: &str) -> &'static str {
    ConnType::parse(t).word()
}

/// `GET /machines/{id}/sources`：来源画像分布、top 来源、已学基线、学习期状态、该节点当前的画像异常。
/// 线路机取它名下所有入口对应的节点（与访问概况一致）。
pub async fn sources_profile(st: &AppState, machine_id: Uuid, window: Window, limit: i64, now: OffsetDateTime) -> ApiResult<Value> {
    let db = &st.db;
    let kind: String = sqlx::query_scalar("select kind::text from machine where id = $1")
        .bind(machine_id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这台机器"))?;
    let nodes: Vec<Uuid> = if kind == "relay" {
        sqlx::query_scalar("select distinct node_id from entry where relay_id = $1").bind(machine_id).fetch_all(db).await?
    } else {
        vec![machine_id]
    };
    let svc = &st.ipintel;
    svc.ensure_internal(db).await;
    let from = access::Filter::new(window, now).from;

    let rows: Vec<(String, i64, OffsetDateTime)> = sqlx::query_as(
        "select host(src_ip), sum(conns)::bigint c, max(hour) from access_source_hourly where node_id = any($1) and hour >= $2
         group by src_ip order by c desc, 1 limit 20000",
    )
    .bind(&nodes)
    .bind(from)
    .fetch_all(db)
    .await?;
    let ips: Vec<IpAddr> = rows.iter().filter_map(|r| r.0.parse().ok()).collect();
    let briefs = svc.enrich(db, &ips, true).await;

    let (mut isp, mut city, mut prov, mut country, mut ctype) = (HashMap::new(), HashMap::new(), HashMap::new(), HashMap::new(), HashMap::new());
    let (mut total, mut total_sources, mut internal_conns, mut private_conns, mut unresolved) = (0i64, 0i64, 0i64, 0i64, 0i64);
    fn bump(m: &mut HashMap<String, DimAcc>, k: String, c: i64) {
        let e: &mut DimAcc = m.entry(k).or_default();
        e.conns += c;
        e.sources += 1;
    }
    for (ip, c, _) in &rows {
        let Some(b) = ip.parse::<IpAddr>().ok().and_then(|i| briefs.get(&i)) else {
            unresolved += 1;
            continue;
        };
        if b.internal.is_some() {
            internal_conns += c;
            continue;
        }
        if b.private {
            private_conns += c;
            continue;
        }
        total += c;
        total_sources += 1;
        bump(&mut isp, b.isp.clone(), *c);
        if let Some(v) = city_value(b) {
            bump(&mut city, v, *c);
        }
        if let Some(p) = &b.province {
            bump(&mut prov, format!("{}|{}", b.country.clone().unwrap_or_default(), p), *c);
        }
        if let Some(cc) = &b.country {
            bump(&mut country, cc.clone(), *c);
        }
        bump(&mut ctype, b.conn_type.clone(), *c);
    }
    let top: Vec<Value> = rows
        .iter()
        .take(limit as usize)
        .map(|(ip, c, last)| {
            let intel = ip.parse::<IpAddr>().ok().and_then(|i| briefs.get(&i));
            json!({"src_ip": ip, "conns": c, "last_ts": access::fmt_ts(*last), "intel": intel})
        })
        .collect();

    // 已学基线
    let seen: Vec<(String, String, OffsetDateTime, OffsetDateTime, i64, Option<String>)> = sqlx::query_as(
        "select dim, value, min(first_seen), max(last_seen), sum(conns)::bigint,
                case when bool_or(flag = 'open') then 'open' when bool_or(flag = 'done') then 'done' end
         from node_source_seen where node_id = any($1) and dim in ('isp', 'city', 'conn_type') group by dim, value
         order by 5 desc, 2 limit 300",
    )
    .bind(&nodes)
    .fetch_all(db)
    .await?;
    let mut known = json!({"isp": [], "city": [], "conn_type": []});
    for (dim, value, first, last, conns, flag) in seen {
        let label = match dim.as_str() {
            "city" => city_label(&value),
            "conn_type" => conn_label(&value).to_string(),
            _ => value.clone(),
        };
        if let Some(a) = known[dim.as_str()].as_array_mut() {
            if a.len() < 50 {
                a.push(json!({"value": value, "label": label, "first_seen": access::fmt_ts(first), "last_seen": access::fmt_ts(last), "conns": conns, "anomaly": flag.as_deref() == Some("open")}));
            }
        }
    }

    // 学习期：从这些节点中最早的一条来源记录算起
    let started: Option<OffsetDateTime> = sqlx::query_scalar("select min(first_seen) from node_source_seen where node_id = any($1)").bind(&nodes).fetch_one(db).await?;
    let learning = match started {
        Some(s) => {
            let ends = s + svc.learn;
            json!({"started_at": access::fmt_ts(s), "ends_at": access::fmt_ts(ends), "learn_days": svc.learn.whole_days(), "done": now >= ends,
                   "remaining_hours": (ends - now).whole_hours().max(0)})
        }
        None => json!({"started_at": null, "ends_at": null, "learn_days": svc.learn.whole_days(), "done": false, "remaining_hours": null}),
    };

    let anomalies: Vec<(Uuid, String, String, String, OffsetDateTime)> = sqlx::query_as(
        "select id, rule, severity::text, summary, opened_at from incident
         where state = 'firing' and rule like 'ipintel!_%' escape '!' and subject_id = any($1) order by opened_at desc",
    )
    .bind(&nodes)
    .fetch_all(db)
    .await?;
    let anomalies: Vec<Value> = anomalies
        .into_iter()
        .map(|(id, rule, sev, summary, opened)| json!({"id": id, "rule": rule, "rule_name": notify::rule_name(&rule), "severity": sev, "summary": summary, "opened_at": access::fmt_ts(opened)}))
        .collect();

    Ok(json!({
        "window": match window { Window::H1 => "1h", Window::H24 => "24h", Window::D7 => "7d", Window::D30 => "30d" },
        "window_start": access::fmt_ts(from),
        "empty": rows.is_empty(),
        "degraded": svc.intel.loaded() == 0,
        "total_conns": total,
        "total_sources": total_sources,
        "internal_conns": internal_conns,
        "private_conns": private_conns,
        "unresolved_sources": unresolved,
        "distribution": {
            "isp": dist(isp, total, str::to_string),
            "city": dist(city, total, city_label),
            "province": dist(prov, total, |v| v.split_once('|').map(|x| x.1).unwrap_or(v).to_string()),
            "country": dist(country, total, str::to_string),
            "conn_type": dist(ctype, total, |v| conn_label(v).to_string()),
        },
        "top_sources": top,
        "known": known,
        "learning": learning,
        "anomalies": anomalies,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn place_labels() {
        assert_eq!(place(Some("CN"), Some("河南省"), Some("郑州市")), "河南省郑州市");
        assert_eq!(place(Some("CN"), Some("北京市"), Some("北京市")), "北京市");
        assert_eq!(place(Some("CN"), Some("河南省"), None), "河南省");
        assert_eq!(place(Some("US"), Some("Texas"), Some("Dallas")), "US · Dallas");
        assert_eq!(place(Some("US"), None, None), "US");
        assert_eq!(place(None, None, None), "");
        assert_eq!(city_label("CN|河南省|郑州市"), "河南省郑州市");
        assert_eq!(city_label("US||Dallas"), "US · Dallas");
    }

    #[test]
    fn pct_rounds_to_one_decimal() {
        assert_eq!(pct(1, 3), 33.3);
        assert_eq!(pct(0, 0), 0.0);
        assert_eq!(pct(5, 5), 100.0);
    }

    #[test]
    fn join_truncates() {
        let v = vec!["a".to_string(), "b".to_string()];
        assert_eq!(join_values(&v, 2), "a、b");
        assert_eq!(join_values(&v, 5), "a、b 等 5 项");
    }
}
