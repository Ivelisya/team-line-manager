//! 客户端订阅：自己的客户端（Windows 上的 Clash Verge、OpenClash 的 proxy-provider、v2rayN / Shadowrocket）按一个链接拉入口列表。
//! 订阅 = 名称 + 令牌（库里只存 sha256，新建、轮换时只给一次完整链接）+ 选择规则（存团队 / 机器 id，不存入口参数，见迁移 0077）。
//! 拉取（`GET /sub/{令牌}`，不登录）按请求时的入口现算：替换节点、换线路机、轮换凭据后客户端更新订阅就是新的。
//! 管理接口都要 secrets 权限（同分享链接）；新建、修改、轮换、删除记审计，拉取不记（只节流写最近拉取时间与来源）

use super::ok;
use crate::auth::{self, Actor, ClientIp};
use crate::crypto::{random_token, sha256};
use crate::db::{audit, new_id};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::names;
use crate::state::AppState;
use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::IpAddr;
use time::OffsetDateTime;
use uuid::Uuid;

/// 选择规则（整份存、整份改）
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// all：全部团队（不含备用团队）；only：只要 team_ids 里的团队（列了备用团队就带上备用）；except：全部团队（不含备用团队）除了 team_ids
    #[serde(default = "Rule::all")]
    team_mode: String,
    #[serde(default)]
    team_ids: Vec<Uuid>,
    /// 带不带线路机直出入口
    #[serde(default = "Rule::yes")]
    direct: bool,
    /// 另外带上的机器（节点，或线路机 = 它的直出入口），不看团队与 direct
    #[serde(default)]
    include_ids: Vec<Uuid>,
    /// 一律不带的机器，优先于上面所有规则
    #[serde(default)]
    exclude_ids: Vec<Uuid>,
}

impl Default for Rule {
    fn default() -> Self { Self { team_mode: Self::all(), team_ids: vec![], direct: true, include_ids: vec![], exclude_ids: vec![] } }
}

impl Rule {
    fn all() -> String { "all".into() }
    fn yes() -> bool { true }

    /// 这条入口（按它的节点；直出入口的节点就是线路机自己）带不带
    fn picks(&self, c: &Cand) -> bool {
        if self.exclude_ids.contains(&c.node_id) {
            return false;
        }
        if self.include_ids.contains(&c.node_id) {
            return true;
        }
        if c.direct {
            return self.direct;
        }
        let listed = c.team_id.is_some_and(|t| self.team_ids.contains(&t));
        match self.team_mode.as_str() {
            "only" => listed,
            "except" => !c.spare && !listed,
            _ => !c.spare,
        }
    }

    /// 校验并去重：团队、机器要存在（引用不存在的 400）。在写订阅的同一个事务里调：团队行加共享锁直到提交，
    /// 并发的删团队（delete_team 删团队行后摘 team_ids）要么等这边提交后再摘，要么先删完让这边 400，不会把已删团队写回
    async fn check(mut self, db: &mut sqlx::PgConnection) -> ApiResult<Self> {
        if !matches!(self.team_mode.as_str(), "all" | "only" | "except") {
            return Err(ApiError::bad_request("team_mode 只能是 all、only、except"));
        }
        for l in [&mut self.team_ids, &mut self.include_ids, &mut self.exclude_ids] {
            l.sort();
            l.dedup();
        }
        if self.team_mode == "only" && self.team_ids.is_empty() {
            return Err(ApiError::bad_request("「只要这些团队」至少选一个团队"));
        }
        // 按 id 排序加锁：和别处成批锁团队（machines.rs、provision.rs 的 order by id for update）同序，多团队并发不成环
        let teams: Vec<Uuid> = sqlx::query_scalar("select id from team where id = any($1) order by id for share").bind(&self.team_ids).fetch_all(&mut *db).await?;
        if teams.len() != self.team_ids.len() {
            return Err(ApiError::bad_request("team_ids 里有不存在的团队"));
        }
        let ids: Vec<Uuid> = self.include_ids.iter().chain(&self.exclude_ids).copied().collect();
        // 机器行也加共享锁（按 id 排序）：和彻底删除（purge 先锁机器、再从订阅里摘掉）互斥，不会把正在删的机器写回订阅
        let ms: Vec<Uuid> = sqlx::query_scalar("select id from machine where id = any($1) order by id for share").bind(&ids).fetch_all(&mut *db).await?;
        if ms.len() != ids.iter().collect::<std::collections::HashSet<_>>().len() {
            return Err(ApiError::bad_request("include_ids / exclude_ids 里有不存在的机器"));
        }
        Ok(self)
    }
}

