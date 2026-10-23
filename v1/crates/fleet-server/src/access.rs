//! 访问监控：把 access_event 汇总成小时表（每 5 分钟重算当前小时和被标脏的小时，幂等），以及各个查询。
//!
//! - 1h 窗口直接查 access_event（原始记录）；24h 及以上查小时汇总表，窗口里整天的部分查天汇总表（access_daily，7d / 30d 主要读它）。
//!   各留多久按面板里设的访问记录保留期（settings.rs），
//!   窗口不能比汇总的保留期长（`Window::retained`），过期的由 `prune` 与分区维护删掉。
//! - 随机子域名合并在库里的 `access_collapse()`（见迁移 0006），汇总和 1h 查询共用同一份规则。
//! - 目标按 (节点, 目标) 聚合，「域名与 IP」默认再按目标合并（`target_groups`）。「来源数」在小时表里是各小时去重数的最大值（跨小时无法精确去重，偏小的近似）。

use crate::db::{floor_hour, Args};
use crate::lock;
use crate::settings::AccessRetention;
use crate::state::AppState;
use anyhow::Context;
use serde::Serialize;
use sqlx::{FromRow, PgPool};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration as StdDuration;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

/// 汇总用的咨询锁：同一时刻只让一个汇总在跑（增量累计 total 依赖这点）
const LOCK_KEY: i64 = 0x6163_6365_7373;

// ── 汇总 ─────────────────────────────────────────────────────────────────────

/// 重算 [hour, hour+1h) 的汇总。$1 = hour, $2 = hour + 1h, $3 = 「已关闭」的边界（早于它的小时可能已被来源画像累加过）。
/// 一次扫描原始记录，同时产出 access_hourly、access_source_hourly、access_target_seen 三张表。
/// 先按 (节点, 目标, 来源) 分组，再只对去重后的目标做合并，避免对每条记录调用 plpgsql。
/// target_seen.total 按「本次重算值 - 旧值」累加，所以重复执行不会重复计数。
/// 三处 upsert 都只在值真的变了时才写（重算当前小时时大部分行没变，不产生死元组）。
/// 早于 $3 的小时若来源连接数变了，把差值记进 access_source_late，由 ipintel 的来源同步补记（见 ipintel::sync_sources）。
const ROLLUP: &str = "
with rawg as (
    select en.node_id, en.relay_id, lower(rtrim(e.dest_host, '.')) h, e.src_ip,
           count(*) c, count(*) filter (where e.blocked) b, min(e.ts) f, max(e.ts) l
    from access_event e join entry en on en.id = e.entry_id
    where e.ts >= $1 and e.ts < $2
    group by 1, 2, 3, 4
), hm as (
    select h, access_collapse(h) c from (select distinct h from rawg) d
), agg as (
    select r.node_id, r.relay_id, hm.c dest_host, access_kind(hm.c) kind,
           sum(r.c)::int conns, sum(r.b)::int blocked, count(distinct r.src_ip)::int sources, min(r.f) f, max(r.l) l
    from rawg r join hm on hm.h = r.h
    group by r.node_id, r.relay_id, hm.c
), old as (
    select node_id, dest_host, sum(conns)::bigint c from access_hourly where hour = $1 group by 1, 2
), up as (
    insert into access_hourly (node_id, relay_id, hour, dest_host, kind, conns, blocked, sources)
    select node_id, relay_id, $1, dest_host, kind, conns, blocked, sources from agg
    on conflict (node_id, hour, relay_id, dest_host) do update
      set kind = excluded.kind, conns = excluded.conns, blocked = excluded.blocked, sources = excluded.sources
      where (access_hourly.kind, access_hourly.conns, access_hourly.blocked, access_hourly.sources)
            is distinct from (excluded.kind, excluded.conns, excluded.blocked, excluded.sources)
    returning 1
), nw as (
    select node_id, dest_host, sum(conns)::bigint c, min(f) f, max(l) l from agg group by 1, 2
), src as (
    select node_id, relay_id, src_ip, sum(c)::int conns from rawg where src_ip is not null group by 1, 2, 3
), src_up as (
    insert into access_source_hourly (node_id, relay_id, hour, src_ip, conns)
    select node_id, relay_id, $1, src_ip, conns from src
    on conflict (node_id, hour, relay_id, src_ip) do update set conns = excluded.conns
      where access_source_hourly.conns is distinct from excluded.conns
    returning 1
), late as (
    insert into access_source_late (node_id, hour, src_ip, delta)
    select s.node_id, $1, s.src_ip, sum(s.conns - coalesce(o.conns, 0))::int
    from src s left join access_source_hourly o on o.node_id = s.node_id and o.relay_id = s.relay_id and o.hour = $1 and o.src_ip = s.src_ip
    where $1 < $3 and s.conns <> coalesce(o.conns, 0)
    group by s.node_id, s.src_ip
    on conflict (node_id, hour, src_ip) do update set delta = access_source_late.delta + excluded.delta
    returning 1
)
insert into access_target_seen (node_id, dest_host, first_seen, last_seen, total)
select n.node_id, n.dest_host, n.f, n.l, greatest(n.c - coalesce(o.c, 0), 0)
from nw n left join old o on o.node_id = n.node_id and o.dest_host = n.dest_host
on conflict (node_id, dest_host) do update
  set first_seen = least(access_target_seen.first_seen, excluded.first_seen),
      last_seen  = greatest(access_target_seen.last_seen, excluded.last_seen),
      total      = greatest(access_target_seen.total + excluded.total, 0)
  where excluded.total <> 0 or excluded.first_seen < access_target_seen.first_seen or excluded.last_seen > access_target_seen.last_seen";

/// 从小时表重算一天（$1 = 那天 UTC 零点）的天汇总：各小时相加，来源数取各小时的最大值。小时表的行只增不减（保留期整天删），
/// 天表跟着只 upsert；值没变的行不写
const ROLLUP_DAY: &str = "
insert into access_daily (node_id, relay_id, day, dest_host, kind, conns, blocked, sources)
select node_id, relay_id, $1, dest_host, min(kind), sum(conns), sum(blocked), max(sources)
from access_hourly where hour >= $1 and hour < $1 + interval '1 day'
group by node_id, relay_id, dest_host
on conflict (node_id, day, relay_id, dest_host) do update
  set kind = excluded.kind, conns = excluded.conns, blocked = excluded.blocked, sources = excluded.sources
  where (access_daily.kind, access_daily.conns, access_daily.blocked, access_daily.sources)
        is distinct from (excluded.kind, excluded.conns, excluded.blocked, excluded.sources)";

