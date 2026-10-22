use crate::error::ApiJson;
use super::{ok, paged};
use crate::auth::Actor;
use crate::db::{audit as audit_log, new_id, Args};
use crate::error::{ApiError, ApiPath, ApiQuery, ApiResult};
use crate::state::{AppState, Event};
use crate::views;
use crate::crypto::sha256;
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use axum_extra::extract::cookie::CookieJar;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;
use time::OffsetDateTime;
use tokio::sync::broadcast::{self, error::RecvError};
use uuid::Uuid;

pub async fn overview(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(ok(views::overview_of(&views::live(&st).await?)))
}

/// 共用的枚举中文名、常量、阈值、默认值与组织时区（见 meta.rs）：网页启动时取一次，fleetctl 每次命令需要时取一次
pub async fn meta(_a: Actor) -> ApiResult<Json<Value>> { Ok(ok(crate::meta::to_json())) }

#[derive(Deserialize)]
pub struct SeriesQ {
    minutes: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
struct RatePoint {
    #[serde(with = "time::serde::rfc3339")]
    ts: OffsetDateTime,
    net_rx_bps: f64,
    net_tx_bps: f64,
}

/// 总览网速曲线：最近 N 个结束满 20 s 的分钟，每分钟一个点，旧→新。agent 每 10 s 上报一次，刚结束的那一分钟还缺最后一两份，算出来偏小，不给。
/// 每台 active 机器按主机时序同样的算法（每分钟取计数器最大值，与上一个有数据的分钟相减 ×8 / 秒数，计数器回绕的那一段不计）再求和；
/// 逐台走 (machine_id, ts desc) 索引。
pub async fn overview_series(_a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<SeriesQ>) -> ApiResult<Json<Value>> {
    let n = q.minutes.unwrap_or(30);
    if !(1..=60).contains(&n) {
        return Err(ApiError::bad_request("minutes 只能是 1–60"));
    }
    let v: Vec<RatePoint> = sqlx::query_as(
        "with w as (select e - make_interval(mins => $1::int) s, e from (select date_trunc('minute', now() - interval '20 seconds') e) z),
         b as (
           select m.id, x.t, x.rx, x.tx from w cross join machine m cross join lateral (
             select date_trunc('minute', ts) t, max(net_rx_bytes) rx, max(net_tx_bytes) tx from host_metric
             where machine_id = m.id and ts >= w.s - interval '1 minute' and ts < w.e group by 1) x
           where m.lifecycle = 'active'),
         r as (
           select t, case when rx >= prx then (rx - prx) * 8.0 / nullif(extract(epoch from t - pt), 0) end rx_bps,
                     case when tx >= ptx then (tx - ptx) * 8.0 / nullif(extract(epoch from t - pt), 0) end tx_bps
           from (select *, lag(rx) over p prx, lag(tx) over p ptx, lag(t) over p pt from b window p as (partition by id order by t)) y)
         select g.t ts, coalesce(sum(r.rx_bps), 0)::float8 net_rx_bps, coalesce(sum(r.tx_bps), 0)::float8 net_tx_bps
         from w cross join generate_series(w.s, w.e - interval '1 minute', interval '1 minute') g(t) left join r on r.t = g.t
         group by g.t order by g.t",
    )
    .bind(n)
    .fetch_all(&st.db)
    .await?;
    Ok(ok(json!({"points": v})))
}

/// 心跳条的一格：(次数, 成功次数, 延迟中位数, SYN 重发过的次数)
type Bucket = (i64, i64, Option<f64>, i64);

/// 探测心跳条：每台在用机器按墙钟 3 分钟整格（date_bin 对齐 epoch）共 20 格，旧→新，最后一格是当前这格（还没走完）；
/// 每格 [次数, 成功次数, 成功的延迟中位数 ms 或 null, 建连时 SYN 重发过的次数]；这 20 格里没有数据的机器不出现。
/// - 节点：线路机 → 节点的 TCP 探测（线路机的 agent 做），汇总它的入口；
/// - 线路机：中控 → 线路机的连通性（reach.rs，只判通不通，延迟与重发为 null / 0）。不再汇总它上面的入口：
///   入口通不通是各节点的事，19 条入口合成一格时 114 次里断 2 次就黄，线路机自己好好的也一片黄。
///
/// 只看未移除或在窗口内才移除的入口：更早移除的在窗口里不会有探测，不必逐个去探测表里找。
/// 节点在不在用按入口连（几百行），不按探测行连：当天分区的统计还没跟上「最近一小时」时，探测行数会被估成个位数，
/// 按探测行连机器表的计划会在 30 ms 与 400+ ms 之间跳
pub async fn probe_beats(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    const BUCKETS: usize = 20;
    let rows: Vec<(Uuid, i32, i64, i64, Option<f64>, i64)> = sqlx::query_as(
        "with w as (select s, s + interval '60 minutes' e from (select date_bin('3 minutes', now(), timestamptz 'epoch') - interval '57 minutes' s) z),
         p as (
           select e.node_id m, floor(extract(epoch from x.ts - w.s) / 180)::int i, x.ok, x.latency_ms, x.retrans
           from w cross join entry e join machine mn on mn.id = e.node_id
           cross join lateral (
             select ts, ok, latency_ms, retrans from probe_result
             where entry_id = e.id and kind = 'tcp' and ts >= w.s and ts < w.e) x
           where mn.lifecycle <> 'retired' and (e.lifecycle <> 'removed' or e.removed_at >= w.s)
           union all
           select r.relay_id, floor(extract(epoch from r.ts - w.s) / 180)::int, r.ok, null::real, null::smallint
           from w cross join relay_reach r join machine mr on mr.id = r.relay_id
           where r.ts >= w.s and r.ts < w.e and mr.lifecycle <> 'retired')
         select m, i, count(*), count(*) filter (where ok),
                (percentile_cont(0.5) within group (order by latency_ms) filter (where ok))::float8, count(*) filter (where retrans > 0)
         from p group by m, i",
    )
    .fetch_all(&st.db)
    .await?;
    let mut out: std::collections::HashMap<Uuid, Vec<Bucket>> = std::collections::HashMap::new();
    for (m, i, n, okn, ms, re) in rows {
        out.entry(m).or_insert_with(|| vec![(0, 0, None, 0); BUCKETS])[i as usize] = (n, okn, ms, re);
    }
    Ok(ok(out))
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
    /// 只看这台机器的告警
    subject_id: Option<Uuid>,
    limit: Option<i64>,
    offset: Option<i64>,
}

pub async fn incidents(_a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<IncQ>) -> ApiResult<Json<Value>> {
    let f = match q.state.as_deref() {
        Some("resolved") => "i.state = 'resolved'",
        Some("all") => "true",
        _ => "i.state = 'firing'",
    };
    let (limit, offset) = (q.limit.unwrap_or(100).clamp(1, 500), q.offset.unwrap_or(0).max(0));
    // 按机器筛选的条件只在给了 subject_id 时才拼上（列表与总数各拼一份），走 (subject_id, opened_at desc) 索引
    let cond = |a: &mut Args| match q.subject_id {
        Some(s) => format!("{f} and i.subject_id = {}", a.add(s)),
        None => f.to_string(),
    };
    // 全站列表严重的在前；一台机器的（抽屉「最近告警」）只按时间：进行中在前、再按触发时间新的在前，旧的严重告警不挤掉新的
    let order = if q.subject_id.is_some() { "i.state, i.opened_at desc, i.id desc" } else { "i.state, i.severity desc, i.opened_at desc, i.id desc" };
    let mut a = Args::default();
    let sql = format!(
        "select i.id, i.rule, i.subject_id,
            case when i.rule like 'central!_%' escape '!' then '中控' when i.rule like 'router!_%' escape '!' then r.name
                 else display_name(m.kind::text, m.label, t.name, m.number) end subject_name,
            i.severity::text severity, i.state::text state, i.summary, i.opened_at, i.last_seen_at, i.resolved_at, i.ack_at, i.ack_by, i.reopen_count
         from incident i left join machine m on m.id = i.subject_id left join team t on t.id = m.team_id
              left join router r on r.id = i.subject_id and i.rule like 'router!_%' escape '!'
         where {} order by {order} limit {} offset {}",
        cond(&mut a),
        a.add(limit),
        a.add(offset)
    );
    let v: Vec<IncidentOut> = sqlx::query_as_with(&sql, a.0).fetch_all(&st.db).await?;
    let mut a = Args::default();
    // acked：其中已确认的条数（确认不关闭告警，顶栏计数仍含它们，悬停里分开写）
    let sql = format!("select count(*), count(*) filter (where i.ack_at is not null) from incident i where {}", cond(&mut a));
    let (total, acked): (i64, i64) = sqlx::query_as_with(&sql, a.0).fetch_one(&st.db).await?;
    let out: Vec<Value> = v
        .iter()
        .map(|i| {
            let mut j = serde_json::to_value(i).unwrap_or_default();
            if crate::selfmon::is_central(&i.rule) {
                // 中控自身的告警：对象不是机器，没有可点开的机器
                j["subject"] = Value::Null;
                j["subject_id"] = Value::Null;
            } else if crate::routers::alerts::is_router(&i.rule) {
                // 软路由的告警：对象是路由，前端按 kind 打开路由而不是机器
                j["subject"] = crate::routers::alerts::subject(i.subject_id, i.subject_name.as_deref());
            } else {
                j["subject"] = json!({"id": i.subject_id, "name": i.subject_name.clone().unwrap_or_else(|| "（已删除）".into())});
            }
            j["rule_name"] = json!(crate::notify::rule_name(&i.rule));
            j
        })
        .collect();
    let mut r = paged(out, total, offset);
    r.0["data"]["acked"] = json!(acked);
    Ok(r)
}

pub async fn ack_incident(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let cur: Option<(String, String, Option<String>)> = sqlx::query_as("select rule, summary, ack_by from incident where id = $1 for update")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    let (rule, summary) = match cur {
        None => return Err(ApiError::not_found("没有这条告警")),
        Some((_, _, Some(by))) => return Err(ApiError::conflict(format!("这条告警已经由 {by} 确认过"))),
        Some((rule, summary, None)) => (rule, summary),
    };
    sqlx::query("update incident set ack_at = now(), ack_by = $2 where id = $1").bind(id).bind(&a.name).execute(&mut *tx).await?;
    audit_log(&mut tx, &a.name, "incident.ack", "incident", Some(id), json!({"rule": rule, "summary": summary})).await?;
    tx.commit().await?;
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
    /// 规则中文名（同告警），不限规则时为 null；只在读取时给出
    #[sqlx(skip)]
    #[serde(default, skip_deserializing)]
    rule_name: Option<String>,
}

impl Silence {
    fn named(mut self) -> Self {
        self.rule_name = self.rule.as_deref().map(|r| crate::notify::rule_name(r).to_string());
        self
    }
}

/// 机器名（同列表的 subject_name）：静默的对象、审计里写它
const SUBJECT_NAME: &str = "(select display_name(m.kind::text, m.label, t.name, m.number) from machine m left join team t on t.id = m.team_id where m.id = s.subject_id)";

/// 没结束的静默（含还没开始的预约），按开始时间排：进行中的自然在前，已预约的随后
pub async fn list_silences(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let v: Vec<Silence> = sqlx::query_as(&format!(
        "select s.id, s.subject_id, s.rule, s.starts_at, s.ends_at, s.is_maintenance, s.comment, {SUBJECT_NAME} subject_name
         from silence s where s.deleted_at is null and s.ends_at > now() order by s.starts_at"
    ))
    .fetch_all(&st.db)
    .await?;
    Ok(ok(v.into_iter().map(Silence::named).collect::<Vec<_>>()))
}

pub async fn create_silence(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<Silence>) -> ApiResult<Json<Value>> {
    if b.ends_at <= b.starts_at {
        return Err(ApiError::bad_request("结束时间要晚于开始时间"));
    }
    if b.ends_at - b.starts_at > time::Duration::days(30) {
        return Err(ApiError::bad_request("最长 30 天；更久的请直接把机器退役"));
    }
    // 全部机器的静默 / 维护等于把监控整个关掉：API 密钥（脚本、AI）建的最长 24 小时，更长的要登录网页用账号建
    if b.subject_id.is_none() && !a.is_session() && b.ends_at - b.starts_at > time::Duration::hours(24) {
        return Err(ApiError::bad_request("API 密钥建的全部机器静默 / 维护最长 24 小时；更长的请登录网页用账号建"));
    }
    // 开始时间可以已经过去（「从现在开始」由客户端按它的时钟填），结束时间不行：那样的静默一刻也不会生效
    let now = OffsetDateTime::now_utc();
    if b.ends_at <= now {
        return Err(ApiError::bad_request("结束时间已经过去了"));
    }
    if b.starts_at > now + time::Duration::days(90) {
        return Err(ApiError::bad_request("最多提前 90 天预约"));
    }
    if b.comment.chars().count() > 2000 {
        return Err(ApiError::bad_request("备注最长 2000 个字"));
    }
    if let Some(r) = b.rule.as_deref() {
        // rule_name 对不认识的规则原样返回
        if crate::notify::rule_name(r) == r {
            return Err(ApiError::bad_request(format!("没有「{r}」这条告警规则")));
        }
        // 维护是整台机器的（卡片显示「维护中」、暂停健康判定），只静默一条规则时说不通
        if b.is_maintenance {
            return Err(ApiError::bad_request("维护窗口针对整台机器，不能只选一条规则；只想静默这条告警就不要标记为维护"));
        }
    }
    let subject_name = match b.subject_id {
        Some(sid) => Some(
            sqlx::query_scalar("select display_name(m.kind::text, m.label, t.name, m.number) from machine m left join team t on t.id = m.team_id where m.id = $1")
                .bind(sid)
                .fetch_optional(&st.db)
                .await?
                .ok_or_else(|| ApiError::bad_request("subject_id 指向的机器不存在"))?,
        ),
        None => None,
    };
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
    // 审计详情就是这条静默本身（对象名、规则、起止、备注），摘要见 audit_text
    let s = Silence { id: Some(id), subject_name, ..b }.named();
    audit_log(&mut tx, &a.name, "silence.create", "silence", Some(id), json!(s)).await?;
    tx.commit().await?;
    tell_channels(&st, &a.name, false, &s).await;
    // 卡片上的「维护中」随 Incidents 标记的实时指标比对送达（最多 3 s），不发 Machines 让前端整表重取
    st.emit(Event::Incidents);
    Ok(ok(json!({"id": id})))
}

pub async fn delete_silence(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let s: Silence = sqlx::query_as(&format!(
        "update silence s set deleted_at = now() where s.id = $1 and s.deleted_at is null
         returning s.id, s.subject_id, s.rule, s.starts_at, s.ends_at, s.is_maintenance, s.comment, {SUBJECT_NAME} subject_name"
    ))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("没有这条静默"))?;
    let s = s.named();
    audit_log(&mut tx, &a.name, "silence.delete", "silence", Some(id), json!(s)).await?;
    tx.commit().await?;
    tell_channels(&st, &a.name, true, &s).await;
    st.emit(Event::Incidents);
    Ok(ok(json!({"ok": true})))
}

