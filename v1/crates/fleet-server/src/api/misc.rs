use crate::error::ApiJson;
use super::{ok, paged};
use crate::auth::Actor;
use crate::db::{audit as audit_log, new_id};
use crate::error::{ApiError, ApiResult};
use crate::health::Status;
use crate::state::{AppState, Event};
use crate::views;
use crate::crypto::sha256;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::Json;
use axum_extra::extract::cookie::CookieJar;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;
use time::OffsetDateTime;
use tokio::sync::broadcast::{self, error::RecvError};
use uuid::Uuid;

pub async fn overview(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(ok(overview_of(&views::live(&st).await?)))
}

/// 总览 KPI，只统计 active 的机器（实时指标推送也用它）
pub fn overview_of(ms: &[views::MachineView]) -> Value {
    let mut nodes = json!({"total": 0, "ok": 0, "degraded": 0, "down": 0, "unknown": 0, "maintenance": 0});
    let mut relays = nodes.clone();
    let (mut provisioning, mut retiring, mut due_7d, mut up, mut down, mut rx, mut tx) = (0, 0, 0, 0i64, 0i64, 0f64, 0f64);
    // 月费按币种分别汇总（不做汇率折算）
    let mut cost: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    for m in ms {
        match m.lifecycle.as_str() {
            "provisioning" | "draft" => provisioning += 1,
            "retiring" => retiring += 1,
            _ => {}
        }
        if m.lifecycle != "active" {
            continue;
        }
        let bucket = if m.kind == "relay" { &mut relays } else { &mut nodes };
        let key = match m.health.status {
            Status::Ok => "ok",
            Status::Degraded => "degraded",
            Status::Down => "down",
            Status::Maintenance => "maintenance",
            _ => "unknown",
        };
        bucket["total"] = json!(bucket["total"].as_i64().unwrap_or(0) + 1);
        bucket[key] = json!(bucket[key].as_i64().unwrap_or(0) + 1);
        if let Some(s) = &m.subscription {
            if s.due_days <= 7 {
                due_7d += 1;
            }
            // 折成月费
            let months = match s.cycle.as_str() {
                "quarterly" => 3,
                "semiannual" => 6,
                "yearly" => 12,
                _ => 1,
            };
            *cost.entry(s.currency.clone()).or_default() += s.price_cents as i64 / months;
        }
        if m.kind != "relay" {
            for e in &m.entries {
                up += e.traffic.month_up;
                down += e.traffic.month_down;
            }
            up += m.removed_traffic.month_up;
            down += m.removed_traffic.month_down;
        }
        // 实时速率只算数据没过期的：故障或 agent 离线的机器，最后一份上报的速率不算（同前端 liveRate）
        let stale = m.health.status == Status::Down || m.agent.as_ref().is_some_and(|a| !a.online);
        if let Some(h) = m.host.as_ref().filter(|_| !stale) {
            rx += h.net_rx_bps.unwrap_or(0.0);
            tx += h.net_tx_bps.unwrap_or(0.0);
        }
    }
    json!({
        "nodes": nodes, "relays": relays, "provisioning": provisioning, "retiring": retiring, "due_7d": due_7d,
        "monthly_cost": cost.into_iter().map(|(c, v)| json!({"currency": c, "cents": v})).collect::<Vec<_>>(), "traffic_month": {"up": up, "down": down}, "net_rx_bps": rx, "net_tx_bps": tx,
    })
}

#[derive(Serialize, sqlx::FromRow)]
struct IncidentOut {
    id: Uuid,
    rule: String,
    subject_id: Uuid,
    subject_name: Option<String>,
    severity: String,
    state: String,
    summary: String,
    #[serde(with = "time::serde::rfc3339")]
    opened_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    last_seen_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    resolved_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    ack_at: Option<OffsetDateTime>,
    ack_by: Option<String>,
    /// 恢复后又在重开窗口内复发的次数
    reopen_count: i32,
}

