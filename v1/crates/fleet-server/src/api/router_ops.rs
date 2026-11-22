//! 软路由出口的两个运维操作：
//! - 同步出口参数（`GET` 预览 / `POST /routers/{id}/sync`）：参数和对上的 fleet 入口现在的参数不一致（凭据轮换后没同步，告警 router_exit_drift）的出口
//!   改成入口现在的参数。默认（不点名）只同步「已建立对应」的出口（router_exit_link：中控作业写过，或曾经参数一致）：出口↔入口只按路由上报的
//!   server:port 对应，路由上有人加一个指向别人入口的出口、凭据乱填，不能让同步把那个入口的真凭据写进这台路由。点名同步（`exits`）时，没建立过对应、
//!   而且入口是直出 / 备用团队 / 别的团队的，要 `force`。建一个 router_sync 作业（目标是路由），和替换、改出口指向共用「改软路由出口」那一步（jobs/steps/replace.rs：按路由当时的配置
//!   重新确定、备份 → 改 → 重启 OpenClash → 测延迟，失败自动改回）。重启 OpenClash 会断开经它的全部连接。
//! - 切出口协议（`GET` 预览 / `POST /routers/{id}/protocol`，`to` = anytls | reality）：指向 fleet 入口的出口整块改写成另一种协议（同一个入口、同名出口，
//!   名字、分组、规则不变），作业 router_protocol，同样走「改软路由出口」那一步。线路机没开目标协议（AnyTLS 还要 sing-box 在跑、端口算得出）、
//!   从没对上过入口的出口跳过并写原因。
//! - 删出口（`GET` 预览 / `POST /routers/{id}/exits/remove`，`exits` 点名）：客户不要了的出口连同指向它的分流规则、策略组里的引用一起从配置里删掉，
//!   作业 router_exit_rm（jobs/steps/router_exits.rs）。对不上面板入口的出口要 `force`；指着的备用机器删完不再被扣住，到期后自动回收。
//! - 临时切换（`POST /routers/{id}/switch`、`POST /routers/{id}/switch/back`）：select 组经 mihomo API 切到组里另一个出口，不重启，可到时自动切回（routers/switch.rs）。
//! - 只重启 OpenClash（`GET` 预览 / `POST /routers/{id}/restart`）：配置不动，作业 router_restart（重启、等 API、测 fleet 出口；隧道断了等回连再取结果）。
//! - 看配置（只读：`GET /routers/{id}/config`、`/config/backups`、`/config/diff?backup=`）：经隧道读路由上的源配置 / 备份，结构、备份列表、
//!   与某份备份的逐行比较（凭据隐去，routers::ops::mask）。这几个会登录路由读东西，记审计 `router.config_read`（同 `fleetctl ssh` 登录前记 ssh.login；
//!   `router show` 只读中控里的快照，不记）。
//! - 出口改名跟上面板（`GET` 预览 / `POST /routers/{id}/rename-exits`）：编号压缩后出口名末尾的编号改成节点现在的编号，作业 router_rename。
//! - 来源 IP 分流规则（`GET /routers/{id}/rules[?assign=…]` 列出 / 预览，`POST` 改）：SRC-IP-CIDR → 出口，作业 router_rules。
//!   改名、规则都按路由此刻的配置算要改的行（预览就读一次配置），执行时再读一次、对不上就不改（routers::ops 的说明）。

use super::ok;
use super::replace::{routers_view, ExitPlan, RouterPlan};
use crate::auth::Actor;
use crate::db::audit;
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::jobs;
use crate::routers::{self, clash, switch};
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
            Some(Ok(e)) => exits.push(e.keep_proto(x.anytls())),
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
pub struct ProtoIn {
    /// anytls / reality
    to: String,
    /// 只切这几个出口（空 = 全部指向 fleet 入口、协议不是 `to` 的）
    #[serde(default)]
    exits: Vec<String>,
}

#[derive(Deserialize)]
pub struct ProtoQ {
    to: String,
    /// 逗号分隔的出口名
    exits: Option<String>,
}

struct ProtoPlan {
    to: &'static str,
    router: RouterPlan,
    /// 要切的出口的明细（出口、入口、线路机、现在 → 改成）
    rows: Vec<Value>,
    blockers: Vec<String>,
    /// 指向 fleet 入口、协议不是 `to`，但切不了的（线路机没开这个协议、从没对上过入口…）：不在计划里，给人看
    skipped: Vec<Value>,
}

impl ProtoPlan {
    fn view(&self) -> Value {
        json!({
            "router": {"id": self.router.id, "name": self.router.name},
            "to": self.to,
            "routers": routers_view(std::slice::from_ref(&self.router)),
            "exits": self.rows,
            "ok": self.blockers.is_empty(),
            "blockers": self.blockers,
            "skipped": self.skipped,
            "warnings": ["改完要重启 OpenClash：经这台路由的连接会全部断开、马上重连"],
        })
    }
}

