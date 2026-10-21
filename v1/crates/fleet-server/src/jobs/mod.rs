//! 作业引擎：PG 是唯一状态。领取用 `FOR UPDATE SKIP LOCKED` + 租约；每步幂等，可从失败处重试。
//!
//! 步骤返回 Done / Skipped / Wait(秒)：Wait 把作业置为 waiting 并在到点后重新领取（等线路机生效、等首次上报…），
//! 等待超过该步的 `timeout_s` 即失败。
//!
//! 租约：领取时给一个短租约，每步开始前按「步骤超时 + 余量」续租；所有对 job / job_step 的状态写入都带
//! `lease_owner = 自己`，0 行受影响说明租约已被别人接管（或作业被取消、被退役清掉），本执行者立刻放弃推进。
//! 服务启动时把运行中的租约全部清空（单实例：此前的执行者一定已经不在了），作业随即被重新领取。

pub mod scripts;
mod steps;

use crate::db::Tx;
use crate::error::ApiResult;
use crate::ssh::{kind_of, ErrKind, StepError};
use crate::state::{AppState, Event};
use serde_json::{json, Value};
use sqlx::PgPool;
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

// 超时要覆盖步骤内所有 ssh/推送的累计时间（含瞬时错误重试），否则会被外层超时截断：
// converge ≈ 基线 600 + 推 ssserver 200 + 脚本 120 + 重启 30；converge_relay ≈ 基线 600 + 推 xray 320 + 推 geo 2×320 + 脚本 60
const PROVISION_VPS: &[StepDef] = &[
    s("preflight", "预检与身份", 240),
    s("bootstrap", "引导密钥", 240),
    s("converge", "节点收敛（SSH 加固、SS、防火墙）", 1500),
    s("agent", "安装 agent", 600),
    s("wait_report", "等待首次上报", 180),
    s("attach", "接入线路机", 60),
    s("wait_applied", "等待线路机应用", 300),
    s("verify", "出口验收", 300),
    s("activate", "上线", 30),
];
const PROVISION_PROXY: &[StepDef] = &[
    s("attach", "接入线路机", 60),
    s("wait_applied", "等待线路机应用", 300),
    s("verify", "出口验收", 300),
    s("activate", "上线", 30),
];
const PROVISION_RELAY: &[StepDef] = &[
    s("preflight", "预检与身份", 240),
    s("bootstrap", "引导密钥", 240),
    s("converge_relay", "线路机收敛（SSH 加固、Xray）", 2400),
    s("agent", "安装 agent", 600),
    s("wait_report", "等待首次上报", 180),
    s("wait_applied", "等待 Xray 配置应用", 300),
    s("activate", "上线", 30),
];
const ATTACH_ENTRY: &[StepDef] = &[
    s("attach", "接入线路机", 60),
    s("firewall", "更新节点防火墙", 300),
    s("wait_applied", "等待线路机应用", 300),
    s("verify", "出口验收", 300),
];
const RETIRE: &[StepDef] = &[
    s("detach", "摘除入口", 60),
    s("wait_detached", "等待线路机摘除", 600),
    s("cleanup", "清理机器上的服务", 600),
    s("revoke", "撤销令牌", 30),
    s("finalize", "完成退役", 30),
];
// 换线路机：新入口先建好并验收，再摘旧入口，节点全程至少有一个可用入口。detach_old 之后不可回滚（见 cancellable_at）。
const MOVE_ENTRY: &[StepDef] = &[
    s("attach_new", "在新线路机建入口", 60),
    s("firewall_widen", "放行新线路机（节点防火墙）", 300),
    s("wait_applied_new", "等待新线路机应用", 300),
    s("verify_new", "经新入口出口验收", 300),
    s("detach_old", "摘除旧入口", 60),
    s("wait_detached_old", "等待旧线路机摘除", 600),
    s("firewall_narrow", "收紧节点防火墙", 300),
];
/// MOVE_ENTRY 里 detach_old 的下标：作业走到这一步（含）之后不能再取消
const MOVE_POINT_OF_NO_RETURN: i32 = 4;
const PURGE: &[StepDef] = &[s("purge", "删除数据", 600)];
const UPGRADE_AGENT: &[StepDef] = &[s("agent", "推送并重启 agent", 600), s("wait_report", "等待重新上报", 180)];