/// UTC 零点（天汇总的「天」）
pub fn floor_day(t: OffsetDateTime) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(t.unix_timestamp().div_euclid(86_400) * 86_400).unwrap_or(t)
}

/// 重算一个整点小时（UTC 对齐），同一事务里重算它所在的那天：天表与小时表任何时候都对得上
pub async fn rollup_hour(db: &PgPool, hour: OffsetDateTime) -> anyhow::Result<()> { rollup_day_hours(db, &[hour]).await }

/// 重算同一天里的几个小时，再重算那一天（一个事务）
async fn rollup_day_hours(db: &PgPool, hours: &[OffsetDateTime]) -> anyhow::Result<()> {
    let Some(&first) = hours.first() else { return Ok(()) };
    let mut tx = db.begin().await?;
    sqlx::query("select pg_advisory_xact_lock($1)").bind(LOCK_KEY).execute(&mut *tx).await?;
    // 往后多算 10 分钟：接近整点时宁可多记一点差值（用不上的会被同步丢弃），也不能漏
    let closed = floor_hour(OffsetDateTime::now_utc() + Duration::minutes(10)) - Duration::hours(1);
    for &hour in hours {
        sqlx::query(ROLLUP).bind(hour).bind(hour + Duration::hours(1)).bind(closed).execute(&mut *tx).await?;
    }
    sqlx::query(ROLLUP_DAY).bind(floor_day(first)).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// 后台循环每轮做的事：当前小时必算，被标脏的小时（agent 补传的迟到数据）补算（语义见 `db::rollup_dirty`）
pub async fn rollup_pending(db: &PgPool) -> anyhow::Result<()> { crate::db::rollup_dirty(db, "access_dirty", |h| rollup_hour(db, h)).await }

/// 补算 [from, 现在] 的所有小时（逐小时，内存有界）；按天一个事务，一天的小时算完再算一次那天
pub async fn rollup_since(db: &PgPool, from: OffsetDateTime) -> anyhow::Result<()> {
    let end = floor_hour(OffsetDateTime::now_utc());
    let mut h = floor_hour(from);
    while h <= end {
        let next_day = floor_day(h) + Duration::days(1);
        let mut hours = vec![];
        while h <= end && h < next_day {
            hours.push(h);
            h += Duration::hours(1);
        }
        rollup_day_hours(db, &hours).await?;
    }
    Ok(())
}

/// 启动补算：从汇总表里最新的小时算起（宕机期间的缺口），表是空的就补整个原始记录的保留期（`raw_days`）。
pub async fn backfill(db: &PgPool, raw_days: u32) -> anyhow::Result<()> {
    let floor = OffsetDateTime::now_utc() - Duration::days(raw_days as i64);
    let last: Option<OffsetDateTime> = sqlx::query_scalar("select max(hour) from access_hourly").fetch_one(db).await?;
    let from = last.map(|l| l.max(floor)).unwrap_or(floor);
    rollup_since(db, from).await
}

/// 按访问记录保留期删过期的汇总与来源画像（后台每小时一次；面板改了保留期后马上一次，见 `settings::enforce`），返回删了多少行。
/// 截止时间对齐整点：窗口起点也对齐整点，窗口和保留期一样长时第一个小时不会被删掉。
/// - 小时汇总、天汇总、来源小时汇总、迟到差值、脏标记：`hourly_days` 天前的小时（天汇总：那天的零点在这之前的。窗口不比保留期长，
///   整天部分从窗口起点之后的第一个零点算起，用不到它）；
/// - 目标首次 / 最近出现（access_target_seen）：最近一次出现在 `hourly_days` 天前的。之后再出现按第一次出现算，
///   即「新目标」= 保留期内没见过；
/// - 画像缓存（ip_intel_cache：访问来源的 IP，另有机器自己的地址——国旗用）：`hourly_days` 天前写入的，再用到时重新查离线库；
/// - 来源画像（node_source_seen：运营商 / 城市 / 接入方式 / 机房·VPN 来源 IP，各维度都删）：最近一次出现在 `profile_days` 天前、
///   不在告警中（open）的。之后再出现按「新运营商 / 新城市 / 首次出现的机房来源」再告警一次，即「新」= 这段时间内没见过；
///   学习期起点是节点剩下的行里最早的首次出现，一个节点的行全删光（闲置超过这么久）之后再有流量，学习期从头算；
/// - 画像异常告警（ipintel_ 开头，摘要里写着来源 IP、运营商、城市）：恢复超过 `profile_days` 天的（别的告警仍是 180 天）。
///
/// 原始记录按天分区，由 `db::maintain_partitions` 删。
pub async fn prune(db: &PgPool, ret: AccessRetention, profile_days: u32) -> anyhow::Result<u64> {
    let hour = floor_hour(OffsetDateTime::now_utc());
    let (hourly, profile) = (hour - Duration::days(ret.hourly_days as i64), hour - Duration::days(profile_days as i64));
    let mut n = 0;
    for (what, sql, cutoff) in [
        ("小时汇总", "delete from access_hourly where hour < $1", hourly),
        ("天汇总", "delete from access_daily where day < $1", hourly),
        ("来源小时汇总", "delete from access_source_hourly where hour < $1", hourly),
        ("迟到差值", "delete from access_source_late where hour < $1", hourly),
        ("脏标记", "delete from access_dirty where hour < $1", hourly),
        ("目标首次出现", "delete from access_target_seen where last_seen < $1", hourly),
        ("来源 IP 画像缓存", "delete from ip_intel_cache where updated_at < $1", hourly),
        ("来源画像", "delete from node_source_seen where last_seen < $1 and flag is distinct from 'open'", profile),
        ("画像异常告警", "delete from incident where engine = 'ipintel' and state = 'resolved' and resolved_at < $1", profile),
    ] {
        n += sqlx::query(sql).bind(cutoff).execute(db).await.with_context(|| format!("清理过期的{what}"))?.rows_affected();
    }
    Ok(n)
}

/// 后台循环：启动补算一次，之后每 5 分钟补算当前小时和脏小时；启动时和之后每小时按保留期清理一次。
pub async fn run_forever(st: AppState) {
    if let Err(e) = backfill(&st.db, st.access_retention().raw_days).await {
        tracing::error!("访问汇总补算失败：{e:#}");
    }
    let mut t = tokio::time::interval(StdDuration::from_secs(300));
    // 一轮跑得比间隔长时不要连着补跑，顺延到下一个间隔
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut n = 0u32;
    loop {
        t.tick().await;
        match rollup_pending(&st.db).await {
            Ok(()) => st.access_cache.invalidate(),
            Err(e) => tracing::error!("访问汇总失败：{e:#}"),
        }
        if n.is_multiple_of(12) {
            if let Err(e) = prune(&st.db, st.access_retention(), st.profile_days()).await {
                tracing::error!("访问记录保留期清理失败：{e:#}");
            }
        }
        n = n.wrapping_add(1);
    }
}

// ── 查询 ─────────────────────────────────────────────────────────────────────

/// 24h / 7d / 30d 查询结果的缓存（键 = 接口 + 全部查询参数，窗口起点已对齐整点）。
/// 这些窗口读的是小时汇总表，只在每轮汇总后才变：每轮汇总成功后整体作废（代数 +1），聚合结果（summary / targets / sources）
/// 缓存 `ttl`，选 7d / 30d 时不必每次轮询都重算全量聚合；单台机器的结果（带最近的原始记录、来源画像的基线与异常）
/// 不全随汇总变，最多缓存 `SHORT_TTL`。1h 窗口读原始记录，不缓存。存 `Arc`：命中时直接拿去序列化响应，不深拷贝整份结果
pub struct QueryCache {
    ttl: StdDuration,
    /// 每轮汇总成功后 +1。计算前记下，期间汇总过的结果放进来也不会命中
    gen: AtomicU64,
    map: std::sync::Mutex<std::collections::HashMap<String, Cached>>,
}

/// (放进来的时间, 计算前的代数, 结果)
type Cached = (std::time::Instant, u64, Arc<serde_json::Value>);

impl QueryCache {
    const MAX: usize = 256;
    /// 单台机器的结果最多缓存这么久
    const SHORT_TTL: StdDuration = StdDuration::from_secs(30);

    pub fn new(ttl: StdDuration) -> Self {
        Self { ttl, gen: AtomicU64::new(0), map: Default::default() }
    }

    /// 当前代数：计算前取，`put` 时带上
    pub fn gen(&self) -> u64 { self.gen.load(Ordering::SeqCst) }

    /// 汇总表变了，之前的结果全部作废
    pub fn invalidate(&self) { self.gen.fetch_add(1, Ordering::SeqCst); }

    /// `short`：单台机器的结果，按 `SHORT_TTL` 过期
    pub fn get(&self, key: &str, short: bool) -> Option<Arc<serde_json::Value>> {
        if self.ttl.is_zero() {
            return None;
        }
        let ttl = if short { self.ttl.min(Self::SHORT_TTL) } else { self.ttl };
        let g = self.gen();
        let m = lock(&self.map);
        m.get(key).filter(|(at, eg, _)| *eg == g && at.elapsed() < ttl).map(|(_, _, v)| v.clone())
    }

    pub fn put(&self, key: String, gen: u64, v: Arc<serde_json::Value>) {
        if self.ttl.is_zero() {
            return;
        }
        let mut m = lock(&self.map);
        if m.len() >= Self::MAX {
            let (ttl, g) = (self.ttl, self.gen());
            m.retain(|_, (at, eg, _)| *eg == g && at.elapsed() < ttl);
            if m.len() >= Self::MAX {
                m.clear();
            }
        }
        m.insert(key, (std::time::Instant::now(), gen, v));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    H1,
    /// 6 小时：同抽屉其它曲线页签的时间范围（1 / 6 / 24 小时、7 / 30 天），读小时汇总
    H6,
    H24,
    D7,
    D30,
}

impl Window {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "1h" => Self::H1,
            "6h" => Self::H6,
            "24h" => Self::H24,
            "7d" => Self::D7,
            "30d" => Self::D30,
            _ => return None,
        })
    }
    pub fn duration(self) -> Duration {
        match self {
            Self::H1 => Duration::hours(1),
            Self::H6 => Duration::hours(6),
            Self::H24 => Duration::hours(24),
            Self::D7 => Duration::days(7),
            Self::D30 => Duration::days(30),
        }
    }
    /// 同访问页的写法
    pub fn label(self) -> &'static str {
        match self {
            Self::H1 => "1 小时",
            Self::H6 => "6 小时",
            Self::H24 => "24 小时",
            Self::D7 => "7 天",
            Self::D30 => "30 天",
        }
    }
    /// 汇总只留 `hourly_days` 天时能不能看这个窗口：1h 读原始记录（至少留 1 天），其余读小时汇总，不能比它的保留期长
    /// （更早的已经删了，算出来的数是残缺的）
    pub fn retained(self, hourly_days: u32) -> bool { self == Self::H1 || self.duration() <= Duration::days(hourly_days as i64) }
}

