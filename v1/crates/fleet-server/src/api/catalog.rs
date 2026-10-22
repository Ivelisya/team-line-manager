use crate::error::ApiJson;
use super::ok;
use crate::auth::Actor;
use crate::db::{audit, new_id};
use crate::error::{ApiError, ApiPath, ApiResult};
use crate::meta;
use crate::state::{AppState, Event};
use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::OffsetDateTime;
use uuid::Uuid;

// ── 团队 ──────────────────────────────────────────────────────────────────

#[derive(Serialize, sqlx::FromRow)]
pub struct Team {
    id: Uuid,
    name: String,
    sort_order: i32,
    node_count: i64,
}

pub async fn list_teams(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let v: Vec<Team> = sqlx::query_as(
        "select t.id, t.name, t.sort_order,
                (select count(*) from machine m where m.team_id = t.id and m.lifecycle <> 'retired') node_count
         from team t order by t.sort_order, t.name",
    )
    .fetch_all(&st.db)
    .await?;
    Ok(ok(v))
}

#[derive(Deserialize)]
pub struct TeamIn {
    name: Option<String>,
    sort_order: Option<i32>,
}

fn clean_name(s: &str, what: &str) -> ApiResult<String> {
    let s = s.trim();
    if s.is_empty() || s.chars().count() > 32 {
        return Err(ApiError::bad_request(format!("{what}不能为空且不超过 32 个字")));
    }
    Ok(s.to_string())
}

pub async fn create_team(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<TeamIn>) -> ApiResult<Json<Value>> {
    let name = clean_name(b.name.as_deref().unwrap_or(""), "团队名")?;
    let id = new_id();
    let mut tx = st.db.begin().await?;
    sqlx::query("insert into team (id, name, sort_order) values ($1, $2, $3)")
        .bind(id)
        .bind(&name)
        .bind(b.sort_order.unwrap_or(0))
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, &a.name, "team.create", "team", Some(id), json!({"name": name})).await?;
    tx.commit().await?;
    st.emit(Event::Catalog);
    Ok(ok(json!({"id": id})))
}

pub async fn patch_team(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<TeamIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let old: (String, i32) = sqlx::query_as("select name, sort_order from team where id = $1 for update")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let name = match &b.name {
        Some(n) => clean_name(n, "团队名")?,
        None => old.0.clone(),
    };
    sqlx::query("update team set name = $2, sort_order = $3 where id = $1")
        .bind(id)
        .bind(&name)
        .bind(b.sort_order.unwrap_or(old.1))
        .execute(&mut *tx)
        .await?;
    // 团队改名 → 该团队所有节点显示名变化 → 线路机备注同步
    let mut relays = vec![];
    if name != old.0 {
        let nodes: Vec<Uuid> = sqlx::query_scalar("select id from machine where team_id = $1 and lifecycle <> 'retired'")
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
        relays = crate::db::bump_relays_of_nodes(&mut tx, &nodes).await?;
    }
    audit(&mut tx, &a.name, "team.update", "team", Some(id),
        json!({"before": {"name": old.0, "sort_order": old.1}, "after": {"name": name, "sort_order": b.sort_order.unwrap_or(old.1)}})).await?;
    tx.commit().await?;
    for r in relays {
        st.relay_bumped(r);
    }
    st.emit(Event::Catalog);
    st.emit(Event::Machines);
    Ok(ok(json!({"id": id})))
}

/// 「还有 3 台机器（另有 2 台已退役）、1 台软路由」：删目录项前列出还有谁用着它；都没有时为 None
fn in_use(parts: &[(i64, &str)]) -> Option<String> {
    let v: Vec<String> = parts.iter().filter(|(n, _)| *n > 0).map(|(n, w)| format!("{n} {w}")).collect();
    (!v.is_empty()).then(|| v.join("、"))
}

