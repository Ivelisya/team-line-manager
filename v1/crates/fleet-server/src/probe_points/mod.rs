//! 探测点：从靠近客户的地方测线路机的客户入口通不通（为什么见 DESIGN.md「探测点」）。
//!
//! 中控在德国，客户在中国大陆：中控连得上不代表客户连得上（跨境线路、运营商封口都在大陆那一侧），中控偶尔连不上也不代表客户受影响。
//! 探测点是放在靠近客户的地方（如香港）的一台小机器，跑 `fleet-agent probe`：每 30 秒向中控取目标（各线路机的 public_host + 在用入口端口），
//! 按 reach.rs 同样的轮换与「对照目标都不通就整轮作废、三台以上全不通照记并标出」规则建连，把通不通与建连的往返时间报回来，进 relay_reach（probe_point_id 是它）。
//! 中控自己的检查照旧（probe_point_id 为 null），可以在设置里关掉。
//!
//! 「探测顺序」（设置 `reach_source`，app_setting：`order` 是探测点 id 的有序列表，`central` 是中控自己的检查开不开）决定线路机卡片的心跳格、
//! 入口表的「连不上」、relay_unreachable 告警看哪一路：按顺序用第一个能用的探测点，第一个就是「主探测来源」；列表为空时主来源是中控。
//! 一个探测点离线（超过 [`OFFLINE_S`] 没上报，或被删掉）、停用，或在线、但超过 [`OFFLINE_S`] 没有一轮检查被采纳
//! （每轮都因对照目标连不上而作废，`probe_point.last_kept_at`）时，它测不了，等于离线，轮到下一个：
//! - 主来源用不了、后面有能用的探测点（如家里 PVE → 香港）：由它代测；后面的都用不了、中控的检查开着：由中控代测。
//!   代测时**连不上照报、连得上不算恢复**（[`Effective::fallback`]）。理由：relay_unreachable 是严重告警，主来源停机的这段时间不能什么都看不到；
//!   代测的一路连续 3 轮连不上（自己的对照目标是通的）多半是线路机或供应商真出了问题，照报；但主来源看到的「连不上」（比如大陆方向被封）
//!   代测的那一路未必看得到，它连得上不能把它判成恢复，等主来源回来再判。告警与卡片写明是谁代替的。
//! - 都用不了、中控的检查关着：标灰（判不出），不开也不关 relay_unreachable。线路机整台挂了仍有 agent 离线告警兜底。
//!
//! 并对探测点本身开告警 `probe_point_offline`（警告，对象是探测点，subject_id = probe_point.id）：启用的、上报过的探测点超过 [`OFFLINE_S`] 没上报，
//! 或在线、但超过 [`OFFLINE_S`] 没有一轮检查被采纳（摘要写「在线，但最近 N 分钟的检查都作废了：对照目标（…）都连不上」）。
//! 上报恢复（且有轮次被采纳）后恢复；停用、删除时关掉、不发恢复。由这里的循环每 30 秒对齐一次（incident.rs，30 分钟内复发重开原告警），
//! 不走机器的告警（alerts.rs 跳过 `probe_` 开头的规则）。
//! 实际用的是探测点、这一轮全部线路机同时连不上（`all_down`）时，relay_unreachable 的摘要对照一下顺序里的下一路（[`Effective::compare`]）最近 2 分钟：
//! 下一路连得上，多半是大陆方向到线路机被封或这个探测点所在网络出境异常，不写「供应商整体故障」；下一路也连不上才写供应商整体故障。只改说法，照常记、照常告警。
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

/// 探测顺序最多这么多个探测点
pub const MAX_ORDER: usize = 8;

/// 设置里的探测顺序：`order` 是探测点 id，按顺序用第一个能用的（空 = 主来源是中控）；`central` = 中控自己的检查开不开
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    pub order: Vec<Uuid>,
    pub central: bool,
}

impl Default for Setting {
    fn default() -> Self { Self { order: vec![], central: true } }
}

