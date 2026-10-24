//! 放置矩阵：每台线路机到每个在用 VPS 节点的网络往返，回答「节点放在哪台线路机最好」，换线前看它，不再现场拼 ping 脚本。
//! 线路机上的入口探测（collect.rs 的 TCP / 出口探测）只测挂在自己身上的入口，测不到节点连另外几台线路机的情况；这里补上。
//!
//! - 下发：`GET /agent/v1/rtt`（agent_api.rs 调 [`config`]），agent 每轮测之前取一次：全部在用 VPS 节点（不含家宽代理、备用团队）的 IP。
//!   不进期望状态：节点集合一变（上架、退役、换 IP）就要给所有线路机代数 +1，「重启 Xray 失败、暂停重试」中的线路机会被新代数
//!   带着再重启一次、断一次全部客户连接（fleet-agent 的 desired.rs 只拦同一代数）；单独取也就不用记清单摘要、不在一个事务里锁全部线路机。
//! - 测：agent 每 5 分钟一轮，最多 4 个节点同时测，只用 ICMP（系统 ping 3 次），用不了就不测。不连 SS 端口（节点防火墙只放行
//!   它所挂的线路机），也不连 SSH 端口（fail2ban 可能把线路机 IP 封掉、连带断业务流量），见 fleet-agent 的 rtt.rs。
//! - 存：`relay_node_rtt_hourly` 小时汇总（迁移 0083），上报入库时直接累加（[`ingest`]），保留 30 天。
//! - 看：`GET /placement?team=`（[`view`]）：每个节点到各线路机近 24 小时的 p50 与成功率、近 7 天 tcp_fail 告警次数、当前与最优线路机，
//!   以及「当前比最优慢 ≥ 10 ms 或当前成功率 < 99%」的换线建议（按目标线路机合成批量 `fleetctl move A,B --to X`）。

use crate::db::Tx;
use crate::error::{ApiError, ApiResult};
use crate::health::Status;
use crate::quota::Q;
use crate::state::AppState;
use crate::views::{self, MachineView};
use fleet_proto::{RttConfig, RttSample, RttTarget};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

/// agent 每轮间隔
pub const INTERVAL_S: u32 = 300;
/// 最优线路机的成功率下限、建议换线的最小延迟差
pub const MIN_SUCCESS: f64 = 0.99;
pub const MIN_GAIN_MS: f64 = 10.0;
/// 矩阵看近多少小时；小时汇总保留多少天
pub const WINDOW_H: i64 = 24;
pub const KEEP_DAYS: i32 = 30;

/// 要测的节点：在用的 VPS 节点（含同机 IP 节点），不含家宽代理、备用团队里的
pub async fn targets(db: &sqlx::PgPool) -> sqlx::Result<Vec<RttTarget>> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "select m.id, host(m.primary_ip) from machine m left join team t on t.id = m.team_id
         where m.kind = 'vps' and m.lifecycle = 'active' and m.primary_ip is not null and not coalesce(t.spare, false) order by m.id",
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|(node_id, host)| RttTarget { node_id, host }).collect())
}

/// `GET /agent/v1/rtt` 的内容：没有节点时 targets 为空
pub async fn config(db: &sqlx::PgPool) -> sqlx::Result<RttConfig> {
    Ok(RttConfig { interval_s: INTERVAL_S, targets: targets(db).await? })
}

