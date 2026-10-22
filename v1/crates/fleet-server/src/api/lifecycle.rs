//! 生命周期操作：换线路机（move_entry）、搬空线路机（evacuate）。
//! 都是作业：这里只做校验、建作业和写审计，真正的步骤在 jobs/steps。

use super::ok;
use crate::auth::Actor;
use crate::db::{audit, Tx};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::jobs;
use crate::meta;
use crate::names::display_name;
use crate::state::{AppState, Event};
use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

const SHARE_NOTE: &str = "换线路机后分享链接会变：客户端需要换成新的分享链接（作业完成后到节点的入口里复制；命令行用 fleetctl share <节点> 取）";

// ── 换线路机 ──────────────────────────────────────────────────────────────

/// (kind, lifecycle, label, 团队名, 编号)
type NodeRow = (String, String, Option<String>, Option<String>, Option<i32>);

struct MovePlan {
    node_name: String,
    from_relay: Uuid,
    from_name: String,
    to_name: String,
    old_entry: Uuid,
}

/// 校验并规划一次换线路机。外层 Err = 数据库/不存在；内层 Err = 业务上不能换（给用户看的原因，evacuate 用它做「跳过」）。
/// `from`：要换掉哪个线路机上的入口；None = 节点必须恰有一个在用入口。
async fn plan_move(tx: &mut Tx<'_>, node_id: Uuid, from: Option<Uuid>, to: Uuid) -> ApiResult<Result<MovePlan, String>> {
    let node: Option<NodeRow> = sqlx::query_as(
        "select m.kind::text, m.lifecycle::text, m.label, t.name, m.number from machine m left join team t on t.id = m.team_id
         where m.id = $1 for share of m",
    )
    .bind(node_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((kind, lc, label, team, number)) = node else { return Err(ApiError::not_found("没有这台机器")) };
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
    let busy: Option<(String,)> = sqlx::query_as("select kind from job where target_id = $1 and state in ('queued', 'running', 'waiting', 'cancelling') limit 1")
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
    Ok(Ok(MovePlan { node_name: name, from_relay, from_name, to_name, old_entry }))
}

async fn enqueue_move(tx: &mut Tx<'_>, actor: &str, node: Uuid, p: &MovePlan, to: Uuid, via: &str) -> ApiResult<Uuid> {
    let job = jobs::enqueue(tx, "move_entry", Some(node), json!({"from_relay_id": p.from_relay, "to_relay_id": to, "old_entry_id": p.old_entry}), actor).await?;
    audit(
        tx,
        actor,
        "entry.move",
        "machine",
        Some(node),
        json!({"name": p.node_name, "from": p.from_name, "to": p.to_name, "from_relay_id": p.from_relay, "to_relay_id": to, "old_entry_id": p.old_entry, "job_id": job, "via": via}),
    )
    .await?;
    Ok(job)
}

#[derive(Deserialize)]
pub struct MoveIn {
    to_relay_id: Uuid,
    /// 节点有多个入口时，指定换掉哪条线路机上的
    from_relay_id: Option<Uuid>,
}

/// 把节点换到另一台线路机：新入口验收通过后才摘旧入口，期间不断网。
pub async fn move_node(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<MoveIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let plan = plan_move(&mut tx, id, b.from_relay_id, b.to_relay_id).await?.map_err(ApiError::conflict)?;
    let job = enqueue_move(&mut tx, &a.name, id, &plan, b.to_relay_id, "move").await?;
    tx.commit().await?;
    st.wake_jobs();
    st.emit(Event::Job { id: job, target: Some(id) });
    Ok(ok(json!({
        "job_id": job, "name": plan.node_name, "from": plan.from_name, "to": plan.to_name,
        "share_changed": true, "note": SHARE_NOTE,
    })))
}

#[derive(Deserialize)]
pub struct EvacuateIn {
    to_relay_id: Uuid,
}

/// 搬空线路机：为这台线路机上每个在用入口的节点各建一个换线路机作业（线路机退役前用）。
/// 暂时不能搬的节点（停用、有别的作业、已在目标线路机上有入口…）列在 skipped 里，不影响其它节点。
pub async fn evacuate(a: Actor, State(st): State<AppState>, ApiPath(relay): ApiPath<Uuid>, ApiJson(b): ApiJson<EvacuateIn>) -> ApiResult<Json<Value>> {
    if relay == b.to_relay_id {
        return Err(ApiError::bad_request("目标线路机不能和要搬空的是同一台"));
    }
    let mut tx = st.db.begin().await?;
    let src: Option<(String, String, String)> = sqlx::query_as("select kind::text, coalesce(label, ''), lifecycle::text from machine where id = $1")
        .bind(relay)
        .fetch_optional(&mut *tx)
        .await?;
    let (kind, from_name, lc) = src.ok_or_else(|| ApiError::not_found("没有这台机器"))?;
    if kind != "relay" {
        return Err(ApiError::conflict("只有线路机能搬空"));
    }
    if lc == "retired" {
        return Err(ApiError::conflict("这台线路机已经退役"));
    }
    let to: Option<(String, String)> = sqlx::query_as("select coalesce(label, ''), lifecycle::text from machine where id = $1 and kind = 'relay'")
        .bind(b.to_relay_id)
        .fetch_optional(&mut *tx)
        .await?;
    let (to_name, to_lc) = to.ok_or_else(|| ApiError::not_found("目标线路机不存在"))?;
    if to_lc != "active" {
        return Err(ApiError::conflict(format!("目标线路机「{to_name}」现在是「{}」，只能搬到在用的线路机", meta::lifecycle_label(&to_lc))));
    }
    let nodes: Vec<(Uuid,)> = sqlx::query_as(
        "select distinct e.node_id from entry e join machine n on n.id = e.node_id
         where e.relay_id = $1 and e.lifecycle in ('pending', 'active', 'disabled') and n.kind <> 'relay' order by 1",
    )
    .bind(relay)
    .fetch_all(&mut *tx)
    .await?;
    let mut created = vec![];
    let mut skipped = vec![];
    for (node,) in nodes {
        match plan_move(&mut tx, node, Some(relay), b.to_relay_id).await? {
            Ok(plan) => {
                let job = enqueue_move(&mut tx, &a.name, node, &plan, b.to_relay_id, "evacuate").await?;
                created.push(json!({"machine_id": node, "name": plan.node_name, "job_id": job}));
            }
            Err(reason) => {
                let name: Option<(String,)> = sqlx::query_as(
                    "select display_name(m.kind::text, m.label, t.name, m.number)
                     from machine m left join team t on t.id = m.team_id where m.id = $1",
                )
                .bind(node)
                .fetch_optional(&mut *tx)
                .await?;
                skipped.push(json!({"machine_id": node, "name": name.map(|(n,)| n), "reason": reason}));
            }
        }
    }
    audit(
        &mut tx,
        &a.name,
        "relay.evacuate",
        "machine",
        Some(relay),
        json!({"from": from_name, "to": to_name, "to_relay_id": b.to_relay_id, "jobs": created, "skipped": skipped}),
    )
    .await?;
    tx.commit().await?;
    st.wake_jobs();
    for j in &created {
        let uuid = |k: &str| j[k].as_str().and_then(|s| Uuid::try_parse(s).ok());
        if let Some(id) = uuid("job_id") {
            st.emit(Event::Job { id, target: uuid("machine_id") });
        }
    }
    st.emit(Event::Machines);
    Ok(ok(json!({"from": from_name, "to": to_name, "jobs": created, "skipped": skipped, "share_changed": !created.is_empty(), "note": SHARE_NOTE})))
}
