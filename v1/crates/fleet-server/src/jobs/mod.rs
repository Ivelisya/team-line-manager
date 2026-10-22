//! 作业引擎：PG 是唯一状态。领取用 `FOR UPDATE SKIP LOCKED` + 租约；每步幂等，可从失败处重试。
//!
//! 步骤返回 Done / Skipped / Wait(秒)：Wait 把作业置为 waiting 并在到点后重新领取（等线路机生效、等首次上报…），
//! 等待超过该步的 `timeout_s` 即失败。
//!
//! 唤醒：建作业、重试、取消后调 `AppState::wake_jobs`；线路机回报应用结果、agent 首次上报时 `wake_for` 把等这些事的作业
//! 提前到现在再唤醒；推进中发现作业已被取消、放开租约等补偿时也唤醒。引擎空闲时等唤醒，兜底每 800 ms 重查
//! （等待中的作业到点、过期租约被接管）。
//!
//! 租约：领取时给一个短租约，每步开始前按「步骤超时 + 余量」续租；所有对 job / job_step 的状态写入都带
//! `lease_owner = 自己`，0 行受影响说明租约已被别人接管（或作业被取消、被退役清掉），本执行者立刻放弃推进。
//! 执行者名每次领取另起一个；本进程正在推进的作业不会被本进程再领（`Running`）。
//! 服务启动时把运行中的租约全部清空（单实例：此前的执行者一定已经不在了），作业随即被重新领取。
//!
//! 步骤里遇到库暂时不可用（`db::unavailable`：PG 重启时被断开的查询、连不上、等不到连接）不算步骤失败：放开租约，
//! 10 秒后从这一步重做（`retry_later`），库恢复后接着往下走。

pub mod scripts;
mod steps;

use crate::db::Tx;
use crate::error::ApiResult;
use crate::ssh::{kind_of, ErrKind, StepError};
use crate::state::{AppState, Event};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
// 同机 IP 节点：SSH 与 agent 都是宿主机的，不做预检引导、不装 agent。先在宿主机上核对 IP 与端口，再装它自己的 ssserver 实例；
// 收敛 ≈ 推 ssserver 200 + 脚本 120 + 重启 30
const PROVISION_IP: &[StepDef] = &[
    s("ip_check", "在宿主机上核对 IP", 240),
    s("converge", "节点收敛（SS 实例、防火墙）", 600),
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
// 换线路机：新入口先建好并验收，再摘旧入口，节点全程至少有一个可用入口。detach_old 之后不可回滚（见 cancel_blocker）。
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
// 替换节点（见 steps/replace.rs）：先把软路由上指向旧节点的出口换成新节点的入口（一台一台改，失败的连同已改好的都换回原配置），
// 再交换编号（旧节点去备用团队 / 空号，新节点接旧编号）。改路由一开始就不能取消（见 cancel_blocker）。
// 一台路由：改配置 + 重启 OpenClash + 等 API + 测延迟最多约 7 分钟（routers::replace::APPLY_TIMEOUT），回滚约 4 分钟；按两三台留足
const REPLACE: &[StepDef] = &[s("routers", "改软路由出口", 1800), s("renumber", "交换编号", 60)];
// 推送时旧程序留成 .prev；起不来、或 2 分钟内等不到新版本上报，就换回去并让作业失败（见 steps::agent / wait_report）。重试从推送重来
const UPGRADE_AGENT: &[StepDef] = &[s("agent", "推送并重启 agent", 600), s("wait_report", "等待新版本上报", 180)];

pub fn defs(kind: &str) -> &'static [StepDef] {
    match kind {
        "provision_vps" => PROVISION_VPS,
        "provision_proxy" => PROVISION_PROXY,
        "provision_relay" => PROVISION_RELAY,
        "provision_ip" => PROVISION_IP,
        "attach_entry" => ATTACH_ENTRY,
        "move_entry" => MOVE_ENTRY,
        "retire" => RETIRE,
        "purge" => PURGE,
        "upgrade_agent" => UPGRADE_AGENT,
        "replace" => REPLACE,
        _ => &[],
    }
}

pub fn title_of(kind: &str, key: &str) -> &'static str {
    defs(kind).iter().find(|d| d.key == key).map(|d| d.title).unwrap_or("")
}

