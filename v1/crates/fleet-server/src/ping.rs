//! 三网 PING：线路机往外测线路质量（延迟、丢包），与探测点（probe_points/，客户连不连得上入口）分工——
//! 探测点管可用性与告警；这里只看线路质量，先只展示、不开告警（建议的告警口径写在 DESIGN.md「三网 PING」）。
//!
//! - 目标：全局 8 个槽位（设置「三网 PING」，app_setting 的 `ping_targets`），每个槽位有名称与目标（`host[:port]`、`[IPv6]:port`）；
//!   默认 武汉电信 / 联通 / 移动（zstaticcdn 的测速地址）、BGP（留空，占位提示 ip.zstaticcdn.com）、Node 1–4（留空）。目标留空的槽位不测。
//!   每台线路机可以单独覆盖某几个槽位与方式（`relay.ping_override`，迁移 0048）。方式：tcp（默认，建连，不写端口时连 80）/ icmp。
//! - 下发：随线路机的期望状态（`DesiredState::ping`，entries.rs 调 [`config_for`]）。改全局时全部在用线路机的代数 +1，改覆盖时那一台 +1。
//!   旧 agent 不认识这个字段、照常应用，只是不测。
//! - 测：线路机上的 fleet-agent 每 60 秒一轮，每个目标发 5 次（TCP 建连 / 系统 ping），记通了的那几次的延迟中位数与丢了几次，随上报带回
//!   （`Report::ping`，agent_api 调 [`ingest`]）。节点不测：客户不直连节点。
//! - 存：`ping_result` 每台每目标每分钟一行，保留 [retention] ping_days（默认 7 天）；`ping_hourly` 小时汇总，保留 ping_hourly_days（默认 90 天）。本模块的后台循环每 5 分钟重算最近 7 小时的汇总、
//!   每小时清一次过期的。
//! - 看：`/ping-beats`（卡片：每个目标一行，近 1 小时 20 格的延迟与丢包）、`/machines/{id}/ping`（这台的目标、覆盖、当前读数）、
//!   `/machines/{id}/ping-series`（抽屉里的曲线）。

use crate::db::{audit, Tx};
use crate::error::{ApiError, ApiResult};
use crate::state::{AppState, Event};
use fleet_proto::{split_target, PingConfig, PingSample, PingTarget};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

pub const SLOTS: usize = 8;
/// 每轮间隔、每个目标发几次
pub const INTERVAL_S: u32 = 60;
pub const COUNT: u32 = 5;
const KEY: &str = "ping_targets";
pub const MODES: [&str; 2] = ["tcp", "icmp"];
const MAX_NAME: usize = 20;

/// 一个槽位：名称与目标（目标空 = 不测）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slot {
    pub name: String,
    #[serde(default)]
    pub target: String,
}

/// 全局设置：方式与 8 个槽位
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Global {
    pub mode: String,
    pub slots: Vec<Slot>,
}

impl Default for Global {
    fn default() -> Self {
        let s = |n: &str, t: &str| Slot { name: n.into(), target: t.into() };
        Self {
            mode: "tcp".into(),
            slots: vec![
                s("武汉电信", "hb-ct-v4.ip.zstaticcdn.com"),
                s("武汉联通", "hb-cu-v4.ip.zstaticcdn.com"),
                s("武汉移动", "hb-cm-v4.ip.zstaticcdn.com"),
                s("BGP", ""),
                s("Node 1", ""),
                s("Node 2", ""),
                s("Node 3", ""),
                s("Node 4", ""),
            ],
        }
    }
}

/// 一台线路机的覆盖：`mode` 为 None 跟随全局；`slots[i]` 为 None 跟随全局的第 i 个（不足 8 个的按 None）
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Override {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub slots: Vec<Option<Slot>>,
}

impl Override {
    pub fn is_empty(&self) -> bool { self.mode.is_none() && self.slots.iter().all(Option::is_none) }
}

/// 目标：`host`、`host:port`、`[IPv6]:port`、裸 IPv6；主机只许字母数字 . - :（IPv6），端口 1–65535。空串 = 不测
pub fn check_target(t: &str) -> Result<String, String> {
    let t = t.trim();
    if t.is_empty() {
        return Ok(String::new());
    }
    let bad = || format!("目标「{t}」不对：写 host、host:port、[IPv6]:port");
    let (h, _) = split_target(t).ok_or_else(bad)?;
    if h.is_empty() || h.len() > 253 || h.starts_with('-') || !h.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':')) {
        return Err(bad());
    }
    Ok(t.to_string())
}

