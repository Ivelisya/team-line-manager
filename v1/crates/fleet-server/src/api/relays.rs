use crate::error::ApiJson;
use super::ok;
use crate::auth::Actor;
use crate::db::audit;
use crate::entries;
use crate::error::{ApiError, ApiPath, ApiResult};
use crate::jobs;
use crate::state::{AppState, Event};
use crate::views;
use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

pub async fn list(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let all = views::live(&st).await?;
    let v: Vec<_> = all.iter().filter(|m| m.kind == "relay").collect();
    Ok(ok(v))
}

#[derive(Deserialize)]
pub struct ActionIn {
    action: String,
    confirm: Option<String>,
}

/// 启用 / 停用 / 轮换凭据。轮换要输入「线路机:端口」确认，没确认时的 400 里列出指向这个入口的软路由出口（detail.router_exits，同 GET /entries/{id}/routers）；
/// 停用、轮换的结果也带上 router_exits：这些出口之后就连不上了（替换节点、换线路机会自动改路由，轮换、停用不会），要到路由上处理
pub async fn entry_action(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<ActionIn>) -> ApiResult<Json<Value>> {
    let (relay_label, port): (String, i32) =
        sqlx::query_as("select m.label, e.port from entry e join machine m on m.id = e.relay_id where e.id = $1")
            .bind(id)
            .fetch_optional(&st.db)
            .await?
            .ok_or_else(|| ApiError::not_found("没有这个入口"))?;
    let (to, rotate) = match b.action.as_str() {
        "enable" => ("active", false),
        "disable" => ("disabled", false),
        "rotate" => {
            let want = format!("{relay_label}:{port}");
            if b.confirm.as_deref().map(str::trim) != Some(want.as_str()) {
                let rs = crate::routers::exits_on_entry(&st.db, id).await?;
                let also = if rs.is_empty() { String::new() } else { format!("。这些软路由出口指向它，轮换后会连不上、要到路由上换成新凭据：{}", crate::routers::exits_text(&rs)) };
                return Err(ApiError::bad_request(format!("轮换凭据会让旧链接立即失效；请输入「{want}」确认{also}")).with_detail(json!({"router_exits": rs})));
            }
            let cur: (String,) = sqlx::query_as("select lifecycle::text from entry where id = $1").bind(id).fetch_one(&st.db).await?;
            if cur.0 == "disabled" {
                return Err(ApiError::conflict("入口停用中，先启用再轮换"));
            }
            ("active", true)
        }
        _ => return Err(ApiError::bad_request("action 只能是 enable / disable / rotate")),
    };
    let mut tx = st.db.begin().await?;
    let (relay_id, gen) = entries::transition(&mut tx, id, to, rotate).await?;
    audit(&mut tx, &a.name, &format!("entry.{}", b.action), "entry", Some(id), json!({"relay": relay_label, "port": port, "generation": gen})).await?;
    tx.commit().await?;
    st.relay_bumped(relay_id);
    st.emit(Event::Machines);
    let rs = if to == "active" && !rotate { vec![] } else { crate::routers::exits_on_entry(&st.db, id).await? };
    Ok(ok(json!({"generation": gen, "router_exits": rs})))
}

/// 指向这个入口的软路由出口（按各路由最近一次采集的快照，出口对上的入口就是它）：`[{id, name, state, exits: [出口名]}]`。
/// 轮换凭据、停用入口的确认里列出来
pub async fn entry_routers(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let n: Option<i32> = sqlx::query_scalar("select 1 from entry where id = $1").bind(id).fetch_optional(&st.db).await?;
    n.ok_or_else(|| ApiError::not_found("没有这个入口"))?;
    Ok(ok(crate::routers::exits_on_entry(&st.db, id).await?))
}