/// 新建 / 删除静默、维护窗口时告诉所有启用的渠道（不看渠道的最低级别，也不受任何静默影响），免得有人悄悄开个全局维护、监控就没了：
/// 全部机器的一律发（含预约的）；只针对一台的，此刻正生效的才发（预约的、已经结束的不影响现在的告警）
async fn tell_channels(st: &AppState, actor: &str, deleted: bool, s: &Silence) {
    let now = OffsetDateTime::now_utc();
    if s.subject_id.is_some() && !(s.starts_at <= now && now < s.ends_at) {
        return;
    }
    let what = if s.is_maintenance { "维护窗口" } else { "静默" };
    let object = s.subject_name.as_deref().unwrap_or("全部机器");
    let rules = match (&s.rule_name, s.is_maintenance) {
        (Some(r), _) => format!("（只静默「{r}」）"),
        (None, true) => String::new(),
        (None, false) => "（全部规则）".into(),
    };
    let mut text = format!(
        "{actor} {}了{what}\n对象：{object}{rules}\n起止：{} ~ {}（{}）",
        if deleted { "删除" } else { "新建" },
        crate::notify::beijing(s.starts_at),
        crate::notify::beijing(s.ends_at),
        crate::meta::org_tz_label()
    );
    // 备注收成一行、最多 200 个字
    let comment: String = s.comment.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(200).collect();
    if !comment.is_empty() {
        text.push_str(&format!("\n备注：{comment}"));
    }
    text.push_str(match (deleted, s.is_maintenance) {
        (true, _) => "\n告警与通知照常恢复",
        (false, true) => "\n期间不开告警、不发通知，健康判定暂停",
        (false, false) => "\n期间不开告警、不发通知",
    });
    let n = crate::notify::Notice {
        kind: crate::notify::Kind::Silence,
        incident_id: None,
        rule: s.rule.clone().unwrap_or_default(),
        severity: "info".into(),
        machine: object.to_string(),
        summary: text,
        at: now,
        opened_at: None,
    };
    crate::notify::enqueue(st, n).await;
}

