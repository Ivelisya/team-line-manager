//! 计费台账的对账（迁移 0097；0104 去掉了「待定」）：每 60 秒（中控启动时先跑一次）把每个非备用团队的「可计费节点」与计费名额对一遍。
//!
//! 可计费节点：VPS / 家宽代理（同机 IP 节点也算）、在用、所在团队不是备用团队、没被标不计费（`BILLABLE`）。线路机不算。
//! 对一个团队（一个事务，按团队取 advisory 锁，两个中控实例同时跑也只是排队，结果一样）：
//! 1. 加入即计费：可计费、没有名额的节点这一轮就建名额，记 add。开始收费日 = 购买日：有订阅的是订阅到期日往前推一个订阅周期
//!    （周期跟随套餐的取套餐的；同机 IP 节点没有自己的订阅看宿主机的；与每一期同一套 add_months，月底对齐），推出来晚于今天的取今天；
//!    没有订阅的是加入那天（组织时区）。地区按节点国家。
//! 2. 离开即结束：名额的占用者不再可计费（离开团队、退役进备用、标了不计费、机器被删）→ 名额结束，结束日 = 今天，记 remove；
//!    已经开始的那一期照收（batches::billed_until）。
//! 3. 回来的节点（撤销退役、取消不计费）：它在这个团队最近结束的名额还在收费期内（billed_until ≥ 今天）就原样接回，不记事件；否则按 1 新建。
//!
//! 补位只认明确的替换：替换节点作业交换编号时在同一个事务里调 `take_over`，新节点接过旧节点的名额（开始收费日、地区、已收都不变），记 swap。
//! 对账不猜哪个新节点顶了哪个旧节点：同一轮一个走、一个来，就是一条 remove、一条 add。
//!
//! 手动名额（系统外的节点）不参与对账。还没初始化过（app_setting 里没有 billing_since，见 POST /billing/init）时对账什么都不做：
//! 部署后、初始化前不会把现有节点都当成新加入的。
//!
//! 决定怎么变的是纯函数 `plan`（传入「现在」），`reconcile_team` 只负责读库、写库。

use crate::batches::{self as bt, fmt_cn};
use crate::country;
use crate::db::new_id;
use crate::meta;
use crate::state::{AppState, Event};
use serde_json::{json, Value};
use sqlx::{PgConnection, PgPool};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use time::{Date, OffsetDateTime, UtcOffset};
use uuid::Uuid;

/// 可计费节点的条件（m = machine，t = 它所在的 team，内连接）
pub const BILLABLE: &str = "m.kind in ('vps', 'proxy') and m.lifecycle = 'active' and not t.spare and not exists (select 1 from billing_exempt x where x.machine_id = m.id)";

/// 初始化过的标记（app_setting 的 key，值是开始收费日）
pub const SINCE_KEY: &str = "billing_since";

/// 团队计费的 advisory 锁（事务级）：对账、改名额、初始化、替换节点接名额同一团队互相排队
pub async fn lock_team(c: &mut PgConnection, team: Uuid) -> sqlx::Result<()> {
    sqlx::query("select pg_advisory_xact_lock(hashtextextended('billing:' || $1::text, 0))").bind(team).execute(c).await?;
    Ok(())
}

pub fn org_date(t: OffsetDateTime, off: UtcOffset) -> Date { t.to_offset(off).date() }

/// 购买日（新名额的开始收费日）：订阅 (到期日, 周期月数) 的到期日往前推一个周期，晚于今天的取今天；没有订阅的是今天（加入那天）
pub fn purchase_date(sub: Option<(Date, u32)>, today: Date) -> Date {
    sub.and_then(|(due, months)| meta::add_months(due, -i64::from(months))).map_or(today, |d| d.min(today))
}

// ── 纯函数：决定怎么变 ───────────────────────────────────────────────

/// 团队里一个可计费节点
#[derive(Debug, Clone)]
pub struct Node {
    pub id: Uuid,
    pub name: String,
    /// 手动指定的国家（优先）
    pub country: Option<String>,
    /// 没指定国家时按它查离线 IP 库
    pub ip: Option<IpAddr>,
    /// 订阅 (到期日, 周期月数)：自己的，没有就宿主机的（同机 IP 节点）；都没有为 None
    pub sub: Option<(Date, u32)>,
}

