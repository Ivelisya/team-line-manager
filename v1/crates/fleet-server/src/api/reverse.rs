//! 穿透开关（crate::reverse、作业 `reverse`）：预览 `GET /entries/{id}/reverse?on=true|false`、发起 `POST /entries/{id}/reverse {on}`，
//! 与「新入口默认模式」设置 `GET/PUT /settings/entry-mode`。这里只做校验、预览、建作业和写审计；推 bridge、切换、验收在 jobs/steps/bridge.rs。

use super::ok;
use crate::auth::Actor;
use crate::db::audit;
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::jobs;
use crate::reverse::{self, mode_zh, NORMAL, REVERSE};
use crate::state::{AppState, Event};
use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

#[derive(sqlx::FromRow)]
struct Ent {
    id: Uuid,
    node_id: Uuid,
    relay_id: Uuid,
    port: i32,
    lifecycle: String,
    mode: String,
    node_name: String,
    node_lifecycle: String,
    relay_name: String,
    relay_ip: String,
    direct: bool,
    egress_dynamic: bool,
}

impl Ent {
    fn name(&self) -> String { format!("{} · {}:{}", self.node_name, self.relay_name, self.port) }
}

struct Plan {
    e: Ent,
    on: bool,
    blockers: Vec<String>,
    /// 会发生什么（确认对话框、fleetctl 预览里逐条列出）
    impacts: Vec<String>,
    warnings: Vec<String>,
}