#[derive(Deserialize)]
pub struct AuditQ {
    target_id: Option<Uuid>,
    limit: Option<i64>,
    /// 翻页：只要 id 小于它的（上一页最后一条的 id）
    before_id: Option<i64>,
    /// 分类：all | auth | exec | machine | config（见 `audit_cat`）
    cat: Option<String>,
    /// 搜索标题、摘要、操作人（见 `audit_search`）
    q: Option<String>,
}

/// 远程执行类的动作：在机器上跑命令、SSH 登录、网页终端、取日志、恢复预检登录机器
const EXEC_ACTIONS: &[&str] = &["exec", "ssh.login", "terminal.open", "terminal.close", "machine.logs", "server.logs", "restore.probe"];

/// 分类条件（操作记录页的分段）：登录与账户（auth.*）、远程执行（`EXEC_ACTIONS`）、
/// 机器与入口（生命周期、改名、费用续费、入口、作业、升级 agent），其余是设置与告警（目录、密钥、通知、静默、告警、访问规则）
fn audit_cat(args: &mut Args, cat: &str) -> ApiResult<String> {
    const MACHINE: &str = "action ~ '^(machine|entry|relay|subscription|agent|job|renewal)(\\.|$)'";
    let exec = args.add(EXEC_ACTIONS);
    Ok(match cat {
        "auth" => " and action like 'auth.%'".into(),
        "exec" => format!(" and action = any({exec})"),
        "machine" => format!(" and {MACHINE} and action <> all({exec})"),
        "config" => format!(" and action not like 'auth.%' and action <> all({exec}) and not {MACHINE}"),
        _ => return Err(ApiError::bad_request("cat 只能是 all、auth、exec、machine、config")),
    })
}

/// 搜索条件：只搜界面上看得到的——操作人（按界面上的写法：中控命令行、「名称（API 密钥）」）、标题（标题含关键字的动作）、
/// 写审计时存下的标题与摘要（search 列，见 `audit_search_text`），不翻 detail 里界面不展示的值。
/// search 里的命令是隐藏的（没有 exec 权限的人看到的那份，免得靠搜索试出命令内容）；有 exec 权限的另搜命令原文
fn audit_search(args: &mut Args, q: &str, can_exec: bool) -> Option<String> {
    let like = crate::access::like_pattern(q)?;
    let lower = q.trim().to_lowercase();
    let titles: Vec<&str> = AUDIT_ACTIONS.iter().copied().filter(|a| audit_text(a, &json!({})).0.to_lowercase().contains(&lower)).collect();
    let (l, t) = (args.add(like), args.add(titles));
    let command = if can_exec { format!(" or case when jsonb_typeof(detail->'command') = 'string' then detail->>'command' end ilike {l}") } else { String::new() };
    Some(format!(
        " and (case when actor = 'cli' then '中控命令行' when actor like 'apikey:%' then substr(actor, 8) || '（API 密钥）' else actor end ilike {l}
           or action = any({t}) or search ilike {l}{command})"
    ))
}

/// 没有 exec 权限的人看到的命令：只给字数（第一行也可能带口令），摘要随之隐藏
fn hide_command(d: &mut Value) {
    if let Some(c) = d.get("command").and_then(Value::as_str) {
        d["command"] = json!(format!("命令已隐藏（{} 字）", c.chars().count()));
    }
}