/// 一次查询的筛选条件。拼 SQL 时只拼给了的条件（见 `db::Args`）
#[derive(Debug, Clone)]
pub struct Filter {
    /// 窗口起点。events=false 时已对齐到整点小时
    pub from: OffsetDateTime,
    /// true：读 access_event（1h 窗口）；false：读小时汇总表
    pub events: bool,
    pub team: Option<Uuid>,
    pub nodes: Option<Vec<Uuid>>,
    /// 只算经这台线路机的流量（线路机的访问概况 / 来源画像；含它名下已移除的入口）
    pub relay: Option<Uuid>,
    /// 已加好 % 并转义的 ilike 模式
    pub like: Option<String>,
    /// domain | ip
    pub kind: Option<String>,
    /// all | blocked | routed
    pub action: &'static str,
    /// 只看这一个目标（合并后的名字，精确匹配）：「域名与 IP」按目标聚合时展开看各节点
    pub host: Option<String>,
}

impl Filter {
    pub fn new(window: Window, now: OffsetDateTime) -> Self {
        let events = window == Window::H1;
        let raw = now - window.duration();
        Self { from: if events { raw } else { floor_hour(raw) }, events, team: None, nodes: None, relay: None, like: None, kind: None, action: "all", host: None }
    }
}

/// 用户输入 → ilike 模式。开头的「*.」当后缀匹配（只留「.」）：小时汇总存的是合并后的「*.example.com」，
/// 原始记录存真实主机名，照字面搜「*」原始记录永远是 0 条，各页签对不上
pub fn like_pattern(s: &str) -> Option<String> {
    let s = s.trim();
    let s = s.strip_prefix('*').filter(|r| r.starts_with('.')).unwrap_or(s);
    if s.is_empty() {
        return None;
    }
    let mut out = String::from("%");
    for c in s.chars().take(200) {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    Some(out)
}

/// 机器显示名（SQL 函数 display_name，与 names::display_name 一致）。退役机器的编号已被回收给别的机器，
/// 历史记录里不加标注会和现在同名的机器混在一起，所以带上「（已退役）」；已进回收站的用回收站里的原名（进备用前的名字，
/// 「备用-01」之后会给下一台备用机器用）加「（已回收）」。
/// 这里的 m 都是入口的节点端：节点是线路机的只有直出入口，名字同入口列表「线路机名-直出」
const NAME: &str = "coalesce((select rb.name from recycle_bin rb where m.recycled_at is not null and rb.source = 'fleet' and rb.source_id = m.id::text),
        display_name(m.kind::text, m.label, t.name, m.number))
    || case when m.kind = 'relay' then '-直出' else '' end
    || case when m.recycled_at is not null then '（已回收）' when m.lifecycle = 'retired' then '（已退役）' else '' end";

