//! 旧系统对外入口的原生实现（下线后由 Caddy 转给 fleetd，API.md §12）：
//! - `/ops/api/*`：`X-Api-Key: tlm_…`（旧密钥继续有效：库里存 sha256），权限 read/write/full 与旧 Handler 相同，记调用日志；
//! - `/api/team-ops/*`：旧界面（Komari 插件页）的会话接口：fleetd 会话或 Komari 管理员会话，POST 要 `X-Ops-Request: 1` + 同源。
//! 读接口由原生旧库渲染（与旧系统逐字节同构），写接口落在原生旧库（compat::writes）或转给 fleetd 的原生流程。
use crate::api::{authenticate, AppState};
use crate::compat::{self, py::Err as CErr, pyjson, Ctx};
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, RawQuery, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use serde_json::{json, Value};
use std::net::SocketAddr;

/// 旧插件的白名单（`:x` 为一段通配）
const ALLOWED_GET: &[&str] = &[
    "overview", "activity", "history", "policies", "policies/preview", "audit", "renewals", "export", "health", "fleet", "node", "incidents",
    "behavior", "panel/overview", "schema", "api-keys", "api-keys/usage", "provision/defaults", "jobs", "jobs/:id", "schema.md", "komari/hosts",
    "komari/hosts/:uuid/metrics", "komari/pings", "komari/pings/:task_id/results", "komari/clients",
];
const ALLOWED_POST: &[&str] = &[
    "assets/edit", "policies/draft", "policies/apply", "policies/discard", "fleet/add", "fleet/activate", "fleet/installer", "node/profile",
    "incidents/ack", "behavior/ack", "panel/action", "api-keys/create", "api-keys/revoke", "provision/relay", "provision/node", "fleet/prepare",
    "fleet/bind", "fleet/commit",
];

fn matches(pattern: &str, path: &str) -> bool {
    let (a, b): (Vec<&str>, Vec<&str>) = (pattern.split('/').collect(), path.split('/').collect());
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.starts_with(':') && !y.is_empty() || x == y)
}

fn allowed(method: &Method, path: &str, api_key: bool) -> bool {
    let p = path.trim_start_matches('/');
    if *method == Method::GET {
        ALLOWED_GET.iter().any(|x| matches(x, p)) || (api_key && p == "public-status")
    } else if *method == Method::POST {
        ALLOWED_POST.iter().any(|x| matches(x, p))
    } else {
        false
    }
}

fn reply(status: u16, body: String, content_type: &'static str) -> Response {
    let mut r = (StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), body).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("referrer-policy", HeaderValue::from_static("same-origin"));
    h.insert("x-frame-options", HeaderValue::from_static("SAMEORIGIN"));
    r
}

fn json_reply(status: u16, v: &Value) -> Response {
    reply(status, pyjson::dumps(v), "application/json; charset=utf-8")
}

fn e(status: u16, msg: &str) -> Response {
    json_reply(status, &json!({"error": msg}))
}

/// 旧 Handler 的 POST 体校验
fn post_body(headers: &HeaderMap, body: &Bytes) -> Result<Value, String> {
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    if ct.split(';').next().unwrap_or("").trim() != "application/json" {
        return Err("仅接受 JSON 请求".into());
    }
    if body.is_empty() || body.len() > 262144 {
        return Err("请求长度无效".into());
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(v @ Value::Object(_)) => Ok(v),
        Ok(_) => Err("请求必须是对象".into()),
        Err(x) => Err(format!("Expecting value: {x}")),
    }
}

/// 写接口：原生旧库上一个事务
fn dispatch_write(cx: &Ctx, path: &str, b: &Value, actor: &str) -> compat::py::R<Value> {
    use compat::writes::*;
    match path {
        "/assets/edit" => edit_asset(cx, b, actor),
        "/node/profile" => node_profile(cx, b, actor),
        "/incidents/ack" => incident_ack(cx, b, actor),
        "/behavior/ack" => behavior_ack(cx, b, actor),
        "/api-keys/create" => api_key_create(cx, b, actor),
        "/api-keys/revoke" => api_key_revoke(cx, b, actor),
        "/policies/draft" => policy_draft(cx, b, actor),
        "/policies/discard" => policy_discard(cx, b, actor),
        p if p.starts_with("/fleet/") || p.starts_with("/provision/") => Err(CErr::Bad("接入已由新上架流程替代：请在管理界面（/）使用“上架”".into())),
        _ => Err(CErr::NotFound),
    }
}

