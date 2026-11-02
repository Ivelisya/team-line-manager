//! 替换节点：新节点接替旧节点。软路由上指向旧节点入口的出口改成新节点的入口（名字、分组、规则不动），
//! 再交换编号：旧节点去备用团队（或指定的团队 / 空号），新节点接旧节点的编号（客户看到的名字不变）。
//! 这里只做校验、预览、建作业和写审计；步骤在 jobs/steps/replace.rs，路由上的脚本在 routers/replace.rs。
//!
//! - 要改哪些路由：预览按各路由最近一次采集的快照，出口对上的入口属于旧节点、且没删除的就改（routers::exits_to，退役预览同一套匹配）；
//!   作业执行时按路由当时的配置重新确定（同一套匹配，`replace_exit`）。
//!   新入口按线路机一一对应（旧入口在哪台线路机，就换成新节点在同一台上的入口）；对不上时新节点只有一个在用入口就用它，否则报错说明。
//! - 改编号与 PATCH /machines/{id} 同一套代码（machines::patch_tx）：同样的校验、审计（machine.rename）、刷新线路机。
//!   编号先挪开旧节点、再给新节点，新节点原来所在的团队随后压缩（同退役）。旧节点拿的号预览与作业同一份计算（`old_to_number`）：
//!   新节点在目标团队里的号算空着——新节点从备用来时旧节点就接它在备用里的号（作业先放临时号再改过去），备用团队谁都不改名。旧节点不退役：
//!   放进备用团队后由备用到期自动回收（spare.rs）接手。

use super::ok;
use crate::auth::Actor;
use crate::db::{audit, Tx};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::jobs;
use crate::meta;
use crate::names::{display_name, node_name};
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

/// 出口指向要改的入口、却不在线（离线 / 还没接入）而要改路由时挡住发起的软路由；`exits` 是它挡住的出口（换线路机批量、搬空时按路由合并各节点的）
#[derive(Clone, Debug, Serialize)]
pub struct OfflineRouter {
    pub id: Uuid,
    pub name: String,
    /// offline / pending
    pub state: String,
    pub error: Option<String>,
    pub exits: Vec<OfflineExit>,
}

#[derive(Clone, Debug, Serialize)]
pub struct OfflineExit {
    /// 出口指向的节点（显示名）
    pub node: String,
    pub exit: String,
}

impl OfflineRouter {
    /// 阻断原因的文字（单个节点的预览、409 报错、搬空跳过的原因）。`(verb, what)` 同 `scan_routers`
    pub fn text(&self, (verb, what): (&str, &str)) -> String {
        let why = self.error.as_deref().map(|e| format!("：{e}")).unwrap_or_default();
        let mut exits: Vec<&str> = vec![];
        for e in &self.exits {
            if !exits.contains(&e.exit.as_str()) {
                exits.push(&e.exit);
            }
        }
        format!(
            "软路由「{}」现在{}{why}。它的出口 {} 指向{what}，要等它在线再{verb}（或选不改软路由）",
            self.name,
            if self.state == "offline" { "离线" } else { "还没接入" },
            exits.join("、")
        )
    }
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
    texts: (&str, &str),
    blockers: &mut Vec<String>,
    warnings: &mut Vec<String>,
    pick: impl FnMut(&EntryRef, &str, u16) -> Option<Result<ExitPlan, String>>,
) -> Vec<RouterPlan> {
    let mut offline = vec![];
    let out = scan_routers_offline(rows, node, team, enabled, blocking, texts.1, blockers, warnings, &mut offline, pick);
    blockers.extend(offline.iter().map(|o| o.text(texts)));
    out
}