fn check_mode(m: &str) -> Result<String, String> {
    let m = m.trim().to_lowercase();
    if MODES.contains(&m.as_str()) { Ok(m) } else { Err(format!("方式只能是 tcp 或 icmp（给的是 {m}）")) }
}

fn check_slot(i: usize, s: &Slot) -> Result<Slot, String> {
    let name = s.name.trim();
    let name = if name.is_empty() { format!("Node {}", i.saturating_sub(3)) } else { name.to_string() };
    if name.chars().count() > MAX_NAME || name.chars().any(char::is_control) {
        return Err(format!("第 {} 个槽位的名称最长 {MAX_NAME} 个字", i + 1));
    }
    Ok(Slot { name, target: check_target(&s.target)? })
}

pub fn check_global(g: &Global) -> Result<Global, String> {
    if g.slots.len() != SLOTS {
        return Err(format!("要正好 {SLOTS} 个槽位"));
    }
    Ok(Global { mode: check_mode(&g.mode)?, slots: g.slots.iter().enumerate().map(|(i, s)| check_slot(i, s)).collect::<Result<_, _>>()? })
}

pub fn check_override(o: &Override) -> Result<Override, String> {
    if o.slots.len() > SLOTS {
        return Err(format!("最多 {SLOTS} 个槽位"));
    }
    let mut slots: Vec<Option<Slot>> = o.slots.iter().enumerate().map(|(i, s)| s.as_ref().map(|s| check_slot(i, s)).transpose()).collect::<Result<_, _>>()?;
    slots.resize(SLOTS, None);
    Ok(Override { mode: o.mode.as_deref().map(check_mode).transpose()?, slots })
}

/// 一个槽位合成之后：(名称, 目标, 来源 global / override)
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Merged {
    pub slot: u8,
    pub name: String,
    pub target: String,
    pub source: &'static str,
}

/// 全局 + 覆盖 → (方式, 8 个槽位)
pub fn merge(g: &Global, o: Option<&Override>) -> (String, Vec<Merged>) {
    let mode = o.and_then(|o| o.mode.clone()).unwrap_or_else(|| g.mode.clone());
    let slots = g
        .slots
        .iter()
        .enumerate()
        .map(|(i, s)| match o.and_then(|o| o.slots.get(i)).and_then(Option::as_ref) {
            Some(x) => Merged { slot: i as u8, name: x.name.clone(), target: x.target.clone(), source: "override" },
            None => Merged { slot: i as u8, name: s.name.clone(), target: s.target.clone(), source: "global" },
        })
        .collect();
    (mode, slots)
}

/// 下发给 agent 的：只含有目标的槽位；一个都没有为 None
pub fn to_config(mode: &str, slots: &[Merged]) -> Option<PingConfig> {
    let targets: Vec<PingTarget> = slots.iter().filter(|s| !s.target.is_empty()).map(|s| PingTarget { slot: s.slot, name: s.name.clone(), target: s.target.clone() }).collect();
    (!targets.is_empty()).then(|| PingConfig { interval_s: INTERVAL_S, count: COUNT, mode: mode.to_string(), targets })
}

pub async fn global(db: &sqlx::PgPool) -> sqlx::Result<Global> {
    let v: Option<Value> = sqlx::query_scalar("select value from app_setting where key = $1").bind(KEY).fetch_optional(db).await?;
    Ok(v.and_then(|v| serde_json::from_value::<Global>(v).ok()).and_then(|g| check_global(&g).ok()).unwrap_or_default())
}

async fn override_of(db: &sqlx::PgPool, relay: Uuid) -> sqlx::Result<Option<Override>> {
    let v: Option<Option<Value>> = sqlx::query_scalar("select ping_override from relay where machine_id = $1").bind(relay).fetch_optional(db).await?;
    Ok(v.flatten().and_then(|v| serde_json::from_value::<Override>(v).ok()).and_then(|o| check_override(&o).ok()))
}

/// 一台线路机此刻的目标（期望状态里的 `ping`）
pub async fn config_for(db: &sqlx::PgPool, relay: Uuid) -> sqlx::Result<Option<PingConfig>> {
    let (g, o) = (global(db).await?, override_of(db, relay).await?);
    let (mode, slots) = merge(&g, o.as_ref());
    Ok(to_config(&mode, &slots))
}