fn to_response(r: compat::py::R<Value>) -> (u16, Value) {
    match r {
        Ok(v) => (200, json!({"data": v})),
        Err(CErr::Bad(m)) => (400, json!({"error": m})),
        Err(CErr::NotFound) => (404, json!({"error": "接口不存在"})),
        Err(CErr::Internal(x)) => {
            tracing::warn!(error = %x, "旧接口写入失败");
            (500, json!({"error": "操作未完成，请查看系统状态后重试"}))
        }
    }
}

/// 读或写一次（阻塞部分放到 spawn_blocking）；返回 (状态码, 响应 JSON 文本)
async fn run(app: &AppState, method: &Method, path: &str, query: Option<&str>, body: Option<Value>, actor: &str) -> (u16, String) {
    let ls = app.legacy_store.clone();
    if !ls.exists() {
        return (503, pyjson::dumps(&json!({"error": "运营服务暂不可用，请稍后重试"})));
    }
    let now = app.clock.now();
    if *method == Method::GET && path.starts_with("/komari/") {
        let (a2, p2, q2) = (app.clone(), path.to_string(), query.unwrap_or("").to_string());
        let r = tokio::task::spawn_blocking(move || {
            let params: std::collections::BTreeMap<String, String> = form_urlencoded::parse(q2.as_bytes()).into_owned().collect();
            crate::komarinative::render(&a2, &p2, &params, now)
        })
        .await
        .ok()
        .flatten();
        return match r {
            Some((st, v)) => (st, pyjson::dumps(&v)),
            None => (404, pyjson::dumps(&json!({"error": "接口不存在"}))),
        };
    }
    if *method == Method::GET {
        let pq = match query {
            Some(q) if !q.is_empty() => format!("{path}?{q}"),
            _ => path.to_string(),
        };
        return tokio::task::spawn_blocking(move || {
            let _span = crate::cpu::Span::new(format!("render {}", pq.split('?').next().unwrap_or(&pq)));
            ls.render_cached(&pq, now).map(|(s, b)| (s, b.to_string()))
        })
            .await
            .ok()
            .flatten()
            .unwrap_or((503, pyjson::dumps(&json!({"error": "运营服务暂不可用，请稍后重试"}))));
    }
    if path == "/panel/action" {
        return panel_action(app, body.unwrap_or(json!({})), actor).await;
    }
    let (p, b, a) = (path.to_string(), body.unwrap_or(json!({})), actor.to_string());
    let app2 = app.clone();
    let r = tokio::task::spawn_blocking(move || {
        ls.with_w(|c| {
            let cfg = crate::legacystore::LegacyStore::compat_cfg(c);
            let cx = Ctx { db: c, now, cfg: &cfg };
            let tx = match c.unchecked_transaction() {
                Ok(t) => t,
                Err(x) => return to_response(Err(CErr::Internal(x.to_string()))),
            };
            let r = if p == "/policies/apply" {
                // 策略发布要同时写 relayspec（fleetd.db）：先写 relayspec，再记版本
                let relays: Vec<(String, String)> = app2.cfg.edge.relay_hosts.iter().map(|(h, n)| (h.clone(), n.clone())).collect();
                let live = app2.managed_relays();
                (|| {
                    let fc = crate::legacystore::fleetd_conn(&app2.cfg.db_path()).map_err(CErr::Internal)?;
                    let ftx = fc.unchecked_transaction().map_err(|e| CErr::Internal(e.to_string()))?;
                    let r = compat::policy_apply::apply(&cx, &ftx, &b, &a, &relays, &live)?;
                    ftx.commit().map_err(|e| CErr::Internal(e.to_string()))?;
                    Ok(r)
                })()
            } else {
                dispatch_write(&cx, &p, &b, &a)
            };
            if r.is_ok() {
                if let Err(x) = tx.commit() {
                    return to_response(Err(CErr::Internal(x.to_string())));
                }
            }
            to_response(r)
        })
    })
    .await
    .ok()
    .flatten();
    if let Some((200, _)) = &r {
        // 写入已提交：旧接口渲染缓存按数据代作废，再通知（先作废再通知，客户端重取时拿到的是新数据）
        app.legacy_store.bump();
        if path.starts_with("/policies/") {
            app.legacy.cache.purge_all();
            app.hub.publish(POLICIES, Some(POLICIES_PATH.into()), app.clock.now());
        }
    }
    match r {
        Some((s, v)) => (s, pyjson::dumps(&v)),
        None => (503, pyjson::dumps(&json!({"error": "运营服务暂不可用，请稍后重试"}))),
    }
}