/// 删团队：还有机器（含已退役的——可能恢复，外键也拦着）或软路由时拒绝；它自己那一行访问规则一起删（团队没有节点，规则不影响任何线路机）
pub async fn delete_team(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let name: String = sqlx::query_scalar("select name from team where id = $1 for update")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这个团队"))?;
    let (live, retired, routers): (i64, i64, i64) = sqlx::query_as(
        "select count(*) filter (where lifecycle <> 'retired'), count(*) filter (where lifecycle = 'retired'), (select count(*) from router where team_id = $1)
         from machine where team_id = $1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if let Some(u) = in_use(&[(live, "台机器"), (retired, "台已退役的机器"), (routers, "台软路由")]) {
        return Err(ApiError::conflict(format!("「{name}」还有 {u}：先把它们移到别的团队（已退役的先彻底删除）再删")));
    }
    sqlx::query("delete from policy where scope = 'team' and scope_id = $1").bind(id).execute(&mut *tx).await?;
    sqlx::query("delete from team where id = $1").bind(id).execute(&mut *tx).await?;
    audit(&mut tx, &a.name, "team.delete", "team", Some(id), json!({"name": name})).await?;
    tx.commit().await?;
    st.emit(Event::Catalog);
    st.emit(Event::Policies);
    Ok(ok(json!({"id": id})))
}

// ── 供应商 ────────────────────────────────────────────────────────────────

#[derive(Serialize, sqlx::FromRow)]
pub struct Provider {
    id: Uuid,
    name: String,
    code: String,
    panel_url: Option<String>,
    /// 账单时区；null = 跟随组织时区（fleet.toml 的 org_timezone）
    timezone: Option<String>,
    /// 流量额度的网卡计量：min / max / sum / out = 本周期入出取小 / 取大 / 相加 / 只算出站
    traffic_rule: String,
    /// 流量额度周期：month = 账单时区的自然月（1 号起），billing = 账单日（到期日几号）起。名下机器没单独设置重置日的跟随它
    traffic_reset: String,
    /// 到期前几天提醒（0–60）：名下机器没单独设置的跟随它
    remind_days: i32,
}

pub async fn list_providers(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let v: Vec<Provider> = sqlx::query_as("select id, name, code, panel_url, timezone, traffic_rule, traffic_reset, remind_days from provider order by name")
        .fetch_all(&st.db)
        .await?;
    Ok(ok(v))
}

/// 新建与 PATCH 共用：没传 = 不改；`panel_url` 传 null / 空串 = 清空；`timezone` null / 空串 = 跟随组织时区（存 null）
#[derive(Deserialize)]
pub struct ProviderIn {
    name: Option<String>,
    code: Option<String>,
    #[serde(default, deserialize_with = "super::some")]
    panel_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::some")]
    timezone: Option<Option<String>>,
    /// 没传 = 不改（新建为 meta::DEFAULT_TRAFFIC_RULE）
    traffic_rule: Option<String>,
    /// month / billing；没传 = 不改（新建为 meta::DEFAULT_TRAFFIC_RESET）
    traffic_reset: Option<String>,
    /// 0–60；没传 = 不改（新建为列默认值 7）
    remind_days: Option<i32>,
}

fn remind_days(v: Option<i32>) -> ApiResult<Option<i32>> {
    match v {
        Some(d) if !(0..=60).contains(&d) => Err(ApiError::bad_request("提醒天数要在 0–60 之间")),
        v => Ok(v),
    }
}

/// 只认 `table` 里的值（meta 的枚举）：报错列出全部可选值与中文名
fn one_of<'a>(v: &'a Option<String>, table: &[(&str, &str)], what: &str) -> ApiResult<Option<&'a str>> {
    match v.as_deref() {
        None => Ok(None),
        Some(r) if table.iter().any(|(k, _)| *k == r) => Ok(Some(r)),
        Some(r) => {
            let all = table.iter().map(|(k, l)| format!("{k}（{l}）")).collect::<Vec<_>>().join(" / ");
            Err(ApiError::bad_request(format!("{what}「{r}」不认识：只能是 {all}")))
        }
    }
}

