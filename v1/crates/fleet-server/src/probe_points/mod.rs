//! 探测点：从靠近客户的地方测线路机的客户入口通不通（为什么见 DESIGN.md「探测点」）。
//!
//! 中控在德国，客户在中国大陆：中控连得上不代表客户连得上（跨境线路、运营商封口都在大陆那一侧），中控偶尔连不上也不代表客户受影响。
//! 探测点是放在靠近客户的地方（如香港）的一台小机器，跑 `fleet-agent probe`：每 30 秒向中控取目标（各线路机的 public_host + 在用入口端口），
//! 按 reach.rs 同样的轮换与「对照目标都不通就整轮作废、三台以上全不通照记并标出」规则建连，把通不通与建连的往返时间报回来，进 relay_reach（probe_point_id 是它）。
//! 中控自己的检查照旧（probe_point_id 为 null），可以在设置里关掉。
//!
//! 「主探测来源」（设置 `reach_source`，app_setting）决定线路机卡片的心跳格、入口表的「连不上」、relay_unreachable 告警看哪一路：
//! 中控，或某个探测点。主来源是探测点、它离线（超过 [`OFFLINE_S`] 没上报，或被停用 / 删掉）时，
//! 或在线、但超过 [`OFFLINE_S`] 没有一轮检查被采纳（每轮都因对照目标连不上而作废，`probe_point.last_kept_at`）时——这时它测不了，等于离线：
//! - 中控的检查开着：退回中控的检查，**连不上照报、连得上不算恢复**（[`Effective::fallback`]）。理由：relay_unreachable 是严重告警，
//!   探测点停机的这段时间不能什么都看不到；中控连续 3 轮连不上（自己的对照目标是通的）多半是线路机或供应商真出了问题，照报；
//!   但香港看到的「连不上」（比如大陆方向被封）中控未必看得到，中控连得上不能把它判成恢复，等探测点回来再判。告警与卡片写明是中控代替的。
//! - 中控的检查关着：标灰（判不出），不开也不关 relay_unreachable。线路机整台挂了仍有 agent 离线告警兜底。
//!
//! 并对探测点本身开告警 `probe_point_offline`（警告，对象是探测点，subject_id = probe_point.id）：启用的、上报过的探测点超过 [`OFFLINE_S`] 没上报，
//! 或在线、但超过 [`OFFLINE_S`] 没有一轮检查被采纳（摘要写「在线，但最近 N 分钟的检查都作废了：对照目标（…）都连不上」）。
//! 上报恢复（且有轮次被采纳）后恢复；停用、删除时关掉、不发恢复。由这里的循环每 30 秒对齐一次（incident.rs，30 分钟内复发重开原告警），
//! 不走机器的告警（alerts.rs 跳过 `probe_` 开头的规则）。
//! 「多久没上报」「多久没采纳」都扣掉中控收不到上报的时段（`AppState::report_gaps`，同 agent 离线）：中控或库停一阵恢复后，不会先判离线、退回中控再切回来。
//!
//! - agent.rs：探测点用的 `/agent/v1/probe/*`（令牌鉴权）；
//! - install.rs：中控经 SSH 安装、一次性安装命令、升级。

pub mod agent;
pub mod install;

use crate::db::audit;
use crate::error::{ApiError, ApiResult};
use crate::incident::{self, Close, Engine, Want};
use crate::state::{AppState, Event};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

/// 探测点超过这么久没上报算离线（每 30 秒报一次，6 轮）：主来源退回中控 / 标灰，开 probe_point_offline。
/// 在线、但这么久没有一轮检查被采纳（每轮都作废）同样处理
pub const OFFLINE_S: i64 = 180;
/// 探测点离线告警的规则名
pub const OFFLINE_RULE: &str = "probe_point_offline";
/// 主探测来源的设置键（app_setting）
const KEY: &str = "reach_source";

/// 卡片、告警里探测点的叫法：名字已经以「探测点」结尾的原样，否则加上（「香港」→「香港探测点」）
pub fn label_of(name: &str) -> String { if name.ends_with("探测点") { name.to_string() } else { format!("{name}探测点") } }

