//! 告警列表与确认、静默与维护窗口
use super::{ok, paged};
use crate::auth::Actor;
use crate::db::{audit as audit_log, new_id, Args};
use crate::incident::Engine;
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::routers;
use crate::state::{AppState, Event};
use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Serialize, sqlx::FromRow)]
struct IncidentOut {
    id: Uuid,
    /// 写这条告警的引擎（incident::Engine：machine / ipintel / central / router / probe）：前端按它决定点对象时开什么
    engine: String,
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
    /// 机器告警的对象是哪类机器（relay / vps / proxy / ip），建议操作用；机器已不在用或进了备用团队为 None（不给针对它的建议）
    #[serde(skip)]
    subject_kind: Option<String>,
}

/// 近 7 天同对象同规则开过几次算「反复」（老毛病）：告警带 `chronic`，status 的「近 7 天反复」、换线建议都按它；只在这里定义
pub const CHRONIC_MIN: i64 = 3;

#[derive(Deserialize)]
pub struct IncQ {
    /// firing（默认）| resolved | all | chronic（近 7 天反复：每个（规则, 对象）取最近一条，含已恢复的）
    state: Option<String>,
    /// 只看这台机器的告警
    subject_id: Option<Uuid>,
    limit: Option<i64>,
    offset: Option<i64>,
}

pub async fn incidents(_a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<IncQ>) -> ApiResult<Json<Value>> {
    let f = match q.state.as_deref() {
        Some("resolved") => "i.state = 'resolved'".to_string(),
        Some("all") => "true".to_string(),
        // 组 = 引擎 + 规则 + 对象，近 7 天出现过 CHRONIC_MIN 次以上的（开过的条数 + 30 分钟内复发合并进去的次数，tcp_fail 只数严重的：incident::occurrences_sql），每组只取最近一条
        Some("chronic") => format!(
            "i.id in (select id from (select id, row_number() over (partition by engine, rule, subject_id order by opened_at desc, id desc) rn,
                                                     sum({}) over (partition by engine, rule, subject_id) c
                                      from incident where opened_at > now() - interval '7 days') x
                      where rn = 1 and c >= {CHRONIC_MIN})",
            crate::incident::occurrences_sql()
        ),
        _ => "i.state = 'firing'".to_string(),
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
        "select i.id, i.engine, i.rule, i.subject_id,
            case when i.engine = 'central' then '中控' when i.engine = 'router' then r.name
                 when i.engine = 'probe' then coalesce(pp.name, '探测点（已删除）')
                 else display_name(m.kind::text, m.label, t.name, m.number) end subject_name,
            i.severity::text severity, i.state::text state, i.summary, i.opened_at, i.last_seen_at, i.resolved_at, i.ack_at, i.ack_by, i.reopen_count,
            case when i.engine = 'machine' and m.lifecycle = 'active' and not coalesce(t.spare, false) then m.kind::text end subject_kind
         from incident i left join machine m on m.id = i.subject_id left join team t on t.id = m.team_id
              left join router r on r.id = i.subject_id and i.engine = 'router'
              left join probe_point pp on pp.id = i.subject_id and i.engine = 'probe'
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
    // 建议操作要看软路由最近一次采集的（出口不通时切到哪个出口）：有这类告警才读
    let routers = if v.iter().any(|i| i.state == "firing" && matches!(i.rule.as_str(), "egress_fail" | crate::anomaly::SPIKE_RULE | routers::alerts::EXITS_DOWN)) {
        routers::load(&st.db, None).await?
    } else {
        vec![]
    };
    // 近 7 天同对象同规则出现过几次（含这次）：开过的条数 + 恢复后 30 分钟内复发合并进这些条的次数，tcp_fail 只数严重的（同可用率页、月报）。一条分组查询，不逐条查
    let subjects: Vec<Uuid> = v.iter().map(|i| i.subject_id).collect();
    let recent: std::collections::HashMap<(String, String, Uuid), i64> = sqlx::query_as::<_, (String, String, Uuid, i64)>(&format!(
        "select engine, rule, subject_id, sum({})::bigint from incident
         where subject_id = any($1) and opened_at > now() - interval '7 days' group by engine, rule, subject_id",
        crate::incident::occurrences_sql()
    ))
    .bind(&subjects)
    .fetch_all(&st.db)
    .await?
    .into_iter()
    .map(|(e, r, s, n)| ((e, r, s), n))
    .collect();
    let out: Vec<Value> = v
        .iter()
        .map(|i| {
            let mut j = serde_json::to_value(i).unwrap_or_default();
            let recent_count = recent.get(&(i.engine.clone(), i.rule.clone(), i.subject_id)).copied().unwrap_or(1).max(1);
            let chronic = recent_count >= CHRONIC_MIN;
            j["recent_count"] = json!(recent_count);
            j["chronic"] = json!(chronic);
            if i.engine == Engine::Central.as_str() || i.engine == Engine::Probe.as_str() {
                // 中控自身、探测点的告警：对象不是机器，没有可点开的机器（探测点的名字在 subject_name）
                j["subject"] = Value::Null;
                j["subject_id"] = Value::Null;
            } else if i.engine == Engine::Router.as_str() {
                // 软路由的告警：对象是路由，前端按 kind 打开路由而不是机器
                j["subject"] = crate::routers::alerts::subject(i.subject_id, i.subject_name.as_deref());
            } else {
                j["subject"] = json!({"id": i.subject_id, "name": i.subject_name.clone().unwrap_or_else(|| "（已删除）".into())});
            }
            j["rule_name"] = json!(crate::meta::rule_label_of(&i.rule, &i.severity));
            let mut advice = if i.state == "firing" { suggest(i, &routers) } else { vec![] };
            // 节点反复连不上 / 出口不通：先看换到哪台线路机更合适（已恢复的也给，status 的「近 7 天反复」用）
            if let (true, true, Some(name)) = (chronic, matches!(i.rule.as_str(), "tcp_fail" | "egress_fail") && i.subject_kind.as_deref() == Some("vps"), i.subject_name.as_deref()) {
                advice.push(Advice { cmd: format!("fleetctl placement --node {}", sh(name)), switch: None });
            }
            j["suggested"] = json!(advice.iter().map(|a| &a.cmd).collect::<Vec<_>>());
            j["quick_switch"] = json!(advice.iter().filter_map(|a| a.switch.as_ref()).collect::<Vec<_>>());
            j
        })
        .collect();
    let mut r = paged(out, total, offset);
    r.0["data"]["acked"] = json!(acked);
    Ok(r)
}