fn traffic_reset(v: &Option<String>) -> ApiResult<Option<&str>> { one_of(v, meta::TRAFFIC_RESETS, "流量重置方式") }

fn traffic_rule(v: &Option<String>) -> ApiResult<Option<&str>> { one_of(v, meta::TRAFFIC_RULES, "流量计量规则") }

/// 空白当没填
fn blank(v: &Option<String>) -> Option<&str> { v.as_deref().map(str::trim).filter(|s| !s.is_empty()) }

/// 控制台地址：空白 = 不填；填了必须是 http(s) 地址（网页上做成链接，不能是 javascript: 之类）
fn panel_url(v: &Option<String>) -> ApiResult<Option<String>> {
    blank(v).map(|u| crate::notify::http_url(u, "控制台地址")).transpose().map_err(ApiError::bad_request)
}

pub async fn create_provider(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<ProviderIn>) -> ApiResult<Json<Value>> {
    let id = new_id();
    let name = clean_name(b.name.as_deref().unwrap_or(""), "供应商名")?;
    let code = clean_name(b.code.as_deref().unwrap_or(""), "供应商缩写")?;
    let panel = b.panel_url.as_ref().map(panel_url).transpose()?.flatten();
    let tz = b.timezone.as_ref().and_then(blank);
    let rule = traffic_rule(&b.traffic_rule)?.unwrap_or(meta::DEFAULT_TRAFFIC_RULE);
    let reset = traffic_reset(&b.traffic_reset)?.unwrap_or(meta::DEFAULT_TRAFFIC_RESET);
    let remind = remind_days(b.remind_days)?;
    let mut tx = st.db.begin().await?;
    // 名下机器的账单时区跟随它，按同一标准校验
    if let Some(tz) = tz {
        super::machines::check_tz(&mut tx, tz).await?;
    }
    sqlx::query("insert into provider (id, name, code, panel_url, traffic_rule, traffic_reset) values ($1, $2, $3, $4, $5, $6)")
        .bind(id)
        .bind(&name)
        .bind(&code)
        .bind(panel)
        .bind(rule)
        .bind(reset)
        .execute(&mut *tx)
        .await?;
    if let Some(tz) = tz {
        sqlx::query("update provider set timezone = $2 where id = $1").bind(id).bind(tz).execute(&mut *tx).await?;
    }
    if let Some(d) = remind {
        sqlx::query("update provider set remind_days = $2 where id = $1").bind(id).bind(d).execute(&mut *tx).await?;
    }
    audit(&mut tx, &a.name, "provider.create", "provider", Some(id), json!({"name": name, "code": code, "traffic_rule": rule, "traffic_reset": reset})).await?;
    tx.commit().await?;
    st.emit(Event::Catalog);
    Ok(ok(json!({"id": id})))
}