fn check_to(to: &str) -> ApiResult<&'static str> {
    match to.trim() {
        "anytls" => Ok(clash::ANYTLS),
        "reality" => Ok(clash::REALITY),
        _ => Err(ApiError::bad_request("to 只能是 anytls 或 reality")),
    }
}

/// 按最近一次采集算：要切哪些出口、切不了的原因。AnyTLS 端口按线路机上真在用的算（sing-box 在跑、端口算得出），同分享链接
async fn proto_plan(st: &AppState, r: &routers::Row, to: &'static str, only: &[String]) -> ApiResult<ProtoPlan> {
    let at = to == clash::ANYTLS;
    let snap = r.snapshot().unwrap_or_default();
    let mut blockers = vec![];
    for name in only {
        match snap.exits.iter().find(|x| &x.name == name) {
            None => blockers.push(format!("最近一次采集里没有出口「{name}」")),
            Some(x) if x.entry.is_none() => blockers.push(format!("出口「{name}」不是面板的入口，不归这里切")),
            Some(x) if x.anytls() == at => blockers.push(format!("出口「{name}」已经是 {}", proto_cn(to))),
            Some(_) => {}
        }
    }
    let relays: Vec<Uuid> = snap.exits.iter().filter_map(|x| x.entry.as_ref().map(|e| e.relay_id)).collect();
    // 线路机 → (入口协议, sing-box 在跑)
    let info: HashMap<Uuid, (String, bool)> = sqlx::query_as::<_, (Uuid, String, bool)>(
        "select r.machine_id, r.protocols, coalesce((a.info->'singbox'->>'running')::bool, false) from relay r left join agent_state a on a.machine_id = r.machine_id where r.machine_id = any($1)",
    )
    .bind(&relays)
    .fetch_all(&st.db)
    .await?
    .into_iter()
    .map(|(id, p, s)| (id, (p, s)))
    .collect();
    let ports = if at { crate::entries::anytls_live(&st.db, &relays).await? } else { HashMap::new() };
    let (mut exits, mut rows, mut skipped) = (vec![], vec![], vec![]);
    for x in &snap.exits {
        let (Some(er), Some(srv), Some(port)) = (x.entry.as_ref(), x.server.as_deref(), x.port) else { continue };
        let mut e = match crate::jobs::proto_exit(only, &x.name, er, srv, port, x.anytls(), at) {
            Some(Ok(e)) => e,
            Some(Err(why)) => {
                blockers.push(format!("出口「{}」：{why}", x.name));
                continue;
            }
            None => continue,
        };
        let (protocols, singbox) = info.get(&er.relay_id).map(|(p, s)| (p.as_str(), *s)).unwrap_or(("", false));
        // 路由上有人加的、从没对上过的出口：不把入口的真凭据写进去（同同步的 untrusted）
        let why = if x.sync == "drift" && !x.linked {
            Some("从没对上过这个入口（可能是路由上有人加的），不切；确认是自己的先 fleetctl router sync --exit 点名同步".to_string())
        } else if at && protocols == crate::entries::REALITY {
            Some(format!("线路机 {} 只开 Reality", er.relay_name))
        } else if at && !singbox {
            Some(format!("线路机 {} 的 AnyTLS（sing-box）没在跑：先升级它的 agent", er.relay_name))
        } else if !at && protocols == crate::entries::ANYTLS {
            Some(format!("线路机 {} 只开 AnyTLS", er.relay_name))
        } else if at {
            match ports.get(&er.id) {
                Some(p) => {
                    e.port = Some(*p);
                    None
                }
                None => Some("入口在线路机上没有 AnyTLS 端口（和别的端口撞了）".to_string()),
            }
        } else {
            None
        };
        if let Some(why) = why {
            if only.contains(&x.name) {
                blockers.push(format!("出口「{}」：{why}", x.name));
            }
            skipped.push(json!({"exit": x.name, "entry": er.name, "relay": er.relay_name, "why": why}));
            continue;
        }
        rows.push(json!({
            "exit": x.name, "entry": er.name, "relay": er.relay_name, "from": format!("{srv}:{port}"), "to": e.to(),
            "from_proto": if x.anytls() { clash::ANYTLS } else { clash::REALITY }, "to_proto": to,
        }));
        exits.push(e);
    }
    if r.state() != "online" {
        blockers.push(format!("软路由「{}」现在不在线：等它在线再切", r.name));
    }
    if exits.is_empty() && blockers.is_empty() {
        blockers.push(format!("软路由「{}」没有要切成 {} 的出口（指向面板入口的都已经是，或都切不了）", r.name, proto_cn(to)));
    }
    let router = RouterPlan { id: r.id, name: r.name.clone(), state: r.state().to_string(), exits };
    Ok(ProtoPlan { to, router, rows, blockers, skipped })
}

fn proto_cn(p: &str) -> &'static str { if p == clash::ANYTLS { "AnyTLS" } else { "Reality" } }

