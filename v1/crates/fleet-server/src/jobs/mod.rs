//! 作业引擎：PG 是唯一状态。领取用 `FOR UPDATE SKIP LOCKED` + 租约；每步幂等，可从失败处重试。
//!
//! 步骤返回 Done / Skipped / Wait(秒)：Wait 把作业置为 waiting 并在到点后重新领取（等线路机生效、等首次上报…），
//! 等待超过该步的 `timeout_s` 即失败。

pub mod scripts;
mod steps;

use crate::db::Tx;
use crate::error::ApiResult;
use crate::state::{AppState, Event};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use tokio::sync::Semaphore;
use uuid::Uuid;

pub struct StepDef {
    pub key: &'static str,
    pub title: &'static str,
    pub timeout_s: i64,
}

const fn s(key: &'static str, title: &'static str, timeout_s: i64) -> StepDef { StepDef { key, title, timeout_s } }

const PROVISION_VPS: &[StepDef] = &[
    s("preflight", "预检与身份", 120),
    s("bootstrap", "引导密钥", 120),
    s("converge", "节点收敛（SSH 加固、SS、防火墙）", 900),
    s("agent", "安装 agent", 300),
    s("wait_report", "等待首次上报", 180),
    s("attach", "接入线路机", 60),
    s("wait_applied", "等待线路机生效", 300),
    s("verify", "出口验收", 300),
    s("activate", "上线", 30),
];
const PROVISION_PROXY: &[StepDef] = &[
    s("attach", "接入线路机", 60),
    s("wait_applied", "等待线路机生效", 300),
    s("verify", "出口验收", 300),
    s("activate", "上线", 30),
];
const PROVISION_RELAY: &[StepDef] = &[
    s("preflight", "预检与身份", 120),
    s("bootstrap", "引导密钥", 120),
    s("converge_relay", "线路机收敛（SSH 加固、Xray）", 900),
    s("agent", "安装 agent", 300),
    s("wait_report", "等待首次上报", 180),
    s("wait_applied", "等待 Xray 配置生效", 300),
    s("activate", "上线", 30),
];
const ATTACH_ENTRY: &[StepDef] = &[
    s("attach", "接入线路机", 60),
    s("firewall", "更新节点防火墙", 120),
    s("wait_applied", "等待线路机生效", 300),
    s("verify", "出口验收", 300),
];
const RETIRE: &[StepDef] = &[
    s("detach", "摘除入口", 60),
    s("wait_detached", "等待线路机摘除", 600),
    s("cleanup", "清理机器上的服务", 300),
    s("revoke", "撤销令牌", 30),
    s("finalize", "完成退役", 30),
];
const PURGE: &[StepDef] = &[s("purge", "删除数据", 600)];
const UPGRADE_AGENT: &[StepDef] = &[s("agent", "推送并重启 agent", 300), s("wait_report", "等待重新上报", 180)];

pub fn defs(kind: &str) -> &'static [StepDef] {
    match kind {
        "provision_vps" => PROVISION_VPS,
        "provision_proxy" => PROVISION_PROXY,
        "provision_relay" => PROVISION_RELAY,
        "attach_entry" => ATTACH_ENTRY,
        "retire" => RETIRE,
        "purge" => PURGE,
        "upgrade_agent" => UPGRADE_AGENT,
        _ => &[],
    }
}

pub fn title_of(kind: &str, key: &str) -> &'static str {
    defs(kind).iter().find(|d| d.key == key).map(|d| d.title).unwrap_or("")
}

pub fn cancellable(kind: &str) -> bool { kind.starts_with("provision_") || kind == "attach_entry" }

/// 建作业（调用方事务内）
pub async fn enqueue(tx: &mut Tx<'_>, kind: &str, target: Option<Uuid>, params: Value, actor: &str) -> ApiResult<Uuid> {
    let id = crate::db::new_id();
    sqlx::query("insert into job (id, kind, target_id, params, created_by) values ($1, $2, $3, $4, $5)")
        .bind(id)
        .bind(kind)
        .bind(target)
        .bind(params)
        .bind(actor)
        .execute(&mut **tx)
        .await?;
    for (i, d) in defs(kind).iter().enumerate() {
        sqlx::query("insert into job_step (job_id, idx, key) values ($1, $2, $3)")
            .bind(id)
            .bind(i as i32)
            .bind(d.key)
            .execute(&mut **tx)
            .await?;
    }
    Ok(id)
}

#[derive(Clone, sqlx::FromRow)]
pub struct JobRow {
    pub id: Uuid,
    pub kind: String,
    pub target_id: Option<Uuid>,
    pub params: Value,
    pub state: String,
    pub step: i32,
    pub created_by: String,
}

pub enum Outcome {
    Done(Value),
    Skipped(String),
    Wait(u64),
}

pub async fn log(st: &AppState, job: Uuid, level: &str, msg: impl AsRef<str>) {
    let m = msg.as_ref();
    tracing::info!(%job, level, "{m}");
    let _ = sqlx::query("insert into job_log (job_id, level, msg) values ($1, $2, $3)").bind(job).bind(level).bind(m).execute(&st.db).await;
}

