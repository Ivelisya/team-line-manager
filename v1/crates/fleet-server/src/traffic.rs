//! 每日流量（`GET /traffic/daily?days=&team=&machine=`）：按组织时区切成自然日，每天全部、每个节点、每台线路机的流量。今天算到目前为止。
//!
//! 口径同机器视图的今天 / 本月（views.rs `flows`）：
//! - 业务流量（下载 / 上传）来自入口小时表 `entry_traffic_hourly`。节点 = 以它为节点的全部入口之和，含已移除的入口
//!   （换线前在旧线路机上跑的那段仍算这个节点）；线路机的「经过它的业务流量」= 挂在它上面的入口（换线前的量留在旧线路机上）。
//!   直出入口的节点就是线路机自己，单列一行「线路机-直出」（id 是线路机的）。家宽代理、同机 IP 节点有自己的入口，各算各的；
//!   全部合计 = 各行之和，每个字节只算一次。窗口内有流量的已退役机器照列，lifecycle 照实给。
//! - 网卡（下行 / 上行）来自主机小时表 `host_metric_hourly`，有自己 agent 的机器都有（线路机、VPS 节点；家宽代理、同机 IP 节点没有，同额度 views.rs `load`）。
//!   计费量按生效的规则（订阅单独设置的，或供应商的）取小 / 取大 / 相加 / 只算出站 / 只算入站（没有供应商取大）：每天一格按当天的下行、上行算，区间合计先求下行之和、上行之和再套规则（同额度已用）。
//!   线路机在 `relays`；VPS 节点的网卡跟在节点行上（`rule` / `rx` / `tx` / `billed` / `nic_daily`），窗口内没有读数的为 null。
//! - 小时表按 UTC 整点分桶，一桶按它的起点落在组织时区的哪天归哪天（半小时时区至多差 30 分钟，同 `flows`）。
//!
//! 成本：节点的 `monthly_usd_cents` = 在用（active）时订阅生效价按周期折成每月再折 USD（同总览 / 团队的 `monthly_usd`，同机 IP 节点各按自己的订阅，没有订阅或没汇率为 null，直出没有月费）；
//! `usd_per_gb` = 月费 ÷ 按区间日均折成的 30 天业务流量（GB 取十进制，同流量额度）。团队的 `teams` 不受团队 / 机器过滤影响，固定看近 30 天。
//! 宿主机上同机 IP 节点的入口流量同时算到宿主机上（`Usage` 的承载量）：成本（`usd_per_gb`）、闲置、待付清单、月报「每 GB 最贵」按它算，节点行里的下载 / 上传、合计、团队的近 30 天流量仍只算各自的入口；
//! 团队成本里没订阅的同机 IP 节点（钱在宿主机订阅上）不算「没价格」。
//! 闲置：在用、上架满 `IDLE_DAYS` 天、近 `IDLE_DAYS` 天（含今天）日均业务流量低于 `IDLE_GB_PER_DAY` GB；阈值只在这里定义，接口里带 `idle` 标记，
//! 待付清单（api/billing.rs）用同一个判定。窗口至少取 `COST_DAYS` 天（成本、闲置要用），展示的是最后 `days` 天。
//!
//! 两条按 (节点, 线路机, 天) / (机器, 天) 分组的聚合各扫一遍窗口内的小时行（走 hour 索引，0015 / 0043；先按入口 × 天聚合再连入口表，
//! bigint 直接 sum（库里按 int128 累加，先转 numeric 慢一倍）），结果按汇总轮次缓存
//! （`LiveCache.daily`，小时汇总每 5 分钟一轮，跑完即作废，所以今天的数据和机器视图一样新）；团队、机器过滤在内存里做。

use crate::error::{ApiError, ApiResult};
use crate::names::direct_name;
use crate::state::AppState;
use crate::views::{self, Ref};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use time::Date;
use uuid::Uuid;

/// 最多看多少天
pub const MAX_DAYS: i64 = 92;
/// 闲置判定：看近几天（含今天）的日均业务流量，也是上架至少要满的天数（不满不判）
pub const IDLE_DAYS: i64 = 7;
/// 闲置判定：近 `IDLE_DAYS` 天日均业务流量低于它（GB，十进制）
pub const IDLE_GB_PER_DAY: f64 = 0.5;
/// 成本口径的天数：团队和待付清单看近 30 天，`usd_per_gb` 折成 30 天
pub const COST_DAYS: i64 = 30;
const GB: f64 = 1e9;