#[derive(Deserialize)]
pub struct IncQ {
    state: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

pub async fn incidents(_a: Actor, State(st): State<AppState>, Query(q): Query<IncQ>) -> ApiResult<Json<Value>> {
    let f = match q.state.as_deref() {
        Some("resolved") => "i.state = 'resolved'",
        Some("all") => "true",
        _ => "i.state = 'firing'",
    };
    let sql = format!(
        "select i.id, i.rule, i.subject_id,
            case when m.kind = 'relay' then m.label when t.name is not null then t.name || '-' || lpad(m.number::text, 2, '0') end subject_name,
            i.severity::text severity, i.state::text state, i.summary, i.opened_at, i.last_seen_at, i.resolved_at, i.ack_at, i.ack_by, i.reopen_count
         from incident i left join machine m on m.id = i.subject_id left join team t on t.id = m.team_id
         where {f} order by i.state, i.severity desc, i.opened_at desc, i.id desc limit $1 offset $2"
    );
    let (limit, offset) = (q.limit.unwrap_or(100).clamp(1, 500), q.offset.unwrap_or(0).max(0));
    let v: Vec<IncidentOut> = sqlx::query_as(&sql).bind(limit).bind(offset).fetch_all(&st.db).await?;
    let total: i64 = sqlx::query_scalar(&format!("select count(*) from incident i where {f}")).fetch_one(&st.db).await?;
    let out: Vec<Value> = v
        .iter()
        .map(|i| {
            let mut j = serde_json::to_value(i).unwrap_or_default();
            j["subject"] = json!({"id": i.subject_id, "name": i.subject_name.clone().unwrap_or_else(|| "（已删除）".into())});
            j
        })
        .collect();
    Ok(paged(out, total, offset))
}

pub async fn ack_incident(a: Actor, State(st): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let n = sqlx::query("update incident set ack_at = now(), ack_by = $2 where id = $1 and ack_at is null")
        .bind(id)
        .bind(&a.name)
        .execute(&st.db)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(ApiError::conflict("没有这条告警，或已经确认过"));
    }
    st.emit(Event::Incidents);
    Ok(ok(json!({"ok": true})))
}

#[derive(Serialize, Deserialize, sqlx::FromRow)]
pub struct Silence {
    #[serde(default)]
    id: Option<Uuid>,
    subject_id: Option<Uuid>,
    rule: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    starts_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    ends_at: OffsetDateTime,
    #[serde(default)]
    is_maintenance: bool,
    #[serde(default)]
    comment: String,
    /// 只在读取时给出
    #[serde(default, skip_deserializing)]
    subject_name: Option<String>,
}

pub async fn list_silences(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let v: Vec<Silence> = sqlx::query_as(
        "select s.id, s.subject_id, s.rule, s.starts_at, s.ends_at, s.is_maintenance, s.comment,
                case when m.kind = 'relay' then m.label when t.name is not null then t.name || '-' || lpad(m.number::text, 2, '0') end subject_name
         from silence s left join machine m on m.id = s.subject_id left join team t on t.id = m.team_id
         where s.deleted_at is null and s.ends_at > now() order by s.starts_at",
    )
    .fetch_all(&st.db)
    .await?;
    Ok(ok(v))
}

pub async fn create_silence(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<Silence>) -> ApiResult<Json<Value>> {
    if b.ends_at <= b.starts_at {
        return Err(ApiError::bad_request("结束时间要晚于开始时间"));
    }
    if (b.ends_at - b.starts_at).whole_days() > 30 {
        return Err(ApiError::bad_request("最长 30 天；更久的请直接把机器退役"));
    }
    let id = new_id();
    let mut tx = st.db.begin().await?;
    sqlx::query(
        "insert into silence (id, subject_id, rule, starts_at, ends_at, is_maintenance, comment, created_by) values ($1,$2,$3,$4,$5,$6,$7,$8)",
    )
    .bind(id)
    .bind(b.subject_id)
    .bind(&b.rule)
    .bind(b.starts_at)
    .bind(b.ends_at)
    .bind(b.is_maintenance)
    .bind(&b.comment)
    .bind(&a.name)
    .execute(&mut *tx)
    .await?;
    audit_log(&mut tx, &a.name, "silence.create", "silence", Some(id), json!({"subject_id": b.subject_id, "maintenance": b.is_maintenance})).await?;
    tx.commit().await?;
    st.emit(Event::Incidents);
    st.emit(Event::Machines);
    Ok(ok(json!({"id": id})))
}