/// 一条可以进订阅的入口：在用，节点、线路机都在用（退役中、已退役、回收的不算；健康与否不管，客户端自己测）
#[derive(sqlx::FromRow)]
struct Cand {
    entry_id: Uuid,
    node_id: Uuid,
    team_id: Option<Uuid>,
    spare: bool,
    direct: bool,
    port: i32,
    client_uuid: Uuid,
    short_id: String,
    host: String,
    sni: String,
    pbk: String,
    kind: String,
    team: Option<String>,
    number: Option<i32>,
    label: Option<String>,
    ip: String,
    country: Option<String>,
    provider: Option<String>,
    relay: String,
}

/// 全部候选入口，按线路机直出在前、团队、编号、线路机排（同机器列表）。`country`：没手动指定国家的按 IP 查画像（同名字旁的国旗）
async fn candidates(st: &AppState, country: bool) -> ApiResult<Vec<Cand>> {
    let mut cs: Vec<Cand> = sqlx::query_as(
        "select e.id entry_id, e.node_id, n.team_id, coalesce(t.spare, false) spare, e.node_id = e.relay_id direct, e.port, e.client_uuid, e.short_id,
                r.public_host host, r.reality_sni sni, r.reality_public_key pbk, n.kind::text kind, t.name team, n.number, n.label,
                host(n.primary_ip) ip, n.country, p.name provider, rm.label relay
         from entry e join relay r on r.machine_id = e.relay_id join machine rm on rm.id = e.relay_id join machine n on n.id = e.node_id
         left join team t on t.id = n.team_id left join provider p on p.id = n.provider_id
         where e.lifecycle = 'active' and n.lifecycle = 'active' and rm.lifecycle = 'active'
         order by e.node_id = e.relay_id desc, t.sort_order nulls first, t.name, n.number, n.label, rm.label, e.port",
    )
    .fetch_all(&st.db)
    .await?;
    if country {
        let ips: Vec<IpAddr> = cs.iter().filter(|c| c.country.is_none()).filter_map(|c| c.ip.parse().ok()).collect();
        if !ips.is_empty() {
            let intel = st.ipintel.enrich(&st.db, &ips, true).await;
            for c in cs.iter_mut().filter(|c| c.country.is_none()) {
                c.country = c.ip.parse::<IpAddr>().ok().and_then(|ip| intel.get(&ip)).and_then(|b| b.country.clone());
            }
        }
    }
    Ok(cs)
}

/// 按规则选出的入口和它在客户端里的名字（names::sub_name）
fn pick<'a>(cs: &'a [Cand], rule: &Rule) -> Vec<(&'a Cand, String)> {
    let chosen: Vec<&Cand> = cs.iter().filter(|c| rule.picks(c)).collect();
    let mut per_node: HashMap<Uuid, usize> = HashMap::new();
    for c in &chosen {
        *per_node.entry(c.node_id).or_default() += 1;
    }
    chosen
        .into_iter()
        .map(|c| {
            let who = names::display_name(&c.kind, c.label.as_deref(), c.team.as_deref(), c.number);
            let via = (per_node[&c.node_id] > 1).then_some(c.relay.as_str());
            (c, names::sub_name(c.country.as_deref(), c.provider.as_deref(), &who, c.direct, via, &c.ip))
        })
        .collect()
}

/// mihomo 的 proxy-provider 文件（只有 `proxies:`）。字符串一律加双引号（JSON 字符串就是合法的 YAML 双引号标量）：short-id 写成 01234567 不会被当成数字
fn clash(items: &[(&Cand, String)]) -> String {
    let q = |s: &str| serde_json::to_string(s).unwrap_or_default();
    if items.is_empty() {
        return "proxies: []\n".into();
    }
    let mut o = String::from("proxies:\n");
    for (c, name) in items {
        o += &format!(
            "  - name: {}\n    type: vless\n    server: {}\n    port: {}\n    uuid: {}\n    network: tcp\n    tls: true\n    udp: true\n    flow: xtls-rprx-vision\n    servername: {}\n    client-fingerprint: chrome\n    reality-opts:\n      public-key: {}\n      short-id: {}\n",
            q(name),
            q(&c.host),
            c.port,
            c.client_uuid,
            q(&c.sni),
            q(&c.pbk),
            q(&c.short_id)
        );
    }
    o
}

/// vless:// 链接一行一条，整体 base64（v2rayN、Shadowrocket 的订阅格式）
fn base64_list(items: &[(&Cand, String)]) -> String {
    use base64::Engine;
    let lines: Vec<String> = items.iter().map(|(c, name)| crate::entries::share_url(c.client_uuid, &c.host, c.port, &c.sni, &c.pbk, &c.short_id, name)).collect();
    base64::engine::general_purpose::STANDARD.encode(lines.join("\n"))
}

