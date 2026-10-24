//! 生命周期操作：换线路机（move_entry，单个或批量）、搬空线路机（evacuate）。
//! 都是作业：这里只做校验、预览、建作业和写审计，真正的步骤在 jobs/steps。
//!
//! 软路由：指向要摘掉的旧入口的出口，在新入口验收之后、摘旧入口之前改成新入口（作业的「改软路由出口」一步，
//! 和替换节点同一套：全部在线才动手，逐台备份 → 改 → 重启 OpenClash → 测延迟，一台失败已改的全部换回、旧入口不摘）。
//! 预览按快照列出要改的出口；执行时按路由当时的配置重新确定（`move_exit`），同一台路由同时只有一个作业在改。
//! 搬空、批量换线路机时各节点的作业参数带同一个 `batch`：都验收完新入口后由一个带头的一起改（每台路由只改一次、重启一次，见 jobs/steps/batch.rs）。
//! 出口写的是旧线路机的 IP 就改成新线路机的 IP，否则改成新线路机的 public_host；端口是新入口的。
//! `routers = false` 只换入口，路由上的出口留着指向旧入口（摘掉后就断了）。

use super::ok;
use super::replace::{new_server, routers_view, scan_routers, ExitPlan, RouterPlan};
use crate::auth::Actor;
use crate::db::{audit, Tx};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::jobs;
use crate::meta;
use crate::names::display_name;
use crate::routers::clash::EntryRef;
use crate::health::Status;
use crate::placement::Score;
use crate::quota::{best_target, pick_moves, Q};
use crate::state::{AppState, Event};
use crate::views::{self, EntryView, MachineView};
use std::collections::{HashMap, HashSet};
use time::OffsetDateTime;
use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

/// 受影响的是换走的入口（线路机和端口都变，列在 `old_entry`）
pub(crate) const SHARE_NOTE: &str = "入口换到新线路机、端口也会变：用订阅链接的客户端更新订阅后自动换成新入口，用分享链接的要换成新的分享链接（作业完成后到节点的入口里复制，或 fleetctl share <节点>）";

// ── 换线路机 ──────────────────────────────────────────────────────────────

/// (kind, lifecycle, label, 团队 id, 团队名, 编号)
type NodeRow = (String, String, Option<String>, Option<Uuid>, Option<String>, Option<i32>);

struct MovePlan {
    node_name: String,
    from_relay: Uuid,
    from_name: String,
    to_name: String,
    old_entry: Uuid,
    old_port: i32,
    routers_enabled: bool,
    /// 指向旧入口、要改成新入口的出口
    routers: Vec<RouterPlan>,
    /// 改不了路由的原因（路由不在线…）：不能发起
    blockers: Vec<String>,
    warnings: Vec<String>,
}

impl MovePlan {
    fn view(&self) -> Value {
        json!({
            "name": self.node_name, "from": self.from_name, "to": self.to_name, "old_entry": self.old_entry_view(),
            "routers_enabled": self.routers_enabled, "routers": routers_view(&self.routers),
            "ok": self.blockers.is_empty(), "blockers": self.blockers, "warnings": self.warnings,
            "share_changed": true, "note": SHARE_NOTE,
        })
    }

    /// 换走的入口（新入口的端口建好时才分配，作业完成后见作业的 result）
    fn old_entry_view(&self) -> Value { json!({"id": self.old_entry, "relay": self.from_name, "port": self.old_port}) }
}

