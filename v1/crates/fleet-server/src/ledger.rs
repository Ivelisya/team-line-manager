//! 计费台账的对账（迁移 0097）：每 60 秒（中控启动时先跑一次）把每个非备用团队的「可计费节点」与计费名额对一遍。
//!
//! 可计费节点：VPS / 家宽代理（同机 IP 节点也算）、在用、所在团队不是备用团队、没被标不计费（`BILLABLE`）。线路机不算。
//! 对一个团队（一个事务，按团队取 advisory 锁，两个中控实例同时跑也只是排队，结果一样）：
//! 1. 名额的占用者不在可计费集合里（离开团队、退役、进备用、标了不计费、机器被删）→ 名额空出待定（holder 置空，vacated_at = 现在）。
//! 2. 可计费、不占名额的节点 → 待定（billing_pending，seen_at = 现在）；待定的节点又不可计费了（加入后 24 小时内又走了）→ 直接删掉，不计费、不记事件。
//!    空出名额的原占用者 24 小时内回来（撤销退役、取消不计费）→ 原样接回，不记事件。
//! 3. 配对：空出待定的名额与待定节点按时间先后一一配对，两边时刻相差不超过 24 小时（含正好 24 小时）就是替换——
//!    新节点接过名额，起算日、地区不变，记 swap（不分先加新后走旧还是先走旧后加新）。
//! 4. 超时：待定节点 seen_at 之后满 24 小时（严格晚于）还没配上 → 新名额，起算日 = seen_at 的北京日期，地区按节点国家，记 add；
//!    空出名额 vacated_at 之后满 24 小时还没配上 → 结束日 = vacated_at 的北京日期，记 remove。事件时间用 seen_at / vacated_at（真实时刻）。
//!
//! 手动名额（系统外的节点）不参与对账。还没初始化过（app_setting 里没有 billing_since，见 POST /billing/init）时对账什么都不做：
//! 部署后、初始化前不会把现有节点都当成新加入的。
//!
//! 决定怎么变的是纯函数 `plan`（传入「现在」，测试不用等 24 小时），`reconcile_team` 只负责读库、写库。

use crate::batches::{self as bt, fmt_md};
use crate::country;
use crate::db::new_id;
use crate::state::AppState;
use serde_json::{json, Value};
use sqlx::{PgConnection, PgPool};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use time::{Date, Duration, OffsetDateTime, UtcOffset};
use uuid::Uuid;

/// 替换的配对窗口
pub const GRACE: Duration = Duration::hours(24);

/// 可计费节点的条件（m = machine，t = 它所在的 team，内连接）
pub const BILLABLE: &str = "m.kind in ('vps', 'proxy') and m.lifecycle = 'active' and not t.spare and not exists (select 1 from billing_exempt x where x.machine_id = m.id)";

/// 初始化过的标记（app_setting 的 key，值是起算日）
pub const SINCE_KEY: &str = "billing_since";

/// 团队计费的 advisory 锁（事务级）：对账、改名额、初始化同一团队互相排队
pub async fn lock_team(c: &mut PgConnection, team: Uuid) -> sqlx::Result<()> {
    sqlx::query("select pg_advisory_xact_lock(hashtextextended('billing:' || $1::text, 0))").bind(team).execute(c).await?;
    Ok(())
}

pub fn org_date(t: OffsetDateTime, off: UtcOffset) -> Date { t.to_offset(off).date() }

// ── 纯函数：决定怎么变 ───────────────────────────────────────────────

/// 团队里一个可计费节点
#[derive(Debug, Clone)]
pub struct Node {
    pub id: Uuid,
    pub name: String,
    pub region: String,
}

/// 团队里一个没结束的名额（end_date 为 null）
#[derive(Debug, Clone)]
pub struct OpenSlot {
    pub id: Uuid,
    pub holder: Option<Uuid>,
    /// 占用者（或空出前的占用者）最近的显示名
    pub holder_name: Option<String>,
    pub vacated_at: Option<OffsetDateTime>,
    pub prev_holder: Option<Uuid>,
    pub manual: bool,
}

