//! 软路由出口的两个运维操作：
//! - 同步出口参数（`GET` 预览 / `POST /routers/{id}/sync`）：参数和对上的 fleet 入口现在的参数不一致（凭据轮换后没同步，告警 router_exit_drift）的出口
//!   改成入口现在的参数。默认（不点名）只同步「已建立对应」的出口（router_exit_link：中控作业写过，或曾经参数一致）：出口↔入口只按路由上报的
//!   server:port 对应，路由上有人加一个指向别人入口的出口、凭据乱填，不能让同步把那个入口的真凭据写进这台路由。点名同步（`exits`）时，没建立过对应、
//!   而且入口是直出 / 备用团队 / 别的团队的，要 `force`。建一个 router_sync 作业（目标是路由），和替换、改出口指向共用「改软路由出口」那一步（jobs/steps/replace.rs：按路由当时的配置
//!   重新确定、备份 → 改 → 重启 OpenClash → 测延迟，失败自动改回）。重启 OpenClash 会断开经它的全部连接。
//! - 临时切换（`POST /routers/{id}/switch`、`POST /routers/{id}/switch/back`）：select 组经 mihomo API 切到组里另一个出口，不重启，可到时自动切回（routers/switch.rs）。

use super::ok;
use super::replace::{routers_view, ExitPlan, RouterPlan};
use crate::auth::Actor;
use crate::db::audit;
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::jobs;
use crate::routers::{self, switch};
use crate::state::{AppState, Event};
use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Deserialize, Default)]
pub struct SyncIn {
    /// 只同步这几个出口（空 = 全部参数不一致的）
    #[serde(default)]
    exits: Vec<String>,
    /// 点名的出口从没对上过入口、而且入口不属于这台路由的团队时，仍要同步
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize)]
pub struct SyncQ {
    /// 逗号分隔的出口名
    exits: Option<String>,
    force: Option<bool>,
}

struct SyncPlan {
    router: RouterPlan,
    blockers: Vec<String>,
    /// 参数不一致、但同步不了的（入口不在用）：不在计划里，给人看
    skipped: Vec<String>,
    /// 参数不一致、但从没对上过入口（路由上有人加的？）：默认不同步，给人看（出口名、入口名）
    untrusted: Vec<Value>,
    /// 点名要同步、但从没对上过入口、入口又不属于这台路由的团队的：没有 force 就是 blocker（出口名、入口名、原因）
    need_force: Vec<Value>,
}

/// 入口的凭据不该默认写进这台路由的原因：直出、备用团队、别的团队的入口。None = 同团队的普通入口
async fn foreign_why(db: &sqlx::PgPool, r: &routers::Row, ids: &[Uuid]) -> ApiResult<HashMap<Uuid, &'static str>> {
    let rows: Vec<(Uuid, bool, Option<Uuid>, bool)> = sqlx::query_as(
        "select e.id, e.node_id = e.relay_id, n.team_id, coalesce(t.spare, false)
         from entry e join machine n on n.id = e.node_id left join team t on t.id = n.team_id where e.id = any($1)",
    )
    .bind(ids)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, direct, team, spare)| {
            let why = if direct {
                "直出入口"
            } else if spare {
                "备用团队的入口"
            } else if team.is_none() || team != r.team_id {
                "别的团队的入口"
            } else {
                return None;
            };
            Some((id, why))
        })
        .collect())
}

/// 按最近一次采集算：要同步哪些出口。`foreign`：入口 → 不属于这台路由团队的原因（见 [`foreign_why`]）
fn sync_plan(r: &routers::Row, only: &[String], force: bool, foreign: &HashMap<Uuid, &'static str>) -> SyncPlan {
    let (mut blockers, mut skipped, mut exits, mut untrusted, mut need_force) = (vec![], vec![], vec![], vec![], vec![]);
    let snap = r.snapshot().unwrap_or_default();
    for name in only {
        match snap.exits.iter().find(|x| &x.name == name) {
            None => blockers.push(format!("最近一次采集里没有出口「{name}」")),
            Some(x) if x.entry.is_none() => blockers.push(format!("出口「{name}」不是面板的入口，不归这里同步")),
            Some(x) if x.sync != "drift" => blockers.push(format!("出口「{name}」的参数已经和入口一致")),
            Some(_) => {}
        }
    }
    for x in &snap.exits {
        let (Some(er), Some(srv), Some(port)) = (x.entry.as_ref(), x.server.as_deref(), x.port) else { continue };
        if x.sync == "drift" && !x.linked {
            if only.is_empty() {
                untrusted.push(json!({"exit": x.name, "entry": er.name}));
                continue;
            }
            if let Some(why) = only.contains(&x.name).then(|| foreign.get(&er.id)).flatten() {
                need_force.push(json!({"exit": x.name, "entry": er.name, "why": why}));
                if !force {
                    blockers.push(format!("出口「{}」从没对上过入口「{}」（{why}），可能是路由上有人加的；确认要同步的话加 --force", x.name, er.name));
                    continue;
                }
            }
        }
        match crate::jobs::sync_exit(only, &x.name, &x.sync, er, srv, port) {
            Some(Ok(e)) => exits.push(e),
            Some(Err(why)) => blockers.push(format!("出口「{}」：{why}", x.name)),
            None if x.sync == "drift" && only.is_empty() => skipped.push(format!("{}（{}，{}）", x.name, er.name, routers::alerts::drift_why(&er.lifecycle))),
            None => {}
        }
    }
    if r.state() != "online" {
        blockers.push(format!("软路由「{}」现在不在线：等它在线再同步", r.name));
    }
    if exits.is_empty() && blockers.is_empty() {
        blockers.push(format!("软路由「{}」没有要同步的出口（参数都和入口一致，或对上的入口已经不在用）", r.name));
    }
    let router = RouterPlan { id: r.id, name: r.name.clone(), state: r.state().to_string(), exits };
    SyncPlan { router, blockers, skipped, untrusted, need_force }
}