/// 原始记录（access_event e）的公共条件：窗口起点、节点、团队、线路机（都换成入口 id 列表，走 (entry_id, ts) 索引）、搜索。
/// `search_src_only`：搜索只匹配来源 IP（来源排行）；否则目标或来源 IP 匹配都算
fn event_where(f: &Filter, a: &mut Args, search_src_only: bool) -> String {
    let mut w = format!("e.ts >= {}", a.add(f.from));
    if let Some(n) = &f.nodes {
        w += &format!(" and e.entry_id = any(array(select id from entry where node_id = any({})))", a.add(n));
    }
    if let Some(t) = f.team {
        w += &format!(" and e.entry_id = any(array(select x.id from entry x join machine mx on mx.id = x.node_id where mx.team_id = {}))", a.add(t));
    }
    if let Some(l) = &f.like {
        let l = a.add(l);
        w += &if search_src_only { format!(" and host(e.src_ip) ilike {l}") } else { format!(" and (e.dest_host ilike {l} or host(e.src_ip) ilike {l})") };
    }
    if let Some(r) = f.relay {
        w += &format!(" and e.entry_id = any(array(select id from entry where relay_id = {}))", a.add(r));
    }
    w
}

/// 原始记录逐条按目标类型、处理结果筛选（拼在 `event_where` 后面）
fn event_kind_action(f: &Filter, a: &mut Args) -> String {
    let mut w = f.kind.as_ref().map(|k| format!(" and access_kind(lower(rtrim(e.dest_host, '.'))) = {}", a.add(k))).unwrap_or_default();
    w += match f.action {
        "blocked" => " and e.blocked",
        "routed" => " and not e.blocked",
        _ => "",
    };
    w
}

/// 小时表（access_hourly / access_source_hourly，别名 `t`）的公共条件；`search` 是搜索要匹配的列
fn hourly_where(f: &Filter, a: &mut Args, t: &str, search: &str) -> String {
    let w = format!("{t}.hour >= {}", a.add(f.from));
    w + &scope_where(f, a, t, search)
}

/// 窗口 [from, 现在] 里整天的部分 [d0, d1)（UTC 零点）：至少有一整天才给。首尾不满一天的读小时表
fn whole_days(from: OffsetDateTime, now: OffsetDateTime) -> Option<(OffsetDateTime, OffsetDateTime)> {
    let d0 = floor_day(from + Duration::days(1) - Duration::nanoseconds(1));
    let d1 = floor_day(now);
    (d1 > d0).then_some((d0, d1))
}

/// 汇总表（别名 `t`）除时间以外的条件，每个都以 and 开头：节点、团队、搜索、线路机
fn scope_where(f: &Filter, a: &mut Args, t: &str, search: &str) -> String {
    let mut w = String::new();
    if let Some(n) = &f.nodes {
        w += &format!(" and {t}.node_id = any({})", a.add(n));
    }
    if let Some(team) = f.team {
        w += &format!(" and {t}.node_id in (select id from machine where team_id = {})", a.add(team));
    }
    if let Some(l) = &f.like {
        w += &format!(" and {search} ilike {}", a.add(l));
    }
    if let Some(r) = f.relay {
        w += &format!(" and {t}.relay_id = {}", a.add(r));
    }
    w
}

/// 聚合成 (节点, 目标) → 连接数、阻止数、来源数：小时表，或原始记录（1h 窗口，同样的输出列）；
/// 再按目标类型与处理结果筛选（blocked 视图里连接数就是被阻止数，routed 视图里去掉被阻止的）。
/// 只看一个目标时：小时表直接按目标查（走 (dest_host, hour) 索引）；原始记录要先合并随机子域名，合并后再筛
fn cte(f: &Filter, a: &mut Args) -> String {
    let agg = if f.events {
        let host = f.host.as_ref().map(|h| format!(" where hm.c = {}", a.add(h))).unwrap_or_default();
        format!(
            "rawg as (
    select en.node_id, lower(rtrim(e.dest_host, '.')) h, e.src_ip,
           count(*) c, count(*) filter (where e.blocked) b, min(e.ts) f, max(e.ts) l
    from access_event e join entry en on en.id = e.entry_id
    where {}
    group by 1, 2, 3
), hm as (
    select h, access_collapse(h) c from (select distinct h from rawg) d
), agg as (
    select r.node_id, hm.c dest_host, access_kind(hm.c) kind, sum(r.c)::bigint conns, sum(r.b)::bigint blocked,
           count(distinct r.src_ip)::bigint sources, min(r.f) first_ts, max(r.l) last_ts
    from rawg r join hm on hm.h = r.h{host}
    group by r.node_id, hm.c)",
            event_where(f, a, false)
        )
    } else {
        let scope = |a: &mut Args, t: &str| {
            let mut w = scope_where(f, a, t, &format!("{t}.dest_host"));
            if let Some(h) = &f.host {
                w += &format!(" and {t}.dest_host = {}", a.add(h));
            }
            w
        };
        // 整天的部分读天表，首尾不满一天的读小时表（`whole_days`）；天表每行是那天各小时的合计，合起来与只读小时表相同
        let rows = match whole_days(f.from, OffsetDateTime::now_utc()) {
            None => format!("select h.node_id, h.dest_host, h.kind, h.conns, h.blocked, h.sources from access_hourly h where h.hour >= {}{}", a.add(f.from), scope(a, "h")),
            Some((d0, d1)) => format!(
                "select h.node_id, h.dest_host, h.kind, h.conns, h.blocked, h.sources from access_hourly h where h.hour >= {} and h.hour < {}{}
    union all
    select d.node_id, d.dest_host, d.kind, d.conns, d.blocked, d.sources from access_daily d where d.day >= {} and d.day < {}{}
    union all
    select h.node_id, h.dest_host, h.kind, h.conns, h.blocked, h.sources from access_hourly h where h.hour >= {}{}",
                a.add(f.from),
                a.add(d0),
                scope(a, "h"),
                a.add(d0),
                a.add(d1),
                scope(a, "d"),
                a.add(d1),
                scope(a, "h")
            ),
        };
        format!(
            "agg as (
    select u.node_id, u.dest_host, u.kind, sum(u.conns)::bigint conns, sum(u.blocked)::bigint blocked, max(u.sources)::bigint sources,
           null::timestamptz first_ts, null::timestamptz last_ts
    from ({rows}) u
    group by u.node_id, u.dest_host, u.kind)"
        )
    };
    let (conns, blocked, keep) = match f.action {
        "blocked" => ("blocked", "blocked", "blocked > 0"),
        "routed" => ("conns - blocked", "0::bigint", "conns > blocked"),
        _ => ("conns", "blocked", "true"),
    };
    let kind = f.kind.as_ref().map(|k| format!(" and kind = {}", a.add(k))).unwrap_or_default();
    format!("with {agg}, f as (select node_id, dest_host, kind, {conns} conns, {blocked} blocked, sources, first_ts, last_ts from agg where {keep}{kind})")
}