/// 后台循环：领取并推进作业
pub async fn run_forever(st: AppState) {
    let owner = format!("{}-{}", std::process::id(), Uuid::new_v4().simple());
    let sem = Arc::new(Semaphore::new(st.cfg.job_workers.max(1)));
    loop {
        let permit = sem.clone().acquire_owned().await.expect("semaphore");
        match claim(&st, &owner).await {
            Ok(Some(job)) => {
                let st = st.clone();
                tokio::spawn(async move {
                    let _p = permit;
                    let id = job.id;
                    if let Err(e) = advance(&st, job).await {
                        log(&st, id, "error", format!("引擎错误：{e:#}")).await;
                        let _ = sqlx::query("update job set lease_until = null, run_after = now() + interval '10 seconds' where id = $1")
                            .bind(id)
                            .execute(&st.db)
                            .await;
                    }
                });
            }
            Ok(None) => {
                drop(permit);
                tokio::time::sleep(Duration::from_millis(800)).await;
            }
            Err(e) => {
                drop(permit);
                tracing::error!("领取作业失败：{e:#}");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

async fn claim(st: &AppState, owner: &str) -> anyhow::Result<Option<JobRow>> {
    Ok(sqlx::query_as(
        "update job set lease_owner = $1, lease_until = now() + interval '20 minutes',
            state = case when state in ('queued', 'waiting') then 'running'::job_state else state end,
            started_at = coalesce(started_at, now())
         where id = (select id from job where state in ('queued', 'running', 'waiting', 'cancelling') and run_after <= now()
                     and (lease_until is null or lease_until < now()) order by created_at for update skip locked limit 1)
         returning id, kind, target_id, params, state::text state, step, created_by",
    )
    .bind(owner)
    .fetch_optional(&st.db)
    .await?)
}

fn notify(st: &AppState, job: &JobRow) {
    st.emit(Event::Job { id: job.id });
    if let Some(t) = job.target_id {
        st.emit(Event::Machine { id: t });
    }
}

async fn advance(st: &AppState, mut job: JobRow) -> anyhow::Result<()> {
    if job.state == "cancelling" {
        steps::compensate(st, &job).await;
        sqlx::query("update job set state = 'cancelled', finished_at = now(), lease_until = null where id = $1").bind(job.id).execute(&st.db).await?;
        log(st, job.id, "warn", "已取消").await;
        notify(st, &job);
        return Ok(());
    }
    let defs = defs(&job.kind);
    while (job.step as usize) < defs.len() {
        let d = &defs[job.step as usize];
        let started: OffsetDateTime = sqlx::query_scalar(
            "update job_step set state = 'running', started_at = coalesce(started_at, now()), error = null
             where job_id = $1 and idx = $2 returning started_at",
        )
        .bind(job.id)
        .bind(job.step)
        .fetch_one(&st.db)
        .await?;
        let first = (OffsetDateTime::now_utc() - started).whole_seconds() < 2;
        if first {
            log(st, job.id, "info", format!("开始：{}", d.title)).await;
            notify(st, &job);
        }
        let ctx = steps::Ctx { st, job: &job, started };
        let res = tokio::time::timeout(Duration::from_secs(d.timeout_s as u64 + 30), steps::run(&ctx, d.key)).await;
        let res = match res {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!("超过 {} 秒没有完成", d.timeout_s)),
        };
        match res {
            Ok(Outcome::Wait(secs)) => {
                if (OffsetDateTime::now_utc() - started).whole_seconds() > d.timeout_s {
                    return fail(st, &job, d, &format!("等了 {} 秒仍未完成", d.timeout_s)).await;
                }
                sqlx::query(
                    "update job set state = case when state = 'cancelling' then state else 'waiting' end,
                        run_after = now() + make_interval(secs => $2), lease_until = null where id = $1",
                )
                    .bind(job.id)
                    .bind(secs as f64)
                    .execute(&st.db)
                    .await?;
                return Ok(());
            }
            Ok(o) => {
                let (state, out, msg) = match o {
                    Outcome::Done(v) => ("done", v, format!("完成：{}", d.title)),
                    Outcome::Skipped(why) => ("skipped", json!({"reason": why}), format!("跳过：{}（{why}）", d.title)),
                    Outcome::Wait(_) => unreachable!(),
                };
                sqlx::query("update job_step set state = $3, finished_at = now(), output = $4 where job_id = $1 and idx = $2")
                    .bind(job.id)
                    .bind(job.step)
                    .bind(state)
                    .bind(out)
                    .execute(&st.db)
                    .await?;
                job.step += 1;
                // 步骤可能改了 params（例如记下 entry_id、丢弃引导密码），重新读一次
                let (params, step, state): (Value, i32, String) = sqlx::query_as(
                    "update job set step = $2 where id = $1 returning params, step, state::text",
                )
                .bind(job.id)
                .bind(job.step)
                .fetch_one(&st.db)
                .await?;
                job.params = params;
                job.step = step;
                log(st, job.id, "info", msg).await;
                notify(st, &job);
                if state == "cancelling" {
                    // 运行中被取消：放开租约，下一轮领取时做补偿
                    sqlx::query("update job set lease_until = null, run_after = now() where id = $1").bind(job.id).execute(&st.db).await?;
                    return Ok(());
                }
            }
            Err(e) => return fail(st, &job, d, &format!("{e:#}")).await,
        }
    }
    sqlx::query("update job set state = 'succeeded', finished_at = now(), lease_until = null, error = null where id = $1")
        .bind(job.id)
        .execute(&st.db)
        .await?;
    log(st, job.id, "info", "作业完成").await;
    notify(st, &job);
    st.emit(Event::Machines);
    Ok(())
}

async fn fail(st: &AppState, job: &JobRow, d: &StepDef, msg: &str) -> anyhow::Result<()> {
    sqlx::query("update job_step set state = 'failed', finished_at = now(), error = $3 where job_id = $1 and idx = $2")
        .bind(job.id)
        .bind(job.step)
        .bind(msg)
        .execute(&st.db)
        .await?;
    sqlx::query("update job set state = 'failed', error = $2, finished_at = now(), lease_until = null where id = $1")
        .bind(job.id)
        .bind(format!("{}：{msg}", d.title))
        .execute(&st.db)
        .await?;
    log(st, job.id, "error", format!("失败：{}：{msg}", d.title)).await;
    notify(st, job);
    Ok(())
}