// ── 建议操作 ──────────────────────────────────────────────────────────────

/// 一条建议：可以直接执行的 fleetctl 命令；临时切换另给结构化的一份（网页的「临时切到…」用）
struct Advice {
    cmd: String,
    switch: Option<Value>,
}

fn cmd(c: String) -> Vec<Advice> { vec![Advice { cmd: c, switch: None }] }

/// 告警规则 → 建议操作。只这几条：出口不通 → 把用着它的 select 组临时切到别的出口；参数不一致 → 同步；线路机额度吃紧 → 搬迁建议；agent 离线 → 看日志；
/// 非业务流量偏高 → 看日志、看连接与防火墙；业务流量突增 → 同出口不通（切走用着它的 select 组），没有组在用它时看近 7 天每日流量；
/// 持续握手丢包 → `fleetctl diag`
fn suggest(i: &IncidentOut, routers: &[routers::Row]) -> Vec<Advice> {
    let Some(name) = i.subject_name.as_deref() else { return vec![] };
    match i.rule.as_str() {
        "egress_fail" => switches(routers, None, |x| x.entry.as_ref().is_some_and(|e| e.node_id == i.subject_id)),
        routers::alerts::EXITS_DOWN => switches(routers, Some(i.subject_id), |x| x.sync != "unmanaged" && x.fail_streak >= 2),
        routers::alerts::EXIT_DRIFT => cmd(format!("fleetctl router sync {}", sh(name))),
        "traffic_quota_high" if i.subject_kind.as_deref() == Some("relay") => cmd(format!("fleetctl rebalance {}", sh(name))),
        "agent_offline" => cmd(format!("fleetctl logs {}", sh(name))),
        // 持续握手丢包：先拿双向 ping / mtr / 握手的原始数据分清入站过滤还是链路问题（可直接发给供应商）
        crate::tcp_loss::RULE => cmd(format!("fleetctl diag {}", sh(name))),
        crate::anomaly::NONBIZ_RULE => vec![
            Advice { cmd: format!("fleetctl logs {}", sh(name)), switch: None },
            Advice { cmd: format!("fleetctl exec {} -- 'ss -s; nft list ruleset | head -50'", sh(name)), switch: None },
        ],
        crate::anomaly::SPIKE_RULE => {
            let s = switches(routers, None, |x| x.entry.as_ref().is_some_and(|e| e.node_id == i.subject_id));
            if s.is_empty() { cmd(format!("fleetctl traffic --node {} --days 7", sh(name))) } else { s }
        }
        _ => vec![],
    }
}