/// 上报里的样本按 (节点, 小时) 累加进小时汇总（agent_api 的事务里调）：时间戳在 [lo, hi]（Unix 毫秒）之外的丢掉，
/// 已经不存在的节点跳过（不让整份上报因外键失败）
pub(crate) async fn ingest(tx: &mut Tx<'_>, relay: Uuid, samples: &[RttSample], lo: i64, hi: i64) -> sqlx::Result<()> {
    // (节点, 小时) → (轮数, 通了几轮, 通了的往返)
    let mut acc: BTreeMap<(Uuid, i64), (i32, i32, Vec<f32>)> = BTreeMap::new();
    for s in samples.iter().take(2000).filter(|s| s.ts_ms >= lo && s.ts_ms <= hi) {
        let a = acc.entry((s.node_id, s.ts_ms.div_euclid(3_600_000) * 3600)).or_default();
        a.0 += 1;
        if s.ok {
            a.1 += 1;
            a.2.extend(s.rtt_ms.filter(|v| v.is_finite() && *v >= 0.0 && *v < 60_000.0));
        }
    }
    if acc.is_empty() {
        return Ok(());
    }
    let hours: Vec<OffsetDateTime> = acc.keys().map(|k| OffsetDateTime::from_unix_timestamp(k.1).unwrap_or(OffsetDateTime::UNIX_EPOCH)).collect();
    // rtts 按 json 传：每个 (节点, 小时) 一个长度不一的数组，Postgres 的 unnest 不收不规则的二维数组
    let rtts: Vec<Value> = acc.values().map(|a| json!(a.2)).collect();
    sqlx::query(
        "insert into relay_node_rtt_hourly as h (relay_id, node_id, hour, samples, ok, p50_ms, max_ms, rtts)
         select $1, u.node, u.hour, u.n, u.k,
                (select percentile_cont(0.5) within group (order by x) from unnest(r.a) x), (select max(x) from unnest(r.a) x), r.a
         from unnest($2::uuid[], $3::timestamptz[], $4::int[], $5::int[], $6::jsonb[]) u(node, hour, n, k, j)
         join machine m on m.id = u.node
         cross join lateral (select coalesce(array_agg(e::real), '{}') a from jsonb_array_elements_text(u.j) e) r
         on conflict (relay_id, node_id, hour) do update set
           samples = h.samples + excluded.samples, ok = h.ok + excluded.ok, rtts = h.rtts || excluded.rtts,
           p50_ms = (select percentile_cont(0.5) within group (order by x) from unnest(h.rtts || excluded.rtts) x),
           max_ms = greatest(h.max_ms, excluded.max_ms)",
    )
    .bind(relay)
    .bind(acc.keys().map(|k| k.0).collect::<Vec<_>>())
    .bind(hours)
    .bind(acc.values().map(|a| a.0).collect::<Vec<_>>())
    .bind(acc.values().map(|a| a.1).collect::<Vec<_>>())
    .bind(rtts)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// 后台：每小时清一次 30 天前的汇总
pub async fn run_forever(st: AppState) {
    let mut t = tokio::time::interval(Duration::from_secs(3600));
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        t.tick().await;
        if let Err(e) = sqlx::query("delete from relay_node_rtt_hourly where hour < now() - make_interval(days => $1)").bind(KEEP_DAYS).execute(&st.db).await {
            tracing::error!("放置矩阵保留期清理失败：{e:#}");
        }
    }
}

// ── 选最优线路机（纯计算，单元测试不起库）────────────────────────────────

/// 一格：近 24 小时测了几轮、通了几轮、通了的往返中位数
#[derive(Debug, Clone, Copy, Default)]
pub struct Cell {
    pub samples: i64,
    pub ok: i64,
    pub p50: Option<f64>,
}

impl Cell {
    pub fn success(&self) -> Option<f64> { (self.samples > 0).then(|| self.ok as f64 / self.samples as f64) }
    /// 够格当最优：成功率 ≥ 99%、有延迟
    fn good(&self) -> bool { self.success().is_some_and(|s| s >= MIN_SUCCESS) && self.p50.is_some() }
}

pub struct NodeIn {
    /// 当前线路机：节点恰有一条在用入口时是它的线路机；没有或有多条为 None（只看、不建议）
    pub current: Option<Uuid>,
    pub cells: HashMap<Uuid, Cell>,
    /// 换过去后目标线路机每天多出来的流量（字节，按近 7 天）
    pub daily: f64,
}