/// 设置里的主探测来源：`primary` 为 None = 中控；`central` = 中控自己的检查开不开
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setting {
    pub primary: Option<Uuid>,
    pub central: bool,
}

impl Default for Setting {
    fn default() -> Self { Self { primary: None, central: true } }
}

impl Setting {
    fn from_json(v: &Value) -> Self {
        Self { primary: v["primary"].as_str().and_then(|s| Uuid::try_parse(s).ok()), central: v["central"].as_bool().unwrap_or(true) }
    }
    fn to_json(self) -> Value { json!({"primary": self.primary, "central": self.central}) }
}

pub async fn setting(db: &sqlx::PgPool) -> sqlx::Result<Setting> {
    let v: Option<Value> = sqlx::query_scalar("select value from app_setting where key = $1").bind(KEY).fetch_optional(db).await?;
    Ok(v.map(|v| Setting::from_json(&v)).unwrap_or_default())
}

/// 此刻实际用哪一路检查（视图、告警、心跳格、入口表都按它取 relay_reach）
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Effective {
    /// central：中控（主来源就是中控）；probe：探测点；fallback：主来源的探测点离线（或在线但检查都作废了），暂由中控代替；none：没有可用的来源（标灰）
    pub state: &'static str,
    /// 用的是哪个探测点的记录（state = probe）
    pub probe_point_id: Option<Uuid>,
    /// 告警、卡片里的说法：「中控」「香港探测点」「中控（代替离线的香港探测点）」
    pub label: String,
    /// 设置里的主来源是探测点时它的叫法（离线、停用也给）
    pub primary_label: Option<String>,
    /// 主来源的探测点为什么用不了（fallback / none 时给，页面直接拿来说）：「香港探测点离线」「香港探测点探测无效」「香港探测点已停用」…
    pub primary_issue: Option<String>,
}

impl Effective {
    pub fn central() -> Self { Self { state: "central", probe_point_id: None, label: "中控".into(), primary_label: None, primary_issue: None } }

    /// 暂由中控代替：连不上照报，连得上不算恢复（reach::unknown_sql）
    pub fn fallback(&self) -> bool { self.state == "fallback" }

    /// relay_reach 里属于这一路的行（`a` 是表别名）；没有可用的来源时一行都不取
    pub fn cond(&self, a: &str) -> String {
        match (self.state, self.probe_point_id) {
            ("none", _) => "false".into(),
            (_, Some(id)) => format!("{a}.probe_point_id = '{id}'::uuid"),
            _ => format!("{a}.probe_point_id is null"),
        }
    }

    /// 写进 relay_reach 的这一路（中控 = None）记的行，是不是此刻用来判断的那一路
    pub fn uses(&self, src: Option<Uuid>) -> bool { self.state != "none" && self.probe_point_id == src }

    /// 说法的 SQL 字面量（拼进告警摘要的那段 SQL）
    pub fn label_sql(&self) -> String { format!("'{}'", self.label.replace('\'', "''")) }
}

/// 探测点此刻的状态（离线、作废都扣掉中控收不到上报的时段）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Liveness {
    /// 上报过
    pub ever: bool,
    /// [`OFFLINE_S`] 内上报过
    pub online: bool,
    /// [`OFFLINE_S`] 内有轮次被采纳（没作废）
    pub kept: bool,
}

impl Liveness {
    pub fn of(st: &AppState, seen: Option<OffsetDateTime>, kept: Option<OffsetDateTime>, now: OffsetDateTime) -> Self {
        let fresh = |t: Option<OffsetDateTime>| t.is_some_and(|t| st.report_gaps.silent_s(t, now) < OFFLINE_S);
        Self { ever: seen.is_some(), online: fresh(seen), kept: fresh(kept) }
    }
    /// 能用来判断：在线、而且最近有轮次被采纳
    pub fn usable(self) -> bool { self.online && self.kept }
}