#[derive(Debug, Serialize, FromRow)]
pub struct NodeRow {
    pub node_id: Uuid,
    pub name: String,
    pub team: Option<String>,
    pub conns: i64,
    pub targets: i64,
    pub domains: i64,
    /// 去重来源数，跟随全部筛选；小时表算不出带目标筛选的来源数，这时为 None
    #[sqlx(skip)]
    pub sources: Option<i64>,
    pub blocked: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_ts: Option<OffsetDateTime>,
    /// 全部节点合起来按目标去重的目标数（每行都带，取出来放进 KPI）
    #[serde(skip)]
    pub unique_targets: i64,
}

#[derive(Debug, Serialize)]
pub struct Kpi {
    pub active_nodes: i64,
    pub conns: i64,
    /// 各节点分别去重后相加（两个节点访问同一个目标算 2 个）
    pub targets: i64,
    /// 按目标去重：同一个目标只算 1 个，与「域名与 IP」按目标合并的 `total` 同口径
    pub unique_targets: i64,
    pub blocked: i64,
}

/// 节点概览 + KPI。
pub async fn summary(db: &PgPool, f: &Filter) -> sqlx::Result<(Kpi, Vec<NodeRow>)> {
    let mut a = Args::default();
    // 按目标去重的总数：不相关子查询只算一次；f 引用了两次，PG 只物化一份，不会把汇总表（或原始记录）再扫一遍
    let sql = format!(
        "{} select f.node_id, {NAME} name, t.name team, sum(f.conns)::bigint conns, count(*)::bigint targets,
                count(*) filter (where f.kind = 'domain')::bigint domains, sum(f.blocked)::bigint blocked,
                max(coalesce(f.last_ts, s.last_seen)) last_ts,
                (select count(distinct u.dest_host) from f u join machine mu on mu.id = u.node_id)::bigint unique_targets
         from f join machine m on m.id = f.node_id left join team t on t.id = m.team_id
              left join access_target_seen s on s.node_id = f.node_id and s.dest_host = f.dest_host
         group by f.node_id, m.id, t.name
         order by conns desc, name",
        cte(f, &mut a)
    );
    let mut nodes: Vec<NodeRow> = sqlx::query_as_with(&sql, a.0).fetch_all(db).await?;

    // 来源数和旁边的连接数同一套筛选：原始记录逐条带上搜索、目标类型、处理结果；
    // 来源小时表没有目标维度，带这几类筛选时算不出来，给 null（不拿没筛过的数冒充）
    let srcs: Option<Vec<(Uuid, i64)>> = if f.events {
        let mut a = Args::default();
        let sql = format!(
            "select en.node_id, count(distinct e.src_ip)::bigint from access_event e join entry en on en.id = e.entry_id
             where {}{} group by 1",
            event_where(f, &mut a, false),
            event_kind_action(f, &mut a)
        );
        Some(sqlx::query_as_with(&sql, a.0).fetch_all(db).await?)
    } else if f.like.is_none() && f.kind.is_none() && f.action == "all" {
        let mut a = Args::default();
        let sql = format!("select s.node_id, count(distinct s.src_ip)::bigint from access_source_hourly s where {} group by 1", hourly_where(f, &mut a, "s", "host(s.src_ip)"));
        Some(sqlx::query_as_with(&sql, a.0).fetch_all(db).await?)
    } else {
        None
    };
    if let Some(srcs) = srcs {
        let srcs: std::collections::HashMap<Uuid, i64> = srcs.into_iter().collect();
        for n in &mut nodes {
            n.sources = Some(srcs.get(&n.node_id).copied().unwrap_or(0));
        }
    }
    let kpi = Kpi {
        active_nodes: nodes.len() as i64,
        conns: nodes.iter().map(|n| n.conns).sum(),
        targets: nodes.iter().map(|n| n.targets).sum(),
        unique_targets: nodes.first().map_or(0, |n| n.unique_targets),
        blocked: nodes.iter().map(|n| n.blocked).sum(),
    };
    Ok((kpi, nodes))
}

#[derive(Debug, Serialize, FromRow)]
pub struct TargetRow {
    pub node_id: Uuid,
    pub name: String,
    pub dest_host: String,
    pub kind: String,
    pub conns: i64,
    pub sources: i64,
    pub blocked: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub first_seen: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_seen: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Copy)]
pub enum TargetSort {
    Conns,
    Last,
    First,
}

