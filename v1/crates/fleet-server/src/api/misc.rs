//! 总览、共用元数据、探测心跳条。告警与静默见 incidents.rs，操作记录见 audit.rs，实时流见 stream.rs
use super::ok;
use crate::auth::Actor;
use crate::error::{ApiError, ApiQuery, ApiResult};
use crate::state::AppState;
use crate::views;
use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::OffsetDateTime;
use uuid::Uuid;

pub async fn overview(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(ok(views::overview(&st).await?))
}

/// 共用的枚举中文名、常量、阈值、默认值与组织时区（见 meta.rs）：网页启动时取一次，fleetctl 每次命令需要时取一次。
/// 另带中控公钥全文 `ssh_public_key`（读不到为 null）：上架页「复制中控公钥」，供应商面板录入它之后上架可以不填 root 密码
pub async fn meta(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut v = crate::meta::to_json();
    v["ssh_public_key"] = json!(crate::ssh::public_key(&st.cfg.ssh_key).await);
    Ok(ok(v))
}

#[derive(Deserialize)]
pub struct DailyQ {
    /// 近几天（含今天），默认 30，最多 92
    days: Option<i64>,
    /// 团队 id 或名称
    team: Option<String>,
    /// 机器 id 或显示名：节点只看它自己的入口，线路机看经过它的全部入口
    machine: Option<String>,
}

/// 每日流量（口径见 `crate::traffic`）
pub async fn traffic_daily(_a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<DailyQ>) -> ApiResult<Json<Value>> {
    Ok(ok(crate::traffic::daily(&st, q.days, q.team.as_deref(), q.machine.as_deref()).await?))
}

