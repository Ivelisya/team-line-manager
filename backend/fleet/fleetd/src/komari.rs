//! 直接读 Komari（127.0.0.1:25774），替代旧后端的 /komari/*（那条路要等一个打开的管理员页面代为执行）。
//!
//! - 登录态：GET /api/me（Cookie session_token），`logged_in == true` 即管理员（Komari 只有一个账户）。
//! - 数据：POST /api/rpc2（JSON-RPC）。只允许白名单里的只读方法；鉴权用 Komari 系统 API 密钥（配置了的话），
//!   否则转发当前请求者的会话 Cookie（请求者已通过管理员校验）。
//! - 返回形状与旧后端 /komari/* 完全一致：{"data": <Komari result>}，komari/clients 去掉 token。
use crate::cache::{Cache, Entry};
use crate::clock::Clock;
use crate::legacy::{short_err, UpstreamStats};
use bytes::Bytes;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 只读方法白名单。任何写方法（admin:editClient、admin:addClient、dbExec …）都不在这里。
pub const READ_METHODS: &[&str] = &[
    "common:getNodesLatestStatus",
    "public:getRecordsByUUID",
    "public:getPingRecords",
    "admin:getAllPingTasks",
    "admin:listClients",
    "common:getMe",
    "common:getVersion",
];

pub struct Komari {
    client: reqwest::Client,
    base: String,
    api_key: Option<String>,
    pub cookie_name: String,
    pub cache: Cache,
    pub stats: Mutex<UpstreamStats>,
    clock: Arc<dyn Clock>,
}

pub enum MeResult {
    Admin(String),
    NotLoggedIn,
}