pub async fn targets(db: &PgPool, f: &Filter, sort: TargetSort, limit: i64, offset: i64) -> sqlx::Result<Vec<TargetRow>> {
    let order = match sort {
        TargetSort::Conns => "f.conns desc, f.dest_host, f.node_id",
        TargetSort::Last => "last_seen desc nulls last, f.dest_host, f.node_id",
        TargetSort::First => "first_seen desc nulls last, f.dest_host, f.node_id",
    };
    let mut a = Args::default();
    let sql = format!(
        "{} select f.node_id, {NAME} name, f.dest_host, f.kind, f.conns, f.sources, f.blocked,
                coalesce(s.first_seen, f.first_ts) first_seen, coalesce(f.last_ts, s.last_seen) last_seen
         from f join machine m on m.id = f.node_id left join team t on t.id = m.team_id
              left join access_target_seen s on s.node_id = f.node_id and s.dest_host = f.dest_host
         order by {order} limit {} offset {}",
        cte(f, &mut a),
        a.add(limit),
        a.add(offset)
    );
    sqlx::query_as_with(&sql, a.0).fetch_all(db).await
}

/// 按目标聚合的一行：几个节点访问了它、合计连接数与阻止数、任一节点第一次 / 最近一次访问它的时间
#[derive(Debug, Serialize, FromRow)]
pub struct TargetGroupRow {
    pub dest_host: String,
    pub kind: String,
    pub nodes: i64,
    pub conns: i64,
    pub blocked: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub first_seen: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_seen: Option<OffsetDateTime>,
    /// 符合条件的目标总数（每行都带，接口层取出来）
    #[serde(skip)]
    pub total: i64,
}

/// 「域名与 IP」按目标聚合：在 (节点, 目标) 聚合（`cte`，同 `targets`）上再按目标合并，同时返回目标总数。
/// 来源数各节点分别去重，合并后相加没有意义，不给
pub async fn target_groups(db: &PgPool, f: &Filter, sort: TargetSort, limit: i64, offset: i64) -> sqlx::Result<(Vec<TargetGroupRow>, i64)> {
    let order = match sort {
        TargetSort::Conns => "conns desc, dest_host",
        TargetSort::Last => "last_seen desc nulls last, dest_host",
        TargetSort::First => "first_seen desc nulls last, dest_host",
    };
    let mut a = Args::default();
    let sql = format!(
        "{}, g as (
            select f.dest_host, f.kind, count(*)::bigint nodes, sum(f.conns)::bigint conns, sum(f.blocked)::bigint blocked,
                   min(coalesce(s.first_seen, f.first_ts)) first_seen, max(coalesce(f.last_ts, s.last_seen)) last_seen
            from f join machine m on m.id = f.node_id
                 left join access_target_seen s on s.node_id = f.node_id and s.dest_host = f.dest_host
            group by f.dest_host, f.kind)
         select *, count(*) over ()::bigint total from g order by {order} limit {} offset {}",
        cte(f, &mut a),
        a.add(limit),
        a.add(offset)
    );
    let items: Vec<TargetGroupRow> = sqlx::query_as_with(&sql, a.0).fetch_all(db).await?;
    let total = match items.first() {
        Some(r) => r.total,
        None if offset == 0 => 0,
        // 翻过了最后一页：总数单独数一次
        None => {
            let mut a = Args::default();
            let sql = format!("{} select count(distinct f.dest_host) from f join machine m on m.id = f.node_id", cte(f, &mut a));
            sqlx::query_scalar_with(&sql, a.0).fetch_one(db).await?
        }
    };
    Ok((items, total))
}

#[derive(Debug, Serialize, FromRow)]
pub struct SourceRow {
    pub node_id: Uuid,
    pub name: String,
    pub src_ip: String,
    pub conns: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_ts: Option<OffsetDateTime>,
    /// 来源画像（运营商、城市、接入方式…），由接口层填；查不到为 null
    #[sqlx(skip)]
    pub intel: Option<crate::ipintel::Brief>,
}

/// 按 (节点, 来源 IP) 的连接数 top。search 在这里匹配来源 IP。
/// `conn`：(接入方式, 画像库版本)，去掉画像缓存里（这个库版本）已经查过、接入方式不是它的来源；
/// 缓存里没有的（刚出现、库换了版本）留着，由调用方查过画像后再筛
pub async fn sources(db: &PgPool, f: &Filter, conn: Option<(&str, &str)>, limit: i64) -> sqlx::Result<Vec<SourceRow>> {
    let mut a = Args::default();
    let src = if f.events { "e.src_ip" } else { "s.src_ip" };
    let having = match conn {
        Some((c, ver)) => format!(
            " having not exists (select 1 from ip_intel_cache c where c.ip = {src} and c.db_version = {} and c.conn_type <> {})",
            a.add(ver.to_string()),
            a.add(c.to_string())
        ),
        None => String::new(),
    };
    let sql = if f.events {
        format!(
            "select en.node_id, {NAME} name, host(e.src_ip) src_ip, count(*)::bigint conns, max(e.ts) last_ts
             from access_event e join entry en on en.id = e.entry_id join machine m on m.id = en.node_id left join team t on t.id = m.team_id
             where {} and e.src_ip is not null
             group by en.node_id, m.id, t.name, e.src_ip{having}
             order by conns desc, src_ip limit {}",
            event_where(f, &mut a, true),
            a.add(limit)
        )
    } else {
        format!(
            "select s.node_id, {NAME} name, host(s.src_ip) src_ip, sum(s.conns)::bigint conns, max(s.hour) last_ts
             from access_source_hourly s join machine m on m.id = s.node_id left join team t on t.id = m.team_id
             where {}
             group by s.node_id, m.id, t.name, s.src_ip{having}
             order by conns desc, src_ip limit {}",
            hourly_where(f, &mut a, "s", "host(s.src_ip)"),
            a.add(limit)
        )
    };
    sqlx::query_as_with(&sql, a.0).fetch_all(db).await
}

#[derive(Debug, Serialize, FromRow)]
pub struct EventRow {
    /// 只用于翻页游标
    #[serde(skip)]
    pub id: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub ts: OffsetDateTime,
    pub node_id: Uuid,
    pub name: String,
    pub src_ip: Option<String>,
    pub dest_host: String,
    pub dest_port: Option<i32>,
    pub network: Option<String>,
    pub blocked: bool,
}

/// 翻页游标「ts|id」（对调用方不透明）：同一毫秒的多条记录靠 id 区分，不会被跳过
pub fn event_cursor(e: &EventRow) -> String { format!("{}|{}", fmt_ts(e.ts), e.id) }

