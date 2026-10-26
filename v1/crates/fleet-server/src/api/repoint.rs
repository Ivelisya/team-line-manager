//! 改出口指向：软路由上指向入口 A 的出口改成入口 B（名字、分组、规则不动），两个入口本身都不动（不建、不摘、不改编号）。
//! 用途：直出出口换一台线路机（线路机1-直出 → 线路机3-直出）、节点出问题时临时把客户的出口指到备用节点、再指回来。
//! 只改指定的路由（`router_ids`，空 = 所有指向入口 A 的路由）。这里只做校验、预览、建作业和写审计；
//! 作业只有一步「改软路由出口」，与替换、换线路机共用（jobs/steps/replace.rs：执行时按路由当时的配置重新确定、备份 → 改 → 重启 OpenClash →
//! 测延迟，任何一台失败全部换回）。出口写的是旧线路机 IP 就换成新线路机的 IP，否则换成新线路机的 public_host（同换线路机）。

use super::lifecycle::{move_exit, relay_addr};
use super::ok;
use super::replace::{routers_view, scan_routers, RouterPlan};
use crate::auth::Actor;
use crate::db::{audit, Tx};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::jobs;
use crate::state::{AppState, Event};
use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

/// 入口：(id, 节点, 线路机, 端口, 状态, 显示名「线路机1-直出 · 线路机1:20010」)
#[derive(sqlx::FromRow, Clone, Debug)]
struct Ent {
    id: Uuid,
    node_id: Uuid,
    relay_id: Uuid,
    port: i32,
    lifecycle: String,
    node_name: String,
    relay_name: String,
}

impl Ent {
    fn name(&self) -> String { format!("{} · {}:{}", self.node_name, self.relay_name, self.port) }
}

const ENT: &str = "select e.id, e.node_id, e.relay_id, e.port, e.lifecycle::text lifecycle, coalesce(rm.label, '') relay_name,
        case when e.node_id = e.relay_id then coalesce(rm.label, '') || '-直出' else display_name(n.kind::text, n.label, t.name, n.number) end node_name
    from entry e join machine rm on rm.id = e.relay_id join machine n on n.id = e.node_id left join team t on t.id = n.team_id where e.id = $1";

#[derive(Deserialize)]
pub struct RepointIn {
    to_entry_id: Uuid,
    /// 只改这几台路由（空 = 所有指向旧入口的）
    #[serde(default)]
    router_ids: Vec<Uuid>,
}

#[derive(Deserialize)]
pub struct PreviewQ {
    to_entry_id: Uuid,
    /// 逗号分隔的路由 id
    routers: Option<String>,
}

struct Plan {
    old: Ent,
    new: Ent,
    router_ids: Vec<Uuid>,
    routers: Vec<RouterPlan>,
    blockers: Vec<String>,
    invalid: Vec<String>,
    warnings: Vec<String>,
}