/// `scan_routers` 的底层（`what` 是提示里的「旧节点」/「旧入口」）：不在线的软路由不写进 blockers 文字，而是结构化地放进 `offline`（出口的 `node` 留空，由调用方填）
#[allow(clippy::too_many_arguments)]
pub fn scan_routers_offline(
    rows: &[crate::routers::Row],
    node: Uuid,
    team: Option<Uuid>,
    enabled: bool,
    blocking: bool,
    what: &str,
    blockers: &mut Vec<String>,
    warnings: &mut Vec<String>,
    offline: &mut Vec<OfflineRouter>,
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
            offline.push(OfflineRouter {
                id: r.id,
                name: r.name.clone(),
                state: state.to_string(),
                error: r.error.clone(),
                exits: exits.iter().map(|e| OfflineExit { node: String::new(), exit: e.exit.clone() }).collect(),
            });
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

/// 旧节点在目标团队拿不到编号的原因（`old_to_number`）
#[derive(Debug, PartialEq, Eq)]
pub enum NumberErr {
    /// 给的编号不在 1–999（输入错误，提交时回 400）
    Range,
    /// 给的编号已被别的机器占着
    Taken(i32),
    /// 没给编号，目标团队 1–999 都占满了
    Full,
}

impl std::fmt::Display for NumberErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NumberErr::Range => f.write_str("编号范围 1–999"),
            NumberErr::Taken(n) => write!(f, "编号 {n} 已被占用"),
            NumberErr::Full => f.write_str("旧节点要去的团队编号用完了"),
        }
    }
}

/// 目标团队 `to_team` 里已占的编号（不算 `skip` 里的机器）；目标就是旧团队时旧编号要留给新节点，也算占着
async fn taken_numbers(conn: &mut PgConnection, to_team: Uuid, skip: &[Uuid], (old_team, old_number): (Uuid, i32)) -> sqlx::Result<HashSet<i32>> {
    let taken: Vec<i32> = sqlx::query_scalar(
        "select number from machine where team_id = $1 and number is not null and kind <> 'relay' and lifecycle <> 'retired' and id <> all($2)",
    )
    .bind(to_team)
    .bind(skip)
    .fetch_all(&mut *conn)
    .await?;
    let mut taken: HashSet<i32> = taken.into_iter().collect();
    if to_team == old_team {
        taken.insert(old_number);
    }
    Ok(taken)
}

/// 旧节点在目标团队 `to_team` 最后拿哪个编号：预览（plan）与作业（jobs/steps/replace.rs `renumber`，锁住团队和两台机器后再算一次）共用这一份，
/// 不各算各的。`given` 是指定的编号，没给取最小空号。
/// 算空号时不算旧节点自己（它要挪走），**新节点在目标团队里的编号也算空着**：交换完它就离开了，旧节点正好接它空出来的号——
/// 新节点从备用团队来（最常见的用法）时旧节点就接新节点在备用里的号，备用团队不用压缩、谁都不改名。
/// 这个号在交换那一刻还被新节点占着，作业要先把旧节点放到临时号、新节点挪走后再改过去（`swap_slot`）。目标就是旧团队时，旧编号要给新节点，算占着
pub async fn old_to_number(
    conn: &mut PgConnection,
    to_team: Uuid,
    (old, new): (Uuid, Uuid),
    (old_team, old_number): (Uuid, i32),
    given: Option<i32>,
) -> sqlx::Result<Result<i32, NumberErr>> {
    if given.is_some_and(|n| !(1..=999).contains(&n)) {
        return Ok(Err(NumberErr::Range));
    }
    let taken = taken_numbers(conn, to_team, &[old, new], (old_team, old_number)).await?;
    Ok(match given {
        Some(n) if taken.contains(&n) => Err(NumberErr::Taken(n)),
        Some(n) => Ok(n),
        None => (1..=999).find(|n| !taken.contains(n)).ok_or(NumberErr::Full),
    })
}

/// 旧节点要接的号此刻还被新节点占着（新节点就在目标团队、号正是 `to_number`）：交换时要先把旧节点放到临时号（`temp_number`）
pub fn swap_slot(new_team: Option<Uuid>, new_number: Option<i32>, to_team: Uuid, to_number: i32) -> bool { new_team == Some(to_team) && new_number == Some(to_number) }

