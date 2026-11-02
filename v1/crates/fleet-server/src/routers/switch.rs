//! 临时切换出口：经隧道调路由上 mihomo 的 API（`PUT /proxies/<组>`，collect.uc 的 MODE=switch）把一个 select 组切到组里已有的另一个出口，
//! 不改 OpenClash 的配置、不重启（不断连接；只有经这个组的新连接走新出口）。用途：某节点被打、临时把客户切到备用出口，恢复后切回。
//!
//! - 只对 select（mihomo 类型 Selector）组：url-test / fallback / load-balance 由 mihomo 自己选，拒绝并说明。只能选组里已有的出口。
//! - 一组同时只有一条进行中的记录（router_switch，state = active）：from_exit 是第一次切之前选中的，再切一次只改 to_exit，
//!   切回（手动 `--back`、或到 back_at 自动）总是回到 from_exit；手动切到的正好是 from_exit 也算切回。
//! - 路由重启、OpenClash 重载（替换节点、改出口指向都会重启它）之后组选择由 OpenClash 自己恢复（配置默认值或它记住的选择）：
//!   采集时发现组现在选中的不是 to_exit（或组没了），记录结束（reverted / gone），待切回随之取消，记审计。只看切换生效之后才开始的采集。
//! - 没有规则用的组（快照里 used = false，见 collect.rs mark_used）切了也不改变流量：拒绝（409），要把客户挪走用改出口指向。旧快照没有这个字段（未知）照常能切。
//! - 到时自动切回之前先看原出口：快照里它不可用、或它指向的节点 / 线路机有进行中的严重机器告警（出口不通、TCP 不通、agent 离线），
//!   就不切回（免得把客户切回坏出口），back_at 往后推 1 小时（defer_why 记原因），第一次推迟时经通知渠道说一声，审计记一条。
//! - 全部记审计：router.switch（切换）、router.switch_back（切回，自动的操作者是「系统」）、router.switch_reverted（路由自己改回了）、router.switch_defer（推迟自动切回）。
//! - 切换、切回在中控上串行（一把锁）：每次一两秒，不值得按路由分锁。

use super::collect::{Group, Snapshot};
use super::tunnel;
use crate::db::{audit, new_id};
use crate::error::{ApiError, ApiResult};
use crate::notify::{self, Kind, Notice};
use crate::state::{AppState, Event};
use serde_json::{json, Value};
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

/// 自动切回最多这么久之后（更久的就该替换节点 / 改出口指向）
pub const MAX_BACK_S: i64 = 7 * 86400;
/// 原出口还没恢复时，自动切回推迟这么久再看
pub const DEFER_S: i64 = 3600;
/// 一次切换最多等这么久
const TIMEOUT: Duration = Duration::from_secs(25);
/// 自动切回的操作者
pub const AUTO: &str = crate::spare::ACTOR;

static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 进行中的一条
#[derive(sqlx::FromRow, Clone)]
pub struct Active {
    pub id: Uuid,
    pub router_id: Uuid,
    pub grp: String,
    pub from_exit: String,
    pub to_exit: String,
    pub back_at: Option<OffsetDateTime>,
    pub created_by: String,
    pub switched_at: OffsetDateTime,
    /// 到时没切回的原因（原出口还没恢复）；`back_at` 是下次检查的时刻
    pub defer_why: Option<String>,
    pub deferred_at: Option<OffsetDateTime>,
}

impl Active {
    pub fn view(&self) -> Value {
        json!({
            "group": self.grp, "from": self.from_exit, "to": self.to_exit,
            "back_at": self.back_at.map(crate::meta::fmt_ts), "by": self.created_by, "switched_at": crate::meta::fmt_ts(self.switched_at),
            "defer_why": self.defer_why, "deferred_at": self.deferred_at.map(crate::meta::fmt_ts),
        })
    }
}

const ACTIVE: &str = "select id, router_id, grp, from_exit, to_exit, back_at, created_by, switched_at, defer_why, deferred_at from router_switch where state = 'active'";

/// 进行中的临时切换（`router` 给了只要这台的；列表与详情里给出）
pub async fn active(db: &sqlx::PgPool, router: Option<Uuid>) -> sqlx::Result<Vec<Active>> {
    sqlx::query_as(&format!("{ACTIVE} and ($1::uuid is null or router_id = $1) order by grp")).bind(router).fetch_all(db).await
}