/// 上架类作业：目标机器处于上架中，失败时机器标为上架失败
pub fn provisions(kind: &str) -> bool { kind.starts_with("provision_") }

pub fn cancellable(kind: &str) -> bool { provisions(kind) || matches!(kind, "attach_entry" | "move_entry" | "upgrade_agent" | "replace") }

/// 作业走到第 `step` 步、处于 `state` 时为什么不能取消；能取消返回 None。
/// 换线路机一旦开始摘旧入口就只能重试到完成（回滚会让节点没有入口）；升级 agent 只能在排队时取消（推送、重启到一半中断更危险）；
/// 替换节点只能在开始改路由之前取消：排队中，或改路由失败（已换回原配置）还停在第一步
pub fn cancel_blocker(kind: &str, step: i32, state: &str) -> Option<&'static str> {
    if !cancellable(kind) {
        return Some("这类作业不能取消（可以重试到完成）");
    }
    if kind == "move_entry" && step >= MOVE_POINT_OF_NO_RETURN {
        return Some("旧入口已经开始摘除，不能再取消（取消会让节点没有入口）：失败了点重试到完成");
    }
    if !matches!(state, "queued" | "running" | "waiting" | "failed") {
        return Some("作业已结束");
    }
    if kind == "replace" && (step > 0 || !matches!(state, "queued" | "failed")) {
        return Some("软路由已经开始改（或已经改好），不能再取消：失败了点重试到完成");
    }
    if kind != "upgrade_agent" || state == "queued" {
        return None;
    }
    Some(if state == "failed" {
        "失败的升级作业不用取消：它不占着机器，可以重试或重新发起升级"
    } else {
        "升级已经开始，不能取消：推送、重启到一半中断更危险，等它结束（新版本有问题会自动换回旧版本）"
    })
}

/// 外层超时：步骤自己的 timeout_s 再加 120 秒余量
fn outer_timeout(d: &StepDef) -> Duration { Duration::from_secs(d.timeout_s as u64 + 120) }

/// 这类作业要求目标机器处于什么状态。None = 不限。
fn required_lifecycle(kind: &str) -> Option<&'static str> {
    match kind {
        k if k.starts_with("provision_") => Some("provisioning"),
        "attach_entry" | "move_entry" | "upgrade_agent" | "replace" => Some("active"),
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
        Some(l) => Err(format!("目标机器现在是「{}」，这个作业需要机器处于「{}」，不能再继续（旧作业不会自动复活，请重新发起）", crate::meta::lifecycle_label(l), crate::meta::lifecycle_label(want))),
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
    // 步骤一条语句插完：idx 按定义顺序从 0 起
    let keys: Vec<&str> = defs(kind).iter().map(|d| d.key).collect();
    sqlx::query("insert into job_step (job_id, idx, key) select $1, (i - 1)::int, k from unnest($2::text[]) with ordinality u(k, i)")
        .bind(id)
        .bind(&keys)
        .execute(&mut **tx)
        .await?;
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

/// 存进作业的远端输出（步骤错误 → 作业错误、日志、机器的状态原因；步骤的警告、跳过原因）最多约 4 KiB：stderr 可能上百 KB。
/// 留开头（在做什么）和结尾（真正的错误多在最后），中间标「已截断」
const DETAIL_CAP: usize = 4096;
const DETAIL_HEAD: usize = 512;

pub fn clip(s: &str) -> std::borrow::Cow<'_, str> {
    if s.len() <= DETAIL_CAP {
        return s.into();
    }
    let mut h = DETAIL_HEAD;
    while !s.is_char_boundary(h) {
        h -= 1;
    }
    let mut t = s.len() - (DETAIL_CAP - DETAIL_HEAD);
    while !s.is_char_boundary(t) {
        t += 1;
    }
    format!("{}\n…（已截断）…\n{}", &s[..h], &s[t..]).into()
}

pub async fn log(st: &AppState, job: Uuid, level: &str, msg: impl AsRef<str>) {
    let m = msg.as_ref();
    tracing::info!(%job, level, "{m}");
    let _ = sqlx::query("insert into job_log (job_id, level, msg) values ($1, $2, $3)").bind(job).bind(level).bind(m).execute(&st.db).await;
}