/// 在用线路机的代数 +1（目标变了，要下发）。返回涉及的线路机
async fn bump_relays(tx: &mut Tx<'_>, only: Option<Uuid>) -> sqlx::Result<Vec<Uuid>> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "select r.machine_id from relay r join machine m on m.id = r.machine_id
         where m.lifecycle = 'active' and ($1::uuid is null or r.machine_id = $1) order by r.machine_id",
    )
    .bind(only)
    .fetch_all(&mut **tx)
    .await?;
    for id in &ids {
        crate::db::bump_relay(tx, *id).await?;
    }
    Ok(ids)
}

/// 改全局：写库、审计、全部在用线路机代数 +1。返回改之前的
pub async fn save_global(st: &AppState, actor: &str, new: Global) -> ApiResult<Global> {
    let new = check_global(&new).map_err(ApiError::bad_request)?;
    let mut tx = st.db.begin().await?;
    sqlx::query("select pg_advisory_xact_lock(hashtext('app_setting.ping_targets'))").execute(&mut *tx).await?;
    let old: Option<Value> = sqlx::query_scalar("select value from app_setting where key = $1").bind(KEY).fetch_optional(&mut *tx).await?;
    let old = old.and_then(|v| serde_json::from_value::<Global>(v).ok()).unwrap_or_default();
    if old == new {
        return Ok(old);
    }
    sqlx::query(
        "insert into app_setting (key, value, updated_by) values ($1, $2, $3)
         on conflict (key) do update set value = excluded.value, updated_at = now(), updated_by = excluded.updated_by",
    )
    .bind(KEY)
    .bind(json!(new))
    .bind(actor)
    .execute(&mut *tx)
    .await?;
    audit(&mut tx, actor, "settings.ping_targets", "setting", None, json!({"from": old, "to": new})).await?;
    let relays = bump_relays(&mut tx, None).await?;
    tx.commit().await?;
    for r in relays {
        st.relay_bumped(r);
    }
    st.emit(Event::Settings);
    Ok(old)
}

/// 改一台线路机的覆盖（None / 空的 = 全部跟随全局）：写库、审计、这台代数 +1
pub async fn save_override(st: &AppState, actor: &str, relay: Uuid, new: Option<Override>) -> ApiResult<()> {
    let new = new.map(|o| check_override(&o)).transpose().map_err(ApiError::bad_request)?.filter(|o| !o.is_empty());
    let mut tx = st.db.begin().await?;
    let cur: Option<(Option<Value>, String, String)> = sqlx::query_as(
        "select r.ping_override, m.lifecycle::text, display_name(m.kind::text, m.label, t.name, m.number)
         from relay r join machine m on m.id = r.machine_id left join team t on t.id = m.team_id where r.machine_id = $1 for update of r",
    )
    .bind(relay)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((old, lc, name)) = cur else { return Err(ApiError::not_found("没有这台线路机（三网 PING 只在线路机上测）")) };
    if lc == "retired" {
        return Err(ApiError::conflict("已退役的线路机"));
    }
    let old: Option<Override> = old.and_then(|v| serde_json::from_value(v).ok());
    if old == new {
        return Ok(());
    }
    sqlx::query("update relay set ping_override = $2 where machine_id = $1").bind(relay).bind(new.as_ref().map(|o| json!(o))).execute(&mut *tx).await?;
    audit(&mut tx, actor, "relay.ping_override", "machine", Some(relay), json!({"name": name, "from": old, "to": new})).await?;
    let bumped = bump_relays(&mut tx, Some(relay)).await?;
    tx.commit().await?;
    for r in bumped {
        st.relay_bumped(r);
    }
    st.emit(Event::Machine { id: relay });
    Ok(())
}