#[derive(Debug, Clone)]
pub struct Pending {
    pub machine: Uuid,
    pub seen_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// 占用者还在，显示名变了（改编号、团队改名）：刷新快照
    Rename { slot: Uuid, name: String },
    /// 占用者离开（机器被删的 prev 为 None），vacated_at = 现在
    Vacate { slot: Uuid, prev: Option<Uuid> },
    /// 原占用者 24 小时内回来：原样接回，不记事件
    Restore { slot: Uuid, machine: Uuid },
    /// 新的待定节点，seen_at = 现在
    Pend { machine: Uuid },
    /// 待定的节点不可计费了：删掉，不记事件
    Unpend { machine: Uuid },
    Swap { slot: Uuid, machine: Uuid, name: String, other: Option<String>, at: OffsetDateTime },
    Add { machine: Uuid, name: String, region: String, start: Date, at: OffsetDateTime },
    End { slot: Uuid, name: Option<String>, end: Date, at: OffsetDateTime },
}

/// 一个团队这一轮怎么变。`held_elsewhere`：可计费、却还占着别的团队没结束的名额的节点（刚换团队、那边还没对账），这一轮先不动它
pub fn plan(slots: &[OpenSlot], pending: &[Pending], billable: &[Node], held_elsewhere: &HashSet<Uuid>, now: OffsetDateTime, off: UtcOffset) -> Vec<Change> {
    let nodes: HashMap<Uuid, &Node> = billable.iter().map(|n| (n.id, n)).collect();
    let mut out = vec![];
    // 1. 占用者还在的刷新名字，不在的空出；已空出的进待配对列表 (名额, 空出时刻, 原占用者, 名字)
    let mut holding: HashSet<Uuid> = HashSet::new();
    let mut vac: Vec<(Uuid, OffsetDateTime, Option<Uuid>, Option<String>)> = vec![];
    for s in slots.iter().filter(|s| !s.manual) {
        match (s.holder, s.vacated_at) {
            (Some(h), _) if nodes.contains_key(&h) => {
                holding.insert(h);
                let name = &nodes[&h].name;
                if s.holder_name.as_deref() != Some(name) {
                    out.push(Change::Rename { slot: s.id, name: name.clone() });
                }
            }
            (Some(h), _) => {
                out.push(Change::Vacate { slot: s.id, prev: Some(h) });
                vac.push((s.id, now, Some(h), s.holder_name.clone()));
            }
            // 机器被物理删除（外键置空）却没空出过：现在空出
            (None, None) => {
                out.push(Change::Vacate { slot: s.id, prev: s.prev_holder });
                vac.push((s.id, now, s.prev_holder, s.holder_name.clone()));
            }
            (None, Some(at)) => vac.push((s.id, at, s.prev_holder, s.holder_name.clone())),
        }
    }
    // 2. 待定：已有的（不可计费了的删掉）+ 新来的
    let known: HashMap<Uuid, OffsetDateTime> = pending.iter().map(|p| (p.machine, p.seen_at)).collect();
    for p in pending {
        if !nodes.contains_key(&p.machine) || holding.contains(&p.machine) || held_elsewhere.contains(&p.machine) {
            out.push(Change::Unpend { machine: p.machine });
        }
    }
    let mut free: Vec<(&Node, OffsetDateTime, bool)> = billable
        .iter()
        .filter(|n| !holding.contains(&n.id) && !held_elsewhere.contains(&n.id))
        .map(|n| match known.get(&n.id) {
            Some(&t) => (n, t, true),
            None => (n, now, false),
        })
        .collect();
    free.sort_by(|a, b| (a.1, &a.0.name).cmp(&(b.1, &b.0.name)));
    // 原占用者回来（窗口内）：原样接回
    vac.retain(|(slot, at, prev, _)| {
        let Some(i) = free.iter().position(|(n, seen, _)| Some(n.id) == *prev && *seen <= *at + GRACE) else { return true };
        out.push(Change::Restore { slot: *slot, machine: free[i].0.id });
        free.remove(i);
        false
    });
    for (n, _, was) in &free {
        if !was {
            out.push(Change::Pend { machine: n.id });
        }
    }
    // 3. 按时间先后配对：两边时刻相差不超过 GRACE
    vac.sort_by_key(|v| (v.1, v.0));
    let (mut i, mut j) = (0, 0);
    let mut paired_v = vec![false; vac.len()];
    let mut paired_f = vec![false; free.len()];
    while i < vac.len() && j < free.len() {
        let (slot, vat, _, ref other) = vac[i];
        let (n, seen, _) = free[j];
        if seen > vat + GRACE {
            i += 1;
        } else if vat > seen + GRACE {
            j += 1;
        } else {
            out.push(Change::Swap { slot, machine: n.id, name: n.name.clone(), other: other.clone(), at: vat.max(seen) });
            (paired_v[i], paired_f[j]) = (true, true);
            i += 1;
            j += 1;
        }
    }
    // 4. 超时：之后再来的都配不上了（严格晚于 GRACE）
    for (k, (n, seen, _)) in free.iter().enumerate() {
        if !paired_f[k] && *seen + GRACE < now {
            out.push(Change::Add { machine: n.id, name: n.name.clone(), region: n.region.clone(), start: org_date(*seen, off), at: *seen });
        }
    }
    for (k, (slot, at, _, name)) in vac.iter().enumerate() {
        if !paired_v[k] && *at + GRACE < now {
            out.push(Change::End { slot: *slot, name: name.clone(), end: org_date(*at, off), at: *at });
        }
    }
    out
}

