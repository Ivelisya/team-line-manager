//! 生命周期操作：换线路机（move_entry）、搬空线路机（evacuate）。
//! 都是作业：这里只做校验、预览、建作业和写审计，真正的步骤在 jobs/steps。
//!
//! 软路由：指向要摘掉的旧入口的出口，在新入口验收之后、摘旧入口之前改成新入口（作业的「改软路由出口」一步，
//! 和替换节点同一套：全部在线才动手，逐台备份 → 改 → 重启 OpenClash → 测延迟，一台失败已改的全部换回、旧入口不摘）。
//! 预览按快照列出要改的出口；执行时按路由当时的配置重新确定（`move_exit`），同一台路由同时只有一个作业在改。
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
use crate::state::{AppState, Event};
use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

const SHARE_NOTE: &str = "换线路机后分享链接会变：客户端需要换成新的分享链接（作业完成后到节点的入口里复制；命令行用 fleetctl share <节点> 取）";

// ── 换线路机 ──────────────────────────────────────────────────────────────

/// (kind, lifecycle, label, 团队 id, 团队名, 编号)
type NodeRow = (String, String, Option<String>, Option<Uuid>, Option<String>, Option<i32>);

struct MovePlan {
    node_name: String,
    from_relay: Uuid,
    from_name: String,
    to_name: String,
    old_entry: Uuid,
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
            "name": self.node_name, "from": self.from_name, "to": self.to_name,
            "routers_enabled": self.routers_enabled, "routers": routers_view(&self.routers),
            "ok": self.blockers.is_empty(), "blockers": self.blockers, "warnings": self.warnings,
            "share_changed": true, "note": SHARE_NOTE,
        })
    }
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
    // 替换作业的目标是旧节点：这台是它的新节点时也算忙（它在改路由、交换编号，路由刚改成指向这台的入口）
    let busy: Option<(String,)> = sqlx::query_as(
        "select kind from job where state in ('queued', 'running', 'waiting', 'cancelling')
           and (target_id = $1 or (kind = 'replace' and params->>'new_id' = $1::text)) limit 1",
    )
    .bind(node_id)
        .fetch_optional(&mut **tx)
        .await?;
    if let Some((k,)) = busy {
        return Ok(Err(format!("「{name}」还有进行中的作业（{}）：等它结束再换线路机", meta::job_kind_label(&k))));
    }
    let live: Vec<(Uuid, Uuid, String, String)> = sqlx::query_as(
        "select e.id, e.relay_id, e.lifecycle::text, coalesce(rm.label, '') from entry e join machine rm on rm.id = e.relay_id
         where e.node_id = $1 and e.lifecycle in ('pending', 'active', 'disabled') order by e.created_at",
    )
    .bind(node_id)
    .fetch_all(&mut **tx)
    .await?;
    let cands: Vec<&(Uuid, Uuid, String, String)> = live.iter().filter(|e| from.is_none_or(|f| f == e.1)).collect();
    let (old_entry, from_relay, elc, from_name) = match cands.as_slice() {
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
    Ok(Ok(MovePlan { node_name: name, from_relay, from_name, to_name, old_entry, routers_enabled: routers_on, routers: rs, blockers, warnings }))
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

async fn enqueue_move(tx: &mut Tx<'_>, actor: &str, node: Uuid, p: &MovePlan, to: Uuid, via: &str) -> ApiResult<Uuid> {
    let routers = if p.routers_enabled { p.routers.as_slice() } else { &[] };
    let params = json!({
        "from_relay_id": p.from_relay, "to_relay_id": to, "old_entry_id": p.old_entry,
        "routers": p.routers_enabled, "plan": {"routers": serde_json::to_value(routers).unwrap_or_default()},
    });
    let job = jobs::enqueue(tx, "move_entry", Some(node), params, actor).await?;
    let changed: Vec<Value> = routers.iter().map(|r| json!({"id": r.id, "name": r.name, "exits": r.exits.iter().map(|e| &e.exit).collect::<Vec<_>>()})).collect();
    audit(
        tx,
        actor,
        "entry.move",
        "machine",
        Some(node),
        json!({"name": p.node_name, "from": p.from_name, "to": p.to_name, "from_relay_id": p.from_relay, "to_relay_id": to, "old_entry_id": p.old_entry, "job_id": job, "via": via,
               "routers": changed, "routers_enabled": p.routers_enabled}),
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
    let job = enqueue_move(&mut tx, &a.name, id, &plan, b.to_relay_id, "move").await?;
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
                let mut v = json!({"machine_id": node, "name": p.node_name, "routers": routers_view(&p.routers)});
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
               "share_changed": !self.plans.is_empty(), "note": SHARE_NOTE})
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
    let mut plans = vec![];
    let mut skipped = vec![];
    for (node,) in nodes {
        let reason = match plan_move(tx, node, Some(relay), to, &routers, routers_on).await? {
            Ok(plan) if plan.blockers.is_empty() => {
                plans.push((node, plan));
                continue;
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
        skipped.push(json!({"machine_id": node, "name": name.map(|(n,)| n), "reason": reason}));
    }
    Ok(Evac { from_name, to_name, plans, skipped })
}

/// 搬空预览：会搬哪些节点、各自要改的软路由出口、哪些暂时不能搬（不写库，同 move_preview）
pub async fn evacuate_preview(_a: Actor, State(st): State<AppState>, ApiPath(relay): ApiPath<Uuid>, ApiQuery(q): ApiQuery<EvacuateIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let e = plan_evacuate(&st, &mut tx, relay, q.to_relay_id, q.routers != Some(false)).await?;
    Ok(ok(e.view(None)))
}

/// 搬空线路机：为这台线路机上每个在用入口的节点各建一个换线路机作业（线路机退役前用）。
/// 暂时不能搬的节点（停用、有别的作业、已在目标线路机上有入口、软路由不在线…）列在 skipped 里，不影响其它节点。
/// 各作业并行；改同一台软路由的「改软路由出口」一步一台路由同时只有一个作业在改（按路由加锁，见 jobs/steps/replace.rs）
pub async fn evacuate(a: Actor, State(st): State<AppState>, ApiPath(relay): ApiPath<Uuid>, ApiJson(b): ApiJson<EvacuateIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let e = plan_evacuate(&st, &mut tx, relay, b.to_relay_id, b.routers != Some(false)).await?;
    let mut created = vec![];
    for (node, plan) in &e.plans {
        created.push(enqueue_move(&mut tx, &a.name, *node, plan, b.to_relay_id, "evacuate").await?);
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