/// 按最近一次采集核对能不能切：组在、是 select、出口在组里
pub fn check<'a>(snap: &'a Snapshot, group: &str, to: &str) -> ApiResult<&'a Group> {
    let g = snap.groups.iter().find(|g| g.name == group).ok_or_else(|| ApiError::bad_request(format!("最近一次采集里没有策略组「{group}」")))?;
    if !g.selectable() {
        return Err(ApiError::bad_request(format!(
            "「{group}」是 {} 组：由 mihomo 按延迟 / 可用性自己选出口，不能手动切换；只有 select 组可以（要换它里面的出口，改配置或用 fleetctl repoint）",
            kind_cn(&g.kind)
        )));
    }
    if g.used == Some(false) {
        return Err(ApiError::conflict(format!(
            "没有规则用策略组「{group}」，切了不影响流量；要把客户挪走用改出口指向（fleetctl repoint）"
        )));
    }
    if !g.all.iter().any(|x| x == to) {
        return Err(ApiError::bad_request(format!("策略组「{group}」里没有「{to}」：只能切到组里已有的出口（{}）", g.all.join("、"))));
    }
    Ok(g)
}

fn kind_cn(k: &str) -> &str {
    match k {
        "URLTest" => "url-test（自动测速）",
        "Fallback" => "fallback（故障转移）",
        "LoadBalance" => "load-balance（负载均衡）",
        "Relay" => "relay（链式）",
        k => k,
    }
}

/// 在路由上切：返回切之前选中的。失败返回原因
async fn exec(st: &AppState, id: Uuid, group: &str, to: &str) -> Result<Option<String>, String> {
    let port: Option<i32> = sqlx::query_scalar("select tunnel_port from router where id = $1").bind(id).fetch_optional(&st.db).await.map_err(|e| e.to_string())?;
    let Some(port) = port else { return Err("这台软路由已经删掉了".into()) };
    let out = crate::ssh::exec(tunnel::via_tunnel(&st.cfg, id, port), &super::collect::switch_script(group, to), TIMEOUT).await.map_err(|e| format!("{e:#}"))?;
    if out.code == 255 {
        return Err(tunnel::explain(&out, true));
    }
    let sw = out.stdout.lines().rev().find_map(|l| serde_json::from_str::<Value>(l.trim()).ok()).map(|v| v["sw"].clone()).unwrap_or(Value::Null);
    if sw["ok"] == true {
        return Ok(sw["was"].as_str().map(String::from));
    }
    Err(sw["error"].as_str().map(String::from).unwrap_or_else(|| format!("切换脚本出错（退出码 {}）：{}", out.code, out.stderr.trim().chars().take(200).collect::<String>())))
}

