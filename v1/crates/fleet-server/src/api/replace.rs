//! 替换节点：新节点接替旧节点。软路由上指向旧节点入口的出口改成新节点的入口（名字、分组、规则不动），
//! 再交换编号：旧节点去备用团队（或指定的团队 / 空号），新节点接旧节点的编号（客户看到的名字不变）。
//! 这里只做校验、预览、建作业和写审计；步骤在 jobs/steps/replace.rs，路由上的脚本在 routers/replace.rs。
//!
//! - 要改哪些路由：预览按各路由最近一次采集的快照，出口对上的入口属于旧节点、且没删除的就改（routers::exits_to，退役预览同一套匹配）；
//!   作业执行时按路由当时的配置重新确定（同一套匹配，`replace_exit`）。
//!   新入口按线路机一一对应（旧入口在哪台线路机，就换成新节点在同一台上的入口）；对不上时新节点只有一个在用入口就用它，否则报错说明。
//! - 改编号与 PATCH /machines/{id} 同一套代码（machines::patch_tx）：同样的校验、审计（machine.rename）、刷新线路机。
//!   编号先挪开旧节点、再给新节点，`numbering.auto_compact` 打开时新节点原来所在的团队随后压缩（同退役）。旧节点不退役：
//!   放进备用团队后由备用到期自动回收（spare.rs）接手。

use super::ok;
use crate::auth::Actor;
use crate::db::{audit, Tx};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::jobs;
use crate::meta;
use crate::names::{compact_numbers, display_name, node_name};
use crate::routers::clash::{host_eq, EntryRef};
use crate::state::{AppState, Event};
use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgConnection;
use std::collections::HashSet;
use uuid::Uuid;