/// 按设置与探测点此刻在不在线、有没有轮次被采纳算出实际用哪一路（一次查询）。`db`：在哪个池上查（视图加载走 `view_db`，有语句超时）
pub async fn effective(st: &AppState, db: &sqlx::PgPool) -> sqlx::Result<Effective> {
    let row: (Option<Value>, Option<String>, bool, Option<OffsetDateTime>, Option<OffsetDateTime>) = sqlx::query_as(
        "select s.v, p.name, coalesce(p.enabled, false), p.last_seen_at, p.last_kept_at
         from (select (select value from app_setting where key = $1) v) s
         left join probe_point p on p.id = case when s.v->>'primary' ~ '^[0-9a-f-]{36}$' then (s.v->>'primary')::uuid end",
    )
    .bind(KEY)
    .fetch_one(db)
    .await?;
    let set = row.0.as_ref().map(Setting::from_json).unwrap_or_default();
    Ok(decide(set, row.1.as_deref(), row.2, Liveness::of(st, row.3, row.4, OffsetDateTime::now_utc())))
}

/// 纯函数部分（便于测试）：`name` 为 None = 主来源的探测点已经删了
pub fn decide(set: Setting, name: Option<&str>, enabled: bool, live: Liveness) -> Effective {
    let Some(id) = set.primary else {
        return if set.central { Effective::central() } else { Effective { state: "none", probe_point_id: None, label: "中控（检查已关闭）".into(), primary_label: None, primary_issue: None } };
    };
    let pl = name.map(label_of).unwrap_or_else(|| "主探测点（已删除）".into());
    if name.is_some() && enabled && live.usable() {
        return Effective { state: "probe", probe_point_id: Some(id), label: pl.clone(), primary_label: Some(pl), primary_issue: None };
    }
    // (定语, 状态)：「离线的香港探测点」放进中控代替时的叫法，「香港探测点离线」给页面直接说
    let (why, issue) = match () {
        _ if name.is_none() => (pl.clone(), "主探测点已删除".to_string()),
        _ if !enabled => (format!("已停用的{pl}"), format!("{pl}已停用")),
        _ if !live.ever => (format!("还没上报的{pl}"), format!("{pl}还没上报")),
        _ if live.online => (format!("探测无效的{pl}"), format!("{pl}探测无效")),
        _ => (format!("离线的{pl}"), format!("{pl}离线")),
    };
    if set.central {
        Effective { state: "fallback", probe_point_id: None, label: format!("中控（代替{why}）"), primary_label: Some(pl), primary_issue: Some(issue) }
    } else {
        Effective { state: "none", probe_point_id: None, label: format!("{pl}（不可用）"), primary_label: Some(pl), primary_issue: Some(issue) }
    }
}

/// 改主探测来源：探测点要存在、启用；主来源是中控时中控的检查不能关。写库、写审计，返回改之前的
pub async fn save_setting(st: &AppState, actor: &str, new: Setting) -> ApiResult<Setting> {
    let mut tx = st.db.begin().await?;
    // 锁住设置行所在的位置（同一时刻只让一个在改），读旧值
    sqlx::query("select pg_advisory_xact_lock(hashtext('app_setting.reach_source'))").execute(&mut *tx).await?;
    let old: Option<Value> = sqlx::query_scalar("select value from app_setting where key = $1").bind(KEY).fetch_optional(&mut *tx).await?;
    let old = old.map(|v| Setting::from_json(&v)).unwrap_or_default();
    let mut name = None;
    if let Some(id) = new.primary {
        let p: Option<(String, bool)> = sqlx::query_as("select name, enabled from probe_point where id = $1").bind(id).fetch_optional(&mut *tx).await?;
        match p {
            None => return Err(ApiError::bad_request("没有这个探测点")),
            Some((n, false)) => return Err(ApiError::conflict(format!("探测点「{n}」已停用，先启用再设为主来源"))),
            Some((n, true)) => name = Some(n),
        }
    } else if !new.central {
        return Err(ApiError::conflict("主探测来源是中控时不能关掉中控的检查：先把主来源设成某个探测点"));
    }
    if old == new {
        return Ok(old);
    }
    sqlx::query(
        "insert into app_setting (key, value, updated_by) values ($1, $2, $3)
         on conflict (key) do update set value = excluded.value, updated_at = now(), updated_by = excluded.updated_by",
    )
    .bind(KEY)
    .bind(new.to_json())
    .bind(actor)
    .execute(&mut *tx)
    .await?;
    let primary = name.unwrap_or_else(|| "中控".into());
    audit(&mut tx, actor, "settings.reach_source", "setting", None, json!({"from": old.to_json(), "to": new.to_json(), "primary_name": primary})).await?;
    tx.commit().await?;
    changed(st);
    Ok(old)
}