/// 切换：`back_after_s` 给了就到时自动切回。返回 {group, from, to, back_at}
pub async fn switch(st: &AppState, actor: &str, id: Uuid, group: &str, to: &str, back_after_s: Option<i64>) -> ApiResult<Value> {
    if let Some(s) = back_after_s.filter(|s| !(60..=MAX_BACK_S).contains(s)) {
        return Err(ApiError::bad_request(format!("自动切回的时间要在 1 分钟到 7 天之间（给的是 {s} 秒）")));
    }
    let _g = LOCK.lock().await;
    let r = super::load(&st.db, Some(id)).await?.into_iter().next().ok_or_else(|| ApiError::not_found("没有这台软路由"))?;
    if r.state() != "online" {
        return Err(ApiError::conflict(format!("软路由「{}」现在不在线，切不了", r.name)));
    }
    let snap = r.snapshot().unwrap_or_default();
    let g = check(&snap, group, to)?;
    let cur = sqlx::query_as::<_, Active>(&format!("{ACTIVE} and router_id = $1 and grp = $2")).bind(id).bind(group).fetch_optional(&st.db).await?;
    if g.now.as_deref() == Some(to) && cur.as_ref().is_none_or(|c| c.to_exit == to) {
        return Err(ApiError::conflict(format!("策略组「{group}」现在选中的已经是「{to}」")));
    }
    let was = exec(st, id, group, to).await.map_err(|e| ApiError::conflict(format!("软路由「{}」：{e}", r.name)))?;
    let from = cur.as_ref().map(|c| c.from_exit.clone()).or(was.clone()).or(g.now.clone()).unwrap_or_default();
    let now = OffsetDateTime::now_utc();
    let back_at = back_after_s.map(|s| now + time::Duration::seconds(s));
    let mut tx = st.db.begin().await?;
    // 切到的正好是第一次切之前的：等于切回，记录结束
    let ended = cur.is_some() && to == from;
    match &cur {
        Some(c) if ended => {
            sqlx::query("update router_switch set state = 'back', ended_at = now(), end_note = '手动切回原来的出口' where id = $1").bind(c.id).execute(&mut *tx).await?;
        }
        Some(c) => {
            sqlx::query("update router_switch set to_exit = $2, back_at = $3, created_by = $4, switched_at = $5, defer_why = null, deferred_at = null where id = $1")
                .bind(c.id)
                .bind(to)
                .bind(back_at)
                .bind(actor)
                .bind(now)
                .execute(&mut *tx)
                .await?;
        }
        // 快照旧了、路由上本来就是它：没有要记的
        None if to == from => {}
        None => {
            sqlx::query("insert into router_switch (id, router_id, grp, from_exit, to_exit, back_at, created_by, switched_at) values ($1, $2, $3, $4, $5, $6, $7, $8)")
                .bind(new_id())
                .bind(id)
                .bind(group)
                .bind(&from)
                .bind(to)
                .bind(back_at)
                .bind(actor)
                .bind(now)
                .execute(&mut *tx)
                .await?;
        }
    }
    let was_now = was.unwrap_or_default();
    let detail = json!({"name": r.name, "group": group, "from": was_now, "to": to, "original": from, "back_at": back_at.map(crate::meta::fmt_ts)});
    audit(&mut tx, actor, if ended { "router.switch_back" } else { "router.switch" }, "router", Some(id), detail).await?;
    tx.commit().await?;
    st.routers_refresh.ask([id]);
    st.emit(Event::Routers);
    Ok(json!({"group": group, "from": was_now, "to": to, "original": from, "back_at": back_at.map(crate::meta::fmt_ts), "ended": ended}))
}

/// 手动切回第一次切之前的出口（`--back`）
pub async fn back(st: &AppState, actor: &str, id: Uuid, group: &str) -> ApiResult<Value> {
    let _g = LOCK.lock().await;
    let c = sqlx::query_as::<_, Active>(&format!("{ACTIVE} and router_id = $1 and grp = $2"))
        .bind(id)
        .bind(group)
        .fetch_optional(&st.db)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("策略组「{group}」没有进行中的临时切换（没切过，或已经切回 / 被路由自己改回）")))?;
    back_one(st, actor, &c, "手动切回").await
}

async fn back_one(st: &AppState, actor: &str, c: &Active, why: &str) -> ApiResult<Value> {
    let name: String = sqlx::query_scalar("select name from router where id = $1").bind(c.router_id).fetch_one(&st.db).await?;
    let was = exec(st, c.router_id, &c.grp, &c.from_exit).await.map_err(|e| ApiError::conflict(format!("软路由「{name}」：{e}")))?;
    let mut tx = st.db.begin().await?;
    let n = sqlx::query("update router_switch set state = 'back', ended_at = now(), end_note = $2 where id = $1 and state = 'active' and switched_at = $3")
        .bind(c.id)
        .bind(why)
        .bind(c.switched_at)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    let from = was.unwrap_or_else(|| c.to_exit.clone());
    if n > 0 {
        audit(&mut tx, actor, "router.switch_back", "router", Some(c.router_id), json!({"name": name, "group": c.grp, "from": from, "to": c.from_exit, "why": why})).await?;
    }
    tx.commit().await?;
    st.routers_refresh.ask([c.router_id]);
    st.emit(Event::Routers);
    Ok(json!({"group": c.grp, "from": from, "to": c.from_exit, "ended": true}))
}

/// 每 30 秒看一次到时间的切回去（单独一个任务，路由卡住时每条最多等 [`TIMEOUT`]，不拖慢采集循环）
pub async fn run_forever(st: AppState) {
    let every = Duration::from_secs(30);
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        due(&st).await;
    }
}