/// 每 GB 业务流量花多少 USD：月费 ÷ 按 `days` 天的量折成的 30 天流量；没有月费或没有流量为 None
pub(crate) fn usd_per_gb(monthly_usd_cents: Option<i64>, bytes: i64, days: i64) -> Option<f64> {
    let gb30 = bytes as f64 / days as f64 * COST_DAYS as f64 / GB;
    if gb30 > 0.0 { monthly_usd_cents.map(|c| c as f64 / 100.0 / gb30) } else { None }
}

/// 闲置：`sum7` = 近 `IDLE_DAYS` 天业务流量合计，`aged` = 上架已满 `IDLE_DAYS` 天；调用方先排除不在用的
pub(crate) fn is_idle(sum7: i64, aged: bool) -> bool { aged && ((sum7 as f64) / IDLE_DAYS as f64) < IDLE_GB_PER_DAY * GB }

/// 一台机器的业务流量：近 `COST_DAYS` 天、近 `IDLE_DAYS` 天的合计（下载加上传）。
/// 宿主机的 `bytes30` / `sum7` 含它上面同机 IP 节点的入口流量（成本、闲置的口径：钱在宿主机订阅上，客户流量走的是 IP 节点，不算上宿主机会被当成没人用）；
/// `own30` 只有这台机器自己的入口（团队合计用，宿主机与 IP 节点常在同一团队，不能一个字节算两遍）
#[derive(Clone, Copy, Default)]
pub(crate) struct Usage {
    pub bytes30: i64,
    pub sum7: i64,
    pub own30: i64,
}

/// 每台机器的 `Usage`：节点 = 以它为节点的入口；线路机 = 挂在它上面的入口（含直出）；同机 IP 节点（`hosts`：IP 节点 → 宿主机）的入口另记到宿主机上。窗口至少 30 天。
/// 不论 IP 节点有没有自己的订阅都记到宿主机：和流量额度同口径（宿主机的网卡流量含上面所有 IP 节点），IP 节点自己的订阅另按它自己的入口算
pub(crate) fn usage_of(raw: &Raw, hosts: &HashMap<Uuid, Uuid>) -> HashMap<Uuid, Usage> {
    let (dates, biz, _) = &**raw;
    let at = |n: i64| dates[dates.len().saturating_sub(n as usize)];
    let (d30, d7) = (at(COST_DAYS), at(IDLE_DAYS));
    let mut out: HashMap<Uuid, Usage> = HashMap::new();
    for &(node, relay, d, down, up) in biz {
        if d < d30 {
            continue;
        }
        let b = down.saturating_add(up);
        let mut ids = if node == relay { vec![relay] } else { vec![node, relay] };
        let own = ids.len();
        let host = hosts.get(&node).filter(|h| !ids.contains(h)).copied();
        ids.extend(host);
        for (i, id) in ids.into_iter().enumerate() {
            let u = out.entry(id).or_default();
            u.bytes30 = u.bytes30.saturating_add(b);
            if i < own {
                u.own30 = u.own30.saturating_add(b);
            }
            if d >= d7 {
                u.sum7 = u.sum7.saturating_add(b);
            }
        }
    }
    out
}

/// 同机 IP 节点 → 宿主机（含已退役的：窗口内它们的流量还在）
pub(crate) async fn ip_hosts(db: &sqlx::PgPool) -> sqlx::Result<HashMap<Uuid, Uuid>> {
    Ok(sqlx::query_as::<_, (Uuid, Uuid)>("select id, host_id from machine where host_id is not null").fetch_all(db).await?.into_iter().collect())
}

/// 近 30 天每台机器的业务流量（待付清单用）
pub(crate) async fn usage(st: &AppState) -> sqlx::Result<HashMap<Uuid, Usage>> { Ok(usage_of(&raw(st, COST_DAYS).await?, &ip_hosts(&st.db).await?)) }