async fn plan(st: &AppState, conn: &mut PgConnection, old_id: Uuid, b: &RepointIn, lock: bool) -> ApiResult<Plan> {
    let (mut blockers, mut invalid, mut warnings) = (vec![], vec![], vec![]);
    let old: Option<Ent> = sqlx::query_as(ENT).bind(old_id).fetch_optional(&mut *conn).await?;
    let old = old.ok_or_else(|| ApiError::not_found("没有这个入口"))?;
    let new: Option<Ent> = sqlx::query_as(ENT).bind(b.to_entry_id).fetch_optional(&mut *conn).await?;
    let Some(new) = new else { return Err(ApiError::bad_request("要改成的入口不存在")) };
    if old.id == new.id {
        blockers.push("新入口就是旧入口".into());
        invalid.push("新入口就是旧入口".into());
    }
    if old.lifecycle == "removed" {
        blockers.push(format!("旧入口「{}」已经删除", old.name()));
    }
    if new.lifecycle != "active" {
        blockers.push(format!("新入口「{}」现在不在用（{}）：等它生效再改", new.name(), new.lifecycle));
    }
    // 新入口那一侧有作业要摘它（换线路机的旧入口就是它、新节点在退役或被替换）：改过去之后随即被摘。
    // 发起时先锁新节点那一行（for update）：和换线路机（对同一行 for share）、退役、替换（for update）都互斥，
    // 先提交的作业后一方都看得到（新节点按 new_id 认）。for share 和换线路机的 for share 不互斥：两边同时发起会都成功，
    // 路由改指向的新入口随即被换线路机摘掉。这是本事务的第一把行锁，之后只读和插作业 / 审计行（job.target_id 没有外键），不会和别人互相等死
    if lock {
        sqlx::query("select 1 from machine where id = $1 for update").bind(new.node_id).execute(&mut *conn).await?;
    }
    let busy: Option<(String,)> = sqlx::query_as(
        "select kind from job where state in ('queued', 'running', 'waiting', 'cancelling')
           and ((kind = 'move_entry' and params->>'old_entry_id' = $1::text) or (kind in ('retire', 'replace') and target_id = $2)) limit 1",
    )
    .bind(new.id)
    .bind(new.node_id)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some((k,)) = busy {
        blockers.push(format!("「{}」还有进行中的作业（{}）：新入口随后会被摘掉或换走，等它结束再改", new.node_name, crate::meta::job_kind_label(&k)));
    }
    let mut rows = crate::routers::load(&st.db, None).await?;
    if !b.router_ids.is_empty() {
        for id in &b.router_ids {
            if !rows.iter().any(|r| r.id == *id) {
                blockers.push(format!("没有软路由 {id}"));
                invalid.push(format!("没有软路由 {id}"));
            }
        }
        rows.retain(|r| b.router_ids.contains(&r.id));
    }
    let from_ip = relay_addr(&mut *conn, old.relay_id).await?.0;
    let to = relay_addr(&mut *conn, new.relay_id).await?;
    let routers = scan_routers(&rows, old.node_id, None, true, true, ("改", "旧入口"), &mut blockers, &mut warnings, |er, srv, port| {
        move_exit(er, old.id, Some((new.id, new.port as u16)), &from_ip, &to, srv, port).map(Ok)
    });
    if routers.is_empty() && invalid.is_empty() {
        blockers.push(format!("没有{}软路由的出口指向「{}」：没有要改的", if b.router_ids.is_empty() { "" } else { "选中的" }, old.name()));
    }
    if new.node_id != old.node_id {
        warnings.push(format!("改完后这些出口的流量走「{}」：入口本身都不动，「{}」照常在用", new.name(), old.name()));
    }
    // 下面都只是预览里的提醒：发起（lock）时不算。views::live 在重算时会持着上面对新节点的行锁等到 VIEW_WAIT，
    // 视图 503 时提交也跟着失败，不能让一条可选的提醒挡住提交
    if !lock {
        // 目标是备用节点（临时把出口指过去的常见去向）：闲置的一指过去就关系到客户（开始告警、计入健康、不再自动回收），已被扣住的只是多一个原因；
        // 自动续费关着的到期要自己登记续费。目标此刻故障的更要说
        let live = crate::views::live(st).await?;
        if let Some(m) = live.iter().find(|m| m.id == new.node_id) {
            if m.spare {
                let due = m.subscription.as_ref().map(|s| format!("{} 到期", s.due_date)).unwrap_or_else(|| "没有订阅".into());
                // 自动续费关着的到期要自己登记续费；已经被扣住的（已有出口指着、宿主机上有在用 IP 节点、开着自动续费）只是多一个原因
                let renew = if m.subscription.as_ref().is_some_and(|s| s.auto_renew) { "开着自动续费" } else { "到期要自己登记续费" };
                warnings.push(if m.idle_spare {
                    format!("「{}」是备用节点，{due}：改指过去后它关系到客户（开始告警、计入健康），不再自动回收；{renew}", new.node_name)
                } else {
                    format!("「{}」是备用节点，{due}：已被扣住、不自动回收，改指过去后多一个原因；{renew}", new.node_name)
                });
            }
            if m.health.status == crate::health::Status::Down {
                warnings.push(format!("「{}」此刻故障：改过去之后这些出口会不通", new.node_name));
            }
        }
        // 旧节点是已过期满 1 天的备用机，只因为出口还指着它才没被回收：这些出口都改走后（没有别的原因扣着它）下一轮维护就自动回收
        let planned: usize = routers.iter().map(|r| r.exits.len()).sum();
        if let Some(o) = live.iter().find(|m| m.id == old.node_id && m.id != new.node_id) {
            let expired = o.subscription.as_ref().is_some_and(|s| s.due_days <= -2 && !s.auto_renew);
            if o.spare && o.lifecycle == "active" && expired && planned > 0 && o.router_exits.len() <= planned && o.live_ip_nodes.is_empty() && o.renewing_ip_nodes.is_empty() {
                warnings.push(format!("「{}」是已过期满 1 天的备用节点：改走后它不再被出口指着，下一轮维护（一小时内）会自动回收", old.node_name));
            }
        }
        if let Some(w) = policy_diff(&mut *conn, &old, &new).await? {
            warnings.push(w);
        }
    }
    Ok(Plan { old, new, router_ids: b.router_ids.clone(), routers, blockers, invalid, warnings })
}