/// 路由（`only` 给了就只看这台）上当前选中「坏」出口的 select 组：切到组里第一个能用（测通或没测过）、不是坏的出口，4 小时后自动切回
fn switches(routers: &[routers::Row], only: Option<Uuid>, bad: impl Fn(&routers::collect::Exit) -> bool) -> Vec<Advice> {
    let mut out = vec![];
    for r in routers.iter().filter(|r| only.is_none_or(|id| id == r.id) && r.state() == "online") {
        let Some(snap) = r.snapshot() else { continue };
        let exit = |n: &str| snap.exits.iter().find(|x| x.name == n);
        for g in snap.groups.iter().filter(|g| g.selectable() && g.used != Some(false)) {
            let Some(now) = g.now.as_deref().filter(|n| exit(n).is_some_and(&bad)) else { continue };
            // fleet 的出口优先，其次客户自己的
            let ok = |x: &&routers::collect::Exit| !bad(x) && routers::collect::usable(x);
            let cands: Vec<&routers::collect::Exit> = g.all.iter().filter_map(|n| exit(n)).filter(ok).collect();
            let Some(to) = cands.iter().find(|x| x.sync != "unmanaged").or(cands.first()) else { continue };
            out.push(Advice {
                cmd: format!("fleetctl router switch {} {} {} --for 4h", sh(&r.name), sh(&g.name), sh(&to.name)),
                switch: Some(json!({"router_id": r.id, "router": r.name, "group": g.name, "from": now, "to": to.name})),
            });
        }
    }
    out
}

/// 命令行参数：只有字母数字（含中文）与 -_.:/@+=, 的原样，否则加单引号
pub(crate) fn sh(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || "-_.:/@+=,".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
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
    /// 对象类型：machine / router / probe_point（[`KINDS`]），全部为 null；新建时给了 subject_id、没给类型的按机器
    #[serde(default)]
    subject_kind: Option<String>,
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
        self.rule_name = self.rule.as_deref().map(|r| crate::meta::rule_label(r).to_string());
        self
    }
}

/// 静默对象的类型：(接口里的 subject_kind, 叫法, 取名字的查询)。silence 表每种一列外键（machine_id / router_id / probe_point_id，至多填一列）
const KINDS: [(&str, &str, &str); 3] = [
    ("machine", "机器", "select display_name(m.kind::text, m.label, t.name, m.number) from machine m left join team t on t.id = m.team_id where m.id = $1"),
    ("router", "软路由", "select name from router where id = $1"),
    ("probe_point", "探测点", "select name from probe_point where id = $1"),
];

/// 一条告警规则属于哪类对象（静默只针对一个对象时，规则要是这类对象的）；中控自身的规则（central_）没有对象，只能用在「全部」上
fn rule_kind(rule: &str) -> Option<&'static str> {
    if rule.starts_with("router_") {
        Some("router")
    } else if rule.starts_with("probe_point_") {
        Some("probe_point")
    } else if rule.starts_with("central_") {
        None
    } else {
        Some("machine")
    }
}