/// 窗口内的按天聚合：(各天, 入口按 (节点, 线路机, 天) 的下载 / 上传, 线路机网卡按 (线路机, 天) 的下行 / 上行)
pub(crate) type Raw = Arc<(Vec<Date>, Vec<(Uuid, Uuid, Date, i64, i64)>, Vec<(Uuid, Date, i64, i64)>)>;
/// `LiveCache.daily`：(汇总轮次, (组织时区的今天, 天数) → 聚合)
pub(crate) type Cache = (u64, HashMap<(Date, i64), Raw>);

#[derive(Serialize, Clone, Copy)]
pub struct Biz {
    pub date: Date,
    pub down: i64,
    pub up: i64,
}

#[derive(Serialize, Clone, Copy)]
pub struct Nic {
    pub date: Date,
    pub rx: i64,
    pub tx: i64,
    pub billed: i64,
}

#[derive(Serialize)]
pub struct NodeRow {
    pub id: Uuid,
    pub name: String,
    pub team: Option<Ref>,
    pub kind: String,
    pub lifecycle: String,
    pub country: Option<String>,
    /// 线路机的直出入口（节点就是线路机自己）
    pub direct: bool,
    pub down: i64,
    pub up: i64,
    /// 月费（USD 分）：在用且有订阅、有汇率的；直出、没价格的为 null
    pub monthly_usd_cents: Option<i64>,
    /// 被扣住不回收的备用机（软路由出口还指着 / 同机 IP 节点还在用）：照常续费，网页不灰显、计入合计
    pub held: bool,
    /// 每 GB 业务流量的 USD（月费 ÷ 按本次区间日均折成的 30 天流量）；没有月费或区间内没流量为 null
    pub usd_per_gb: Option<f64>,
    /// 近 `idle.days` 天（含今天）日均业务流量（字节）；直出为 null
    pub avg7: Option<i64>,
    /// 闲置：在用、上架满 `idle.days` 天、`avg7` 低于 `idle.gb_per_day` GB
    pub idle: bool,
    pub daily: Vec<Biz>,
    /// 节点自己的网卡（VPS 节点按额度同口径计费；家宽代理、同机 IP 节点、直出行、窗口内没有读数的为 null）：生效的规则
    pub rule: Option<&'static str>,
    /// 区间合计的下行 / 上行 / 计费量（先加总再套规则，同线路机行）
    pub rx: Option<i64>,
    pub tx: Option<i64>,
    pub billed: Option<i64>,
    /// 每天的网卡下行 / 上行 / 计费量，与 `dates` 等长
    pub nic_daily: Option<Vec<Nic>>,
}

/// 团队成本（近 30 天，不受团队 / 机器过滤影响）
#[derive(Serialize)]
pub struct TeamRow {
    pub id: Uuid,
    pub name: String,
    pub spare: bool,
    /// 在用的节点数
    pub nodes: i64,
    /// 其中闲置的
    pub idle: i64,
    /// 在用节点的月费合计（USD 分）；全都没价格为 null。备用团队只算被扣住不回收的那几台（闲置备用不续费，同首页）
    pub monthly_usd_cents: Option<i64>,
    /// 备用团队里被扣住不回收的在用台数（别的团队为 0）；`monthly_usd_cents`、`usd_per_gb`、`unpriced` 在备用团队只按这几台算
    pub held: i64,
    /// 这几台近 30 天的业务流量（字节）；别的团队为 0
    pub held_bytes_30d: i64,
    /// 在用节点里没价格（没订阅或没汇率）、没算进月费的台数（备用团队只数被扣住的）
    pub unpriced: i64,
    /// 近 30 天业务流量（字节）
    pub bytes_30d: i64,
    pub usd_per_gb: Option<f64>,
}

#[derive(Serialize)]
pub struct Idle {
    pub days: i64,
    pub gb_per_day: f64,
}

#[derive(Serialize)]
pub struct RelayRow {
    pub id: Uuid,
    pub name: String,
    pub lifecycle: String,
    pub country: Option<String>,
    /// 生效的网卡计量规则（订阅单独设置的，或供应商的）：min / max / sum / out / in
    pub rule: &'static str,
    pub rx: i64,
    pub tx: i64,
    /// 区间合计的计费量 = 按规则套区间内的下行之和、上行之和（和额度已用同口径）；每天的在 daily[].billed
    pub billed: i64,
    pub down: i64,
    pub up: i64,
    pub daily: Vec<Nic>,
    pub business: Vec<Biz>,
}