impl Setting {
    fn from_json(v: &Value) -> Self {
        let order = v["order"].as_array().map(|a| a.iter().filter_map(|s| s.as_str().and_then(|s| Uuid::try_parse(s).ok())).collect()).unwrap_or_default();
        Self { order, central: v["central"].as_bool().unwrap_or(true) }
    }
    fn to_json(&self) -> Value { json!({"order": self.order, "central": self.central}) }
    /// 主探测来源（顺序里的第一个探测点；None = 中控）
    pub fn primary(&self) -> Option<Uuid> { self.order.first().copied() }
    /// 在顺序里排第几（从 1 起）
    pub fn rank(&self, id: Uuid) -> Option<usize> { self.order.iter().position(|o| *o == id).map(|i| i + 1) }
}

pub async fn setting(db: &sqlx::PgPool) -> sqlx::Result<Setting> {
    let v: Option<Value> = sqlx::query_scalar("select value from app_setting where key = $1").bind(KEY).fetch_optional(db).await?;
    Ok(v.map(|v| Setting::from_json(&v)).unwrap_or_default())
}

/// 此刻实际用哪一路检查（视图、告警、心跳格、入口表都按它取 relay_reach）
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Effective {
    /// central：中控（主来源就是中控）；probe：主来源的探测点；fallback：主来源的探测点离线（或在线但检查都作废了），暂由顺序里后面的探测点或中控代替；
    /// none：没有可用的来源（标灰）
    pub state: &'static str,
    /// 用的是哪个探测点的记录（state = probe，或由探测点代测的 fallback；None = 中控的）
    pub probe_point_id: Option<Uuid>,
    /// 告警、卡片里的说法：「中控」「香港探测点」「香港探测点（代替离线的PVE探测点）」「中控（代替离线的香港探测点）」
    pub label: String,
    /// 设置里的主来源是探测点时它的叫法（离线、停用也给）
    pub primary_label: Option<String>,
    /// 主来源的探测点为什么用不了（fallback / none 时给，页面直接拿来说）：「香港探测点离线」「香港探测点探测无效」「香港探测点已停用」…
    pub primary_issue: Option<String>,
    /// 代测的是谁（fallback 时给）：「香港探测点」「中控」
    pub substitute: Option<String>,
    /// 这一路是探测点时，探测顺序里排在它后面的下一路最近有没有连得上线路机，决定全部线路机同时连不上时摘要怎么说（reach::unreachable_sql）；中控、没有下一路为 None
    #[serde(skip)]
    pub compare: Option<Compare>,
}

/// 对照用的下一路：(探测点 id，None = 中控)、叫法，和现在这一路的探测点的叫法
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compare {
    pub id: Option<Uuid>,
    pub label: String,
    pub own: String,
}

impl Compare {
    /// 下一路的行（`a` 是 relay_reach 的表别名）
    pub fn cond(&self, a: &str) -> String {
        match self.id {
            Some(id) => format!("{a}.probe_point_id = '{id}'::uuid"),
            None => format!("{a}.probe_point_id is null"),
        }
    }
}

impl Effective {
    pub fn central() -> Self { Self { state: "central", probe_point_id: None, label: "中控".into(), primary_label: None, primary_issue: None, substitute: None, compare: None } }

    /// 暂由别的来源代替主来源：连不上照报，连得上不算恢复（reach::unknown_sql）
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

/// 顺序里一个探测点此刻的情况（[`decide`] 用）：`name` 为 None = 已经删了
#[derive(Debug, Clone, Copy)]
pub struct Cand<'a> {
    pub id: Uuid,
    pub name: Option<&'a str>,
    pub enabled: bool,
    pub live: Liveness,
}

/// 顺序里探测点的一行：(id, 名字, 启用, 最后上报, 最后采纳)
type LiveRow = (Uuid, String, bool, Option<OffsetDateTime>, Option<OffsetDateTime>);

/// 顺序里各探测点此刻的情况（已经删了的 name 为 None）。`rows` 是 `order` 里探测点在 probe_point 表里查到的行
fn cands_of<'a>(st: &AppState, order: &[Uuid], rows: &'a [LiveRow], now: OffsetDateTime) -> Vec<Cand<'a>> {
    order
        .iter()
        .map(|id| match rows.iter().find(|r| r.0 == *id) {
            Some(r) => Cand { id: *id, name: Some(&r.1), enabled: r.2, live: Liveness::of(st, r.3, r.4, now) },
            None => Cand { id: *id, name: None, enabled: false, live: Liveness { ever: false, online: false, kept: false } },
        })
        .collect()
}