/// 路由上要改的一个出口。发起时按快照算的存进作业参数（预览、审计里给人看）；执行时按路由当时的配置重新算一遍，按重新算的改
/// （jobs/steps/replace.rs）。凭据不在这里，执行时从库里取。
/// 替换节点和换线路机（api/lifecycle.rs）共用；换线路机的新入口在作业里才建，预览里 `new_entry_id` / `port` 为空
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExitPlan {
    pub exit: String,
    pub old_entry_id: Uuid,
    pub new_entry_id: Option<Uuid>,
    /// 出口现在的 server:port（核对用：路由上它必须还指向这里，或已经是新入口）
    pub old_server: String,
    pub old_port: u16,
    /// 要改成的 server:port：出口原来写的是线路机 IP 就写新线路机的 IP，否则写它的 public_host
    pub server: String,
    pub port: Option<u16>,
    /// 改不了的原因（替换时新节点上对不上入口）：预览照样列出这个出口，同时是 blocker
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ExitPlan {
    /// 预览里的「改成」：换线路机的新端口建好才知道
    pub fn to(&self) -> String { format!("{}:{}", self.server, self.port.map_or("新端口".to_string(), |p| p.to_string())) }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RouterPlan {
    pub id: Uuid,
    pub name: String,
    /// 预览时的状态（online / offline / pending）
    pub state: String,
    pub exits: Vec<ExitPlan>,
}

#[derive(sqlx::FromRow)]
struct Node {
    id: Uuid,
    kind: String,
    lifecycle: String,
    team_id: Option<Uuid>,
    team_name: Option<String>,
    number: Option<i32>,
    label: Option<String>,
}

impl Node {
    fn name(&self) -> String { display_name(&self.kind, self.label.as_deref(), self.team_name.as_deref(), self.number) }
}

async fn load_node(conn: &mut PgConnection, id: Uuid, lock: bool) -> ApiResult<Option<Node>> {
    let sql = format!(
        "select m.id, m.kind::text kind, m.lifecycle::text lifecycle, m.team_id, t.name team_name, m.number, m.label
         from machine m left join team t on t.id = m.team_id where m.id = $1{}",
        if lock { " for update of m" } else { "" }
    );
    Ok(sqlx::query_as(&sql).bind(id).fetch_optional(conn).await?)
}

/// 节点的入口：(id, 线路机, 端口, 状态, public_host, 线路机 IP, 线路机名)
#[derive(sqlx::FromRow, Clone, Debug)]
pub struct Ent {
    pub id: Uuid,
    pub relay_id: Uuid,
    pub port: i32,
    pub lifecycle: String,
    pub public_host: String,
    pub relay_ip: String,
    pub relay_name: String,
}

const ENTS: &str = "select e.id, e.relay_id, e.port, e.lifecycle::text lifecycle, r.public_host, host(rm.primary_ip) relay_ip, coalesce(rm.label, '') relay_name
    from entry e join relay r on r.machine_id = e.relay_id join machine rm on rm.id = e.relay_id where e.node_id = $1";

/// 旧节点没删除的入口、新节点在用的入口（先建的在前）：出口按它们换（预览与执行共用）
pub async fn replace_entries(conn: &mut PgConnection, old: Uuid, new: Uuid) -> sqlx::Result<(Vec<Ent>, Vec<Ent>)> {
    let old_es = sqlx::query_as(&format!("{ENTS} and e.lifecycle <> 'removed'")).bind(old).fetch_all(&mut *conn).await?;
    let new_es = sqlx::query_as(&format!("{ENTS} and e.lifecycle = 'active' order by e.created_at")).bind(new).fetch_all(&mut *conn).await?;
    Ok((old_es, new_es))
}

/// 替换：对上入口 `er`（server:port 是 `srv:port`）的出口改成什么。None = 不是旧节点的入口；Err = 新节点上对不上
pub fn replace_exit(old_es: &[Ent], new_es: &[Ent], er: &EntryRef, srv: &str, port: u16) -> Option<Result<ExitPlan, String>> {
    let oe = old_es.iter().find(|e| e.id == er.id)?;
    Some(map_entry(oe, new_es).map(|ne| ExitPlan {
        exit: String::new(),
        old_entry_id: oe.id,
        new_entry_id: Some(ne.id),
        old_server: srv.to_string(),
        old_port: port,
        server: new_server(srv, &oe.relay_ip, &ne.relay_ip, &ne.public_host),
        port: Some(ne.port as u16),
        error: None,
    }))
}

/// 旧入口换成新节点的哪个入口：同一台线路机上的；没有时新节点只有一个在用入口就用它，否则说明为什么对不上
pub fn map_entry<'a>(old: &Ent, new: &'a [Ent]) -> Result<&'a Ent, String> {
    if let Some(e) = new.iter().find(|e| e.relay_id == old.relay_id) {
        return Ok(e);
    }
    match new {
        [one] => Ok(one),
        [] => Err("新节点没有在用的入口".into()),
        many => Err(format!(
            "旧入口在线路机 {} 上，新节点在这台线路机上没有入口，而它有 {} 个入口（{}），不知道换成哪个：先给新节点在 {} 上加入口，或只留一个入口",
            old.relay_name,
            many.len(),
            many.iter().map(|e| format!("{}:{}", e.relay_name, e.port)).collect::<Vec<_>>().join("、"),
            old.relay_name
        )),
    }
}

/// 出口要改成的 server：原来写的是旧线路机的 IP 就写新线路机的 IP，否则写新线路机的 public_host
pub fn new_server(old_server: &str, old_relay_ip: &str, new_relay_ip: &str, new_host: &str) -> String {
    if host_eq(old_server, old_relay_ip) { new_relay_ip.to_string() } else { new_host.to_string() }
}