/// 上报里的三网 PING 入库（agent_api 的事务里调）：时间戳在 [lo, hi]（Unix 毫秒）之外的、槽位不对的丢掉；同一时刻重报的不重复写
pub(crate) async fn ingest(tx: &mut Tx<'_>, relay: Uuid, samples: &[PingSample], lo: i64, hi: i64) -> sqlx::Result<()> {
    let ok: Vec<(&PingSample, OffsetDateTime)> = samples
        .iter()
        .take(2000)
        .filter(|s| (s.slot as usize) < SLOTS && s.ts_ms >= lo && s.ts_ms <= hi && s.sent > 0 && s.lost <= s.sent)
        .filter_map(|s| OffsetDateTime::from_unix_timestamp_nanos(s.ts_ms as i128 * 1_000_000).ok().map(|t| (s, t)))
        .collect();
    if ok.is_empty() {
        return Ok(());
    }
    let clip = |s: &str, n: usize| s.replace('\0', "").chars().take(n).collect::<String>();
    sqlx::query(
        "insert into ping_result (relay_id, slot, ts, target, mode, sent, lost, rtt_ms)
         select $1, * from unnest($2::smallint[], $3::timestamptz[], $4::text[], $5::text[], $6::smallint[], $7::smallint[], $8::real[])
         on conflict do nothing",
    )
    .bind(relay)
    .bind(ok.iter().map(|(s, _)| s.slot as i16).collect::<Vec<_>>())
    .bind(ok.iter().map(|(_, t)| *t).collect::<Vec<_>>())
    .bind(ok.iter().map(|(s, _)| clip(&s.target, 260)).collect::<Vec<_>>())
    .bind(ok.iter().map(|(s, _)| clip(&s.mode, 8)).collect::<Vec<_>>())
    .bind(ok.iter().map(|(s, _)| s.sent.min(i16::MAX as u16) as i16).collect::<Vec<_>>())
    .bind(ok.iter().map(|(s, _)| s.lost.min(i16::MAX as u16) as i16).collect::<Vec<_>>())
    .bind(ok.iter().map(|(s, _)| s.rtt_ms.filter(|v| v.is_finite() && *v >= 0.0 && *v < 60_000.0)).collect::<Vec<_>>())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// 后台：每 5 分钟重算最近 7 小时的小时汇总（覆盖 agent 离线补传），每小时清一次过期的
pub async fn run_forever(st: AppState) {
    let mut t = tokio::time::interval(Duration::from_secs(300));
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut n = 0u64;
    loop {
        t.tick().await;
        if let Err(e) = rollup(&st.db).await {
            tracing::error!("三网 PING 小时汇总失败：{e:#}");
        }
        if n.is_multiple_of(12) {
            if let Err(e) = cleanup(&st.db, &st.cfg.retention).await {
                tracing::error!("三网 PING 保留期清理失败：{e:#}");
            }
        }
        n += 1;
    }
}

pub async fn rollup(db: &sqlx::PgPool) -> sqlx::Result<u64> {
    Ok(sqlx::query(
        "insert into ping_hourly (relay_id, slot, hour, target, sent, lost, rtt_avg, rtt_max)
         select relay_id, slot, date_trunc('hour', ts), (array_agg(target order by ts desc))[1], sum(sent), sum(lost), avg(rtt_ms), max(rtt_ms)
         from ping_result where ts >= date_trunc('hour', now()) - interval '7 hours'
         group by relay_id, slot, date_trunc('hour', ts)
         on conflict (relay_id, slot, hour) do update set target = excluded.target, sent = excluded.sent, lost = excluded.lost,
           rtt_avg = excluded.rtt_avg, rtt_max = excluded.rtt_max",
    )
    .execute(db)
    .await?
    .rows_affected())
}

pub async fn cleanup(db: &sqlx::PgPool, r: &crate::config::Retention) -> sqlx::Result<()> {
    sqlx::query("delete from ping_result where ts < now() - make_interval(days => $1)").bind(r.ping_days as i32).execute(db).await?;
    sqlx::query("delete from ping_hourly where hour < now() - make_interval(days => $1)").bind(r.ping_hourly_days as i32).execute(db).await?;
    Ok(())
}

/// 卡片：每台在用线路机、每个在测的目标（按此刻合成的槽位，目标改了之后旧目标的数据不算）近 1 小时 20 格（3 分钟一格，墙钟对齐，同 /probe-beats），
/// 每格 `[延迟中位数 ms | null, 丢包 %| null]`（没有数据为 null），另给最近一次的读数 `cur_ms`、`cur_loss`（%）与时间。
/// `{<线路机 id>: [{slot, name, target, mode, cur_ms, cur_loss, cur_ts, cells}]}`；没有目标的线路机不出现
pub async fn beats(db: &sqlx::PgPool) -> sqlx::Result<Value> {
    const BUCKETS: usize = 20;
    let g = global(db).await?;
    let relays: Vec<(Uuid, Option<Value>)> = sqlx::query_as(
        "select r.machine_id, r.ping_override from relay r join machine m on m.id = r.machine_id where m.lifecycle = 'active' and m.kind = 'relay'",
    )
    .fetch_all(db)
    .await?;
    let rows: Vec<(Uuid, i16, String, i32, Option<f64>, i64, i64)> = sqlx::query_as(
        "with w as (select date_bin('3 minutes', now(), timestamptz 'epoch') - interval '57 minutes' s)
         select p.relay_id, p.slot, p.target, floor(extract(epoch from p.ts - w.s) / 180)::int,
                (percentile_cont(0.5) within group (order by p.rtt_ms))::float8, sum(p.sent)::bigint, sum(p.lost)::bigint
         from ping_result p, w where p.ts >= w.s and p.ts < w.s + interval '60 minutes'
         group by 1, 2, 3, 4",
    )
    .fetch_all(db)
    .await?;
    let last: Vec<(Uuid, i16, String, String, Option<f32>, i16, i16, OffsetDateTime)> = sqlx::query_as(
        "select distinct on (relay_id, slot) relay_id, slot, target, mode, rtt_ms, sent, lost, ts from ping_result
         where ts > now() - interval '15 minutes' order by relay_id, slot, ts desc",
    )
    .fetch_all(db)
    .await?;
    let mut cells: HashMap<(Uuid, i16, String), Vec<Value>> = HashMap::new();
    for (r, s, t, i, ms, sent, lost) in rows {
        if let Some(c) = cells.entry((r, s, t)).or_insert_with(|| vec![json!([null, null]); BUCKETS]).get_mut(i as usize) {
            *c = json!([ms.map(|m| (m * 10.0).round() / 10.0), (sent > 0).then(|| (lost as f64 * 1000.0 / sent as f64).round() / 10.0)]);
        }
    }
    let cur: HashMap<(Uuid, i16, String), (String, Option<f32>, i16, i16, OffsetDateTime)> =
        last.into_iter().map(|(r, s, t, m, ms, sent, lost, ts)| ((r, s, t), (m, ms, sent, lost, ts))).collect();
    let mut out = serde_json::Map::new();
    for (id, ov) in relays {
        let o: Option<Override> = ov.and_then(|v| serde_json::from_value(v).ok());
        let (mode, slots) = merge(&g, o.as_ref());
        let list: Vec<Value> = slots
            .iter()
            .filter(|s| !s.target.is_empty())
            .map(|s| {
                let k = (id, s.slot as i16, s.target.clone());
                let c = cur.get(&k);
                json!({
                    "slot": s.slot, "name": s.name, "target": s.target, "mode": c.map(|c| c.0.clone()).unwrap_or_else(|| mode.clone()),
                    "cur_ms": c.and_then(|c| c.1), "cur_loss": c.filter(|c| c.2 > 0).map(|c| (c.3 as f64 * 1000.0 / c.2 as f64).round() / 10.0),
                    "cur_ts": c.and_then(|c| c.4.format(&time::format_description::well_known::Rfc3339).ok()),
                    "cells": cells.remove(&k).unwrap_or_else(|| vec![json!([null, null]); BUCKETS]),
                })
            })
            .collect();
        if !list.is_empty() {
            out.insert(id.to_string(), Value::Array(list));
        }
    }
    Ok(Value::Object(out))
}

/// 一台线路机：全局、覆盖、合成后的槽位（含来源），与每个槽位的当前读数（近 15 分钟最近一次）
pub async fn machine_view(db: &sqlx::PgPool, relay: Uuid) -> ApiResult<Value> {
    let kind: Option<String> = sqlx::query_scalar("select kind::text from machine where id = $1").bind(relay).fetch_optional(db).await?;
    if kind.as_deref() != Some("relay") {
        return Err(ApiError::not_found("没有这台线路机（三网 PING 只在线路机上测）"));
    }
    let (g, o) = (global(db).await?, override_of(db, relay).await?);
    let (mode, slots) = merge(&g, o.as_ref());
    let beats = beats(db).await?;
    Ok(json!({"mode": mode, "slots": slots, "override": o, "global": g, "current": beats.get(relay.to_string()).cloned().unwrap_or(json!([]))}))
}

/// 抽屉里的曲线：range 1h / 6h / 24h 用每分钟的原始点，7d / 30d 用小时汇总。
/// `{range, step_s, series: [{slot, name, target, points: [{ts, rtt_ms, loss_pct}]}]}`（按此刻在测的槽位；目标改过的只给新目标的点）
pub async fn series(db: &sqlx::PgPool, relay: Uuid, range: &str) -> ApiResult<Value> {
    let (back, step) = match range {
        "1h" => (time::Duration::hours(1), 60),
        "6h" => (time::Duration::hours(6), 60),
        "24h" => (time::Duration::hours(24), 60),
        "7d" => (time::Duration::days(7), 3600),
        "30d" => (time::Duration::days(30), 3600),
        _ => return Err(ApiError::bad_request("range 只能是 1h/6h/24h/7d/30d")),
    };
    let view = machine_view(db, relay).await?;
    let since = OffsetDateTime::now_utc() - back;
    let sql = if step == 60 {
        "select slot, target, ts, rtt_ms::float8, (100.0 * lost / nullif(sent, 0))::float8 from ping_result where relay_id = $1 and ts >= $2 order by ts"
    } else {
        "select slot, target, hour, rtt_avg::float8, (100.0 * lost / nullif(sent, 0))::float8 from ping_hourly where relay_id = $1 and hour >= $2 order by hour"
    };
    let rows: Vec<(i16, String, OffsetDateTime, Option<f64>, Option<f64>)> = sqlx::query_as(sql).bind(relay).bind(since).fetch_all(db).await?;
    let slots: Vec<Merged> = serde_json::from_value::<Vec<Value>>(view["slots"].clone())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| {
            Some(Merged { slot: v["slot"].as_u64()? as u8, name: v["name"].as_str()?.into(), target: v["target"].as_str()?.into(), source: "global" })
        })
        .filter(|s| !s.target.is_empty())
        .collect();
    let series: Vec<Value> = slots
        .iter()
        .map(|s| {
            let pts: Vec<Value> = rows
                .iter()
                .filter(|r| r.0 as u8 == s.slot && r.1 == s.target)
                .map(|r| json!({"ts": r.2.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(), "rtt_ms": r.3, "loss_pct": r.4}))
                .collect();
            json!({"slot": s.slot, "name": s.name, "target": s.target, "points": pts})
        })
        .collect();
    Ok(json!({"range": range, "step_s": step, "series": series}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_validation() {
        let g = Global::default();
        assert_eq!(check_global(&g).unwrap(), g);
        assert_eq!(to_config("tcp", &merge(&g, None).1).unwrap().targets.len(), 3, "默认只测三网，BGP 与 Node 1–4 留空");
        for ok in ["1.2.3.4", "a.example.com:443", "[2001:db8::1]:80", "2001:db8::1", "[2001:db8::1]"] {
            assert!(check_target(ok).is_ok(), "{ok}");
        }
        for bad in ["a b", "-x", "host:0", "host:99999", "[::1", "a;b", "h:p"] {
            assert!(check_target(bad).is_err(), "{bad}");
        }
        assert!(check_mode("udp").is_err() && check_mode(" ICMP ").unwrap() == "icmp");
        let mut short = g.clone();
        short.slots.pop();
        assert!(check_global(&short).is_err());
    }

    #[test]
    fn override_merges_per_slot() {
        let g = Global::default();
        let o = check_override(&Override { mode: Some("icmp".into()), slots: vec![None, Some(Slot { name: "联通 9929".into(), target: "1.1.1.1:443".into() })] }).unwrap();
        assert_eq!(o.slots.len(), SLOTS);
        let (mode, s) = merge(&g, Some(&o));
        assert_eq!((mode.as_str(), s[0].source, s[1].source, s[1].name.as_str()), ("icmp", "global", "override", "联通 9929"));
        // 覆盖成空目标 = 这台不测这个槽位
        let o2 = Override { mode: None, slots: vec![Some(Slot { name: "武汉电信".into(), target: String::new() })] };
        let c = to_config("tcp", &merge(&g, Some(&check_override(&o2).unwrap())).1).unwrap();
        assert_eq!(c.targets.iter().map(|t| t.slot).collect::<Vec<_>>(), [1, 2]);
        assert!(Override::default().is_empty() && !o.is_empty());
    }
}