/// 存进 search 列的搜索文本：标题与摘要（命令隐藏后的，同没有 exec 权限的人看到的）
pub fn audit_search_text(action: &str, d: &Value) -> String {
    let mut d = d.clone();
    hide_command(&mut d);
    let (title, summary) = audit_text(action, &d);
    format!("{title}\n{summary}")
}

/// 操作记录查询的语句超时：关键词搜索沿主键倒序边扫边筛，命中少的词可能扫很多行
const AUDIT_TIMEOUT_S: u32 = 5;

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

pub async fn audit(a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<AuditQ>) -> ApiResult<Json<Value>> {
    // 没有 exec 权限的密钥看不到命令（第一行也可能带口令）：只给字数，摘要随之隐藏
    let can_exec = a.require_scope("exec").is_ok();
    // 只拼给了的条件：按对象查时才走得上 (target_id, id desc) 索引；分类、搜索沿主键倒序边扫边筛，同样按 before_id 往前翻
    let mut args = Args::default();
    let mut w = String::from("true");
    if let Some(t) = q.target_id {
        w += &format!(" and target_id = {}", args.add(t));
    }
    if let Some(b) = q.before_id {
        w += &format!(" and id < {}", args.add(b));
    }
    match q.cat.as_deref().map(str::trim) {
        None | Some("" | "all") => {}
        Some(c) => w += &audit_cat(&mut args, c)?,
    }
    if let Some(s) = q.q.as_deref().and_then(|s| audit_search(&mut args, s, can_exec)) {
        w += &s;
    }
    let sql = format!(
        "select id, ts, actor, action, target_kind, target_id, detail from audit_log where {w} order by id desc limit {}",
        args.add(q.limit.unwrap_or(100).clamp(1, 1000))
    );
    let mut tx = st.db.begin().await?;
    sqlx::query(&format!("set local statement_timeout = '{AUDIT_TIMEOUT_S}s'")).execute(&mut *tx).await?;
    let v: Vec<AuditOut> = sqlx::query_as_with(&sql, args.0).fetch_all(&mut *tx).await.map_err(|e| match &e {
        sqlx::Error::Database(d) if d.code().as_deref() == Some("57014") => ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            format!("搜索超过 {AUDIT_TIMEOUT_S} 秒还没查完，已中止：换个更具体的关键词，或先按分类、对象筛选"),
        ),
        _ => e.into(),
    })?;
    tx.commit().await?;
    let out: Vec<Value> = v
        .into_iter()
        .map(|mut a| {
            if !can_exec {
                hide_command(&mut a.detail);
            }
            let (title, summary) = audit_text(&a.action, &a.detail);
            let mut j = serde_json::to_value(&a).unwrap_or_default();
            j["title"] = json!(title);
            j["summary"] = json!(summary);
            j
        })
        .collect();
    Ok(ok(out))
}

