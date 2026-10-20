use super::{ok, paged};
use crate::auth::Actor;
use crate::db::audit;
use crate::error::{ApiError, ApiResult};
use crate::jobs::{self, cancellable, cancellable_at, title_of};
use crate::state::{AppState, Event};
use axum::extract::{Path, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Serialize, sqlx::FromRow)]
pub struct JobOut {
    id: Uuid,
    kind: String,
    #[serde(skip)]
    target_id: Option<Uuid>,
    #[serde(skip)]
    target_name: Option<String>,
    state: String,
    step: i32,
    error: Option<String>,
    created_by: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    started_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    finished_at: Option<OffsetDateTime>,
    step_count: i64,
}

const SELECT: &str = "select j.id, j.kind, j.target_id,
    case when m.kind = 'relay' then m.label when t.name is not null then t.name || '-' || lpad(m.number::text, 2, '0') end target_name,
    j.state::text state, j.step, j.error, j.created_by, j.created_at, j.started_at, j.finished_at,
    (select count(*) from job_step s where s.job_id = j.id) step_count
  from job j left join machine m on m.id = j.target_id left join team t on t.id = m.team_id";

#[derive(Deserialize)]
pub struct ListQ {
    state: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
    target_id: Option<Uuid>,
}

/// 作业 JSON：目标机器以 `target{id,name}` 给出
fn job_json(j: &JobOut) -> Value {
    let mut v = serde_json::to_value(j).unwrap_or_default();
    v["target"] = match j.target_id {
        Some(id) => json!({"id": id, "name": j.target_name.clone().unwrap_or_else(|| "（已删除）".into())}),
        None => Value::Null,
    };
    v
}

pub async fn list(_a: Actor, State(st): State<AppState>, Query(q): Query<ListQ>) -> ApiResult<Json<Value>> {
    let filter = match q.state.as_deref() {
        Some("active") => "j.state in ('queued','running','waiting','cancelling')",
        Some("done") => "j.state in ('succeeded','failed','cancelled')",
        Some("failed") => "j.state = 'failed'",
        _ => "true",
    };
    let (limit, offset) = (q.limit.unwrap_or(100).clamp(1, 500), q.offset.unwrap_or(0).max(0));
    let sql = format!("{SELECT} where {filter} and ($1::uuid is null or j.target_id = $1) order by j.id desc limit $2 offset $3");
    let v: Vec<JobOut> = sqlx::query_as(&sql).bind(q.target_id).bind(limit).bind(offset).fetch_all(&st.db).await?;
    let total: i64 = sqlx::query_scalar(&format!("select count(*) from job j where {filter} and ($1::uuid is null or j.target_id = $1)"))
        .bind(q.target_id)
        .fetch_one(&st.db)
        .await?;
    Ok(paged(v.iter().map(job_json).collect(), total, offset))
}

#[derive(Serialize, sqlx::FromRow)]
struct StepOut {
    idx: i32,
    key: String,
    state: String,
    #[serde(with = "time::serde::rfc3339::option")]
    started_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    finished_at: Option<OffsetDateTime>,
    output: Option<Value>,
    error: Option<String>,
    /// 失败类别：network / auth / host_key / remote / timeout / internal
    error_kind: Option<String>,
}

pub async fn get_one(_a: Actor, State(st): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let j: JobOut = sqlx::query_as(&format!("{SELECT} where j.id = $1")).bind(id).fetch_optional(&st.db).await?.ok_or_else(|| ApiError::not_found("没有这个作业"))?;
    let steps: Vec<StepOut> = sqlx::query_as("select idx, key, state, started_at, finished_at, output, error, error_kind from job_step where job_id = $1 order by idx")
        .bind(id)
        .fetch_all(&st.db)
        .await?;
    let steps: Vec<Value> = steps
        .into_iter()
        .map(|s| {
            let mut v = serde_json::to_value(&s).unwrap_or_default();
            v["title"] = json!(title_of(&j.kind, &s.key));
            v
        })
        .collect();
    let mut v = job_json(&j);
    v["steps"] = json!(steps);
    // 换线路机：把新入口 id 摆在明处（前端据此提供「复制新分享链接」），并提示分享链接已变
    if j.kind == "move_entry" {
        let p: Option<Value> = sqlx::query_scalar("select params from job where id = $1").bind(id).fetch_optional(&st.db).await?;
        if let Some(p) = p {
            v["result"] = json!({
                "new_entry_id": p["entry_id"], "old_entry_id": p["old_entry_id"], "from_relay_id": p["from_relay_id"], "to_relay_id": p["to_relay_id"],
                "share_changed": true, "note": "客户端需要换成新的分享链接",
            });
        }
    }
    v["cancellable"] = json!(cancellable_at(&j.kind, j.step) && matches!(j.state.as_str(), "queued" | "running" | "waiting" | "failed"));
    Ok(ok(v))
}