/// 按设置与顺序里各探测点此刻在不在线、有没有轮次被采纳算出实际用哪一路。`db`：在哪个池上查（视图加载走 `view_db`，有语句超时）
pub async fn effective(st: &AppState, db: &sqlx::PgPool) -> sqlx::Result<Effective> {
    let v: Option<Value> = sqlx::query_scalar("select value from app_setting where key = $1").bind(KEY).fetch_optional(db).await?;
    let set = v.as_ref().map(Setting::from_json).unwrap_or_default();
    let rows: Vec<LiveRow> = if set.order.is_empty() {
        vec![]
    } else {
        sqlx::query_as("select id, name, enabled, last_seen_at, last_kept_at from probe_point where id = any($1)").bind(&set.order).fetch_all(db).await?
    };
    let cands = cands_of(st, &set.order, &rows, OffsetDateTime::now_utc());
    Ok(decide(set.central, &cands))
}

/// 纯函数部分（便于测试）：`central` = 中控的检查开不开，`cands` = 探测顺序里的探测点（按顺序）
pub fn decide(central: bool, cands: &[Cand]) -> Effective {
    let mut e = pick(central, cands);
    // 实际用的是探测点：记下顺序里它后面的下一路（全部线路机同时连不上时拿来对照）
    e.compare = e.probe_point_id.and_then(|id| {
        let own = label_of(cands.iter().find(|c| c.id == id)?.name?);
        next_source(central, cands, id).map(|(id, label)| Compare { id, label, own })
    });
    e
}

fn pick(central: bool, cands: &[Cand]) -> Effective {
    let Some(first) = cands.first() else {
        return if central {
            Effective::central()
        } else {
            Effective { state: "none", probe_point_id: None, label: "中控（检查已关闭）".into(), primary_label: None, primary_issue: None, substitute: None, compare: None }
        };
    };
    let usable = |c: &Cand| c.name.is_some() && c.enabled && c.live.usable();
    let pl = first.name.map(label_of).unwrap_or_else(|| "主探测点（已删除）".into());
    if usable(first) {
        return Effective { state: "probe", probe_point_id: Some(first.id), label: pl.clone(), primary_label: Some(pl), primary_issue: None, substitute: None, compare: None };
    }
    // (定语, 状态)：「离线的香港探测点」放进代替时的叫法，「香港探测点离线」给页面直接说
    let (why, issue) = match () {
        _ if first.name.is_none() => (pl.clone(), "主探测点已删除".to_string()),
        _ if !first.enabled => (format!("已停用的{pl}"), format!("{pl}已停用")),
        _ if !first.live.ever => (format!("还没上报的{pl}"), format!("{pl}还没上报")),
        _ if first.live.online => (format!("探测无效的{pl}"), format!("{pl}探测无效")),
        _ => (format!("离线的{pl}"), format!("{pl}离线")),
    };
    let by = cands[1..].iter().find(|c| usable(c)).map(|c| (Some(c.id), label_of(c.name.unwrap_or_default())));
    match by.or_else(|| central.then(|| (None, "中控".to_string()))) {
        Some((id, sub)) => Effective {
            state: "fallback",
            probe_point_id: id,
            label: format!("{sub}（代替{why}）"),
            primary_label: Some(pl),
            primary_issue: Some(issue),
            substitute: Some(sub),
            compare: None,
        },
        None => Effective { state: "none", probe_point_id: None, label: format!("{pl}（不可用）"), primary_label: Some(pl), primary_issue: Some(issue), substitute: None, compare: None },
    }
}

/// 探测顺序里排在 `me` 后面、此刻能用的下一路来源：(探测点 id，None = 中控, 叫法)。`me` 不在顺序里、或后面没有能用的探测点而中控的检查也关着为 None
pub fn next_source(central: bool, cands: &[Cand], me: Uuid) -> Option<(Option<Uuid>, String)> {
    let i = cands.iter().position(|c| c.id == me)?;
    let next = cands[i + 1..].iter().find(|c| c.name.is_some() && c.enabled && c.live.usable());
    match next {
        Some(c) => Some((Some(c.id), label_of(c.name.unwrap_or_default()))),
        None => central.then(|| (None, "中控".to_string())),
    }
}

