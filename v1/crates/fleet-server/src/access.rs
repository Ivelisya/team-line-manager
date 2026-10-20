//! 访问监控：把 access_event 汇总成小时表（每 5 分钟重算最近两小时，幂等），以及各个查询。
//!
//! - 1h 窗口直接查 access_event（原始记录，保留 access_days 天）；24h 及以上查小时汇总表（保留 90 天）。
//! - 随机子域名合并在库里的 `access_collapse()`（见迁移 0006），汇总和 1h 查询共用同一份规则。
//! - 目标按 (节点, 目标) 聚合。「来源数」在小时表里是各小时去重数的最大值（跨小时无法精确去重，偏小的近似）。

use crate::state::AppState;
use serde::Serialize;
use sqlx::{FromRow, PgPool};
use std::time::Duration as StdDuration;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

/// 汇总用的咨询锁：同一时刻只让一个汇总在跑（增量累计 total 依赖这点）
const LOCK_KEY: i64 = 0x6163_6365_7373;
/// 小时汇总表保留天数
pub const HOURLY_DAYS: i64 = 90;

pub fn floor_hour(t: OffsetDateTime) -> OffsetDateTime {
    let s = t.unix_timestamp();
    OffsetDateTime::from_unix_timestamp(s - s.rem_euclid(3600)).unwrap_or(t)
}

// ── 汇总 ─────────────────────────────────────────────────────────────────────

/// 重算 [hour, hour+1h) 的汇总。$1 = hour, $2 = hour + 1h。
/// 先按 (节点, 目标, 来源) 分组，再只对去重后的目标做合并，避免对每条记录调用 plpgsql。
/// target_seen.total 按「本次重算值 - 旧值」累加，所以重复执行不会重复计数。
const ROLLUP_HOURLY: &str = "
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
    returning 1
), nw as (
    select node_id, dest_host, sum(conns)::bigint c, min(f) f, max(l) l from agg group by 1, 2
)
insert into access_target_seen (node_id, dest_host, first_seen, last_seen, total)
select n.node_id, n.dest_host, n.f, n.l, greatest(n.c - coalesce(o.c, 0), 0)
from nw n left join old o on o.node_id = n.node_id and o.dest_host = n.dest_host
on conflict (node_id, dest_host) do update
  set first_seen = least(access_target_seen.first_seen, excluded.first_seen),
      last_seen  = greatest(access_target_seen.last_seen, excluded.last_seen),
      total      = greatest(access_target_seen.total + excluded.total, 0)";

const ROLLUP_SOURCES: &str = "
insert into access_source_hourly (node_id, hour, src_ip, conns)
select en.node_id, $1, e.src_ip, count(*)::int
from access_event e join entry en on en.id = e.entry_id
where e.ts >= $1 and e.ts < $2 and e.src_ip is not null
group by en.node_id, e.src_ip
on conflict (node_id, hour, src_ip) do update set conns = excluded.conns";

/// 重算一个整点小时（UTC 对齐）
pub async fn rollup_hour(db: &PgPool, hour: OffsetDateTime) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("select pg_advisory_xact_lock($1)").bind(LOCK_KEY).execute(&mut *tx).await?;
    let end = hour + Duration::hours(1);
    sqlx::query(ROLLUP_HOURLY).bind(hour).bind(end).execute(&mut *tx).await?;
    sqlx::query(ROLLUP_SOURCES).bind(hour).bind(end).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// 重算上一个整点小时和当前小时
pub async fn rollup_recent(db: &PgPool) -> anyhow::Result<()> {
    let now = floor_hour(OffsetDateTime::now_utc());
    rollup_hour(db, now - Duration::hours(1)).await?;
    rollup_hour(db, now).await
}

/// 补算 [from, 现在] 的所有小时（逐小时，内存有界）。
pub async fn rollup_since(db: &PgPool, from: OffsetDateTime) -> anyhow::Result<()> {
    let end = floor_hour(OffsetDateTime::now_utc());
    let mut h = floor_hour(from);
    while h <= end {
        rollup_hour(db, h).await?;
        h += Duration::hours(1);
    }
    Ok(())
}

