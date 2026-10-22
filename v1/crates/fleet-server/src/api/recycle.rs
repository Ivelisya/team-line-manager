//! 回收站接口：只读列表、单条（含快照）、旧面板退役机器导入。面板里的机器何时写进回收站见 recycle.rs（退役收尾、维护任务）。

use super::{ok, paged};
use crate::auth::Actor;
use crate::db::{audit, Args};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::state::AppState;
use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::net::IpAddr;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

/// 列表的一行（不含快照）
#[derive(Serialize, sqlx::FromRow)]
pub struct Item {
    id: i64,
    /// fleet = 面板里退役的；legacy = 从旧面板导入的
    source: String,
    source_id: String,
    machine_id: Option<Uuid>,
    kind: String,
    /// 退役前的名字（进过备用的是进备用前的）
    name: String,
    /// 回收时的名字（备用-03）；立即回收的同 name，导入的为 null
    last_name: Option<String>,
    team: Option<String>,
    ip: Option<String>,
    host_name: Option<String>,
    provider: Option<String>,
    plan: Option<String>,
    service_ref: Option<String>,
    price_cents: Option<i32>,
    currency: Option<String>,
    cycle: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    activated_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    retired_at: Option<OffsetDateTime>,
    due_date: Option<Date>,
    #[serde(with = "time::serde::rfc3339")]
    recycled_at: OffsetDateTime,
    reason: Option<String>,
    /// 入口业务流量合计（字节）；导入时没给为 null
    traffic_up: Option<i64>,
    traffic_down: Option<i64>,
    /// 网卡累计（字节）；没有网卡读数的（家宽代理、同机 IP 节点）为 null
    nic_rx: Option<i64>,
    nic_tx: Option<i64>,
    actor: String,
}

const COLS: &str = "id, source, source_id, machine_id, kind, name, last_name, team, host(ip) ip, host_name, provider, plan, service_ref, price_cents, currency, cycle,
    activated_at, retired_at, due_date, recycled_at, reason, traffic_up, traffic_down, nic_rx, nic_tx, actor";

