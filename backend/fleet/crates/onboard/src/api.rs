//! fleetd 的 HTTP 处理函数（与框架无关）：fleetd 在 axum 里把 `/v2/api/v1/onboard/*` 转给 `OnboardApi::handle`，
//! 鉴权/CSRF/审计仍由 fleetd 的 guard 中间件负责（写操作要会话管理员 + X-Fleet-Request）。接法见 WIRING.md §2。
//!
//! | 方法 | 路径（前缀 /v2/api/v1/onboard） | 作用 |
//! |---|---|---|
//! | GET  | /batches | 批次列表 |
//! | POST | /batches | body {csv, title?, options?}：plan + 建批次（同一份 CSV 幂等），返回计划与问题 |
//! | GET  | /batches/{id} | 批次视图（与原型上架流水线页 demo_batch 同形） |
//! | GET  | /batches/{id}/plan | 重新计算计划（只读） |
//! | POST | /batches/{id}/start | body {plan_hash}：哈希不符 409；后台开始执行，202 |
//! | POST | /batches/{id}/resume | 接管过期租约继续跑，202 |
//! | POST | /batches/{id}/cancel | body {row?}：补偿并取消（整批或一行） |
//! | POST | /batches/{id}/rows/{name}/actions | body {action: provide_input|continue|retry, password?, force?} |
//! | GET  | /batches/{id}/events?after=N | 事件（SSE 由 fleetd 用它做 Last-Event-ID 回放） |
//! | GET  | /batches/{id}/relay-batches | 线路机批次与零重启证明（pid/启动时刻/配置哈希前后） |
//! | GET  | /assets | 本流水线登记的资产（不含机密） |

use crate::agent::AgentTokens;
use crate::config::Config;
use crate::db::Db;
use crate::engine::Engine;
use crate::planner;
use crate::view;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Clone)]
pub struct OnboardApi {
    pub cfg: Arc<Config>,
    pub db: Db,
    /// P11 的令牌后端：fleetd 注入自己的 fleetd-ingest（见 `with_agent_tokens`）；None = 按 [agent].token_cmd（独立运行）
    pub tokens: Option<Arc<dyn AgentTokens>>,
}

/// 百分号解码一个路径段（UTF-8；非法序列原样保留）。
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = ((b[i + 1] as char).to_digit(16), (b[i + 2] as char).to_digit(16)) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

fn ok(v: Value) -> (u16, Value) {
    (200, json!({"data": v}))
}
fn bad(code: u16, m: impl Into<String>) -> (u16, Value) {
    (code, json!({"error": m.into()}))
}

impl OnboardApi {
    pub fn new(cfg: Arc<Config>) -> Result<OnboardApi, String> {
        let db = Db::open(&cfg.db)?;
        Ok(OnboardApi { cfg, db, tokens: None })
    }

    /// fleetd 启动时调用：P11 用 fleetd 进程内的 fleetd-ingest 签发/撤销令牌、核对第一批（本 crate 不打开 ingest.db）。
    pub fn with_agent_tokens(mut self, t: Arc<dyn AgentTokens>) -> OnboardApi {
        self.tokens = Some(t);
        self
    }

    fn engine(&self) -> Result<Engine, String> {
        Ok(Engine::new(self.cfg.clone(), self.db.clone())?.with_agent_tokens(self.tokens.clone()))
    }