/// 校验并规划一次换线路机。外层 Err = 数据库/不存在；内层 Err = 业务上不能换（给用户看的原因，evacuate 用它做「跳过」）。
/// `from`：要换掉哪个线路机上的入口；None = 节点必须恰有一个在用入口。`routers`：各路由（含最近的快照），`routers_on` = 要改路由
async fn plan_move(
    tx: &mut Tx<'_>,
    node_id: Uuid,
    from: Option<Uuid>,
    to: Uuid,
    routers: &[crate::routers::Row],
    routers_on: bool,
) -> ApiResult<Result<MovePlan, String>> {
    let node: Option<NodeRow> = sqlx::query_as(
        "select m.kind::text, m.lifecycle::text, m.label, m.team_id, t.name, m.number from machine m left join team t on t.id = m.team_id
         where m.id = $1 for share of m",
    )
    .bind(node_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((kind, lc, label, team_id, team, number)) = node else { return Err(ApiError::not_found("没有这台机器")) };
    let name = display_name(&kind, label.as_deref(), team.as_deref(), number);
    if kind == "relay" {
        return Ok(Err(format!("「{name}」是线路机，只有节点（VPS 节点 / 家宽代理）能换线路机")));
    }
    if lc != "active" {
        return Ok(Err(format!("「{name}」现在是「{}」，只有在用的节点能换线路机", meta::lifecycle_label(&lc))));
    }
    let target: Option<(String, String)> =
        sqlx::query_as("select coalesce(label, ''), lifecycle::text from machine where id = $1 and kind = 'relay' for share").bind(to).fetch_optional(&mut **tx).await?;
    let Some((to_name, to_lc)) = target else { return Ok(Err("目标线路机不存在".into())) };
    if to_lc != "active" {
        return Ok(Err(format!("目标线路机「{to_name}」现在是「{}」，只能换到在用的线路机", meta::lifecycle_label(&to_lc))));
    }
    // 替换、改出口指向作业的目标是旧节点：这台是它的新节点时也算忙（它在改路由、交换编号，路由刚改成指向这台的入口）
    let busy: Option<(String,)> = sqlx::query_as(
        "select kind from job where state in ('queued', 'running', 'waiting', 'cancelling')
           and (target_id = $1 or (kind in ('replace', 'repoint') and params->>'new_id' = $1::text)) limit 1",
    )
    .bind(node_id)
        .fetch_optional(&mut **tx)
        .await?;
    if let Some((k,)) = busy {
        return Ok(Err(format!("「{name}」还有进行中的作业（{}）：等它结束再换线路机", meta::job_kind_label(&k))));
    }
    let live: Vec<(Uuid, Uuid, String, String, i32)> = sqlx::query_as(
        "select e.id, e.relay_id, e.lifecycle::text, coalesce(rm.label, ''), e.port from entry e join machine rm on rm.id = e.relay_id
         where e.node_id = $1 and e.lifecycle in ('pending', 'active', 'disabled') order by e.created_at",
    )
    .bind(node_id)
    .fetch_all(&mut **tx)
    .await?;
    let cands: Vec<&(Uuid, Uuid, String, String, i32)> = live.iter().filter(|e| from.is_none_or(|f| f == e.1)).collect();
    let (old_entry, from_relay, elc, from_name, old_port) = match cands.as_slice() {
        [] => return Ok(Err(format!("「{name}」{}没有在用的入口", if from.is_some() { "在指定的线路机上" } else { "" }))),
        [one] => (*one).clone(),
        many => {
            let relays: Vec<&str> = many.iter().map(|e| e.3.as_str()).collect();
            return Ok(Err(format!("「{name}」有 {} 个入口（{}）：请指定要换掉哪条线路机上的入口（from_relay_id）", many.len(), relays.join("、"))));
        }
    };
    if elc != "active" {
        return Ok(Err(format!("「{name}」在 {from_name} 上的入口现在是「{}」：先启用或等它应用完再换线路机", crate::entries::state_zh(&elc))));
    }
    if from_relay == to {
        return Ok(Err(format!("目标线路机「{to_name}」就是节点当前所在的线路机")));
    }
    let dup: Option<(Uuid,)> = sqlx::query_as("select id from entry where node_id = $1 and relay_id = $2 and lifecycle <> 'removed'")
        .bind(node_id)
        .bind(to)
        .fetch_optional(&mut **tx)
        .await?;
    if dup.is_some() {
        return Ok(Err(format!("「{name}」在「{to_name}」上已经有入口")));
    }
    // 软路由上指向旧入口的出口：server 按旧线路机的 IP / 新线路机的 IP、public_host 换
    let (from_ip, _) = relay_addr(&mut **tx, from_relay).await?;
    let to_addr = relay_addr(&mut **tx, to).await?;
    let (mut blockers, mut warnings) = (vec![], vec![]);
    let rs = scan_routers(routers, node_id, team_id, routers_on, routers_on, ("换线路机", "这个入口"), &mut blockers, &mut warnings, |er, srv, port| {
        move_exit(er, old_entry, None, &from_ip, &to_addr, srv, port).map(Ok)
    });
    Ok(Ok(MovePlan { node_name: name, from_relay, from_name, to_name, old_entry, old_port, routers_enabled: routers_on, routers: rs, blockers, warnings }))
}

/// 线路机的 (IP, public_host)
pub async fn relay_addr<'e>(db: impl sqlx::PgExecutor<'e>, id: Uuid) -> sqlx::Result<(String, String)> {
    sqlx::query_as("select host(m.primary_ip), r.public_host from relay r join machine m on m.id = r.machine_id where r.machine_id = $1").bind(id).fetch_one(db).await
}

/// 换线路机：对上旧入口 `old_entry` 的出口（server:port 是 `srv:port`）改成什么，不是旧入口返回 None（预览与执行共用）。
/// `new`：新入口与端口（预览时还没建）；`from_ip`：旧线路机的 IP；`to`：新线路机的 (IP, public_host)
pub fn move_exit(er: &EntryRef, old_entry: Uuid, new: Option<(Uuid, u16)>, from_ip: &str, to: &(String, String), srv: &str, port: u16) -> Option<ExitPlan> {
    (er.id == old_entry).then(|| ExitPlan {
        exit: String::new(),
        old_entry_id: old_entry,
        new_entry_id: new.map(|n| n.0),
        old_server: srv.to_string(),
        old_port: port,
        server: new_server(srv, from_ip, &to.0, &to.1),
        port: new.map(|n| n.1),
        error: None,
    })
}