/// 到时间的切回去。切不了的（路由不在线、mihomo 不通）留着，下一轮再试，失败只记日志；
/// 原出口还没恢复的不切回，推迟 [`DEFER_S`] 再看（见 [`origin_blocker`]）
pub async fn due(st: &AppState) {
    let rows: Vec<Active> = match sqlx::query_as(&format!("{ACTIVE} and back_at <= now() order by back_at")).fetch_all(&st.db).await {
        Ok(r) => r,
        Err(e) => return tracing::warn!("查到时切回的临时切换失败：{e}"),
    };
    due_each(st, rows).await;
}

/// [`due`] 的逐条处理：每条拿到锁后按 id 重读（测试直接给一份「拿锁之前读的」旧记录来验这一步）
pub async fn due_each(st: &AppState, rows: Vec<Active>) {
    for c in rows {
        let _g = LOCK.lock().await;
        // 上面的记录是拿锁之前读的：等锁期间（前面每条切回最多 25 秒）人可能又切了一次或切回了，按 id 重读，不再到时的就跳过
        let c: Active = match sqlx::query_as(&format!("{ACTIVE} and id = $1 and back_at <= now()")).bind(c.id).fetch_optional(&st.db).await {
            Ok(Some(c)) => c,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(router = %c.router_id, group = %c.grp, "拿到锁后重读临时切换失败，下一轮再试：{e}");
                continue;
            }
        };
        match origin_blocker(st, &c).await {
            Ok(Some(why)) => {
                if let Err(e) = defer(st, &c, &why).await {
                    tracing::warn!(router = %c.router_id, group = %c.grp, "推迟自动切回没记成，下一轮再试：{}", e.message);
                }
                continue;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(router = %c.router_id, group = %c.grp, "查原出口恢复了没有时出错，下一轮再试：{}", e.message);
                continue;
            }
        }
        if let Err(e) = back_one(st, AUTO, &c, "到时自动切回").await {
            tracing::warn!(router = %c.router_id, group = %c.grp, "到时自动切回没成，下一轮再试：{}", e.message);
        }
    }
}

/// 原出口现在能不能把客户切回去：快照里它连续不通，或它指向的节点 / 线路机有进行中的严重机器告警（出口不通、TCP 不通、agent 离线）时，返回原因。
/// 路由不在线（切回本来就做不成）、原出口不是单个出口（嵌套的组、DIRECT）、快照里没有它时没法判断，不拦
async fn origin_blocker(st: &AppState, c: &Active) -> ApiResult<Option<String>> {
    let Some(r) = super::load(&st.db, Some(c.router_id)).await?.into_iter().next().filter(|r| r.state() == "online") else { return Ok(None) };
    let snap = r.snapshot().unwrap_or_default();
    let Some(x) = snap.exits.iter().find(|x| x.name == c.from_exit) else { return Ok(None) };
    if !super::collect::usable(x) {
        return Ok(Some(format!("原出口「{}」现在不通（连续 {} 次测延迟不通）", x.name, x.fail_streak)));
    }
    let Some(e) = &x.entry else { return Ok(None) };
    // 同机 IP 节点没有自己的 agent，agent 离线只由宿主机报：宿主机的 agent 离线也算，文案写宿主机名；宿主机的出口不通 / TCP 不通不算（IP 节点有自己的 IP 和探测，各报各的，见 health.rs）
    let hit: Option<(Uuid, String, Option<String>)> = sqlx::query_as(
        "select i.subject_id, i.rule, case when i.subject_id <> $1 and i.subject_id <> $2 then display_name(m.kind::text, m.label, t.name, m.number) end
         from incident i left join machine m on m.id = i.subject_id left join team t on t.id = m.team_id
         where i.state = 'firing' and i.engine = 'machine' and i.severity::text = 'critical' and i.rule in ('egress_fail', 'tcp_fail', 'agent_offline')
           and (i.subject_id in ($1, $2) or (i.rule = 'agent_offline' and i.subject_id = (select host_id from machine where id = $1))) order by i.opened_at limit 1",
    )
    .bind(e.node_id)
    .bind(e.relay_id)
    .fetch_optional(&st.db)
    .await?;
    Ok(hit.map(|(id, rule, host)| {
        let who = if id == e.node_id {
            &e.node_name
        } else if id == e.relay_id {
            &e.relay_name
        } else {
            host.as_ref().unwrap_or(&e.node_name)
        };
        format!("原出口「{}」指向的 {who} 有进行中的严重告警（{}）", x.name, crate::meta::rule_label(&rule))
    }))
}

