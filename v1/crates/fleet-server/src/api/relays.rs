use crate::error::ApiJson;
use super::ok;
use crate::auth::Actor;
use crate::db::audit;
use crate::entries;
use crate::error::{ApiError, ApiPath, ApiResult};
use crate::jobs;
use crate::state::{AppState, Event};
use crate::views;
use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use uuid::Uuid;

pub async fn list(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let all = views::live(&st).await?;
    let v: Vec<_> = all.iter().filter(|m| m.kind == "relay").collect();
    Ok(ok(v))
}

#[derive(Deserialize)]
pub struct ActionIn {
    action: String,
    confirm: Option<String>,
}

/// 启用 / 停用 / 轮换凭据。轮换要输入「线路机:端口」确认，没确认时的 400 里列出指向这个入口的软路由出口（detail.router_exits，同 GET /entries/{id}/routers）；
/// 停用、轮换的结果也带上 router_exits：这些出口之后就连不上了（替换节点、换线路机会自动改路由，轮换、停用不会）：轮换的用 router sync 同步新凭据，停用的用 repoint 改指向
pub async fn entry_action(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<ActionIn>) -> ApiResult<Json<Value>> {
    let (relay_label, port): (String, i32) =
        sqlx::query_as("select m.label, e.port from entry e join machine m on m.id = e.relay_id where e.id = $1")
            .bind(id)
            .fetch_optional(&st.db)
            .await?
            .ok_or_else(|| ApiError::not_found("没有这个入口"))?;
    let (to, rotate) = match b.action.as_str() {
        "enable" => ("active", false),
        "disable" => ("disabled", false),
        "rotate" => {
            let want = format!("{relay_label}:{port}");
            if b.confirm.as_deref().map(str::trim) != Some(want.as_str()) {
                let rs = crate::routers::exits_on_entry(&st.db, id).await?;
                let also = if rs.is_empty() { String::new() } else { format!("。这些软路由出口指向它，轮换后会连不上、要到路由上换成新凭据：{}", crate::routers::exits_text(&rs)) };
                return Err(ApiError::bad_request(format!("轮换凭据会让旧链接立即失效；请输入「{want}」确认{also}")).with_detail(json!({"router_exits": rs})));
            }
            let cur: (String,) = sqlx::query_as("select lifecycle::text from entry where id = $1").bind(id).fetch_one(&st.db).await?;
            if cur.0 == "disabled" {
                return Err(ApiError::conflict("入口停用中，先启用再轮换"));
            }
            ("active", true)
        }
        _ => return Err(ApiError::bad_request("action 只能是 enable / disable / rotate")),
    };
    let mut tx = st.db.begin().await?;
    let (relay_id, gen) = entries::transition(&mut tx, id, to, rotate).await?;
    audit(&mut tx, &a.name, &format!("entry.{}", b.action), "entry", Some(id), json!({"relay": relay_label, "port": port, "generation": gen})).await?;
    tx.commit().await?;
    st.relay_bumped(relay_id);
    st.emit(Event::Machines);
    let rs = if to == "active" && !rotate { vec![] } else { crate::routers::exits_on_entry(&st.db, id).await? };
    Ok(ok(json!({"generation": gen, "router_exits": rs})))
}

/// 指向这个入口的软路由出口（按各路由最近一次采集的快照，出口对上的入口就是它）：`[{id, name, state, exits: [出口名]}]`。
/// 轮换凭据、停用入口的确认里列出来
pub async fn entry_routers(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let n: Option<i32> = sqlx::query_scalar("select 1 from entry where id = $1").bind(id).fetch_optional(&st.db).await?;
    n.ok_or_else(|| ApiError::not_found("没有这个入口"))?;
    Ok(ok(crate::routers::exits_on_entry(&st.db, id).await?))
}

/// 一条入口的分享链接
#[derive(Clone)]
struct Share {
    /// Reality（vless://）：线路机只留 AnyTLS（relay.protocols = anytls）时没有
    url: Option<String>,
    /// AnyTLS 链接：线路机报了 sing-box 在跑、入口有 AnyTLS 端口时才有（entries::anytls_live）
    anytls_url: Option<String>,
    /// 入口名「节点名」或「线路机名-直出」
    name: String,
    relay: Option<String>,
    port: i32,
}