/// 启动补算：从汇总表里最新的小时算起（宕机期间的缺口），表是空的就补整个 access_event 保留期。
pub async fn backfill(db: &PgPool, access_days: u32) -> anyhow::Result<()> {
    let floor = OffsetDateTime::now_utc() - Duration::days(access_days as i64);
    let last: Option<OffsetDateTime> = sqlx::query_scalar("select max(hour) from access_hourly").fetch_one(db).await?;
    let from = last.map(|l| l.max(floor)).unwrap_or(floor);
    rollup_since(db, from).await
}

/// 清理 90 天前的汇总
pub async fn prune(db: &PgPool) -> anyhow::Result<()> {
    let cutoff = OffsetDateTime::now_utc() - Duration::days(HOURLY_DAYS);
    for s in [
        "delete from access_hourly where hour < $1",
        "delete from access_source_hourly where hour < $1",
        "delete from access_target_seen where last_seen < $1",
    ] {
        sqlx::query(s).bind(cutoff).execute(db).await?;
    }
    Ok(())
}

/// 后台循环：启动补算一次，之后每 5 分钟重算最近两小时并清理过期汇总。
pub async fn run_forever(st: AppState) {
    if let Err(e) = backfill(&st.db, st.cfg.retention.access_days).await {
        tracing::error!("访问汇总补算失败：{e:#}");
    }
    let mut t = tokio::time::interval(StdDuration::from_secs(300));
    let mut n = 0u32;
    loop {
        t.tick().await;
        if let Err(e) = rollup_recent(&st.db).await {
            tracing::error!("访问汇总失败：{e:#}");
        }
        if n % 12 == 0 {
            if let Err(e) = prune(&st.db).await {
                tracing::error!("访问汇总清理失败：{e:#}");
            }
        }
        n = n.wrapping_add(1);
    }
}

// ── 查询 ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    H1,
    H24,
    D7,
    D30,
}

impl Window {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "1h" => Self::H1,
            "24h" => Self::H24,
            "7d" => Self::D7,
            "30d" => Self::D30,
            _ => return None,
        })
    }
    pub fn duration(self) -> Duration {
        match self {
            Self::H1 => Duration::hours(1),
            Self::H24 => Duration::hours(24),
            Self::D7 => Duration::days(7),
            Self::D30 => Duration::days(30),
        }
    }
}

/// 一次查询的筛选条件。绑定顺序固定：$1 from、$2 team、$3 nodes、$4 like、$5 kind、$6 action。
#[derive(Debug, Clone)]
pub struct Filter {
    /// 窗口起点。events=false 时已对齐到整点小时
    pub from: OffsetDateTime,
    /// true：读 access_event（1h 窗口）；false：读小时汇总表
    pub events: bool,
    pub team: Option<Uuid>,
    pub nodes: Option<Vec<Uuid>>,
    /// 已加好 % 并转义的 ilike 模式
    pub like: Option<String>,
    /// domain | ip
    pub kind: Option<String>,
    /// all | blocked | routed
    pub action: &'static str,
}

impl Filter {
    pub fn new(window: Window, now: OffsetDateTime) -> Self {
        let events = window == Window::H1;
        let raw = now - window.duration();
        Self { from: if events { raw } else { floor_hour(raw) }, events, team: None, nodes: None, like: None, kind: None, action: "all" }
    }
}

/// 用户输入 → ilike 模式
pub fn like_pattern(s: &str) -> Option<String> {
    let s = s.trim();
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

macro_rules! base {
    ($q:expr, $f:expr) => {
        $q.bind($f.from).bind($f.team).bind(&$f.nodes).bind(&$f.like)
    };
}
macro_rules! full {
    ($q:expr, $f:expr) => {
        base!($q, $f).bind(&$f.kind).bind($f.action)
    };
}

/// 机器显示名（与 names::display_name 一致；编号不足两位补零）
const NAME: &str = "coalesce(case when m.kind = 'relay' then m.label
    else t.name || '-' || case when m.number < 10 then '0' || m.number::text else m.number::text end end, '未命名')";