/// 预览：会把哪些出口切成 `to`（出口名、对上的入口与线路机、现在 → 改成）与切不了的原因
pub async fn protocol_preview(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiQuery(q): ApiQuery<ProtoQ>) -> ApiResult<Json<Value>> {
    let to = check_to(&q.to)?;
    let r = row(&st, id).await?;
    Ok(ok(proto_plan(&st, &r, to, &names(q.exits.as_deref())).await?.view()))
}

/// 发起：建 router_protocol 作业（同同步：备份 → 改写 → 重启 OpenClash → 测延迟，失败全部改回）
pub async fn protocol(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<ProtoIn>) -> ApiResult<Json<Value>> {
    let to = check_to(&b.to)?;
    let r = row(&st, id).await?;
    let p = proto_plan(&st, &r, to, &b.exits).await?;
    if !p.blockers.is_empty() {
        return Err(ApiError::conflict(p.blockers.join("；")).with_detail(json!({"blockers": p.blockers})));
    }
    let mut tx = st.db.begin().await?;
    let busy: Option<Uuid> = sqlx::query_scalar("select id from job where target_id = $1 and state in ('queued', 'running', 'waiting', 'cancelling')").bind(id).fetch_optional(&mut *tx).await?;
    if let Some(j) = busy {
        return Err(ApiError::conflict(format!("这台软路由已经有一个改出口的作业没做完（{j}）")).with_detail(json!({"job_id": j})));
    }
    let exits: Vec<&str> = p.router.exits.iter().map(|e| e.exit.as_str()).collect();
    // 执行时只动这几个出口、而且只在它们还对着现在核对过的入口时动
    let pins: serde_json::Map<String, Value> = p.router.exits.iter().filter_map(|e| Some((e.exit.clone(), json!(e.new_entry_id?)))).collect();
    let params = json!({
        "router_ids": [id],
        "routers": true,
        "to": to,
        "exits": exits,
        "pins": pins,
        "plan": {"routers": [serde_json::to_value(&p.router).unwrap_or_default()]},
        "router_name": r.name,
    });
    let job = jobs::enqueue(&mut tx, "router_protocol", Some(id), params, &a.name).await?;
    audit(&mut tx, &a.name, "router.protocol", "router", Some(id), json!({"name": r.name, "to": to, "exits": exits, "job_id": job})).await?;
    tx.commit().await?;
    st.wake_jobs();
    st.emit(Event::Job { id: job, target: Some(id) });
    let mut v = p.view();
    v["job_id"] = json!(job);
    Ok(ok(v))
}

#[derive(Deserialize)]
pub struct RemoveIn {
    /// 要删的出口名
    #[serde(default)]
    exits: Vec<String>,
    /// 出口不是面板的入口（对不上任何入口：客户自己加的，或对应的节点早已删除）也删
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize)]
pub struct RemoveQ {
    /// 逗号分隔的出口名
    exits: Option<String>,
    force: Option<bool>,
}

struct RemovePlan {
    router: RouterPlan,
    /// 要删的出口：出口名、对上的入口、此刻的客户端数与连接数
    rows: Vec<Value>,
    blockers: Vec<String>,
    warnings: Vec<String>,
}

impl RemovePlan {
    fn view(&self) -> Value {
        json!({
            "router": {"id": self.router.id, "name": self.router.name},
            "exits": self.rows,
            "ok": self.blockers.is_empty(),
            "blockers": self.blockers,
            "warnings": self.warnings,
        })
    }
}