/// 一批入口的分享链接，一次查回，按 ids 的顺序；有一条查不到就是 404
async fn share_links(db: &sqlx::PgPool, ids: &[Uuid]) -> ApiResult<Vec<Share>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        id: Uuid,
        relay_id: Uuid,
        client_uuid: Uuid,
        short_id: String,
        port: i32,
        public_host: String,
        reality_sni: String,
        reality_public_key: String,
        kind: String,
        team: Option<String>,
        number: Option<i32>,
        label: Option<String>,
        direct: bool,
        relay_label: Option<String>,
        protocols: String,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "select e.id, e.relay_id, e.client_uuid, e.short_id, e.port, r.public_host, r.reality_sni, r.reality_public_key, r.protocols,
                n.kind::text kind, t.name team, n.number, n.label, e.node_id = e.relay_id direct, rm.label relay_label
         from entry e join relay r on r.machine_id = e.relay_id join machine rm on rm.id = e.relay_id join machine n on n.id = e.node_id
         left join team t on t.id = n.team_id where e.id = any($1) and e.lifecycle <> 'removed'",
    )
    .bind(ids)
    .fetch_all(db)
    .await?;
    let mut relays: Vec<Uuid> = rows.iter().map(|r| r.relay_id).collect();
    relays.sort();
    relays.dedup();
    let at = if relays.is_empty() { HashMap::new() } else { entries::anytls_live(db, &relays).await? };
    let links: HashMap<Uuid, Share> = rows
        .into_iter()
        .map(|r| {
            let name = crate::names::entry_name(r.direct, &r.kind, r.label.as_deref(), r.team.as_deref(), r.number);
            let url = (r.protocols != entries::ANYTLS).then(|| entries::share_url(r.client_uuid, &r.public_host, r.port, &r.reality_sni, &r.reality_public_key, &r.short_id, &name));
            let anytls_url = at.get(&r.id).map(|&p| entries::anytls_url(r.client_uuid, &r.public_host, p, &r.reality_sni, &entries::anytls_name(&name)));
            (r.id, Share { url, anytls_url, name, relay: r.relay_label, port: r.port })
        })
        .collect();
    // 重复的 id 各出一条（同逐个取）
    ids.iter().map(|id| links.get(id).cloned().ok_or_else(|| ApiError::not_found("没有这个入口"))).collect()
}

pub async fn share(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    a.require_scope("secrets")?;
    let s = share_links(&st.db, &[id]).await?.swap_remove(0);
    let mut tx = st.db.begin().await?;
    // 记是哪条入口（同启用 / 停用的写法加入口名），不记链接本身
    audit(&mut tx, &a.name, "entry.share", "entry", Some(id), json!({"relay": s.relay, "port": s.port, "entry": s.name})).await?;
    tx.commit().await?;
    Ok(ok(json!({"url": s.url, "anytls_url": s.anytls_url})))
}

#[derive(Deserialize)]
pub struct ShareBatchIn {
    ids: Vec<Uuid>,
}