#[derive(Deserialize)]
pub struct LogQ {
    after: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
struct LogOut {
    id: i64,
    #[serde(with = "time::serde::rfc3339")]
    ts: OffsetDateTime,
    level: String,
    msg: String,
}

pub async fn logs(_a: Actor, State(st): State<AppState>, Path(id): Path<Uuid>, Query(q): Query<LogQ>) -> ApiResult<Json<Value>> {
    let v: Vec<LogOut> = sqlx::query_as("select id, ts, level, msg from job_log where job_id = $1 and id > $2 order by id limit 1000")
        .bind(id)
        .bind(q.after.unwrap_or(0))
        .fetch_all(&st.db)
        .await?;
    Ok(ok(v))
}

#[derive(Deserialize, Default)]
pub struct RetryQ {
    /// 预检报「主机指纹变化」后，确认机器重装过，重试时覆盖已记录的指纹
    reset_host_key: Option<bool>,
}

pub async fn retry(a: Actor, State(st): State<AppState>, Path(id): Path<Uuid>, Query(q): Query<RetryQ>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let (state, step, kind, target): (String, i32, String, Option<Uuid>) =
        sqlx::query_as("select state::text, step, kind, target_id from job where id = $1 for update")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| ApiError::not_found("没有这个作业"))?;
    if state != "failed" {
        return Err(ApiError::conflict("只有失败的作业能重试"));
    }
    // 机器状态已经变了（比如上架一半的机器后来退役了）：旧作业不能复活
    let lc: Option<String> = match target {
        Some(t) => sqlx::query_scalar("select lifecycle::text from machine where id = $1").bind(t).fetch_optional(&mut *tx).await?,
        None => None,
    };
    jobs::check_target(&kind, lc.as_deref()).map_err(ApiError::conflict)?;
    sqlx::query("update job_step set state = 'pending', started_at = null, finished_at = null, error = null, error_kind = null where job_id = $1 and idx = $2")
        .bind(id)
        .bind(step)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "update job set state = 'queued', error = null, finished_at = null, run_after = now(), lease_until = null, lease_owner = null,
            params = case when $2 then params || '{\"reset_host_key\": true}'::jsonb else params end
         where id = $1",
    )
    .bind(id)
    .bind(q.reset_host_key == Some(true))
    .execute(&mut *tx)
    .await?;
    audit(&mut tx, &a.name, "job.retry", "job", Some(id), json!({"step": step, "reset_host_key": q.reset_host_key == Some(true)})).await?;
    tx.commit().await?;
    st.emit(Event::Job { id });
    Ok(ok(json!({"ok": true})))
}

pub async fn cancel(a: Actor, State(st): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let (state, kind, step): (String, String, i32) = sqlx::query_as("select state::text, kind, step from job where id = $1 for update")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这个作业"))?;
    if !cancellable(&kind) {
        return Err(ApiError::conflict("这类作业不能取消（可以重试到完成）"));
    }
    if !cancellable_at(&kind, step) {
        return Err(ApiError::conflict("旧入口已经开始摘除，不能再取消（取消会让节点没有入口）：失败了点重试到完成"));
    }
    if !matches!(state.as_str(), "queued" | "running" | "waiting" | "failed") {
        return Err(ApiError::conflict("作业已结束"));
    }
    sqlx::query("update job set state = 'cancelling', run_after = now(), lease_until = case when state = 'running' then lease_until else null end where id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, &a.name, "job.cancel", "job", Some(id), json!({})).await?;
    tx.commit().await?;
    st.emit(Event::Job { id });
    Ok(ok(json!({"ok": true})))
}