// ── 增减记录的一句话 ─────────────────────────────────────────────────

/// 手动名额在记录与列表里的名字：「手动 · 墨西哥家宽」
pub fn manual_name(note: &str) -> String { if note.is_empty() { "手动".into() } else { format!("手动 · {note}") } }

fn md(v: &Value) -> String { v.as_str().and_then(bt::parse_date).map(fmt_md).unwrap_or_else(|| "?".into()) }

/// 「算到 11-14」；一期都没计的写「不计费」
fn until(v: &Value) -> String { if v.is_null() { "不计费".into() } else { format!("算到 {}", md(v)) } }

/// 事件的中文一句话（读取时拼，名字是当时的快照）
pub fn event_text(kind: &str, machine: Option<&str>, other: Option<&str>, d: &Value) -> String {
    let m = machine.unwrap_or("（已删除的节点）");
    match kind {
        "init" => format!("{m} 计入计费（初始化，{} 起算）", md(&d["start_date"])),
        "add" => format!("{m} 加入，新增计费节点（{} 起算）", md(&d["start_date"])),
        "remove" => format!("{m} 离开，计费节点减少（{}）", until(&d["billed_until"])),
        "swap" => format!("{} → {m} 替换，计费不变", other.unwrap_or("（已删除的节点）")),
        "edit" => format!("{m}：{}", d["what"].as_str().unwrap_or("修改")),
        "manual_add" => format!("添加手动计费节点 {m}（{}，{} 起算）", d["region"].as_str().unwrap_or(""), md(&d["start_date"])),
        "manual_end" => format!("{m} 结束计费（{}）", until(&d["billed_until"])),
        "exempt" if d["exempt"] == true => format!("{m} 设为不计费"),
        "exempt" => format!("{m} 恢复计费"),
        k => format!("{m} {k}"),
    }
}