/// 批量取分享链接（`POST /entries/share`，要 secrets 权限）：一次请求、一条审计（列出入口名，不记链接）。
/// 返回 `{items: [{id, name, url, anytls_url}]}`（url 在线路机只留 AnyTLS 时为 null，anytls_url 没有时为 null），按请求的顺序；有一条不存在就整批失败
pub async fn share_batch(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<ShareBatchIn>) -> ApiResult<Json<Value>> {
    a.require_scope("secrets")?;
    if b.ids.is_empty() || b.ids.len() > 500 {
        return Err(ApiError::bad_request("一次取 1–500 条入口的链接"));
    }
    let links = share_links(&st.db, &b.ids).await?;
    let items: Vec<Value> = b.ids.iter().zip(links).map(|(id, s)| json!({"id": id, "name": s.name, "url": s.url, "anytls_url": s.anytls_url})).collect();
    let names: Vec<&Value> = items.iter().map(|i| &i["name"]).collect();
    let mut tx = st.db.begin().await?;
    audit(&mut tx, &a.name, "entry.share", "entry", None, json!({"entries": names})).await?;
    tx.commit().await?;
    Ok(ok(json!({"items": items})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayPatchIn {
    protocols: String,
}

/// 改线路机的入口协议（`PATCH /relays/{id}`，`{protocols: both|reality|anytls}`）：代数 +1、记审计 `relay.protocols`。
/// 改成只 AnyTLS（Reality 入站改听回环、公网只剩 AnyTLS）要先满足三条，否则 409：
/// ①线路机 agent 报了 sing-box 在跑、且认 `reality_public`（caps 有 reality_private），不然公网一个协议都没有或 Reality 照样开着；
/// ②这台线路机上没有穿透入口：节点的 bridge 连的是入口的公网 REALITY 端口；
/// ③没有软路由出口指着这台线路机上的入口：软路由（OpenClash）只用 Reality
pub async fn patch_relay(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<RelayPatchIn>) -> ApiResult<Json<Value>> {
    let p = b.protocols.trim();
    if ![entries::BOTH, entries::REALITY, entries::ANYTLS].contains(&p) {
        return Err(ApiError::bad_request("protocols 只能是 both、reality、anytls"));
    }
    // 先锁线路机那一行再查再改：开穿透（reverse::set_mode）、加入口等同样先锁它，三项检查与改动之间不会插进别的变更
    let mut tx = st.db.begin().await?;
    // (名称, 现在的协议, sing-box 在跑, agent 的 caps)
    type Cur = (Option<String>, String, Option<bool>, Option<Value>);
    let cur: Option<Cur> = sqlx::query_as(
        "select m.label, r.protocols, (a.info->'singbox'->>'running')::bool, a.info->'caps'
         from relay r join machine m on m.id = r.machine_id left join agent_state a on a.machine_id = r.machine_id where r.machine_id = $1 for update of r",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let (label, from, singbox, caps) = cur.ok_or_else(|| ApiError::not_found("没有这台线路机"))?;
    let name = label.unwrap_or_default();
    if p == entries::ANYTLS {
        if singbox != Some(true) {
            return Err(ApiError::conflict("线路机还没装好 AnyTLS（sing-box），先升级 agent"));
        }
        if !caps.as_ref().and_then(Value::as_array).is_some_and(|c| c.iter().any(|x| x == fleet_proto::CAP_REALITY_PRIVATE)) {
            return Err(ApiError::conflict("线路机的 agent 版本旧，不认「只留 AnyTLS」（Reality 会照样对公网开着），先升级 agent"));
        }
        // 穿透入口的节点 (kind, label, 团队, 编号) 与入口端口
        type Rev = (String, Option<String>, Option<String>, Option<i32>, i32);
        let rev: Vec<Rev> = sqlx::query_as(
            "select n.kind::text, n.label, t.name, n.number, e.port from entry e join machine n on n.id = e.node_id left join team t on t.id = n.team_id
             where e.relay_id = $1 and e.mode = $2 and e.lifecycle in ('pending', 'active', 'disabled') order by e.port",
        )
        .bind(id)
        .bind(crate::reverse::REVERSE)
        .fetch_all(&mut *tx)
        .await?;
        if !rev.is_empty() {
            let names: Vec<String> = rev.iter().map(|(k, l, t, n, port)| format!("{}（{port}）", crate::names::entry_name(false, k, l.as_deref(), t.as_deref(), *n))).collect();
            return Err(ApiError::conflict(format!("这台线路机上有穿透入口：{}。穿透的 bridge 连的是入口的公网 Reality 端口，先关掉穿透", names.join("、")))
                .with_detail(json!({"reverse_entries": names})));
        }
        let ids: Vec<Uuid> = sqlx::query_scalar("select id from entry where relay_id = $1 and lifecycle <> 'removed'").bind(id).fetch_all(&mut *tx).await?;
        let rs = crate::routers::exits_on_entries(&st.db, &ids).await?;
        if !rs.is_empty() {
            return Err(ApiError::conflict(format!("软路由只用 Reality，这些出口指着这台线路机上的入口：{}。先把它们改指到别的线路机", crate::routers::exits_text(&rs)))
                .with_detail(json!({"router_exits": rs})));
        }
    }
    if p == from {
        let g: i64 = sqlx::query_scalar("select generation from relay where machine_id = $1").bind(id).fetch_one(&mut *tx).await?;
        return Ok(ok(json!({"relay_id": id, "name": name, "protocols": p, "generation": g, "changed": false})));
    }
    sqlx::query("update relay set protocols = $2 where machine_id = $1").bind(id).bind(p).execute(&mut *tx).await?;
    let gen = crate::db::bump_relay(&mut tx, id).await?;
    audit(&mut tx, &a.name, "relay.protocols", "machine", Some(id), json!({"relay": name, "from": from, "to": p, "generation": gen})).await?;
    tx.commit().await?;
    st.relay_bumped(id);
    st.emit(Event::Machines);
    Ok(ok(json!({"relay_id": id, "name": name, "protocols": p, "generation": gen, "changed": true})))
}

#[derive(Deserialize)]
pub struct AddEntryIn {
    relay_id: Uuid,
    /// 入口模式 normal / reverse（穿透）；不填 = 面板设置的新入口默认模式。家宽代理、同机 IP 节点、直出入口一律普通（结果里 note 写明）
    mode: Option<String>,
}

/// 给已在用的节点再挂一条入口（作业：建入口 → 等线路机应用 → 等出口探测通过）。
/// 线路机给自己加（`relay_id` 就是它自己）= 直出入口：从线路机自己的 IP 出去，每台线路机至多一条
pub async fn add_entry(a: Actor, State(st): State<AppState>, ApiPath(node_id): ApiPath<Uuid>, ApiJson(b): ApiJson<AddEntryIn>) -> ApiResult<Json<Value>> {
    let (kind, lifecycle): (String, String) = sqlx::query_as("select kind::text, lifecycle::text from machine where id = $1")
        .bind(node_id)
        .fetch_optional(&st.db)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这台机器"))?;
    let direct = kind == "relay";
    if direct && b.relay_id != node_id {
        return Err(ApiError::conflict("线路机上只能加它自己的直出入口（relay_id 填这台线路机自己）"));
    }
    if lifecycle != "active" {
        return Err(ApiError::conflict(if direct { "只能给在用的线路机加直出入口" } else { "只能给在用的节点加入口" }));
    }
    // 入队前先拦下必败的请求，免得留一个失败作业
    let relay: Option<(String, Option<String>)> = sqlx::query_as("select lifecycle::text, label from machine where id = $1 and kind = 'relay'")
        .bind(b.relay_id)
        .fetch_optional(&st.db)
        .await?;
    match relay {
        // 请求体里引用的对象不存在是输入错误
        None => return Err(ApiError::bad_request("relay_id 指向的线路机不存在")),
        Some((lc, label)) if lc != "active" => {
            return Err(ApiError::conflict(format!("线路机 {} 不在用，不能在上面加入口", label.unwrap_or_default())));
        }
        Some(_) => {}
    }
    let dup: Option<(i32,)> = sqlx::query_as("select 1 from entry where node_id = $1 and relay_id = $2 and lifecycle <> 'removed'")
        .bind(node_id)
        .bind(b.relay_id)
        .fetch_optional(&st.db)
        .await?;
    if dup.is_some() {
        return Err(ApiError::conflict(if direct { "这台线路机已经有直出入口" } else { "这个节点在这台线路机上已经有入口" }));
    }
    // 入口模式：给了的用给的，没给按面板的默认；只看机器类型就知道不支持的退回普通（agent 太旧等由作业建入口时再核）
    let want = match b.mode.as_deref().filter(|m| !m.trim().is_empty()) {
        Some(m) => crate::reverse::parse_mode(m).map_err(ApiError::bad_request)?,
        None => crate::reverse::default_mode(&st.db).await?.0,
    };
    let host_id: Option<Uuid> = sqlx::query_scalar("select host_id from machine where id = $1").bind(node_id).fetch_one(&st.db).await?;
    let why: Option<String> = match (want == crate::reverse::REVERSE, direct) {
        (false, _) => None,
        (true, true) => Some("线路机直出入口没有落地节点，用不着穿透".into()),
        (true, false) => match crate::reverse::kind_unsupported(&kind, host_id) {
            Some(w) => Some(w.into()),
            None => crate::reverse::relay_unsupported(&st.db, b.relay_id).await?,
        },
    };
    let mode = if why.is_some() { crate::reverse::NORMAL } else { want };
    let note = why.map(|w| format!("按普通模式建入口：{w}"));
    let mut params = json!({"relay_id": b.relay_id, "entry_mode": mode});
    if let Some(n) = &note {
        params["entry_mode_note"] = json!(n);
    }
    let mut tx = st.db.begin().await?;
    let job = jobs::enqueue(&mut tx, "attach_entry", Some(node_id), params, &a.name).await?;
    audit(&mut tx, &a.name, "entry.add", "machine", Some(node_id), json!({"relay_id": b.relay_id, "job_id": job, "direct": direct, "mode": mode})).await?;
    tx.commit().await?;
    st.wake_jobs();
    st.emit(Event::Job { id: job, target: Some(node_id) });
    Ok(ok(json!({"job_id": job, "entry_mode": mode, "note": note})))
}