/// 团队里一个没结束的名额（end_date 为 null）
#[derive(Debug, Clone)]
pub struct OpenSlot {
    pub id: Uuid,
    pub holder: Option<Uuid>,
    /// 占用者最近的显示名
    pub holder_name: Option<String>,
    pub manual: bool,
}

/// 可计费节点在这个团队最近结束的系统名额
#[derive(Debug, Clone)]
pub struct Ended {
    pub slot: Uuid,
    pub machine: Uuid,
    /// 最后收费的那一期的最后一天；一期都没收为 None
    pub billed_until: Option<Date>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// 占用者还在，显示名变了（改编号、团队改名）：刷新快照
    Rename { slot: Uuid, name: String },
    /// 占用者不再可计费（机器被删的 prev 为 None）：名额在 end（今天）结束，记 remove
    End { slot: Uuid, prev: Option<Uuid>, name: Option<String>, end: Date },
    /// 回来的节点：最近结束的名额还在收费期内，原样接回，不记事件
    Restore { slot: Uuid, machine: Uuid, name: String },
    /// 新加入：建名额，开始收费日 = 购买日，记 add
    Add { machine: Uuid, start: Date },
}

/// 一个团队这一轮怎么变。`ended`：可计费节点在这个团队最近结束的名额（每个节点最多一个）；
/// `held_elsewhere`：可计费、却还占着别的团队没结束的名额的节点（刚换团队、那边还没对账），这一轮先不动它
pub fn plan(slots: &[OpenSlot], ended: &[Ended], billable: &[Node], held_elsewhere: &HashSet<Uuid>, now: OffsetDateTime, off: UtcOffset) -> Vec<Change> {
    let today = org_date(now, off);
    let nodes: HashMap<Uuid, &Node> = billable.iter().map(|n| (n.id, n)).collect();
    let mut out = vec![];
    let mut holding: HashSet<Uuid> = HashSet::new();
    for s in slots.iter().filter(|s| !s.manual) {
        match s.holder.and_then(|h| nodes.get(&h)) {
            Some(n) => {
                holding.insert(n.id);
                if s.holder_name.as_deref() != Some(n.name.as_str()) {
                    out.push(Change::Rename { slot: s.id, name: n.name.clone() });
                }
            }
            None => out.push(Change::End { slot: s.id, prev: s.holder, name: s.holder_name.clone(), end: today }),
        }
    }
    for n in billable.iter().filter(|n| !holding.contains(&n.id) && !held_elsewhere.contains(&n.id)) {
        match ended.iter().find(|e| e.machine == n.id && e.billed_until.is_some_and(|d| d >= today)) {
            Some(e) => out.push(Change::Restore { slot: e.slot, machine: n.id, name: n.name.clone() }),
            None => out.push(Change::Add { machine: n.id, start: purchase_date(n.sub, today) }),
        }
    }
    out
}

// ── 增减记录的一句话 ─────────────────────────────────────────────────

/// 手动名额在记录与列表里的名字：备注（墨西哥家宽），没有备注的「手动添加的节点」
pub fn manual_name(note: &str) -> String { if note.is_empty() { "手动添加的节点".into() } else { note.to_string() } }

/// 「10 月 30 日」
fn cn(v: &Value) -> String { v.as_str().and_then(bt::parse_date).map(fmt_cn).unwrap_or_else(|| "?".into()) }

/// 停止收费的节点最后一期收到哪天：「收费到 11 月 29 日」；一期都没收（billed_until 为 null）的「没收过费」
fn until(d: &Value) -> String { if d["billed_until"].is_null() { "没收过费".into() } else { format!("收费到 {}", cn(&d["billed_until"])) } }