pub async fn share(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    a.require_scope("secrets")?;
    #[derive(sqlx::FromRow)]
    struct Row {
        client_uuid: Uuid,
        short_id: String,
        port: i32,
        public_host: String,
        reality_sni: String,
        reality_public_key: String,
        kind: String,
        team: Option<String>,
        number: Option<i32>,
        label: Option<String>,
        direct: bool,
        relay_label: Option<String>,
    }
    let r: Row = sqlx::query_as(
        "select e.client_uuid, e.short_id, e.port, r.public_host, r.reality_sni, r.reality_public_key,
                n.kind::text kind, t.name team, n.number, n.label, e.node_id = e.relay_id direct, rm.label relay_label
         from entry e join relay r on r.machine_id = e.relay_id join machine rm on rm.id = e.relay_id join machine n on n.id = e.node_id
         left join team t on t.id = n.team_id where e.id = $1 and e.lifecycle <> 'removed'",
    )
    .bind(id)
    .fetch_optional(&st.db)
    .await?
    .ok_or_else(|| ApiError::not_found("没有这个入口"))?;
    let remark = crate::names::entry_name(r.direct, &r.kind, r.label.as_deref(), r.team.as_deref(), r.number);
    let url = entries::share_url(r.client_uuid, &r.public_host, r.port, &r.reality_sni, &r.reality_public_key, &r.short_id, &remark);
    let mut tx = st.db.begin().await?;
    // 记是哪条入口（同启用 / 停用的写法加入口名），不记链接本身
    audit(&mut tx, &a.name, "entry.share", "entry", Some(id), json!({"relay": r.relay_label, "port": r.port, "entry": remark})).await?;
    tx.commit().await?;
    Ok(ok(json!({"url": url})))
}

#[derive(Deserialize)]
pub struct AddEntryIn {
    relay_id: Uuid,
}

/// 给已在用的节点再挂一条入口（作业：建入口 → 等线路机应用 → 等出口探测通过）。
/// 线路机给自己加（`relay_id` 就是它自己）= 直出入口：从线路机自己的 IP 出去，每台线路机至多一条
pub async fn add_entry(a: Actor, State(st): State<AppState>, ApiPath(node_id): ApiPath<Uuid>, ApiJson(b): ApiJson<AddEntryIn>) -> ApiResult<Json<Value>> {
    let (kind, lifecycle): (String, String) = sqlx::query_as("select kind::text, lifecycle::text from machine where id = $1")
        .bind(node_id)
        .fetch_optional(&st.db)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这台机器"))?;
    let direct = kind == "relay";
    if direct && b.relay_id != node_id {
        return Err(ApiError::conflict("线路机上只能加它自己的直出入口（relay_id 填这台线路机自己）"));
    }
    if lifecycle != "active" {
        return Err(ApiError::conflict(if direct { "只能给在用的线路机加直出入口" } else { "只能给在用的节点加入口" }));
    }
    // 入队前先拦下必败的请求，免得留一个失败作业
    let relay: Option<(String, Option<String>)> = sqlx::query_as("select lifecycle::text, label from machine where id = $1 and kind = 'relay'")
        .bind(b.relay_id)
        .fetch_optional(&st.db)
        .await?;
    match relay {
        // 请求体里引用的对象不存在是输入错误
        None => return Err(ApiError::bad_request("relay_id 指向的线路机不存在")),
        Some((lc, label)) if lc != "active" => {
            return Err(ApiError::conflict(format!("线路机 {} 不在用，不能在上面加入口", label.unwrap_or_default())));
        }
        Some(_) => {}
    }
    let dup: Option<(i32,)> = sqlx::query_as("select 1 from entry where node_id = $1 and relay_id = $2 and lifecycle <> 'removed'")
        .bind(node_id)
        .bind(b.relay_id)
        .fetch_optional(&st.db)
        .await?;
    if dup.is_some() {
        return Err(ApiError::conflict(if direct { "这台线路机已经有直出入口" } else { "这个节点在这台线路机上已经有入口" }));
    }
    let mut tx = st.db.begin().await?;
    let job = jobs::enqueue(&mut tx, "attach_entry", Some(node_id), json!({"relay_id": b.relay_id}), &a.name).await?;
    audit(&mut tx, &a.name, "entry.add", "machine", Some(node_id), json!({"relay_id": b.relay_id, "job_id": job, "direct": direct})).await?;
    tx.commit().await?;
    st.wake_jobs();
    st.emit(Event::Job { id: job, target: Some(node_id) });
    Ok(ok(json!({"job_id": job})))
}