/// SSE 资源：访问规则（草稿、发布、回执定版 / 回滚）
pub const POLICIES: &str = "policies";
pub const POLICIES_PATH: &str = "/v2/api/legacy/policies";

/// 旧 panel.panel_action 的原生版：x-ui 面板已停用，操作翻译成 relayd 管理协议（入口启停/重置流量/改备注/轮换 UUID/重启 Xray/读日志）。
/// 返回旧形状 `{"data":{"ok","msg",…}}`；relayd 的执行是异步的（op 下发后返回），结果见 GET /v2/api/v1/ops/{op_id}。
async fn panel_action(app: &AppState, b: Value, actor: &str) -> (u16, String) {
    const ACTIONS: [&str; 8] = ["inbound_enable", "inbound_reset", "client_reset", "xray_restart", "inbound_remark", "client_rotate", "xray_logs", "probe_now"];
    const CONFIRM: [&str; 5] = ["inbound_enable", "xray_restart", "inbound_reset", "client_reset", "client_rotate"];
    let bad = |m: &str| (400u16, pyjson::dumps(&json!({"error": m})));
    let (Some(relay), Some(action)) = (b["relay"].as_str(), b["action"].as_str()) else { return (404, pyjson::dumps(&json!({"error": "接口不存在"}))) };
    let args = b.get("args").cloned().unwrap_or(json!({}));
    if !ACTIONS.contains(&action) {
        return bad("不支持的面板操作");
    }
    if CONFIRM.contains(&action) && args["confirm"] != true {
        return bad("需要确认");
    }
    let ident = crate::auth::Identity { username: actor.to_string(), session: None, via: "legacy", fleet_token: None };
    // 旧 x-ui 入站 id → 端口（原生旧库里的线路机快照）
    let port_of = |id: &Value| -> Option<i64> {
        let c = app.legacy_store.reader()?;
        let snap: Value = c.query_row("SELECT data FROM relays WHERE relay=?", [relay], |r| r.get::<_, String>(0)).ok().and_then(|s| serde_json::from_str(&s).ok())?;
        snap["inbounds"].as_array()?.iter().find(|i| crate::compat::store::py_eq(&i["id"], id)).and_then(|i| i["port"].as_i64())
    };
    let call_entry = |port: i64, body: Value| {
        let (a, i) = (app.clone(), ident.clone());
        async move {
            let r = crate::api::entry_action(State(a), axum::Extension(i), Path((relay.to_string(), port)), Bytes::from(body.to_string())).await;
            body_json(r).await
        }
    };
    let (ok, msg, extra): (bool, String, Value) = match action {
        "probe_now" => (false, "实际入口验证已由 relayd 每分钟自动执行，无需手动触发".into(), json!({})),
        "xray_restart" | "xray_logs" => {
            let body = if action == "xray_restart" { json!({"action": "restart_xray", "confirm": relay}) } else { json!({"action": "xray_logs", "which": "error", "lines": args["count"].as_u64().unwrap_or(100)}) };
            let r = crate::api::relay_action(State(app.clone()), axum::Extension(ident.clone()), Path(relay.to_string()), Bytes::from(body.to_string())).await;
            let (st, v) = body_json(r).await;
            if st >= 400 {
                (false, v["error"].as_str().unwrap_or("操作失败").to_string(), json!({}))
            } else if action == "xray_logs" {
                (v["data"]["status"] == "applied", v["data"]["error"].as_str().unwrap_or("").to_string(), json!({"lines": v["data"]["lines"]}))
            } else {
                (true, format!("已下发（op {}）", v["data"]["op_id"].as_str().unwrap_or("")), json!({}))
            }
        }
        _ => {
            let Some(port) = port_of(&args["id"]) else { return bad("入站不存在") };
            let confirm = format!("{relay}:{port}");
            let body = match action {
                "inbound_enable" => json!({"action": if args["enable"] == true { "enable" } else { "disable" }, "confirm": confirm}),
                "inbound_reset" | "client_reset" => json!({"action": "reset_traffic", "confirm": confirm}),
                "inbound_remark" => json!({"action": "rename", "remark": args["remark"]}),
                _ => json!({"action": "rotate_uuid", "confirm": confirm}),
            };
            let (st, v) = call_entry(port, body).await;
            if st >= 400 {
                (false, v["error"].as_str().unwrap_or("操作失败").to_string(), json!({}))
            } else if action == "client_rotate" {
                // 新 UUID：刚生成的期望状态里该入站的第一个客户端
                let host = crate::api::relay_host_pub(app, relay).map(|x| x.0).unwrap_or_default();
                let uuid = app.store.call(move |c| {
                    let s = crate::relayspec::current_spec(c, &host)?;
                    let i = crate::relayspec::find_inbound(&s.managed, port)?;
                    s.managed["inbounds"][i]["settings"]["clients"][0]["id"].as_str().map(String::from)
                }).await;
                (true, format!("已下发（op {}）", v["data"]["op_id"].as_str().unwrap_or("")), json!({"uuid": uuid}))
            } else {
                let msg = match v["data"]["op_id"].as_str() {
                    Some(op) => format!("已下发（op {op}）"),
                    None => "已保存".to_string(),
                };
                (true, msg, json!({}))
            }
        }
    };
    let mut resp = json!({"ok": ok, "msg": msg});
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        resp[k] = v;
    }
    // 旧审计（原生旧库）：panel.<action> relay:<名>
    let detail = if action == "xray_logs" || action == "probe_now" { json!({}) } else {
        let mut d = args.as_object().cloned().unwrap_or_default();
        d.insert("result".into(), json!(if action == "client_rotate" { "rotated".to_string() } else { msg.clone() }));
        Value::Object(d)
    };
    let (ls, now, a, act, target) = (app.legacy_store.clone(), app.clock.now(), actor.to_string(), format!("panel.{action}"), format!("relay:{relay}"));
    let _ = tokio::task::spawn_blocking(move || {
        ls.with_w(|c| {
            let cfg = crate::legacystore::LegacyStore::compat_cfg(c);
            let _ = compat::writes::audit(&Ctx { db: c, now, cfg: &cfg }, &a, &act, &target, &detail);
        })
    })
    .await;
    (200, pyjson::dumps(&json!({"data": resp})))
}