/// 这台机器（线路机：还有经它的入口所属的节点）上在等待的作业提前到现在，并唤醒引擎。
/// 线路机回报应用结果、agent 首次上报之后调：等应用 / 等摘除 / 等首次上报的步骤不必等到兜底的重查时间。
/// 已经到点的作业（Wait 到期时引擎正好在睡）同样要唤醒，所以不看改了几行
pub async fn wake_for(st: &AppState, machine: Uuid) {
    if let Err(e) = sqlx::query(
        "update job set run_after = least(run_after, now()) where state = 'waiting'
           and (target_id = $1 or target_id in (select node_id from entry where relay_id = $1))",
    )
    .bind(machine)
    .execute(&st.db)
    .await
    {
        // 回报本身已经入库，不因此让 agent 重发；作业照样会在兜底重查时推进
        tracing::warn!(%machine, "提前等待中的作业失败：{e:#}");
    }
    st.wake_jobs();
}

/// 本进程正在推进的作业。领取时排除它们：租约按库里的墙钟算，墙钟前跳（开机校时、虚拟机暂停后恢复）、续租前库卡住，
/// 都可能让还在跑的作业租约过期而被本进程再领一次——同一步两个任务同时跑（SSH 副作用重复），还各自往下推进
#[derive(Default)]
pub struct Running {
    ids: Mutex<HashSet<Uuid>>,
    /// 中控要退出了：不再领取，推进中的做完当前这一步就放手（见 `drain`）
    stopping: AtomicBool,
}

impl Running {
    fn ids(&self) -> Vec<Uuid> { self.ids.lock().unwrap().iter().copied().collect() }
    fn stopping(&self) -> bool { self.stopping.load(Ordering::SeqCst) }
}

/// 本进程在推进这个作业；drop 时（推进结束，含 panic）移出 `Running`
struct Entered {
    st: AppState,
    id: Uuid,
}

impl Drop for Entered {
    fn drop(&mut self) { self.st.jobs_running.ids.lock().unwrap().remove(&self.id); }
}

/// 记下本进程开始推进 `id`：在返回值 drop 之前 `claim` 不会再领它
fn enter(st: &AppState, id: Uuid) -> Entered {
    st.jobs_running.ids.lock().unwrap().insert(id);
    Entered { st: st.clone(), id }
}

/// 进程启动时（main 占住端口之后、起后台任务之前）调一次：单实例，重启前留下的租约属于已经不存在的执行者，
/// 清掉让作业立刻被重新领取（每步幂等，从当前步重跑）。不放在 `run_forever` 里：引擎循环被重新拉起时不能清掉本进程还在推进的租约
pub async fn release_stale_leases(st: &AppState) {
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
}