/// 按最近一次采集算：删这几个出口会怎样（客户端、策略组、之后不再被出口指着的备用机器）、不能删的原因。
/// 真正删什么由路由上的脚本按当时的配置定（同名出口、流式写法、兜底规则指向它、别处还引用它都会拒绝，一行不写）
async fn remove_plan(st: &AppState, r: &routers::Row, only: &[String], force: bool) -> ApiResult<RemovePlan> {
    let snap = r.snapshot().unwrap_or_default();
    let (mut blockers, mut warnings, mut rows) = (vec![], vec![], vec![]);
    let mut names: Vec<String> = vec![];
    for n in only {
        if !names.contains(n) {
            names.push(n.clone());
        }
    }
    if names.is_empty() {
        blockers.push("没有点名要删的出口".to_string());
    }
    let mut entries = vec![];
    for name in &names {
        let Some(x) = snap.exits.iter().find(|x| &x.name == name) else {
            blockers.push(format!("最近一次采集里没有出口「{name}」"));
            continue;
        };
        match &x.entry {
            Some(er) => entries.push(er.node_id),
            None if force => {}
            None => blockers.push(format!("出口「{name}」不是面板的入口（对不上任何入口，可能是路由上自己加的）：确认要删的话加 --force")),
        }
        if x.clients > 0 {
            warnings.push(format!("出口「{name}」上现在有 {} 个客户端：删掉后这些设备按后面的规则走（兜底规则多半是拒绝，就上不了网了）", x.clients));
        }
        rows.push(json!({"exit": x.name, "entry": x.entry.as_ref().map(|e| e.name.clone()), "server": x.server, "port": x.port, "clients": x.clients, "conns": x.conns}));
    }
    // 策略组（GLOBAL 是 mihomo 自带的，不在配置里）：去掉这些出口；删完就空了的路由上的脚本也会拒绝，这里先说
    for g in snap.groups.iter().filter(|g| g.name != "GLOBAL") {
        let gone: Vec<&str> = g.all.iter().filter(|m| names.contains(m)).map(String::as_str).collect();
        if gone.is_empty() {
            continue;
        }
        if gone.len() == g.all.len() {
            blockers.push(format!("策略组「{}」的成员全是要删的出口，删完它就空了：先在路由上改这个组", g.name));
        } else {
            warnings.push(format!("策略组「{}」里去掉 {}", g.name, gone.join("、")));
        }
    }
    let left = snap.exits.iter().filter(|x| x.entry.is_some() && !names.contains(&x.name)).count();
    if left == 0 && !names.is_empty() {
        warnings.push(format!("删完软路由「{}」就没有对应面板入口的出口了", r.name));
    }
    // 指着的节点：备用里的删完不再被出口指着，到期满 1 天后中控自动回收；在用的说一声（团队里它就没有客户了）。
    // 删完还剩几个出口指着它：备用机看视图的 router_exits（含经同机 IP 节点的）；在用节点视图里不算 router_exits（永远是空的），
    // 按各路由的快照现算（routers::exits_using），否则「这台路由别的出口还指着它」的在用节点也会被说成「不再经它出去」
    let live = crate::views::live(st).await?;
    let mut seen = vec![];
    for id in &entries {
        if !seen.contains(id) {
            seen.push(*id);
        }
    }
    let using = crate::routers::exits_using(&st.db, &seen).await?;
    for id in seen {
        let Some(m) = live.iter().find(|m| m.id == id) else { continue };
        let rest = if m.spare {
            let mine = |l: &String| names.iter().any(|n| l == &format!("{} · {n}", r.name) || l.starts_with(&format!("{} · {n}（", r.name)));
            m.router_exits.iter().filter(|l| !mine(l)).count()
        } else {
            using.get(&id).into_iter().flatten().filter(|(rid, _, exit)| !(*rid == r.id && names.contains(exit))).count()
        };
        if m.spare && rest == 0 && m.live_ip_nodes.is_empty() {
            let when = m.recycle_on.map(|d| format!("{d} 起")).unwrap_or_else(|| "到期满 1 天后".into());
            warnings.push(format!("「{}」删完就没有出口指着了：{when}中控自动回收（要马上回收用 fleetctl retire {} --now）", m.name, m.name));
        } else if !m.spare && m.kind != "relay" && rest == 0 {
            warnings.push(format!("「{}」是在用的节点：删完软路由「{}」就不再经它出去（不用了的话退役：fleetctl retire {}）", m.name, r.name, m.name));
        }
    }
    if r.state() != "online" {
        blockers.push(format!("软路由「{}」现在不在线：等它在线再删", r.name));
    }
    warnings.push("删完要重启 OpenClash：经这台路由的连接会全部断开、马上重连".into());
    let router = RouterPlan { id: r.id, name: r.name.clone(), state: r.state().to_string(), exits: vec![] };
    Ok(RemovePlan { router, rows, blockers, warnings })
}

/// 预览：删哪些出口、客户端与策略组会怎样、不能删的原因
pub async fn remove_preview(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiQuery(q): ApiQuery<RemoveQ>) -> ApiResult<Json<Value>> {
    let r = row(&st, id).await?;
    Ok(ok(remove_plan(&st, &r, &names(q.exits.as_deref()), q.force.unwrap_or(false)).await?.view()))
}