/// 静默对象在通知、审计里的叫法：机器是显示名，软路由、探测点带上类型（「软路由「客户A」」），全部是「全部机器」（全局静默对软路由、探测点同样生效）
pub(super) fn subject_label(kind: Option<&str>, name: Option<&str>) -> String {
    match (kind.and_then(|k| KINDS.iter().find(|x| x.0 == k)), name) {
        (_, None) => "全部机器".into(),
        (Some((k, what, _)), Some(n)) if *k != "machine" => format!("{what}「{n}」"),
        (_, Some(n)) => n.into(),
    }
}

/// 列表、删除时读出的一行：对象类型、对象 id、对象名（机器同告警列表的 subject_name，软路由、探测点是名字）
const COLS: &str = "s.id,
    case when s.machine_id is not null then 'machine' when s.router_id is not null then 'router' when s.probe_point_id is not null then 'probe_point' end subject_kind,
    coalesce(s.machine_id, s.router_id, s.probe_point_id) subject_id, s.rule, s.starts_at, s.ends_at, s.is_maintenance, s.comment,
    coalesce((select display_name(m.kind::text, m.label, t.name, m.number) from machine m left join team t on t.id = m.team_id where m.id = s.machine_id),
             (select r.name from router r where r.id = s.router_id), (select p.name from probe_point p where p.id = s.probe_point_id)) subject_name";

/// 没结束的静默（含还没开始的预约），按开始时间排：进行中的自然在前，已预约的随后
pub async fn list_silences(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let v: Vec<Silence> = sqlx::query_as(&format!("select {COLS} from silence s where s.deleted_at is null and s.ends_at > now() order by s.starts_at"))
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
    let kind = match (b.subject_id, b.subject_kind.as_deref()) {
        (None, None) => None,
        (None, Some(_)) => return Err(ApiError::bad_request("给了 subject_kind 就要给 subject_id；全部对象两个都不给")),
        (Some(_), k) => Some(KINDS.iter().find(|x| x.0 == k.unwrap_or("machine")).ok_or_else(|| ApiError::bad_request("subject_kind 只能是 machine / router / probe_point"))?),
    };
    if let Some(r) = b.rule.as_deref() {
        // rule_name 对不认识的规则原样返回
        if crate::meta::rule_label(r) == r {
            return Err(ApiError::bad_request(format!("没有「{r}」这条告警规则")));
        }
        // 维护是整个对象的（卡片显示「维护中」、暂停健康判定 / 冻结告警），只静默一条规则时说不通
        if b.is_maintenance {
            return Err(ApiError::bad_request("维护窗口针对整台机器，不能只选一条规则；只想静默这条告警就不要标记为维护"));
        }
        if let Some((_, what, _)) = kind.filter(|(k, ..)| rule_kind(r) != Some(*k)) {
            return Err(ApiError::bad_request(format!("「{}」不是{what}的告警规则", crate::meta::rule_label(r))));
        }
    }
    let subject_name: Option<String> = match (kind, b.subject_id) {
        (Some((_, what, sql)), Some(sid)) => Some(
            sqlx::query_scalar(sql).bind(sid).fetch_optional(&st.db).await?.ok_or_else(|| ApiError::bad_request(format!("subject_id 指向的{what}不存在")))?,
        ),
        _ => None,
    };
    let col = |k: &str| b.subject_id.filter(|_| kind.is_some_and(|x| x.0 == k));
    let id = new_id();
    let mut tx = st.db.begin().await?;
    sqlx::query(
        "insert into silence (id, machine_id, router_id, probe_point_id, rule, starts_at, ends_at, is_maintenance, comment, created_by)
         values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    )
    .bind(id)
    .bind(col("machine"))
    .bind(col("router"))
    .bind(col("probe_point"))
    .bind(&b.rule)
    .bind(b.starts_at)
    .bind(b.ends_at)
    .bind(b.is_maintenance)
    .bind(&b.comment)
    .bind(&a.name)
    .execute(&mut *tx)
    .await?;
    // 审计详情就是这条静默本身（对象类型与名字、规则、起止、备注），摘要见 audit_text
    let s = Silence { id: Some(id), subject_kind: kind.map(|k| k.0.to_string()), subject_name, ..b }.named();
    audit_log(&mut tx, &a.name, "silence.create", "silence", Some(id), json!(s)).await?;
    tx.commit().await?;
    tell_channels(&st, &a.name, false, &s).await;
    changed(&st, &s);
    Ok(ok(json!({"id": id})))
}