/// 推迟自动切回：back_at 往后推 [`DEFER_S`]，记原因；第一次推迟时记审计、经通知渠道说一声（之后每小时复查，原因变了只更新记录）
async fn defer(st: &AppState, c: &Active, why: &str) -> ApiResult<()> {
    let name: String = sqlx::query_scalar("select name from router where id = $1").bind(c.router_id).fetch_one(&st.db).await?;
    let now = OffsetDateTime::now_utc();
    let next = now + time::Duration::seconds(DEFER_S);
    let mut tx = st.db.begin().await?;
    // 只推迟这次看到的这一条（期间又切了一次的，switched_at 变了，不动）
    let n = sqlx::query("update router_switch set back_at = $2, defer_why = $3, deferred_at = coalesce(deferred_at, $4) where id = $1 and state = 'active' and switched_at = $5")
        .bind(c.id)
        .bind(next)
        .bind(why)
        .bind(now)
        .bind(c.switched_at)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    let first = n > 0 && c.deferred_at.is_none();
    if first {
        let detail = json!({"name": name, "group": c.grp, "to": c.from_exit, "why": why, "next_check": crate::meta::fmt_ts(next)});
        audit(&mut tx, AUTO, "router.switch_defer", "router", Some(c.router_id), detail).await?;
    }
    tx.commit().await?;
    if n == 0 {
        return Ok(());
    }
    st.emit(Event::Routers);
    if first {
        let summary = format!(
            "策略组「{}」临时切到「{}」，到时要切回「{}」，但{why}：先不切回，{} 再检查，恢复后自动切回",
            c.grp,
            c.to_exit,
            c.from_exit,
            crate::meta::org_time(next)
        );
        notify::enqueue(st, Notice { kind: Kind::Switch, incident_id: None, rule: "router_switch_defer".into(), severity: "warning".into(), machine: name, summary, at: now, opened_at: None }).await;
    }
    Ok(())
}

/// 采集成功之后核对：组现在选中的不是记录里切到的（或组没了）= 路由那边改了（有人手动改、换了配置；OpenClash 重启一般保留选择），记录结束、待切回取消。
/// `started` 是这次采集开始的时刻：切换生效之前开始的采集读到的可能是切之前的，不算。mihomo 的 API 没调通（没有组）时不判
pub async fn reconcile(st: &AppState, router: Uuid, snap: &Snapshot, started: OffsetDateTime) -> anyhow::Result<()> {
    if snap.groups.is_empty() {
        return Ok(());
    }
    let rows: Vec<Active> = sqlx::query_as(&format!("{ACTIVE} and router_id = $1 and switched_at < $2")).bind(router).bind(started).fetch_all(&st.db).await?;
    let mut changed = false;
    for c in rows {
        let g = snap.groups.iter().find(|g| g.name == c.grp);
        if g.is_some_and(|g| g.now.as_deref() == Some(c.to_exit.as_str())) {
            continue;
        }
        let (state, note) = match g {
            Some(g) => ("reverted", format!("已被路由自己改回（现在选中的是「{}」）", g.now.as_deref().unwrap_or("?"))),
            None => ("gone", "路由上已经没有这个策略组".to_string()),
        };
        let mut tx = st.db.begin().await?;
        // 只结束这次核对看到的那一条（期间又切了一次的，switched_at 变了，不动）
        let n = sqlx::query("update router_switch set state = $2, ended_at = now(), end_note = $3 where id = $1 and state = 'active' and switched_at = $4")
            .bind(c.id)
            .bind(state)
            .bind(&note)
            .bind(c.switched_at)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n > 0 {
            let name: String = sqlx::query_scalar("select name from router where id = $1").bind(router).fetch_one(&mut *tx).await?;
            let detail = json!({"name": name, "group": c.grp, "to": c.to_exit, "now": g.and_then(|g| g.now.clone()), "back_at": c.back_at.map(crate::meta::fmt_ts), "note": note});
            audit(&mut tx, AUTO, "router.switch_reverted", "router", Some(router), detail).await?;
            changed = true;
        }
        tx.commit().await?;
    }
    if changed {
        st.emit(Event::Routers);
    }
    Ok(())
}