// ── 拉取（不登录）──────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct FetchQ {
    format: Option<String>,
}

fn text(status: StatusCode, body: String) -> Response {
    (status, [(header::CONTENT_TYPE, "text/plain; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], body).into_response()
}

/// `GET /sub/{令牌}?format=clash|base64`（默认 clash）。令牌按 sha256 查库（比的是哈希，耗时和令牌内容无关，同 API 密钥）；
/// 不对或停用了回 404，并和登录共用按来源的失败限速（auth::begin_attempt：同一来源 15 分钟 20 次，含登录失败）。
/// 限速键按令牌的哈希分开：别人猜错的不会把对的令牌一起封掉。成功不记审计，最近拉取时间与来源 10 分钟内只写一次
pub async fn fetch(State(st): State<AppState>, ClientIp(ip): ClientIp, ApiPath(token): ApiPath<String>, ApiQuery(q): ApiQuery<FetchQ>) -> Response {
    let fmt = q.format.as_deref().unwrap_or("clash");
    if !matches!(fmt, "clash" | "base64") {
        return text(StatusCode::BAD_REQUEST, "format 只能是 clash 或 base64\n".into());
    }
    match fetch_inner(&st, ip, &token, fmt).await {
        Ok(r) => r,
        Err(e) if e.status == StatusCode::TOO_MANY_REQUESTS => {
            let mut r = text(e.status, "失败次数过多，请稍后再试\n".into());
            if let Some(s) = e.detail.as_ref().and_then(|d| d["retry_after_s"].as_i64()) {
                r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(s));
            }
            r
        }
        Err(e) => e.into_response(),
    }
}

async fn fetch_inner(st: &AppState, ip: Option<IpAddr>, token: &str, fmt: &str) -> ApiResult<Response> {
    let hash = sha256(token.as_bytes());
    let key = format!("sub:{}", &hex::encode(&hash)[..16]);
    let attempt = auth::begin_attempt(st, &key, ip.map(auth::ip_group)).await?;
    let row: Option<Row> = sqlx::query_as(&format!("select {COLS} from client_sub where token_hash = $1 and enabled")).bind(&hash).fetch_optional(&st.db).await?;
    // 不对：开头记下的那条失败留着
    let Some(Row { id, name, rule, .. }) = row else { return Ok(text(StatusCode::NOT_FOUND, "订阅链接不对或已停用\n".into())) };
    auth::forget_attempt(st, attempt).await?;
    sqlx::query(
        "update client_sub set last_fetch_at = now(), last_fetch_ip = $2
         where id = $1 and (last_fetch_at is null or last_fetch_at < now() - interval '10 minutes')",
    )
    .bind(id)
    .bind(ip.map(ipnetwork::IpNetwork::from))
    .execute(&st.db)
    .await?;
    let cs = candidates(st, true).await?;
    let items = pick(&cs, &rule);
    let body = if fmt == "base64" { base64_list(&items) } else { clash(&items) };
    let mut r = text(StatusCode::OK, body);
    let h = r.headers_mut();
    // Clash Verge 拿它当配置名、按它定时更新（小时）
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename*=UTF-8''{}", crate::entries::pct(&name))) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert("profile-update-interval", HeaderValue::from_static("6"));
    Ok(r)
}

// ── 管理（secrets 权限）────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    name: String,
    #[sqlx(flatten)]
    rule: Rule,
    enabled: bool,
    token_at: OffsetDateTime,
    last_fetch_at: Option<OffsetDateTime>,
    last_fetch_ip: Option<String>,
}

const COLS: &str = "id, name, team_mode, team_ids, direct, include_ids, exclude_ids, enabled, token_at, last_fetch_at, host(last_fetch_ip) last_fetch_ip";

/// 机器的 (id, kind, label, 团队名, 编号)
type MachineRef = (Uuid, String, Option<String>, Option<String>, Option<i32>);

/// 规则摘要里用的团队名、机器显示名
async fn ref_names(db: &sqlx::PgPool, rows: &[&Rule]) -> ApiResult<HashMap<Uuid, String>> {
    let ids: Vec<Uuid> = rows.iter().flat_map(|r| r.team_ids.iter().chain(&r.include_ids).chain(&r.exclude_ids)).copied().collect();
    let teams: Vec<(Uuid, String)> = sqlx::query_as("select id, name from team where id = any($1)").bind(&ids).fetch_all(db).await?;
    let ms: Vec<MachineRef> = sqlx::query_as(
        "select m.id, m.kind::text, m.label, t.name, m.number from machine m left join team t on t.id = m.team_id where m.id = any($1)",
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;
    let mut out: HashMap<Uuid, String> = teams.into_iter().collect();
    out.extend(ms.into_iter().map(|(id, k, l, t, n)| (id, names::display_name(&k, l.as_deref(), t.as_deref(), n))));
    Ok(out)
}

/// 规则的一句话：「全部团队（不含备用团队）；含线路机直出；另加 备用-03；排除 团队1-04」
fn rule_text(r: &Rule, names: &HashMap<Uuid, String>) -> String {
    let list = |ids: &[Uuid]| ids.iter().map(|i| names.get(i).cloned().unwrap_or_else(|| "（已删除）".into())).collect::<Vec<_>>().join("、");
    let mut parts = vec![match r.team_mode.as_str() {
        "only" => format!("只要 {}", list(&r.team_ids)),
        "except" => format!("全部团队（不含备用团队），除了 {}", list(&r.team_ids)),
        _ => "全部团队（不含备用团队）".into(),
    }];
    parts.push(if r.direct { "含线路机直出".into() } else { "不含线路机直出".into() });
    if !r.include_ids.is_empty() {
        parts.push(format!("另加 {}", list(&r.include_ids)));
    }
    if !r.exclude_ids.is_empty() {
        parts.push(format!("排除 {}", list(&r.exclude_ids)));
    }
    parts.join("；")
}

fn view(r: &Row, cs: &[Cand], names: &HashMap<Uuid, String>) -> Value {
    json!({
        "id": r.id, "name": r.name, "enabled": r.enabled, "rule": r.rule, "rule_text": rule_text(&r.rule, names),
        "items": cs.iter().filter(|c| r.rule.picks(c)).count(),
        "token_at": r.token_at.format(&time::format_description::well_known::Rfc3339).ok(),
        "last_fetch_at": r.last_fetch_at.and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()),
        "last_fetch_ip": r.last_fetch_ip,
    })
}

async fn load(st: &AppState, id: Uuid) -> ApiResult<Row> {
    sqlx::query_as(&format!("select {COLS} from client_sub where id = $1")).bind(id).fetch_optional(&st.db).await?.ok_or_else(|| ApiError::not_found("没有这个订阅"))
}

/// 一个订阅的视图（含条目数），按现在的入口算
async fn view_one(st: &AppState, r: &Row) -> ApiResult<Value> {
    let cs = candidates(st, false).await?;
    Ok(view(r, &cs, &ref_names(&st.db, &[&r.rule]).await?))
}

fn link(st: &AppState, token: &str) -> String { format!("{}/sub/{token}", st.cfg.public_url.trim_end_matches('/')) }

fn check_name(n: &str) -> ApiResult<String> {
    let n = n.trim();
    if n.is_empty() || n.chars().count() > 40 {
        return Err(ApiError::bad_request("名称要 1–40 个字符"));
    }
    Ok(n.to_string())
}

pub async fn list(a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    a.require_scope("secrets")?;
    let rows: Vec<Row> = sqlx::query_as(&format!("select {COLS} from client_sub order by created_at, id")).fetch_all(&st.db).await?;
    let cs = candidates(&st, false).await?;
    let names = ref_names(&st.db, &rows.iter().map(|r| &r.rule).collect::<Vec<_>>()).await?;
    Ok(ok(rows.iter().map(|r| view(r, &cs, &names)).collect::<Vec<_>>()))
}

/// 一个订阅与它现在的条目（名字、走哪台线路机哪个端口；不含凭据）
pub async fn get_one(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    a.require_scope("secrets")?;
    let r = load(&st, id).await?;
    let cs = candidates(&st, true).await?;
    let mut v = view(&r, &cs, &ref_names(&st.db, &[&r.rule]).await?);
    v["entries"] = pick(&cs, &r.rule)
        .into_iter()
        .map(|(c, name)| json!({"name": name, "entry_id": c.entry_id, "relay": c.relay, "port": c.port}))
        .collect();
    Ok(ok(v))
}

#[derive(Deserialize)]
pub struct CreateIn {
    name: String,
    #[serde(default)]
    rule: Rule,
}

/// 新建：返回的 `url` 只给这一次（库里只存哈希）
pub async fn create(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<CreateIn>) -> ApiResult<Json<Value>> {
    a.require_scope("secrets")?;
    let name = check_name(&b.name)?;
    let (id, token) = (new_id(), random_token(32));
    let mut tx = st.db.begin().await?;
    let rule = b.rule.check(&mut tx).await?;
    sqlx::query(
        "insert into client_sub (id, name, token_hash, team_mode, team_ids, direct, include_ids, exclude_ids) values ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(id)
    .bind(&name)
    .bind(sha256(token.as_bytes()))
    .bind(&rule.team_mode)
    .bind(&rule.team_ids)
    .bind(rule.direct)
    .bind(&rule.include_ids)
    .bind(&rule.exclude_ids)
    .execute(&mut *tx)
    .await?;
    let r: Row = sqlx::query_as(&format!("select {COLS} from client_sub where id = $1")).bind(id).fetch_one(&mut *tx).await?;
    let mut v = view_one(&st, &r).await?;
    audit(&mut tx, &a.name, "client_sub.create", "client_sub", Some(id), json!({"name": name, "rule": v["rule_text"]})).await?;
    tx.commit().await?;
    v["url"] = json!(link(&st, &token));
    Ok(ok(v))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchIn {
    name: Option<String>,
    rule: Option<Rule>,
    enabled: Option<bool>,
}

/// 改名称、规则（整份替换）、启用 / 停用（停用后拉取回 404，令牌不变）。只写实际改的列：只改名 / 启停不碰规则列，不会把并发的删团队摘掉的 team_ids 写回
pub async fn patch(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<PatchIn>) -> ApiResult<Json<Value>> {
    a.require_scope("secrets")?;
    let cur = load(&st, id).await?;
    let name = b.name.as_deref().map(check_name).transpose()?;
    let mut tx = st.db.begin().await?;
    // 先校验规则（团队行 for share）再写订阅行：和删团队（先锁团队、再改订阅）同序，两者并发不会死锁
    let rule = match b.rule {
        Some(r) => Some(r.check(&mut tx).await?),
        None => None,
    };
    sqlx::query("update client_sub set name = coalesce($2, name), enabled = coalesce($3, enabled), updated_at = now() where id = $1")
        .bind(id)
        .bind(&name)
        .bind(b.enabled)
        .execute(&mut *tx)
        .await?;
    if let Some(rule) = rule {
        sqlx::query("update client_sub set team_mode = $2, team_ids = $3, direct = $4, include_ids = $5, exclude_ids = $6 where id = $1")
            .bind(id)
            .bind(&rule.team_mode)
            .bind(&rule.team_ids)
            .bind(rule.direct)
            .bind(&rule.include_ids)
            .bind(&rule.exclude_ids)
            .execute(&mut *tx)
            .await?;
    }
    let r: Row = sqlx::query_as(&format!("select {COLS} from client_sub where id = $1")).bind(id).fetch_one(&mut *tx).await?;
    let v = view_one(&st, &r).await?;
    audit(&mut tx, &a.name, "client_sub.update", "client_sub", Some(id), json!({"name": r.name, "old_name": cur.name, "enabled": r.enabled, "rule": v["rule_text"]})).await?;
    tx.commit().await?;
    Ok(ok(v))
}

/// 轮换令牌：旧链接立即失效（404），返回的新 `url` 只给这一次；最近拉取清空（看得出客户端换没换上新链接）
pub async fn rotate(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    a.require_scope("secrets")?;
    let token = random_token(32);
    let mut tx = st.db.begin().await?;
    let name: Option<String> = sqlx::query_scalar(
        "update client_sub set token_hash = $2, token_at = now(), last_fetch_at = null, last_fetch_ip = null, updated_at = now() where id = $1 returning name",
    )
    .bind(id)
    .bind(sha256(token.as_bytes()))
    .fetch_optional(&mut *tx)
    .await?;
    let name = name.ok_or_else(|| ApiError::not_found("没有这个订阅"))?;
    audit(&mut tx, &a.name, "client_sub.rotate", "client_sub", Some(id), json!({"name": name})).await?;
    tx.commit().await?;
    let mut v = view_one(&st, &load(&st, id).await?).await?;
    v["url"] = json!(link(&st, &token));
    Ok(ok(v))
}

pub async fn delete(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    a.require_scope("secrets")?;
    let mut tx = st.db.begin().await?;
    let name: Option<String> = sqlx::query_scalar("delete from client_sub where id = $1 returning name").bind(id).fetch_optional(&mut *tx).await?;
    let name = name.ok_or_else(|| ApiError::not_found("没有这个订阅"))?;
    audit(&mut tx, &a.name, "client_sub.delete", "client_sub", Some(id), json!({"name": name})).await?;
    tx.commit().await?;
    Ok(ok(json!({"ok": true})))
}