impl SyncPlan {
    fn view(&self) -> Value {
        json!({
            "router": {"id": self.router.id, "name": self.router.name},
            "routers": routers_view(std::slice::from_ref(&self.router)),
            "ok": self.blockers.is_empty(),
            "blockers": self.blockers,
            "skipped": self.skipped,
            "untrusted": self.untrusted,
            "need_force": self.need_force,
            "warnings": ["改完要重启 OpenClash：经这台路由的连接会全部断开、马上重连"],
        })
    }
}

async fn row(st: &AppState, id: Uuid) -> ApiResult<routers::Row> {
    routers::load(&st.db, Some(id)).await?.into_iter().next().ok_or_else(|| ApiError::not_found("没有这台软路由"))
}

fn names(s: Option<&str>) -> Vec<String> { s.unwrap_or("").split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect() }

async fn plan(st: &AppState, r: &routers::Row, only: &[String], force: bool) -> ApiResult<SyncPlan> {
    let snap = r.snapshot().unwrap_or_default();
    let ids: Vec<Uuid> = snap.exits.iter().filter(|x| x.sync == "drift" && !x.linked).filter_map(|x| x.entry.as_ref().map(|e| e.id)).collect();
    let foreign = foreign_why(&st.db, r, &ids).await?;
    Ok(sync_plan(r, only, force, &foreign))
}

/// 预览：会同步哪些出口（出口名、现在指向、对上的入口）与不能同步的原因
pub async fn sync_preview(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiQuery(q): ApiQuery<SyncQ>) -> ApiResult<Json<Value>> {
    let r = row(&st, id).await?;
    Ok(ok(plan(&st, &r, &names(q.exits.as_deref()), q.force.unwrap_or(false)).await?.view()))
}

/// 发起：建 router_sync 作业
pub async fn sync(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<SyncIn>) -> ApiResult<Json<Value>> {
    let r = row(&st, id).await?;
    let p = plan(&st, &r, &b.exits, b.force).await?;
    if !p.blockers.is_empty() {
        return Err(ApiError::conflict(p.blockers.join("；")).with_detail(json!({"blockers": p.blockers})));
    }
    let mut tx = st.db.begin().await?;
    let busy: Option<Uuid> = sqlx::query_scalar("select id from job where target_id = $1 and state in ('queued', 'running', 'waiting', 'cancelling')").bind(id).fetch_optional(&mut *tx).await?;
    if let Some(j) = busy {
        return Err(ApiError::conflict(format!("这台软路由已经有一个同步作业没做完（{j}）")).with_detail(json!({"job_id": j})));
    }
    let exits: Vec<&str> = p.router.exits.iter().map(|e: &ExitPlan| e.exit.as_str()).collect();
    // 执行时只动这几个出口、而且只在它们还对着现在核对过的入口时动（默认「全部」也是核对后的这几个，不再重新按路由当时的配置圈）
    let pins: serde_json::Map<String, Value> = p.router.exits.iter().filter_map(|e| Some((e.exit.clone(), json!(e.new_entry_id?)))).collect();
    let params = json!({
        "router_ids": [id],
        "routers": true,
        "exits": exits,
        "pins": pins,
        "force": b.force,
        "plan": {"routers": [serde_json::to_value(&p.router).unwrap_or_default()]},
        "router_name": r.name,
    });
    let job = jobs::enqueue(&mut tx, "router_sync", Some(id), params, &a.name).await?;
    audit(&mut tx, &a.name, "router.sync", "router", Some(id), json!({"name": r.name, "exits": exits, "force": b.force, "job_id": job})).await?;
    tx.commit().await?;
    st.wake_jobs();
    st.emit(Event::Job { id: job, target: Some(id) });
    let mut v = p.view();
    v["job_id"] = json!(job);
    Ok(ok(v))
}

#[derive(Deserialize)]
pub struct SwitchIn {
    group: String,
    to: String,
    /// 多少秒后自动切回（不给 = 不自动切回）
    back_after_s: Option<i64>,
}

/// 临时切换：select 组切到组里另一个出口（不重启 OpenClash）
pub async fn switch_to(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<SwitchIn>) -> ApiResult<Json<Value>> {
    Ok(ok(switch::switch(&st, &a.name, id, &b.group, &b.to, b.back_after_s).await?))
}

#[derive(Deserialize)]
pub struct BackIn {
    group: String,
}

/// 切回第一次切之前的出口
pub async fn switch_back(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<BackIn>) -> ApiResult<Json<Value>> {
    Ok(ok(switch::back(&st, &a.name, id, &b.group).await?))
}
