//! 软路由出口的两个运维操作：
//! - 同步出口参数（`GET` 预览 / `POST /routers/{id}/sync`）：参数和对上的 fleet 入口现在的参数不一致（凭据轮换后没同步，告警 router_exit_drift）的出口
//!   改成入口现在的参数。建一个 router_sync 作业（目标是路由），和替换、改出口指向共用「改软路由出口」那一步（jobs/steps/replace.rs：按路由当时的配置
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
use uuid::Uuid;

#[derive(Deserialize, Default)]
pub struct SyncIn {
    /// 只同步这几个出口（空 = 全部参数不一致的）
    #[serde(default)]
    exits: Vec<String>,
}

#[derive(Deserialize)]
pub struct SyncQ {
    /// 逗号分隔的出口名
    exits: Option<String>,
}

struct SyncPlan {
    router: RouterPlan,
    blockers: Vec<String>,
    /// 参数不一致、但同步不了的（入口不在用）：不在计划里，给人看
    skipped: Vec<String>,
}

/// 按最近一次采集算：要同步哪些出口
fn sync_plan(r: &routers::Row, only: &[String]) -> SyncPlan {
    let (mut blockers, mut skipped, mut exits) = (vec![], vec![], vec![]);
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
    SyncPlan { router, blockers, skipped }
}

impl SyncPlan {
    fn view(&self) -> Value {
        json!({
            "router": {"id": self.router.id, "name": self.router.name},
            "routers": routers_view(std::slice::from_ref(&self.router)),
            "ok": self.blockers.is_empty(),
            "blockers": self.blockers,
            "skipped": self.skipped,
            "warnings": ["改完要重启 OpenClash：经这台路由的连接会全部断开、马上重连"],
        })
    }
}

async fn row(st: &AppState, id: Uuid) -> ApiResult<routers::Row> {
    routers::load(&st.db, Some(id)).await?.into_iter().next().ok_or_else(|| ApiError::not_found("没有这台软路由"))
}

fn names(s: Option<&str>) -> Vec<String> { s.unwrap_or("").split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect() }

/// 预览：会同步哪些出口（出口名、现在指向、对上的入口）与不能同步的原因
pub async fn sync_preview(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiQuery(q): ApiQuery<SyncQ>) -> ApiResult<Json<Value>> {
    Ok(ok(sync_plan(&row(&st, id).await?, &names(q.exits.as_deref())).view()))
}

/// 发起：建 router_sync 作业
pub async fn sync(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<SyncIn>) -> ApiResult<Json<Value>> {
    let r = row(&st, id).await?;
    let p = sync_plan(&r, &b.exits);
    if !p.blockers.is_empty() {
        return Err(ApiError::conflict(p.blockers.join("；")).with_detail(json!({"blockers": p.blockers})));
    }
    let mut tx = st.db.begin().await?;
    let busy: Option<Uuid> = sqlx::query_scalar("select id from job where target_id = $1 and state in ('queued', 'running', 'waiting', 'cancelling')").bind(id).fetch_optional(&mut *tx).await?;
    if let Some(j) = busy {
        return Err(ApiError::conflict(format!("这台软路由已经有一个同步作业没做完（{j}）")).with_detail(json!({"job_id": j})));
    }
    let exits: Vec<&str> = p.router.exits.iter().map(|e: &ExitPlan| e.exit.as_str()).collect();
    let params = json!({
        "router_ids": [id],
        "routers": true,
        "exits": b.exits,
        "plan": {"routers": [serde_json::to_value(&p.router).unwrap_or_default()]},
        "router_name": r.name,
    });
    let job = jobs::enqueue(&mut tx, "router_sync", Some(id), params, &a.name).await?;
    audit(&mut tx, &a.name, "router.sync", "router", Some(id), json!({"name": r.name, "exits": exits, "job_id": job})).await?;
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