async fn body_json(r: Response) -> (u16, Value) {
    let st = r.status().as_u16();
    let b = axum::body::to_bytes(r.into_body(), 4 * 1024 * 1024).await.unwrap_or_default();
    (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

fn finish(path: &str, status: u16, body: String) -> Response {
    if path == "/schema.md" && status == 200 {
        if let Ok(v) = serde_json::from_str::<Value>(&body) {
            if let Some(md) = v["data"]["markdown"].as_str() {
                return reply(200, md.to_string(), "text/markdown; charset=utf-8");
            }
        }
    }
    reply(status, body, "application/json; charset=utf-8")
}

fn client_ip(headers: &HeaderMap, peer: SocketAddr) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| peer.ip().to_string())
}

async fn ops_api(State(app): State<AppState>, ConnectInfo(peer): ConnectInfo<SocketAddr>, method: Method, Path(rest): Path<String>,
    RawQuery(q): RawQuery, headers: HeaderMap, body: Bytes) -> Response {
    let path = format!("/{rest}");
    let Some(key) = headers.get("x-api-key").and_then(|v| v.to_str().ok()).map(String::from) else { return e(401, "missing api key") };
    if !allowed(&method, &path, true) {
        return e(404, "接口不存在");
    }
    let ls = app.legacy_store.clone();
    let now = app.clock.now();
    let k2 = key.clone();
    let auth = tokio::task::spawn_blocking(move || {
        let c = ls.reader()?;
        let cfg = crate::legacystore::LegacyStore::compat_cfg(&c);
        compat::writes::api_key_auth(&Ctx { db: &c, now, cfg: &cfg }, &k2).ok().flatten()
    })
    .await
    .ok()
    .flatten();
    let Some(row) = auth else { return e(401, "invalid api key") };
    let (kid, kname, scope) = (row["id"].as_str().unwrap_or("").to_string(), row["name"].as_str().unwrap_or("").to_string(), row["scope"].as_str().unwrap_or("").to_string());
    let (status, body) = if path.starts_with("/api-keys") && scope != "full" {
        (403, pyjson::dumps(&json!({"error": "forbidden for api key"})))
    } else if method == Method::POST && scope == "read" {
        (403, pyjson::dumps(&json!({"error": "write scope required"})))
    } else {
        let b = if method == Method::POST {
            match post_body(&headers, &body) {
                Ok(v) => Some(v),
                Err(m) => {
                    let (s, x) = (400u16, pyjson::dumps(&json!({"error": m})));
                    record(&app, &kid, &method, &path, s, &client_ip(&headers, peer)).await;
                    return finish(&path, s, x);
                }
            }
        } else {
            None
        };
        run(&app, &method, &path, q.as_deref(), b, &format!("apikey:{kname}")).await
    };
    record(&app, &kid, &method, &path, status, &client_ip(&headers, peer)).await;
    finish(&path, status, body)
}