pub async fn delete_silence(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let s: Silence = sqlx::query_as(&format!("update silence s set deleted_at = now() where s.id = $1 and s.deleted_at is null returning {COLS}"))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这条静默"))?;
    let s = s.named();
    audit_log(&mut tx, &a.name, "silence.delete", "silence", Some(id), json!(s)).await?;
    tx.commit().await?;
    tell_channels(&st, &a.name, true, &s).await;
    changed(&st, &s);
    Ok(ok(json!({"ok": true})))
}

/// 静默变了：告警页重取。机器卡片上的「维护中」随 Incidents 标记的实时指标比对送达（最多 3 s），不发 Machines 让前端整表重取；
/// 软路由的（单台或全部的）再发 Routers，路由卡片跟上（窗口自己到期、预约的到点没有这一步，由软路由告警评估每 30 秒比对发，见 routers/alerts.rs）；探测点列表每 30 秒自己重取
fn changed(st: &AppState, s: &Silence) {
    st.emit(Event::Incidents);
    // 机器告警按新的静默 / 维护马上评估一次（软路由的随它自己的评估）
    st.wake_alerts();
    if s.subject_kind.as_deref().is_none_or(|k| k == "router") {
        st.emit(Event::Routers);
    }
}

/// 新建 / 删除静默、维护窗口时告诉所有启用的渠道（不看渠道的最低级别，也不受任何静默影响），免得有人悄悄开个全局维护、监控就没了：
/// 全部机器的一律发（含预约的）；只针对一个对象（机器、软路由、探测点）的，此刻正生效的才发（预约的、已经结束的不影响现在的告警）
async fn tell_channels(st: &AppState, actor: &str, deleted: bool, s: &Silence) {
    let now = OffsetDateTime::now_utc();
    if s.subject_id.is_some() && !(s.starts_at <= now && now < s.ends_at) {
        return;
    }
    let what = if s.is_maintenance { "维护窗口" } else { "静默" };
    let object = subject_label(s.subject_kind.as_deref(), s.subject_name.as_deref());
    let rules = match (&s.rule_name, s.is_maintenance) {
        (Some(r), _) => format!("（只静默「{r}」）"),
        (None, true) => String::new(),
        (None, false) => "（全部规则）".into(),
    };
    let mut text = format!(
        "{actor} {}了{what}\n对象：{object}{rules}\n起止：{} ~ {}（{}）",
        if deleted { "删除" } else { "新建" },
        crate::meta::org_time(s.starts_at),
        crate::meta::org_time(s.ends_at),
        crate::meta::org_tz_label()
    );
    // 备注收成一行、最多 200 个字
    let comment: String = s.comment.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(200).collect();
    if !comment.is_empty() {
        text.push_str(&format!("\n备注：{comment}"));
    }
    text.push_str(match (deleted, s.is_maintenance, s.subject_kind.as_deref()) {
        (true, ..) => "\n告警与通知照常恢复",
        // 软路由、探测点没有健康判定：维护是冻结开着的告警（incident.rs）
        (false, true, Some("router" | "probe_point")) => "\n期间不开新告警、开着的保持原状、不发通知",
        (false, true, _) => "\n期间不开告警、不发通知，健康判定暂停",
        (false, false, _) => "\n期间不开告警、不发通知",
    });
    let n = crate::notify::Notice {
        kind: crate::notify::Kind::Silence,
        incident_id: None,
        rule: s.rule.clone().unwrap_or_default(),
        severity: "info".into(),
        machine: object,
        summary: text,
        at: now,
        opened_at: None,
    };
    crate::notify::enqueue(st, n).await;
}

#[cfg(test)]
mod tests {
    use super::sh;

    #[test]
    fn sh_quotes_single_quote() {
        assert_eq!(sh("线路-1"), "线路-1");
        assert_eq!(sh("a'b c"), "'a'\\''b c'");
    }
}