/// 各路由最近一次采集的快照里指向 `node` 的出口，`pick` 决定每个出口怎么改（None = 不改，Err = 改不了的原因：照样列出、带上原因，
/// `blocking` 时进 blockers）。计划里有出口的路由不在线、又要改路由（`enabled`）时阻断；`team` 里的路由没有快照时提醒，要改路由时任何路由配置读不了都提醒。
/// `(verb, what)`：「替换」/「旧节点」、「换线路机」/「旧入口」，拼进提示
#[allow(clippy::too_many_arguments)]
pub fn scan_routers(
    rows: &[crate::routers::Row],
    node: Uuid,
    team: Option<Uuid>,
    enabled: bool,
    blocking: bool,
    (verb, what): (&str, &str),
    blockers: &mut Vec<String>,
    warnings: &mut Vec<String>,
    mut pick: impl FnMut(&EntryRef, &str, u16) -> Option<Result<ExitPlan, String>>,
) -> Vec<RouterPlan> {
    let mut out = vec![];
    for r in rows {
        let Some(snap) = r.snapshot() else {
            if team.is_some() && r.team_id == team {
                warnings.push(format!("软路由「{}」还没有采集到状态，看不出它有没有用{what}", r.name));
            }
            continue;
        };
        // 配置读不了的不限团队都提醒：执行时还读不了，作业就失败、一台都不改（jobs/steps/replace.rs）
        if let Some(e) = snap.openclash.config_error.as_deref().filter(|_| enabled) {
            warnings.push(if snap.openclash.config_stale {
                format!("软路由「{}」的配置读不了（{e}），出口按上次读到的配置列出；执行时还读不了，作业会失败、一台都不改", r.name)
            } else {
                format!("软路由「{}」的配置读不了（{e}），看不出它有没有用{what}；执行时还读不了，作业会失败、一台都不改", r.name)
            });
        }
        let mut exits = vec![];
        for (x, er) in crate::routers::exits_to(&snap, node) {
            let (Some(srv), Some(port)) = (x.server.as_deref(), x.port) else { continue };
            match pick(er, srv, port) {
                Some(Ok(e)) => exits.push(ExitPlan { exit: x.name.clone(), ..e }),
                Some(Err(why)) => {
                    if blocking {
                        blockers.push(format!("软路由「{}」的出口「{}」：{why}", r.name, x.name));
                    }
                    exits.push(ExitPlan {
                        exit: x.name.clone(),
                        old_entry_id: er.id,
                        new_entry_id: None,
                        old_server: srv.to_string(),
                        old_port: port,
                        server: String::new(),
                        port: None,
                        error: Some(why),
                    });
                }
                None => {}
            }
        }
        if exits.is_empty() {
            continue;
        }
        let state = r.state();
        if enabled && state != "online" {
            let why = r.error.as_deref().map(|e| format!("：{e}")).unwrap_or_default();
            blockers.push(format!(
                "软路由「{}」现在{}{why}。它的出口 {} 指向{what}，要等它在线再{verb}（或选不改软路由）",
                r.name,
                if state == "offline" { "离线" } else { "还没接入" },
                exits.iter().map(|e| e.exit.as_str()).collect::<Vec<_>>().join("、")
            ));
        }
        out.push(RouterPlan { id: r.id, name: r.name.clone(), state: state.to_string(), exits });
    }
    out
}

/// 预览 / 响应里的路由计划
pub fn routers_view(rs: &[RouterPlan]) -> Value {
    json!(rs
        .iter()
        .map(|r| json!({
            "id": r.id, "name": r.name, "state": r.state,
            "exits": r.exits.iter().map(|e| json!({
                "exit": e.exit, "old_entry_id": e.old_entry_id, "new_entry_id": e.new_entry_id,
                "from": format!("{}:{}", e.old_server, e.old_port), "to": if e.error.is_some() { Value::Null } else { json!(e.to()) }, "error": e.error,
            })).collect::<Vec<_>>(),
        }))
        .collect::<Vec<_>>())
}