/// 客户实际受的是入口所在节点的访问策略（节点单独设的 > 团队的 > 默认）：新旧入口所在节点的生效策略不同时说一声。
/// 只看现在已发布的（线路机上跑的就是它）；同一个节点、或两边一样就没有提醒
async fn policy_diff(conn: &mut PgConnection, old: &Ent, new: &Ent) -> ApiResult<Option<String>> {
    use crate::policy;
    if old.node_id == new.node_id {
        return Ok(None);
    }
    let teams: Vec<(Uuid, Option<Uuid>, Option<String>)> =
        sqlx::query_as("select m.id, m.team_id, t.name from machine m left join team t on t.id = m.team_id where m.id = any($1)").bind(vec![old.node_id, new.node_id]).fetch_all(&mut *conn).await?;
    let team_of = |id: Uuid| teams.iter().find(|t| t.0 == id).map(|t| (t.1, t.2.clone())).unwrap_or((None, None));
    let set = policy::set_current(&policy::load_rows(&mut *conn, false).await?);
    let eff = |e: &Ent| {
        let (team_id, team_name) = team_of(e.node_id);
        let (d, src) = set.effective(e.node_id, team_id);
        let who = src.label(&e.node_name, team_name.as_deref());
        (d, who)
    };
    let ((od, ow), (nd, nw)) = (eff(old), eff(new));
    if !policy::differs(&od, &nd) {
        return Ok(None);
    }
    Ok(Some(format!("新出口受{nw}的访问策略：{}，与原来（{ow}：{}）不同", nd.summary(), od.summary())))
}

impl Plan {
    fn view(&self) -> Value {
        let side = |e: &Ent| json!({"id": e.id, "name": e.name(), "node_id": e.node_id, "relay_id": e.relay_id, "port": e.port});
        json!({
            "old": side(&self.old),
            "new": side(&self.new),
            "router_ids": self.router_ids,
            "routers": routers_view(&self.routers),
            "ok": self.blockers.is_empty(),
            "blockers": self.blockers,
            "warnings": self.warnings,
        })
    }
}

fn parse_ids(s: Option<&str>) -> ApiResult<Vec<Uuid>> {
    s.unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(|x| Uuid::try_parse(x).map_err(|_| ApiError::bad_request(format!("软路由 id 不对：{x}"))))
        .collect()
}

/// 预览：会改哪些路由的哪些出口，以及不能改的原因
pub async fn preview(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiQuery(q): ApiQuery<PreviewQ>) -> ApiResult<Json<Value>> {
    let b = RepointIn { to_entry_id: q.to_entry_id, router_ids: parse_ids(q.routers.as_deref())? };
    let mut conn = st.db.acquire().await?;
    let p = plan(&st, &mut conn, id, &b, false).await?;
    Ok(ok(p.view()))
}

/// 发起：建 repoint 作业（只有「改软路由出口」一步）
pub async fn repoint(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<RepointIn>) -> ApiResult<Json<Value>> {
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
    st.emit(Event::Job { id: job, target: Some(p.old.node_id) });
    let mut v = p.view();
    v["job_id"] = json!(job);
    Ok(ok(v))
}

async fn enqueue(tx: &mut Tx<'_>, actor: &str, p: &Plan) -> ApiResult<Uuid> {
    let params = json!({
        "old_entry_id": p.old.id,
        "new_entry_id": p.new.id,
        "new_id": p.new.node_id,
        "from_relay_id": p.old.relay_id,
        "to_relay_id": p.new.relay_id,
        "router_ids": p.router_ids,
        "routers": true,
        "plan": {"routers": serde_json::to_value(&p.routers).unwrap_or_default()},
        "old_name": p.old.name(),
        "new_name": p.new.name(),
    });
    let job = jobs::enqueue(tx, "repoint", Some(p.old.node_id), params, actor).await?;
    let routers: Vec<Value> = p.routers.iter().map(|r| json!({"id": r.id, "name": r.name, "exits": r.exits.iter().map(|e| &e.exit).collect::<Vec<_>>()})).collect();
    audit(
        tx,
        actor,
        "entry.repoint",
        "machine",
        Some(p.old.node_id),
        json!({"from": p.old.name(), "from_id": p.old.id, "to": p.new.name(), "to_id": p.new.id, "routers": routers, "job_id": job}),
    )
    .await?;
    Ok(job)
}