/// 改探测顺序：探测点要存在、启用、不重复，最多 [`MAX_ORDER`] 个；顺序为空（主来源是中控）时中控的检查不能关。写库、写审计，返回改之前的
pub async fn save_setting(st: &AppState, actor: &str, new: Setting) -> ApiResult<Setting> {
    if new.order.len() > MAX_ORDER {
        return Err(ApiError::bad_request(format!("探测顺序最多 {MAX_ORDER} 个探测点")));
    }
    if new.order.iter().enumerate().any(|(i, id)| new.order[..i].contains(id)) {
        return Err(ApiError::bad_request("探测顺序里有重复的探测点"));
    }
    if new.order.is_empty() && !new.central {
        return Err(ApiError::conflict("主探测来源是中控时不能关掉中控的检查：先在探测顺序里放一个探测点"));
    }
    let mut tx = st.db.begin().await?;
    // 锁住设置行所在的位置（同一时刻只让一个在改），读旧值
    sqlx::query("select pg_advisory_xact_lock(hashtext('app_setting.reach_source'))").execute(&mut *tx).await?;
    let old: Option<Value> = sqlx::query_scalar("select value from app_setting where key = $1").bind(KEY).fetch_optional(&mut *tx).await?;
    let old = old.map(|v| Setting::from_json(&v)).unwrap_or_default();
    let rows: Vec<(Uuid, String, bool)> = sqlx::query_as("select id, name, enabled from probe_point where id = any($1)").bind(&new.order).fetch_all(&mut *tx).await?;
    let mut names = vec![];
    for id in &new.order {
        match rows.iter().find(|r| r.0 == *id) {
            None => return Err(ApiError::bad_request("没有这个探测点")),
            Some((_, n, false)) => return Err(ApiError::conflict(format!("探测点「{n}」已停用，先启用再放进探测顺序"))),
            Some((_, n, true)) => names.push(n.clone()),
        }
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
    audit(&mut tx, actor, "settings.reach_source", "setting", None, json!({"from": old.to_json(), "to": new.to_json(), "order_names": names})).await?;
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
    let cands: Vec<Cand> = set
        .order
        .iter()
        .filter_map(|id| rows.iter().find(|r| r.0 == *id))
        .map(|r| Cand { id: r.0, name: Some(&r.1), enabled: r.2, live: Liveness::of(st, r.3, r.4, now) })
        .collect();
    let eff = decide(set.central, &cands);
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
            match set.rank(*id) {
                Some(1) => match &eff.substitute {
                    Some(s) => text.push_str(&format!("；它是主探测来源，暂由{s}代测（连不上照常告警，连得上不算恢复）")),
                    None => text.push_str("；它是主探测来源，后面的探测来源也都用不了、中控探测关着，暂时判断不了线路机通不通"),
                },
                Some(n) => text.push_str(&format!("；它在探测顺序里排第 {n}，主来源用不了时现在也代替不了")),
                None => {}
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

/// 列表里一个探测点：状态（online / blind：在线但检查都作废了 / offline / pending / disabled）、维护中（maintenance：自己的或全局的维护窗口）、最近上报、版本、升级（upgrade：在等它换上；
/// upgrade_failed：上次升级失败了 {sha256, to, from, at}，见 agent.rs）、
/// 近 `window`（1h 读原始记录；24h / 7d / 30d 读小时汇总，见 reach::quality）到各线路机的成功次数与延迟
pub async fn list(st: &AppState, window: crate::reach::Window) -> sqlx::Result<Vec<Value>> {
    let set = setting(&st.db).await?;
    let silences = crate::silence::Silences::load(&st.db).await?;
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
    let ts = |t: Option<OffsetDateTime>| t.map(crate::meta::fmt_ts);
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
                "primary": set.primary() == Some(r.id), "rank": set.rank(r.id), "maintenance": silences.maintenance(r.id), "last_seen_at": ts(r.last_seen_at), "last_kept_at": ts(r.last_kept_at),
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

    fn live(ever: bool, online: bool, kept: bool) -> Liveness { Liveness { ever, online, kept } }
    const UP: Liveness = Liveness { ever: true, online: true, kept: true };
    const OFF: Liveness = Liveness { ever: true, online: false, kept: false };
    const NEVER: Liveness = Liveness { ever: false, online: false, kept: false };

    fn c(id: Uuid, name: Option<&str>, enabled: bool, live: Liveness) -> Cand<'_> { Cand { id, name, enabled, live } }

    #[test]
    fn effective_source_and_fallback() {
        let id = Uuid::nil();
        let hk = |enabled, l| [c(id, Some("香港"), enabled, l)];
        assert_eq!(decide(true, &[]), Effective::central());
        let e = decide(true, &hk(true, UP));
        assert_eq!((e.state, e.label.as_str(), e.probe_point_id), ("probe", "香港探测点", Some(id)));
        assert_eq!(e.cond("rr"), format!("rr.probe_point_id = '{id}'::uuid"));
        assert!(e.uses(Some(id)) && !e.uses(None));
        // 离线：中控的检查开着就退回中控（恢复不算），关着就标灰
        let f = decide(true, &[c(id, Some("香港探测点"), true, OFF)]);
        assert_eq!((f.state, f.label.as_str(), f.cond("r")), ("fallback", "中控（代替离线的香港探测点）", "r.probe_point_id is null".to_string()));
        assert!(f.fallback() && f.uses(None));
        assert_eq!(f.substitute.as_deref(), Some("中控"));
        let g = decide(false, &hk(true, OFF));
        assert_eq!((g.state, g.cond("r")), ("none", "false".to_string()));
        assert!(!g.uses(None) && !g.uses(Some(id)));
        assert_eq!(g.primary_issue.as_deref(), Some("香港探测点离线"));
        // 中控的检查关着、主探测点在线但探测无效：页面要说「探测无效」，不能说成离线
        let gb = decide(false, &hk(true, live(true, true, false)));
        assert_eq!((gb.state, gb.label.as_str(), gb.primary_issue.as_deref()), ("none", "香港探测点（不可用）", Some("香港探测点探测无效")));
        // 在线、但最近的检查都作废了：同离线
        let b = decide(true, &hk(true, live(true, true, false)));
        assert_eq!((b.state, b.label.as_str(), b.primary_issue.as_deref()), ("fallback", "中控（代替探测无效的香港探测点）", Some("香港探测点探测无效")));
        assert_eq!(decide(true, &hk(true, NEVER)).label, "中控（代替还没上报的香港探测点）");
        assert_eq!(decide(true, &hk(false, UP)).label, "中控（代替已停用的香港探测点）");
        assert_eq!(decide(true, &[c(id, None, false, NEVER)]).state, "fallback");
        assert_eq!(decide(false, &[]).state, "none");
    }

    /// 探测顺序：主来源用不了时按顺序找下一个能用的探测点，都用不了才轮到中控
    #[test]
    fn order_falls_through_to_next_probe_then_central() {
        let (pve, hk) = (Uuid::now_v7(), Uuid::now_v7());
        let e = decide(true, &[c(pve, Some("PVE"), true, UP), c(hk, Some("香港"), true, UP)]);
        assert_eq!((e.state, e.probe_point_id), ("probe", Some(pve)));
        let f = decide(true, &[c(pve, Some("PVE"), true, OFF), c(hk, Some("香港"), true, UP)]);
        assert_eq!((f.state, f.label.as_str(), f.probe_point_id), ("fallback", "香港探测点（代替离线的PVE探测点）", Some(hk)));
        assert_eq!((f.substitute.as_deref(), f.primary_issue.as_deref()), (Some("香港探测点"), Some("PVE探测点离线")));
        assert!(f.fallback() && f.uses(Some(hk)) && !f.uses(None) && !f.uses(Some(pve)));
        assert_eq!(f.cond("r"), format!("r.probe_point_id = '{hk}'::uuid"));
        // 后面的也用不了（停用、探测无效）：中控代测；中控也关着：标灰
        let both = [c(pve, Some("PVE"), true, OFF), c(hk, Some("香港"), true, live(true, true, false))];
        let g = decide(true, &both);
        assert_eq!((g.state, g.label.as_str(), g.probe_point_id), ("fallback", "中控（代替离线的PVE探测点）", None));
        assert_eq!(decide(false, &both).label, "PVE探测点（不可用）");
        assert_eq!(decide(true, &[c(pve, Some("PVE"), true, OFF), c(hk, Some("香港"), false, UP)]).substitute.as_deref(), Some("中控"));
    }

    /// 对照下一路：顺序里排在后面、此刻能用的探测点，没有就是中控（开着的话）；自己不在顺序里没有下一路
    #[test]
    fn next_source_is_next_usable_probe_then_central() {
        let (pve, hk, tk) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let all = [c(pve, Some("PVE"), true, UP), c(hk, Some("香港"), true, UP), c(tk, Some("东京"), true, UP)];
        assert_eq!(next_source(true, &all, pve), Some((Some(hk), "香港探测点".to_string())));
        assert_eq!(next_source(true, &all, hk), Some((Some(tk), "东京探测点".to_string())));
        // 下一个用不了（探测无效、停用、已删除）：跳过，找再后面的
        let skip = [c(pve, Some("PVE"), true, UP), c(hk, Some("香港"), true, live(true, true, false)), c(Uuid::now_v7(), None, false, NEVER), c(tk, Some("东京"), true, UP)];
        assert_eq!(next_source(false, &skip, pve), Some((Some(tk), "东京探测点".to_string())));
        // 后面的都用不了：中控的检查开着就是中控，关着没有；最后一个同理；不在顺序里没有
        assert_eq!(next_source(true, &skip[..2], pve), Some((None, "中控".to_string())));
        assert_eq!(next_source(false, &skip[..2], pve), None);
        assert_eq!(next_source(true, &all, tk), Some((None, "中控".to_string())));
        assert_eq!(next_source(true, &all, Uuid::now_v7()), None);
    }

    /// 全部线路机同时连不上时拿来对照的下一路：相对实际用的那个探测点；中控、标灰、没有下一路都没有
    #[test]
    fn effective_compare_follows_the_used_probe() {
        let (pve, hk) = (Uuid::now_v7(), Uuid::now_v7());
        let cmp = |id, label: &str, own: &str| Some(Compare { id, label: label.into(), own: own.into() });
        let e = decide(true, &[c(pve, Some("PVE"), true, UP), c(hk, Some("香港"), true, UP)]);
        assert_eq!(e.compare, cmp(Some(hk), "香港探测点", "PVE探测点"));
        assert_eq!(e.compare.unwrap().cond("nx"), format!("nx.probe_point_id = '{hk}'::uuid"));
        // 只有一个探测点：下一路是中控；中控关着没有
        assert_eq!(decide(true, &[c(pve, Some("PVE"), true, UP)]).compare, cmp(None, "中控", "PVE探测点"));
        assert_eq!(decide(false, &[c(pve, Some("PVE"), true, UP)]).compare, None);
        // PVE 用不了、香港代测：对照香港后面的（这里是中控）
        assert_eq!(decide(true, &[c(pve, Some("PVE"), true, OFF), c(hk, Some("香港"), true, UP)]).compare, cmp(None, "中控", "香港探测点"));
        // 中控代测、主来源是中控、标灰：没有
        assert_eq!(decide(true, &[c(pve, Some("PVE"), true, OFF)]).compare, None);
        assert_eq!(decide(true, &[]).compare, None);
        assert_eq!(decide(false, &[c(pve, Some("PVE"), true, OFF)]).compare, None);
    }

    #[test]
    fn label_sql_escapes_quotes() {
        let e = Effective { state: "probe", probe_point_id: None, label: "o'clock探测点".into(), primary_label: None, primary_issue: None, substitute: None, compare: None };
        assert_eq!(e.label_sql(), "'o''clock探测点'");
    }

    #[test]
    fn setting_json_roundtrip() {
        let s = Setting { order: vec![Uuid::now_v7(), Uuid::now_v7()], central: false };
        assert_eq!(Setting::from_json(&s.to_json()), s);
        assert_eq!((s.primary(), s.rank(s.order[1]), s.rank(Uuid::nil())), (Some(s.order[0]), Some(2), None));
        assert_eq!(Setting::from_json(&json!({})), Setting::default());
        assert_eq!(Setting::default().primary(), None);
    }
}