/// 没指定团队时旧节点去哪：唯一的备用团队（迁移 0041 保证有且只有一个）
async fn default_spare(conn: &mut PgConnection) -> ApiResult<(Uuid, String)> {
    Ok(sqlx::query_as("select id, name from team where spare").fetch_one(&mut *conn).await?)
}

/// 替换的完整计划（预览与提交共用）
pub struct Plan {
    old: Node,
    new: Node,
    to_team: Option<(Uuid, String, bool)>,
    to_number: Option<i32>,
    number_given: bool,
    /// 替换后旧节点 / 新节点的名字
    old_after: String,
    new_after: String,
    renames: Vec<Value>,
    routers_enabled: bool,
    pub routers: Vec<RouterPlan>,
    blockers: Vec<String>,
    /// blockers 里属于输入错误的：提交时回 400，其余回 409
    invalid: Vec<String>,
    warnings: Vec<String>,
}

#[derive(Deserialize, Default)]
pub struct OldTo {
    team_id: Option<Uuid>,
    number: Option<i32>,
}

#[derive(Deserialize)]
pub struct ReplaceIn {
    new_machine_id: Uuid,
    #[serde(default)]
    old_to: OldTo,
    /// 改软路由（默认 true）
    routers: Option<bool>,
}

#[derive(Deserialize)]
pub struct PreviewQ {
    new_machine_id: Uuid,
    team_id: Option<Uuid>,
    number: Option<i32>,
    routers: Option<bool>,
}