/// `swap_slot` 时旧节点先放的临时号：目标团队里除旧节点外谁都没占的最小号（新节点的号占着）；目标就是旧团队时旧编号要留给新节点，也不能用
pub async fn temp_number(conn: &mut PgConnection, to_team: Uuid, old: Uuid, old_at: (Uuid, i32)) -> sqlx::Result<Option<i32>> {
    let taken = taken_numbers(conn, to_team, &[old], old_at).await?;
    Ok((1..=999).find(|n| !taken.contains(n)))
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
    // 两台机器作为目标的，和作为别的替换 / 改出口指向的新节点的（那个作业的目标是它的旧节点）
    let busy: Vec<(Uuid, String)> = sqlx::query_as(
        "select case when target_id = any($1) then target_id else (params->>'new_id')::uuid end, kind from job
         where state in ('queued', 'running', 'waiting', 'cancelling')
           and (target_id = any($1) or (kind in ('replace', 'repoint') and params->>'new_id' = any($1::uuid[]::text[])))",
    )
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
        // 和作业同一份计算（old_to_number）：新节点在目标团队里的号算空着，交换时旧节点接它
        to_number = match old_to_number(&mut *conn, *t, (old.id, new.id), (old_team, old_number), b.old_to.number).await? {
            Ok(n) => Some(n),
            Err(e) => {
                if e == NumberErr::Range {
                    invalid.push(e.to_string());
                }
                blockers.push(e.to_string());
                None
            }
        };
    }
    let old_after = match (&to_team, to_number) {
        (Some((_, tn, _)), Some(n)) => node_name(tn, n),
        _ => old_name.clone(),
    };
    let new_after = node_name(&old_team_name, old_number);
    // 软路由计划先算（旧节点进备用后是不是被扣住，要看出口还指不指着它）；它的提示与阻断留到最后再接
    let routers_enabled = b.routers != Some(false);
    let (mut router_blockers, mut router_warnings) = (vec![], vec![]);
    let routers = if old.kind == "relay" || new.kind == "relay" {
        vec![]
    } else {
        router_plan(st, conn, &old, &new, old_team, routers_enabled, &mut router_blockers, &mut router_warnings).await?
    };
    if new.team_id != Some(old_team) && new.kind != "relay" {
        warnings.push(format!("新节点「{new_name}」不在旧节点的团队（{old_team_name}），替换时改到这个团队"));
    }
    if to_team.as_ref().is_some_and(|t| !t.2) {
        warnings.push("旧节点要去的不是备用团队：不会到期自动回收".into());
    } else if to_team.is_some() {
        // 替换不带走旧节点上的同机 IP 节点：它们留在原团队继续用，宿主机退役会连带它们，所以旧节点进备用后不会自动回收（视图 live_ip_nodes），
        // 这时「什么时候回收」那几条（过期、自动续费后照常回收）都不成立，只留这一句
        let live_ips: Vec<String> = sqlx::query_scalar(
            "select display_name(m.kind::text, m.label, t.name, m.number) from machine m left join team t on t.id = m.team_id
             where m.host_id = $1 and m.lifecycle <> 'retired' and not coalesce(t.spare, false) order by t.sort_order nulls first, t.name, m.number, m.id",
        )
        .bind(old.id)
        .fetch_all(&mut *conn)
        .await?;
        // 进备用之后什么时候回收（spare.rs）：没有订阅的永远不回收，已过期满 1 天的下一轮维护就回收；开着的自动续费会关掉（machines::patch_tx）
        let sub: Option<(time::Date, time::Date, bool)> =
            sqlx::query_as("select due_date, local_date(billing_tz), auto_renew from subscription_eff where machine_id = $1").bind(old.id).fetch_optional(&mut *conn).await?;
        // 被扣住（不自动回收、要照常续费）：有在用的同机 IP 节点、已经在备用里且开着自动续费的同机 IP 节点（视图 renewing_ip_nodes），或不改软路由而出口还指着旧节点
        let renewing = super::machines::renewing_spare_ip_nodes(&mut *conn, old.id).await?;
        let held = super::machines::spare_held_reason(!routers_enabled && !routers.is_empty(), !live_ips.is_empty(), renewing);
        if let Some(reason) = held {
            if sub.is_some_and(|s| s.2) {
                warnings.push(format!("旧节点「{old_name}」开着自动续费：进备用时中控这边会关掉{}", super::machines::spare_held_tail(&reason)));
            }
            if renewing {
                warnings.push("旧节点上有已在备用、开着自动续费的同机 IP 节点：旧节点进备用后不会自动回收（它们照常续费），不要了先关掉那些 IP 节点的自动续费".into());
            }
            if !live_ips.is_empty() {
                warnings.push(format!("旧节点上的同机 IP 节点 {} 仍在用：旧节点进备用后不会自动回收，要处理先退役或换走这些 IP 节点", live_ips.join("、")));
            }
            if !routers_enabled && !routers.is_empty() {
                warnings.push("没有改软路由：出口仍指向旧节点，旧节点进备用后不会自动回收，要处理先改指向或之后「立即回收」".into());
            }
        } else {
            match sub {
                None => warnings.push(format!("旧节点「{old_name}」没有订阅：进备用后没有到期日，不会自动回收（不要了就之后「立即回收」）")),
                Some((due, today, _)) if crate::spare::expired(today, due) => {
                    warnings.push(format!("旧节点「{old_name}」的订阅已过期满 1 天：进备用后下一轮维护（一小时内）就自动回收；还想留几天对账，先续费或改到期日"))
                }
                Some((_, _, true)) => warnings.push(format!("旧节点「{old_name}」开着自动续费：进备用时会关掉，到期后照常回收（供应商那边的自动扣款要自己去关）")),
                Some(_) => {}
            }
        }
    }

    // 访问策略与订阅设置按机器 id 存：交换编号时随编号转给新节点（jobs/steps/replace.rs），预览列出来，免得替换后才发现客户的限制变了
    let old_policy = crate::policy::load_row(&mut *conn, crate::policy::Scope::Node, old.id).await?.filter(|r| !r.is_default());
    let old_subs = super::client_subs::names_of_machine(&mut *conn, old.id).await?;
    if old_policy.is_some() || !old_subs.is_empty() {
        let mut parts = vec![];
        if let Some(p) = &old_policy {
            parts.push(format!("访问策略（{}）", p.summary()));
        }
        if !old_subs.is_empty() {
            parts.push(format!("客户端订阅的排除 / 另加（{}）", old_subs.join("、")));
        }
        let mut w = format!("旧节点「{old_name}」单独设的{}随编号转给新节点「{new_name}」；旧节点进备用后跟随备用团队", parts.join("和"));
        if old_policy.is_some() {
            if let Some(p) = crate::policy::load_row(&mut *conn, crate::policy::Scope::Node, new.id).await?.filter(|r| !r.is_default()) {
                w += &format!("，新节点自己单独设的访问策略（{}）会被覆盖", p.summary());
            }
        }
        warnings.push(w);
    }

    // 新节点此刻的健康与到期：替换后它接客户（同改出口指向预览的写法）。只在预览里算：发起时（lock）持着两台机器的行锁，
    // views::live 重算会等这把锁到 VIEW_WAIT，不能让一条可选的提醒挡住提交（见 repoint.rs）
    if !lock && new.kind != "relay" {
        let live = crate::views::live(st).await?;
        if let Some(m) = live.iter().find(|m| m.id == new.id) {
            warnings.extend(new_node_notes(m, &new_name));
        }
    }

    // 编号压缩：新节点原来的团队少了一台（同退役）
    let mut renames = vec![];
    if let (Some(nt), Some(nn)) = (new.team_id, new.team_name.as_deref()) {
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
        let changes = crate::db::preview_compaction(&mut *conn, nt, nn, members, &mut warnings).await?;
        renames = changes.iter().map(|(id, o, n)| json!({"machine_id": id, "old_name": o, "new_name": n})).collect();
        // 压缩改名的节点还被出口指着：面板名和出口名会错位。旧节点自己不算（它的出口这次就改到新节点）
        let others: Vec<(Uuid, String, String)> = changes.into_iter().filter(|c| c.0 != old.id).collect();
        let tail = format!("不想错位就换一台新节点：用备用团队里的，或{nn}里编号最大的那台（后面没有节点要前移）");
        warnings.extend(crate::routers::compact_exits_note(&st.db, &others, &tail).await?);
    }

    // 软路由计划的提示与阻断接在最后（计划在上面先算出来）
    blockers.extend(router_blockers);
    warnings.extend(router_warnings);
    Ok(Plan { old, new, to_team, to_number, number_given: b.old_to.number.is_some(), old_after, new_after, renames, routers_enabled, routers, blockers, invalid, warnings })
}