pub fn parse_event_cursor(s: &str) -> Option<(OffsetDateTime, i64)> {
    let (ts, id) = s.split_once('|')?;
    Some((OffsetDateTime::parse(ts, &time::format_description::well_known::Rfc3339).ok()?, id.parse().ok()?))
}

/// 原始记录（access_event，按 (ts, id) 倒序）。f.from 是下界，before 是上一页最后一条的 (ts, id)（严格小于）。
pub async fn events(db: &PgPool, f: &Filter, before: Option<(OffsetDateTime, i64)>, limit: i64) -> sqlx::Result<Vec<EventRow>> {
    if f.nodes.is_some() || f.relay.is_some() {
        return events_of_entries(db, f, before, limit).await;
    }
    let mut a = Args::default();
    let mut w = event_where(f, &mut a, false);
    if let Some((ts, id)) = before {
        w += &format!(" and (e.ts, e.id) < ({}, {})", a.add(ts), a.add(id));
    }
    w += &event_kind_action(f, &mut a);
    let sql = format!(
        "select e.id, e.ts, en.node_id, {NAME} name, host(e.src_ip) src_ip, e.dest_host, e.dest_port, e.network, e.blocked
         from access_event e join entry en on en.id = e.entry_id join machine m on m.id = en.node_id left join team t on t.id = m.team_id
         where {w}
         order by e.ts desc, e.id desc limit {}",
        a.add(limit)
    );
    sqlx::query_as_with(&sql, a.0).fetch_all(db).await
}

/// 按节点 / 线路机筛选的原始记录：先取这些入口，逐个入口沿 (entry_id, ts desc) 索引各取前 limit 条，再合并取前 limit 条，
/// 不用把窗口内全部记录按时间扫一遍。排序与游标同 `events`。
async fn events_of_entries(db: &PgPool, f: &Filter, before: Option<(OffsetDateTime, i64)>, limit: i64) -> sqlx::Result<Vec<EventRow>> {
    let mut a = Args::default();
    let mut w = String::from("true");
    if let Some(n) = &f.nodes {
        w += &format!(" and node_id = any({})", a.add(n));
    }
    if let Some(r) = f.relay {
        w += &format!(" and relay_id = {}", a.add(r));
    }
    if let Some(t) = f.team {
        w += &format!(" and node_id in (select id from machine where team_id = {})", a.add(t));
    }
    let ids: Vec<Uuid> = sqlx::query_scalar_with(&format!("select id from entry where {w}"), a.0).fetch_all(db).await?;

    let mut a = Args::default();
    let entries = a.add(&ids);
    let mut w = format!("e.entry_id = x.id and e.ts >= {}", a.add(f.from));
    // 游标条件只在翻页时拼上：ts 上界能进索引条件
    if let Some((ts, id)) = before {
        let ts = a.add(ts);
        w += &format!(" and e.ts <= {ts} and (e.ts, e.id) < ({ts}, {})", a.add(id));
    }
    if let Some(l) = &f.like {
        let l = a.add(l);
        w += &format!(" and (e.dest_host ilike {l} or host(e.src_ip) ilike {l})");
    }
    w += &event_kind_action(f, &mut a);
    let limit = a.add(limit);
    let sql = format!(
        "select e.id, e.ts, en.node_id, {NAME} name, host(e.src_ip) src_ip, e.dest_host, e.dest_port, e.network, e.blocked
         from unnest({entries}::uuid[]) x(id) cross join lateral (
             select e.id, e.entry_id, e.ts, e.src_ip, e.dest_host, e.dest_port, e.network, e.blocked from access_event e
             where {w}
             order by e.ts desc, e.id desc limit {limit}) e
         join entry en on en.id = e.entry_id join machine m on m.id = en.node_id left join team t on t.id = m.team_id
         order by e.ts desc, e.id desc limit {limit}"
    );
    sqlx::query_as_with(&sql, a.0).fetch_all(db).await
}

#[derive(Debug, Serialize, FromRow)]
pub struct NewTargetRow {
    pub node_id: Uuid,
    pub name: String,
    pub dest_host: String,
    pub kind: String,
    #[serde(with = "time::serde::rfc3339")]
    pub first_seen: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen: OffsetDateTime,
    pub total: i64,
}

/// access_target_seen s（连 machine m）上的新目标条件
fn new_where(f: &Filter, a: &mut Args) -> String {
    let mut w = format!("s.first_seen >= {}", a.add(f.from));
    if let Some(n) = &f.nodes {
        w += &format!(" and s.node_id = any({})", a.add(n));
    }
    if let Some(t) = f.team {
        w += &format!(" and m.team_id = {}", a.add(t));
    }
    if let Some(l) = &f.like {
        w += &format!(" and s.dest_host ilike {}", a.add(l));
    }
    if let Some(k) = &f.kind {
        w += &format!(" and access_kind(s.dest_host) = {}", a.add(k));
    }
    w
}

/// 窗口内第一次出现的目标（f.from 取精确的 now - window，不对齐整点）；同时返回符合条件的总数
pub async fn new_targets(db: &PgPool, f: &Filter, limit: i64, offset: i64) -> sqlx::Result<(Vec<NewTargetRow>, i64)> {
    let mut a = Args::default();
    let sql = format!(
        "select s.node_id, {NAME} name, s.dest_host, access_kind(s.dest_host) kind, s.first_seen, s.last_seen, s.total
         from access_target_seen s join machine m on m.id = s.node_id left join team t on t.id = m.team_id
         where {}
         order by s.first_seen desc, s.dest_host, s.node_id limit {} offset {}",
        new_where(f, &mut a),
        a.add(limit),
        a.add(offset)
    );
    let items = sqlx::query_as_with(&sql, a.0).fetch_all(db).await?;
    let mut a = Args::default();
    let sql = format!("select count(*) from access_target_seen s join machine m on m.id = s.node_id where {}", new_where(f, &mut a));
    let total = sqlx::query_scalar_with(&sql, a.0).fetch_one(db).await?;
    Ok((items, total))
}

#[derive(Debug, Serialize, FromRow)]
pub struct TopDomain {
    pub dest_host: String,
    pub kind: String,
    pub conns: i64,
    pub blocked: i64,
    pub sources: i64,
}