async fn plan(st: &AppState, conn: &mut PgConnection, old_id: Uuid, b: &ReplaceIn, lock: bool) -> ApiResult<Plan> {
    if b.new_machine_id == old_id {
        return Err(ApiError::bad_request("新节点不能是旧节点自己"));
    }
    let (mut blockers, mut invalid, mut warnings) = (vec![], vec![], vec![]);
    // 两台机器按 id 顺序加锁（同一对机器反过来替换的请求不会互相等死）
    let (first, second) = if old_id < b.new_machine_id { (old_id, b.new_machine_id) } else { (b.new_machine_id, old_id) };
    let a = load_node(&mut *conn, first, lock).await?;
    let z = load_node(&mut *conn, second, lock).await?;
    let (old, new) = if first == old_id { (a, z) } else { (z, a) };
    let old = old.ok_or_else(|| ApiError::not_found("没有这台机器"))?;
    let Some(new) = new else { return Err(ApiError::bad_request("新节点不存在")) };
    let (old_name, new_name) = (old.name(), new.name());
    for (m, who) in [(&old, "旧节点"), (&new, "新节点")] {
        if m.kind == "relay" {
            blockers.push(format!("{who}「{}」是线路机：只有节点（VPS 节点 / 家宽代理）能替换", m.name()));
        } else if m.lifecycle != "active" {
            blockers.push(format!("{who}「{}」现在是「{}」，只有在用的节点能替换", m.name(), meta::lifecycle_label(&m.lifecycle)));
        }
    }
    let busy: Vec<(Uuid, String)> = sqlx::query_as("select target_id, kind from job where target_id = any($1) and state in ('queued', 'running', 'waiting', 'cancelling')")
        .bind(vec![old.id, new.id])
        .fetch_all(&mut *conn)
        .await?;
    for (t, k) in busy {
        let n = if t == old.id { &old_name } else { &new_name };
        blockers.push(format!("「{n}」还有进行中的作业（{}）：等它结束再替换", meta::job_kind_label(&k)));
    }
    let (Some(old_team), Some(old_team_name), Some(old_number)) = (old.team_id, old.team_name.clone(), old.number) else {
        return Err(ApiError::conflict(format!("旧节点「{old_name}」没有团队编号，不能替换")));
    };

    // 旧节点去哪
    let to_team: Option<(Uuid, String, bool)> = match b.old_to.team_id {
        Some(t) => {
            let r: Option<(String, bool)> = sqlx::query_as(if lock { "select name, spare from team where id = $1 for update" } else { "select name, spare from team where id = $1" })
                .bind(t)
                .fetch_optional(&mut *conn)
                .await?;
            match r {
                Some((n, s)) => Some((t, n, s)),
                None => {
                    blockers.push("旧节点要去的团队不存在".into());
                    invalid.push("旧节点要去的团队不存在".into());
                    None
                }
            }
        }
        None => {
            let (t, n) = default_spare(conn).await?;
            Some((t, n, true))
        }
    };
    let mut to_number = None;
    if let Some((t, _, _)) = &to_team {
        // 目标团队里占着的编号：不算旧、新两台（它们都要挪）；目标就是旧团队时，旧编号要给新节点
        let taken: Vec<i32> = sqlx::query_scalar(
            "select number from machine where team_id = $1 and number is not null and kind <> 'relay' and lifecycle <> 'retired' and id <> all($2)",
        )
        .bind(t)
        .bind(vec![old.id, new.id])
        .fetch_all(&mut *conn)
        .await?;
        let mut taken: HashSet<i32> = taken.into_iter().collect();
        if *t == old_team {
            taken.insert(old_number);
        }
        to_number = match b.old_to.number {
            Some(n) if !(1..=999).contains(&n) => {
                blockers.push("编号范围 1–999".into());
                invalid.push("编号范围 1–999".into());
                None
            }
            Some(n) if taken.contains(&n) => {
                blockers.push(format!("编号 {n} 已被占用"));
                None
            }
            Some(n) => Some(n),
            None => {
                let n = (1..=999).find(|n| !taken.contains(n));
                if n.is_none() {
                    blockers.push("旧节点要去的团队编号用完了".into());
                }
                n
            }
        };
    }
    let old_after = match (&to_team, to_number) {
        (Some((_, tn, _)), Some(n)) => node_name(tn, n),
        _ => old_name.clone(),
    };
    let new_after = node_name(&old_team_name, old_number);
    if new.team_id != Some(old_team) && new.kind != "relay" {
        warnings.push(format!("新节点「{new_name}」不在旧节点的团队（{old_team_name}），替换时改到这个团队"));
    }
    if to_team.as_ref().is_some_and(|t| !t.2) {
        warnings.push("旧节点要去的不是备用团队：不会到期自动回收".into());
    } else if to_team.is_some() {
        // 进备用之后什么时候回收（spare.rs）：没有订阅的永远不回收，已过期满 1 天的下一轮维护就回收；开着的自动续费会关掉（machines::patch_tx）
        let sub: Option<(time::Date, time::Date, bool)> =
            sqlx::query_as("select due_date, local_date(billing_tz), auto_renew from subscription_eff where machine_id = $1").bind(old.id).fetch_optional(&mut *conn).await?;
        match sub {
            None => warnings.push(format!("旧节点「{old_name}」没有订阅：进备用后没有到期日，不会自动回收（不要了就之后「立即回收」）")),
            Some((due, today, _)) if crate::spare::expired(today, due) => {
                warnings.push(format!("旧节点「{old_name}」的订阅已过期满 1 天：进备用后下一轮维护（一小时内）就自动回收；还想留几天对账，先续费或改到期日"))
            }
            Some((_, _, true)) => warnings.push(format!("旧节点「{old_name}」开着自动续费：进备用时会关掉，到期后照常回收（供应商那边的自动扣款要自己去关）")),
            Some(_) => {}
        }
    }

    // 编号压缩：新节点原来的团队少了一台（auto_compact 时，同退役）
    let mut renames = vec![];
    if let (true, Some(nt), Some(nn)) = (st.cfg.numbering.auto_compact, new.team_id, new.team_name.as_deref()) {
        let rows: Vec<(Uuid, i32)> = sqlx::query_as("select id, number from machine where team_id = $1 and number is not null and kind <> 'relay' and lifecycle <> 'retired' and id <> all($2)")
            .bind(nt)
            .bind(vec![old.id, new.id])
            .fetch_all(&mut *conn)
            .await?;
        let mut members = rows;
        if nt == old_team {
            members.push((new.id, old_number));
        }
        if to_team.as_ref().is_some_and(|t| t.0 == nt) {
            if let Some(n) = to_number {
                members.push((old.id, n));
            }
        }
        renames = compact_numbers(members)
            .into_iter()
            .map(|(id, o, n)| json!({"machine_id": id, "old_name": node_name(nn, o), "new_name": node_name(nn, n)}))
            .collect();
    }

    // 软路由
    let routers_enabled = b.routers != Some(false);
    let routers = if old.kind == "relay" || new.kind == "relay" {
        vec![]
    } else {
        router_plan(st, conn, &old, &new, old_team, routers_enabled, &mut blockers, &mut warnings).await?
    };
    Ok(Plan { old, new, to_team, to_number, number_given: b.old_to.number.is_some(), old_after, new_after, renames, routers_enabled, routers, blockers, invalid, warnings })
}