/// 发起：建 router_exit_rm 作业（备份 → 删出口、规则、组里的引用 → 重启 OpenClash → 确认删掉了，不行整份换回）
pub async fn remove(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<RemoveIn>) -> ApiResult<Json<Value>> {
    let r = row(&st, id).await?;
    let p = remove_plan(&st, &r, &b.exits, b.force).await?;
    if !p.blockers.is_empty() {
        return Err(ApiError::conflict(p.blockers.join("；")).with_detail(json!({"blockers": p.blockers})));
    }
    let mut tx = st.db.begin().await?;
    let busy: Option<Uuid> = sqlx::query_scalar("select id from job where target_id = $1 and state in ('queued', 'running', 'waiting', 'cancelling')").bind(id).fetch_optional(&mut *tx).await?;
    if let Some(j) = busy {
        return Err(ApiError::conflict(format!("这台软路由已经有一个改出口的作业没做完（{j}）")).with_detail(json!({"job_id": j})));
    }
    let exits: Vec<&str> = p.rows.iter().filter_map(|x| x["exit"].as_str()).collect();
    let params = json!({"exits": exits, "force": b.force, "router_name": r.name});
    let job = jobs::enqueue(&mut tx, "router_exit_rm", Some(id), params, &a.name).await?;
    audit(&mut tx, &a.name, "router.exits_remove", "router", Some(id), json!({"name": r.name, "exits": exits, "force": b.force, "job_id": job})).await?;
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

// ── 只重启 OpenClash ───────────────────────────────────────────────────────

/// 这台路由已经有没做完的作业（改出口、重启…）：409
async fn not_busy(tx: &mut crate::db::Tx<'_>, id: Uuid) -> ApiResult<()> {
    let busy: Option<Uuid> = sqlx::query_scalar("select id from job where target_id = $1 and state in ('queued', 'running', 'waiting', 'cancelling')").bind(id).fetch_optional(&mut **tx).await?;
    if let Some(j) = busy {
        return Err(ApiError::conflict(format!("这台软路由已经有一个作业没做完（{j}）：等它结束再发起")).with_detail(json!({"job_id": j})));
    }
    Ok(())
}

/// 重启的预览：会断开多少连接、重启后测哪些出口（现在通不通）
fn restart_plan(r: &routers::Row) -> Value {
    let snap = r.snapshot().unwrap_or_default();
    let mut blockers = vec![];
    if r.state() != "online" {
        blockers.push(format!("软路由「{}」现在不在线：等它在线再重启", r.name));
    } else if !snap.openclash.enabled {
        blockers.push(format!("软路由「{}」的 OpenClash 没有启用：重启会把它停掉，不归这里做", r.name));
    }
    let exits: Vec<Value> = snap
        .exits
        .iter()
        .filter(|x| x.sync != "unmanaged")
        .map(|x| json!({"exit": x.name, "entry": x.entry.as_ref().map(|e| e.name.clone()), "alive": routers::collect::usable(x), "delay_ms": x.delay_ms}))
        .collect();
    let mut warnings = vec![format!(
        "重启 OpenClash 会断开经这台路由的全部连接（此刻 {} 个连接、{} 个在线客户端），客户端马上重连；路由自身流量走 OpenClash 的话隧道也会断一下，中控等它回连",
        snap.openclash.conns.unwrap_or(0),
        snap.clients.iter().filter(|c| c.online).count()
    )];
    let dead: Vec<&str> = exits.iter().filter(|x| x["alive"] == false).filter_map(|x| x["exit"].as_str()).collect();
    if !dead.is_empty() {
        warnings.push(format!("这些出口现在就不通，重启后不通不算重启出的问题：{}", dead.join("、")));
    }
    json!({
        "router": {"id": r.id, "name": r.name},
        "ok": blockers.is_empty(),
        "blockers": blockers,
        "openclash": {"running": snap.openclash.running, "version": snap.openclash.version, "conns": snap.openclash.conns},
        "exits": exits,
        "warnings": warnings,
    })
}

/// 预览：重启会断开什么、重启后测哪些出口
pub async fn restart_preview(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let r = row(&st, id).await?;
    Ok(ok(restart_plan(&r)))
}

/// 有 blocker 时 409（`detail.blockers`）
fn blocked(v: &Value) -> ApiResult<()> {
    if v["ok"] == true {
        return Ok(());
    }
    let b: Vec<String> = serde_json::from_value(v["blockers"].clone()).unwrap_or_default();
    Err(ApiError::conflict(b.join("；")).with_detail(json!({"blockers": b})))
}

/// 发起：建 router_restart 作业（重启 OpenClash、等 API、测 fleet 出口；配置不动）
pub async fn restart(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let r = row(&st, id).await?;
    let mut v = restart_plan(&r);
    blocked(&v)?;
    let mut tx = st.db.begin().await?;
    not_busy(&mut tx, id).await?;
    let job = jobs::enqueue(&mut tx, "router_restart", Some(id), json!({"router_name": r.name}), &a.name).await?;
    audit(&mut tx, &a.name, "router.restart", "router", Some(id), json!({"name": r.name, "job_id": job})).await?;
    tx.commit().await?;
    st.wake_jobs();
    st.emit(Event::Job { id: job, target: Some(id) });
    v["job_id"] = json!(job);
    Ok(ok(v))
}

// ── 看配置（只读） ────────────────────────────────────────────────────────

/// 经隧道读路由上的东西：不在线、连不上、脚本说读不了 409（写明原因）；没有那份备份 404
async fn read_live(st: &AppState, r: &routers::Row, script: &str) -> ApiResult<String> {
    if r.state() != "online" {
        return Err(ApiError::conflict(format!("软路由「{}」现在不在线，读不了它的配置", r.name)));
    }
    routers::ops::read_remote(st, r.id, r.tunnel_port, script).await.map_err(|(code, why)| {
        if code == routers::ops::NO_BACKUP {
            ApiError::not_found(format!("软路由「{}」：{why}", r.name))
        } else {
            ApiError::conflict(format!("软路由「{}」：{why}", r.name))
        }
    })
}

/// 读了路由上的东西：记审计 router.config_read（`view` 是读的什么）
async fn audit_read(st: &AppState, a: &Actor, r: &routers::Row, view: &str, extra: Value) -> ApiResult<()> {
    let mut detail = json!({"name": r.name, "view": view});
    if let (Some(d), Some(e)) = (detail.as_object_mut(), extra.as_object()) {
        d.extend(e.clone());
    }
    let mut tx = st.db.begin().await?;
    audit(&mut tx, &a.name, "router.config_read", "router", Some(r.id), detail).await?;
    tx.commit().await?;
    Ok(())
}

/// `GET /routers/{id}/config`：现在的源配置的结构（出口、策略组、规则 → 出口；凭据不给）
pub async fn config(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let r = row(&st, id).await?;
    let text = read_live(&st, &r, &routers::replace::read_script()).await?;
    audit_read(&st, &a, &r, "config", json!({})).await?;
    let snap = r.snapshot();
    let mut v = routers::ops::structure(&text, snap.as_ref()).map_err(|e| ApiError::conflict(format!("软路由「{}」：{e}", r.name)))?;
    let oc = snap.as_ref().map(|s| &s.openclash);
    v["router"] = json!({"id": r.id, "name": r.name});
    v["config_path"] = json!(oc.and_then(|o| o.config_path.clone()));
    v["config_mtime"] = json!(oc.and_then(|o| o.config_mtime).map(crate::meta::fmt_ts));
    v["bytes"] = json!(text.len());
    Ok(ok(v))
}

/// `GET /routers/{id}/config/backups`：路由上 /etc/fleet-backup/openclash/ 里的备份（中控作业改配置前留的），新的在前
pub async fn config_backups(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let r = row(&st, id).await?;
    let out = read_live(&st, &r, &routers::ops::backups_script()).await?;
    audit_read(&st, &a, &r, "backups", json!({})).await?;
    let (backups, path) = routers::ops::parse_backups(&out);
    Ok(ok(json!({"router": {"id": r.id, "name": r.name}, "config_path": path, "backups": backups})))
}

#[derive(Deserialize)]
pub struct DiffQ {
    /// 备份的文件名，或只写时间戳（20261023-073012Z）
    backup: String,
}

/// `GET /routers/{id}/config/diff?backup=`：现在的源配置和这份备份逐行比较（在中控算；两边先隐去凭据，同一次比较里同值同标记）
pub async fn config_diff(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiQuery(q): ApiQuery<DiffQ>) -> ApiResult<Json<Value>> {
    let name = q.backup.trim();
    if !routers::ops::backup_name_ok(name) {
        return Err(ApiError::bad_request("backup 写备份的文件名（c.yaml.20261023-073012Z）或时间戳（20261023-073012Z）：fleetctl router config <路由> --backups 看有哪些"));
    }
    let r = row(&st, id).await?;
    let old = read_live(&st, &r, &routers::ops::backup_read_script(name)).await?;
    let cur = read_live(&st, &r, &routers::replace::read_script()).await?;
    audit_read(&st, &a, &r, "diff", json!({"backup": name})).await?;
    let salt = crate::crypto::random_token(32);
    let (old, cur) = (routers::ops::mask(&old, salt.as_bytes()), routers::ops::mask(&cur, salt.as_bytes()));
    let mut v = routers::ops::diff(&old, &cur, &format!("备份 {name}"), "现在").map_err(ApiError::conflict)?;
    v["router"] = json!({"id": r.id, "name": r.name});
    v["backup"] = json!(name);
    Ok(ok(v))
}

// ── 出口改名跟上面板 ──────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct RenameIn {
    /// 只改这几个出口（空 = 全部已建立对应、名字对不上面板的）
    #[serde(default)]
    exits: Vec<String>,
    /// 点名的出口从没对上过入口、或指向直出 / 备用团队 / 别的团队的节点，仍要改
    #[serde(default)]
    force: bool,
}