#[derive(Deserialize)]
pub struct SeriesQ {
    minutes: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
struct RatePoint {
    #[serde(with = "time::serde::rfc3339")]
    ts: OffsetDateTime,
    net_rx_bps: f64,
    net_tx_bps: f64,
}

/// 总览网速曲线：最近 N 个结束满 20 s 的分钟，每分钟一个点，旧→新。agent 每 10 s 上报一次，刚结束的那一分钟还缺最后一两份，算出来偏小，不给。
/// 每台 active 机器按主机时序同样的算法（每分钟取计数器最大值，与上一个有数据的分钟相减 ×8 / 秒数，计数器回绕的那一段不计）再求和；
/// 逐台走 (machine_id, ts desc) 索引。
pub async fn overview_series(_a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<SeriesQ>) -> ApiResult<Json<Value>> {
    let n = q.minutes.unwrap_or(30);
    if !(1..=60).contains(&n) {
        return Err(ApiError::bad_request("minutes 只能是 1–60"));
    }
    let v: Vec<RatePoint> = sqlx::query_as(
        "with w as (select e - make_interval(mins => $1::int) s, e from (select date_trunc('minute', now() - interval '20 seconds') e) z),
         b as (
           select m.id, x.t, x.rx, x.tx from w cross join machine m cross join lateral (
             select date_trunc('minute', ts) t, max(net_rx_bytes) rx, max(net_tx_bytes) tx from host_metric
             where machine_id = m.id and ts >= w.s - interval '1 minute' and ts < w.e group by 1) x
           where m.lifecycle = 'active'),
         r as (
           select t, case when rx >= prx then (rx - prx) * 8.0 / nullif(extract(epoch from t - pt), 0) end rx_bps,
                     case when tx >= ptx then (tx - ptx) * 8.0 / nullif(extract(epoch from t - pt), 0) end tx_bps
           from (select *, lag(rx) over p prx, lag(tx) over p ptx, lag(t) over p pt from b window p as (partition by id order by t)) y)
         select g.t ts, coalesce(sum(r.rx_bps), 0)::float8 net_rx_bps, coalesce(sum(r.tx_bps), 0)::float8 net_tx_bps
         from w cross join generate_series(w.s, w.e - interval '1 minute', interval '1 minute') g(t) left join r on r.t = g.t
         group by g.t order by g.t",
    )
    .bind(n)
    .fetch_all(&st.db)
    .await?;
    Ok(ok(json!({"points": v})))
}

/// 心跳条的一格：(次数, 成功次数, 延迟中位数, SYN 重发过的次数)
type Bucket = (i64, i64, Option<f64>, i64);

/// 探测心跳条：每台在用机器按墙钟 3 分钟整格（date_bin 对齐 epoch）共 20 格，旧→新，最后一格是当前这格（还没走完）；
/// 每格 [次数, 成功次数, 成功的延迟中位数 ms 或 null, 建连时 SYN 重发过的次数]；这 20 格里没有数据的机器不出现。
/// - 节点：线路机 → 节点的 TCP 探测（线路机的 agent 做），汇总它的入口；穿透入口线路机不连节点，用经入口的出口探测（只看通不通，延迟为 null）；
/// - 线路机：主探测来源（中控或探测点，见 probe_points/；GET /settings/reach-source 说是哪一路）→ 线路机的连通性（reach.rs）。
///   中控只判通不通（延迟为 null）；探测点另记建连的往返时间，延迟是它的中位数。重发恒为 0。主来源离线时是代替它的中控的，标灰时没有线路机。
///   不再汇总它上面的入口：入口通不通是各节点的事，19 条入口合成一格时 114 次里断 2 次就黄，线路机自己好好的也一片黄。
///
/// 只看未移除或在窗口内才移除的入口：更早移除的在窗口里不会有探测，不必逐个去探测表里找。
/// 节点在不在用按入口连（几百行），不按探测行连：当天分区的统计还没跟上「最近一小时」时，探测行数会被估成个位数，
/// 按探测行连机器表的计划会在 30 ms 与 400+ ms 之间跳；
/// 所以在用的入口先物化成 ee（机器过滤落在入口这几百行上，不落在 2 万多条探测行上；实验室 72 ms → 47 ms）
pub async fn probe_beats(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    const BUCKETS: usize = 20;
    let src = crate::probe_points::effective(&st, &st.db).await?;
    let rows: Vec<(Uuid, i32, i64, i64, Option<f64>, i64)> = sqlx::query_as(&format!(
        "with ee as materialized (select e0.* from entry e0 where e0.node_id in (select id from machine where lifecycle <> 'retired')),
         w as (select s, s + interval '60 minutes' e from (select date_bin('3 minutes', now(), timestamptz 'epoch') - interval '57 minutes' s) z),
         p as (
           select e.node_id m, floor(extract(epoch from x.ts - w.s) / 180)::int i, x.ok, case when e.mode = 'reverse' then null else x.latency_ms end, x.retrans
           from w cross join ee e
           cross join lateral (
             select ts, ok, latency_ms, retrans from probe_result
             where entry_id = e.id and kind = case when e.mode = 'reverse' then 'egress' else 'tcp' end and ts >= w.s and ts < w.e) x
           where e.lifecycle <> 'removed' or e.removed_at >= w.s
           union all
           select r.relay_id, floor(extract(epoch from r.ts - w.s) / 180)::int, r.ok, r.rtt_ms, null::smallint
           from w cross join relay_reach r join machine mr on mr.id = r.relay_id
           where r.ts >= w.s and r.ts < w.e and {src} and mr.lifecycle <> 'retired')
         select m, i, count(*), count(*) filter (where ok),
                (percentile_cont(0.5) within group (order by latency_ms) filter (where ok))::float8, count(*) filter (where retrans > 0)
         from p group by m, i",
        src = src.cond("r")
    ))
    .fetch_all(&st.db)
    .await?;
    let mut out: std::collections::HashMap<Uuid, Vec<Bucket>> = std::collections::HashMap::new();
    for (m, i, n, okn, ms, re) in rows {
        out.entry(m).or_insert_with(|| vec![(0, 0, None, 0); BUCKETS])[i as usize] = (n, okn, ms, re);
    }
    Ok(ok(out))
}