pub async fn patch_provider(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<ProviderIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let old: Provider = sqlx::query_as("select id, name, code, panel_url, timezone, traffic_rule, traffic_reset, remind_days from provider where id = $1 for update")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这个供应商"))?;
    let name = match &b.name {
        Some(n) => clean_name(n, "供应商名")?,
        None => old.name.clone(),
    };
    let code = match &b.code {
        Some(c) => clean_name(c, "供应商缩写")?,
        None => old.code.clone(),
    };
    // 网页表单每次都带上控制台地址：和库里一样就不重新校验（http(s) 校验之前存下的旧值不能让别的字段改不了）
    let panel_url = match &b.panel_url {
        Some(u) if blank(u) == blank(&old.panel_url) => old.panel_url.clone(),
        Some(u) => panel_url(u)?,
        None => old.panel_url.clone(),
    };
    let tz = match &b.timezone {
        Some(t) => blank(t).map(String::from),
        None => old.timezone.clone(),
    };
    let rule = traffic_rule(&b.traffic_rule)?.unwrap_or(old.traffic_rule.as_str()).to_string();
    let reset = traffic_reset(&b.traffic_reset)?.unwrap_or(old.traffic_reset.as_str()).to_string();
    let remind = remind_days(b.remind_days)?.unwrap_or(old.remind_days);
    if let Some(tz) = &tz {
        super::machines::check_tz(&mut tx, tz).await?;
    }
    sqlx::query("update provider set name = $2, code = $3, panel_url = $4, traffic_rule = $5, traffic_reset = $6, remind_days = $7 where id = $1")
        .bind(id)
        .bind(&name)
        .bind(&code)
        .bind(&panel_url)
        .bind(&rule)
        .bind(&reset)
        .bind(remind)
        .execute(&mut *tx)
        .await?;
    // 时区清空 = 跟随组织时区（null）。名下机器没单独设置账单时区 / 重置日的立即跟着变（订阅里不存模板的值）
    sqlx::query("update provider set timezone = $2 where id = $1").bind(id).bind(&tz).execute(&mut *tx).await?;
    audit(&mut tx, &a.name, "provider.update", "provider", Some(id),
        json!({"before": {"name": old.name, "code": old.code, "panel_url": old.panel_url, "timezone": old.timezone, "traffic_rule": old.traffic_rule, "traffic_reset": old.traffic_reset, "remind_days": old.remind_days},
               "after": {"name": name, "code": code, "panel_url": panel_url, "timezone": tz, "traffic_rule": rule, "traffic_reset": reset, "remind_days": remind}})).await?;
    tx.commit().await?;
    // 机器视图里的供应商名 / 缩写：Catalog 事件已让视图缓存和前端机器列表作废
    st.emit(Event::Catalog);
    Ok(ok(json!({"id": id})))
}

/// 删供应商：还有套餐（含已归档的）或机器（含已退役的）时拒绝
pub async fn delete_provider(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let name: String = sqlx::query_scalar("select name from provider where id = $1 for update")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这个供应商"))?;
    let (plans, machines): (i64, i64) = sqlx::query_as("select (select count(*) from plan where provider_id = $1), (select count(*) from machine where provider_id = $1)")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    if let Some(u) = in_use(&[(plans, "个套餐"), (machines, "台机器")]) {
        return Err(ApiError::conflict(format!("「{name}」还有 {u}（含已归档、已退役的）：先删掉或改到别的供应商再删")));
    }
    sqlx::query("delete from provider where id = $1").bind(id).execute(&mut *tx).await?;
    audit(&mut tx, &a.name, "provider.delete", "provider", Some(id), json!({"name": name})).await?;
    tx.commit().await?;
    st.emit(Event::Catalog);
    Ok(ok(json!({"id": id})))
}

// ── 套餐 ──────────────────────────────────────────────────────────────────

#[derive(Serialize, sqlx::FromRow)]
pub struct Plan {
    id: Uuid,
    provider_id: Uuid,
    name: String,
    cpu_cores: Option<i32>,
    mem_mb: Option<i32>,
    disk_gb: Option<i32>,
    bandwidth_mbps: Option<i32>,
    traffic_gb: Option<i32>,
    price_cents: i32,
    currency: String,
    cycle: String,
    notes: String,
    #[serde(with = "time::serde::rfc3339::option")]
    archived_at: Option<OffsetDateTime>,
    machine_count: i64,
}

pub async fn list_plans(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let v: Vec<Plan> = sqlx::query_as(
        "select p.id, p.provider_id, p.name, p.cpu_cores, p.mem_mb, p.disk_gb, p.bandwidth_mbps, p.traffic_gb,
                p.price_cents, p.currency, p.cycle::text cycle, p.notes, p.archived_at,
                (select count(*) from machine m where m.plan_id = p.id and m.lifecycle <> 'retired') machine_count
         from plan p order by p.archived_at nulls first, p.name",
    )
    .fetch_all(&st.db)
    .await?;
    Ok(ok(v))
}