/// 写一条增减记录
#[allow(clippy::too_many_arguments)]
pub async fn log(c: &mut PgConnection, team: Uuid, slot: Option<Uuid>, at: OffsetDateTime, kind: &str, machine: Option<&str>, other: Option<&str>, detail: Value) -> sqlx::Result<()> {
    sqlx::query("insert into billing_event (id, team_id, slot_id, at, kind, machine_name, other_name, detail) values ($1, $2, $3, $4, $5, $6, $7, $8)")
        .bind(new_id())
        .bind(team)
        .bind(slot)
        .bind(at)
        .bind(kind)
        .bind(machine)
        .bind(other)
        .bind(detail)
        .execute(c)
        .await?;
    Ok(())
}

// ── 读库、写库 ───────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct NodeRow {
    id: Uuid,
    name: String,
    country: Option<String>,
    ip: Option<String>,
}

/// 团队现在的可计费节点（含地区：手动指定的国家优先，否则按地址查离线 IP 库，同名字旁的国旗）
pub async fn billable_nodes(st: &AppState, c: &mut PgConnection, team: Uuid) -> sqlx::Result<Vec<Node>> {
    let rows: Vec<NodeRow> = sqlx::query_as(&format!(
        "select m.id, display_name(m.kind::text, m.label, t.name, m.number) name, m.country, host(m.primary_ip) ip
         from machine m join team t on t.id = m.team_id where m.team_id = $1 and {BILLABLE} order by m.number, m.id"
    ))
    .bind(team)
    .fetch_all(&mut *c)
    .await?;
    let ips: Vec<IpAddr> = rows.iter().filter(|r| r.country.is_none()).filter_map(|r| r.ip.as_deref()?.parse().ok()).collect();
    let intel = if ips.is_empty() { HashMap::new() } else { st.ipintel.enrich(&st.db, &ips, true).await };
    Ok(rows
        .into_iter()
        .map(|r| {
            let auto = r.ip.as_deref().and_then(|ip| ip.parse::<IpAddr>().ok()).and_then(|ip| intel.get(&ip)).and_then(|b| b.country.clone());
            Node { id: r.id, name: r.name, region: country::region(r.country.or(auto).as_deref()) }
        })
        .collect())
}

#[derive(sqlx::FromRow)]
struct SlotRow {
    id: Uuid,
    holder_id: Option<Uuid>,
    holder_name: Option<String>,
    vacated_at: Option<OffsetDateTime>,
    prev_holder_id: Option<Uuid>,
    manual: bool,
    start_date: Date,
    region: String,
}

/// 初始化过没有（没有就不对账）
pub async fn initialized(db: &PgPool) -> sqlx::Result<bool> {
    sqlx::query_scalar("select exists (select 1 from app_setting where key = $1)").bind(SINCE_KEY).fetch_one(db).await
}