/// `batch`：同批换线路机的标识（一起改软路由），单独换线路机为 None
async fn enqueue_move(tx: &mut Tx<'_>, actor: &str, node: Uuid, p: &MovePlan, to: Uuid, via: &str, batch: Option<Uuid>) -> ApiResult<Uuid> {
    let routers = if p.routers_enabled { p.routers.as_slice() } else { &[] };
    let mut params = json!({
        "from_relay_id": p.from_relay, "to_relay_id": to, "old_entry_id": p.old_entry,
        "routers": p.routers_enabled, "plan": {"routers": serde_json::to_value(routers).unwrap_or_default()},
    });
    if let Some(b) = batch {
        params["batch"] = json!(b);
    }
    let job = jobs::enqueue(tx, "move_entry", Some(node), params, actor).await?;
    let changed: Vec<Value> = routers.iter().map(|r| json!({"id": r.id, "name": r.name, "exits": r.exits.iter().map(|e| &e.exit).collect::<Vec<_>>()})).collect();
    audit(
        tx,
        actor,
        "entry.move",
        "machine",
        Some(node),
        json!({"name": p.node_name, "from": p.from_name, "to": p.to_name, "from_relay_id": p.from_relay, "to_relay_id": to, "old_entry_id": p.old_entry, "job_id": job, "via": via,
               "routers": changed, "routers_enabled": p.routers_enabled, "batch": batch}),
    )
    .await?;
    Ok(job)
}

#[derive(Deserialize)]
pub struct MoveIn {
    to_relay_id: Uuid,
    /// 节点有多个入口时，指定换掉哪条线路机上的
    from_relay_id: Option<Uuid>,
    /// 改软路由（默认 true）
    routers: Option<bool>,
}

/// 预览：要换掉的入口、要改的软路由出口、不能换的原因（不写库；规划时对节点、线路机加的共享锁随只读事务结束放开）
pub async fn move_preview(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiQuery(q): ApiQuery<MoveIn>) -> ApiResult<Json<Value>> {
    let routers = crate::routers::load(&st.db, None).await?;
    let on = q.routers != Some(false);
    let mut tx = st.db.begin().await?;
    Ok(ok(match plan_move(&mut tx, id, q.from_relay_id, q.to_relay_id, &routers, on).await? {
        Ok(p) => p.view(),
        Err(why) => json!({"ok": false, "blockers": [why], "warnings": [], "routers_enabled": on, "routers": [], "share_changed": true, "note": SHARE_NOTE}),
    }))
}

/// 把节点换到另一台线路机：新入口验收通过后才改软路由、摘旧入口，期间不断网。
pub async fn move_node(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<MoveIn>) -> ApiResult<Json<Value>> {
    let routers = crate::routers::load(&st.db, None).await?;
    let mut tx = st.db.begin().await?;
    let plan = plan_move(&mut tx, id, b.from_relay_id, b.to_relay_id, &routers, b.routers != Some(false)).await?.map_err(ApiError::conflict)?;
    if !plan.blockers.is_empty() {
        return Err(ApiError::conflict(plan.blockers.join("；")).with_detail(json!({"blockers": plan.blockers})));
    }
    let job = enqueue_move(&mut tx, &a.name, id, &plan, b.to_relay_id, "move", None).await?;
    tx.commit().await?;
    st.wake_jobs();
    st.emit(Event::Job { id: job, target: Some(id) });
    let mut v = plan.view();
    v["job_id"] = json!(job);
    Ok(ok(v))
}

#[derive(Deserialize)]
pub struct EvacuateIn {
    to_relay_id: Uuid,
    /// 改软路由（默认 true）
    routers: Option<bool>,
}

/// 搬空的计划：能搬的节点各自的换线路机计划，和暂时不能搬的（原因）
struct Evac {
    from_name: String,
    to_name: String,
    plans: Vec<(Uuid, MovePlan)>,
    skipped: Vec<Value>,
}

impl Evac {
    fn view(&self, jobs: Option<&[Uuid]>) -> Value {
        let list: Vec<Value> = self
            .plans
            .iter()
            .enumerate()
            .map(|(i, (node, p))| {
                let mut v = json!({"machine_id": node, "name": p.node_name, "from": p.from_name, "old_entry": p.old_entry_view(), "routers": routers_view(&p.routers)});
                if let Some(j) = jobs {
                    v["job_id"] = json!(j[i]);
                }
                v
            })
            .collect();
        let mut warnings: Vec<&String> = self.plans.iter().flat_map(|(_, p)| &p.warnings).collect();
        warnings.sort();
        warnings.dedup();
        json!({"from": self.from_name, "to": self.to_name, "jobs": list, "skipped": self.skipped, "warnings": warnings,
               "router_restarts": self.router_restarts(), "share_changed": !self.plans.is_empty(), "note": SHARE_NOTE})
    }

    /// 要改的软路由：同批一起改，每台只重启一次 OpenClash（断开这台路由上的全部连接）；`nodes` 是有出口要改的节点数
    /// （按计划，不改软路由时为空）。`[{id, name, times: 1, nodes}]`，节点多的在前
    fn router_restarts(&self) -> Vec<Value> {
        let mut n: Vec<(uuid::Uuid, &str, usize)> = vec![];
        for r in self.plans.iter().filter(|(_, p)| p.routers_enabled).flat_map(|(_, p)| &p.routers) {
            match n.iter_mut().find(|x| x.0 == r.id) {
                Some(x) => x.2 += 1,
                None => n.push((r.id, &r.name, 1)),
            }
        }
        n.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(b.1)));
        n.into_iter().map(|(id, name, nodes)| json!({"id": id, "name": name, "times": 1, "nodes": nodes})).collect()
    }
}