pub struct RelayIn {
    pub id: Uuid,
    /// 在用、非故障（降级算正常）
    pub usable: bool,
    pub q: Q,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Pick {
    pub best: Option<Uuid>,
    pub suggest: bool,
    /// 比最优还快、但额度放不下而跳过的线路机
    pub no_room: Vec<Uuid>,
}

/// 每个节点的最优线路机：够格（成功率 ≥ 99%、有延迟）且能用的线路机里 p50 最小的；不是当前那台时，还要接下它近 7 天的流量后
/// 到重置日仍在安全线内（`Q::fits`，同一轮里已建议换过去的节点的流量一起算）。
/// 建议换线 = 最优不是当前、且当前比最优慢 ≥ 10 ms 或当前成功率 < 99%（或测不到）。
/// 收益大的节点先挑（当前不够格的最先），额度先给它们
pub fn pick(nodes: &[NodeIn], relays: &[RelayIn]) -> Vec<Pick> {
    let cands = |n: &NodeIn| -> Vec<(Uuid, f64)> {
        let mut c: Vec<(Uuid, f64)> = relays.iter().filter(|r| r.usable).filter_map(|r| n.cells.get(&r.id).filter(|c| c.good()).map(|c| (r.id, c.p50.unwrap_or(f64::MAX)))).collect();
        c.sort_by(|a, b| a.1.total_cmp(&b.1));
        c
    };
    let cur_cell = |n: &NodeIn| n.current.and_then(|c| n.cells.get(&c)).copied().unwrap_or_default();
    let gain = |n: &NodeIn| {
        let best = cands(n).first().map_or(f64::MAX, |c| c.1);
        let cur = cur_cell(n);
        if cur.good() { cur.p50.unwrap_or(f64::MAX) - best } else { f64::INFINITY }
    };
    let mut order: Vec<usize> = (0..nodes.len()).collect();
    order.sort_by(|&a, &b| gain(&nodes[b]).total_cmp(&gain(&nodes[a])));
    let mut load: HashMap<Uuid, f64> = HashMap::new();
    let mut out = vec![Pick::default(); nodes.len()];
    for i in order {
        let n = &nodes[i];
        let p = &mut out[i];
        for (id, p50) in cands(n) {
            let q = relays.iter().find(|r| r.id == id).map(|r| r.q);
            if Some(id) != n.current && !q.is_some_and(|q| q.fits(load.get(&id).copied().unwrap_or(0.0) + n.daily)) {
                p.no_room.push(id);
                continue;
            }
            p.best = Some(id);
            let cur = cur_cell(n);
            p.suggest = n.current.is_some_and(|c| c != id) && (!cur.good() || cur.p50.is_some_and(|c| c - p50 >= MIN_GAIN_MS));
            if p.suggest {
                *load.entry(id).or_default() += n.daily;
            }
            break;
        }
    }
    out
}

// ── 接口 ──────────────────────────────────────────────────────────────────

/// `GET /placement?team=`（team 填团队 id 或名称，不填 = 全部）
pub async fn view(st: &AppState, team: Option<&str>) -> ApiResult<Value> {
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
    let all = views::live(st).await?;
    let now = OffsetDateTime::now_utc();
    let mut relays: Vec<&MachineView> = all.iter().filter(|m| m.kind == "relay" && m.lifecycle == "active" && !m.spare).collect();
    relays.sort_by(|a, b| a.name.cmp(&b.name));
    let nodes: Vec<&MachineView> = all
        .iter()
        .filter(|m| m.kind == "vps" && m.lifecycle == "active" && !m.spare && team_id.is_none_or(|t| m.team.as_ref().is_some_and(|x| x.id == t)))
        .collect();
    let ids: Vec<Uuid> = nodes.iter().map(|m| m.id).collect();

    let since = now - time::Duration::hours(WINDOW_H);
    let rows: Vec<(Uuid, Uuid, i64, i64, Option<f64>)> = sqlx::query_as(
        "with w as (select * from relay_node_rtt_hourly where node_id = any($1) and hour >= date_trunc('hour', $2::timestamptz))
         select s.relay_id, s.node_id, s.n, s.k, p.p50 from
           (select relay_id, node_id, sum(samples)::bigint n, sum(ok)::bigint k from w group by 1, 2) s
           left join (select relay_id, node_id, percentile_cont(0.5) within group (order by x) p50 from w cross join lateral unnest(w.rtts) x group by 1, 2) p
           using (relay_id, node_id)",
    )
    .bind(&ids)
    .bind(since)
    .fetch_all(&st.db)
    .await?;
    let mut cells: HashMap<Uuid, HashMap<Uuid, Cell>> = HashMap::new();
    for (r, n, samples, ok, p50) in rows {
        cells.entry(n).or_default().insert(r, Cell { samples, ok, p50 });
    }
    // 近 7 天 tcp_fail 告警次数：同一条告警恢复后又触发记在 reopen_count 上
    let fails: HashMap<Uuid, i64> = sqlx::query_as(
        "select subject_id, (count(*) + coalesce(sum(reopen_count), 0))::bigint from incident
         where rule = 'tcp_fail' and subject_id = any($1) and last_seen_at >= now() - interval '7 days' group by 1",
    )
    .bind(&ids)
    .fetch_all(&st.db)
    .await?
    .into_iter()
    .collect();
    // 各入口近 7 天的业务流量：节点每天多少；线路机网卡日均 ÷ 它上面各入口合计 = 折算系数（同搬迁建议）
    let traffic: Vec<(Uuid, Uuid, i64)> = sqlx::query_as(
        "select e.relay_id, e.node_id, least(coalesce(sum(h.up_bytes::numeric + h.down_bytes), 0), 9e18)::bigint
         from entry_traffic_hourly h join entry e on e.id = h.entry_id
         where e.lifecycle = 'active' and h.hour >= now() - interval '7 days' group by 1, 2",
    )
    .fetch_all(&st.db)
    .await?;
    let days_of = |m: &MachineView| m.activated_at.map_or(7.0, |a| ((now - a).as_seconds_f64() / 86400.0).clamp(1.0, 7.0));
    let mut node_daily: HashMap<Uuid, f64> = HashMap::new();
    let mut relay_sum: HashMap<Uuid, f64> = HashMap::new();
    for (r, n, b) in &traffic {
        let d = all.iter().find(|m| m.id == *n).map_or(*b as f64 / 7.0, |m| *b as f64 / days_of(m));
        *node_daily.entry(*n).or_default() += d;
        *relay_sum.entry(*r).or_default() += d;
    }
    let qs: HashMap<Uuid, Q> = relays.iter().map(|r| (r.id, crate::api::lifecycle::quota_q(r, now))).collect();
    let k_of = |r: Uuid| relay_sum.get(&r).filter(|s| **s > 0.0).map_or(1.0, |s| qs.get(&r).map_or(0.0, |q| q.daily) / s).max(1.0);

    let current = |m: &MachineView| {
        let live: Vec<_> = m.entries.iter().filter(|e| e.lifecycle == "active" && !e.direct).collect();
        (live.len() == 1).then(|| (live[0].relay.id, live[0].port))
    };
    let ins: Vec<NodeIn> = nodes
        .iter()
        .map(|m| {
            let cur = current(m).map(|c| c.0);
            NodeIn { current: cur, cells: cells.remove(&m.id).unwrap_or_default(), daily: node_daily.get(&m.id).copied().unwrap_or(0.0) * cur.map_or(1.0, k_of) }
        })
        .collect();
    let rin: Vec<RelayIn> = relays.iter().map(|r| RelayIn { id: r.id, usable: r.health.status != Status::Down, q: qs[&r.id] }).collect();
    let picks = pick(&ins, &rin);

    let pct = |c: &Cell| c.success().map(|s| (s * 1000.0).round() / 10.0);
    let ms = |v: Option<f64>| v.map(|x| (x * 10.0).round() / 10.0);
    let name_of = |id: Uuid| relays.iter().find(|r| r.id == id).map_or_else(String::new, |r| r.name.clone());
    let mut groups: BTreeMap<String, (Uuid, Vec<Value>)> = BTreeMap::new();
    let mut out_nodes = vec![];
    for ((m, n), p) in nodes.iter().zip(&ins).zip(&picks) {
        let cur = current(m);
        let cur_cell = n.current.and_then(|c| n.cells.get(&c)).copied().unwrap_or_default();
        let best_cell = p.best.and_then(|b| n.cells.get(&b)).copied().unwrap_or_default();
        let reason = p.suggest.then(|| match (cur_cell.good(), cur_cell.p50, best_cell.p50) {
            (true, Some(c), Some(b)) => format!("慢 {:.0} ms", c - b),
            _ => match pct(&cur_cell) {
                Some(s) => format!("当前成功率 {s}%"),
                None => "当前线路机测不到".into(),
            },
        });
        let row_cells: Vec<Value> = relays
            .iter()
            .map(|r| {
                let c = n.cells.get(&r.id).copied().unwrap_or_default();
                json!({"relay_id": r.id, "samples": c.samples, "ok": c.ok, "success_pct": pct(&c), "p50_ms": ms(c.p50)})
            })
            .collect();
        if let (true, Some(to), Some((from, port))) = (p.suggest, p.best, cur) {
            groups.entry(name_of(to)).or_insert((to, vec![])).1.push(json!({"id": m.id, "name": m.name, "from": {"id": from, "name": name_of(from)}, "port": port, "reason": reason}));
        }
        out_nodes.push(json!({
            "id": m.id, "name": m.name, "team": m.team, "ip": m.ip,
            "current_relay_id": n.current, "entries": m.entries.iter().filter(|e| e.lifecycle == "active" && !e.direct).count(),
            "cells": row_cells, "tcp_fail_7d": fails.get(&m.id).copied().unwrap_or(0), "daily_bytes": n.daily.round() as i64,
            "best_relay_id": p.best, "suggest": p.suggest, "reason": reason, "no_room": p.no_room,
        }));
    }
    let suggestions: Vec<Value> = groups
        .into_iter()
        .map(|(to, (id, ns))| {
            let names: Vec<String> = ns.iter().map(|n| crate::api::incidents::sh(n["name"].as_str().unwrap_or(""))).collect();
            json!({"to": {"id": id, "name": to}, "nodes": ns, "command": format!("fleetctl move {} --to {}", names.join(","), crate::api::incidents::sh(&to))})
        })
        .collect();
    Ok(json!({
        "relays": relays.iter().map(|r| json!({"id": r.id, "name": r.name, "status": r.health.status, "quota": {"limit_gb": r.quota.limit_gb, "used": r.quota.used, "forecast_full_at": r.quota.forecast_full_at.map(crate::meta::fmt_ts)}})).collect::<Vec<_>>(),
        "nodes": out_nodes, "suggestions": suggestions,
        "window_h": WINDOW_H, "interval_s": INTERVAL_S, "min_success_pct": MIN_SUCCESS * 100.0, "min_gain_ms": MIN_GAIN_MS,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 最优：够格的里面 p50 最小；放不下（额度）的跳过、同一轮换过去的流量累计；慢 ≥ 10 ms 或当前不够格才建议
    #[test]
    fn picks_fastest_that_fits() {
        let (a, b, c) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let free = Q { limit: None, used: 0.0, daily: 0.0, days_left: 20.0 };
        // c：2000 GB 上限、已用 1000，日均 20 → 到重置日 1400，安全线 1600：每天还能多接 10 GB
        let tight = Q { limit: Some(2000e9), used: 1000e9, daily: 20e9, days_left: 20.0 };
        let relays = vec![RelayIn { id: a, usable: true, q: free }, RelayIn { id: b, usable: false, q: free }, RelayIn { id: c, usable: true, q: tight }];
        let cell = |ok: i64, p50: f64| Cell { samples: 100, ok, p50: Some(p50) };
        let node = |cur: Uuid, cells: Vec<(Uuid, Cell)>, daily: f64| NodeIn { current: Some(cur), cells: cells.into_iter().collect(), daily };
        let nodes = vec![
            // 0：当前 a 180 ms；b 最快但故障；c 150 ms、接 6 GB/天放得下 → 换到 c
            node(a, vec![(a, cell(100, 180.0)), (b, cell(100, 100.0)), (c, cell(100, 150.0))], 6e9),
            // 1：同样想去 c，但 c 已接了节点 0 的 6 GB，再接 6 GB 超了安全线 → 跳过 c，最优是当前 a，不建议
            node(a, vec![(a, cell(100, 175.0)), (c, cell(100, 150.0))], 6e9),
            // 2：当前 c 成功率 97%（不够格）→ 换到 a（当前不用看额度）
            node(c, vec![(a, cell(100, 200.0)), (c, cell(97, 120.0))], 50e9),
            // 3：只快 5 ms：最优是 c 但不建议，也不占 c 的额度
            node(a, vec![(a, cell(100, 155.0)), (c, cell(100, 150.0))], 1e9),
        ];
        let p = pick(&nodes, &relays);
        assert_eq!(p[0], Pick { best: Some(c), suggest: true, no_room: vec![] });
        assert_eq!(p[1], Pick { best: Some(a), suggest: false, no_room: vec![c] });
        assert_eq!(p[2], Pick { best: Some(a), suggest: true, no_room: vec![] });
        assert_eq!(p[3], Pick { best: Some(c), suggest: false, no_room: vec![] });
    }
}