/// 所有写审计的动作（git grep 'audit(' 核对过）；新增动作时补这里和 audit_text。按标题搜索时在这里面找
const AUDIT_ACTIONS: &[&str] = &[
    "machine.provision", "machine.retire", "machine.purge", "machine.rename", "machine.update", "machine.restore", "subscription.set", "renewal",
    "team.create", "team.update", "team.delete", "team.renumber", "provider.create", "provider.update", "provider.delete", "plan.create", "plan.update", "plan.delete", "entry.enable",
    "entry.disable", "entry.rotate", "entry.share", "entry.add", "entry.move", "relay.evacuate", "job.retry", "job.cancel", "silence.create",
    "silence.delete", "incident.ack", "restore.probe", "agent.upgrade", "notify.create", "notify.update", "notify.delete", "exec", "machine.logs", "server.logs", "apikey.create",
    "apikey.revoke", "auth.login", "auth.login_failed", "auth.logout", "auth.totp_enabled", "auth.totp_disabled", "auth.totp_recovery_used",
    "auth.totp_recovery_regenerated", "auth.password_changed", "auth.session_revoked", "auth.verify_failed", "policy.draft", "policy.discard", "policy.publish", "policy.rollback",
    "settings.access_retention", "terminal.open", "terminal.close", "ssh.login",
    "router.create", "router.update", "router.delete", "router.bootstrap", "router.bootstrap_used", "router.deploy",
];

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
        "team.delete" => "删除团队",
        "team.renumber" => "重新编号",
        "provider.create" => "新建供应商",
        "provider.update" => "修改供应商",
        "provider.delete" => "删除供应商",
        "plan.create" => "新建套餐",
        // 只改了归档的写成归档 / 取消归档
        "plan.update" => match plan_archive(d) {
            Some(true) => "归档套餐",
            Some(false) => "取消归档套餐",
            None => "修改套餐",
        },
        "plan.delete" => "删除套餐",
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
        // 标记为维护的叫维护窗口，否则叫静默（同界面）
        "silence.create" if d["is_maintenance"] == true => "新建维护窗口",
        "silence.delete" if d["is_maintenance"] == true => "删除维护窗口",
        "silence.create" => "新建静默",
        "silence.delete" => "删除静默",
        "incident.ack" => "确认告警",
        "restore.probe" => "恢复预检（登录机器）",
        "agent.upgrade" => "升级 agent",
        "notify.create" => "新建通知渠道",
        "notify.update" => "修改通知渠道",
        "notify.delete" => "删除通知渠道",
        "exec" => "执行命令",
        "ssh.login" => "SSH 登录",
        "machine.logs" => "查看机器日志",
        "server.logs" => "查看中控日志",
        "terminal.open" => "打开网页终端",
        "terminal.close" => "关闭网页终端",
        "apikey.create" => "新建 API 密钥",
        "apikey.revoke" => "撤销 API 密钥",
        "auth.login" => "登录",
        "auth.login_failed" => "登录失败",
        "auth.logout" => "退出登录",
        "auth.totp_enabled" => "开启两步验证",
        "auth.totp_disabled" => "关闭两步验证",
        "auth.totp_recovery_used" => "用恢复码验证",
        "auth.totp_recovery_regenerated" => "重新生成恢复码",
        "auth.password_changed" => "修改密码",
        "auth.session_revoked" => "撤销登录会话",
        "auth.verify_failed" => "密码或验证码校验失败",
        "policy.draft" => "修改访问规则草稿",
        "policy.discard" => "丢弃访问规则草稿",
        "policy.publish" => "发布访问规则",
        "policy.rollback" => "回滚访问规则",
        "settings.access_retention" => "修改访问记录保留期",
        "router.create" => "新建软路由",
        "router.update" => "修改软路由",
        "router.delete" => "删除软路由",
        "router.bootstrap" => "重新生成接入命令",
        "router.bootstrap_used" => "路由取走接入脚本",
        "router.deploy" => "一键部署软路由",
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
        // 「团队1-03 · 价格 5.00 → 6.00 USD，到期 2026-11-01 → 2026-12-01」；这一版之前的记录没有改动前后，不写摘要
        "subscription.set" if d["after"].is_object() => {
            let changes = sub_changes(d);
            match (s("name"), changes.is_empty()) {
                (n, true) => format!("{n} · 没有改动"),
                (n, false) => format!("{n} · {changes}"),
            }
        }
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
        "entry.add" if d["direct"] == true => "线路机直出入口".into(),
        "provider.update" => d["after"]["name"].as_str().unwrap_or("").to_string(),
        "team.create" | "team.delete" | "provider.create" | "provider.delete" | "plan.create" | "plan.delete" | "notify.create" | "notify.update" | "notify.delete" => s("name"),
        // 「测试团队 → 测试团队改，排序 200 → 50」：改了名写从什么改成什么，没改名只写名字
        "team.update" => {
            let (b, a) = (&d["before"], &d["after"]);
            let name = if b["name"] == a["name"] { val(&a["name"]) } else { format!("{} → {}", val(&b["name"]), val(&a["name"])) };
            match (b["sort_order"].as_i64(), a["sort_order"].as_i64()) {
                (Some(x), Some(y)) if x != y => format!("{name}，排序 {x} → {y}"),
                _ => name,
            }
        }
        // 「ATTVPS 基础版 · 价格 59.00 → 69.00 USD，流量 800 → 1000 GB」；只改了归档的只写名字（标题已经说了归档）。这一版之前的记录没有名字
        "plan.update" => {
            let changes = plan_changes(d);
            match (s("name"), changes.is_empty() || plan_archive(d).is_some()) {
                (n, true) => n,
                (n, false) if n.is_empty() => changes,
                (n, false) => format!("{n} · {changes}"),
            }
        }
        // 「团队1-07 → 团队1-03」；修改资料写名字和改了的国家（团队、编号变了算改名）
        "machine.rename" if d["old_name"].is_string() => format!("{} → {}", s("old_name"), s("name")),
        "machine.update" if d["name"].is_string() => match (d["before"]["country"].as_str(), d["after"]["country"].as_str()) {
            (b, a) if b != a => format!("{} · 国家 {} → {}", s("name"), b.unwrap_or("自动"), a.unwrap_or("自动")),
            _ => s("name"),
        },
        // 「团队1-03」「5 台（团队1-03、团队1-04、团队1-05、…），跳过 1 台」；这一版之前的记录没有台名，只写台数
        "agent.upgrade" => {
            let n = d["count"].as_u64().unwrap_or(0);
            let names: Vec<&str> = d["names"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            let mut s = match names.as_slice() {
                [one] if n == 1 => one.to_string(),
                [] => format!("{n} 台"),
                l => format!("{n} 台（{}{}）", l.join("、"), if n as usize > l.len() { "、…" } else { "" }),
            };
            if let Some(k) = d["skipped"].as_u64().filter(|k| *k > 0) {
                s += &format!("，跳过 {k} 台");
            }
            s
        }
        // 「换线路机 · 团队1-03」：这一版之前的记录没有作业类型，不写摘要
        "job.cancel" | "job.retry" if d["kind"].is_string() => {
            let k = crate::meta::job_kind_label(d["kind"].as_str().unwrap_or(""));
            match d["name"].as_str() {
                Some(n) => format!("{k} · {n}"),
                None => k.to_string(),
            }
        }
        "exec" => {
            // 命令只取第一行、最多 60 字
            let cmd = s("command");
            let line = cmd.lines().next().unwrap_or("");
            let mut c: String = line.chars().take(60).collect();
            if c.len() < cmd.trim_end().len() {
                c.push('…');
            }
            let res = match (d["exit_code"].as_i64(), d["timed_out"] == true) {
                _ if d["started"] == true => "执行中或中断（未记录结果）".to_string(),
                (_, true) => "超时".to_string(),
                (Some(code), _) => format!("退出码 {code}"),
                (None, _) => format!("失败：{}", s("error")),
            };
            format!("{} · {c} · {res}", s("name"))
        }
        "machine.logs" | "restore.probe" => s("name"),
        "ssh.login" => format!("{} · {}", s("name"), if d["command"] == true { "带命令（内容不记录）" } else { "交互式" }),
        "incident.ack" => format!("{} · {}", crate::notify::rule_name(d["rule"].as_str().unwrap_or("")), s("summary")),
        "server.logs" => format!("{} 行", d["lines"]),
        "apikey.create" | "apikey.revoke" if d["via"] == "local-admin" => format!("{}（{}）· 中控命令行 local-admin", s("name"), s("prefix")),
        "terminal.open" | "terminal.close" => crate::terminal::audit_summary(action, d),
        "apikey.create" | "apikey.revoke" => format!("{}（{}）", s("name"), s("prefix")),
        "auth.login" | "auth.login_failed" | "auth.totp_recovery_regenerated" | "auth.verify_failed" => s("ip"),
        "auth.totp_disabled" if d["via"] == "totp-reset" => "中控命令行 totp-reset".into(),
        "auth.totp_disabled" => s("ip"),
        "auth.totp_recovery_used" => format!("{} · 剩 {} 个", s("ip"), d["left"]),
        "auth.password_changed" if d["revoked_api_keys"].as_u64().is_some_and(|n| n > 0) => {
            format!("{} · 撤销其它 {} 个会话、{} 个 API 密钥", s("ip"), d["revoked_sessions"], d["revoked_api_keys"])
        }
        "auth.password_changed" => format!("{} · 撤销其它 {} 个会话", s("ip"), d["revoked_sessions"]),
        "auth.session_revoked" if d["others"].is_number() => format!("退出其它设备（{} 个）", d["others"]),
        "auth.session_revoked" => s("ip"),
        // 「团队1 · 黑名单 · 2 个域名」：这一版之前的记录没有范围名
        "policy.draft" => {
            let what = if d["cleared"] == true {
                "撤销草稿".to_string()
            } else {
                let mode = crate::meta::label(crate::meta::POLICY_MODES, d["mode"].as_str().unwrap_or(""));
                format!("{mode} · {} 个域名", d["domains"])
            };
            match d["name"].as_str() {
                Some(n) => format!("{n} · {what}"),
                None => what,
            }
        }
        // 这一版之前的只记了对象 id 与是否维护，不写摘要
        "silence.create" | "silence.delete" if d["starts_at"].is_string() => {
            let at = |k: &str| {
                let t = d[k].as_str().and_then(|v| OffsetDateTime::parse(v, &time::format_description::well_known::Rfc3339).ok());
                t.map(|t| crate::notify::beijing(t).chars().take(16).collect::<String>()).unwrap_or_default()
            };
            let what = if d["is_maintenance"] == true { "维护窗口" } else { "静默" };
            let rule = d["rule_name"].as_str().map(|r| format!(" · 只静默「{r}」")).unwrap_or_default();
            let note = d["comment"].as_str().filter(|c| !c.is_empty()).map(|c| format!("（{c}）")).unwrap_or_default();
            format!("{} · {what}{rule} · {} → {}{note}", d["subject_name"].as_str().unwrap_or("全部机器"), at("starts_at"), at("ends_at"))
        }
        "policy.discard" => format!("{} 个草稿", d["drafts"]),
        "router.create" if d["method"] == "deploy" => format!("{} · 一键部署", s("name")),
        "router.create" | "router.update" | "router.delete" | "router.bootstrap" => s("name"),
        "router.bootstrap_used" => format!("{} · 来自 {}", s("name"), s("ip")),
        "router.deploy" => {
            let res = match d["result"].as_str() {
                _ if d["started"] == true => "进行中或中断（未记录结果）".to_string(),
                Some("ok") => "成功".to_string(),
                _ => format!("失败：{}", s("error")),
            };
            let how = if d["password"] == true { "密码" } else { "中控密钥" };
            format!("{} · {}@{}:{}（{how}）· {res}", s("name"), s("user"), s("host"), d["port"])
        }
        // 「v5 · 回滚到 v3 · 团队1：黑名单 → 仅观察；团队2-03：白名单 · 域名 2 → 3 个 · 6 台线路机」
        "policy.publish" | "policy.rollback" => {
            let ch = d["changes"].as_array().cloned().unwrap_or_default();
            let mut parts = vec![format!("v{}", d["seq"])];
            if let Some(to) = d["rollback_of_seq"].as_i64() {
                parts.push(format!("回滚到 v{to}"));
            }
            let mut what: Vec<String> = ch.iter().take(2).map(policy_change).collect();
            if ch.len() > 2 {
                what.push(format!("等 {} 处", ch.len()));
            }
            parts.push(if what.is_empty() { "只改了备注".into() } else { what.join("；") });
            if let Some(n) = d["relays"].as_u64().filter(|n| *n > 0) {
                parts.push(format!("{n} 台线路机"));
            }
            parts.join(" · ")
        }
        // 「原始记录 7 → 3 天，汇总 31 天」：改了的写从几天到几天，没改的只写天数
        "settings.access_retention" => [("raw_days", "原始记录"), ("hourly_days", "汇总")]
            .iter()
            .filter_map(|(k, label)| match (d["from"][k].as_i64(), d["to"][k].as_i64()?) {
                (Some(a), b) if a != b => Some(format!("{label} {a} → {b} 天")),
                (_, b) => Some(format!("{label} {b} 天")),
            })
            .collect::<Vec<_>>()
            .join("，"),
        _ => String::new(),
    };
    (title, summary)
}