async fn plan_evacuate(st: &AppState, tx: &mut Tx<'_>, relay: Uuid, to: Uuid, routers_on: bool) -> ApiResult<Evac> {
    if relay == to {
        return Err(ApiError::bad_request("目标线路机不能和要搬空的是同一台"));
    }
    let src: Option<(String, String, String)> = sqlx::query_as("select kind::text, coalesce(label, ''), lifecycle::text from machine where id = $1")
        .bind(relay)
        .fetch_optional(&mut **tx)
        .await?;
    let (kind, from_name, lc) = src.ok_or_else(|| ApiError::not_found("没有这台机器"))?;
    if kind != "relay" {
        return Err(ApiError::conflict("只有线路机能搬空"));
    }
    if lc == "retired" {
        return Err(ApiError::conflict("这台线路机已经退役"));
    }
    let to_row: Option<(String, String)> = sqlx::query_as("select coalesce(label, ''), lifecycle::text from machine where id = $1 and kind = 'relay'")
        .bind(to)
        .fetch_optional(&mut **tx)
        .await?;
    let (to_name, to_lc) = to_row.ok_or_else(|| ApiError::not_found("目标线路机不存在"))?;
    if to_lc != "active" {
        return Err(ApiError::conflict(format!("目标线路机「{to_name}」现在是「{}」，只能搬到在用的线路机", meta::lifecycle_label(&to_lc))));
    }
    let nodes: Vec<(Uuid,)> = sqlx::query_as(
        "select distinct e.node_id from entry e join machine n on n.id = e.node_id
         where e.relay_id = $1 and e.lifecycle in ('pending', 'active', 'disabled') and n.kind <> 'relay' order by 1",
    )
    .bind(relay)
    .fetch_all(&mut **tx)
    .await?;
    let routers = crate::routers::load(&st.db, None).await?;
    let mut e = Evac { from_name, to_name, plans: vec![], skipped: vec![] };
    for (node,) in nodes {
        e.plan(tx, node, Some(relay), to, &routers, routers_on).await?;
    }
    Ok(e)
}

impl Evac {
    /// 规划一个节点：能换的进 plans，不能换的连同原因进 skipped
    async fn plan(&mut self, tx: &mut Tx<'_>, node: Uuid, from: Option<Uuid>, to: Uuid, routers: &[crate::routers::Row], routers_on: bool) -> ApiResult<()> {
        let reason = match plan_move(tx, node, from, to, routers, routers_on).await? {
            Ok(plan) if plan.blockers.is_empty() => {
                self.plans.push((node, plan));
                return Ok(());
            }
            Ok(plan) => plan.blockers.join("；"),
            Err(reason) => reason,
        };
        let name: Option<(String,)> = sqlx::query_as(
            "select display_name(m.kind::text, m.label, t.name, m.number)
             from machine m left join team t on t.id = m.team_id where m.id = $1",
        )
        .bind(node)
        .fetch_optional(&mut **tx)
        .await?;
        self.skipped.push(json!({"machine_id": node, "name": name.map(|(n,)| n), "reason": reason}));
        Ok(())
    }
}

/// 搬空预览：会搬哪些节点、各自要改的软路由出口、哪些暂时不能搬（不写库，同 move_preview）
pub async fn evacuate_preview(_a: Actor, State(st): State<AppState>, ApiPath(relay): ApiPath<Uuid>, ApiQuery(q): ApiQuery<EvacuateIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let e = plan_evacuate(&st, &mut tx, relay, q.to_relay_id, q.routers != Some(false)).await?;
    Ok(ok(e.view(None)))
}

/// 搬空线路机：为这台线路机上每个在用入口的节点各建一个换线路机作业（线路机退役前用）。
/// 暂时不能搬的节点（停用、有别的作业、已在目标线路机上有入口、软路由不在线…）列在 skipped 里，不影响其它节点。
/// 各作业并行建新入口、验收；「改软路由出口」一步同批一起做：都验收完后由一个带头的一次改完，每台路由只重启一次
/// （任何一台失败全部换回、几个节点的旧入口都不摘，见 jobs/steps/batch.rs）
pub async fn evacuate(a: Actor, State(st): State<AppState>, ApiPath(relay): ApiPath<Uuid>, ApiJson(b): ApiJson<EvacuateIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let e = plan_evacuate(&st, &mut tx, relay, b.to_relay_id, b.routers != Some(false)).await?;
    let batch = crate::db::new_id();
    let mut created = vec![];
    for (node, plan) in &e.plans {
        created.push(enqueue_move(&mut tx, &a.name, *node, plan, b.to_relay_id, "evacuate", Some(batch)).await?);
    }
    let v = e.view(Some(&created));
    audit(
        &mut tx,
        &a.name,
        "relay.evacuate",
        "machine",
        Some(relay),
        json!({"from": e.from_name, "to": e.to_name, "to_relay_id": b.to_relay_id,
               "jobs": e.plans.iter().zip(&created).map(|((n, p), j)| json!({"machine_id": n, "name": p.node_name, "job_id": j})).collect::<Vec<_>>(),
               "skipped": e.skipped}),
    )
    .await?;
    tx.commit().await?;
    st.wake_jobs();
    for ((node, _), job) in e.plans.iter().zip(&created) {
        st.emit(Event::Job { id: *job, target: Some(*node) });
    }
    st.emit(Event::Machines);
    Ok(ok(v))
}