pub fn defs(kind: &str) -> &'static [StepDef] {
    match kind {
        "provision_vps" => PROVISION_VPS,
        "provision_proxy" => PROVISION_PROXY,
        "provision_relay" => PROVISION_RELAY,
        "attach_entry" => ATTACH_ENTRY,
        "move_entry" => MOVE_ENTRY,
        // 恢复已退役的机器：和上架走同一套收敛步骤
        "restore_vps" => PROVISION_VPS,
        "restore_proxy" => PROVISION_PROXY,
        "restore_relay" => PROVISION_RELAY,
        "retire" => RETIRE,
        "purge" => PURGE,
        "upgrade_agent" => UPGRADE_AGENT,
        _ => &[],
    }
}

pub fn title_of(kind: &str, key: &str) -> &'static str {
    defs(kind).iter().find(|d| d.key == key).map(|d| d.title).unwrap_or("")
}

pub fn cancellable(kind: &str) -> bool { kind.starts_with("provision_") || kind.starts_with("restore_") || kind == "attach_entry" || kind == "move_entry" }

/// 作业走到第 `step` 步时还能不能取消：换线路机一旦开始摘旧入口就只能重试到完成（回滚会让节点没有入口）
pub fn cancellable_at(kind: &str, step: i32) -> bool { cancellable(kind) && !(kind == "move_entry" && step >= MOVE_POINT_OF_NO_RETURN) }

/// 外层超时：步骤自己的 timeout_s 再加 120 秒余量
fn outer_timeout(d: &StepDef) -> Duration { Duration::from_secs(d.timeout_s as u64 + 120) }

/// 机器生命周期的中文（同网页 LIFECYCLE_LABEL）
pub fn lifecycle_zh(l: &str) -> &str {
    match l {
        "draft" => "草稿",
        "provisioning" => "上架中",
        "active" => "运行中",
        "retiring" => "退役中",
        "retired" => "已退役",
        "failed" => "上架失败",
        o => o,
    }
}

/// 作业类型的中文（同网页与 fleetctl）
pub fn kind_zh(k: &str) -> &str {
    match k {
        "provision_vps" => "上架落地机",
        "provision_proxy" => "上架家宽代理",
        "provision_relay" => "上架线路机",
        "attach_entry" => "新增入口",
        "move_entry" => "换线路机",
        "restore_vps" => "恢复落地机",
        "restore_proxy" => "恢复家宽代理",
        "restore_relay" => "恢复线路机",
        "retire" => "退役",
        "purge" => "彻底删除",
        "upgrade_agent" => "升级 agent",
        k => k,
    }
}

/// 这类作业要求目标机器处于什么状态。None = 不限。
fn required_lifecycle(kind: &str) -> Option<&'static str> {
    match kind {
        k if k.starts_with("provision_") || k.starts_with("restore_") => Some("provisioning"),
        "attach_entry" | "move_entry" | "upgrade_agent" => Some("active"),
        "retire" => Some("retiring"),
        "purge" => Some("retired"),
        _ => None,
    }
}

/// 机器当前状态是否还适合继续（或重试）这类作业。机器已不存在：只有 purge 算适合（目标已经达成）。
pub fn check_target(kind: &str, lifecycle: Option<&str>) -> Result<(), String> {
    let Some(want) = required_lifecycle(kind) else { return Ok(()) };
    match lifecycle {
        None if kind == "purge" => Ok(()),
        None => Err("目标机器已经不存在，这个作业不能再继续".into()),
        Some(l) if l == want => Ok(()),
        Some(l) => Err(format!("目标机器现在是「{}」，这个作业需要机器处于「{}」，不能再继续（旧作业不会自动复活，请重新发起）", lifecycle_zh(l), lifecycle_zh(want))),
    }
}