#[derive(Deserialize)]
pub struct PlanIn {
    provider_id: Option<Uuid>,
    name: Option<String>,
    #[serde(default, deserialize_with = "super::some")]
    cpu_cores: Option<Option<i32>>,
    #[serde(default, deserialize_with = "super::some")]
    mem_mb: Option<Option<i32>>,
    #[serde(default, deserialize_with = "super::some")]
    disk_gb: Option<Option<i32>>,
    #[serde(default, deserialize_with = "super::some")]
    bandwidth_mbps: Option<Option<i32>>,
    #[serde(default, deserialize_with = "super::some")]
    traffic_gb: Option<Option<i32>>,
    price_cents: Option<i32>,
    currency: Option<String>,
    cycle: Option<String>,
    notes: Option<String>,
    archived: Option<bool>,
}

/// 套餐字段校验（新建与 PATCH 共用，只看传了的）：价格不为负，币种 3 个字母（转成大写，与库里的约束一致），周期四选一，备注不超过 2000 字
fn check_plan(b: &mut PlanIn) -> ApiResult<()> {
    if b.price_cents.is_some_and(|p| p < 0) {
        return Err(ApiError::bad_request("价格不能为负"));
    }
    if let Some(c) = &mut b.currency {
        *c = meta::currency(c).map_err(ApiError::bad_request)?;
    }
    if let Some(c) = &b.cycle {
        meta::check_cycle(c).map_err(ApiError::bad_request)?;
    }
    if b.notes.as_ref().is_some_and(|n| n.chars().count() > 2000) {
        return Err(ApiError::bad_request("备注不超过 2000 字"));
    }
    Ok(())
}

pub async fn create_plan(a: Actor, State(st): State<AppState>, ApiJson(mut b): ApiJson<PlanIn>) -> ApiResult<Json<Value>> {
    check_plan(&mut b)?;
    let id = new_id();
    let provider = b.provider_id.ok_or_else(|| ApiError::bad_request("缺少 provider_id"))?;
    let name = clean_name(b.name.as_deref().unwrap_or(""), "套餐名")?;
    let price = b.price_cents.ok_or_else(|| ApiError::bad_request("缺少 price_cents"))?;
    let mut tx = st.db.begin().await?;
    sqlx::query(
        "insert into plan (id, provider_id, name, cpu_cores, mem_mb, disk_gb, bandwidth_mbps, traffic_gb, price_cents, currency, cycle, notes)
         values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11::billing_cycle,$12)",
    )
    .bind(id)
    .bind(provider)
    .bind(&name)
    .bind(b.cpu_cores.flatten())
    .bind(b.mem_mb.flatten())
    .bind(b.disk_gb.flatten())
    .bind(b.bandwidth_mbps.flatten())
    .bind(b.traffic_gb.flatten())
    .bind(price)
    .bind(b.currency.as_deref().unwrap_or(meta::DEFAULT_CURRENCY))
    .bind(b.cycle.as_deref().unwrap_or(meta::DEFAULT_CYCLE))
    .bind(b.notes.as_deref().unwrap_or(""))
    .execute(&mut *tx)
    .await?;
    audit(&mut tx, &a.name, "plan.create", "plan", Some(id), json!({"name": name, "price_cents": price})).await?;
    tx.commit().await?;
    st.emit(Event::Catalog);
    Ok(ok(json!({"id": id})))
}