// ── 批量换线路机 ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct MovesIn {
    machine_ids: Vec<Uuid>,
    to_relay_id: Uuid,
    /// 都换掉这台线路机上的入口（节点有多个入口时要填）
    from_relay_id: Option<Uuid>,
    /// 改软路由（默认 true）
    routers: Option<bool>,
}

/// 预览的查询参数：machine_ids 逗号分隔
#[derive(Deserialize)]
pub struct MovesQ {
    machine_ids: String,
    to_relay_id: Uuid,
    from_relay_id: Option<Uuid>,
    routers: Option<bool>,
}

/// 批量换线路机的计划：各节点的换线路机计划（同 plan_move），有一个不能换就不能发起（skipped 里写原因）
async fn plan_moves(st: &AppState, tx: &mut Tx<'_>, ids: &[Uuid], from: Option<Uuid>, to: Uuid, routers_on: bool) -> ApiResult<Evac> {
    if ids.is_empty() {
        return Err(ApiError::bad_request("要换的节点（machine_ids）不能为空"));
    }
    if ids.iter().collect::<HashSet<_>>().len() != ids.len() {
        return Err(ApiError::bad_request("machine_ids 里有重复的节点"));
    }
    let to_name: Option<String> = sqlx::query_scalar("select coalesce(label, '') from machine where id = $1 and kind = 'relay'").bind(to).fetch_optional(&mut **tx).await?;
    let to_name = to_name.ok_or_else(|| ApiError::not_found("目标线路机不存在"))?;
    let routers = crate::routers::load(&st.db, None).await?;
    let mut e = Evac { from_name: String::new(), to_name, plans: vec![], skipped: vec![] };
    for &node in ids {
        e.plan(tx, node, from, to, &routers, routers_on).await?;
    }
    let mut froms: Vec<&str> = e.plans.iter().map(|(_, p)| p.from_name.as_str()).collect();
    froms.sort();
    froms.dedup();
    e.from_name = froms.join("、");
    Ok(e)
}

/// 同搬空的结果，另有 ok / blockers（不能换的节点，前面带节点名）
fn moves_view(e: &Evac, jobs: Option<&[Uuid]>) -> Value {
    let mut v = e.view(jobs);
    let blockers: Vec<String> = e
        .skipped
        .iter()
        .map(|k| {
            let (n, r) = (k["name"].as_str().unwrap_or("节点"), k["reason"].as_str().unwrap_or(""));
            if r.contains(n) { r.to_string() } else { format!("{n}：{r}") }
        })
        .collect();
    v["ok"] = json!(blockers.is_empty());
    v["blockers"] = json!(blockers);
    v
}

/// 批量换线路机预览（不写库，同 move_preview）：各节点要改的软路由出口、每台路由只重启一次、不能换的原因
pub async fn moves_preview(_a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<MovesQ>) -> ApiResult<Json<Value>> {
    let ids = q
        .machine_ids
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().parse().map_err(|_| ApiError::bad_request("machine_ids 要填节点 id（逗号分隔）")))
        .collect::<Result<Vec<Uuid>, _>>()?;
    let mut tx = st.db.begin().await?;
    let e = plan_moves(&st, &mut tx, &ids, q.from_relay_id, q.to_relay_id, q.routers != Some(false)).await?;
    Ok(ok(moves_view(&e, None)))
}

/// 批量换线路机：几个节点一起换到同一台线路机，各建一个换线路机作业，参数带同一个 `batch`（同搬空）：
/// 都验收完新入口后由一个带头的一起改软路由，每台路由只改一次、OpenClash 只重启一次。有一个不能换就都不建（409，原因在 blockers）
pub async fn moves(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<MovesIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let e = plan_moves(&st, &mut tx, &b.machine_ids, b.from_relay_id, b.to_relay_id, b.routers != Some(false)).await?;
    let pv = moves_view(&e, None);
    if pv["ok"] != true {
        let why: Vec<&str> = pv["blockers"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        return Err(ApiError::conflict(why.join("；")).with_detail(json!({"blockers": pv["blockers"], "skipped": e.skipped})));
    }
    let batch = (e.plans.len() > 1).then(crate::db::new_id);
    let mut created = vec![];
    for (node, plan) in &e.plans {
        created.push(enqueue_move(&mut tx, &a.name, *node, plan, b.to_relay_id, "moves", batch).await?);
    }
    tx.commit().await?;
    st.wake_jobs();
    for ((node, _), job) in e.plans.iter().zip(&created) {
        st.emit(Event::Job { id: *job, target: Some(*node) });
    }
    st.emit(Event::Machines);
    Ok(ok(moves_view(&e, Some(&created))))
}

// ── 放置矩阵 ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PlacementQ {
    /// 团队 id 或名称；不填 = 全部
    team: Option<String>,
    /// 只看这个节点（id 或显示名）
    node: Option<String>,
}