#[derive(Deserialize)]
pub struct ListQ {
    /// 搜名字（含回收时的名字）、IP、团队、供应商、套餐、供应商编号、宿主机
    q: Option<String>,
    /// fleet | legacy
    source: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// 回收站列表：按回收时间倒序，`{items, total, next_offset}`
pub async fn list(_a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<ListQ>) -> ApiResult<Json<Value>> {
    let (limit, offset) = (q.limit.unwrap_or(100).clamp(1, 500), q.offset.unwrap_or(0).max(0));
    let source = match q.source.as_deref().map(str::trim) {
        None | Some("" | "all") => None,
        Some(s @ ("fleet" | "legacy")) => Some(s.to_string()),
        Some(_) => return Err(ApiError::bad_request("source 只能是 fleet 或 legacy")),
    };
    let key = q.q.as_deref().map(str::trim).filter(|k| !k.is_empty()).map(str::to_lowercase);
    // 条件只在给了时才拼上（列表与总数各拼一份）
    let cond = |a: &mut Args| {
        let mut w = String::from("true");
        if let Some(s) = &source {
            w += &format!(" and source = {}", a.add(s.clone()));
        }
        if let Some(k) = &key {
            w += &format!(" and strpos(lower(concat_ws(' ', name, last_name, team, host(ip), provider, plan, service_ref, host_name)), {}) > 0", a.add(k.clone()));
        }
        w
    };
    let mut a = Args::default();
    let sql = format!("select {COLS} from recycle_bin where {} order by recycled_at desc, id desc limit {} offset {}", cond(&mut a), a.add(limit), a.add(offset));
    let items: Vec<Item> = sqlx::query_as_with(&sql, a.0).fetch_all(&st.db).await?;
    let mut a = Args::default();
    let sql = format!("select count(*) from recycle_bin where {}", cond(&mut a));
    let total: i64 = sqlx::query_scalar_with(&sql, a.0).fetch_one(&st.db).await?;
    Ok(paged(items, total, offset))
}

/// 单条，带回收时的快照（`snapshot`）
pub async fn get_one(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<i64>) -> ApiResult<Json<Value>> {
    let item: Item = sqlx::query_as(&format!("select {COLS} from recycle_bin where id = $1"))
        .bind(id)
        .fetch_optional(&st.db)
        .await?
        .ok_or_else(|| ApiError::not_found("回收站里没有这一条"))?;
    let snap: Value = sqlx::query_scalar("select snapshot from recycle_bin where id = $1").bind(id).fetch_one(&st.db).await?;
    let mut v = serde_json::to_value(item).map_err(|e| ApiError::internal(e.to_string()))?;
    v["snapshot"] = snap;
    Ok(ok(v))
}

/// 导入的一条：旧面板资产（运维从旧库只读整理出来，用户确认名单后导入）。必填 source_id、kind、name，其余可以没有
#[derive(Deserialize)]
pub struct ImportItem {
    /// 旧面板资产的 uuid；没有 uuid 的用「供应商:service_id」
    source_id: String,
    kind: String,
    name: String,
    team: Option<String>,
    ip: Option<String>,
    host_name: Option<String>,
    provider: Option<String>,
    plan: Option<String>,
    service_ref: Option<String>,
    price_cents: Option<i32>,
    currency: Option<String>,
    cycle: Option<String>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    activated_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    retired_at: Option<OffsetDateTime>,
    due_date: Option<Date>,
    reason: Option<String>,
    traffic_up: Option<i64>,
    traffic_down: Option<i64>,
    nic_rx: Option<i64>,
    nic_tx: Option<i64>,
    /// 旧面板的原始记录
    #[serde(default)]
    snapshot: Value,
}

#[derive(Deserialize)]
pub struct ImportIn {
    items: Vec<ImportItem>,
    /// false（默认）= 只预演：返回会导入、会跳过哪些，不写
    #[serde(default)]
    confirm: bool,
}

/// 一次最多导入这么多条
const IMPORT_MAX: usize = 1000;

/// 旧面板的退役机器导入回收站（source 固定为 legacy）。幂等键 (source, source_id)：导入过的跳过，重复导入不出错。
/// 防止导错：IP 或（供应商, 供应商编号）对上面板里还没回收的机器的跳过，说明是哪台。默认只预演，`confirm: true` 才写
pub async fn import(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<ImportIn>) -> ApiResult<Json<Value>> {
    if b.items.is_empty() || b.items.len() > IMPORT_MAX {
        return Err(ApiError::bad_request(format!("items 要有 1–{IMPORT_MAX} 条")));
    }
    for (i, x) in b.items.iter().enumerate() {
        let bad = |m: &str| ApiError::bad_request(format!("第 {} 条（{}）：{m}", i + 1, x.name));
        if x.source_id.trim().is_empty() || x.source_id.chars().count() > 200 || x.name.trim().is_empty() {
            return Err(bad("source_id、name 不能为空（source_id 最多 200 个字符）"));
        }
        if !matches!(x.kind.as_str(), "vps" | "proxy" | "relay") {
            return Err(bad("kind 只能是 vps、proxy、relay"));
        }
        if x.ip.as_deref().is_some_and(|ip| ip.trim().parse::<IpAddr>().is_err()) {
            return Err(bad("ip 不是合法的 IP 地址"));
        }
    }
    let mut tx = st.db.begin().await?;
    let (mut take, mut skipped) = (vec![], vec![]);
    let mut seen = HashSet::new();
    for x in &b.items {
        let sid = x.source_id.trim();
        let skip = |why: String| json!({"source_id": sid, "name": x.name, "why": why});
        if !seen.insert(sid.to_string()) {
            skipped.push(skip("同一批里重复".into()));
            continue;
        }
        let had: Option<i64> = sqlx::query_scalar("select id from recycle_bin where source = 'legacy' and source_id = $1").bind(sid).fetch_optional(&mut *tx).await?;
        if let Some(id) = had {
            skipped.push(skip(format!("已经导入过（回收站 #{id}）")));
            continue;
        }
        let live: Option<(String, String)> = sqlx::query_as(
            "select display_name(m.kind::text, m.label, t.name, m.number), m.lifecycle::text from machine m left join team t on t.id = m.team_id
               left join plan pl on pl.id = m.plan_id left join provider p on p.id = coalesce(pl.provider_id, m.provider_id)
             where m.recycled_at is null
               and (m.primary_ip = $1::inet
                    or ($2::text <> '' and $3::text <> '' and m.service_ref = $3 and (lower(p.name) = lower($2) or lower(p.code) = lower($2))))
             limit 1",
        )
        .bind(x.ip.as_deref().map(str::trim))
        .bind(x.provider.as_deref().map(str::trim).unwrap_or(""))
        .bind(x.service_ref.as_deref().map(str::trim).unwrap_or(""))
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((n, lc)) = live {
            let why = if lc == "active" { format!("是在用机器 {n}") } else { format!("是面板里还没回收的机器 {n}（{}）", crate::meta::lifecycle_label(&lc)) };
            skipped.push(skip(why));
            continue;
        }
        take.push(x);
    }
    let mut inserted = vec![];
    for x in &take {
        let sid = x.source_id.trim();
        let id: Option<i64> = if b.confirm {
            sqlx::query_scalar(
                "insert into recycle_bin (source, source_id, kind, name, team, ip, host_name, provider, plan, service_ref, price_cents, currency, cycle,
                     activated_at, retired_at, due_date, reason, traffic_up, traffic_down, nic_rx, nic_tx, actor, snapshot)
                 values ('legacy', $1, $2, $3, $4, $5::inet, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22)
                 on conflict (source, source_id) do nothing returning id",
            )
            .bind(sid)
            .bind(&x.kind)
            .bind(x.name.trim())
            .bind(&x.team)
            .bind(x.ip.as_deref().map(str::trim))
            .bind(&x.host_name)
            .bind(&x.provider)
            .bind(&x.plan)
            .bind(&x.service_ref)
            .bind(x.price_cents)
            .bind(&x.currency)
            .bind(&x.cycle)
            .bind(x.activated_at)
            .bind(x.retired_at)
            .bind(x.due_date)
            .bind(&x.reason)
            .bind(x.traffic_up)
            .bind(x.traffic_down)
            .bind(x.nic_rx)
            .bind(x.nic_tx)
            .bind(&a.name)
            .bind(&x.snapshot)
            .fetch_optional(&mut *tx)
            .await?
        } else {
            None
        };
        inserted.push(json!({"source_id": sid, "name": x.name.trim(), "id": id}));
    }
    if b.confirm {
        let names: Vec<&Value> = inserted.iter().take(3).map(|x| &x["name"]).collect();
        audit(&mut tx, &a.name, "recycle.import", "recycle_bin", None, json!({"inserted": inserted.len(), "skipped": skipped.len(), "names": names})).await?;
        tx.commit().await?;
    }
    Ok(ok(json!({"dry_run": !b.confirm, "inserted": inserted, "skipped": skipped})))
}