/// subscription.set 改了的生效值（before / after）；另写按面板校准的已用流量
fn sub_changes(d: &Value) -> String {
    let (b, a) = (&d["before"], &d["after"]);
    if b.is_null() {
        return format!("新建订阅，到期 {}", val(&a["due_date"]));
    }
    let cur = a["currency"].as_str().unwrap_or("");
    let money = |v: &Value| v.as_i64().map_or("—".into(), |c| format!("{:.2}", c as f64 / 100.0));
    let gb = |v: &Value| v.as_i64().filter(|g| *g > 0).map_or("不限".into(), |g| format!("{g} GB"));
    let cycle = |v: &Value| v.as_str().and_then(|k| crate::meta::CYCLES.iter().find(|c| c.key == k)).map_or_else(|| val(v), |c| c.label.to_string());
    let onoff = |v: &Value| if v == true { "开" } else { "关" }.to_string();
    let fields: [(&str, &str, &dyn Fn(&Value) -> String); 9] = [
        ("due_date", "到期", &val),
        ("price_cents", "价格", &money),
        ("currency", "币种", &val),
        ("cycle", "周期", &cycle),
        ("traffic_gb", "流量", &gb),
        ("traffic_reset_day", "流量重置日", &|v| format!("{} 号", val(v))),
        ("remind_days", "提前提醒", &|v| format!("{} 天", val(v))),
        ("billing_tz", "账单时区", &val),
        ("auto_renew", "自动续费", &onoff),
    ];
    let mut out: Vec<String> = fields
        .iter()
        .filter(|(k, ..)| b[*k] != a[*k])
        .map(|(k, label, f)| match *k {
            // 同时换了币种：旧价格按旧币种写
            "price_cents" => match b["currency"].as_str().filter(|c| *c != cur) {
                Some(old) => format!("{label} {} {old} → {} {cur}", f(&b[*k]), f(&a[*k])),
                None => format!("{label} {} → {} {cur}", f(&b[*k]), f(&a[*k])),
            },
            _ => format!("{label} {} → {}", f(&b[*k]), f(&a[*k])),
        })
        .collect();
    match d.get("traffic_used_gb") {
        Some(Value::Null) => out.push("清除已用流量校准".into()),
        Some(v) if v.is_number() => out.push(format!("已用流量校准为 {v} GB")),
        _ => {}
    }
    out.join("，")
}

/// 访问规则版本里的一处变更（同前端发布记录的写法）：模式变了写「团队1：黑名单 → 仅观察」，没变写域名、备注改了什么：
/// 「黑名单 · 域名 2 → 3 个」「黑名单 · 换了域名（2 个），改了备注」「黑名单 · 改了备注」。
/// 没有 domains_changed / note_changed 的旧记录：用域名的模式算改了域名，不用的算改了备注
fn policy_change(c: &Value) -> String {
    let (f, t) = (&c["from"], &c["to"]);
    let mode = |v: &Value| crate::meta::label(crate::meta::POLICY_MODES, v["mode"].as_str().unwrap_or("")).to_string();
    let uses = |v: &Value| matches!(v["mode"].as_str(), Some("allowlist" | "denylist"));
    let what = if f["mode"] != t["mode"] {
        let n = if uses(t) { format!("（{} 个域名）", t["domains"]) } else { String::new() };
        format!("{} → {}{n}", mode(f), mode(t))
    } else {
        let mut parts = vec![];
        if uses(t) && c["domains_changed"].as_bool().unwrap_or(true) {
            parts.push(if f["domains"] == t["domains"] {
                format!("换了域名（{} 个）", t["domains"])
            } else {
                format!("域名 {} → {} 个", f["domains"], t["domains"])
            });
        }
        if c["note_changed"].as_bool().unwrap_or(!uses(t)) {
            parts.push("改了备注".into());
        }
        format!("{} · {}", mode(t), parts.join("，"))
    };
    format!("{}：{what}", c["name"].as_str().unwrap_or(""))
}

/// JSON 值的显示：字符串原样，null 写「—」，其余（数字）照写
fn val(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "—".into(),
        v => v.to_string(),
    }
}