pub async fn patch_plan(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(mut b): ApiJson<PlanIn>) -> ApiResult<Json<Value>> {
    check_plan(&mut b)?;
    let mut tx = st.db.begin().await?;
    const ROW: &str = "select to_jsonb(p) - 'created_at' from plan p where id = $1";
    let before: Value = sqlx::query_scalar(&format!("{ROW} for update")).bind(id).fetch_one(&mut *tx).await?;
    // 逐字段更新：Option<Option<T>> 区分「没传」与「设为空」
    macro_rules! set {
        ($col:literal, $v:expr) => {
            if let Some(v) = $v {
                sqlx::query(concat!("update plan set ", $col, " = $2 where id = $1")).bind(id).bind(v).execute(&mut *tx).await?;
            }
        };
    }
    if let Some(n) = &b.name {
        let n = clean_name(n, "套餐名")?;
        set!("name", Some(n));
    }
    // 机器的供应商就是套餐的：有机器（含已退役，可能恢复）用着的套餐不能换供应商
    if let Some(pv) = b.provider_id {
        let n: i64 = sqlx::query_scalar("select count(*) from machine m join plan p on p.id = m.plan_id where p.id = $1 and p.provider_id <> $2")
            .bind(id)
            .bind(pv)
            .fetch_one(&mut *tx)
            .await?;
        if n > 0 {
            return Err(ApiError::conflict(format!("有 {n} 台机器用着这个套餐，不能改它的供应商：给新供应商另建套餐")));
        }
    }
    set!("provider_id", b.provider_id);
    set!("cpu_cores", b.cpu_cores);
    set!("mem_mb", b.mem_mb);
    set!("disk_gb", b.disk_gb);
    set!("bandwidth_mbps", b.bandwidth_mbps);
    set!("traffic_gb", b.traffic_gb);
    set!("price_cents", b.price_cents);
    set!("currency", b.currency.clone());
    set!("notes", b.notes.clone());
    if let Some(c) = &b.cycle {
        sqlx::query("update plan set cycle = $2::billing_cycle where id = $1").bind(id).bind(c).execute(&mut *tx).await?;
    }
    if let Some(ar) = b.archived {
        sqlx::query("update plan set archived_at = case when $2 then coalesce(archived_at, now()) else null end where id = $1")
            .bind(id)
            .bind(ar)
            .execute(&mut *tx)
            .await?;
    }
    // 审计只记改了的字段（同团队、供应商的 before / after）
    let after: Value = sqlx::query_scalar(ROW).bind(id).fetch_one(&mut *tx).await?;
    let (mut was, mut now) = (serde_json::Map::new(), serde_json::Map::new());
    if let (Value::Object(x), Value::Object(y)) = (&before, &after) {
        for (k, v) in y.iter().filter(|(k, v)| x.get(*k) != Some(*v)) {
            was.insert(k.clone(), x.get(k).cloned().unwrap_or(Value::Null));
            now.insert(k.clone(), v.clone());
        }
    }
    // 套餐名、币种（改后的）写在外层：摘要要写「归档套餐 X」「价格 59.00 → 69.00 USD」，只记了改动字段的 before / after 里不一定有
    audit(&mut tx, &a.name, "plan.update", "plan", Some(id), json!({"name": after["name"], "currency": after["currency"], "before": was, "after": now})).await?;
    tx.commit().await?;
    st.emit(Event::Catalog);
    Ok(ok(json!({"id": id})))
}

/// 删套餐：还有机器（含已退役的，可能恢复）用着时拒绝，用不着的套餐建议归档
pub async fn delete_plan(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let name: String = sqlx::query_scalar("select name from plan where id = $1 for update")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这个套餐"))?;
    let (live, retired): (i64, i64) = sqlx::query_as("select count(*) filter (where lifecycle <> 'retired'), count(*) filter (where lifecycle = 'retired') from machine where plan_id = $1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    if let Some(u) = in_use(&[(live, "台机器"), (retired, "台已退役的机器")]) {
        return Err(ApiError::conflict(format!("「{name}」还有 {u}用着：不再上架的可以归档")));
    }
    sqlx::query("delete from plan where id = $1").bind(id).execute(&mut *tx).await?;
    audit(&mut tx, &a.name, "plan.delete", "plan", Some(id), json!({"name": name})).await?;
    tx.commit().await?;
    st.emit(Event::Catalog);
    Ok(ok(json!({"id": id})))
}