#[derive(Serialize)]
pub struct Daily {
    pub tz: String,
    pub dates: Vec<Date>,
    pub total: Vec<Biz>,
    pub idle: Idle,
    pub teams: Vec<TeamRow>,
    pub nodes: Vec<NodeRow>,
    pub relays: Vec<RelayRow>,
}

/// 窗口内的按天聚合，按汇总轮次缓存
async fn raw(st: &AppState, days: i64) -> sqlx::Result<Raw> {
    let tz = crate::meta::org_tz();
    // 今天看库里的时钟（同 flows）
    let (today, start): (Date, time::OffsetDateTime) = sqlx::query_as(
        "select l, (l - $2::int)::timestamp at time zone $1 from (select (now() at time zone $1)::date l) x",
    )
    .bind(&tz)
    .bind((days - 1) as i32)
    .fetch_one(&st.view_db)
    .await?;
    let c = &st.live;
    let mut slot = c.daily.lock().await;
    let rolled = c.rolled();
    if slot.0 != rolled {
        *slot = (rolled, HashMap::new());
    }
    if let Some(r) = slot.1.get(&(today, days)) {
        return Ok(r.clone());
    }
    let biz_q = sqlx::query_as::<_, (Uuid, Uuid, Date, i64, i64)>(
        "select e.node_id, e.relay_id, x.d, least(sum(x.down), 9e18)::bigint, least(sum(x.up), 9e18)::bigint
         from (select entry_id, (hour at time zone $2)::date d, sum(down_bytes) down, sum(up_bytes) up
               from entry_traffic_hourly where hour >= $1 group by 1, 2) x
         join entry e on e.id = x.entry_id group by 1, 2, 3",
    )
    .bind(start)
    .bind(&tz)
    .fetch_all(&st.view_db);
    let nic_q = sqlx::query_as::<_, (Uuid, Date, i64, i64)>(
        "select h.machine_id, (h.hour at time zone $2)::date d,
                least(sum(h.net_rx), 9e18)::bigint, least(sum(h.net_tx), 9e18)::bigint
         from host_metric_hourly h join machine m on m.id = h.machine_id
         where h.hour >= $1 and m.kind <> 'proxy' and m.host_id is null group by 1, 2",
    )
    .bind(start)
    .bind(&tz)
    .fetch_all(&st.view_db);
    let (biz, nic) = tokio::try_join!(biz_q, nic_q)?;
    let dates: Vec<Date> = (0..days).rev().map(|i| today - time::Duration::days(i)).collect();
    let r: Raw = Arc::new((dates, biz, nic));
    slot.1.insert((today, days), r.clone());
    Ok(r)
}

#[derive(sqlx::FromRow)]
struct M {
    id: Uuid,
    kind: String,
    name: String,
    team_id: Option<Uuid>,
    team_name: Option<String>,
    team_spare: bool,
    lifecycle: String,
    country: Option<String>,
    rule: Option<String>,
    price: Option<i32>,
    host_id: Option<Uuid>,
    currency: Option<String>,
    cycle: Option<String>,
    /// 上架已满 `IDLE_DAYS` 天
    aged: bool,
}