/// 改名的计划（按最近一次采集定改哪些，再按路由此刻的配置算要改的行）：(预览, 改名表, 读没读路由的配置)
async fn rename_plan(st: &AppState, r: &routers::Row, only: &[String], force: bool) -> ApiResult<(Value, Vec<routers::ops::Rename>, bool)> {
    let snap = r.snapshot().unwrap_or_default();
    let node_ids: Vec<Uuid> = snap.exits.iter().filter_map(|x| x.entry.as_ref().map(|e| e.node_id)).collect();
    let rows: Vec<(Uuid, String, Option<i32>, bool)> = sqlx::query_as(
        "select m.id, display_name(m.kind::text, m.label, t.name, m.number), m.number, coalesce(t.spare, false) from machine m left join team t on t.id = m.team_id where m.id = any($1)",
    )
    .bind(&node_ids)
    .fetch_all(&st.db)
    .await?;
    let direct: std::collections::HashSet<Uuid> = snap.exits.iter().filter_map(|x| x.entry.as_ref().filter(|e| e.node_id == e.relay_id).map(|e| e.node_id)).collect();
    let nodes: HashMap<Uuid, routers::ops::NodeNum> =
        rows.into_iter().map(|(id, name, number, spare)| (id, routers::ops::NodeNum { name, number, spare, direct: direct.contains(&id) })).collect();
    let ids: Vec<Uuid> = snap.exits.iter().filter_map(|x| x.entry.as_ref().map(|e| e.id)).collect();
    let foreign = foreign_why(&st.db, r, &ids).await?;
    let mut p = routers::ops::rename_plan(&snap, &nodes, &foreign, only, force);
    // 临时切换的记录按出口名记：改名后对不上，先切回
    let involved: Vec<&str> = p.map.iter().flat_map(|m| [m.old.as_str(), m.new.as_str()]).collect();
    for s in switch::active(&st.db, Some(r.id)).await? {
        if involved.contains(&s.from_exit.as_str()) || involved.contains(&s.to_exit.as_str()) {
            p.blockers.push(format!("策略组「{}」正在临时切换（{} → {}），记录按出口名记：先切回（fleetctl router switch {} {} --back）再改名", s.grp, s.from_exit, s.to_exit, r.name, s.grp));
        }
    }
    if r.state() != "online" {
        p.blockers.push(format!("软路由「{}」现在不在线：等它在线再改", r.name));
    }
    if p.map.is_empty() && p.blockers.is_empty() {
        p.blockers.push(format!("软路由「{}」没有要改名的出口（名字都已经和面板的编号对上，或都不归这里改，见 skipped / untrusted）", r.name));
    }
    let (mut lines, mut groups, mut read) = (vec![], vec![], false);
    if p.blockers.is_empty() {
        let text = read_live(st, r, &routers::replace::read_script()).await?;
        read = true;
        match routers::ops::rename_patch(&text, &p.map) {
            Ok((patch, sel)) => {
                lines = patch.edits.iter().map(|e| json!({"line": e.line, "was": e.was, "now": e.now})).collect();
                groups = sel;
            }
            Err(e) => p.blockers.extend(e),
        }
    }
    let mut warnings = vec!["改完要重启 OpenClash：经这台路由的连接会全部断开、马上重连".to_string()];
    if !groups.is_empty() {
        warnings.push(format!("select 组（{}）现在选着要改名的出口的，改完按新名字选回同一个出口", groups.join("、")));
    }
    warnings.push("出口的历史指标、「已建立的对应」跟着改到新名字下".into());
    let v = json!({
        "router": {"id": r.id, "name": r.name},
        "ok": p.blockers.is_empty(),
        "blockers": p.blockers,
        "renames": p.rows,
        "skipped": p.skipped,
        "untrusted": p.untrusted,
        "need_force": p.need_force,
        "lines": lines,
        "warnings": warnings,
    });
    Ok((v, p.map, read))
}

