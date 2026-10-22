use super::{ok, paged};
use crate::auth::Actor;
use crate::db::{audit, Args};
use crate::error::{ApiError, ApiPath, ApiQuery, ApiResult};
use crate::jobs::{self, cancel_blocker, title_of};
use crate::state::{AppState, Event};
use axum::extract::State;
use axum::Json;
use base64::Engine as _;
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
}

const SELECT: &str = "select j.id, j.kind, j.target_id,
    display_name(m.kind::text, m.label, t.name, m.number) target_name,
    j.state::text state, j.step, j.error, j.created_by, j.created_at, j.started_at, j.finished_at
  from job j left join machine m on m.id = j.target_id left join team t on t.id = m.team_id";

#[derive(Deserialize)]
pub struct ListQ {
    state: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
    target_id: Option<Uuid>,
}

/// 作业 JSON：目标机器以 `target{id,name}` 给出；`step_title` = 当前（或失败的）步骤标题，做完为 null。列表不带 steps
fn job_json(j: &JobOut) -> Value {
    let mut v = serde_json::to_value(j).unwrap_or_default();
    // 步骤数 = 静态步骤定义的个数（作业建立时按定义建步骤行），不逐行子查询
    let defs = jobs::defs(&j.kind);
    v["step_count"] = json!(defs.len());
    v["step_title"] = json!(defs.get(j.step as usize).map(|d| d.title));
    v["target"] = match j.target_id {
        Some(id) => json!({"id": id, "name": j.target_name.clone().unwrap_or_else(|| "（已删除）".into())}),
        None => Value::Null,
    };
    v
}