    /// path 可以带或不带 `/v2/api/v1/onboard` 前缀。
    pub async fn handle(&self, method: &str, path: &str, query: &str, body: &[u8], actor: &str) -> (u16, Value) {
        let p = path.trim_start_matches("/v2/api/v1/onboard").trim_matches('/');
        // 路径段按百分号解码：浏览器/curl 会把「团队9-03」编码成 %E5%9B%A2…，不解码就找不到这一行
        let owned: Vec<String> = p.split('/').filter(|s| !s.is_empty()).map(percent_decode).collect();
        let seg: Vec<&str> = owned.iter().map(String::as_str).collect();
        let body: Value = if body.is_empty() { json!({}) } else {
            match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(e) => return bad(400, format!("body 不是 JSON：{e}")),
            }
        };
        let r = match (method, seg.as_slice()) {
            ("GET", ["batches"]) => self.list(),
            ("POST", ["batches"]) => self.create(&body, actor).await,
            ("GET", ["batches", id]) => view::batch_view(&self.db, &self.cfg, id).map(|v| (200, json!({"data": v}))),
            ("GET", ["batches", id, "plan"]) => self.replan(id).await,
            ("POST", ["batches", id, "start"]) => self.start(id, &body).await,
            ("POST", ["batches", id, "resume"]) => self.spawn_run(id).await,
            ("POST", ["batches", id, "cancel"]) => self.cancel(id, &body).await,
            ("POST", ["batches", id, "rows", name, "actions"]) => self.action(id, name, &body),
            ("GET", ["batches", id, "events"]) => {
                let after = query.split('&').find_map(|kv| kv.strip_prefix("after=")).and_then(|x| x.parse().ok()).unwrap_or(0);
                self.db.events(id, after, 500).map(|v| ok(json!(v)))
            }
            ("GET", ["batches", id, "relay-batches"]) => self.db.relay_batches(id).map(|v| ok(json!(v))),
            ("GET", ["assets"]) => self.db.assets().map(|v| ok(json!(v))),
            _ => return bad(404, "接口不存在"),
        };
        r.unwrap_or_else(|e| bad(500, e))
    }

    fn list(&self) -> Result<(u16, Value), String> {
        let mut out = vec![];
        for b in self.db.batches()? {
            let rows = self.db.rows(&b.id)?;
            let c = |s: &str| rows.iter().filter(|r| r.state == s).count();
            out.push(json!({"id": b.id, "title": b.title, "status": b.status, "operator": b.operator, "created_at": b.created_at / 1000,
                "started_at": b.started_at.map(|x| x / 1000), "finished_at": b.finished_at.map(|x| x / 1000), "total": rows.len(),
                "active": c("active"), "attention": c("waiting_input") + c("blocked") + c("failed"), "plan_hash": b.plan_hash, "error": b.error}));
        }
        Ok(ok(json!(out)))
    }

    async fn create(&self, body: &Value, actor: &str) -> Result<(u16, Value), String> {
        let Some(csv) = body["csv"].as_str() else { return Ok(bad(400, "缺少 csv")) };
        // 网页上传时密码不走 CSV：password 列写成 password:<已上传的 secret 文件名>，文件由 rows/{name}/actions 上传
        let dir = self.cfg.work_dir.join("secrets");
        let p = planner::plan(&self.cfg, &self.db, csv, &dir, &body["options"]).await?;
        if p.errors() > 0 {
            return Ok((422, json!({"error": "计划有错误", "data": p.to_json()})));
        }
        let (id, created) = planner::ensure_batch(&self.db, &p, csv, None, body["title"].as_str(), actor)?;
        let mut v = p.to_json();
        v["created"] = json!(created);
        v["batch_id"] = json!(id);
        Ok((if created { 201 } else { 200 }, json!({"data": v})))
    }

    async fn replan(&self, id: &str) -> Result<(u16, Value), String> {
        let b = self.db.batch(id)?.ok_or("批次不存在")?;
        let dir = b.csv_path.as_deref().and_then(|p| std::path::Path::new(p).parent().map(|x| x.to_path_buf())).unwrap_or(self.cfg.work_dir.join("secrets"));
        let p = planner::plan(&self.cfg, &self.db, &b.csv_text, &dir, &b.options).await?;
        Ok(ok(p.to_json()))
    }

    async fn start(&self, id: &str, body: &Value) -> Result<(u16, Value), String> {
        let b = self.db.batch(id)?.ok_or("批次不存在")?;
        if body["plan_hash"].as_str() != Some(b.plan_hash.as_str()) {
            return Ok(bad(409, format!("计划哈希不符（当前 {}）：请重新确认计划", b.plan_hash)));
        }
        self.spawn_run(id).await
    }

    async fn spawn_run(&self, id: &str) -> Result<(u16, Value), String> {
        let eng = self.engine()?;
        // 先同步抢租约，抢不到直接告诉调用方（别人正在跑）
        let b = self.db.batch(id)?.ok_or("批次不存在")?;
        if let (Some(o), Some(u)) = (&b.lease_owner, b.lease_until) {
            if u > crate::db::now_ms() {
                return Ok(bad(409, format!("批次正由 {o} 执行")));
            }
        }
        let id2 = id.to_string();
        tokio::spawn(async move {
            if let Err(e) = eng.run_batch(&id2).await {
                crate::engine::log(&format!("批次 {id2} 执行出错：{e}"));
            }
        });
        Ok((202, json!({"data": {"batch_id": id, "status": "running"}})))
    }

    async fn cancel(&self, id: &str, body: &Value) -> Result<(u16, Value), String> {
        let b = self.db.batch(id)?.ok_or("批次不存在")?;
        // 正在执行（别的执行者持有未过期租约）：记下取消请求，由执行中的引擎停止派发新步骤、等手上的做完再补偿
        if let (Some(o), Some(u)) = (&b.lease_owner, b.lease_until) {
            if u > crate::db::now_ms() {
                let what = body["row"].as_str().unwrap_or("*").to_string();
                self.db.add_cancel_request(id, &what)?;
                let _ = self.db.event(id, None, "batch.updated", json!({"status": if what == "*" { "cancelling" } else { "running" }, "cancel_requested": what}));
                return Ok((202, json!({"data": {"batch_id": id, "status": "cancelling", "row": body["row"], "owner": o,
                    "message": "已请求取消：执行中的步骤做完后停止并补偿（进度见事件流）"}})));
            }
        }
        let eng = self.engine()?;
        match eng.cancel(id, body["row"].as_str()).await {
            Ok(v) => Ok(ok(v)),
            Err(e) if e.starts_with("批次正由") => Ok(bad(409, e)),
            Err(e) => Err(e),
        }
    }

    fn action(&self, id: &str, name: &str, body: &Value) -> Result<(u16, Value), String> {
        let eng = self.engine()?;
        let mut pwfile = None;
        if let Some(pw) = body["password"].as_str() {
            // 只写不读：落 0600 文件，库里只记路径
            let dir = self.cfg.work_dir.join("secrets");
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let f = dir.join(format!("{id}-{}.pw", crate::steps::uuid_v4()));
            std::fs::write(&f, pw).map_err(|e| e.to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
            }
            pwfile = Some(f.display().to_string());
        }
        match body["action"].as_str() {
            Some("provide_input" | "continue" | "retry") => {
                // 业务性拒绝（“现在是 X，不需要输入”等）是状态冲突，不是服务器错误
                match eng.provide(id, name, pwfile, body["force"].as_bool().unwrap_or(false)) {
                    Ok(m) => Ok(ok(json!({"message": m, "next": "POST /batches/{id}/resume"}))),
                    Err(e) => Ok(bad(409, e)),
                }
            }
            _ => Ok(bad(400, "action 只能是 provide_input / continue / retry")),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn percent_decode_segments() {
        assert_eq!(super::percent_decode("%E5%9B%A2%E9%98%9F9-03"), "团队9-03");
        assert_eq!(super::percent_decode("%e5%9b%a2%e9%98%9f9-03"), "团队9-03");
        assert_eq!(super::percent_decode("团队9-03"), "团队9-03");
        assert_eq!(super::percent_decode("b-1234"), "b-1234");
        assert_eq!(super::percent_decode("50%"), "50%");
        assert_eq!(super::percent_decode("%zz%4"), "%zz%4");
        assert_eq!(super::percent_decode("%FF"), "%FF");
    }
}
