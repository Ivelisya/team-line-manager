use crate::error::ApiJson;
use super::ok;
use crate::auth::Actor;
use crate::db::audit;
use crate::entries;
use crate::error::{ApiError, ApiResult};
use crate::jobs;
use crate::state::{AppState, Event};
use crate::views::{self, Scope};
use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

pub async fn list(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let v: Vec<_> = views::load(&st.db, Scope::Live, None).await?.into_iter().filter(|m| m.kind == "relay").collect();
    Ok(ok(v))
}

#[derive(Deserialize)]
pub struct ActionIn {
    action: String,
    confirm: Option<String>,
}

pub async fn entry_action(a: Actor, State(st): State<AppState>, Path(id): Path<Uuid>, ApiJson(b): ApiJson<ActionIn>) -> ApiResult<Json<Value>> {
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
                return Err(ApiError::bad_request(format!("轮换凭据会让旧链接立即失效；请输入「{want}」确认")));
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
    Ok(ok(json!({"generation": gen})))
}

pub async fn share(a: Actor, State(st): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
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
    }
    let r: Row = sqlx::query_as(
        "select e.client_uuid, e.short_id, e.port, r.public_host, r.reality_sni, r.reality_public_key,
                n.kind::text kind, t.name team, n.number, n.label
         from entry e join relay r on r.machine_id = e.relay_id join machine n on n.id = e.node_id
         left join team t on t.id = n.team_id where e.id = $1 and e.lifecycle <> 'removed'",
    )
    .bind(id)
    .fetch_optional(&st.db)
    .await?
    .ok_or_else(|| ApiError::not_found("没有这个入口"))?;
    let remark = crate::names::display_name(&r.kind, r.label.as_deref(), r.team.as_deref(), r.number);
    let url = entries::share_url(r.client_uuid, &r.public_host, r.port, &r.reality_sni, &r.reality_public_key, &r.short_id, &remark);
    let mut tx = st.db.begin().await?;
    audit(&mut tx, &a.name, "entry.share", "entry", Some(id), json!({})).await?;
    tx.commit().await?;
    Ok(ok(json!({"url": url})))
}

#[derive(Deserialize)]
pub struct AddEntryIn {
    relay_id: Uuid,
}

/// 给已在用的节点再挂一条入口（作业：建入口 → 等线路机应用 → 等出口探测通过）
pub async fn add_entry(a: Actor, State(st): State<AppState>, Path(node_id): Path<Uuid>, ApiJson(b): ApiJson<AddEntryIn>) -> ApiResult<Json<Value>> {
    let (kind, lifecycle): (String, String) = sqlx::query_as("select kind::text, lifecycle::text from machine where id = $1")
        .bind(node_id)
        .fetch_optional(&st.db)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这台机器"))?;
    if kind == "relay" || lifecycle != "active" {
        return Err(ApiError::conflict("只能给在用的节点加入口"));
    }
    let mut tx = st.db.begin().await?;
    let job = jobs::enqueue(&mut tx, "attach_entry", Some(node_id), json!({"relay_id": b.relay_id}), &a.name).await?;
    audit(&mut tx, &a.name, "entry.add", "machine", Some(node_id), json!({"relay_id": b.relay_id, "job_id": job})).await?;
    tx.commit().await?;
    st.emit(Event::Job { id: job });
    Ok(ok(json!({"job_id": job})))
}