/// 小时表聚合：(节点, 目标) → 连接数、阻止数、来源数
const AGG_HOURLY: &str = "agg as (
    select h.node_id, h.dest_host, h.kind, sum(h.conns)::bigint conns, sum(h.blocked)::bigint blocked, max(h.sources)::bigint sources,
           null::timestamptz first_ts, null::timestamptz last_ts
    from access_hourly h
    where h.hour >= $1 and ($3::uuid[] is null or h.node_id = any($3))
      and ($2::uuid is null or h.node_id in (select id from machine where team_id = $2))
      and ($4::text is null or h.dest_host ilike $4)
    group by h.node_id, h.dest_host, h.kind)";

/// 原始记录聚合（1h 窗口）：同样的输出列
const AGG_EVENTS: &str = "rawg as (
    select en.node_id, lower(rtrim(e.dest_host, '.')) h, e.src_ip,
           count(*) c, count(*) filter (where e.blocked) b, min(e.ts) f, max(e.ts) l
    from access_event e join entry en on en.id = e.entry_id
    where e.ts >= $1 and ($3::uuid[] is null or en.node_id = any($3))
      and ($2::uuid is null or en.node_id in (select id from machine where team_id = $2))
      and ($4::text is null or e.dest_host ilike $4 or host(e.src_ip) ilike $4)
    group by 1, 2, 3
), hm as (
    select h, access_collapse(h) c from (select distinct h from rawg) d
), agg as (
    select r.node_id, hm.c dest_host, access_kind(hm.c) kind, sum(r.c)::bigint conns, sum(r.b)::bigint blocked,
           count(distinct r.src_ip)::bigint sources, min(r.f) first_ts, max(r.l) last_ts
    from rawg r join hm on hm.h = r.h
    group by r.node_id, hm.c)";

/// 目标类型与处理结果筛选；blocked 视图里连接数就是被阻止数，routed 视图里去掉被阻止的
const FILTERED: &str = "f as (
    select node_id, dest_host, kind,
           case $6::text when 'blocked' then blocked when 'routed' then conns - blocked else conns end conns,
           case $6::text when 'routed' then 0 else blocked end blocked,
           sources, first_ts, last_ts
    from agg
    where ($5::text is null or kind = $5)
      and case $6::text when 'blocked' then blocked > 0 when 'routed' then conns > blocked else true end)";

fn cte(f: &Filter) -> String {
    format!("with {}, {FILTERED}", if f.events { AGG_EVENTS } else { AGG_HOURLY })
}

#[derive(Debug, Serialize, FromRow)]
pub struct NodeRow {
    pub node_id: Uuid,
    pub name: String,
    pub team: Option<String>,
    pub conns: i64,
    pub targets: i64,
    pub domains: i64,
    #[sqlx(skip)]
    pub sources: i64,
    pub blocked: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_ts: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize)]
pub struct Kpi {
    pub active_nodes: i64,
    pub conns: i64,
    pub targets: i64,
    pub blocked: i64,
}