/// 优雅退出：不再领取新作业；正在推进的做完当前这一步就放开租约（见 `advance`），重启后从下一步接着做。
/// 最多等 `max`，返回到时还在推进的作业数（它们这一步被打断，重启后从这一步重做）
pub async fn drain(st: &AppState, max: Duration) -> usize {
    st.jobs_running.stopping.store(true, Ordering::SeqCst);
    let until = tokio::time::Instant::now() + max;
    loop {
        let n = st.jobs_running.ids.lock().unwrap().len();
        if n == 0 || tokio::time::Instant::now() >= until {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// 后台循环：领取并推进作业
pub async fn run_forever(st: AppState) {
    let sem = Arc::new(Semaphore::new(st.cfg.job_workers.max(1)));
    loop {
        let permit = sem.clone().acquire_owned().await.expect("semaphore");
        if st.jobs_running.stopping() {
            // 要退出了：不再领取。不返回，免得被当成引擎意外退出
            return std::future::pending().await;
        }
        // 每次领取都用新的执行者名：租约过期被接管后，旧任务对作业的写入一律落空
        let owner = format!("{}-{}", std::process::id(), Uuid::new_v4().simple());
        match claim(&st, &owner).await {
            Ok(Some(job)) => {
                let entered = enter(&st, job.id);
                let st = st.clone();
                tokio::spawn(async move {
                    let (_p, _e) = (permit, entered);
                    let id = job.id;
                    if let Err(e) = advance(&st, job, &owner).await {
                        retry_later(&st, id, &owner, &e).await;
                    }
                });
            }
            Ok(None) => {
                drop(permit);
                // 唤醒在忙的时候到达也不丢（Notify 记一次许可），下一轮立刻再领
                tokio::select! {
                    _ = st.jobs_wake.notified() => {}
                    _ = tokio::time::sleep(Duration::from_millis(800)) => {}
                }
            }
            Err(e) => {
                drop(permit);
                tracing::error!("领取作业失败：{e:#}");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

/// 推进出错（步骤里库暂时不可用、推进本身读写库失败）：放开租约，10 秒后从这一步重做（每步幂等）。
/// 库还连不上就隔 5 秒再放：租约是按整步的超时续的，留在手里要等十几分钟才有人接手
async fn retry_later(st: &AppState, id: Uuid, owner: &str, e: &anyhow::Error) {
    log(st, id, "error", format!("引擎错误：{e:#}（10 秒后从这一步重做）")).await;
    while let Err(e) = sqlx::query(
        "update job set lease_until = null, lease_owner = null, run_after = now() + interval '10 seconds' where id = $1 and lease_owner = $2",
    )
    .bind(id)
    .bind(owner)
    .execute(&st.db)
    .await
    {
        tracing::warn!(%id, "放开作业租约失败，5 秒后再试：{e:#}");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// 领取一个可执行的作业，租约先给 3 分钟；每步开始前会按该步的超时续租。本进程正在推进的不领（见 `Running`）。
/// 升级 agent 排在其它作业之后：「全部升级」一次建一百多个，不能让之后的上架、换线路机、退役排在它们后面。
/// 同一事务建的作业 created_at 相同，再按 id（UUIDv7，按生成先后递增）：全部升级里先建的线路机先领。
/// 参数里有 `after`（「全部升级」的编排：要等的作业）的，那个作业结束之前领不到；它在推送之后失败或被取消，这个作业领到后不开始、直接取消（见 `halted`）。
/// 排队时被取消的不用等：领到就收尾（没开始过，`started_at` 留空）
pub async fn claim(st: &AppState, owner: &str) -> anyhow::Result<Option<JobRow>> {
    Ok(sqlx::query_as(
        "update job set lease_owner = $1, lease_until = now() + interval '3 minutes',
            state = case when state in ('queued', 'waiting') then 'running'::job_state else state end,
            started_at = case when state = 'cancelling' then started_at else coalesce(started_at, now()) end
         where id = (select id from job j where state in ('queued', 'running', 'waiting', 'cancelling') and run_after <= now()
                     and (lease_until is null or lease_until < now()) and id <> all($2)
                     and (state = 'cancelling'
                          or not exists (select 1 from job p where p.id = (j.params->>'after')::uuid and p.state in ('queued', 'running', 'waiting', 'cancelling')))
                     order by kind = 'upgrade_agent', created_at, id for update skip locked limit 1)
         returning id, kind, target_id, params, state::text state, step, created_by",
    )
    .bind(owner)
    .bind(st.jobs_running.ids())
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
    // 编排：开始前看该不该开始。同一次尝试里中控重启后接着做的不再看：它已经开始了
    if job.step == 0 {
        let fresh: Option<bool> = sqlx::query_scalar("select state = 'pending' from job_step where job_id = $1 and idx = 0").bind(job.id).fetch_optional(&st.db).await?;
        if fresh.unwrap_or(true) {
            if let Some(why) = halted(st, &job).await? {
                return halt(st, &job, owner, &why).await;
            }
        }
    }
    let defs = defs(&job.kind);
    while (job.step as usize) < defs.len() {
        let d = &defs[job.step as usize];
        if st.jobs_running.stopping() {
            // 中控要退出了：做完的步骤都已记下，放开租约，重启后从这一步接着做
            sqlx::query("update job set lease_until = null, lease_owner = null, run_after = now() where id = $1 and lease_owner = $2")
                .bind(job.id)
                .bind(owner)
                .execute(&st.db)
                .await?;
            log(st, job.id, "info", format!("中控正在重启：「{}」等重启后接着做", d.title)).await;
            return Ok(());
        }
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
                let state: Option<String> = sqlx::query_scalar(
                    "update job set state = case when state = 'cancelling' then state else 'waiting' end,
                        run_after = case when state = 'cancelling' then now() else now() + make_interval(secs => $2) end,
                        lease_until = null, lease_owner = null
                     where id = $1 and lease_owner = $3 returning state::text",
                )
                .bind(job.id)
                .bind(secs as f64)
                .bind(owner)
                .fetch_optional(&st.db)
                .await?;
                let Some(state) = state else { return lost(st, &job).await };
                if state == "cancelling" {
                    // 等待中被取消：放开租约后立刻去补偿
                    st.wake_jobs();
                }
                return Ok(());
            }
            Ok(o) => {
                let (state, out, msg) = match o {
                    Outcome::Done(v) => ("done", v, format!("完成：{}", d.title)),
                    Outcome::Skipped(why) => {
                        let why = clip(&why);
                        ("skipped", json!({"reason": why}), format!("跳过：{}（{why}）", d.title))
                    }
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
                    // 运行中被取消：放开租约，唤醒引擎重新领取去做补偿
                    sqlx::query("update job set lease_until = null, lease_owner = null, run_after = now() where id = $1 and lease_owner = $2")
                        .bind(job.id)
                        .bind(owner)
                        .execute(&st.db)
                        .await?;
                    st.wake_jobs();
                    return Ok(());
                }
            }
            // 库暂时不可用（PG 重启时被断开的查询、连不上）不是这一步的错：交给 retry_later，库恢复后从这一步重做
            Err(e) if crate::db::unavailable_in(&e) => return Err(e),
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
        Some("cancelling") => {
            log(st, job.id, "warn", "作业刚好做完时收到了取消，将执行补偿").await;
            st.wake_jobs();
        }
        Some(_) => log(st, job.id, "info", "作业完成").await,
    }
    notify(st, &job);
    st.emit(Event::Machines);
    Ok(())
}

/// dist 里 agent 程序的 sha256（十六进制）
pub async fn dist_agent_sha256(st: &AppState) -> std::io::Result<String> {
    let data = tokio::fs::read(st.cfg.dist_dir.join("fleet-agent")).await?;
    Ok(hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&data)))
}

/// 「全部升级」发起之后 dist 里的 agent 换了（中途重新部署过中控）：接着推，后面的机器装上的是金丝雀没验过的版本
pub fn rollout_changed(params: &Value) -> String {
    format!("中控上的 agent 在升级过程中换了版本（发起时是 {}），请重新发起全部升级", params["rollout"]["version"].as_str().unwrap_or("?"))
}

/// 「全部升级」的编排：这个作业不该开始时返回原因（之后一步都不做，见 `halt`）。
/// dist 里的 agent 已经不是发起时的那个（参数 `rollout`）也停下；推送时还会再核对真正推的内容（steps::agent）
async fn halted(st: &AppState, job: &JobRow) -> sqlx::Result<Option<String>> {
    if let Some(why) = after_stopped(st, job).await? {
        return Ok(Some(why));
    }
    let Some(want) = job.params["rollout"]["sha256"].as_str() else { return Ok(None) };
    Ok((dist_agent_sha256(st).await.ok().as_deref() != Some(want)).then(|| rollout_changed(&job.params)))
}

/// 要等的作业（参数 `after`）在推送之后失败了（新版本起不来、没上报、线路机检查没通过）或被取消，返回这个作业不再开始的原因。
/// 它在推送之前就失败的（连不上、推送失败…，机器上的程序没换过，与新版本无关）不算，照常开始。
/// 要等的那个也是这样停下的（`halted`），就沿用它的原因：一路都指向最先失败的那台
async fn after_stopped(st: &AppState, job: &JobRow) -> sqlx::Result<Option<String>> {
    let Some(after) = job.params["after"].as_str().and_then(|s| Uuid::try_parse(s).ok()) else { return Ok(None) };
    // pushed 由 agent 步骤记（每次尝试从 false 起，要用 dist 里的程序重启 agent 之前置 true）；没有记的按推送之后算
    let prev = sqlx::query_as::<_, (String, Option<String>, bool, bool, bool, Option<String>)>(
        "select p.state::text, p.error, p.params ? 'halted', p.params ? 'after', coalesce((p.params->>'pushed')::bool, true),
                display_name(m.kind::text, m.label, t.name, m.number)
         from job p left join machine m on m.id = p.target_id left join team t on t.id = m.team_id where p.id = $1",
    )
    .bind(after)
    .fetch_optional(&st.db)
    .await?;
    let Some((state, err, was_halted, chained, pushed, name)) = prev.filter(|p| p.0 != "succeeded") else { return Ok(None) };
    let name = name.unwrap_or_else(|| "（已删除）".into());
    Ok(match (state.as_str(), chained) {
        _ if was_halted => Some(err.unwrap_or_default()),
        ("failed", _) if !pushed => None,
        ("failed", false) => Some(format!("金丝雀「{name}」升级失败，其余机器不再升级")),
        ("failed", true) => Some(format!("「{name}」升级失败，后面的机器不再升级")),
        _ => Some(format!("「{name}」的升级没有完成（{}），后面的机器不再升级", err.as_deref().unwrap_or("已取消"))),
    })
}

/// 编排停下了（见 `halted`）：一步都不做，直接取消并写明原因；记上 `halted`，等它的作业沿用同一个原因
async fn halt(st: &AppState, job: &JobRow, owner: &str, why: &str) -> anyhow::Result<()> {
    let n = sqlx::query(
        "update job set state = 'cancelled', error = $3, started_at = null, finished_at = now(), lease_until = null, lease_owner = null,
            params = params || jsonb_build_object('halted', true)
         where id = $1 and lease_owner = $2",
    )
    .bind(job.id)
    .bind(owner)
    .bind(why)
    .execute(&st.db)
    .await?
    .rows_affected();
    if n == 0 {
        return lost(st, job).await;
    }
    log(st, job.id, "warn", format!("没有开始：{why}")).await;
    notify(st, job);
    Ok(())
}

/// 取消的收尾：做补偿，清掉保存的 root 密码，步骤标为已补偿，作业置为 cancelled
async fn finish_cancel(st: &AppState, job: &JobRow, owner: &str) -> anyhow::Result<()> {
    // 补偿可能要 ssh 清理机器，给足租约
    if !renew(st, job.id, owner, 900).await? {
        return lost(st, job).await;
    }
    steps::compensate(st, job, owner).await;
    // 置为已取消与标步骤已补偿同一事务：外面看到 cancelled 时步骤已经是补偿后的状态。
    // 先改作业行（带租约条件，顺带锁住它）：租约已不在自己手里时一步都不改，不去动别人正在推进的作业的步骤
    let mut tx = st.db.begin().await?;
    let n = sqlx::query(
        "update job set state = 'cancelled', finished_at = now(), lease_until = null, lease_owner = null, params = params - 'login'
         where id = $1 and lease_owner = $2 and state = 'cancelling'",
    )
    .bind(job.id)
    .bind(owner)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        return lost(st, job).await;
    }
    sqlx::query("update job_step set state = 'compensated' where job_id = $1 and state in ('done', 'running')").bind(job.id).execute(&mut *tx).await?;
    tx.commit().await?;
    log(st, job.id, "warn", "已取消").await;
    notify(st, job);
    // 补偿可能把机器退役 / 退回已退役
    st.emit(Event::Machines);
    Ok(())
}

/// 步骤失败。若作业同时被标为取消，保留 cancelling（只释放租约，让下一轮去补偿），不改成 failed。
async fn fail(st: &AppState, job: &JobRow, owner: &str, d: &StepDef, kind: ErrKind, msg: &str) -> anyhow::Result<()> {
    let msg = &*clip(msg);
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
    let err = format!("{}：{msg}", d.title);
    // 作业置失败与机器置「上架失败」同一事务：看到作业失败时机器状态已经变了
    let mut tx = st.db.begin().await?;
    let state: Option<String> = sqlx::query_scalar(
        "update job set state = case when state = 'cancelling' then state else 'failed' end, error = $2,
            finished_at = case when state = 'cancelling' then finished_at else now() end,
            run_after = case when state = 'cancelling' then now() else run_after end,
            lease_until = null, lease_owner = null
         where id = $1 and lease_owner = $3 returning state::text",
    )
    .bind(job.id)
    .bind(&err)
    .bind(owner)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(state) = state else { return lost(st, job).await };
    // 上架失败：机器标为「上架失败」（重试回到上架中，取消则补偿退役）
    let marked = state == "failed"
        && provisions(&job.kind)
        && sqlx::query(
            "update machine set lifecycle = 'failed', lifecycle_reason = $2, revision = revision + 1, updated_at = now()
             where id = $1 and lifecycle = 'provisioning'",
        )
        .bind(job.target_id)
        .bind(&err)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
    tx.commit().await?;
    log(st, job.id, "error", format!("失败：{err}")).await;
    if state == "cancelling" {
        log(st, job.id, "warn", "作业正在取消，将执行补偿").await;
        st.wake_jobs();
    }
    if marked {
        st.emit(Event::Machines);
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
        assert!(check_target("purge", Some("retired")).is_ok());
        assert!(check_target("purge", None).is_ok(), "机器已删：purge 的目标已达成");
        assert!(check_target("provision_vps", None).is_err());
        // 同机 IP 节点的上架：和别的上架类作业一样要求上架中，失败标上架失败、可以取消
        assert!(check_target("provision_ip", Some("provisioning")).is_ok() && check_target("provision_ip", Some("active")).is_err());
        assert!(provisions("provision_ip") && cancellable("provision_ip"));
        let keys = |k: &str| defs(k).iter().map(|d| d.key).collect::<Vec<_>>();
        assert_eq!(keys("provision_ip"), ["ip_check", "converge", "attach", "wait_applied", "verify", "activate"]);
        let zh = crate::meta::job_kind_label;
        assert_eq!((zh("provision_ip"), zh("provision_vps")), ("上架同机 IP 节点", "上架 VPS 节点"));
    }

    #[test]
    fn outer_timeout_covers_step_timeout() {
        for k in [
            "provision_vps", "provision_proxy", "provision_relay", "provision_ip", "attach_entry", "retire", "purge", "upgrade_agent", "move_entry", "replace",
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
    fn clip_keeps_head_and_tail() {
        assert_eq!(clip("短"), "短");
        let long = format!("推送失败：{}真正的错误", "噪".repeat(100_000));
        let c = clip(&long);
        assert!(c.len() < DETAIL_CAP + 64 && c.starts_with("推送失败：") && c.ends_with("真正的错误") && c.contains("（已截断）"), "{}", c.len());
    }

    #[test]
    fn cancel_window() {
        let can = |kind, step, state| cancel_blocker(kind, step, state).is_none();
        assert!(can("move_entry", 0, "running") && can("move_entry", 3, "failed"));
        assert!(!can("move_entry", 4, "running") && !can("move_entry", 6, "failed"), "摘旧入口之后不能取消");
        assert_eq!(defs("move_entry")[MOVE_POINT_OF_NO_RETURN as usize].key, "detach_old");
        assert!(can("provision_vps", 8, "failed") && can("provision_vps", 8, "waiting"));
        assert!(!can("retire", 0, "queued") && !can("provision_vps", 0, "succeeded"));
        // 升级 agent 只能在排队时取消
        assert!(can("upgrade_agent", 0, "queued"));
        assert!(["running", "waiting", "failed", "cancelling"].iter().all(|s| !can("upgrade_agent", 0, s)));
        // 替换节点：排队中、或改路由失败（已换回原配置）停在第一步时能取消；改路由在跑、已经走到交换编号都不能
        assert!(can("replace", 0, "queued") && can("replace", 0, "failed"));
        assert!(!can("replace", 0, "running") && !can("replace", 1, "failed") && !can("replace", 1, "queued"));
        assert_eq!(defs("replace").iter().map(|d| d.key).collect::<Vec<_>>(), ["routers", "renumber"]);
        assert!(check_target("replace", Some("active")).is_ok() && check_target("replace", Some("retiring")).is_err());
        // 路由一台最多 APPLY_TIMEOUT，失败时还要回滚：至少放得下两台改加两台回滚
        let d = &defs("replace")[0];
        assert!(d.timeout_s as u64 >= 2 * (crate::routers::replace::APPLY_TIMEOUT + crate::routers::replace::ROLLBACK_TIMEOUT).as_secs());
    }
}