/// 放置矩阵（只读，逻辑见 `crate::placement`）：每个在用 VPS 节点到各线路机近 24 小时的 p50 与成功率、当前与最优线路机，
/// 建议换线的按目标线路机合成一条批量 `fleetctl move A,B --to X`；
/// 有多个在用入口的节点只给带 `--from` 的单条建议（`single_suggestions`）
pub async fn placement(_a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<PlacementQ>) -> ApiResult<Json<Value>> {
    Ok(ok(crate::placement::view(&st, q.team.as_deref(), q.node.as_deref()).await?))
}

// ── 搬迁建议 ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RebalanceIn {
    /// 搬到哪台线路机；不填取在用、非故障、额度放得下的线路机里往返最好的（没有往返数据时取额度余量最多的）
    target_relay_id: Option<Uuid>,
    /// 不动的节点 id（逗号分隔），如分享给别人的入口所在节点
    exclude: Option<String>,
}

pub(crate) fn quota_q(m: &MachineView, now: OffsetDateTime) -> Q {
    let q = &m.quota;
    let days_left = q.period_end.map_or(0.0, |e| ((q.period_start_at + time::Duration::days((e - q.period_start).whole_days())) - now).as_seconds_f64().max(0.0) / 86400.0);
    Q { limit: q.limit_gb.map(|g| g as f64 * 1e9), used: q.used as f64, daily: q.daily_avg.unwrap_or(0) as f64, days_left }
}

/// 各线路机的折算系数 k = 网卡计费日均 ÷ 它转发的业务日均（搬迁建议、放置矩阵共用）。业务量是近 7 天（各自的日均窗口）
/// 按入口的 relay_id 归、不限入口生命周期（窗口里搬走 / 退役的入口也算），和网卡日均同窗口；同样的业务量在双向计费（sum）的
/// 线路机上约是单向的两倍，所以各按自己的 k 折算。没有业务量（新线路机）、日均估不出来，或实测值偏离 `rule_factor` 太多
/// （见 `quota::sane_k`）时无从实测，按转发的物理关系取：入站 ≈ 出站 ≈ 业务量，sum 折 2，单向（max / min / in / out）折 1
pub(crate) async fn relay_ks(db: &sqlx::PgPool, all: &[MachineView], now: OffsetDateTime) -> sqlx::Result<HashMap<Uuid, f64>> {
    let relays: Vec<&MachineView> = all.iter().filter(|m| m.kind == "relay").collect();
    let (rids, rsince): (Vec<Uuid>, Vec<OffsetDateTime>) = relays.iter().map(|m| (m.id, m.quota.window_start)).unzip();
    let rows: Vec<(Uuid, i64)> = sqlx::query_as(
        "select w.relay_id, least(coalesce(sum(h.up_bytes::numeric + h.down_bytes), 0), 9e18)::bigint
         from unnest($1::uuid[], $2::timestamptz[]) w(relay_id, since)
         join entry e on e.relay_id = w.relay_id join entry_traffic_hourly h on h.entry_id = e.id and h.hour >= w.since
         group by w.relay_id",
    )
    .bind(&rids)
    .bind(&rsince)
    .fetch_all(db)
    .await?;
    let biz: HashMap<Uuid, i64> = rows.into_iter().collect();
    let now_h = OffsetDateTime::from_unix_timestamp(now.unix_timestamp() / 3600 * 3600).unwrap_or(now);
    Ok(relays
        .into_iter()
        .map(|t| {
            let days = ((now_h - t.quota.window_start).as_seconds_f64() / 86400.0).max(1.0);
            let b = biz.get(&t.id).map_or(0.0, |&b| b as f64 / days);
            let k = match t.quota.daily_avg {
                Some(d) if d > 0 && b > 0.0 => crate::quota::sane_k(d as f64 / b, t.quota.rule),
                _ => crate::quota::rule_factor(t.quota.rule),
            };
            (t.id, k)
        })
        .collect())
}