/// 节点概览 + 四张 KPI。访问目标是各节点分别去重后相加。
pub async fn summary(db: &PgPool, f: &Filter) -> sqlx::Result<(Kpi, Vec<NodeRow>)> {
    let sql = format!(
        "{} select f.node_id, {NAME} name, t.name team, sum(f.conns)::bigint conns, count(*)::bigint targets,
                count(*) filter (where f.kind = 'domain')::bigint domains, sum(f.blocked)::bigint blocked,
                max(coalesce(f.last_ts, s.last_seen)) last_ts
         from f join machine m on m.id = f.node_id left join team t on t.id = m.team_id
              left join access_target_seen s on s.node_id = f.node_id and s.dest_host = f.dest_host
         group by f.node_id, m.kind, m.label, m.number, t.name
         order by conns desc, name",
        cte(f)
    );
    let mut nodes: Vec<NodeRow> = full!(sqlx::query_as(&sql), f).fetch_all(db).await?;

    let src_sql = if f.events {
        "select en.node_id, count(distinct e.src_ip)::bigint from access_event e join entry en on en.id = e.entry_id
         where e.ts >= $1 and ($3::uuid[] is null or en.node_id = any($3))
           and ($2::uuid is null or en.node_id in (select id from machine where team_id = $2))
         group by 1"
    } else {
        "select node_id, count(distinct src_ip)::bigint from access_source_hourly
         where hour >= $1 and ($3::uuid[] is null or node_id = any($3))
           and ($2::uuid is null or node_id in (select id from machine where team_id = $2))
         group by 1"
    };
    let srcs: Vec<(Uuid, i64)> = sqlx::query_as(src_sql).bind(f.from).bind(f.team).bind(&f.nodes).fetch_all(db).await?;
    let srcs: std::collections::HashMap<Uuid, i64> = srcs.into_iter().collect();
    for n in &mut nodes {
        n.sources = srcs.get(&n.node_id).copied().unwrap_or(0);
    }
    let kpi = Kpi {
        active_nodes: nodes.len() as i64,
        conns: nodes.iter().map(|n| n.conns).sum(),
        targets: nodes.iter().map(|n| n.targets).sum(),
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
    let sql = format!(
        "{} select f.node_id, {NAME} name, f.dest_host, f.kind, f.conns, f.sources, f.blocked,
                coalesce(s.first_seen, f.first_ts) first_seen, coalesce(f.last_ts, s.last_seen) last_seen
         from f join machine m on m.id = f.node_id left join team t on t.id = m.team_id
              left join access_target_seen s on s.node_id = f.node_id and s.dest_host = f.dest_host
         order by {order} limit $7 offset $8",
        cte(f)
    );
    full!(sqlx::query_as(&sql), f).bind(limit).bind(offset).fetch_all(db).await
}

#[derive(Debug, Serialize, FromRow)]
pub struct SourceRow {
    pub node_id: Uuid,
    pub name: String,
    pub src_ip: String,
    pub conns: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_ts: Option<OffsetDateTime>,
}

/// 按 (节点, 来源 IP) 的连接数 top。search 在这里匹配来源 IP。
pub async fn sources(db: &PgPool, f: &Filter, limit: i64) -> sqlx::Result<Vec<SourceRow>> {
    let sql = if f.events {
        format!(
            "select en.node_id, {NAME} name, host(e.src_ip) src_ip, count(*)::bigint conns, max(e.ts) last_ts
             from access_event e join entry en on en.id = e.entry_id join machine m on m.id = en.node_id left join team t on t.id = m.team_id
             where e.ts >= $1 and e.src_ip is not null and ($3::uuid[] is null or en.node_id = any($3))
               and ($2::uuid is null or m.team_id = $2) and ($4::text is null or host(e.src_ip) ilike $4)
             group by en.node_id, m.kind, m.label, m.number, t.name, e.src_ip
             order by conns desc, src_ip limit $5"
        )
    } else {
        format!(
            "select s.node_id, {NAME} name, host(s.src_ip) src_ip, sum(s.conns)::bigint conns, max(s.hour) last_ts
             from access_source_hourly s join machine m on m.id = s.node_id left join team t on t.id = m.team_id
             where s.hour >= $1 and ($3::uuid[] is null or s.node_id = any($3))
               and ($2::uuid is null or m.team_id = $2) and ($4::text is null or host(s.src_ip) ilike $4)
             group by s.node_id, m.kind, m.label, m.number, t.name, s.src_ip
             order by conns desc, src_ip limit $5"
        )
    };
    base!(sqlx::query_as(&sql), f).bind(limit).fetch_all(db).await
}

#[derive(Debug, Serialize, FromRow)]
pub struct EventRow {
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

/// 原始记录（access_event，按时间倒序）。f.from 是下界，before 是游标（严格小于）。
pub async fn events(db: &PgPool, f: &Filter, before: Option<OffsetDateTime>, limit: i64) -> sqlx::Result<Vec<EventRow>> {
    let sql = format!(
        "select e.ts, en.node_id, {NAME} name, host(e.src_ip) src_ip, e.dest_host, e.dest_port, e.network, e.blocked
         from access_event e join entry en on en.id = e.entry_id join machine m on m.id = en.node_id left join team t on t.id = m.team_id
         where e.ts >= $1 and ($7::timestamptz is null or e.ts < $7)
           and ($3::uuid[] is null or en.node_id = any($3)) and ($2::uuid is null or m.team_id = $2)
           and ($4::text is null or e.dest_host ilike $4 or host(e.src_ip) ilike $4)
           and ($5::text is null or access_kind(lower(rtrim(e.dest_host, '.'))) = $5)
           and case $6::text when 'blocked' then e.blocked when 'routed' then not e.blocked else true end
         order by e.ts desc limit $8"
    );
    full!(sqlx::query_as(&sql), f).bind(before).bind(limit).fetch_all(db).await
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

/// 窗口内第一次出现的目标（f.from 取精确的 now - window，不对齐整点）
pub async fn new_targets(db: &PgPool, f: &Filter, limit: i64, offset: i64) -> sqlx::Result<Vec<NewTargetRow>> {
    let sql = format!(
        "select s.node_id, {NAME} name, s.dest_host, access_kind(s.dest_host) kind, s.first_seen, s.last_seen, s.total
         from access_target_seen s join machine m on m.id = s.node_id left join team t on t.id = m.team_id
         where s.first_seen >= $1 and ($3::uuid[] is null or s.node_id = any($3)) and ($2::uuid is null or m.team_id = $2)
           and ($4::text is null or s.dest_host ilike $4) and ($5::text is null or access_kind(s.dest_host) = $5)
         order by s.first_seen desc, s.dest_host, s.node_id limit $6 offset $7"
    );
    sqlx::query_as(&sql).bind(f.from).bind(f.team).bind(&f.nodes).bind(&f.like).bind(&f.kind).bind(limit).bind(offset).fetch_all(db).await
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

/// 单台机器的访问概况。线路机取它名下所有入口对应的节点；节点就是它自己。
pub async fn machine_access(db: &PgPool, machine_id: Uuid, window: Window) -> Result<MachineAccess, crate::error::ApiError> {
    let kind: String = sqlx::query_scalar("select kind::text from machine where id = $1")
        .bind(machine_id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| crate::error::ApiError::not_found("没有这台机器"))?;
    let ids: Vec<Uuid> = if kind == "relay" {
        sqlx::query_scalar("select distinct node_id from entry where relay_id = $1").bind(machine_id).fetch_all(db).await?
    } else {
        vec![machine_id]
    };
    let now = OffsetDateTime::now_utc();
    let mut f = Filter::new(window, now);
    f.nodes = Some(ids.clone());

    let sql = format!(
        "{} select dest_host, kind, sum(conns)::bigint conns, sum(blocked)::bigint blocked, max(sources)::bigint sources
         from f group by dest_host, kind order by conns desc, dest_host limit 30",
        cte(&f)
    );
    let top_domains: Vec<TopDomain> = full!(sqlx::query_as(&sql), f).fetch_all(db).await?;
    let top_sources = sources(db, &f, 20).await?;

    let mut ef = Filter::new(Window::D7, now);
    ef.nodes = Some(ids.clone());
    let recent = events(db, &ef, None, 20).await?;

    // 小时序列：至少 24 小时，缺的小时补 0
    let sw = if window == Window::H1 { Window::H24 } else { window };
    let series_from = floor_hour(now - sw.duration());
    let hourly: Vec<HourPoint> = sqlx::query_as(
        "select g.h as hour, coalesce(sum(a.conns), 0)::bigint conns, coalesce(sum(a.blocked), 0)::bigint blocked
         from generate_series($1::timestamptz, $2::timestamptz, interval '1 hour') g(h)
         left join access_hourly a on a.hour = g.h and a.node_id = any($3)
         group by g.h order by g.h",
    )
    .bind(series_from)
    .bind(floor_hour(now))
    .bind(&ids)
    .fetch_all(db)
    .await?;

    Ok(MachineAccess { window_start: fmt_ts(f.from), top_domains, top_sources, recent, hourly })
}

pub fn fmt_ts(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_escapes() {
        assert_eq!(like_pattern("  "), None);
        assert_eq!(like_pattern("a_b%c").as_deref(), Some("%a\\_b\\%c%"));
    }

    #[test]
    fn window_parse() {
        assert_eq!(Window::parse("24h"), Some(Window::H24));
        assert_eq!(Window::parse("14d"), None);
    }

    #[test]
    fn hour_floor() {
        let t = OffsetDateTime::from_unix_timestamp(1_700_003_599).unwrap();
        assert_eq!(floor_hour(t).unix_timestamp(), 1_700_002_800);
        assert_eq!(floor_hour(t).unix_timestamp() % 3600, 0);
    }
}