/// 预览：哪些出口改成什么、要改的行、跳过的及原因
pub async fn rename_preview(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiQuery(q): ApiQuery<SyncQ>) -> ApiResult<Json<Value>> {
    let r = row(&st, id).await?;
    let (v, _, read) = rename_plan(&st, &r, &names(q.exits.as_deref()), q.force.unwrap_or(false)).await?;
    if read {
        audit_read(&st, &a, &r, "rename_preview", json!({})).await?;
    }
    Ok(ok(v))
}

/// 发起：建 router_rename 作业（备份 → 改名 → 重启 OpenClash → 核对、select 组选回，不行整份换回）
pub async fn rename(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<RenameIn>) -> ApiResult<Json<Value>> {
    let r = row(&st, id).await?;
    let (mut v, map, _) = rename_plan(&st, &r, &b.exits, b.force).await?;
    blocked(&v)?;
    let mut tx = st.db.begin().await?;
    not_busy(&mut tx, id).await?;
    let params = json!({"map": map, "router_name": r.name, "plan": v["renames"], "force": b.force});
    let job = jobs::enqueue(&mut tx, "router_rename", Some(id), params, &a.name).await?;
    let renames: Vec<Value> = map.iter().map(|m| json!({"from": m.old, "to": m.new})).collect();
    audit(&mut tx, &a.name, "router.rename_exits", "router", Some(id), json!({"name": r.name, "renames": renames, "force": b.force, "job_id": job})).await?;
    tx.commit().await?;
    st.wake_jobs();
    st.emit(Event::Job { id: job, target: Some(id) });
    v["job_id"] = json!(job);
    Ok(ok(v))
}

// ── 来源 IP 分流规则 ──────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RulesQ {
    /// 逗号分隔的「来源=出口」（来源是 CIDR、IP 或 IP 段 a-b）；不给只列出现有的
    assign: Option<String>,
}

#[derive(Deserialize)]
pub struct RulesIn {
    assign: Vec<routers::ops::Assign>,
}