/// plan.update 是不是只改了归档：Some(true) 归档、Some(false) 取消归档、None 还改了别的
fn plan_archive(d: &Value) -> Option<bool> {
    let a = d["after"].as_object()?;
    (a.len() == 1).then(|| a.get("archived_at").map(|v| !v.is_null())).flatten()
}

/// plan.update 改了的字段：「价格 59.00 → 69.00 USD，流量 800 → 1000 GB」
fn plan_changes(d: &Value) -> String {
    let (b, a) = (&d["before"], &d["after"]);
    let Some(keys) = a.as_object() else { return String::new() };
    let cur = d["currency"].as_str().unwrap_or("");
    let money = |v: &Value| v.as_i64().map_or("—".into(), |c| format!("{:.2}", c as f64 / 100.0));
    let gb = |v: &Value| v.as_i64().filter(|g| *g > 0).map_or("不限".into(), |g| format!("{g}"));
    let cycle = |v: &Value| v.as_str().and_then(|k| crate::meta::CYCLES.iter().find(|c| c.key == k)).map_or_else(|| val(v), |c| c.label.to_string());
    keys.keys()
        .filter_map(|k| {
            let (x, y) = (&b[k.as_str()], &a[k.as_str()]);
            Some(match k.as_str() {
                "name" => format!("改名 {} → {}", val(x), val(y)),
                "price_cents" => format!("价格 {} → {} {cur}", money(x), money(y)),
                "currency" => format!("币种 {} → {}", val(x), val(y)),
                "cycle" => format!("周期 {} → {}", cycle(x), cycle(y)),
                "traffic_gb" => format!("流量 {} → {} GB", gb(x), gb(y)),
                "bandwidth_mbps" => format!("带宽 {} → {} Mbps", val(x), val(y)),
                "cpu_cores" => format!("CPU {} → {} 核", val(x), val(y)),
                "mem_mb" => format!("内存 {} → {} MiB", val(x), val(y)),
                "disk_gb" => format!("磁盘 {} → {} GiB", val(x), val(y)),
                "provider_id" => "换了供应商".into(),
                "notes" => "改了备注".into(),
                "archived_at" => if y.is_null() { "取消归档" } else { "归档" }.into(),
                _ => return None,
            })
        })
        .collect::<Vec<_>>()
        .join("，")
}

/// 实时流连接用的凭据，用来定期复核「会话/密钥还有效吗」
enum Cred {
    Session(Vec<u8>),
    Key(Vec<u8>),
}

async fn still_valid(db: &sqlx::PgPool, c: &Cred) -> sqlx::Result<bool> {
    match c {
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
    }
}

/// 心跳：每隔这么久发一次 ping（另带一个文本心跳 `{"type":"hb"}`），到下一次 ping 还没收到 pong 就当连接已死
const PING_EVERY: Duration = Duration::from_secs(15);
const RECHECK_EVERY: Duration = Duration::from_secs(60);
/// 会话/密钥失效时的关闭码（4000–4999 归应用自用）
pub const CLOSE_REVOKED: u16 = 4401;