/// 对一个团队（一个事务）。返回记了几条增减记录
pub async fn reconcile_team(st: &AppState, team: Uuid, now: OffsetDateTime) -> anyhow::Result<usize> {
    let off = crate::meta::org_offset();
    let mut tx = st.db.begin().await?;
    lock_team(&mut tx, team).await?;
    let spare: Option<bool> = sqlx::query_scalar("select spare from team where id = $1").bind(team).fetch_optional(&mut *tx).await?;
    if spare != Some(false) {
        return Ok(0);
    }
    let billable = billable_nodes(st, &mut tx, team).await?;
    let rows: Vec<SlotRow> = sqlx::query_as(
        "select id, holder_id, holder_name, vacated_at, prev_holder_id, manual, start_date, region from billing_slot where team_id = $1 and end_date is null",
    )
    .bind(team)
    .fetch_all(&mut *tx)
    .await?;
    let pending: Vec<Pending> = sqlx::query_as::<_, (Uuid, OffsetDateTime)>("select machine_id, seen_at from billing_pending where team_id = $1")
        .bind(team)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(|(machine, seen_at)| Pending { machine, seen_at })
        .collect();
    let ids: Vec<Uuid> = billable.iter().map(|n| n.id).collect();
    let held_elsewhere: HashSet<Uuid> =
        sqlx::query_scalar("select holder_id from billing_slot where end_date is null and team_id <> $1 and holder_id = any($2)").bind(team).bind(&ids).fetch_all(&mut *tx).await?.into_iter().collect();
    let slots: Vec<OpenSlot> = rows
        .iter()
        .map(|r| OpenSlot { id: r.id, holder: r.holder_id, holder_name: r.holder_name.clone(), vacated_at: r.vacated_at, prev_holder: r.prev_holder_id, manual: r.manual })
        .collect();
    let by_id: HashMap<Uuid, &SlotRow> = rows.iter().map(|r| (r.id, r)).collect();
    let changes = plan(&slots, &pending, &billable, &held_elsewhere, now, off);
    let mut events = 0;
    for ch in &changes {
        match ch {
            Change::Rename { slot, name } => {
                sqlx::query("update billing_slot set holder_name = $2 where id = $1").bind(slot).bind(name).execute(&mut *tx).await?;
            }
            Change::Vacate { slot, prev } => {
                sqlx::query("update billing_slot set holder_id = null, vacated_at = $2, prev_holder_id = $3 where id = $1").bind(slot).bind(now).bind(prev).execute(&mut *tx).await?;
            }
            Change::Restore { slot, machine } => {
                sqlx::query("update billing_slot set holder_id = $2, vacated_at = null, prev_holder_id = null where id = $1").bind(slot).bind(machine).execute(&mut *tx).await?;
                sqlx::query("delete from billing_pending where machine_id = $1").bind(machine).execute(&mut *tx).await?;
            }
            Change::Pend { machine } => {
                // 别的团队还留着它的待定行（刚换团队、那边还没对账）：改成这个团队、从现在算
                sqlx::query(
                    "insert into billing_pending (machine_id, team_id, seen_at) values ($1, $2, $3)
                     on conflict (machine_id) do update set team_id = excluded.team_id, seen_at = excluded.seen_at where billing_pending.team_id <> excluded.team_id",
                )
                .bind(machine)
                .bind(team)
                .bind(now)
                .execute(&mut *tx)
                .await?;
            }
            Change::Unpend { machine } => {
                sqlx::query("delete from billing_pending where machine_id = $1 and team_id = $2").bind(machine).bind(team).execute(&mut *tx).await?;
            }
            Change::Swap { slot, machine, name, other, at } => {
                sqlx::query("update billing_slot set holder_id = $2, holder_name = $3, vacated_at = null, prev_holder_id = null where id = $1")
                    .bind(slot)
                    .bind(machine)
                    .bind(name)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("delete from billing_pending where machine_id = $1").bind(machine).execute(&mut *tx).await?;
                let region = by_id.get(slot).map(|r| r.region.as_str()).unwrap_or("");
                let start = by_id.get(slot).map(|r| bt::fmt_date(r.start_date));
                log(&mut tx, team, Some(*slot), *at, "swap", Some(name), other.as_deref(), json!({"region": region, "start_date": start})).await?;
                events += 1;
            }
            Change::Add { machine, name, region, start, at } => {
                let id = new_id();
                sqlx::query("insert into billing_slot (id, team_id, region, start_date, holder_id, holder_name) values ($1, $2, $3, $4, $5, $6)")
                    .bind(id)
                    .bind(team)
                    .bind(region)
                    .bind(start)
                    .bind(machine)
                    .bind(name)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("delete from billing_pending where machine_id = $1").bind(machine).execute(&mut *tx).await?;
                log(&mut tx, team, Some(id), *at, "add", Some(name), None, json!({"region": region, "start_date": bt::fmt_date(*start)})).await?;
                events += 1;
            }
            Change::End { slot, name, end, at } => {
                sqlx::query("update billing_slot set end_date = $2 where id = $1").bind(slot).bind(end).execute(&mut *tx).await?;
                let r = by_id.get(slot);
                let until = r.and_then(|r| bt::billed_until(r.start_date, *end)).map(bt::fmt_date);
                log(&mut tx, team, Some(*slot), *at, "remove", name.as_deref(), None, json!({"region": r.map(|r| r.region.clone()), "end_date": bt::fmt_date(*end), "billed_until": until}))
                    .await?;
                events += 1;
            }
        }
    }
    tx.commit().await?;
    Ok(events)
}