/// 「11 月已收：团队1-01、团队1-02（共 2 个）」：名字最多列 10 个
fn paid_text(head: &str, d: &Value) -> String {
    let month = d["month"].as_str().and_then(bt::parse_month).map(|m| format!("{} 月", u8::from(m.month()))).unwrap_or_default();
    let names: Vec<&str> = d["names"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let more = if names.len() > 10 { " 等" } else { "" };
    format!("{month}{head}：{}{more}（共 {} 个）", names.iter().take(10).copied().collect::<Vec<_>>().join("、"), names.len())
}

/// 事件的中文一句话（读取时拼，名字是当时的快照）
pub fn event_text(kind: &str, machine: Option<&str>, other: Option<&str>, d: &Value) -> String {
    let m = machine.unwrap_or("（已删除的节点）");
    match kind {
        "init" => format!("开始记账：{} 个节点从 {}开始收费", d["count"].as_i64().unwrap_or(0), cn(&d["start_date"])),
        "add" => format!("{m} 加入，从 {}开始收费", cn(&d["start_date"])),
        "remove" => format!("{m} 离开，{}", until(d)),
        "swap" => format!("{m} 补位接替 {}，收费不变", other.unwrap_or("（已删除的节点）")),
        "edit" => format!("{m}：{}", d["what"].as_str().unwrap_or("修改")),
        "manual_add" => format!("手动添加 {m}（{}），从 {}开始收费", d["region"].as_str().unwrap_or(""), cn(&d["start_date"])),
        "manual_end" => format!("{m} 停止收费，{}", until(d)),
        "exempt" if d["exempt"] == true => format!("{m} 设为不收费"),
        "exempt" => format!("{m} 恢复收费"),
        "paid" => paid_text("已收", d),
        "unpaid" => paid_text("改回没收", d),
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
    due_date: Option<Date>,
    cycle: Option<String>,
}

/// 团队现在的可计费节点，带订阅（自己的，没有就宿主机的；周期跟随套餐时是套餐的，见 subscription_eff）。
/// 地区不在这里查：每轮每团队对所有节点查 IP 库太贵，见 `regions`
pub async fn billable_nodes(c: &mut PgConnection, team: Uuid) -> sqlx::Result<Vec<Node>> {
    let rows: Vec<NodeRow> = sqlx::query_as(&format!(
        "select m.id, display_name(m.kind::text, m.label, t.name, m.number) name, m.country, host(m.primary_ip) ip, sub.due_date, sub.cycle
         from machine m join team t on t.id = m.team_id
         left join lateral (select s.due_date, s.cycle::text cycle from subscription_eff s where s.machine_id in (m.id, m.host_id)
                            order by s.machine_id = m.id desc limit 1) sub on true
         where m.team_id = $1 and {BILLABLE} order by m.number, m.id"
    ))
    .bind(team)
    .fetch_all(c)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Node {
            id: r.id,
            name: r.name,
            country: r.country,
            ip: r.ip.as_deref().and_then(|ip| ip.parse().ok()),
            sub: r.due_date.zip(r.cycle.as_deref().and_then(meta::cycle_months)),
        })
        .collect())
}

/// 这些节点的地区：手动指定的国家优先，否则按地址查离线 IP 库（只在要建名额时调）
pub async fn regions(st: &AppState, nodes: &[&Node]) -> HashMap<Uuid, String> {
    let ips: Vec<IpAddr> = nodes.iter().filter(|n| n.country.is_none()).filter_map(|n| n.ip).collect();
    let intel = if ips.is_empty() { HashMap::new() } else { st.ipintel.enrich(&st.db, &ips, true).await };
    nodes
        .iter()
        .map(|n| {
            let auto = n.ip.and_then(|ip| intel.get(&ip)).and_then(|b| b.country.clone());
            (n.id, country::region(n.country.clone().or(auto).as_deref()))
        })
        .collect()
}

/// 建一个系统名额（节点占着，从开始收费日算），不记事件（初始化一个团队只记一条合并的 init，见 `log_init`）
pub async fn create_slot(c: &mut PgConnection, team: Uuid, node: &Node, region: &str, start: Date) -> sqlx::Result<Uuid> {
    let id = new_id();
    sqlx::query("insert into billing_slot (id, team_id, region, start_date, holder_id, holder_name) values ($1, $2, $3, $4, $5, $6)")
        .bind(id)
        .bind(team)
        .bind(region)
        .bind(start)
        .bind(node.id)
        .bind(&node.name)
        .execute(&mut *c)
        .await?;
    Ok(id)
}

/// 对账新增的名额：建名额并记一条 add
pub async fn insert_slot(c: &mut PgConnection, team: Uuid, node: &Node, region: &str, start: Date, at: OffsetDateTime) -> sqlx::Result<Uuid> {
    let id = create_slot(c, team, node, region, start).await?;
    log(c, team, Some(id), at, "add", Some(&node.name), None, json!({"region": region, "start_date": bt::fmt_date(start)})).await?;
    Ok(id)
}

