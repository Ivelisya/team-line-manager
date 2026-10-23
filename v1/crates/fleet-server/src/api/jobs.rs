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
    /// 目标机器退役（进备用、回收）前的名字：还在备用的取 spare_from，回收了的取回收站里那条的
    #[serde(skip)]
    target_origin: Option<String>,
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
    /// 参数里的 routers_stuck（改软路由出口改回失败的路由）：算 `manual`
    #[serde(skip)]
    routers_stuck: Option<Value>,
}

const SELECT: &str = "select j.id, j.kind, j.target_id,
    coalesce(display_name(m.kind::text, m.label, t.name, m.number), ro.name) target_name,
    coalesce(m.spare_from, (select rb.name from recycle_bin rb where rb.source = 'fleet' and rb.source_id = j.target_id::text)) target_origin,
    j.state::text state, j.step, j.error, j.created_by, j.created_at, j.started_at, j.finished_at, j.params->'routers_stuck' routers_stuck
  from job j left join machine m on m.id = j.target_id left join team t on t.id = m.team_id left join router ro on ro.id = j.target_id";

#[derive(Deserialize)]
pub struct ListQ {
    state: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
    target_id: Option<Uuid>,
    /// 作业类型：provision（各类上架），或 job.kind 本身（attach_entry、move_entry、retire、purge、upgrade_agent、tune…）
    kind: Option<String>,
}