/// 主来源可能变了（设置、探测点启停 / 删除 / 刚回来）：视图整份重算（卡片、告警换一路检查），设置页与心跳格重取
pub fn changed(st: &AppState) {
    st.emit(Event::Machines);
    st.emit(Event::Settings);
}

/// 后台循环：每 30 秒对齐探测点离线告警；实际用的那一路变了（主来源的探测点离线 / 检查都作废 / 回来）时发事件，视图不用等别的变化才重算
pub async fn run_forever(st: AppState) {
    let mut t = tokio::time::interval(Duration::from_secs(30));
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    if let Err(e) = install::mark_interrupted(&st).await {
        tracing::warn!("把中断的探测点安装标为失败：{e:#}");
    }
    let mut last: Option<Effective> = None;
    loop {
        t.tick().await;
        match effective(&st, &st.db).await {
            Ok(e) => {
                if last.as_ref().is_some_and(|l| *l != e) {
                    tracing::info!(from = ?last.as_ref().map(|l| &l.label), to = %e.label, "线路机连通性改用另一路检查");
                    changed(&st);
                }
                last = Some(e);
            }
            Err(e) => tracing::warn!("读主探测来源失败：{e:#}"),
        }
        if let Err(e) = check_once(&st, OffsetDateTime::now_utc()).await {
            tracing::warn!("探测点告警：{e:#}");
        }
    }
}

/// 探测点表的一行：(id, 名字, 启用, 最后上报, 最后采纳, 丢弃原因)
type ProbeRow = (Uuid, String, bool, Option<OffsetDateTime>, Option<OffsetDateTime>, Option<String>);

/// 对齐一轮 probe_point_offline（incident.rs）：离线（超过 [`OFFLINE_S`] 没上报），或在线但超过 [`OFFLINE_S`] 没有一轮检查被采纳。
/// 都扣掉中控收不到上报的时段。`now` 由调用方给（测试可以快进）
pub async fn check_once(st: &AppState, now: OffsetDateTime) -> anyhow::Result<()> {
    let open = incident::firing(&st.db, Engine::Probe).await?;
    let rows: Vec<ProbeRow> =
        sqlx::query_as("select id, name, enabled, last_seen_at, last_kept_at, discard_reason from probe_point").fetch_all(&st.db).await?;
    let set = setting(&st.db).await?;
    let names: HashMap<Uuid, (String, bool)> = rows.iter().map(|r| (r.0, (r.1.clone(), r.2))).collect();
    let dur = |t: OffsetDateTime| crate::notify::fmt_duration((now - t).whole_seconds());
    let want: Vec<Want> = rows
        .iter()
        .filter_map(|(id, name, enabled, seen, kept, why)| {
            let seen = (*seen).filter(|_| *enabled)?;
            let live = Liveness::of(st, Some(seen), *kept, now);
            if live.usable() {
                return None;
            }
            let why = why.as_deref().unwrap_or("原因不明");
            let mut text = match *kept {
                _ if !live.online => format!("{}没有上报（最近一次 {}）", dur(seen), crate::meta::org_md(seen)),
                Some(k) => format!("在线，但最近 {}的探测都无效：{why}", dur(k)),
                None => format!("在线，但还没有一次有效的探测：{why}"),
            };
            if set.primary == Some(*id) {
                text.push_str(if set.central { "；它是主探测来源，暂由中控代测（连不上照常告警，连得上不算恢复）" } else { "；它是主探测来源，中控探测也关着，暂时判断不了线路机通不通" });
            }
            Some(Want { rule: OFFLINE_RULE, subject: *id, name, severity: "warning", text, detail: None, ready: true })
        })
        .collect();
    incident::reconcile(st, Engine::Probe, open, want, "exists (select 1 from probe_point where id = $3)", now, |inc| match names.get(&inc.subject_id) {
        None => Close::Quiet("probe_point_removed"),
        Some((_, false)) => Close::Quiet("probe_point_disabled"),
        Some((n, true)) => Close::Resolve(n),
    })
    .await?;
    Ok(())
}

