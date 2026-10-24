//! 每日流量（`GET /traffic/daily?days=&team=&machine=`）：按组织时区切成自然日，每天全部、每个节点、每台线路机的流量。今天算到目前为止。
//!
//! 口径同机器视图的今天 / 本月（views.rs `flows`）：
//! - 业务流量（下载 / 上传）来自入口小时表 `entry_traffic_hourly`。节点 = 以它为节点的全部入口之和，含已移除的入口
//!   （换线前在旧线路机上跑的那段仍算这个节点）；线路机的「经过它的业务流量」= 挂在它上面的入口（换线前的量留在旧线路机上）。
//!   直出入口的节点就是线路机自己，单列一行「线路机-直出」（id 是线路机的）。家宽代理、同机 IP 节点有自己的入口，各算各的；
//!   全部合计 = 各行之和，每个字节只算一次。窗口内有流量的已退役机器照列，lifecycle 照实给。
//! - 线路机网卡（下行 / 上行）来自主机小时表 `host_metric_hourly`，计费量按生效的规则（订阅单独设置的，或供应商的）取小 / 取大 / 相加 / 只算出站 / 只算入站（没有供应商取大）：每天一格按当天的下行、上行算，区间合计先求下行之和、上行之和再套规则（同额度已用）。
//! - 小时表按 UTC 整点分桶，一桶按它的起点落在组织时区的哪天归哪天（半小时时区至多差 30 分钟，同 `flows`）。
//!
//! 两条按 (节点, 线路机, 天) / (线路机, 天) 分组的聚合各扫一遍窗口内的小时行（走 hour 索引，0015 / 0043；先按入口 × 天聚合再连入口表，
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
    pub daily: Vec<Biz>,
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
         where h.hour >= $1 and m.kind = 'relay' group by 1, 2",
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

/// 按规则算一天的计费量
fn billed(rule: &str, rx: i64, tx: i64) -> i64 {
    match rule {
        "min" => rx.min(tx),
        "sum" => rx.saturating_add(tx),
        "out" => tx,
        "in" => rx,
        _ => rx.max(tx),
    }
}

#[derive(sqlx::FromRow)]
struct M {
    id: Uuid,
    kind: String,
    name: String,
    team_id: Option<Uuid>,
    team_name: Option<String>,
    lifecycle: String,
    country: Option<String>,
    rule: Option<String>,
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
    let raw = raw(st, days).await?;
    let (dates, biz, nic) = &*raw;
    let ids: Vec<Uuid> = biz.iter().flat_map(|b| [b.0, b.1]).chain(nic.iter().map(|n| n.0)).collect::<HashSet<_>>().into_iter().collect();
    let ms: Vec<M> = sqlx::query_as(
        "select m.id, m.kind::text kind, display_name(m.kind::text, m.label, t.name, m.number) name, t.id team_id, t.name team_name,
                m.lifecycle::text lifecycle, m.country, coalesce(s.traffic_rule, tp.traffic_rule) rule
         from machine m left join team t on t.id = m.team_id
         left join subscription_eff s on s.machine_id = m.id left join sub_template tp on tp.machine_id = m.id
         where m.id = any($1) or (m.lifecycle <> 'retired' and m.recycled_at is null)
         order by m.kind = 'relay' desc, t.sort_order nulls first, t.name, m.number, m.label, m.id",
    )
    .bind(&ids)
    .fetch_all(&st.db)
    .await?;
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
    // 没流量的在用（active）节点也列出（看得出谁一直没流量；草稿、上架中、上架失败、退役中的没有流量就不列）；直出只在有流量时列
    for m in &ms {
        let picked = machine_id.map_or(m.kind != "relay" && m.lifecycle == "active" && in_team(m.id), |x| x == m.id && m.kind != "relay");
        if picked {
            node_days.entry((m.id, false)).or_insert_with(empty_biz);
        }
    }
    let sum = |v: &[Biz]| v.iter().fold((0i64, 0i64), |a, b| (a.0.saturating_add(b.down), a.1.saturating_add(b.up)));
    let mut nodes: Vec<NodeRow> = node_days
        .into_iter()
        .filter_map(|((id, direct), daily)| {
            let m = by_id.get(&id)?;
            let (down, up) = sum(&daily);
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
                daily,
            })
        })
        .collect();
    let order: HashMap<Uuid, usize> = ms.iter().enumerate().map(|(i, m)| (m.id, i)).collect();
    nodes.sort_by(|a, b| (b.down.saturating_add(b.up)).cmp(&a.down.saturating_add(a.up)).then(order[&a.id].cmp(&order[&b.id])).then(a.direct.cmp(&b.direct)));

    // 线路机：不过滤时列全部未退役的与窗口内有数据的；过滤时只列经过它的业务流量不为 0 的（过滤的机器就是这台线路机时总列出）
    let mut nic_days: BTreeMap<Uuid, Vec<Nic>> = BTreeMap::new();
    for &(id, d, rx, tx) in nic {
        let Some(&i) = pos.get(&d) else { continue };
        let v = &mut nic_days.entry(id).or_insert_with(empty_nic)[i];
        v.rx = v.rx.saturating_add(rx);
        v.tx = v.tx.saturating_add(tx);
    }
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
                v.billed = billed(rule, v.rx, v.tx);
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
                billed: billed(rule, rx, tx),
                down,
                up,
                daily,
                business,
            }
        })
        .collect();
    Ok(Daily { tz: crate::meta::org_tz(), dates: dates.clone(), total, nodes, relays })
}