pub async fn daily(st: &AppState, days: Option<i64>, team: Option<&str>, machine: Option<&str>) -> ApiResult<Daily> {
    let days = days.unwrap_or(30);
    if !(1..=MAX_DAYS).contains(&days) {
        return Err(ApiError::bad_request(format!("days 只能是 1–{MAX_DAYS}")));
    }
    let team_id: Option<Uuid> = match team.map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(t) => Some(
            sqlx::query_scalar("select id from team where id::text = $1 or name = $1 limit 1")
                .bind(t)
                .fetch_optional(&st.db)
                .await?
                .ok_or_else(|| ApiError::not_found(format!("没有团队「{t}」")))?,
        ),
    };
    let machine_id: Option<Uuid> = match machine.map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(q) => Some(
            sqlx::query_scalar(
                "select m.id from machine m left join team t on t.id = m.team_id
                 where m.recycled_at is null and (m.id::text = $1 or display_name(m.kind::text, m.label, t.name, m.number) = $1 or m.label = $1)
                 order by m.lifecycle = 'retired', m.created_at desc limit 1",
            )
            .bind(q)
            .fetch_optional(&st.db)
            .await?
            .ok_or_else(|| ApiError::not_found(format!("没有机器「{q}」")))?,
        ),
    };
    // 窗口至少 30 天（团队成本、闲置要用），展示的是最后 days 天
    let raw = raw(st, days.max(COST_DAYS)).await?;
    let (all_dates, biz, nic) = &*raw;
    let dates = &all_dates[all_dates.len() - days as usize..];
    let hosts = ip_hosts(&st.db).await?;
    let held = views::held_ids(st.live.latest());
    let uses = usage_of(&raw, &hosts);
    let fx = crate::fx::current(&st.db).await?;
    let ids: Vec<Uuid> = biz.iter().flat_map(|b| [b.0, b.1]).chain(nic.iter().map(|n| n.0)).collect::<HashSet<_>>().into_iter().collect();
    let ms: Vec<M> = sqlx::query_as(
        "select m.id, m.kind::text kind, display_name(m.kind::text, m.label, t.name, m.number) name, t.id team_id, t.name team_name, coalesce(t.spare, false) team_spare,
                m.lifecycle::text lifecycle, m.host_id, m.country, coalesce(s.traffic_rule, tp.traffic_rule) rule, s.price_cents price, s.currency, s.cycle::text cycle,
                coalesce(m.activated_at <= now() - make_interval(days => $2), false) aged
         from machine m left join team t on t.id = m.team_id
         left join subscription_eff s on s.machine_id = m.id left join sub_template tp on tp.machine_id = m.id
         where m.id = any($1) or (m.lifecycle <> 'retired' and m.recycled_at is null)
         order by m.kind = 'relay' desc, t.sort_order nulls first, t.name, m.number, m.label, m.id",
    )
    .bind(&ids)
    .bind(IDLE_DAYS as i32)
    .fetch_all(&st.db)
    .await?;
    // 月费（USD 分）：只算在用的，订阅生效价按周期折成每月再按汇率折（同总览的 monthly_usd）
    let monthly_usd = |m: &M| -> Option<i64> {
        if m.lifecycle != "active" {
            return None;
        }
        crate::fx::convert(&fx, crate::meta::monthly_cents(m.price?, m.cycle.as_deref()?), m.currency.as_deref()?).0
    };
    // 国旗：没手动指定国家的取机器视图里按 IP 画像得出的；用已经算好的最新一份、不等视图重算（视图卡住时本接口照常出，没有就是 null）
    let auto: HashMap<Uuid, Option<String>> = st.live.latest().map(|v| v.iter().map(|m| (m.id, m.country.clone())).collect()).unwrap_or_default();
    let country = |m: &M| m.country.clone().or_else(|| auto.get(&m.id).cloned().flatten());
    let by_id: HashMap<Uuid, &M> = ms.iter().map(|m| (m.id, m)).collect();
    let pos: HashMap<Date, usize> = dates.iter().enumerate().map(|(i, d)| (*d, i)).collect();
    let empty_biz = || dates.iter().map(|&date| Biz { date, down: 0, up: 0 }).collect::<Vec<_>>();
    let empty_nic = || dates.iter().map(|&date| Nic { date, rx: 0, tx: 0, billed: 0 }).collect::<Vec<_>>();
    let in_team = |id: Uuid| team_id.is_none_or(|t| by_id.get(&id).is_some_and(|m| m.team_id == Some(t)));

    // 过滤 = 选入口：团队按节点所在团队；机器是节点或线路机都行
    let mut total = empty_biz();
    // 节点行：(节点, 是否直出) → 每天
    let mut node_days: HashMap<(Uuid, bool), Vec<Biz>> = HashMap::new();
    let mut relay_biz: HashMap<Uuid, Vec<Biz>> = HashMap::new();
    for &(node, relay, d, down, up) in biz {
        if !in_team(node) || machine_id.is_some_and(|m| m != node && m != relay) {
            continue;
        }
        let Some(&i) = pos.get(&d) else { continue };
        for v in [&mut total[i], &mut node_days.entry((node, node == relay)).or_insert_with(empty_biz)[i], &mut relay_biz.entry(relay).or_insert_with(empty_biz)[i]] {
            v.down = v.down.saturating_add(down);
            v.up = v.up.saturating_add(up);
        }
    }
    // 宿主机承载的同机 IP 节点的窗口内流量（不受团队 / 机器过滤影响）：宿主机的 usd_per_gb 按它加上自己的算，行里的下载 / 上传仍只是自己的
    let mut ip_extra: HashMap<Uuid, i64> = HashMap::new();
    for &(node, relay, d, down, up) in biz {
        if let Some(h) = hosts.get(&node).filter(|_| node != relay && pos.contains_key(&d)) {
            let e = ip_extra.entry(*h).or_default();
            *e = e.saturating_add(down).saturating_add(up);
        }
    }
    // 没流量的在用（active）节点也列出（看得出谁一直没流量；草稿、上架中、上架失败、退役中的没有流量就不列）；直出只在有流量时列
    for m in &ms {
        let picked = machine_id.map_or(m.kind != "relay" && m.lifecycle == "active" && in_team(m.id), |x| x == m.id && m.kind != "relay");
        if picked {
            node_days.entry((m.id, false)).or_insert_with(empty_biz);
        }
    }
    // 网卡按天聚合（线路机、VPS 节点共用；每天一格，下面各行再按各自的规则算计费量）
    let mut nic_days: BTreeMap<Uuid, Vec<Nic>> = BTreeMap::new();
    for &(id, d, rx, tx) in nic {
        let Some(&i) = pos.get(&d) else { continue };
        let v = &mut nic_days.entry(id).or_insert_with(empty_nic)[i];
        v.rx = v.rx.saturating_add(rx);
        v.tx = v.tx.saturating_add(tx);
    }
    let sum = |v: &[Biz]| v.iter().fold((0i64, 0i64), |a, b| (a.0.saturating_add(b.down), a.1.saturating_add(b.up)));
    let mut nodes: Vec<NodeRow> = node_days
        .into_iter()
        .filter_map(|((id, direct), daily)| {
            let m = by_id.get(&id)?;
            let (down, up) = sum(&daily);
            // 直出行是线路机自己（网卡在 relays 里）；节点有网卡读数的才带
            let nic = nic_days.get(&id).filter(|_| !direct).map(|days| {
                let rule = views::nic_rule(m.rule.as_deref());
                let days: Vec<Nic> = days.iter().map(|v| Nic { billed: crate::meta::billed(rule, v.rx, v.tx), ..*v }).collect();
                let f = |g: fn(&Nic) -> i64| days.iter().map(g).fold(0i64, i64::saturating_add);
                let (rx, tx) = (f(|v| v.rx), f(|v| v.tx));
                (rule, rx, tx, crate::meta::billed(rule, rx, tx), days)
            });
            let monthly_usd_cents = if direct { None } else { monthly_usd(m) };
            let sum7 = (!direct).then(|| uses.get(&id).map_or(0, |u| u.sum7));
            Some(NodeRow {
                id,
                name: if direct { direct_name(&m.name) } else { m.name.clone() },
                team: m.team_id.map(|id| Ref { id, name: m.team_name.clone().unwrap_or_default() }),
                kind: m.kind.clone(),
                lifecycle: m.lifecycle.clone(),
                country: country(m),
                direct,
                down,
                up,
                monthly_usd_cents,
                held: held.contains(&id),
                usd_per_gb: usd_per_gb(monthly_usd_cents, down.saturating_add(up).saturating_add(ip_extra.get(&id).copied().unwrap_or(0)), days),
                avg7: sum7.map(|s| s / IDLE_DAYS),
                idle: sum7.is_some_and(|s| m.lifecycle == "active" && is_idle(s, m.aged)),
                daily,
                rule: nic.as_ref().map(|n| n.0),
                rx: nic.as_ref().map(|n| n.1),
                tx: nic.as_ref().map(|n| n.2),
                billed: nic.as_ref().map(|n| n.3),
                nic_daily: nic.map(|n| n.4),
            })
        })
        .collect();
    let order: HashMap<Uuid, usize> = ms.iter().enumerate().map(|(i, m)| (m.id, i)).collect();
    nodes.sort_by(|a, b| (b.down.saturating_add(b.up)).cmp(&a.down.saturating_add(a.up)).then(order[&a.id].cmp(&order[&b.id])).then(a.direct.cmp(&b.direct)));

    // 线路机：不过滤时列全部未退役的与窗口内有数据的；过滤时只列经过它的业务流量不为 0 的（过滤的机器就是这台线路机时总列出）
    let filtered = team_id.is_some() || machine_id.is_some();
    let shown: Vec<&M> = ms
        .iter()
        .filter(|m| m.kind == "relay")
        .filter(|m| {
            let has = relay_biz.get(&m.id).is_some_and(|v| v.iter().any(|b| b.down > 0 || b.up > 0));
            if filtered {
                has || machine_id == Some(m.id)
            } else {
                m.lifecycle != "retired" || has || nic_days.contains_key(&m.id)
            }
        })
        .collect();
    let relays: Vec<RelayRow> = shown
        .into_iter()
        .map(|m| {
            let rule = views::nic_rule(m.rule.as_deref());
            let mut daily = nic_days.remove(&m.id).unwrap_or_else(empty_nic);
            for v in &mut daily {
                v.billed = crate::meta::billed(rule, v.rx, v.tx);
            }
            let business = relay_biz.remove(&m.id).unwrap_or_else(empty_biz);
            let (down, up) = sum(&business);
            let f = |g: fn(&Nic) -> i64| daily.iter().map(g).fold(0i64, i64::saturating_add);
            // 区间合计的计费量：先求区间内的下行、上行之和，再套规则（同额度已用 views.rs `quota_used`），不是逐天计费量相加
            let (rx, tx) = (f(|v| v.rx), f(|v| v.tx));
            RelayRow {
                id: m.id,
                name: m.name.clone(),
                lifecycle: m.lifecycle.clone(),
                country: country(m),
                rule,
                rx,
                tx,
                billed: crate::meta::billed(rule, rx, tx),
                down,
                up,
                daily,
                business,
            }
        })
        .collect();
    // 团队成本：ms 已按团队排序，先到先列；没有在用节点也没有流量的团队不列
    let mut teams: Vec<TeamRow> = vec![];
    for m in ms.iter().filter(|m| m.kind != "relay") {
        let Some(tid) = m.team_id else { continue };
        let i = teams.iter().position(|t| t.id == tid).unwrap_or_else(|| {
            teams.push(TeamRow { id: tid, name: m.team_name.clone().unwrap_or_default(), spare: m.team_spare, nodes: 0, idle: 0, monthly_usd_cents: None, held: 0, held_bytes_30d: 0, unpriced: 0, bytes_30d: 0, usd_per_gb: None });
            teams.len() - 1
        });
        let (t, u) = (&mut teams[i], uses.get(&m.id).copied().unwrap_or_default());
        t.bytes_30d = t.bytes_30d.saturating_add(u.own30);
        if m.lifecycle == "active" {
            t.nodes += 1;
            // 备用团队的机器本来就是放着不用的，不标闲置
            t.idle += i64::from(!m.team_spare && is_idle(u.sum7, m.aged));
            // 备用团队只有被扣住不回收的几台照常续费，月费、没价格只算它们
            let is_held = m.team_spare && held.contains(&m.id);
            t.held += i64::from(is_held);
            if is_held {
                t.held_bytes_30d = t.held_bytes_30d.saturating_add(u.own30);
            }
            if !m.team_spare || is_held {
                match monthly_usd(m) {
                    Some(c) => t.monthly_usd_cents = Some(t.monthly_usd_cents.unwrap_or(0) + c),
                    // 同机 IP 节点默认不记费用（钱在宿主机订阅上），没订阅的不算「没价格」；有订阅却折不出 USD（没汇率）的照算
                    None if m.host_id.is_some() && m.price.is_none() => {}
                    None => t.unpriced += 1,
                }
            }
        }
    }
    teams.retain(|t| t.nodes > 0 || t.bytes_30d > 0);
    for t in &mut teams {
        t.usd_per_gb = usd_per_gb(t.monthly_usd_cents, if t.spare { t.held_bytes_30d } else { t.bytes_30d }, COST_DAYS);
    }
    Ok(Daily { tz: crate::meta::org_tz(), dates: dates.to_vec(), total, idle: Idle { days: IDLE_DAYS, gb_per_day: IDLE_GB_PER_DAY }, teams, nodes, relays })
}
