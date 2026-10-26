//! 放置矩阵：每台线路机到每个在用 VPS 节点的网络往返，回答「节点放在哪台线路机最好」，换线前看它，不再现场拼 ping 脚本。
//! 线路机上的入口探测（collect.rs 的 TCP / 出口探测）只测挂在自己身上的入口，测不到节点连另外几台线路机的情况；这里补上。
//!
//! - 下发：`GET /agent/v1/rtt`（agent_api.rs 调 [`config`]），agent 每轮测之前取一次：全部在用 VPS 节点（不含家宽代理、备用团队）的 IP。
//!   不进期望状态：节点集合一变（上架、退役、换 IP）就要给所有线路机代数 +1，「重启 Xray 失败、暂停重试」中的线路机会被新代数
//!   带着再重启一次、断一次全部客户连接（fleet-agent 的 desired.rs 只拦同一代数）；单独取也就不用记清单摘要、不在一个事务里锁全部线路机。
//! - 测：agent 每 5 分钟一轮，最多 4 个节点同时测，只用 ICMP（系统 ping 3 次），用不了就不测。不连 SS 端口（节点防火墙只放行
//!   它所挂的线路机），也不连 SSH 端口（fail2ban 可能把线路机 IP 封掉、连带断业务流量），见 fleet-agent 的 rtt.rs。
//! - 存：`relay_node_rtt_hourly` 小时汇总（迁移 0083），上报入库时直接累加（[`ingest`]），保留 7 天；每小时一轮把早于上一小时的 rtts 清空（只有当前小时与上一小时的 p50 还要靠它重算）。
//! - 看：`GET /placement?team=&node=`（[`view`]）：每个节点到各线路机近 24 小时的 p50 与成功率、近 7 天 tcp_fail 告警次数、当前与最优线路机，
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
pub const KEEP_DAYS: i32 = 7;

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
/// 已经不存在的节点跳过（不让整份上报因外键失败）。
/// 迟到的上报可能落进 rtts 已被清空的旧小时（有 max_ms 却没有 rtts）：这种行的 p50_ms 保持不变、不再累加 rtts，只累加轮数
/// （否则 p50 会被只剩迟到那几个值重算成另一个数）
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
           samples = h.samples + excluded.samples, ok = h.ok + excluded.ok,
           rtts = case when cardinality(h.rtts) = 0 and h.max_ms is not null then h.rtts else h.rtts || excluded.rtts end,
           p50_ms = case when cardinality(h.rtts) = 0 and h.max_ms is not null then h.p50_ms
                         else (select percentile_cont(0.5) within group (order by x) from unnest(h.rtts || excluded.rtts) x) end,
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

/// 后台：每小时一轮，清掉保留期外的汇总，并把早于上一小时的 rtts 清空。
/// rtts 只用来在当前小时继续累加时重算 p50（上一小时留着，给跨整点才到的上报）；更早的小时 p50_ms 已定，矩阵也只读 p50_ms，
/// 留着它们是每行十几个 float 的白占空间
pub async fn run_forever(st: AppState) {
    let mut t = tokio::time::interval(Duration::from_secs(3600));
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        t.tick().await;
        if let Err(e) = prune(&st.db).await {
            tracing::error!("放置矩阵清理失败：{e:#}");
        }
    }
}

/// 每小时那一轮：先把早于上一小时的 rtts 清空，再删保留期外的汇总
pub async fn prune(db: &sqlx::PgPool) -> sqlx::Result<()> {
    sqlx::query("update relay_node_rtt_hourly set rtts = '{}' where hour < date_trunc('hour', now()) - interval '1 hour' and cardinality(rtts) > 0")
        .execute(db)
        .await?;
    sqlx::query("delete from relay_node_rtt_hourly where hour < now() - make_interval(days => $1)").bind(KEEP_DAYS).execute(db).await?;
    Ok(())
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
    /// 节点每天的业务流量（字节，按近 7 天，不乘折算系数）；换到某台线路机后多出的计费量 = 它 × 那台的 `RelayIn::k`
    pub daily: f64,
}