/// 对所有非备用团队跑一轮（没初始化过什么都不做）。一个团队出错只记日志，不影响别的团队
pub async fn reconcile(st: &AppState, now: OffsetDateTime) -> anyhow::Result<usize> {
    if !initialized(&st.db).await? {
        return Ok(0);
    }
    let teams: Vec<(Uuid, String)> = sqlx::query_as("select id, name from team where not spare order by sort_order, name").fetch_all(&st.db).await?;
    let mut n = 0;
    for (id, name) in teams {
        match reconcile_team(st, id, now).await {
            Ok(k) => n += k,
            Err(e) => tracing::error!("计费对账（{name}）失败：{e:#}"),
        }
    }
    Ok(n)
}

/// 每 60 秒对一轮（启动时立即一次）
pub async fn run_forever(st: AppState) {
    let mut t = tokio::time::interval(std::time::Duration::from_secs(60));
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        t.tick().await;
        match reconcile(&st, OffsetDateTime::now_utc()).await {
            Ok(n) if n > 0 => tracing::info!("计费对账：记了 {n} 条增减"),
            Ok(_) => {}
            Err(e) => tracing::error!("计费对账失败：{e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::{datetime, offset};

    const OFF: UtcOffset = offset!(+8);

    fn id(n: u128) -> Uuid { Uuid::from_u128(n) }

    fn node(n: u128, name: &str) -> Node { Node { id: id(n), name: name.into(), region: "美区".into() } }

    fn held(slot: u128, holder: u128, name: &str) -> OpenSlot {
        OpenSlot { id: id(slot), holder: Some(id(holder)), holder_name: Some(name.into()), vacated_at: None, prev_holder: None, manual: false }
    }

    fn vacated(slot: u128, prev: u128, name: &str, at: OffsetDateTime) -> OpenSlot {
        OpenSlot { id: id(slot), holder: None, holder_name: Some(name.into()), vacated_at: Some(at), prev_holder: Some(id(prev)), manual: false }
    }

    fn pend(m: u128, at: OffsetDateTime) -> Pending { Pending { machine: id(m), seen_at: at } }

    fn run(slots: &[OpenSlot], pending: &[Pending], nodes: &[Node], now: OffsetDateTime) -> Vec<Change> { plan(slots, pending, nodes, &HashSet::new(), now, OFF) }

    const T0: OffsetDateTime = datetime!(2026-10-28 02:00 UTC);

    #[test]
    fn steady_state_changes_nothing() {
        assert!(run(&[held(1, 10, "团队1-01")], &[], &[node(10, "团队1-01")], T0).is_empty());
        // 改了编号：只刷新名字
        assert_eq!(run(&[held(1, 10, "团队1-01")], &[], &[node(10, "团队1-02")], T0), [Change::Rename { slot: id(1), name: "团队1-02".into() }]);
    }

    #[test]
    fn replace_new_first_then_old_leaves() {
        // 先加新：待定
        let c = run(&[held(1, 10, "团队2-07")], &[], &[node(10, "团队2-07"), node(11, "团队2-20")], T0);
        assert_eq!(c, [Change::Pend { machine: id(11) }]);
        // 3 小时后旧的走了：同一轮空出并配上，替换时刻取两边较晚的（空出 = 现在）
        let now = T0 + Duration::hours(3);
        let c = run(&[held(1, 10, "团队2-07")], &[pend(11, T0)], &[node(11, "团队2-20")], now);
        assert_eq!(
            c,
            [Change::Vacate { slot: id(1), prev: Some(id(10)) }, Change::Swap { slot: id(1), machine: id(11), name: "团队2-20".into(), other: Some("团队2-07".into()), at: now }]
        );
    }

    #[test]
    fn replace_old_leaves_then_new_joins() {
        // 先走旧：空出待定，什么都不记
        let c = run(&[held(1, 10, "团队2-07")], &[], &[], T0);
        assert_eq!(c, [Change::Vacate { slot: id(1), prev: Some(id(10)) }]);
        // 23 小时后新的来了：配上
        let now = T0 + Duration::hours(23);
        let c = run(&[vacated(1, 10, "团队2-07", T0)], &[], &[node(11, "团队2-20")], now);
        assert_eq!(c, [Change::Pend { machine: id(11) }, Change::Swap { slot: id(1), machine: id(11), name: "团队2-20".into(), other: Some("团队2-07".into()), at: now }]);
        // 正好 24 小时也算替换
        let c = run(&[vacated(1, 10, "团队2-07", T0)], &[], &[node(11, "团队2-20")], T0 + GRACE);
        assert!(matches!(c.last(), Some(Change::Swap { .. })), "{c:?}");
    }

    #[test]
    fn timeouts_add_and_remove_at_real_time() {
        // 待定满 24 小时（严格晚于）没配上 → 新增，起算日 = seen_at 的北京日期（T0 是北京 10-28 10:00）
        assert!(run(&[], &[pend(11, T0)], &[node(11, "团队1-10")], T0 + GRACE).is_empty(), "正好 24 小时还在等");
        let c = run(&[], &[pend(11, T0)], &[node(11, "团队1-10")], T0 + GRACE + Duration::seconds(1));
        assert_eq!(c, [Change::Add { machine: id(11), name: "团队1-10".into(), region: "美区".into(), start: time::macros::date!(2026 - 10 - 28), at: T0 }]);
        // 北京时间跨日：UTC 10-27 17:00 = 北京 10-28 01:00
        let late = datetime!(2026-10-27 17:00 UTC);
        let c = run(&[], &[pend(11, late)], &[node(11, "团队1-10")], late + Duration::days(2));
        assert!(matches!(&c[0], Change::Add { start, .. } if *start == time::macros::date!(2026 - 10 - 28)), "{c:?}");
        // 空出满 24 小时没配上 → 减少，结束日 = 空出那天
        let c = run(&[vacated(1, 10, "团队2-05", T0)], &[], &[], T0 + Duration::hours(25));
        assert_eq!(c, [Change::End { slot: id(1), name: Some("团队2-05".into()), end: time::macros::date!(2026 - 10 - 28), at: T0 }]);
    }

    #[test]
    fn far_apart_do_not_pair() {
        // 空出 30 小时后才有节点加入（中控停过）：不配对，空出的结束，新来的待定
        let now = T0 + Duration::hours(30);
        let c = run(&[vacated(1, 10, "团队2-05", T0)], &[], &[node(11, "团队2-21")], now);
        assert_eq!(c, [Change::Pend { machine: id(11) }, Change::End { slot: id(1), name: Some("团队2-05".into()), end: time::macros::date!(2026 - 10 - 28), at: T0 }]);
    }

    #[test]
    fn joined_then_left_within_grace_is_dropped() {
        let c = run(&[], &[pend(11, T0)], &[], T0 + Duration::hours(5));
        assert_eq!(c, [Change::Unpend { machine: id(11) }]);
    }

    #[test]
    fn spare_exempt_or_team_change_count_as_leaving() {
        // 进备用、标不计费、改去别的团队：都只是不在这个团队的可计费集合里 → 空出
        let c = run(&[held(1, 10, "团队1-03"), held(2, 11, "团队1-04")], &[], &[node(11, "团队1-04")], T0);
        assert_eq!(c, [Change::Vacate { slot: id(1), prev: Some(id(10)) }]);
        // 新团队这边：还占着原团队的名额（那边这一轮还没对账）先不动，下一轮再待定
        let elsewhere: HashSet<Uuid> = [id(10)].into();
        assert!(plan(&[], &[], &[node(10, "团队2-30")], &elsewhere, T0, OFF).is_empty());
        assert_eq!(plan(&[], &[pend(10, T0)], &[node(10, "团队2-30")], &elsewhere, T0, OFF), [Change::Unpend { machine: id(10) }]);
    }

    #[test]
    fn swap_in_from_spare() {
        // 备用机换入：它进团队就是一个新的待定节点，旧的空出后配上（与普通替换一样）
        let c = run(&[vacated(1, 10, "团队2-07", T0)], &[], &[node(20, "团队2-07")], T0 + Duration::minutes(10));
        assert!(matches!(c.as_slice(), [Change::Pend { .. }, Change::Swap { machine, other: Some(o), .. }] if *machine == id(20) && o == "团队2-07"), "{c:?}");
    }

    #[test]
    fn former_holder_back_within_grace_is_restored() {
        // 撤销退役 / 取消不计费：原节点 24 小时内回来，原样接回，不记事件；其它待定节点不抢它
        let c = run(&[vacated(1, 10, "团队1-03", T0)], &[pend(11, T0 - Duration::hours(1))], &[node(10, "团队1-03"), node(11, "团队1-11")], T0 + Duration::hours(2));
        assert_eq!(c[0], Change::Restore { slot: id(1), machine: id(10) });
        assert_eq!(c.len(), 1, "{c:?}");
    }

    #[test]
    fn pairs_in_time_order_and_leftovers_wait() {
        // 两个空出、三个待定：先空出的配先来的，多的一个继续等
        let a = T0;
        let b = T0 + Duration::hours(1);
        let now = T0 + Duration::hours(2);
        let c = run(
            &[vacated(1, 10, "团队2-01", b), vacated(2, 11, "团队2-02", a)],
            &[pend(21, a), pend(22, b), pend(23, now)],
            &[node(21, "团队2-21"), node(22, "团队2-22"), node(23, "团队2-23")],
            now,
        );
        let swaps: Vec<(Uuid, Uuid)> = c.iter().filter_map(|x| if let Change::Swap { slot, machine, .. } = x { Some((*slot, *machine)) } else { None }).collect();
        assert_eq!(swaps, [(id(2), id(21)), (id(1), id(22))]);
        assert_eq!(c.len(), 2, "第三个在等，不新增: {c:?}");
    }

    #[test]
    fn manual_slots_are_left_alone_and_deleted_holder_vacates() {
        let manual = OpenSlot { id: id(1), holder: None, holder_name: None, vacated_at: None, prev_holder: None, manual: true };
        assert!(run(&[manual], &[], &[], T0).is_empty());
        let gone = OpenSlot { id: id(2), holder: None, holder_name: Some("团队1-05".into()), vacated_at: None, prev_holder: None, manual: false };
        assert_eq!(run(&[gone], &[], &[], T0), [Change::Vacate { slot: id(2), prev: None }]);
    }

    #[test]
    fn texts() {
        assert_eq!(event_text("add", Some("团队1-10"), None, &json!({"start_date": "2026-10-30"})), "团队1-10 加入，新增计费节点（10-30 起算）");
        assert_eq!(event_text("swap", Some("团队2-20"), Some("团队2-07"), &json!({})), "团队2-07 → 团队2-20 替换，计费不变");
        assert_eq!(event_text("remove", Some("团队2-05"), None, &json!({"billed_until": "2026-11-14"})), "团队2-05 离开，计费节点减少（算到 11-14）");
        assert_eq!(event_text("remove", Some("团队2-05"), None, &json!({"billed_until": null})), "团队2-05 离开，计费节点减少（不计费）");
        assert_eq!(event_text("init", Some("团队1-01"), None, &json!({"start_date": "2026-10-01"})), "团队1-01 计入计费（初始化，10-01 起算）");
        assert_eq!(event_text("exempt", Some("团队1-05"), None, &json!({"exempt": true})), "团队1-05 设为不计费");
        assert_eq!(manual_name("墨西哥家宽"), "手动 · 墨西哥家宽");
    }
}