/// 名额结束（占用者离开）：结束日 = end，离开原因按这台机器此刻是否标了不计费落库（exempt = 节点还在团队里只是不计费；
/// left = 离开团队、退役、进备用或被删），记 remove（收费到哪天）
pub async fn end_slot(c: &mut PgConnection, team: Uuid, slot: Uuid, prev: Option<Uuid>, name: Option<&str>, end: Date, at: OffsetDateTime) -> sqlx::Result<()> {
    let (start, region): (Date, String) = sqlx::query_as(
        "update billing_slot set holder_id = null, prev_holder_id = coalesce($3, prev_holder_id), vacated_at = $4, end_date = $2,
             vacate_reason = case when exists (select 1 from billing_exempt x where x.machine_id = coalesce($3, prev_holder_id)) then 'exempt' else 'left' end
         where id = $1 returning start_date, region",
    )
    .bind(slot)
    .bind(end)
    .bind(prev)
    .bind(at)
    .fetch_one(&mut *c)
    .await?;
    let until = bt::billed_until(start, end).map(bt::fmt_date);
    log(c, team, Some(slot), at, "remove", name, None, json!({"region": region, "end_date": bt::fmt_date(end), "billed_until": until})).await
}

/// 初始化一个团队只记一条：节点名单（名字与地区）放在 detail.nodes，不显示在文案里
pub async fn log_init(c: &mut PgConnection, team: Uuid, start: Date, nodes: &[(&Node, &str)], at: OffsetDateTime) -> sqlx::Result<()> {
    let list: Vec<Value> = nodes.iter().map(|(n, r)| json!({"name": n.name, "region": r})).collect();
    log(c, team, None, at, "init", None, None, json!({"start_date": bt::fmt_date(start), "count": nodes.len(), "nodes": list})).await
}

/// 替换节点（jobs/steps/replace.rs 交换编号的那一步，同一个事务里调；调用方已按团队取了计费锁）：新节点 `new` 接过旧节点 `old` 在团队 `team`
/// 里的名额，开始收费日、地区、已收标记都不变，记 swap（「新节点原名 补位接替 旧节点原名，收费不变」，名字是交换编号前的）。
/// 新节点自己的名额：同一个团队的（上架进团队后对账建的）删掉，连同它那条 add，它已收的期挪到接过的名额同一个月的那一期上；
/// 别的团队的（从别的团队调过来）那边按离开结束。旧节点之后去备用就不再占名额，不另记 remove。
/// 旧节点在这个团队没有名额（还没初始化、标了不计费）什么都不做。返回接过没有
pub async fn take_over(c: &mut PgConnection, team: Uuid, old: Uuid, new: Uuid, (old_name, new_name): (&str, &str), now: OffsetDateTime) -> sqlx::Result<bool> {
    let slot: Option<(Uuid, Date, String)> = sqlx::query_as("select id, start_date, region from billing_slot where team_id = $1 and holder_id = $2 and end_date is null")
        .bind(team)
        .bind(old)
        .fetch_optional(&mut *c)
        .await?;
    let Some((slot, start, region)) = slot else { return Ok(false) };
    let mine: Option<(Uuid, Uuid, Option<String>)> =
        sqlx::query_as("select id, team_id, holder_name from billing_slot where holder_id = $1 and end_date is null").bind(new).fetch_optional(&mut *c).await?;
    if let Some((id, t, name)) = mine {
        if t == team {
            let paid: Vec<(Date, OffsetDateTime)> = sqlx::query_as("select period_start, paid_at from billing_paid where slot_id = $1").bind(id).fetch_all(&mut *c).await?;
            for (ps, at) in paid {
                if let Some((to, _)) = bt::period_in(start, None, bt::month_of(ps)) {
                    sqlx::query("insert into billing_paid (slot_id, period_start, paid_at) values ($1, $2, $3) on conflict do nothing").bind(slot).bind(to).bind(at).execute(&mut *c).await?;
                }
            }
            sqlx::query("delete from billing_event where slot_id = $1 and kind = 'add'").bind(id).execute(&mut *c).await?;
            sqlx::query("delete from billing_slot where id = $1").bind(id).execute(&mut *c).await?;
        } else {
            end_slot(c, t, id, Some(new), name.as_deref(), org_date(now, meta::org_offset()), now).await?;
        }
    }
    sqlx::query(
        "update billing_slot set holder_id = $2,
             holder_name = (select display_name(m.kind::text, m.label, t.name, m.number) from machine m left join team t on t.id = m.team_id where m.id = $2)
         where id = $1",
    )
    .bind(slot)
    .bind(new)
    .execute(&mut *c)
    .await?;
    log(c, team, Some(slot), now, "swap", Some(new_name), Some(old_name), json!({"region": region, "start_date": bt::fmt_date(start)})).await?;
    Ok(true)
}