pub struct RelayIn {
    pub id: Uuid,
    /// 在用、非故障（降级算正常）
    pub usable: bool,
    pub q: Q,
    /// 折算系数（网卡计费日均 ÷ 业务日均，api::lifecycle::relay_ks）：同样的业务量在这台上多出的计费量 = 业务量 × k
    pub k: f64,
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
            let r = relays.iter().find(|r| r.id == id);
            let add = n.daily * r.map_or(1.0, |r| r.k);
            if Some(id) != n.current && !r.is_some_and(|r| r.q.fits(load.get(&id).copied().unwrap_or(0.0) + add)) {
                p.no_room.push(id);
                continue;
            }
            p.best = Some(id);
            let cur = cur_cell(n);
            // 当前线路机一个样本都没有（它的 agent 还没升级、系统 ping 用不了）：是它没在测，不是链路差，只展示不建议
            p.suggest = n.current.is_some_and(|c| c != id) && cur.samples > 0 && (!cur.good() || cur.p50.is_some_and(|c| c - p50 >= MIN_GAIN_MS));
            if p.suggest {
                *load.entry(id).or_default() += add;
            }
            break;
        }
    }
    out
}

// ── 接口 ──────────────────────────────────────────────────────────────────
/// 近 `WINDOW_H` 小时的格子：节点 → 线路机 → 一格（取自小时汇总，只读；搬迁建议选目标线路机也读它）。
/// 往返取各小时 p50_ms 的中位数（中位数的中位数），是近似值：与成功率一样按小时汇总算，不展开 24 小时的 rtts（早于上一小时的已清空）；
/// 调用方传视图池（有语句超时）
pub async fn cells(db: &sqlx::PgPool, nodes: &[Uuid]) -> sqlx::Result<HashMap<Uuid, HashMap<Uuid, Cell>>> {
    let since = OffsetDateTime::now_utc() - time::Duration::hours(WINDOW_H);
    let rows: Vec<(Uuid, Uuid, i64, i64, Option<f64>)> = sqlx::query_as(
        "select relay_id, node_id, sum(samples)::bigint, sum(ok)::bigint, percentile_cont(0.5) within group (order by p50_ms)
         from relay_node_rtt_hourly where node_id = any($1) and hour >= date_trunc('hour', $2::timestamptz) group by 1, 2",
    )
    .bind(nodes)
    .bind(since)
    .fetch_all(db)
    .await?;
    let mut out: HashMap<Uuid, HashMap<Uuid, Cell>> = HashMap::new();
    for (r, n, samples, ok, p50) in rows {
        out.entry(n).or_default().insert(r, Cell { samples, ok, p50 });
    }
    Ok(out)
}

/// 搬迁目标按往返比较时一台候选线路机的成绩
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Score {
    /// 被搬的节点到它近 24 小时 p50 的平均（毫秒），每个节点都有够格的数据
    Rtt(f64),
    /// 有被搬的节点到它的成功率不足 99%：不选
    Bad,
    /// 有被搬的节点没测过（或没有往返）：比不了
    NoData,
}

/// 被搬的 `nodes` 到线路机 `relay` 的成绩
pub fn score(nodes: &[Uuid], cells: &HashMap<Uuid, HashMap<Uuid, Cell>>, relay: Uuid) -> Score {
    let cs: Vec<Option<Cell>> = nodes.iter().map(|n| cells.get(n).and_then(|c| c.get(&relay)).copied()).collect();
    if cs.iter().flatten().any(|c| c.success().is_some_and(|s| s < MIN_SUCCESS)) {
        return Score::Bad;
    }
    let p50: Vec<f64> = cs.iter().filter_map(|c| c.filter(Cell::good).and_then(|c| c.p50)).collect();
    if p50.is_empty() || p50.len() < nodes.len() {
        return Score::NoData;
    }
    Score::Rtt(p50.iter().sum::<f64>() / p50.len() as f64)
}