async fn record(app: &AppState, kid: &str, method: &Method, path: &str, status: u16, ip: &str) {
    let usage_path = if path.starts_with("/komari/hosts/") { "/komari/hosts/<uuid>/metrics".to_string() } else { path.to_string() };
    let (ls, now, kid, m, ip) = (app.legacy_store.clone(), app.clock.now(), kid.to_string(), method.to_string(), ip.to_string());
    let _ = tokio::task::spawn_blocking(move || {
        ls.with_w(|c| {
            let cfg = crate::legacystore::LegacyStore::compat_cfg(c);
            let _ = compat::writes::api_key_usage(&Ctx { db: c, now, cfg: &cfg }, &kid, &m, &usage_path, status, &ip);
        })
    })
    .await;
}

async fn team_ops_api(State(app): State<AppState>, ConnectInfo(peer): ConnectInfo<SocketAddr>, method: Method, Path(rest): Path<String>,
    RawQuery(q): RawQuery, headers: HeaderMap, body: Bytes) -> Response {
    let path = format!("/{rest}");
    let public_status = path == "/public-status" && method == Method::GET;
    if !public_status && !allowed(&method, &path, false) {
        return e(404, "接口不存在");
    }
    let ident = match authenticate(&app, &headers, peer).await {
        Ok(i) => i,
        Err(_) => return e(401, if public_status { "请先登录" } else { "请先登录管理员账户" }),
    };
    let mut b = None;
    if method == Method::POST {
        let site_ok = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()).is_none_or(|s| s == "same-origin");
        if headers.get("x-ops-request").and_then(|v| v.to_str().ok()) != Some("1") || !site_ok {
            return e(403, "请求来源校验失败");
        }
        match post_body(&headers, &body) {
            Ok(v) => b = Some(v),
            Err(m) => return e(400, &m),
        }
    }
    let (status, out) = run(&app, &method, &path, q.as_deref(), b, &ident.username).await;
    if method == Method::POST && status == 200 {
        app.legacy.cache.purge_all();
        app.legacy_store.bump();
        app.hub.publish("overview", None, app.clock.now());
        app.force_poll(&["overview", "health", "panel"]);
    }
    finish(&path, status, out)
}

/// 新界面的原生写接口：POST /v2/api/v1/legacy-write/<旧 POST 路径>，请求体与返回形状同旧接口；
/// 只在 legacy_source = "native"（旧系统已下线）时可用，否则旧系统仍是权威，写进原生旧库会分叉。
pub async fn v2_write(app: &AppState, path: &str, body: &Bytes, actor: &str) -> Response {
    if app.cfg.legacy_source != "native" {
        return e(409, "旧系统仍在运行：原生写接口在下线后开放（system.legacy_source = native）");
    }
    let b = match serde_json::from_slice::<Value>(body) {
        Ok(v @ Value::Object(_)) => v,
        _ => return e(400, "请求必须是对象"),
    };
    if !allowed(&Method::POST, path, false) {
        return e(404, "接口不存在");
    }
    let (status, out) = run(app, &Method::POST, path, None, Some(b), actor).await;
    if status == 200 {
        app.legacy.cache.purge_all();
        app.legacy_store.bump();
        app.hub.publish("overview", None, app.clock.now());
        app.force_poll(&["overview", "health", "panel"]);
    }
    finish(path, status, out)
}

pub fn routes(app: AppState) -> Router {
    Router::new()
        .route("/ops/api/{*rest}", any(ops_api))
        .route("/api/team-ops/{*rest}", any(team_ops_api))
        .with_state(app)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn allowlist() {
        assert!(allowed(&Method::GET, "/jobs/abc", false));
        assert!(allowed(&Method::GET, "/komari/hosts/u1/metrics", false));
        assert!(!allowed(&Method::GET, "/komari/hosts/u1", false));
        assert!(allowed(&Method::GET, "/public-status", true));
        assert!(!allowed(&Method::GET, "/public-status", false));
        assert!(!allowed(&Method::GET, "/sync/pending", true));
        assert!(allowed(&Method::POST, "/assets/edit", true));
        assert!(!allowed(&Method::POST, "/sync/hosts", true));
        assert!(!allowed(&Method::DELETE, "/overview", true));
    }
}