/// 实时流（WebSocket，JSON 文本帧）：资源变更通知 + 实时指标（`live.rs`）。
/// 广播落后丢了消息时发 `resync`（前端让全部查询失效）；每 60 s 复核一次会话/密钥，被撤销就以 4401 关闭；
/// 撤销会话 / 密钥的地方另发 `Event::Revoked`，各连接马上复核。
pub async fn stream(a: Actor, State(st): State<AppState>, jar: CookieJar, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    // 会话（Cookie）连接要求 Origin 是中控自己（public_url 的协议 + 主机 + 端口）：WebSocket 握手不受同源策略约束，
    // 别的页面能借浏览器自动带上的 Cookie 连上来读实时数据。API 密钥不是浏览器带的，不查
    if a.is_session() {
        let want = reqwest::Url::parse(&st.cfg.public_url).map(|u| u.origin().ascii_serialization()).unwrap_or_default();
        if headers.get("origin").and_then(|v| v.to_str().ok()) != Some(want.as_str()) {
            return ApiError::forbidden("实时流只接受本站页面的连接（Origin 不符）").into_response();
        }
    }
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
                // 有会话 / 密钥被撤销：马上复核，不等下一轮
                Ok(Event::Revoked) => {
                    if !keep(&mut ws, &db, &cred).await {
                        return;
                    }
                    continue;
                }
                Ok(e) => e.to_text(),
                Err(RecvError::Lagged(n)) => {
                    tracing::debug!("实时流订阅者落后 {n} 条消息，通知前端重新同步");
                    // 丢的消息里可能有 Revoked
                    if !keep(&mut ws, &db, &cred).await {
                        return;
                    }
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
                // 协议层的 ping 浏览器脚本看不到：另发一个文本心跳，前端据此发现半开的连接（web/src/lib/stream.ts）
                if pong_due
                    || !send(&mut ws, Message::Ping(Default::default())).await
                    || !send(&mut ws, Message::Text(Utf8Bytes::from_static(r#"{"type":"hb"}"#))).await
                {
                    return;
                }
                pong_due = true;
                continue;
            }
            _ = check.tick() => {
                if !keep(&mut ws, &db, &cred).await {
                    return;
                }
                continue;
            }
        };
        if !send(&mut ws, Message::Text(text)).await {
            return;
        }
    }
}

/// 复核会话 / 密钥：仍有效返回 true；已失效以 4401 关闭、复核查询失败以 1011 关闭（客户端重连时重新鉴权），返回 false
async fn keep(ws: &mut WebSocket, db: &sqlx::PgPool, cred: &Option<Cred>) -> bool {
    let Some(c) = cred else { return true };
    let close = match still_valid(db, c).await {
        Ok(true) => return true,
        Ok(false) => CloseFrame { code: CLOSE_REVOKED, reason: "会话或密钥已失效".into() },
        Err(e) => {
            tracing::warn!("实时流复核会话失败：{e}");
            CloseFrame { code: 1011, reason: "复核会话失败，请重连".into() }
        }
    };
    send(ws, Message::Close(Some(close))).await;
    false
}

/// 发一帧；对端卡住（不读、TCP 窗口满）超过一个心跳周期就当连接已死，返回 false 由调用方断开
async fn send(ws: &mut WebSocket, m: Message) -> bool {
    matches!(tokio::time::timeout(PING_EVERY, ws.send(m)).await, Ok(Ok(())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_audit_action_has_title() {
        for &a in AUDIT_ACTIONS {
            assert_ne!(audit_text(a, &json!({})).0, a, "{a} 没有中文标题");
        }
    }

    #[test]
    fn retention_summary_says_from_and_to() {
        let s = |from: Value, to: Value| audit_text("settings.access_retention", &json!({"from": from, "to": to})).1;
        assert_eq!(s(json!({"raw_days": 7, "hourly_days": 31}), json!({"raw_days": 3, "hourly_days": 31})), "原始记录 7 → 3 天，汇总 31 天");
        assert_eq!(s(json!({"raw_days": 7, "hourly_days": 31}), json!({"raw_days": 7, "hourly_days": 60})), "原始记录 7 天，汇总 31 → 60 天");
        assert_eq!(s(json!({"raw_days": 7, "hourly_days": 31}), json!({"raw_days": 1, "hourly_days": 14})), "原始记录 7 → 1 天，汇总 31 → 14 天");
    }

    #[test]
    fn catalog_and_machine_summaries_name_the_object() {
        let t = |a: &str, d: Value| audit_text(a, &d);
        assert_eq!(t("team.update", json!({"before": {"name": "甲", "sort_order": 200}, "after": {"name": "甲改", "sort_order": 50}})).1, "甲 → 甲改，排序 200 → 50");
        assert_eq!(t("team.update", json!({"before": {"name": "甲", "sort_order": 1}, "after": {"name": "甲", "sort_order": 2}})).1, "甲，排序 1 → 2");
        let arch = json!({"name": "ATTVPS 基础版", "currency": "USD", "before": {"archived_at": null}, "after": {"archived_at": "2026-10-22T00:00:00Z"}});
        assert_eq!(t("plan.update", arch), ("归档套餐".to_string(), "ATTVPS 基础版".to_string()));
        let (title, s) = t("plan.update", json!({"name": "季付", "currency": "USD", "before": {"price_cents": 5900, "traffic_gb": 800}, "after": {"price_cents": 6900, "traffic_gb": 1000}}));
        assert_eq!((title.as_str(), s.as_str()), ("修改套餐", "季付 · 价格 59.00 → 69.00 USD，流量 800 → 1000 GB"));
        assert_eq!(t("machine.rename", json!({"name": "团队1-03", "old_name": "团队1-07"})).1, "团队1-07 → 团队1-03");
        assert_eq!(t("policy.draft", json!({"name": "团队1", "mode": "denylist", "domains": 2, "cleared": false})).1, "团队1 · 黑名单 · 2 个域名");
        let ch = |name: &str, f: &str, fd: i64, to: &str, td: i64| json!({"name": name, "from": {"mode": f, "domains": fd}, "to": {"mode": to, "domains": td}});
        let changes = json!([ch("团队1", "denylist", 2, "observe", 0), ch("团队2-03", "allowlist", 2, "allowlist", 3)]);
        assert_eq!(
            t("policy.publish", json!({"seq": 5, "rollback_of_seq": null, "changes": changes, "relays": 6})).1,
            "v5 · 团队1：黑名单 → 仅观察；团队2-03：白名单 · 域名 2 → 3 个 · 6 台线路机"
        );
        assert_eq!(t("policy.rollback", json!({"seq": 6, "rollback_of_seq": 4, "changes": [ch("团队1", "observe", 0, "denylist", 2)], "relays": 0})).1, "v6 · 回滚到 v4 · 团队1：仅观察 → 黑名单（2 个域名）");
        // 同模式同个数：分得清只改了备注、换了域名、两样都改
        let same = |dc: bool, nc: bool| {
            let mut c = ch("团队1", "denylist", 2, "denylist", 2);
            c["domains_changed"] = json!(dc);
            c["note_changed"] = json!(nc);
            t("policy.publish", json!({"seq": 7, "changes": [c], "relays": 0})).1
        };
        assert_eq!(same(false, true), "v7 · 团队1：黑名单 · 改了备注");
        assert_eq!(same(true, false), "v7 · 团队1：黑名单 · 换了域名（2 个）");
        assert_eq!(same(true, true), "v7 · 团队1：黑名单 · 换了域名（2 个），改了备注");
        assert_eq!(t("silence.create", json!({"is_maintenance": true})).0, "新建维护窗口");
        let sub = |b: Value, a: Value| t("subscription.set", json!({"name": "C-01", "before": b, "after": a})).1;
        let b = json!({"due_date": "2026-11-01", "price_cents": 500, "currency": "USD", "cycle": "monthly", "traffic_gb": null, "remind_days": 7, "auto_renew": false});
        let mut a = b.clone();
        for (k, v) in [("due_date", json!("2026-12-01")), ("price_cents", json!(600)), ("traffic_gb", json!(1000)), ("auto_renew", json!(true))] {
            a[k] = v;
        }
        assert_eq!(sub(b.clone(), a), "C-01 · 到期 2026-11-01 → 2026-12-01，价格 5.00 → 6.00 USD，流量 不限 → 1000 GB，自动续费 关 → 开");
        assert_eq!(sub(b.clone(), b.clone()), "C-01 · 没有改动");
        let mut a = b.clone();
        a["price_cents"] = json!(4000);
        a["currency"] = json!("CNY");
        assert_eq!(sub(b.clone(), a), "C-01 · 价格 5.00 USD → 40.00 CNY，币种 USD → CNY", "旧价格用旧币种");
        assert_eq!(t("subscription.set", json!({"due_date": "2026-11-01"})).1, "", "旧记录没有改动前后");
        assert_eq!(t("agent.upgrade", json!({"count": 12, "skipped": 1})).1, "12 台，跳过 1 台");
        assert_eq!(t("agent.upgrade", json!({"count": 1, "skipped": 0, "names": ["团队1-03"]})).1, "团队1-03");
        let three = json!(["团队1-03", "团队1-04", "线路机2"]);
        assert_eq!(t("agent.upgrade", json!({"count": 5, "skipped": 1, "names": three})).1, "5 台（团队1-03、团队1-04、线路机2、…），跳过 1 台");
        assert_eq!(t("agent.upgrade", json!({"count": 3, "skipped": 0, "names": three})).1, "3 台（团队1-03、团队1-04、线路机2）");
        assert_eq!(t("job.cancel", json!({"kind": "move_entry", "name": "模拟团队C-21"})), ("取消作业".to_string(), "换线路机 · 模拟团队C-21".to_string()));
        assert_eq!(t("job.retry", json!({"kind": "upgrade_agent", "name": null, "step": 0})).1, "升级 agent");
        assert_eq!(t("job.cancel", json!({})).1, "");
    }

    #[test]
    fn exec_summary_shows_command_and_exit_code() {
        let long = format!("echo {}\nsecond line", "x".repeat(100));
        let (_, s) = audit_text("exec", &json!({"name": "团队1-01", "command": long, "exit_code": 3, "timed_out": false}));
        assert!(s.starts_with("团队1-01 · echo x") && s.contains("…") && s.ends_with("退出码 3") && !s.contains("second"), "{s}");
        let (_, s) = audit_text("exec", &json!({"name": "n", "command": "uptime", "exit_code": 0, "timed_out": false}));
        assert_eq!(s, "n · uptime · 退出码 0");
        let (_, s) = audit_text("exec", &json!({"name": "n", "command": "uptime", "error": "连不上"}));
        assert_eq!(s, "n · uptime · 失败：连不上");
    }
}