impl Komari {
    pub fn new(base: &str, api_key: Option<String>, cookie_name: &str, timeout_s: u64, clock: Arc<dyn Clock>) -> Komari {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_s))
            .pool_max_idle_per_host(2)
            .no_proxy()
            .build()
            .expect("http client");
        Komari {
            client,
            base: base.trim_end_matches('/').to_string(),
            api_key,
            cookie_name: cookie_name.to_string(),
            cache: Cache::new(256),
            stats: Mutex::new(UpstreamStats::default()),
            clock,
        }
    }

    pub fn has_api_key(&self) -> bool {
        self.api_key.is_some()
    }

    /// 管理员会话校验。
    pub async fn me(&self, token: Option<&str>) -> Result<MeResult, String> {
        let t0 = Instant::now();
        let mut rb = self.client.get(format!("{}/api/me", self.base)).timeout(Duration::from_secs(5));
        if let Some(t) = token {
            rb = rb.header("Cookie", format!("{}={}", self.cookie_name, t));
        }
        let res = async {
            let r = rb.send().await.map_err(|e| format!("连接 Komari 失败：{}", short_err(&e)))?;
            let status = r.status().as_u16();
            let body: Value = r.json().await.unwrap_or(Value::Null);
            Ok::<_, String>((status, body))
        }
        .await;
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        let now = self.clock.now();
        match res {
            Ok((status, body)) => {
                self.stats.lock().unwrap().record(status < 500, ms, now, if status < 500 { None } else { Some(format!("HTTP {status}")) }, 0);
                if status >= 500 {
                    return Err(format!("Komari 返回 HTTP {status}"));
                }
                if status == 200 && body.get("logged_in").and_then(|x| x.as_bool()) == Some(true) {
                    let user = body.get("username").and_then(|x| x.as_str()).unwrap_or("admin").to_string();
                    Ok(MeResult::Admin(user))
                } else {
                    Ok(MeResult::NotLoggedIn)
                }
            }
            Err(e) => {
                self.stats.lock().unwrap().record(false, ms, now, Some(e.clone()), 0);
                Err(e)
            }
        }
    }

    pub async fn rpc(&self, method: &str, params: Value, session: Option<&str>) -> Result<Value, String> {
        let raw = self.rpc_raw(method, params, session).await?;
        serde_json::from_slice(&raw).map_err(|e| format!("Komari 返回无效 JSON：{e}"))
    }

    /// 返回 result 的原始 JSON 字节（不重新编码，数值与字段顺序保持 Komari 的原样）。
    pub async fn rpc_raw(&self, method: &str, params: Value, session: Option<&str>) -> Result<Bytes, String> {
        if !READ_METHODS.contains(&method) {
            return Err(format!("方法 {method} 不在只读白名单内"));
        }
        let mut rb = self
            .client
            .post(format!("{}/api/rpc2", self.base))
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}));
        if let Some(k) = &self.api_key {
            rb = rb.header("Authorization", format!("Bearer {k}"));
        } else if let Some(t) = session {
            rb = rb.header("Cookie", format!("{}={}", self.cookie_name, t));
        } else {
            return Err("读取 Komari 需要管理员会话或系统 API 密钥".into());
        }
        #[derive(serde::Deserialize)]
        struct Resp<'a> {
            #[serde(borrow)]
            result: Option<&'a serde_json::value::RawValue>,
            error: Option<Value>,
        }
        let t0 = Instant::now();
        let res = async {
            let r = rb.send().await.map_err(|e| format!("连接 Komari 失败：{}", short_err(&e)))?;
            let body = r.bytes().await.map_err(|e| format!("读取 Komari 响应失败：{}", short_err(&e)))?;
            let parsed: Resp = serde_json::from_slice(&body).map_err(|e| format!("Komari 返回无效 JSON：{e}"))?;
            if let Some(err) = parsed.error.filter(|e| !e.is_null()) {
                return Err(format!("Komari RPC 错误：{}", err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown")));
            }
            Ok(Bytes::copy_from_slice(parsed.result.map(|r| r.get()).unwrap_or("null").as_bytes()))
        }
        .await;
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        self.stats.lock().unwrap().record(res.is_ok(), ms, self.clock.now(), res.as_ref().err().cloned(), res.as_ref().map(|b| b.len()).unwrap_or(0));
        res
    }

    /// 旧后端 /komari/<...> 的等价实现。key 是规范化后的缓存键（/komari/... ?query）。
    /// 返回 (HTTP 状态, 响应体)——状态与错误文案同旧后端 Handler 的映射。
    pub async fn legacy_shape(&self, key: &str, session: Option<&str>, max_age: f64) -> (Result<Arc<Entry>, String>, bool) {
        let now = self.clock.now();
        let key_owned = key.to_string();
        self.cache
            .get_or_fetch(key, now, max_age, || async move {
                let t0 = Instant::now();
                let (path, query) = crate::legacy::split_key(&key_owned);
                let params: std::collections::BTreeMap<String, String> =
                    form_urlencoded::parse(query.as_bytes()).into_owned().collect();
                let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
                let call = match segs.as_slice() {
                    ["komari", "hosts"] => Some(("common:getNodesLatestStatus", json!({}))),
                    ["komari", "hosts", uuid, "metrics"] => {
                        let hours = match params.get("range").map(String::as_str) {
                            Some("1h") => Some("1"),
                            Some("4h") => Some("4"),
                            Some("1d") => Some("24"),
                            _ => None,
                        };
                        hours.map(|h| ("public:getRecordsByUUID", json!({"uuid": uuid, "load_type": "all", "hours": h})))
                    }
                    ["komari", "pings"] => Some(("admin:getAllPingTasks", json!({}))),
                    ["komari", "pings", task, "results"] => match task.parse::<i64>() {
                        Ok(id) => {
                            // Komari 1.5.0 把 task_id 与 hours 当 JSON 字符串绑定
                            let mut p = serde_json::Map::new();
                            p.insert("task_id".into(), Value::String(id.to_string()));
                            for k in ["hours", "uuid"] {
                                if let Some(v) = params.get(k) {
                                    p.insert(k.into(), Value::String(v.clone()));
                                }
                            }
                            Some(("public:getPingRecords", Value::Object(p)))
                        }
                        Err(_) => {
                            let body = json!({"error": format!("invalid literal for int() with base 10: '{task}'")});
                            return Ok(Entry::new(400, Bytes::from(serde_json::to_vec(&body).unwrap()), now, 0.0));
                        }
                    },
                    ["komari", "clients"] => Some(("admin:listClients", json!({}))),
                    _ => None,
                };
                let Some((method, params)) = call else {
                    // 旧后端：未知路径或 range 取值无效 → KeyError → 404
                    return Ok(Entry::new(404, Bytes::from_static("{\"error\":\"接口不存在\"}".as_bytes()), now, 0.0));
                };
                match self.rpc_raw(method, params, session).await {
                    Ok(raw) => {
                        let body = if method == "admin:listClients" {
                            // 与旧后端一致：去掉 Agent token
                            let mut result: Value = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
                            if let Value::Array(rows) = &mut result {
                                for r in rows.iter_mut() {
                                    if let Value::Object(m) = r {
                                        m.remove("token");
                                    }
                                }
                            }
                            serde_json::to_vec(&json!({"data": result})).unwrap()
                        } else {
                            let mut b = Vec::with_capacity(raw.len() + 9);
                            b.extend_from_slice(b"{\"data\":");
                            b.extend_from_slice(&raw);
                            b.push(b'}');
                            b
                        };
                        Ok(Entry::new(200, Bytes::from(body), now, t0.elapsed().as_secs_f64() * 1000.0))
                    }
                    Err(e) => {
                        tracing::warn!(key = %key_owned, error = %e, "komari read failed");
                        Ok(Entry::new(500, Bytes::from_static("{\"error\":\"操作未完成，请查看系统状态后重试\"}".as_bytes()), now, 0.0))
                    }
                }
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitelist_is_read_only() {
        for m in READ_METHODS {
            let name = m.split(':').nth(1).unwrap();
            assert!(name.starts_with("get") || name.starts_with("list"), "{m}");
        }
    }

    #[tokio::test]
    async fn refuses_write_methods() {
        let k = Komari::new("http://127.0.0.1:1", Some("k".into()), "session_token", 1, Arc::new(crate::clock::SystemClock));
        for m in ["admin:editClient", "admin:addClient", "admin:dbExec", "admin:removeClient"] {
            let e = k.rpc(m, json!({}), None).await.unwrap_err();
            assert!(e.contains("白名单"), "{e}");
        }
    }
}