/// 读目标机器的状态；机器不存在返回 None
pub async fn target_lifecycle(db: &PgPool, target: Option<Uuid>) -> sqlx::Result<Option<String>> {
    let Some(t) = target else { return Ok(None) };
    sqlx::query_scalar("select lifecycle::text from machine where id = $1").bind(t).fetch_optional(db).await
}

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
    // 单实例：重启前留下的租约属于已经不存在的执行者，清掉让作业立刻被重新领取（每步幂等，从当前步重跑）
    match sqlx::query(
        "update job set lease_owner = null, lease_until = null, run_after = now() where state in ('running', 'cancelling') and lease_owner is not null",
    )
    .execute(&st.db)
    .await
    {
        Ok(r) if r.rows_affected() > 0 => tracing::warn!("启动时释放了 {} 个作业的旧租约", r.rows_affected()),
        Ok(_) => {}
        Err(e) => tracing::error!("启动时释放旧租约失败：{e:#}"),
    }
    let sem = Arc::new(Semaphore::new(st.cfg.job_workers.max(1)));
    loop {
        let permit = sem.clone().acquire_owned().await.expect("semaphore");
        match claim(&st, &owner).await {
            Ok(Some(job)) => {
                let st = st.clone();
                let owner = owner.clone();
                tokio::spawn(async move {
                    let _p = permit;
                    let id = job.id;
                    if let Err(e) = advance(&st, job, &owner).await {
                        log(&st, id, "error", format!("引擎错误：{e:#}")).await;
                        let _ = sqlx::query(
                            "update job set lease_until = null, lease_owner = null, run_after = now() + interval '10 seconds' where id = $1 and lease_owner = $2",
                        )
                        .bind(id)
                        .bind(&owner)
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

/// 领取一个可执行的作业，租约先给 3 分钟；每步开始前会按该步的超时续租。
pub async fn claim(st: &AppState, owner: &str) -> anyhow::Result<Option<JobRow>> {
    Ok(sqlx::query_as(
        "update job set lease_owner = $1, lease_until = now() + interval '3 minutes',
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

/// 续租。返回 false = 租约已不属于 `owner`（被别人接管、作业已取消/结束），调用方应放弃推进。
pub async fn renew(st: &AppState, job: Uuid, owner: &str, secs: u64) -> sqlx::Result<bool> {
    Ok(sqlx::query(
        "update job set lease_until = now() + make_interval(secs => $3)
         where id = $1 and lease_owner = $2 and state in ('running', 'cancelling')",
    )
    .bind(job)
    .bind(owner)
    .bind(secs as f64)
    .execute(&st.db)
    .await?
    .rows_affected()
        == 1)
}

fn notify(st: &AppState, job: &JobRow) {
    st.emit(Event::Job { id: job.id, target: job.target_id });
}

/// 租约已丢：放弃本次推进，不再写任何状态
async fn lost(st: &AppState, job: &JobRow) -> anyhow::Result<()> {
    log(st, job.id, "warn", "作业的租约已不属于本执行者（被接管或已取消），放弃本次推进").await;
    Ok(())
}

/// 对 job_step 的写入要求持有租约：把「job 行的 lease_owner = 我」写进同一条语句
const OWN: &str = "exists (select 1 from job where id = $1 and lease_owner = $3)";

/// 推进一个已领取的作业，直到它等待、失败、完成、被取消或丢失租约。`owner` 必须是领取时用的执行者名。
pub async fn advance(st: &AppState, mut job: JobRow, owner: &str) -> anyhow::Result<()> {
    if job.state == "cancelling" {
        return finish_cancel(st, &job, owner).await;
    }
    let defs = defs(&job.kind);
    while (job.step as usize) < defs.len() {
        let d = &defs[job.step as usize];
        // 旧作业不能在机器已经换了状态之后继续跑（例如退役后重试上架）
        let lc = target_lifecycle(&st.db, job.target_id).await?;
        if let Err(why) = check_target(&job.kind, lc.as_deref()) {
            return fail(st, &job, owner, d, ErrKind::Internal, &why).await;
        }
        let outer = outer_timeout(d);
        if !renew(st, job.id, owner, outer.as_secs() + 60).await? {
            return lost(st, &job).await;
        }
        let prev: Option<String> = sqlx::query_scalar("select state from job_step where job_id = $1 and idx = $2").bind(job.id).bind(job.step).fetch_optional(&st.db).await?;
        let started: Option<OffsetDateTime> = sqlx::query_scalar(&format!(
            "update job_step set state = 'running', started_at = coalesce(started_at, now()), error = null, error_kind = null
             where job_id = $1 and idx = $2 and {OWN} returning started_at"
        ))
        .bind(job.id)
        .bind(job.step)
        .bind(owner)
        .fetch_optional(&st.db)
        .await?;
        let Some(started) = started else { return lost(st, &job).await };
        // 步骤第一次运行（含「重试」把步骤重置之后的第一次）；等待后重新领取时为 false
        let first = prev.as_deref() == Some("pending");
        if first {
            log(st, job.id, "info", format!("开始：{}", d.title)).await;
            notify(st, &job);
        }
        let ctx = steps::Ctx { st, job: &job, owner, started, first };
        let res = match tokio::time::timeout(outer, steps::run(&ctx, d.key)).await {
            Ok(r) => r,
            Err(_) => Err(StepError::err(ErrKind::Timeout, format!("超过 {} 秒没有完成", outer.as_secs()))),
        };
        match res {
            Ok(Outcome::Wait(secs)) => {
                if (OffsetDateTime::now_utc() - started).whole_seconds() > d.timeout_s {
                    return fail(st, &job, owner, d, ErrKind::Timeout, &format!("等了 {} 秒仍未完成", d.timeout_s)).await;
                }
                let n = sqlx::query(
                    "update job set state = case when state = 'cancelling' then state else 'waiting' end,
                        run_after = case when state = 'cancelling' then now() else now() + make_interval(secs => $2) end,
                        lease_until = null, lease_owner = null
                     where id = $1 and lease_owner = $3",
                )
                .bind(job.id)
                .bind(secs as f64)
                .bind(owner)
                .execute(&st.db)
                .await?
                .rows_affected();
                if n == 0 {
                    return lost(st, &job).await;
                }
                return Ok(());
            }
            Ok(o) => {
                let (state, out, msg) = match o {
                    Outcome::Done(v) => ("done", v, format!("完成：{}", d.title)),
                    Outcome::Skipped(why) => ("skipped", json!({"reason": why}), format!("跳过：{}（{why}）", d.title)),
                    Outcome::Wait(_) => unreachable!(),
                };
                let n = sqlx::query(&format!("update job_step set state = $4, finished_at = now(), output = $5 where job_id = $1 and idx = $2 and {OWN}"))
                    .bind(job.id)
                    .bind(job.step)
                    .bind(owner)
                    .bind(state)
                    .bind(out)
                    .execute(&st.db)
                    .await?
                    .rows_affected();
                if n == 0 {
                    return lost(st, &job).await;
                }
                job.step += 1;
                // 步骤可能改了 params（例如记下 entry_id、丢弃引导密码），重新读一次
                let row: Option<(Value, i32, String)> =
                    sqlx::query_as("update job set step = $2 where id = $1 and lease_owner = $3 returning params, step, state::text")
                        .bind(job.id)
                        .bind(job.step)
                        .bind(owner)
                        .fetch_optional(&st.db)
                        .await?;
                let Some((params, step, state)) = row else { return lost(st, &job).await };
                job.params = params;
                job.step = step;
                log(st, job.id, "info", msg).await;
                notify(st, &job);
                if state == "cancelling" {
                    // 运行中被取消：放开租约，下一轮领取时做补偿
                    sqlx::query("update job set lease_until = null, lease_owner = null, run_after = now() where id = $1 and lease_owner = $2")
                        .bind(job.id)
                        .bind(owner)
                        .execute(&st.db)
                        .await?;
                    return Ok(());
                }
            }
            Err(e) => return fail(st, &job, owner, d, kind_of(&e), &format!("{e:#}")).await,
        }
    }
    // 最后一步做完时若作业已被标为取消，保留 cancelling，交给下一轮领取去补偿；否则成功并丢掉引导密码
    let state: Option<String> = sqlx::query_scalar(
        "update job set state = case when state = 'cancelling' then state else 'succeeded' end,
            finished_at = case when state = 'cancelling' then finished_at else now() end,
            run_after = case when state = 'cancelling' then now() else run_after end,
            params = case when state = 'cancelling' then params else params - 'login' end,
            lease_until = null, lease_owner = null, error = null
         where id = $1 and lease_owner = $2 returning state::text",
    )
    .bind(job.id)
    .bind(owner)
    .fetch_optional(&st.db)
    .await?;
    match state.as_deref() {
        None => return lost(st, &job).await,
        Some("cancelling") => log(st, job.id, "warn", "作业刚好做完时收到了取消，将执行补偿").await,
        Some(_) => log(st, job.id, "info", "作业完成").await,
    }
    notify(st, &job);
    st.emit(Event::Machines);
    Ok(())
}

/// 取消的收尾：做补偿，清掉保存的 root 密码，步骤标为已补偿，作业置为 cancelled
async fn finish_cancel(st: &AppState, job: &JobRow, owner: &str) -> anyhow::Result<()> {
    // 补偿可能要 ssh 清理机器，给足租约
    if !renew(st, job.id, owner, 900).await? {
        return lost(st, job).await;
    }
    steps::compensate(st, job, owner).await;
    let n = sqlx::query(
        "update job set state = 'cancelled', finished_at = now(), lease_until = null, lease_owner = null, params = params - 'login'
         where id = $1 and lease_owner = $2 and state = 'cancelling'",
    )
    .bind(job.id)
    .bind(owner)
    .execute(&st.db)
    .await?
    .rows_affected();
    if n == 0 {
        return lost(st, job).await;
    }
    if let Err(e) = sqlx::query("update job_step set state = 'compensated' where job_id = $1 and state in ('done', 'running')").bind(job.id).execute(&st.db).await {
        log(st, job.id, "warn", format!("标记步骤已补偿失败：{e}")).await;
    }
    log(st, job.id, "warn", "已取消").await;
    notify(st, job);
    // 补偿可能把机器退役 / 退回已退役
    st.emit(Event::Machines);
    Ok(())
}

/// 步骤失败。若作业同时被标为取消，保留 cancelling（只释放租约，让下一轮去补偿），不改成 failed。
async fn fail(st: &AppState, job: &JobRow, owner: &str, d: &StepDef, kind: ErrKind, msg: &str) -> anyhow::Result<()> {
    let n = sqlx::query(&format!(
        "update job_step set state = 'failed', finished_at = now(), error = $4, error_kind = $5 where job_id = $1 and idx = $2 and {OWN}"
    ))
    .bind(job.id)
    .bind(job.step)
    .bind(owner)
    .bind(msg)
    .bind(kind.as_str())
    .execute(&st.db)
    .await?
    .rows_affected();
    if n == 0 {
        return lost(st, job).await;
    }
    let state: Option<String> = sqlx::query_scalar(
        "update job set state = case when state = 'cancelling' then state else 'failed' end, error = $2,
            finished_at = case when state = 'cancelling' then finished_at else now() end,
            run_after = case when state = 'cancelling' then now() else run_after end,
            lease_until = null, lease_owner = null
         where id = $1 and lease_owner = $3 returning state::text",
    )
    .bind(job.id)
    .bind(format!("{}：{msg}", d.title))
    .bind(owner)
    .fetch_optional(&st.db)
    .await?;
    let Some(state) = state else { return lost(st, job).await };
    log(st, job.id, "error", format!("失败：{}：{msg}", d.title)).await;
    if state == "cancelling" {
        log(st, job.id, "warn", "作业正在取消，将执行补偿").await;
    }
    notify(st, job);
    Ok(())
}

/// 清理结果
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    /// 清掉了 root 密码的作业数
    pub passwords_cleared: u64,
    /// 删除的旧作业数（连同步骤与日志）
    pub jobs_deleted: u64,
}

/// 作业清理，由维护循环定期调用：
/// - 失败、取消、成功的作业超过 3 天，丢掉 params 里还留着的加密 root 密码（失败的作业重试可能还要用，所以不在失败时立刻清）；
/// - 成功/取消且结束超过 90 天、失败且结束超过 180 天的作业连同步骤、日志一起删除。
pub async fn prune(db: &PgPool) -> anyhow::Result<Pruned> {
    let passwords_cleared = sqlx::query(
        "update job set params = params - 'login'
         where jsonb_typeof(params->'login') = 'object' and state in ('failed', 'cancelled', 'succeeded')
           and finished_at < now() - interval '3 days'",
    )
    .execute(db)
    .await?
    .rows_affected();
    let jobs_deleted = sqlx::query(
        "delete from job where (state in ('succeeded', 'cancelled') and finished_at < now() - interval '90 days')
            or (state = 'failed' and finished_at < now() - interval '180 days')",
    )
    .execute(db)
    .await?
    .rows_affected();
    Ok(Pruned { passwords_cleared, jobs_deleted })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_state_rules() {
        assert!(check_target("provision_vps", Some("provisioning")).is_ok());
        assert!(check_target("provision_vps", Some("retired")).is_err());
        assert!(check_target("provision_proxy", Some("retiring")).is_err());
        assert!(check_target("attach_entry", Some("active")).is_ok());
        assert!(check_target("attach_entry", Some("retiring")).is_err());
        assert!(check_target("upgrade_agent", Some("active")).is_ok());
        assert!(check_target("retire", Some("retiring")).is_ok());
        assert!(check_target("retire", Some("retired")).is_err());
        assert!(check_target("move_entry", Some("active")).is_ok());
        assert!(check_target("move_entry", Some("retiring")).is_err());
        assert!(check_target("restore_vps", Some("provisioning")).is_ok());
        assert!(check_target("restore_vps", Some("retired")).is_err(), "恢复作业要求机器处于上架中");
        assert!(check_target("restore_proxy", Some("active")).is_err());
        assert!(check_target("purge", Some("retired")).is_ok());
        assert!(check_target("purge", None).is_ok(), "机器已删：purge 的目标已达成");
        assert!(check_target("provision_vps", None).is_err());
    }

    #[test]
    fn outer_timeout_covers_step_timeout() {
        for k in [
            "provision_vps", "provision_proxy", "provision_relay", "attach_entry", "retire", "purge", "upgrade_agent", "move_entry", "restore_vps",
            "restore_proxy", "restore_relay",
        ] {
            for d in defs(k) {
                assert!(outer_timeout(d).as_secs() as i64 >= d.timeout_s + 120);
            }
        }
        // 线路机收敛内层累计约 1620 秒，必须放得下
        let d = defs("provision_relay").iter().find(|d| d.key == "converge_relay").unwrap();
        assert!(outer_timeout(d).as_secs() >= 1620);
    }

    #[test]
    fn move_cancel_window() {
        assert!(cancellable_at("move_entry", 0) && cancellable_at("move_entry", 3));
        assert!(!cancellable_at("move_entry", 4) && !cancellable_at("move_entry", 6), "摘旧入口之后不能取消");
        assert_eq!(defs("move_entry")[MOVE_POINT_OF_NO_RETURN as usize].key, "detach_old");
        assert!(cancellable_at("restore_vps", 8) && cancellable_at("provision_vps", 8));
        assert!(!cancellable_at("retire", 0));
    }
}