pub async fn delete_silence(a: Actor, State(st): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let n = sqlx::query("update silence set deleted_at = now() where id = $1 and deleted_at is null").bind(id).execute(&mut *tx).await?.rows_affected();
    if n == 0 {
        return Err(ApiError::not_found("没有这条静默"));
    }
    audit_log(&mut tx, &a.name, "silence.delete", "silence", Some(id), json!({})).await?;
    tx.commit().await?;
    st.emit(Event::Incidents);
    st.emit(Event::Machines);
    Ok(ok(json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct AuditQ {
    target_id: Option<Uuid>,
    limit: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
struct AuditOut {
    id: i64,
    #[serde(with = "time::serde::rfc3339")]
    ts: OffsetDateTime,
    actor: String,
    action: String,
    target_kind: Option<String>,
    target_id: Option<Uuid>,
    detail: Value,
}

pub async fn audit(_a: Actor, State(st): State<AppState>, Query(q): Query<AuditQ>) -> ApiResult<Json<Value>> {
    let v: Vec<AuditOut> = sqlx::query_as(
        "select id, ts, actor, action, target_kind, target_id, detail from audit_log
         where ($1::uuid is null or target_id = $1) order by id desc limit $2",
    )
    .bind(q.target_id)
    .bind(q.limit.unwrap_or(100).clamp(1, 1000))
    .fetch_all(&st.db)
    .await?;
    let out: Vec<Value> = v
        .into_iter()
        .map(|a| {
            let (title, summary) = audit_text(&a.action, &a.detail);
            let mut j = serde_json::to_value(&a).unwrap_or_default();
            j["title"] = json!(title);
            j["summary"] = json!(summary);
            j
        })
        .collect();
    Ok(ok(out))
}

/// 审计记录的中文标题与一句话摘要（界面直接显示）
pub fn audit_text(action: &str, d: &Value) -> (String, String) {
    let s = |k: &str| d[k].as_str().unwrap_or("").to_string();
    let title = match action {
        "machine.provision" => "上架",
        "machine.retire" => "退役",
        "machine.purge" => "彻底删除",
        "machine.rename" => "改名",
        "machine.update" => "修改资料",
        "subscription.set" => "修改费用",
        "renewal" => "登记续费",
        "team.create" => "新建团队",
        "team.update" => "修改团队",
        "team.renumber" => "重新编号",
        "provider.create" => "新建供应商",
        "provider.update" => "修改供应商",
        "plan.create" => "新建套餐",
        "plan.update" => "修改套餐",
        "entry.enable" => "启用入口",
        "entry.disable" => "停用入口",
        "entry.rotate" => "轮换凭据",
        "entry.share" => "查看分享链接",
        "entry.add" => "新增入口",
        "entry.move" => "换线路机",
        "relay.evacuate" => "搬空线路机",
        "machine.restore" => "恢复机器",
        "job.retry" => "重试作业",
        "job.cancel" => "取消作业",
        "silence.create" => "新建静默",
        "silence.delete" => "删除静默",
        "agent.upgrade" => "升级 agent",
        "notify.create" => "新建通知渠道",
        "notify.update" => "修改通知渠道",
        "notify.delete" => "删除通知渠道",
        other => other,
    }
    .to_string();
    let summary = match action {
        "machine.provision" => format!("{} · {}", s("name"), s("ip")),
        "machine.retire" => match d["reason"].as_str().filter(|r| !r.is_empty()) {
            Some(r) => format!("{}（{r}）", s("name")),
            None => s("name"),
        },
        "machine.purge" => s("name"),
        "renewal" => format!("到期 {} → {}", d["old_due"].as_str().unwrap_or("?"), d["new_due"].as_str().unwrap_or("?")),
        "team.renumber" => {
            let ch = d["changes"].as_array().cloned().unwrap_or_default();
            let mut parts: Vec<String> = ch.iter().take(3).map(|c| format!("{}→{}", c["from"].as_str().unwrap_or(""), c["to"].as_str().unwrap_or(""))).collect();
            if ch.len() > 3 {
                parts.push(format!("等 {} 台", ch.len()));
            }
            let why = d["why"].as_str().map(|w| format!("{w}：")).unwrap_or_default();
            format!("{why}{}", parts.join("，"))
        }
        "entry.move" => format!("{} · {} → {}", s("name"), s("from"), s("to")),
        "relay.evacuate" => format!("{} → {}（{} 个作业）", s("from"), s("to"), d["jobs"].as_array().map_or(0, |a| a.len())),
        "machine.restore" => match d["reason"].as_str().filter(|r| !r.is_empty()) {
            Some(r) => format!("{} → {}（{r}）", s("old_name"), s("name")),
            None => format!("{} → {}", s("old_name"), s("name")),
        },
        a if a.starts_with("entry.") && d["relay"].is_string() => format!("{}:{}", s("relay"), d["port"]),
        "provider.update" => d["after"]["name"].as_str().unwrap_or("").to_string(),
        "team.create" | "provider.create" | "plan.create" | "notify.create" | "notify.update" | "notify.delete" => s("name"),
        _ => String::new(),
    };
    (title, summary)
}

/// 实时流连接用的凭据，用来定期复核「会话/密钥还有效吗」
enum Cred {
    Session(Vec<u8>),
    Key(Vec<u8>),
}

/// 查询出错时按「仍有效」处理：数据库一时抖动不该把所有页面的实时流都断掉
async fn still_valid(db: &sqlx::PgPool, c: &Cred) -> bool {
    let r = match c {
        Cred::Session(h) => {
            sqlx::query_scalar::<_, bool>(
                "select exists (select 1 from session s join app_user u on u.id = s.user_id
                 where s.id_hash = $1 and s.expires_at > now() and u.disabled_at is null)",
            )
            .bind(h)
            .fetch_one(db)
            .await
        }
        Cred::Key(h) => {
            sqlx::query_scalar::<_, bool>(
                "select exists (select 1 from api_key where key_hash = $1 and revoked_at is null and (expires_at is null or expires_at > now()))",
            )
            .bind(h)
            .fetch_one(db)
            .await
        }
    };
    r.unwrap_or(true)
}

/// 心跳：每隔这么久发一次 ping，到下一次 ping 还没收到 pong 就当连接已死
const PING_EVERY: Duration = Duration::from_secs(15);
const RECHECK_EVERY: Duration = Duration::from_secs(60);
/// 会话/密钥失效时的关闭码（4000–4999 归应用自用）
pub const CLOSE_REVOKED: u16 = 4401;

/// 实时流（WebSocket，JSON 文本帧）：资源变更通知 + 实时指标（`live.rs`）。
/// 广播落后丢了消息时发 `resync`（前端让全部查询失效）；每 60 s 复核一次会话/密钥，被撤销就以 4401 关闭。
pub async fn stream(_a: Actor, State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    let bearer = headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    let cred = match (bearer, jar.get(crate::auth::COOKIE)) {
        (Some(k), _) => Some(Cred::Key(sha256(k.as_bytes()))),
        (None, Some(c)) => Some(Cred::Session(sha256(c.value().as_bytes()))),
        _ => None,
    };
    // 握手前就订阅：升级期间的消息也不漏
    let rx = st.events.subscribe();
    let db = st.db.clone();
    ws.on_upgrade(move |socket| serve_ws(socket, rx, db, cred))
}

async fn serve_ws(mut ws: WebSocket, mut rx: broadcast::Receiver<Event>, db: sqlx::PgPool, cred: Option<Cred>) {
    let start = tokio::time::Instant::now();
    let mut ping = tokio::time::interval_at(start + PING_EVERY, PING_EVERY);
    let mut check = tokio::time::interval_at(start + RECHECK_EVERY, RECHECK_EVERY);
    let mut pong_due = false;
    loop {
        let text = tokio::select! {
            r = rx.recv() => match r {
                Ok(e) => e.to_text(),
                Err(RecvError::Lagged(n)) => {
                    tracing::debug!("实时流订阅者落后 {n} 条消息，通知前端重新同步");
                    Event::Resync.to_text()
                }
                Err(RecvError::Closed) => return,
            },
            m = ws.recv() => match m {
                Some(Ok(Message::Pong(_))) => {
                    pong_due = false;
                    continue;
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(_)) => continue,
            },
            _ = ping.tick() => {
                if pong_due || !send(&mut ws, Message::Ping(Default::default())).await {
                    return;
                }
                pong_due = true;
                continue;
            }
            _ = check.tick() => {
                if let Some(c) = &cred {
                    if !still_valid(&db, c).await {
                        send(&mut ws, Message::Close(Some(CloseFrame { code: CLOSE_REVOKED, reason: "会话或密钥已失效".into() }))).await;
                        return;
                    }
                }
                continue;
            }
        };
        if !send(&mut ws, Message::Text(text)).await {
            return;
        }
    }
}

/// 发一帧；对端卡住（不读、TCP 窗口满）超过一个心跳周期就当连接已死，返回 false 由调用方断开
async fn send(ws: &mut WebSocket, m: Message) -> bool {
    matches!(tokio::time::timeout(PING_EVERY, ws.send(m)).await, Ok(Ok(())))
}