#[derive(sqlx::FromRow)]
struct SlotRow {
    id: Uuid,
    holder_id: Option<Uuid>,
    holder_name: Option<String>,
    manual: bool,
}

/// 初始化过没有（没有就不对账）
pub async fn initialized(db: &PgPool) -> sqlx::Result<bool> {
    sqlx::query_scalar("select exists (select 1 from app_setting where key = $1)").bind(SINCE_KEY).fetch_one(db).await
}

/// 对一个团队（一个事务）。返回记了几条增减记录
pub async fn reconcile_team(st: &AppState, team: Uuid, now: OffsetDateTime) -> anyhow::Result<usize> {
    let off = meta::org_offset();
    let mut tx = st.db.begin().await?;
    lock_team(&mut tx, team).await?;
    let spare: Option<bool> = sqlx::query_scalar("select spare from team where id = $1").bind(team).fetch_optional(&mut *tx).await?;
    if spare != Some(false) {
        return Ok(0);
    }
    let billable = billable_nodes(&mut tx, team).await?;
    let rows: Vec<SlotRow> = sqlx::query_as("select id, holder_id, holder_name, manual from billing_slot where team_id = $1 and end_date is null").bind(team).fetch_all(&mut *tx).await?;
    let ids: Vec<Uuid> = billable.iter().map(|n| n.id).collect();
    let held_elsewhere: HashSet<Uuid> =
        sqlx::query_scalar("select holder_id from billing_slot where end_date is null and team_id <> $1 and holder_id = any($2)").bind(team).bind(&ids).fetch_all(&mut *tx).await?.into_iter().collect();
    let ended: Vec<Ended> = sqlx::query_as::<_, (Uuid, Uuid, Date, Date)>(
        "select distinct on (prev_holder_id) id, prev_holder_id, start_date, end_date from billing_slot
         where team_id = $1 and not manual and end_date is not null and prev_holder_id = any($2)
         order by prev_holder_id, end_date desc, created_at desc",
    )
    .bind(team)
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|(slot, machine, start, end)| Ended { slot, machine, billed_until: bt::billed_until(start, end) })
    .collect();
    let slots: Vec<OpenSlot> = rows.iter().map(|r| OpenSlot { id: r.id, holder: r.holder_id, holder_name: r.holder_name.clone(), manual: r.manual }).collect();
    let changes = plan(&slots, &ended, &billable, &held_elsewhere, now, off);
    // 只给要新建名额的节点查地区
    let adds: Vec<&Node> = changes.iter().filter_map(|c| if let Change::Add { machine, .. } = c { billable.iter().find(|n| n.id == *machine) } else { None }).collect();
    let regions = regions(st, &adds).await;
    let mut events = 0;
    for ch in &changes {
        match ch {
            Change::Rename { slot, name } => {
                sqlx::query("update billing_slot set holder_name = $2 where id = $1").bind(slot).bind(name).execute(&mut *tx).await?;
            }
            Change::End { slot, prev, name, end } => {
                end_slot(&mut tx, team, *slot, *prev, name.as_deref(), *end, now).await?;
                events += 1;
            }
            Change::Restore { slot, machine, name } => {
                sqlx::query("update billing_slot set holder_id = $2, holder_name = $3, end_date = null, vacated_at = null, prev_holder_id = null, vacate_reason = null where id = $1")
                    .bind(slot)
                    .bind(machine)
                    .bind(name)
                    .execute(&mut *tx)
                    .await?;
            }
            Change::Add { machine, start } => {
                if let Some(n) = billable.iter().find(|n| n.id == *machine) {
                    insert_slot(&mut tx, team, n, &regions[machine], *start, now).await?;
                    events += 1;
                }
            }
        }
    }
    tx.commit().await?;
    if !changes.is_empty() {
        st.emit(Event::Billing { id: team });
    }
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
    use time::macros::{date, datetime, offset};
    use time::Duration;

    const OFF: UtcOffset = offset!(+8);

    fn id(n: u128) -> Uuid { Uuid::from_u128(n) }

    fn node(n: u128, name: &str) -> Node { Node { id: id(n), name: name.into(), country: Some("US".into()), ip: None, sub: None } }

    fn held(slot: u128, holder: u128, name: &str) -> OpenSlot { OpenSlot { id: id(slot), holder: Some(id(holder)), holder_name: Some(name.into()), manual: false } }

    fn run(slots: &[OpenSlot], ended: &[Ended], nodes: &[Node], now: OffsetDateTime) -> Vec<Change> { plan(slots, ended, nodes, &HashSet::new(), now, OFF) }

    /// 北京时间 2026-11-03 10:00
    const T0: OffsetDateTime = datetime!(2026-11-03 02:00 UTC);

    #[test]
    fn steady_state_changes_nothing() {
        assert!(run(&[held(1, 10, "团队1-01")], &[], &[node(10, "团队1-01")], T0).is_empty());
        // 改了编号：只刷新名字
        assert_eq!(run(&[held(1, 10, "团队1-01")], &[], &[node(10, "团队1-02")], T0), [Change::Rename { slot: id(1), name: "团队1-02".into() }]);
    }

    #[test]
    fn purchase_date_from_subscription() {
        let today = date!(2026 - 11 - 03);
        // 到期日往前推一个周期（月底对齐，同每一期的 add_months）
        assert_eq!(purchase_date(Some((date!(2026 - 11 - 20), 1)), today), date!(2026 - 10 - 20));
        assert_eq!(purchase_date(Some((date!(2026 - 11 - 30), 3)), today), date!(2026 - 08 - 30));
        assert_eq!(purchase_date(Some((date!(2027 - 05 - 01), 6)), today), date!(2026 - 11 - 01));
        assert_eq!(purchase_date(Some((date!(2027 - 10 - 15), 12)), today), date!(2026 - 10 - 15));
        assert_eq!(purchase_date(Some((date!(2026 - 03 - 31), 1)), today), date!(2026 - 02 - 28));
        // 推出来晚于今天（到期日在一个周期以后）取今天；没有订阅的是今天（加入那天）
        assert_eq!(purchase_date(Some((date!(2026 - 12 - 20), 1)), today), today);
        assert_eq!(purchase_date(None, today), today);
    }

    #[test]
    fn joining_bills_at_once() {
        // 规则 1：新节点这一轮就建名额，不等；有订阅的从购买日，没有的从今天（北京日期：UTC 11-02 17:00 = 北京 11-03 01:00）
        let mut a = node(11, "团队1-10");
        a.sub = Some((date!(2026 - 11 - 25), 1));
        let c = run(&[held(1, 10, "团队1-01")], &[], &[node(10, "团队1-01"), a, node(12, "团队1-11")], datetime!(2026-11-02 17:00 UTC));
        assert_eq!(c, [Change::Add { machine: id(11), start: date!(2026 - 10 - 25) }, Change::Add { machine: id(12), start: date!(2026 - 11 - 03) }]);
    }

    #[test]
    fn leaving_ends_at_once() {
        // 规则 2：进备用、退役、标不计费、改去别的团队：都只是不在这个团队的可计费集合里 → 今天结束
        let c = run(&[held(1, 10, "团队1-03"), held(2, 11, "团队1-04")], &[], &[node(11, "团队1-04")], T0);
        assert_eq!(c, [Change::End { slot: id(1), prev: Some(id(10)), name: Some("团队1-03".into()), end: date!(2026 - 11 - 03) }]);
        // 机器被物理删除（外键置空）：一样结束，原占用者不知道
        let gone = OpenSlot { id: id(2), holder: None, holder_name: Some("团队1-05".into()), manual: false };
        assert_eq!(run(&[gone], &[], &[], T0), [Change::End { slot: id(2), prev: None, name: Some("团队1-05".into()), end: date!(2026 - 11 - 03) }]);
        // 手动名额不动
        let manual = OpenSlot { id: id(3), holder: None, holder_name: None, manual: true };
        assert!(run(&[manual], &[], &[], T0).is_empty());
    }

    #[test]
    fn no_guessed_pairing() {
        // 规则 3：同一轮一个走、一个来，不猜是补位：一条结束、一条新增（补位只认替换作业的 take_over）
        let c = run(&[held(1, 10, "团队2-07")], &[], &[node(11, "团队2-20")], T0);
        assert_eq!(
            c,
            [Change::End { slot: id(1), prev: Some(id(10)), name: Some("团队2-07".into()), end: date!(2026 - 11 - 03) }, Change::Add { machine: id(11), start: date!(2026 - 11 - 03) }]
        );
        // 替换作业接过名额之后（新节点占着旧名额、旧节点进了备用）：什么都不变
        assert!(run(&[held(1, 11, "团队2-07")], &[], &[node(11, "团队2-07")], T0).is_empty());
    }

    #[test]
    fn returning_node_restored_within_paid_period() {
        // 规则 4：最近结束的名额收费到今天或以后 → 原样接回，不记事件
        let back = |until: Option<Date>| run(&[], &[Ended { slot: id(1), machine: id(10), billed_until: until }], &[node(10, "团队1-03")], T0);
        let restore = [Change::Restore { slot: id(1), machine: id(10), name: "团队1-03".into() }];
        assert_eq!(back(Some(date!(2026 - 11 - 03))), restore);
        assert_eq!(back(Some(date!(2026 - 11 - 29))), restore);
        // 收费期已经过了 / 一期都没收：按新加入建名额
        assert_eq!(back(Some(date!(2026 - 11 - 02))), [Change::Add { machine: id(10), start: date!(2026 - 11 - 03) }]);
        assert_eq!(back(None), [Change::Add { machine: id(10), start: date!(2026 - 11 - 03) }]);
        // 别的节点结束的名额不接
        let c = run(&[], &[Ended { slot: id(1), machine: id(10), billed_until: Some(date!(2026 - 11 - 29)) }], &[node(11, "团队1-04")], T0);
        assert!(matches!(c.as_slice(), [Change::Add { .. }]), "{c:?}");
    }

    #[test]
    fn moved_team_waits_for_old_team() {
        // 新团队这边：还占着原团队的名额（那边这一轮还没对账）先不动，下一轮再建
        let elsewhere: HashSet<Uuid> = [id(10)].into();
        assert!(plan(&[], &[], &[node(10, "团队2-30")], &elsewhere, T0, OFF).is_empty());
        assert_eq!(plan(&[], &[], &[node(10, "团队2-30")], &HashSet::new(), T0 + Duration::minutes(1), OFF).len(), 1);
    }

    #[test]
    fn texts() {
        assert_eq!(event_text("add", Some("团队1-10"), None, &json!({"start_date": "2026-10-30"})), "团队1-10 加入，从 10 月 30 日开始收费");
        assert_eq!(event_text("swap", Some("团队2-20"), Some("团队2-07"), &json!({})), "团队2-20 补位接替 团队2-07，收费不变");
        assert_eq!(event_text("remove", Some("团队2-05"), None, &json!({"end_date": "2026-11-20", "billed_until": "2026-11-29"})), "团队2-05 离开，收费到 11 月 29 日");
        assert_eq!(event_text("remove", Some("团队2-05"), None, &json!({"end_date": "2026-11-14", "billed_until": null})), "团队2-05 离开，没收过费");
        assert_eq!(event_text("init", None, None, &json!({"start_date": "2026-10-01", "count": 52})), "开始记账：52 个节点从 10 月 1 日开始收费");
        assert_eq!(event_text("exempt", Some("团队1-05"), None, &json!({"exempt": true})), "团队1-05 设为不收费");
        let names: Vec<String> = (1..=12).map(|i| format!("团队1-{i:02}")).collect();
        assert_eq!(event_text("paid", None, None, &json!({"month": "2026-11", "names": ["团队1-01", "团队1-02"]})), "11 月已收：团队1-01、团队1-02（共 2 个）");
        assert!(event_text("unpaid", None, None, &json!({"month": "2026-11", "names": names})).ends_with("团队1-10 等（共 12 个）"));
        assert_eq!(manual_name("墨西哥家宽"), "墨西哥家宽");
        assert_eq!(manual_name(""), "手动添加的节点");
    }
}