async fn plan(st: &AppState, conn: &mut PgConnection, id: Uuid, on: bool, lock: bool) -> ApiResult<Plan> {
    let e: Option<Ent> = sqlx::query_as(
        "select e.id, e.node_id, e.relay_id, e.port, e.lifecycle::text lifecycle, e.mode, e.node_id = e.relay_id direct,
                display_name(n.kind::text, n.label, t.name, n.number) node_name, n.lifecycle::text node_lifecycle, n.egress_dynamic,
                coalesce(rm.label, '') relay_name, host(rm.primary_ip) relay_ip
         from entry e join machine n on n.id = e.node_id join machine rm on rm.id = e.relay_id left join team t on t.id = n.team_id
         where e.id = $1",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    let e = e.ok_or_else(|| ApiError::not_found("没有这个入口"))?;
    // 发起时先锁节点那一行：和换线路机、退役、替换、另一个穿透开关互斥（它们都先锁节点行，先提交的作业后一方看得到）
    if lock {
        sqlx::query("select 1 from machine where id = $1 for update").bind(e.node_id).execute(&mut *conn).await?;
    }
    let (mut blockers, mut impacts, mut warnings) = (vec![], vec![], vec![]);
    let to = if on { REVERSE } else { NORMAL };
    if e.direct {
        blockers.push("线路机直出入口没有落地节点，用不着穿透".into());
    }
    if e.lifecycle != "active" {
        blockers.push(format!("入口现在是「{}」：只有在用的入口能切换（停用的先启用）", crate::entries::state_zh(&e.lifecycle)));
    }
    if e.node_lifecycle != "active" {
        blockers.push(format!("节点「{}」不在用", e.node_name));
    }
    if e.mode == to {
        blockers.push(format!("入口已经是{}模式", mode_zh(to)));
    }
    if on && !e.direct {
        if let Some(why) = reverse::unsupported(&mut *conn, e.node_id, e.relay_id).await? {
            blockers.push(why);
        }
        if !st.cfg.dist_dir.join("xray").exists() {
            blockers.push("中控 dist 里没有 xray（/opt/fleet/dist/xray），装不了 bridge".into());
        }
    }
    // 节点上还有动入口的作业：换线路机、加入口、退役、替换、另一个穿透开关、上架
    let busy: Option<(String,)> = sqlx::query_as(
        "select kind from job where state in ('queued', 'running', 'waiting', 'cancelling') and target_id = $1
           and (kind in ('attach_entry', 'move_entry', 'retire', 'replace', 'reverse') or starts_with(kind, 'provision_')) limit 1",
    )
    .bind(e.node_id)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some((k,)) = busy {
        blockers.push(format!("「{}」还有进行中的作业（{}）：等它结束再切换", e.node_name, crate::meta::job_kind_label(&k)));
    }
    // 这台节点上别的穿透入口：bridge 加 / 减隧道的影响
    let others: i64 = sqlx::query_scalar(
        "select count(*) from entry where node_id = $1 and id <> $2 and mode = 'reverse' and lifecycle in ('pending', 'active', 'disabled')",
    )
    .bind(e.node_id)
    .bind(e.id)
    .fetch_one(&mut *conn)
    .await?;
    if on {
        impacts.push(format!("节点「{}」上装 bridge（推 xray 程序、装 fleet-xray-bridge 单元），由它主动连 {}:{}（这个入口自己的端口），客户流量经这条隧道从节点自己的 IP 出去", e.node_name, e.relay_ip, e.port));
        if others > 0 {
            impacts.push(format!("节点上的 bridge 要重启：经它的另外 {others} 个穿透入口的连接断开几秒、客户端自动重连"));
        }
    } else {
        impacts.push(format!("线路机 {} 改回连节点的 ssserver（节点上一直留着，防火墙照旧放行这台线路机），节点入站丢包的问题会回来", e.relay_name));
        impacts.push(if others > 0 { format!("bridge 只去掉这条隧道、不重启（节点上还有 {others} 个穿透入口）") } else { "验收通过后节点上的 bridge 停掉删掉（xray 程序留着）".into() });
    }
    impacts.push(format!("线路机 {} 不重启 Xray：只换这个入口的入站，这个入口上正在进行的连接断开一次、客户端自动重连；线路机上别的入口不受影响", e.relay_name));
    impacts.push("分享链接、客户端订阅、软路由出口都不变（同一个入口端口与凭据）".into());
    if e.egress_dynamic {
        impacts.push("验收：线路机应用后经入口的出口探测能出网即可（出口动态）；线路机只留 AnyTLS 时放行节点最近上报的出站 IP，IP 变了自动跟着改；不通就自动切回原来的模式、作业失败".into());
    } else {
        impacts.push("验收：线路机应用后经入口的出口探测要看到节点自己的 IP；不通就自动切回原来的模式、作业失败".into());
    }
    if on {
        warnings.push("之后这个入口的 TCP 延迟改由节点量（节点 → 线路机，只画曲线、不告警；节点 agent 版本旧时没有这项数据）；隧道断了体现为出口探测失败（照常告警），隧道挂不上时中控会叫节点 agent 重启 bridge".into());
    }
    Ok(Plan { e, on, blockers, impacts, warnings })
}

impl Plan {
    fn view(&self) -> Value {
        let e = &self.e;
        json!({
            "entry": {"id": e.id, "name": e.name(), "node_id": e.node_id, "node": e.node_name, "relay_id": e.relay_id, "relay": e.relay_name, "port": e.port, "mode": e.mode},
            "on": self.on,
            "from": e.mode,
            "to": if self.on { REVERSE } else { NORMAL },
            "ok": self.blockers.is_empty(),
            "blockers": self.blockers,
            "impacts": self.impacts,
            "warnings": self.warnings,
        })
    }
}

#[derive(Deserialize)]
pub struct PreviewQ {
    on: bool,
}

#[derive(Deserialize)]
pub struct ReverseIn {
    on: bool,
}

/// 预览：会发生什么、不能切换的原因
pub async fn preview(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiQuery(q): ApiQuery<PreviewQ>) -> ApiResult<Json<Value>> {
    let mut conn = st.db.acquire().await?;
    Ok(ok(plan(&st, &mut conn, id, q.on, false).await?.view()))
}

/// 发起：建 reverse 作业（目标是节点）
pub async fn reverse(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<ReverseIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let p = plan(&st, &mut tx, id, b.on, true).await?;
    if !p.blockers.is_empty() {
        return Err(ApiError::conflict(p.blockers.join("；")).with_detail(json!({"blockers": p.blockers})));
    }
    let e = &p.e;
    let params = json!({"entry_id": e.id, "on": p.on, "from": e.mode, "relay_id": e.relay_id, "name": e.name()});
    let job = jobs::enqueue(&mut tx, "reverse", Some(e.node_id), params, &a.name).await?;
    audit(
        &mut tx,
        &a.name,
        "entry.reverse",
        "entry",
        Some(e.id),
        json!({"relay": e.relay_name, "port": e.port, "entry": e.node_name, "on": p.on, "job_id": job}),
    )
    .await?;
    tx.commit().await?;
    st.wake_jobs();
    st.emit(Event::Job { id: job, target: Some(e.node_id) });
    let mut v = p.view();
    v["job_id"] = json!(job);
    Ok(ok(v))
}

// ── 新入口默认模式 ──────────────────────────────────────────────────────────

async fn mode_view(st: &AppState) -> ApiResult<Value> {
    let (m, set) = reverse::default_mode(&st.db).await?;
    Ok(json!({"default": m, "label": mode_zh(m), "set": set, "builtin": reverse::DEFAULT_MODE}))
}

/// `{default, label, set, builtin}`：`set` = 面板里设过（没设过时按 `builtin`，即穿透）
pub async fn get_entry_mode(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> { Ok(ok(mode_view(&st).await?)) }

#[derive(Deserialize)]
pub struct EntryModeIn {
    default: String,
}

pub async fn put_entry_mode(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<EntryModeIn>) -> ApiResult<Json<Value>> {
    let m = reverse::parse_mode(&b.default).map_err(ApiError::bad_request)?;
    let old = reverse::save_default(&st, &a.name, m).await?;
    if old != m {
        st.emit(Event::Settings);
    }
    Ok(ok(mode_view(&st).await?))
}