#[derive(Debug, Serialize, FromRow)]
pub struct HourPoint {
    #[serde(with = "time::serde::rfc3339")]
    pub hour: OffsetDateTime,
    pub conns: i64,
    pub blocked: i64,
}

#[derive(Debug, Serialize)]
pub struct MachineAccess {
    pub window_start: String,
    pub top_domains: Vec<TopDomain>,
    pub top_sources: Vec<SourceRow>,
    pub recent: Vec<EventRow>,
    pub hourly: Vec<HourPoint>,
}

/// 单台机器的访问概况。线路机只算经它的流量（它名下的入口，含已移除的；同一节点经别的线路机的不算）；节点算它自己全部入口的。
/// `raw_days`：原始记录的保留期，最近记录回看这么久
pub async fn machine_access(db: &PgPool, machine_id: Uuid, window: Window, raw_days: u32) -> Result<MachineAccess, crate::error::ApiError> {
    let kind: String = sqlx::query_scalar("select kind::text from machine where id = $1")
        .bind(machine_id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| crate::error::ApiError::not_found("没有这台机器"))?;
    let relay = kind == "relay";
    let now = OffsetDateTime::now_utc();
    let mut f = Filter::new(window, now);
    if relay {
        f.relay = Some(machine_id);
    } else {
        f.nodes = Some(vec![machine_id]);
    }

    let mut a = Args::default();
    let sql = format!(
        "{} select dest_host, kind, sum(conns)::bigint conns, sum(blocked)::bigint blocked, max(sources)::bigint sources
         from f group by dest_host, kind order by conns desc, dest_host limit 30",
        cte(&f, &mut a)
    );
    let top_domains: Vec<TopDomain> = sqlx::query_as_with(&sql, a.0).fetch_all(db).await?;
    let top_sources = sources(db, &f, None, 20).await?;

    // 最近的原始记录回看整个保留期：逐入口沿 (entry_id, ts desc) 索引各取前 20 条，回看多久都不慢
    let mut ef = Filter { from: now - Duration::days(raw_days as i64), ..Filter::new(Window::H1, now) };
    (ef.nodes, ef.relay) = (f.nodes.clone(), f.relay);
    let recent = events(db, &ef, None, 20).await?;

    // 小时序列：至少 24 小时，缺的小时补 0
    let sw = if window == Window::H1 { Window::H24 } else { window };
    let series_from = floor_hour(now - sw.duration());
    let col = if relay { "relay_id" } else { "node_id" };
    let hourly: Vec<HourPoint> = sqlx::query_as(&format!(
        "select g.h as hour, coalesce(sum(a.conns), 0)::bigint conns, coalesce(sum(a.blocked), 0)::bigint blocked
         from generate_series($1::timestamptz, $2::timestamptz, interval '1 hour') g(h)
         left join access_hourly a on a.hour = g.h and a.{col} = $3
         group by g.h order by g.h"
    ))
    .bind(series_from)
    .bind(floor_hour(now))
    .bind(machine_id)
    .fetch_all(db)
    .await?;

    Ok(MachineAccess { window_start: fmt_ts(f.from), top_domains, top_sources, recent, hourly })
}

pub use crate::meta::fmt_ts;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_escapes() {
        assert_eq!(like_pattern("  "), None);
        assert_eq!(like_pattern("a_b%c").as_deref(), Some("%a\\_b\\%c%"));
        // 开头的「*.」是后缀匹配：同时命中汇总里的「*.edge.net」和原始记录里的「x1.edge.net」
        assert_eq!(like_pattern(" *.edge.net").as_deref(), Some("%.edge.net%"));
        assert_eq!(like_pattern("a*b").as_deref(), Some("%a*b%"));
    }

    #[test]
    fn query_cache_ttl() {
        let c = QueryCache::new(StdDuration::from_millis(60));
        assert!(c.get("a", false).is_none());
        c.put("a".into(), c.gen(), Arc::new(serde_json::json!(1)));
        assert_eq!(c.get("a", false).as_deref(), Some(&serde_json::json!(1)));
        std::thread::sleep(StdDuration::from_millis(80));
        assert!(c.get("a", false).is_none(), "过期不再命中");
        // 汇总后作废：之前放进来的、以及汇总前开始算的结果都不再命中
        let g = c.gen();
        c.put("a".into(), g, Arc::new(serde_json::json!(1)));
        c.invalidate();
        assert!(c.get("a", false).is_none());
        c.put("a".into(), g, Arc::new(serde_json::json!(1)));
        assert!(c.get("a", false).is_none(), "汇总前开始算的结果");
        let off = QueryCache::new(StdDuration::ZERO);
        off.put("a".into(), off.gen(), Arc::new(serde_json::json!(1)));
        assert!(off.get("a", false).is_none(), "ttl 为 0 等于不缓存");
    }

    #[test]
    fn window_parse() {
        assert_eq!(Window::parse("24h"), Some(Window::H24));
        assert!(Window::parse("6h").is_some_and(|w| w.retained(1)));
        assert_eq!(Window::parse("14d"), None);
        // 窗口不能比汇总的保留期长；1h 读原始记录，总能看
        assert!(Window::H1.retained(1) && Window::H24.retained(1) && Window::D7.retained(7) && Window::D30.retained(30));
        assert!(!Window::D7.retained(6) && !Window::D30.retained(29));
    }

    #[test]
    fn whole_days_of_window() {
        let at = |s: &str| OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).unwrap();
        let now = at("2026-10-23T11:20:00Z");
        // 7 天：起点之后的第一个零点到今天零点
        assert_eq!(whole_days(at("2026-10-16T11:00:00Z"), now), Some((at("2026-10-17T00:00:00Z"), at("2026-10-23T00:00:00Z"))));
        // 起点正好是零点：那天整天算
        assert_eq!(whole_days(at("2026-10-16T00:00:00Z"), now), Some((at("2026-10-16T00:00:00Z"), at("2026-10-23T00:00:00Z"))));
        // 24 小时一般凑不出整天
        assert_eq!(whole_days(at("2026-10-22T11:00:00Z"), now), None);
        assert_eq!(floor_day(now), at("2026-10-23T00:00:00Z"));
    }

    #[test]
    fn hour_floor() {
        let t = OffsetDateTime::from_unix_timestamp(1_700_003_599).unwrap();
        assert_eq!(floor_hour(t).unix_timestamp(), 1_700_002_800);
        assert_eq!(floor_hour(t).unix_timestamp() % 3600, 0);
    }
}