/// 列表里一个探测点：状态（online / blind：在线但检查都作废了 / offline / pending / disabled）、最近上报、版本、升级（upgrade：在等它换上；
/// upgrade_failed：上次升级失败了 {sha256, to, from, at}，见 agent.rs）、
/// 近 `window`（1h 读原始记录；24h / 7d / 30d 读小时汇总，见 reach::quality）到各线路机的成功次数与延迟
pub async fn list(st: &AppState, window: crate::reach::Window) -> sqlx::Result<Vec<Value>> {
    let set = setting(&st.db).await?;
    #[derive(sqlx::FromRow)]
    struct PRow {
        id: Uuid,
        name: String,
        enabled: bool,
        note: String,
        installed: bool,
        bootstrap_expires_at: Option<OffsetDateTime>,
        install_state: Option<String>,
        install_step: Option<String>,
        install_error: Option<String>,
        install_host: Option<String>,
        install_port: Option<i32>,
        install_started_at: Option<OffsetDateTime>,
        install_finished_at: Option<OffsetDateTime>,
        upgrade_sha256: Option<String>,
        upgrade_version: Option<String>,
        upgrade_failed: Option<Value>,
        last_seen_at: Option<OffsetDateTime>,
        last_kept_at: Option<OffsetDateTime>,
        discard_reason: Option<String>,
        last_ip: Option<String>,
        version: Option<String>,
        created_at: OffsetDateTime,
    }
    let rows: Vec<PRow> = sqlx::query_as(
        "select id, name, enabled, note, token_hash is not null installed, case when bootstrap_expires_at > now() then bootstrap_expires_at end bootstrap_expires_at,
                install_state, install_step, install_error, install_host, install_port, install_started_at, install_finished_at, upgrade_sha256,
                upgrade_version, upgrade_failed, last_seen_at, last_kept_at, discard_reason, host(last_ip) last_ip, version, created_at
         from probe_point order by created_at, id",
    )
    .fetch_all(&st.db)
    .await?;
    // 到各线路机：平均 / 中位延迟（只算连上的）、连了几次、通了几次（中控那一路不在这里）
    let mut per: HashMap<Uuid, Vec<Value>> = HashMap::new();
    for q in crate::reach::quality(&st.db, window, None).await? {
        if let Some(p) = q.probe_point_id {
            per.entry(p).or_default().push(json!({"relay_id": q.relay_id, "name": q.relay_name, "avg_ms": q.avg_ms, "median_ms": q.median_ms, "total": q.total, "ok": q.ok}));
        }
    }
    let now = OffsetDateTime::now_utc();
    let ts = |t: Option<OffsetDateTime>| t.and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok());
    Ok(rows
        .into_iter()
        .map(|r| {
            let live = Liveness::of(st, r.last_seen_at, r.last_kept_at, now);
            let state = match () {
                _ if !r.enabled => "disabled",
                _ if live.usable() => "online",
                _ if live.online => "blind",
                _ if r.last_seen_at.is_none() => "pending",
                _ => "offline",
            };
            let install = r.install_state.as_ref().map(|s| {
                json!({"state": s, "step": r.install_step, "error": r.install_error, "host": r.install_host, "port": r.install_port,
                       "started_at": ts(r.install_started_at), "finished_at": ts(r.install_finished_at)})
            });
            json!({
                "id": r.id, "name": r.name, "label": label_of(&r.name), "enabled": r.enabled, "note": r.note, "state": state, "installed": r.installed,
                "primary": set.primary == Some(r.id), "last_seen_at": ts(r.last_seen_at), "last_kept_at": ts(r.last_kept_at),
                "discard_reason": r.discard_reason.filter(|_| live.online && !live.kept), "last_ip": r.last_ip, "version": r.version,
                "window": window.as_str(),
                "created_at": ts(Some(r.created_at)), "bootstrap_expires_at": ts(r.bootstrap_expires_at), "install": install,
                "upgrade": r.upgrade_sha256.map(|s| json!({"sha256": s, "version": r.upgrade_version})), "upgrade_failed": r.upgrade_failed,
                "relays": per.remove(&r.id).unwrap_or_default(),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_source_and_fallback() {
        let id = Uuid::nil();
        let hk = |central| Setting { primary: Some(id), central };
        let live = |ever, online, kept| Liveness { ever, online, kept };
        let up = live(true, true, true);
        let off = live(true, false, false);
        let never = live(false, false, false);
        assert_eq!(decide(Setting::default(), None, false, never), Effective::central());
        let e = decide(hk(true), Some("香港"), true, up);
        assert_eq!((e.state, e.label.as_str(), e.probe_point_id), ("probe", "香港探测点", Some(id)));
        assert_eq!(e.cond("rr"), format!("rr.probe_point_id = '{id}'::uuid"));
        assert!(e.uses(Some(id)) && !e.uses(None));
        // 离线：中控的检查开着就退回中控（恢复不算），关着就标灰
        let f = decide(hk(true), Some("香港探测点"), true, off);
        assert_eq!((f.state, f.label.as_str(), f.cond("r")), ("fallback", "中控（代替离线的香港探测点）", "r.probe_point_id is null".to_string()));
        assert!(f.fallback() && f.uses(None));
        let g = decide(hk(false), Some("香港"), true, off);
        assert_eq!((g.state, g.cond("r")), ("none", "false".to_string()));
        assert!(!g.uses(None) && !g.uses(Some(id)));
        assert_eq!(g.primary_issue.as_deref(), Some("香港探测点离线"));
        // 中控的检查关着、主探测点在线但探测无效：页面要说「探测无效」，不能说成离线
        let gb = decide(hk(false), Some("香港"), true, live(true, true, false));
        assert_eq!((gb.state, gb.label.as_str(), gb.primary_issue.as_deref()), ("none", "香港探测点（不可用）", Some("香港探测点探测无效")));
        // 在线、但最近的检查都作废了：同离线
        let b = decide(hk(true), Some("香港"), true, live(true, true, false));
        assert_eq!((b.state, b.label.as_str(), b.primary_issue.as_deref()), ("fallback", "中控（代替探测无效的香港探测点）", Some("香港探测点探测无效")));
        assert_eq!(decide(hk(true), Some("香港"), true, never).label, "中控（代替还没上报的香港探测点）");
        assert_eq!(decide(hk(true), Some("香港"), false, up).label, "中控（代替已停用的香港探测点）");
        assert_eq!(decide(hk(true), None, false, never).state, "fallback");
        assert_eq!(decide(Setting { primary: None, central: false }, None, false, never).state, "none");
    }

    #[test]
    fn label_sql_escapes_quotes() {
        let e = Effective { state: "probe", probe_point_id: None, label: "o'clock探测点".into(), primary_label: None, primary_issue: None };
        assert_eq!(e.label_sql(), "'o''clock探测点'");
    }

    #[test]
    fn setting_json_roundtrip() {
        let s = Setting { primary: Some(Uuid::now_v7()), central: false };
        assert_eq!(Setting::from_json(&s.to_json()), s);
        assert_eq!(Setting::from_json(&json!({})), Setting::default());
    }
}