/// 新节点的提醒（预览）：此刻故障 / 降级（写上原因：替换后客户的出口就走它），没开自动续费、提前提醒天数内到期或已过期（写到期日，同改出口指向预览）。
/// 从备用来的另说一句：进备用时自动续费关了，替换出来也不会自己打开
pub fn new_node_notes(m: &crate::views::MachineView, name: &str) -> Vec<String> {
    use crate::health::Status;
    let mut out = vec![];
    let why = || if m.health.reasons.is_empty() { String::new() } else { format!("（{}）", m.health.reasons.iter().map(|r| r.text.as_str()).collect::<Vec<_>>().join("；")) };
    match m.health.status {
        Status::Down => out.push(format!("新节点「{name}」此刻故障{}：替换后客户的出口就走它，会不通；先查清，或换一台正常的", why())),
        Status::Degraded => out.push(format!("新节点「{name}」此刻降级{}：照常能用，替换前先看看原因", why())),
        _ => {}
    }
    if let Some(s) = m.subscription.as_ref().filter(|s| !s.auto_renew && s.due_days <= i64::from(s.remind_days)) {
        let left = match s.due_days {
            d if d < 0 => format!("已过期 {} 天", -d),
            0 => "今天到期".into(),
            d => format!("还有 {d} 天"),
        };
        let spare = if m.spare { "；它在备用里，进备用时自动续费关了，替换出来也不会自己打开" } else { "" };
        out.push(format!("新节点「{name}」{} 到期（{left}）{spare}：替换后它接客户，要继续用记得登记续费（或打开自动续费）", s.due_date));
    }
    out
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

    /// 新节点的提醒：故障 / 降级写原因；没开自动续费、提前提醒天数内到期的写到期日（备用里来的另说自动续费已关）；开着自动续费、还早的不说
    #[test]
    fn new_node_notes_health_and_due() {
        use crate::health::tests::{base, sub, sub_with};
        use crate::health::{Reason, Status};
        let mut m = base("proxy");
        assert!(new_node_notes(&m, "备用-04").is_empty());
        m.health.status = Status::Down;
        m.health.reasons = vec![Reason { key: "tcp_fail", severity: "critical", text: "TCP 探测全部失败".into() }];
        m.spare = true;
        m.subscription = Some(sub(2));
        assert_eq!(
            new_node_notes(&m, "备用-04"),
            [
                "新节点「备用-04」此刻故障（TCP 探测全部失败）：替换后客户的出口就走它，会不通；先查清，或换一台正常的",
                "新节点「备用-04」2030-01-01 到期（还有 2 天）；它在备用里，进备用时自动续费关了，替换出来也不会自己打开：替换后它接客户，要继续用记得登记续费（或打开自动续费）",
            ]
        );
        m.health.status = Status::Degraded;
        m.health.reasons.clear();
        m.spare = false;
        m.subscription = Some(sub(-3));
        assert_eq!(
            new_node_notes(&m, "团队3-02"),
            ["新节点「团队3-02」此刻降级：照常能用，替换前先看看原因", "新节点「团队3-02」2030-01-01 到期（已过期 3 天）：替换后它接客户，要继续用记得登记续费（或打开自动续费）"]
        );
        m.health.status = Status::Ok;
        for s in [sub(30), sub_with(1, true)] {
            m.subscription = Some(s);
            assert!(new_node_notes(&m, "x").is_empty());
        }
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