/// 作业 JSON：目标机器以 `target{id,name,origin}` 给出（origin：退役前的名字，没退役过为 null）；`step_title` = 当前（或失败的）步骤标题，做完为 null；
/// `manual` = 要人工处理时的说明（jobs::manual），否则 null。列表不带 steps
fn job_json(j: &JobOut) -> Value {
    let mut v = serde_json::to_value(j).unwrap_or_default();
    // 步骤数 = 静态步骤定义的个数（作业建立时按定义建步骤行），不逐行子查询
    let defs = jobs::defs(&j.kind);
    v["step_count"] = json!(defs.len());
    v["step_title"] = json!(defs.get(j.step as usize).map(|d| d.title));
    v["target"] = match j.target_id {
        Some(id) => json!({"id": id, "name": j.target_name.clone().unwrap_or_else(|| "（已删除）".into()), "origin": j.target_origin}),
        None => Value::Null,
    };
    v["manual"] = jobs::manual(&j.kind, j.step, &j.state, j.routers_stuck.as_ref().unwrap_or(&Value::Null));
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
    // 上架有四类机器，按大类筛；其余按 job.kind 原值
    let kind = match q.kind.as_deref().map(str::trim) {
        None | Some("" | "all") => None,
        Some("provision") => Some(("provision\\_%".to_string(), true)),
        Some(k) if !jobs::defs(k).is_empty() => Some((k.to_string(), false)),
        Some(_) => return Err(ApiError::bad_request("kind 只能是 provision 或作业类型（attach_entry、move_entry、retire、purge、upgrade_agent、tune、provision_vps…）")),
    };
    // 按机器、类型筛选的条件只在给了时才拼上（列表与总数各拼一份）
    let cond = |a: &mut Args| {
        let mut w = filter.to_string();
        if let Some(t) = q.target_id {
            w += &format!(" and j.target_id = {}", a.add(t));
        }
        match &kind {
            Some((p, true)) => w += &format!(" and j.kind like {}", a.add(p)),
            Some((k, false)) => w += &format!(" and j.kind = {}", a.add(k)),
            None => {}
        }
        w
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
    let p: Value = sqlx::query_scalar("select params from job where id = $1").bind(id).fetch_optional(&st.db).await?.unwrap_or_default();
    // 换线路机：把新入口 id 摆在明处（前端据此提供「复制新分享链接」），并提示分享链接已变
    if j.kind == "move_entry" {
        v["result"] = json!({
            "new_entry_id": p["entry_id"], "old_entry_id": p["old_entry_id"], "from_relay_id": p["from_relay_id"], "to_relay_id": p["to_relay_id"],
            "share_changed": true, "note": super::lifecycle::SHARE_NOTE,
        });
    }
    v["cancellable"] = json!(cancel_blocker(&j.kind, j.step, &j.state, &p).is_none());
    Ok(ok(v))
}

#[derive(Deserialize)]
pub struct LogQ {
    after: Option<i64>,
    /// 只要（after 之后的）最后 N 行：网页打开日志很长的作业时不用从头一页页拉完再丢掉前面的
    tail: Option<i64>,
}

/// tail 最多这么多行（网页内存里最多留 5000 行）
const TAIL_MAX: i64 = 5000;

#[derive(Serialize, sqlx::FromRow)]
struct LogOut {
    id: i64,
    #[serde(with = "time::serde::rfc3339")]
    ts: OffsetDateTime,
    level: String,
    msg: String,
}

/// 按 id 升序。不带 tail：after 之后的最多 1000 行（满页接着按最后一行的 id 拉）。
/// 带 tail：`{logs, skipped, after}`——最后 N 行（N 夹到 1..=5000）、前面没给的行数、续拉用的 after（最后一行的 id）
pub async fn logs(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiQuery(q): ApiQuery<LogQ>) -> ApiResult<Json<Value>> {
    let after = q.after.unwrap_or(0);
    let Some(tail) = q.tail else {
        let v: Vec<LogOut> = sqlx::query_as("select id, ts, level, msg from job_log where job_id = $1 and id > $2 order by id limit 1000")
            .bind(id)
            .bind(after)
            .fetch_all(&st.db)
            .await?;
        return Ok(ok(v));
    };
    let v: Vec<LogOut> = sqlx::query_as(
        "select * from (select id, ts, level, msg from job_log where job_id = $1 and id > $2 order by id desc limit $3) x order by id",
    )
    .bind(id)
    .bind(after)
    .bind(tail.clamp(1, TAIL_MAX))
    .fetch_all(&st.db)
    .await?;
    let skipped: i64 = match v.first() {
        Some(f) => sqlx::query_scalar("select count(*) from job_log where job_id = $1 and id > $2 and id < $3").bind(id).bind(after).bind(f.id).fetch_one(&st.db).await?,
        None => 0,
    };
    let after = v.last().map_or(after, |l| l.id);
    Ok(ok(json!({"logs": v, "skipped": skipped, "after": after})))
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
/// 不限作业种类：中控确实看到了指纹不符，重置是操作者对已知变化（比如服务商重装了在用节点的系统）的确认
async fn reset_target(tx: &mut crate::db::Tx<'_>, job: Uuid, step: i32, target: Option<Uuid>) -> ApiResult<(String, u16)> {
    let Some(t) = target else { return Err(ApiError::bad_request("这个作业没有目标机器，没有主机指纹可重置")) };
    let ek: Option<String> = sqlx::query_scalar("select error_kind from job_step where job_id = $1 and idx = $2").bind(job).bind(step).fetch_optional(&mut **tx).await?.flatten();
    if ek.as_deref() != Some("host_key") {
        return Err(ApiError::conflict("这个作业不是因为主机指纹不符失败的，不需要重置主机指纹：去掉「重置主机指纹」再重试"));
    }
    let (ip, port, host): (String, Option<i32>, Option<Uuid>) = sqlx::query_as("select host(primary_ip), ssh_port, host_id from machine where id = $1")
        .bind(t)
        .fetch_one(&mut **tx)
        .await?;
    if host.is_some() {
        return Err(ApiError::conflict("同机 IP 节点登录的是宿主机，不能在这里重置宿主机的主机指纹"));
    }
    // 不是同机 IP 节点就有自己的 SSH 端口（库里约束保证）
    Ok((ip, port.unwrap_or(22) as u16))
}

/// 审计里写是哪个作业：作业类型与目标机器（同步软路由出口是路由）的显示名（摘要「换线路机 · 团队1-03」）；没有目标、目标已删除的不写 name
async fn job_ref(tx: &mut crate::db::Tx<'_>, kind: &str, target: Option<Uuid>) -> sqlx::Result<Value> {
    let name: Option<String> = match target {
        Some(t) => sqlx::query_scalar(
            "select coalesce((select display_name(m.kind::text, m.label, t.name, m.number) from machine m left join team t on t.id = m.team_id where m.id = $1),
                             (select name from router where id = $1))",
        )
            .bind(t)
            .fetch_optional(&mut **tx)
            .await?
            .flatten(),
        None => None,
    };
    Ok(json!({"kind": kind, "name": name}))
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
            "update machine set lifecycle = 'provisioning', lifecycle_reason = null, revision = revision + 1, updated_at = now() where id = $1",
        )
        .bind(target)
        .execute(&mut *tx)
        .await?;
        lc = Some("provisioning".into());
    }
    // 机器状态已经变了（比如上架一半的机器后来退役了）：旧作业不能复活
    jobs::check_target(&kind, lc.as_deref()).map_err(ApiError::conflict)?;
    // 重置主机指纹：只给正是因为指纹不符失败的作业；同机 IP 节点登录的是宿主机，不能借它重置宿主机的指纹
    let pin = if reset { Some(reset_target(&mut tx, id, step, target).await?) } else { None };
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
    let mut detail = job_ref(&mut tx, &kind, target).await?;
    detail["step"] = json!(step);
    detail["reset_host_key"] = json!(reset);
    detail["password"] = json!(password.is_some());
    audit(&mut tx, &a.name, "job.retry", "job", Some(id), detail).await?;
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
    let (state, kind, step, target, params): (String, String, i32, Option<Uuid>, Value) = sqlx::query_as("select state::text, kind, step, target_id, params from job where id = $1 for update")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这个作业"))?;
    if let Some(why) = cancel_blocker(&kind, step, &state, &params) {
        return Err(ApiError::conflict(why));
    }
    // 排队中的升级作业：领到就收尾，编排里等它的随后跟着取消（jobs::claim、halted）
    sqlx::query("update job set state = 'cancelling', run_after = now(), lease_until = case when state = 'running' then lease_until else null end where id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let detail = job_ref(&mut tx, &kind, target).await?;
    audit(&mut tx, &a.name, "job.cancel", "job", Some(id), detail).await?;
    tx.commit().await?;
    st.wake_jobs();
    st.emit(Event::Job { id, target });
    Ok(ok(json!({"ok": true})))
}