/// 哪些路由的哪些出口指向旧节点、各换成新节点的哪个入口
#[allow(clippy::too_many_arguments)]
async fn router_plan(
    st: &AppState,
    conn: &mut PgConnection,
    old: &Node,
    new: &Node,
    old_team: Uuid,
    enabled: bool,
    blockers: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> ApiResult<Vec<RouterPlan>> {
    let (old_es, new_es) = replace_entries(conn, old.id, new.id).await?;
    if new_es.is_empty() {
        blockers.push(format!("新节点「{}」没有在用的入口（上架完成、入口生效后再替换）", new.name()));
    }
    let rows = crate::routers::load(&st.db, None).await?;
    let has_new = !new_es.is_empty();
    // 新节点没有在用的入口时出口也照样列出（原因是这个），只是不再逐个出口重复阻断
    let out = scan_routers(&rows, old.id, Some(old_team), enabled, enabled && has_new, ("替换", "旧节点"), blockers, warnings, |er, srv, port| {
        replace_exit(&old_es, &new_es, er, srv, port)
    });
    if enabled && out.is_empty() {
        warnings.push("没有软路由的出口指向旧节点：只交换编号".into());
    }
    Ok(out)
}

impl Plan {
    fn view(&self) -> Value {
        let side = |n: &Node| json!({"id": n.id, "name": n.name(), "kind": n.kind, "team": n.team_id.map(|id| json!({"id": id, "name": n.team_name}))});
        json!({
            "old": side(&self.old),
            "new": side(&self.new),
            "new_name": self.new_after,
            "old_to": {
                "team": self.to_team.as_ref().map(|(id, name, spare)| json!({"id": id, "name": name, "spare": spare})),
                "number": self.to_number,
                "name": self.old_after,
            },
            "renames": self.renames,
            "routers_enabled": self.routers_enabled,
            "routers": routers_view(&self.routers),
            "ok": self.blockers.is_empty(),
            "blockers": self.blockers,
            "warnings": self.warnings,
        })
    }
}

/// 预览：会改哪些路由的哪些出口、编号怎么变、旧节点去哪，以及不能替换的原因
pub async fn preview(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiQuery(q): ApiQuery<PreviewQ>) -> ApiResult<Json<Value>> {
    let b = ReplaceIn { new_machine_id: q.new_machine_id, old_to: OldTo { team_id: q.team_id, number: q.number }, routers: q.routers };
    let mut conn = st.db.acquire().await?;
    let p = plan(&st, &mut conn, id, &b, false).await?;
    Ok(ok(p.view()))
}

/// 发起替换：建 replace 作业（改路由 → 交换编号）
pub async fn replace(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<ReplaceIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let p = plan(&st, &mut tx, id, &b, true).await?;
    if !p.invalid.is_empty() {
        return Err(ApiError::bad_request(p.invalid.join("；")).with_detail(json!({"blockers": p.blockers})));
    }
    if !p.blockers.is_empty() {
        return Err(ApiError::conflict(p.blockers.join("；")).with_detail(json!({"blockers": p.blockers})));
    }
    let job = enqueue(&mut tx, &a.name, &p).await?;
    tx.commit().await?;
    st.wake_jobs();
    st.emit(Event::Job { id: job, target: Some(id) });
    let mut v = p.view();
    v["job_id"] = json!(job);
    Ok(ok(v))
}

async fn enqueue(tx: &mut Tx<'_>, actor: &str, p: &Plan) -> ApiResult<Uuid> {
    let (to_team, to_name, _) = p.to_team.clone().ok_or_else(|| ApiError::internal("没有目标团队"))?;
    let params = json!({
        "new_id": p.new.id,
        "old_team_id": p.old.team_id,
        "old_number": p.old.number,
        "to_team_id": to_team,
        // 没指定编号的执行时再取一次空号（排队期间可能有人占了）
        "to_number": if p.number_given { json!(p.to_number) } else { Value::Null },
        "routers": p.routers_enabled,
        "plan": {"routers": if p.routers_enabled { serde_json::to_value(&p.routers).unwrap_or_default() } else { json!([]) }},
        "old_name": p.old.name(),
        "new_name": p.new.name(),
    });
    let job = jobs::enqueue(tx, "replace", Some(p.old.id), params, actor).await?;
    let routers: Vec<Value> = if p.routers_enabled {
        p.routers.iter().map(|r| json!({"id": r.id, "name": r.name, "exits": r.exits.iter().map(|e| &e.exit).collect::<Vec<_>>()})).collect()
    } else {
        vec![]
    };
    audit(
        tx,
        actor,
        "machine.replace",
        "machine",
        Some(p.old.id),
        json!({
            "name": p.old.name(), "new": p.new.name(), "new_id": p.new.id, "old_to": p.old_after, "to_team": to_name, "new_name": p.new_after,
            "routers": routers, "routers_enabled": p.routers_enabled, "job_id": job,
        }),
    )
    .await?;
    Ok(job)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ent(relay: Uuid, name: &str, port: i32) -> Ent {
        Ent { id: Uuid::now_v7(), relay_id: relay, port, lifecycle: "active".into(), public_host: format!("{name}.test"), relay_ip: "198.51.100.2".into(), relay_name: name.into() }
    }

    #[test]
    fn entries_map_by_relay_or_single() {
        let (r1, r2) = (Uuid::now_v7(), Uuid::now_v7());
        let old = ent(r1, "线路机1", 20003);
        let n1 = ent(r1, "线路机1", 20020);
        let n2 = ent(r2, "线路机2", 20030);
        assert_eq!(map_entry(&old, &[n2.clone(), n1.clone()]).unwrap().id, n1.id, "同一台线路机上的优先");
        assert_eq!(map_entry(&old, std::slice::from_ref(&n2)).unwrap().id, n2.id, "只有一个入口就用它");
        let n3 = ent(Uuid::now_v7(), "线路机3", 20040);
        let e = map_entry(&old, &[n2, n3]).unwrap_err();
        assert!(e.contains("线路机2:20030、线路机3:20040") && e.contains("先给新节点在 线路机1 上加入口"), "{e}");
        assert!(map_entry(&old, &[]).is_err());
        // 出口写的是线路机 IP 就换成新线路机的 IP，写的是域名就换成新线路机的 public_host
        let other = Ent { relay_ip: "203.0.113.7".into(), ..ent(r2, "线路机2", 20030) };
        assert_eq!(new_server("198.51.100.2", &old.relay_ip, &other.relay_ip, &other.public_host), "203.0.113.7");
        assert_eq!(new_server("线路机1.TEST.", &old.relay_ip, &other.relay_ip, &other.public_host), "线路机2.test");
    }
}