fn parse_assign(s: &str) -> ApiResult<Vec<routers::ops::Assign>> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(|x| {
            let (spec, to) = x.split_once('=').ok_or_else(|| ApiError::bad_request(format!("「{x}」要写成 来源=出口（10.31.5.0/24=节点04）")))?;
            Ok(routers::ops::Assign { spec: spec.trim().to_string(), to: to.trim().to_string() })
        })
        .collect()
}

/// 规则的计划（按路由此刻的配置）：(预览, 计划明细)。`assign` 空 = 只列出现有的 SRC-IP-CIDR 规则
async fn rules_plan(st: &AppState, a: &Actor, r: &routers::Row, assign: &[routers::ops::Assign]) -> ApiResult<(Value, Vec<Value>)> {
    let text = read_live(st, r, &routers::replace::read_script()).await?;
    audit_read(st, a, r, "rules", json!({"assign": assign.len()})).await?;
    let snap = r.snapshot().unwrap_or_default();
    let view = routers::ops::structure(&text, Some(&snap)).map_err(|e| ApiError::conflict(format!("软路由「{}」：{e}", r.name)))?;
    let mut v = json!({
        "router": {"id": r.id, "name": r.name},
        "src_rules": view["src_rules"], "rules_total": view["rules_total"], "match": view["match"], "rule_targets": view["rule_targets"],
    });
    if assign.is_empty() {
        return Ok((v, vec![]));
    }
    let mut blockers: Vec<String> = vec![];
    let (mut rows, mut lines) = (vec![], vec![]);
    match routers::ops::rules_patch(&text, assign) {
        Ok((p, rs)) => {
            lines = p.edits.iter().map(|e| json!({"line": e.line, "was": e.was, "now": e.now})).collect();
            rows = rs;
        }
        Err(e) => blockers.extend(e),
    }
    // 目标出口现在就不通：改完核对测不通会整份换回，先说
    for t in assign.iter().map(|x| x.to.as_str()).collect::<std::collections::BTreeSet<_>>() {
        if let Some(x) = snap.exits.iter().find(|x| x.name == t).filter(|x| !routers::collect::usable(x)) {
            blockers.push(format!("出口「{t}」现在不通（连续 {} 次测延迟不通）：改完核对测不通会整份换回，先让它恢复", x.fail_streak));
        }
    }
    if blockers.is_empty() && !rows.is_empty() && rows.iter().all(|x| x["op"] == "same") {
        blockers.push("这些来源已经都指向给的出口了，没有要改的".into());
    }
    let mut warnings = vec!["改完要重启 OpenClash：经这台路由的连接会全部断开、马上重连".to_string()];
    let moved: Vec<String> = rows
        .iter()
        .filter(|x| x["op"] == "retarget")
        .map(|x| format!("{}（{} → {}）", x["cidr"].as_str().unwrap_or(""), x["from"].as_str().unwrap_or(""), x["to"].as_str().unwrap_or("")))
        .collect();
    if !moved.is_empty() {
        warnings.push(format!("这些来源现在有规则指着别的出口，改完换出口：{}", moved.join("、")));
    }
    if r.state() != "online" {
        blockers.push(format!("软路由「{}」现在不在线：等它在线再改", r.name));
    }
    v["ok"] = json!(blockers.is_empty());
    v["blockers"] = json!(blockers);
    v["rows"] = json!(rows);
    v["lines"] = json!(lines);
    v["warnings"] = json!(warnings);
    Ok((v, rows))
}

/// `GET /routers/{id}/rules[?assign=来源=出口,…]`：不给 assign 列出现有的 SRC-IP-CIDR 规则（来源 IP → 出口）；给了是改规则的预览
pub async fn rules_preview(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiQuery(q): ApiQuery<RulesQ>) -> ApiResult<Json<Value>> {
    let assign = parse_assign(q.assign.as_deref().unwrap_or(""))?;
    let r = row(&st, id).await?;
    Ok(ok(rules_plan(&st, &a, &r, &assign).await?.0))
}

/// 发起：建 router_rules 作业（备份 → 加 / 改规则 → 重启 OpenClash → 目标出口测通、规则生效，不行整份换回）
pub async fn rules(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<RulesIn>) -> ApiResult<Json<Value>> {
    if b.assign.is_empty() {
        return Err(ApiError::bad_request("没有给要改的规则（assign）"));
    }
    let r = row(&st, id).await?;
    let (mut v, rows) = rules_plan(&st, &a, &r, &b.assign).await?;
    blocked(&v)?;
    let mut tx = st.db.begin().await?;
    not_busy(&mut tx, id).await?;
    let params = json!({"assign": b.assign, "rows": rows, "router_name": r.name});
    let job = jobs::enqueue(&mut tx, "router_rules", Some(id), params, &a.name).await?;
    audit(&mut tx, &a.name, "router.rules", "router", Some(id), json!({"name": r.name, "assign": b.assign, "rows": rows, "job_id": job})).await?;
    tx.commit().await?;
    st.wake_jobs();
    st.emit(Event::Job { id: job, target: Some(id) });
    v["job_id"] = json!(job);
    Ok(ok(v))
}