/// 搬迁建议（只读，不动任何东西）：这台线路机按近 7 天的速度重置前会用满的话，挑出搬走后能撑到重置日的最少入口，
/// 并给出搬去哪台线路机；已经超出额度的，挑出搬走后日均降到原来 `OVER_LIMIT_KEEP_PCT`% 以下的最少入口（到重置日前不再大量增长）。每个入口每天的流量按近 7 天（窗口同日均）的业务流量算，再按「线路机网卡日均 ÷ 各入口合计」折算
/// （网卡口径与入口上下行合计不完全相等）；目标线路机接收后多出的量按目标自己的计费方式折算（见 `quota::rule_factor`）。真正执行是一次批量换线路机 `POST /moves`（fleetctl move A,B,C --to X），每台软路由只重启一次
pub async fn rebalance(_a: Actor, State(st): State<AppState>, ApiPath(relay): ApiPath<Uuid>, ApiQuery(b): ApiQuery<RebalanceIn>) -> ApiResult<Json<Value>> {
    let all = views::live(&st).await?;
    let src = all.iter().find(|m| m.id == relay && m.kind == "relay").ok_or_else(|| ApiError::not_found("没有这台在用的线路机"))?;
    let now = OffsetDateTime::now_utc();
    let sq = quota_q(src, now);
    let Some(limit) = sq.limit else { return Err(ApiError::bad_request("这台线路机没有设流量上限，没法判断会不会用满")) };
    if src.quota.period_end.is_none() {
        return Err(ApiError::bad_request("这台线路机按近 30 天滚动计流量，没有重置日，不做搬迁建议"));
    }
    let exclude: HashSet<Uuid> = b.exclude.as_deref().unwrap_or("").split(',').filter(|s| !s.trim().is_empty()).map(|s| s.trim().parse().map_err(|_| ApiError::bad_request("exclude 要填节点 id（逗号分隔）"))).collect::<Result<_, _>>()?;
    let pct = |x: f64, l: f64| (x * 1000.0 / l).round() / 10.0;
    let mut out = json!({
        "relay": {"id": src.id, "name": src.name},
        "limit_gb": src.quota.limit_gb, "used": src.quota.used, "period_end": src.quota.period_end,
        "daily_avg": src.quota.daily_avg, "forecast_full_at": src.quota.forecast_full_at.map(meta::fmt_ts),
        "projected_pct": (sq.daily > 0.0).then(|| pct(sq.projected(), limit)),
        "target": null, "moves": [], "needed": false, "ok": false, "reason": null, "over_limit": sq.used >= limit,
    });
    let early = if src.quota.daily_avg.is_none() {
        Some(("本周期的数据不足 1 天，还估不出速度", false))
    } else if sq.used >= limit && sq.daily <= 0.0 {
        Some(("本周期已经超出额度，但近 7 天没有新增流量，不用搬", false))
    } else if sq.used < limit && sq.projected() <= limit {
        Some(("按近 7 天的速度，重置前用不满，不用搬", false))
    } else {
        None
    };
    if let Some((reason, needed)) = early {
        out["reason"] = json!(reason);
        out["needed"] = json!(needed);
        return Ok(ok(out));
    }
    out["needed"] = json!(true);

    // 各入口近 7 天的流量
    let live: Vec<&EntryView> = src.entries.iter().filter(|e| e.lifecycle == "active").collect();
    let eids: Vec<Uuid> = live.iter().map(|e| e.id).collect();
    let rows: Vec<(Uuid, i64, i64)> = sqlx::query_as(
        "select h.entry_id, least(coalesce(sum(h.up_bytes::numeric + h.down_bytes), 0), 9e18)::bigint,
                least(coalesce(sum(h.up_bytes::numeric + h.down_bytes) filter (where h.hour >= $3), 0), 9e18)::bigint
         from entry_traffic_hourly h where h.entry_id = any($1) and h.hour >= $2 group by h.entry_id",
    )
    .bind(&eids)
    .bind(src.quota.period_start_at)
    .bind(src.quota.window_start)
    .fetch_all(&st.db)
    .await?;
    let traffic: HashMap<Uuid, (i64, i64)> = rows.into_iter().map(|(id, p, r)| (id, (p, r))).collect();
    let now_h = OffsetDateTime::from_unix_timestamp(now.unix_timestamp() / 3600 * 3600).unwrap_or(now);
    let wdays = ((now_h - src.quota.window_start).as_seconds_f64() / 86400.0).max(1.0);
    let rate = |e: &EntryView| traffic.get(&e.id).map_or(0.0, |t| t.1 as f64 / wdays);
    let ks = relay_ks(&st.db, &all, now).await?;
    let k_of = |t: &MachineView| ks.get(&t.id).copied().unwrap_or_else(|| crate::quota::rule_factor(t.quota.rule));
    let k = k_of(src);
    // 源线路机上折出的搬走量 `cut`（源的计费口径）→ 目标线路机每天多出的计费量
    let onto = |t: &MachineView, cut: f64| cut / k * k_of(t);
    // 搬走后每天最多还能走多少：没超额度时 = 剩余额度 ÷ 剩余天数（撑到重置日）；
    // 已超额度时撑到重置日已无从谈起，目标改为「到重置日前不再大量增长」：剩余日均降到原来的 OVER_LIMIT_KEEP_PCT% 以下
    let over = sq.used >= limit;
    let allowed = if over { sq.daily * crate::quota::OVER_LIMIT_KEEP_PCT / 100.0 } else { (limit - sq.used) / sq.days_left.max(0.04) };

    // 搬去 `t` 的方案：能搬的 = 在用的非直出入口，节点没被排除、在 t 上还没有入口。(能搬的入口, 各自每天的流量, 选中的下标, 搬走后每天少的流量, 撑不撑得到重置日)
    let plan_for = |t: &MachineView| {
        let on_target: HashSet<Uuid> = t.entries.iter().map(|e| e.node.id).collect();
        let cands: Vec<&EntryView> = live.iter().copied().filter(|e| !e.direct && !exclude.contains(&e.node.id) && !on_target.contains(&e.node.id)).collect();
        let rates: Vec<f64> = cands.iter().map(|e| rate(e) * k).collect();
        let (picked, cut, reached) = pick_moves(sq.daily, allowed, &rates);
        (cands, rates, picked, cut, reached)
    };

    // 目标：指定的；没指定就在用、非故障、非备用的线路机里挑——额度放得下的优先（都放不下仍按余量挑一台，下面照旧写明放不下），
    // 再看放置矩阵：被搬节点到它近 24 小时 p50 的平均最小的（有节点成功率不足 99% 的不要）；没有往返数据就按额度余量最多的
    let usable = |m: &&MachineView| m.kind == "relay" && m.id != relay && m.lifecycle == "active" && !m.spare && m.health.status != Status::Down;
    let mut avg_rtt: Option<f64> = None;
    let tgt = match b.target_relay_id {
        Some(t) => all.iter().find(|m| m.id == t && m.kind == "relay").filter(|m| m.id != relay).ok_or_else(|| ApiError::bad_request("目标要是另一台线路机"))?,
        None => {
            let pool: Vec<&MachineView> = all.iter().filter(usable).collect();
            if pool.is_empty() {
                return Err(ApiError::conflict("没有可作为目标的线路机（要在用、非故障）"));
            }
            let plans: Vec<_> = pool.iter().map(|t| plan_for(t)).collect();
            let fits: Vec<usize> = (0..pool.len()).filter(|&i| plans[i].4 && !plans[i].2.is_empty() && quota_q(pool[i], now).fits(onto(pool[i], plans[i].3))).collect();
            let idx: Vec<usize> = if fits.is_empty() { (0..pool.len()).collect() } else { fits };
            let moved = |i: usize| -> Vec<Uuid> { plans[i].2.iter().map(|&j| plans[i].0[j].node.id).collect() };
            let all_moved: Vec<Uuid> = idx.iter().flat_map(|&i| moved(i)).collect();
            let grid = crate::placement::cells(&st.db, &all_moved).await?;
            let scores: Vec<Score> = idx.iter().map(|&i| crate::placement::score(&moved(i), &grid, pool[i].id)).collect();
            let fastest = idx.iter().zip(&scores).filter_map(|(&i, s)| if let Score::Rtt(v) = s { Some((i, *v)) } else { None }).min_by(|a, b| a.1.total_cmp(&b.1));
            let chosen = match fastest {
                Some((i, v)) => {
                    avg_rtt = Some(v);
                    i
                }
                None => {
                    let ok: Vec<usize> = idx.iter().zip(&scores).filter(|(_, s)| **s != Score::Bad).map(|(&i, _)| i).collect();
                    let from = if ok.is_empty() { &idx } else { &ok };
                    let c: Vec<(Uuid, Q)> = from.iter().map(|&i| (pool[i].id, quota_q(pool[i], now))).collect();
                    let id = best_target(&c).expect("池子非空");
                    pool.iter().position(|m| m.id == id).expect("来自同一份列表")
                }
            };
            pool[chosen]
        }
    };
    let tq = quota_q(tgt, now);
    out["target"] = json!({"id": tgt.id, "name": tgt.name, "limit_gb": tgt.quota.limit_gb, "used": tgt.quota.used, "daily_avg": tgt.quota.daily_avg,
                           "projected_pct": tq.limit.map(|l| pct(tq.projected(), l)), "avg_rtt_ms": avg_rtt.map(|v| (v * 10.0).round() / 10.0)});

    let (cands, rates, picked, cut, reached) = plan_for(tgt);
    let mut moves = vec![];
    for &i in &picked {
        let e = cands[i];
        let active_entries = all.iter().filter(|m| m.kind == "relay").flat_map(|m| m.entries.iter()).filter(|x| x.node.id == e.node.id && x.lifecycle == "active").count();
        moves.push(json!({"node": e.node, "entry_id": e.id, "port": e.port, "period_bytes": traffic.get(&e.id).map_or(0, |t| t.0),
                          "daily_bytes": rates[i].round() as i64, "needs_from": active_entries > 1}));
    }
    out["moves"] = json!(moves);
    out["ok"] = json!(reached && !picked.is_empty());
    out["after"] = json!({"daily_avg": (sq.daily - cut).max(0.0).round() as i64, "projected_pct": pct(sq.used + (sq.daily - cut).max(0.0) * sq.days_left, limit)});
    let extra = onto(tgt, cut);
    out["target_after"] = json!({"fits": tq.fits(extra), "projected_pct": tq.limit.map(|l| pct(tq.projected() + extra * tq.days_left, l))});
    if !reached && over {
        out["reason"] = json!(format!("能搬的入口都搬走，日均也降不到原来的 {}% 以下（被排除的、已在目标上有入口的、直出的不算）", crate::quota::OVER_LIMIT_KEEP_PCT));
    } else if !reached {
        out["reason"] = json!("能搬的入口都搬走也撑不到重置日（被排除的、已在目标上有入口的、直出的不算）");
    } else if !tq.fits(extra) {
        out["reason"] = json!(format!("目标线路机放不下：接收后到重置日预计超过它上限的 {}%", crate::quota::TARGET_SAFE_PCT));
    }
    Ok(ok(out))
}