/// `GET /placement?team=&node=`（team 填团队 id 或名称，node 填节点 id 或显示名；都不填 = 全部）
pub async fn view(st: &AppState, team: Option<&str>, node: Option<&str>) -> ApiResult<Value> {
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
    let node_q = node.map(str::trim).filter(|s| !s.is_empty());
    let all = views::live(st).await?;
    let now = OffsetDateTime::now_utc();
    let mut relays: Vec<&MachineView> = all.iter().filter(|m| m.kind == "relay" && m.lifecycle == "active" && !m.spare).collect();
    relays.sort_by(|a, b| a.name.cmp(&b.name));
    let nodes: Vec<&MachineView> = all
        .iter()
        .filter(|m| {
            m.kind == "vps"
                && m.lifecycle == "active"
                && !m.spare
                && team_id.is_none_or(|t| m.team.as_ref().is_some_and(|x| x.id == t))
                && node_q.is_none_or(|q| m.id.to_string() == q || m.name == q)
        })
        .collect();
    if let (true, Some(q)) = (nodes.is_empty(), node_q) {
        return Err(ApiError::not_found(format!("没有在用的 VPS 节点「{q}」")));
    }
    let ids: Vec<Uuid> = nodes.iter().map(|m| m.id).collect();

    let mut grid = cells(&st.view_db, &ids).await?;
    // 近 7 天严重的 tcp_fail 告警次数：同一条告警恢复后又触发记在 reopen_count 上；警告级的（TCP 偶发失败、出口正常）不算
    let fails: HashMap<Uuid, i64> = sqlx::query_as(
        "select subject_id, (count(*) + coalesce(sum(reopen_count), 0))::bigint from incident
         where rule = 'tcp_fail' and severity = 'critical' and subject_id = any($1) and last_seen_at >= now() - interval '7 days' group by 1",
    )
    .bind(&ids)
    .fetch_all(&st.db)
    .await?
    .into_iter()
    .collect();
    // 各在用入口近 7 天的业务流量：节点每天多少（搬过去的是它，业务量）；线路机的折算系数 k 同搬迁建议（api::lifecycle::relay_ks），按目标线路机的 k 折算
    let traffic: Vec<(Uuid, Uuid, i64)> = sqlx::query_as(
        "select e.relay_id, e.node_id, least(coalesce(sum(h.up_bytes::numeric + h.down_bytes), 0), 9e18)::bigint
         from entry_traffic_hourly h join entry e on e.id = h.entry_id
         where e.lifecycle = 'active' and h.hour >= now() - interval '7 days' group by 1, 2",
    )
    .fetch_all(&st.db)
    .await?;
    let days_of = |m: &MachineView| m.activated_at.map_or(7.0, |a| ((now - a).as_seconds_f64() / 86400.0).clamp(1.0, 7.0));
    let mut node_daily: HashMap<Uuid, f64> = HashMap::new();
    let mut pair_daily: HashMap<(Uuid, Uuid), f64> = HashMap::new();
    for (r, n, b) in &traffic {
        let d = all.iter().find(|m| m.id == *n).map_or(*b as f64 / 7.0, |m| *b as f64 / days_of(m));
        *node_daily.entry(*n).or_default() += d;
        *pair_daily.entry((*r, *n)).or_default() += d;
    }
    let qs: HashMap<Uuid, Q> = relays.iter().map(|r| (r.id, crate::api::lifecycle::quota_q(r, now))).collect();
    let ks = crate::api::lifecycle::relay_ks(&st.db, &all, now).await?;

    // 入口状态口径同换线规划（api/lifecycle.rs 的 plan_move）：待应用、在用、停用都算节点的入口
    let live = |m: &MachineView| -> Vec<(Uuid, i32, bool)> {
        m.entries.iter().filter(|e| !e.direct && matches!(e.lifecycle.as_str(), "pending" | "active" | "disabled")).map(|e| (e.relay.id, e.port, e.lifecycle == "active")).collect()
    };
    // 当前线路机：恰有一条入口、而且在用；否则不给批量建议（`move A,B --to X` 会被整组拒绝）
    let current = |m: &MachineView| {
        let l = live(m);
        (l.len() == 1 && l[0].2).then(|| (l[0].0, l[0].1))
    };
    let mut ins: Vec<NodeIn> = nodes
        .iter()
        .map(|m| {
            let cur = current(m).map(|c| c.0);
            NodeIn { current: cur, cells: grid.remove(&m.id).unwrap_or_default(), daily: node_daily.get(&m.id).copied().unwrap_or(0.0) }
        })
        .collect();
    // 有多个在用入口的节点：每个在用入口各算一份（当前 = 这条入口所在的线路机；节点已有入口的线路机不当候选，换过去会被拒），
    // 只给带 --from 的单条建议，不进批量命令
    let mut multi: Vec<(usize, Uuid, i32)> = vec![];
    for (i, m) in nodes.iter().enumerate() {
        let l = live(m);
        if l.iter().filter(|e| e.2).count() < 2 {
            continue;
        }
        for &(relay, port, _) in l.iter().filter(|e| e.2) {
            let mut cs = ins[i].cells.clone();
            cs.retain(|r, _| *r == relay || !l.iter().any(|e| e.0 == *r));
            let daily = pair_daily.get(&(relay, m.id)).copied().unwrap_or(0.0);
            ins.push(NodeIn { current: Some(relay), cells: cs, daily });
            multi.push((i, relay, port));
        }
    }
    let rin: Vec<RelayIn> = relays.iter().map(|r| RelayIn { id: r.id, usable: r.health.status != Status::Down, q: qs[&r.id], k: ks.get(&r.id).copied().unwrap_or_else(|| crate::quota::rule_factor(r.quota.rule)) }).collect();
    let picks = pick(&ins, &rin);

    let pct = |c: &Cell| c.success().map(|s| (s * 1000.0).round() / 10.0);
    let ms = |v: Option<f64>| v.map(|x| (x * 10.0).round() / 10.0);
    let name_of = |id: Uuid| relays.iter().find(|r| r.id == id).map_or_else(String::new, |r| r.name.clone());
    let sh = crate::api::incidents::sh;
    let why = |cur: Cell, best: Cell| match (cur.good(), cur.p50, best.p50) {
        (true, Some(c), Some(b)) => format!("可快 {:.0} ms", c - b),
        _ => match pct(&cur) {
            Some(s) => format!("当前成功率 {s}%"),
            None => "当前线路机测不到".into(),
        },
    };
    let cell_of = |n: &NodeIn, r: Option<Uuid>| r.and_then(|r| n.cells.get(&r)).copied().unwrap_or_default();
    let mut groups: BTreeMap<String, (Uuid, Vec<Value>)> = BTreeMap::new();
    let mut out_nodes = vec![];
    for (i, m) in nodes.iter().enumerate() {
        let (n, p) = (&ins[i], &picks[i]);
        let cur = current(m);
        let reason = p.suggest.then(|| why(cell_of(n, n.current), cell_of(n, p.best)));
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
        let on: Vec<Uuid> = live(m).iter().filter(|e| e.2).map(|e| e.0).collect();
        out_nodes.push(json!({
            "id": m.id, "name": m.name, "team": m.team, "ip": m.ip,
            "current_relay_id": n.current, "entry_relay_ids": on, "entries": on.len(),
            "cells": row_cells, "tcp_fail_7d": fails.get(&m.id).copied().unwrap_or(0), "daily_bytes": n.daily.round() as i64,
            "best_relay_id": p.best, "suggest": p.suggest, "reason": reason, "no_room": p.no_room,
        }));
    }
    let suggestions: Vec<Value> = groups
        .into_iter()
        .map(|(to, (id, ns))| {
            let names: Vec<String> = ns.iter().map(|n| sh(n["name"].as_str().unwrap_or(""))).collect();
            json!({"to": {"id": id, "name": to}, "nodes": ns, "command": format!("fleetctl move {} --to {}", names.join(","), sh(&to))})
        })
        .collect();
    // 多入口节点的单条建议：换掉哪台线路机上的入口
    let single: Vec<Value> = multi
        .iter()
        .zip(&picks[nodes.len()..])
        .zip(&ins[nodes.len()..])
        .filter_map(|(((i, from, port), p), n)| {
            let to = p.best.filter(|_| p.suggest)?;
            let m = nodes[*i];
            Some(json!({
                "node": {"id": m.id, "name": m.name}, "from": {"id": from, "name": name_of(*from)}, "to": {"id": to, "name": name_of(to)}, "port": port,
                "reason": why(cell_of(n, Some(*from)), cell_of(n, Some(to))),
                "command": format!("fleetctl move {} --from {} --to {}", sh(&m.name), sh(&name_of(*from)), sh(&name_of(to))),
            }))
        })
        .collect();
    Ok(json!({
        "relays": relays.iter().map(|r| json!({"id": r.id, "name": r.name, "status": r.health.status, "quota": {"limit_gb": r.quota.limit_gb, "used": r.quota.used, "forecast_full_at": r.quota.forecast_full_at.map(crate::meta::fmt_ts)}})).collect::<Vec<_>>(),
        "nodes": out_nodes, "suggestions": suggestions, "single_suggestions": single,
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
        let free = Q { limit: None, used: 0.0, daily: 0.0, days_left: 20.0, rolling: false };
        // c：2000 GB 上限、已用 1000，日均 20 → 到重置日 1400，安全线 1600：每天还能多接 10 GB
        let tight = Q { limit: Some(2000e9), used: 1000e9, daily: 20e9, days_left: 20.0, rolling: false };
        let relays = vec![RelayIn { id: a, usable: true, q: free, k: 1.0 }, RelayIn { id: b, usable: false, q: free, k: 1.0 }, RelayIn { id: c, usable: true, q: tight, k: 1.0 }];
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
        // 当前线路机一个样本都没有（agent 没升级 / ping 用不了）：不建议搬走
        let p = pick(&[node(c, vec![(a, cell(100, 150.0))], 1e9)], &relays);
        assert!(!p[0].suggest, "{:?}", p[0]);
    }

    /// 放得下按目标线路机的 k 折算：业务量 6 GB/天，目标 c 每天只能再接 10 GB 计费量；c 是单向计费（k=1）放得下，
    /// 是双向计费（k=2，折成 12 GB）放不下（原来按当前线路机 a 的 k=1 折算，会误判放得下）；同一轮累计的也按目标的 k
    #[test]
    fn room_is_measured_with_the_target_k() {
        let (a, c) = (Uuid::from_u128(1), Uuid::from_u128(3));
        let free = Q { limit: None, used: 0.0, daily: 0.0, days_left: 20.0, rolling: false };
        let tight = Q { limit: Some(2000e9), used: 1000e9, daily: 20e9, days_left: 20.0, rolling: false };
        let cell = |p50: f64| Cell { samples: 100, ok: 100, p50: Some(p50) };
        let node = NodeIn { current: Some(a), cells: [(a, cell(180.0)), (c, cell(150.0))].into_iter().collect(), daily: 6e9 };
        let relays = |kc: f64| vec![RelayIn { id: a, usable: true, q: free, k: 1.0 }, RelayIn { id: c, usable: true, q: tight, k: kc }];
        assert_eq!(pick(std::slice::from_ref(&node), &relays(1.0))[0], Pick { best: Some(c), suggest: true, no_room: vec![] });
        assert_eq!(pick(std::slice::from_ref(&node), &relays(2.0))[0], Pick { best: Some(a), suggest: false, no_room: vec![c] });
        // 反过来：当前 a 是双向（k=2）、目标 c 是单向（k=1），业务量 9 GB/天按 c 的 k 算是 9 GB，放得下（原来按 a 的 k 算成 18 GB 误判放不下）
        let big = NodeIn { daily: 9e9, ..node };
        let rs = vec![RelayIn { id: a, usable: true, q: free, k: 2.0 }, RelayIn { id: c, usable: true, q: tight, k: 1.0 }];
        assert_eq!(pick(&[big], &rs)[0], Pick { best: Some(c), suggest: true, no_room: vec![] });
    }

    /// 搬迁目标的往返成绩：被搬节点的 p50 取平均；有节点成功率不足 99% 为 Bad；有节点没测过为 NoData
    #[test]
    fn score_averages_and_flags_weak_or_missing() {
        let (n1, n2, r) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(9));
        let cell = |ok: i64, p50: f64| Cell { samples: 100, ok, p50: Some(p50) };
        let grid = |a: Option<Cell>, b: Option<Cell>| -> HashMap<Uuid, HashMap<Uuid, Cell>> {
            [(n1, a), (n2, b)].into_iter().filter_map(|(n, c)| c.map(|c| (n, HashMap::from([(r, c)])))).collect()
        };
        assert_eq!(score(&[n1, n2], &grid(Some(cell(100, 100.0)), Some(cell(100, 140.0))), r), Score::Rtt(120.0));
        assert_eq!(score(&[n1, n2], &grid(Some(cell(100, 100.0)), Some(cell(97, 90.0))), r), Score::Bad);
        assert_eq!(score(&[n1, n2], &grid(Some(cell(100, 100.0)), None), r), Score::NoData);
    }
}