pub async fn list(_a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<ListQ>) -> ApiResult<Json<Value>> {
    let filter = match q.state.as_deref() {
        Some("active") => "j.state in ('queued','running','waiting','cancelling')",
        Some("done") => "j.state in ('succeeded','failed','cancelled')",
        Some("failed") => "j.state = 'failed'",
        _ => "true",
    };
    let (limit, offset) = (q.limit.unwrap_or(100).clamp(1, 500), q.offset.unwrap_or(0).max(0));
    // 按机器筛选的条件只在给了 target_id 时才拼上（列表与总数各拼一份）
    let cond = |a: &mut Args| match q.target_id {
        Some(t) => format!("{filter} and j.target_id = {}", a.add(t)),
        None => filter.to_string(),
    };
    let mut a = Args::default();
    let sql = format!("{SELECT} where {} order by j.id desc limit {} offset {}", cond(&mut a), a.add(limit), a.add(offset));
    let v: Vec<JobOut> = sqlx::query_as_with(&sql, a.0).fetch_all(&st.db).await?;
    let mut a = Args::default();
    let sql = format!("select count(*) from job j where {}", cond(&mut a));
    let total: i64 = sqlx::query_scalar_with(&sql, a.0).fetch_one(&st.db).await?;
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

pub async fn get_one(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
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
    v["cancellable"] = json!(cancel_blocker(&j.kind, j.step, &j.state).is_none());
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

pub async fn logs(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiQuery(q): ApiQuery<LogQ>) -> ApiResult<Json<Value>> {
    let v: Vec<LogOut> = sqlx::query_as("select id, ts, level, msg from job_log where job_id = $1 and id > $2 order by id limit 1000")
        .bind(id)
        .bind(q.after.unwrap_or(0))
        .fetch_all(&st.db)
        .await?;
    Ok(ok(v))
}

/// 重试的可选请求体（不带请求体 = 两项都不要）
#[derive(Deserialize, Default)]
pub struct RetryIn {
    /// 报「主机指纹变化」后，确认机器重装过：清掉已记录的指纹并按现在的重新记录
    reset_host_key: Option<bool>,
    /// root 密码（中控密钥登录不了时用）：和上架时一样加密存进作业参数，用过即丢
    ssh_password: Option<String>,
}

/// 这次重试能不能重置主机指纹，能就返回要重新记录的 (IP, SSH 端口)。在步骤被重置之前调（要读失败那一步的类别）
async fn reset_target(tx: &mut crate::db::Tx<'_>, job: Uuid, step: i32, kind: &str, target: Option<Uuid>) -> ApiResult<(String, u16)> {
    if !jobs::provisions(kind) {
        return Err(ApiError::conflict("只有上架、恢复作业能在重试时重置主机指纹：其它作业遇到指纹变化，先确认机器是否重装过，再退役后恢复"));
    }
    let Some(t) = target else { return Err(ApiError::bad_request("这个作业没有目标机器，没有主机指纹可重置")) };
    let ek: Option<String> = sqlx::query_scalar("select error_kind from job_step where job_id = $1 and idx = $2").bind(job).bind(step).fetch_optional(&mut **tx).await?.flatten();
    if ek.as_deref() != Some("host_key") {
        return Err(ApiError::conflict("这个作业不是因为主机指纹不符失败的，不需要重置主机指纹：去掉「重置主机指纹」再重试"));
    }
    let (ip, port, host): (String, i32, Option<Uuid>) = sqlx::query_as("select host(primary_ip), ssh_port, host_id from machine where id = $1")
        .bind(t)
        .fetch_one(&mut **tx)
        .await?;
    if host.is_some() {
        return Err(ApiError::conflict("同机 IP 节点登录的是宿主机，不能在这里重置宿主机的主机指纹"));
    }
    Ok((ip, port as u16))
}

pub async fn retry(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, body: axum::body::Bytes) -> ApiResult<Json<Value>> {
    let b: RetryIn = if body.iter().all(u8::is_ascii_whitespace) {
        RetryIn::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(format!("请求内容不对：{e}")))?
    };
    let reset = b.reset_host_key == Some(true);
    let password = b.ssh_password.filter(|p| !p.is_empty());
    let (target, state0): (Option<Uuid>, String) = sqlx::query_as("select target_id, state::text from job where id = $1")
        .bind(id)
        .fetch_optional(&st.db)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这个作业"))?;
    if state0 != "failed" {
        return Err(ApiError::conflict("只有失败的作业能重试"));
    }
    let mut tx = st.db.begin().await?;
    // 先锁机器行再锁作业行：和退役（锁机器 → 作废其作业）同一顺序，避免死锁
    let mut lc: Option<String> = match target {
        Some(t) => sqlx::query_scalar("select lifecycle::text from machine where id = $1 for update").bind(t).fetch_optional(&mut *tx).await?,
        None => None,
    };
    let (state, step, kind): (String, i32, String) = sqlx::query_as("select state::text, step, kind from job where id = $1 for update")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这个作业"))?;
    if state != "failed" {
        return Err(ApiError::conflict("只有失败的作业能重试"));
    }
    // 上架失败的机器重试：回到上架中
    let back = jobs::provisions(&kind) && lc.as_deref() == Some("failed");
    if back {
        sqlx::query(
            "update machine set lifecycle = 'provisioning', lifecycle_reason = case when $2 then '恢复中' end, revision = revision + 1, updated_at = now() where id = $1",
        )
        .bind(target)
        .bind(kind.starts_with("restore_"))
        .execute(&mut *tx)
        .await?;
        lc = Some("provisioning".into());
    }
    // 机器状态已经变了（比如上架一半的机器后来退役了）：旧作业不能复活
    jobs::check_target(&kind, lc.as_deref()).map_err(ApiError::conflict)?;
    // 重置主机指纹：只给上架（恢复）作业、且正是因为指纹不符失败的；同机 IP 节点登录的是宿主机，不能借它重置宿主机的指纹
    let pin = if reset { Some(reset_target(&mut tx, id, step, &kind, target).await?) } else { None };
    // 从失败那一步重来；升级 agent 从推送重来：失败时多半已经换回了旧程序，只重跑「等待新版本上报」永远等不到
    let from = if kind == "upgrade_agent" { 0 } else { step };
    sqlx::query(
        "update job_step set state = 'pending', started_at = null, finished_at = null, output = null, error = null, error_kind = null
         where job_id = $1 and idx >= $2",
    )
    .bind(id)
    .bind(from)
    .execute(&mut *tx)
    .await?;
    let login = password.as_deref().map(|pw| json!({"login": {"password_enc": base64::engine::general_purpose::STANDARD.encode(st.secrets.seal_str(pw))}}));
    sqlx::query(
        "update job set state = 'queued', step = $3, error = null, finished_at = null, run_after = now(), lease_until = null, lease_owner = null,
            params = params || coalesce($2, '{}'::jsonb)
         where id = $1",
    )
    .bind(id)
    .bind(login)
    .bind(from)
    .execute(&mut *tx)
    .await?;
    audit(&mut tx, &a.name, "job.retry", "job", Some(id), json!({"step": step, "reset_host_key": reset, "password": password.is_some()})).await?;
    // 所有检查（含「同一台机器只能有一个进行中的作业」）都过了才动 known_hosts。在事务里做：作业此刻还不会被领走，
    // 重置失败则整个重试作废；keyscan 最多十来秒，其间机器行与作业行一直锁着
    if let Some((ip, port)) = pin {
        crate::ssh::pin_host_key(&st.cfg.known_hosts, &ip, port, true)
            .await
            .map_err(|e| ApiError::conflict(format!("重置主机指纹失败：{e:#}")))?;
    }
    tx.commit().await?;
    st.wake_jobs();
    st.emit(Event::Job { id, target });
    if back {
        st.emit(Event::Machines);
    }
    Ok(ok(json!({"ok": true})))
}

pub async fn cancel(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let (state, kind, step, target): (String, String, i32, Option<Uuid>) = sqlx::query_as("select state::text, kind, step, target_id from job where id = $1 for update")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这个作业"))?;
    if let Some(why) = cancel_blocker(&kind, step, &state) {
        return Err(ApiError::conflict(why));
    }
    // 排队中的升级作业：领到就收尾，编排里等它的随后跟着取消（jobs::claim、halted）
    sqlx::query("update job set state = 'cancelling', run_after = now(), lease_until = case when state = 'running' then lease_until else null end where id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, &a.name, "job.cancel", "job", Some(id), json!({})).await?;
    tx.commit().await?;
    st.wake_jobs();
    st.emit(Event::Job { id, target });
    Ok(ok(json!({"ok": true})))
}
