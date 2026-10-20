//! 旧系统（team-ops，Python）读接口的原生实现。
//!
//! 数据是旧库格式（operations.db 的表结构，下线后由 fleetd 自己的喂数维护），逻辑逐行移植自
//! release 20260922T081813Z-providers；输出用 [`pyjson`] 序列化，和旧 Handler 的
//! `json.dumps({"data": …}, ensure_ascii=False, separators=(",", ":"))` 逐字节一致。
//! 核对方法：`fleetd legacy verify --golden DIR`（DIR 由 staging/golden.py 生成：同一份库、同一时刻的旧系统输出）。
pub mod behavior;
pub mod behavior_write;
pub mod domains;
pub mod monitor;
pub mod monitor_eval;
pub mod policy;
pub mod policy_apply;
pub mod py;
pub mod rollup;
pub mod pyjson;
pub mod store;
pub mod window;
pub mod writes;

use py::*;
use rusqlite::Connection;
use serde_json::{json, Value};

/// 旧系统进程配置里、不在库里的那几项
#[derive(Clone, Debug, Default)]
pub struct CompatCfg {
    /// backend.json 的 relays（名字，按配置顺序）；fleet 表里另有启用的线路机会追加在后面
    pub config_relays: Vec<String>,
    /// provision.key.pub 的内容
    pub public_key: String,
    /// packages/packages.json 里文件存在的条目（去掉 file 字段）
    pub packages: Value,
    /// 行为统计缓存（behavior_cache）的最大年龄：超过就现算（原生旧库按需刷新缓存）；None = 总用缓存（与旧系统同一时刻的核对）
    pub cache_max_age: Option<f64>,
    /// 按账本版本缓存的派生结果（totals / history）：账本不变、窗口不变时结果不变
    pub memo: Option<std::sync::Arc<Memo>>,
    /// 生产：去掉内部核对字段（supplier_verification 等），见 catalog::INTERNAL_KEYS
    pub redact_internal: bool,
    /// 生产：/behavior 的 events 把“新域名”和其他事件各取最近 200 条（旧系统合在一起取 200 条，
    /// 新域名一多就把连接突增 / 新来源挤出列表，而界面只展示非新域名事件）
    pub events_split: bool,
    /// 生产：随机命名的子域名按主域名合并成一条新域名事件（compat::domains；需要 behavior_events.evidence 列）
    pub domain_collapse: bool,
    /// 生产：“实际入口到出口”的新鲜期（秒）。relayd 自适应巡检：健康入口 120 s 一轮，取 2 轮 = 240 s；None = 旧值 180 s
    pub path_fresh_s: Option<f64>,
    /// false（生产）：节点健康只看连通性——出口验证取到 IP 就算通过，实际出口 IP 只记录不比对（IP 变化由喂数自动更新资产）；
    /// true = 旧系统口径（出口 IP 须等于资产 IP）
    pub egress_ip_check: bool,
}

/// 7 / 14 天视图多久没人看就释放小时汇总镜像（秒）
pub const ROLLUP_IDLE_S: f64 = 600.0;

/// 账本（minutes / hours）派生结果的缓存；ledger_gen 在账本写入后加一
#[derive(Default)]
pub struct Memo {
    /// 最近 24 h 访问记录的增量聚合（默认 /behavior、默认 /activity 用）
    pub window: std::sync::Mutex<Option<window::Window>>,
    /// 7 天 / 14 天的小时汇总镜像（compat::rollup）；ROLLUP_IDLE_S 没人用就释放（约 30 MB），下次用时从表重新载入
    pub rollup: std::sync::Mutex<Option<rollup::Rollup>>,
    /// 最近一次用小时汇总的时刻
    pub rollup_used_at: std::sync::Mutex<f64>,
    /// 最近一次算出的 overview（2 s 内复用：panel/overview、public-status、monitor 紧接着要同一份）
    pub overview: std::sync::Mutex<Option<(f64, Value)>>,
    /// 原生旧库的数据代（LegacyStore::bump 同步加一）：overview 在同一代内（≤15 s）复用
    pub data_gen: std::sync::atomic::AtomicU64,
    overview_gen: std::sync::atomic::AtomicU64,
    pub ledger_gen: std::sync::atomic::AtomicU64,
    pub totals: std::sync::Mutex<std::collections::HashMap<(i64, u64), serde_json::Map<String, Value>>>,
    pub history: std::sync::Mutex<std::collections::HashMap<(String, i64, i64, u64), Value>>,
    /// history 的增量缓存：(资产, 小时数) → (已定稿边界, 计算时刻, 各桶)
    pub history_inc: std::sync::Mutex<std::collections::HashMap<(String, i64), (i64, f64, Vec<Value>)>>,
}

impl std::fmt::Debug for Memo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Memo(ledger_gen={})", self.gen())
    }
}

impl Memo {
    /// 在窗口上算（先同步到 now）；窗口不可用时 None
    pub fn with_window<T>(&self, db: &Connection, now: f64, f: impl FnOnce(&window::Window) -> py::R<T>) -> Option<py::R<T>> {
        let mut g = self.window.lock().unwrap_or_else(|p| p.into_inner());
        if g.as_ref().is_some_and(|w| w.bloated()) {
            *g = None;
        }
        let w = g.get_or_insert_with(window::Window::default);
        if let Err(e) = w.sync(db, now) {
            *g = None;
            return Some(Err(e));
        }
        Some(f(w))
    }

    /// 在小时汇总上算：表未就绪（回填中）→ None；镜像未载入就先从表载入
    pub fn with_rollup<T>(&self, db: &Connection, now: f64, f: impl FnOnce(&mut rollup::Rollup) -> py::R<T>) -> Option<py::R<T>> {
        if !rollup::ready(db) {
            return None;
        }
        *self.rollup_used_at.lock().unwrap() = now;
        let mut g = self.rollup.lock().unwrap_or_else(|p| p.into_inner());
        if g.is_none() {
            match rollup::Rollup::load(db) {
                Ok(r) => *g = Some(r),
                Err(e) => return Some(Err(e)),
            }
        }
        Some(f(g.as_mut().unwrap()))
    }

    /// 喂数后：把同一批格子并进镜像；衔接不上就作废；ROLLUP_IDLE_S 没人用就释放
    pub fn rollup_apply(&self, start: i64, end: i64, batch: &rollup::Batch, now: f64) {
        let idle = now - *self.rollup_used_at.lock().unwrap() > ROLLUP_IDLE_S;
        let mut g = self.rollup.lock().unwrap_or_else(|p| p.into_inner());
        if idle && g.is_some() {
            *g = None;
            return;
        }
        if let Some(r) = g.as_mut() {
            if !r.apply(start, end, batch, now) {
                *g = None;
            }
        }
    }

    /// 内存读数（/v2/api/v1/debug/mem）
    pub fn mem_stats(&self) -> Value {
        let w = self.window.lock().unwrap_or_else(|p| p.into_inner()).as_ref().map(|w| w.mem_stats());
        let r = self.rollup.lock().unwrap_or_else(|p| p.into_inner()).as_ref().map(|r| r.mem_stats());
        let ov = self.overview.lock().unwrap().as_ref().map(|(_, v)| v.to_string().len());
        let hi = self.history_inc.lock().unwrap();
        let hi_bytes: usize = hi.values().map(|(_, _, b)| b.iter().map(|x| x.to_string().len()).sum::<usize>()).sum();
        let hist = self.history.lock().unwrap();
        let hist_bytes: usize = hist.values().map(|v| v.to_string().len()).sum();
        json!({"window": w, "rollup": r, "overview_json_bytes": ov, "totals": self.totals.lock().unwrap().len(),
            "history": hist.len(), "history_json_bytes": hist_bytes,
            "history_inc": hi.len(), "history_inc_buckets": hi.values().map(|x| x.2.len()).sum::<usize>(), "history_inc_json_bytes": hi_bytes})
    }

    /// 丢掉可重建的缓存（剖析用：比较丢弃前后的匿名内存）
    pub fn drop_caches(&self, what: &str) {
        match what {
            "window" => *self.window.lock().unwrap_or_else(|p| p.into_inner()) = None,
            "rollup" => *self.rollup.lock().unwrap_or_else(|p| p.into_inner()) = None,
            "memo" => {
                *self.overview.lock().unwrap() = None;
                self.totals.lock().unwrap().clear();
                self.history.lock().unwrap().clear();
                self.history_inc.lock().unwrap().clear();
            }
            _ => {}
        }
    }

    pub fn overview_gen(&self) -> u64 {
        self.overview_gen.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_overview_gen(&self, g: u64) {
        self.overview_gen.store(g, std::sync::atomic::Ordering::Relaxed)
    }

    pub fn gen(&self) -> u64 {
        self.ledger_gen.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn bump(&self) {
        self.ledger_gen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.totals.lock().unwrap().clear();
        self.history.lock().unwrap().clear();
    }
}

impl CompatCfg {
    /// 测试服副本的约定（与 staging/fleet_stage.py 的 load_backend 相同）：relays 表里、未在 fleet 表启用的线路机
    pub fn stage_defaults(db: &Connection) -> CompatCfg {
        let names: Vec<String> = db
            .prepare("SELECT relay FROM relays")
            .and_then(|mut s| s.query_map([], |r| r.get::<_, String>(0)).map(|it| it.flatten().collect()))
            .unwrap_or_default();
        let active: Vec<String> = db
            .prepare("SELECT relay, data FROM fleet")
            .and_then(|mut s| {
                s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                    .map(|it| it.flatten().filter(|(_, d)| serde_json::from_str::<Value>(d).map(|v| truthy(get(&v, "active"))).unwrap_or(false)).map(|(n, _)| n).collect())
            })
            .unwrap_or_default();
        CompatCfg {
            config_relays: names.into_iter().filter(|n| !active.contains(n)).collect(),
            public_key: "（测试服副本不含 provision 公钥）".into(),
            packages: json!({}),
            cache_max_age: None,
            memo: None,
            redact_internal: false,
            events_split: false,
            domain_collapse: false,
            path_fresh_s: None,
            egress_ip_check: true,
        }
    }
}

pub struct Ctx<'a> {
    pub db: &'a Connection,
    pub now: f64,
    pub cfg: &'a CompatCfg,
}

const SCHEMA_JSON: &str = include_str!("legacy_schema.json");

pub fn schema() -> Value {
    serde_json::from_str(SCHEMA_JSON).expect("legacy_schema.json")
}

fn md_cell(s: &str) -> String {
    s.replace('|', "\\|").replace('<', "&lt;").replace('>', "&gt;")
}

/// apikeys.schema_markdown()
pub fn schema_markdown() -> String {
    let s = schema();
    let auth = &s["auth"];
    let mut lines: Vec<String> = vec!["# Team Ops API".into(), "".into(), "## 认证".into(), "".into(),
        format!("- header: {}", auth["header"].as_str().unwrap_or("")), format!("- base_url: {}", auth["base_url"].as_str().unwrap_or("")),
        "".into(), "```sh".into(), auth["example"].as_str().unwrap_or("").into(), "```".into(), "".into(), "## 权限级别".into(), "".into()];
    for (scope, d) in s["scopes"].as_object().cloned().unwrap_or_default() {
        lines.push(format!("- {scope}：{}", d.as_str().unwrap_or("")));
    }
    lines.extend(["".into(), "所有密钥均禁止 /sync/hosts、/sync/ack、/sync/pending、/komari/pending、/komari/done。".into(), "".into(), "## 接口".into(), "".into()]);
    let groups = ["概览与资产", "访问记录与行为", "策略", "线路机与接入任务", "3x-ui 面板", "Komari 原生数据", "API 密钥管理"];
    let rules: [(&[&str], &str); 6] = [(&["/activity", "/behavior"], "访问记录与行为"), (&["/policies"], "策略"), (&["/fleet", "/provision", "/jobs"], "线路机与接入任务"),
        (&["/panel"], "3x-ui 面板"), (&["/komari"], "Komari 原生数据"), (&["/api-keys"], "API 密钥管理")];
    let eps = s["endpoints"].as_array().cloned().unwrap_or_default();
    for g in groups {
        lines.extend([format!("### {g}"), "".into(), "| method | path | scope | params | description |".into(), "| --- | --- | --- | --- | --- |".into()]);
        for e in &eps {
            let path = e["path"].as_str().unwrap_or("");
            let mut grp = "概览与资产";
            for (prefixes, name) in rules {
                if prefixes.iter().any(|p| path.starts_with(p)) {
                    grp = name;
                }
            }
            if grp == g {
                let cells: Vec<String> = ["method", "path", "scope", "params", "description"]
                    .iter()
                    .map(|k| md_cell(&match &e[*k] { Value::String(x) => x.clone(), Value::Null => "None".into(), v => v.to_string() }))
                    .collect();
                lines.push(format!("| {} |", cells.join(" | ")));
            }
        }
        lines.push("".into());
    }
    lines.extend(["## UI 动作映射".into(), "".into(), "| action | method | path | description |".into(), "| --- | --- | --- | --- |".into()]);
    for a in s["ui_actions"].as_array().cloned().unwrap_or_default() {
        let cells: Vec<String> = ["action", "method", "path", "description"]
            .iter()
            .map(|k| md_cell(a[*k].as_str().unwrap_or("—")))
            .collect();
        lines.push(format!("| {} |", cells.join(" | ")));
    }
    lines.extend(["".into(), "## 口径说明".into(), "".into(), s["ai_notes"].as_str().unwrap_or("").into(), "".into()]);
    lines.join("\n")
}

fn jobs_list(cx: &Ctx, limit: i64) -> R<Value> {
    let mut out = vec![];
    for mut r in rows(cx.db, "SELECT id,kind,title,status,created_at,finished_at,steps FROM jobs ORDER BY created_at DESC LIMIT ?", &[si(limit)])? {
        let steps = loads(&r.shift_remove("steps").unwrap_or(Value::Null))?;
        let arr = steps.as_array().cloned().unwrap_or_default();
        let ok = arr.iter().filter(|s| get(s, "status") == "ok").count();
        r.insert("step_summary".into(), json!(format!("{ok}/{}", arr.len())));
        out.push(Value::Object(r));
    }
    Ok(json!({"jobs": out}))
}

fn job_get(cx: &Ctx, id: &str) -> R<Value> {
    let mut r = rows(cx.db, "SELECT * FROM jobs WHERE id=?", &[st(id)])?.into_iter().next().ok_or(Err::NotFound)?;
    for k in ["params", "steps", "result"] {
        let v = loads(&r[k])?;
        r.insert(k.into(), v);
    }
    let e = get(&r["result"], "error").clone();
    r.insert("error".into(), e);
    Ok(Value::Object(r))
}

fn with_json(cx: &Ctx, sql: &str, args: &[rusqlite::types::Value], col: &str) -> R<Value> {
    let mut out = vec![];
    for mut r in rows(cx.db, sql, args)? {
        let v = loads(&r[col])?;
        r.insert(col.into(), v);
        out.push(Value::Object(r));
    }
    Ok(Value::Array(out))
}

fn plain(cx: &Ctx, sql: &str) -> R<Value> {
    Ok(Value::Array(rows(cx.db, sql, &[])?.into_iter().map(Value::Object).collect()))
}

/// server.Application.dispatch 的 GET 分支（komari 实时 RPC 与 /komari/pending 除外——那些 fleetd 直接读 Komari）
pub fn dispatch(cx: &Ctx, path: &str, p: &Params) -> R<Value> {
    match path {
        "/komari/hosts" => meta(cx.db, "hosts", json!({})),
        "/provision/defaults" => store::provision_defaults(cx),
        "/jobs" => jobs_list(cx, p_int(p, "limit", 20)?),
        "/panel/overview" => store::panel_overview(cx, p.get("relay").map(|s| s.as_str())),
        "/api-keys" => {
            let mut out = vec![];
            for mut r in rows(cx.db, "SELECT * FROM api_keys ORDER BY created_at DESC", &[])? {
                r.shift_remove("key_hash");
                out.push(Value::Object(r));
            }
            Ok(Value::Array(out))
        }
        "/api-keys/usage" => plain(cx, "SELECT u.ts,u.key_id,k.name,u.method,u.path,u.status,u.ip FROM api_key_usage u JOIN api_keys k ON k.id=u.key_id ORDER BY u.ts DESC LIMIT 100"),
        "/schema" => Ok(schema()),
        "/schema.md" => Ok(json!({"markdown": schema_markdown()})),
        "/behavior" => behavior::summary(cx, p),
        "/node" => monitor::detail(cx, p.get("asset").map(|s| s.as_str())),
        "/incidents" => monitor::incidents(cx, p.get("asset").map(|s| s.as_str()), p.get("status").map(|s| s.as_str()) == Some("open")),
        "/public-status" => store::public_status(cx),
        "/fleet" => store::fleet_state(cx),
        "/overview" => store::overview(cx),
        "/activity" => store::activity(cx, p),
        "/history" => {
            let hours = p_int(p, "hours", 24)?;
            store::history(cx, p.get("asset").map(|s| s.as_str()), hours)
        }
        "/policies" => policy::policy_state(cx),
        "/policies/preview" => policy::preview(cx),
        "/audit" => with_json(cx, "SELECT * FROM audit ORDER BY id DESC LIMIT 150", &[], "detail"),
        "/renewals" => plain(cx, "SELECT * FROM renewals ORDER BY id DESC LIMIT 200"),
        "/export" => store::export(cx),
        "/health" => store::health(cx),
        "/sync/pending" => Ok(Value::Array(store::raw_assets(cx)?.into_iter().filter(|a| truthy(get(a, "sync_pending"))).collect())),
        x if x.starts_with("/jobs/") => job_get(cx, &x[6..]),
        _ => Err(Err::NotFound),
    }
}

/// 按旧 Handler 编码：(状态码, 响应字节)
pub fn respond(cx: &Ctx, path: &str, p: &Params) -> (u16, String) {
    match dispatch(cx, path, p) {
        Ok(mut v) => {
            if cx.cfg.redact_internal {
                crate::catalog::strip_internal(&mut v);
            }
            (200, pyjson::dumps(&json!({"data": v})))
        }
        Err(Err::Bad(m)) => (400, pyjson::dumps(&json!({"error": m}))),
        Err(Err::NotFound) => (404, pyjson::dumps(&json!({"error": "接口不存在"}))),
        Err(Err::Internal(e)) => {
            tracing::warn!(path, error = %e, "compat 读接口失败");
            (500, pyjson::dumps(&json!({"error": "操作未完成，请查看系统状态后重试"})))
        }
    }
}

/// `path?query` → (path, params)，与 `parse_qs`（丢弃空值、同名取最后一个）一致
pub fn split_query(full: &str) -> (String, Params) {
    let (path, q) = full.split_once('?').unwrap_or((full, ""));
    let mut p = Params::new();
    for (k, v) in form_urlencoded::parse(q.as_bytes()) {
        if !v.is_empty() {
            p.insert(k.into_owned(), v.into_owned());
        }
    }
    (path.to_string(), p)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schema_md_shape() {
        let md = schema_markdown();
        assert!(md.starts_with("# Team Ops API\n\n## 认证\n\n- header: X-Api-Key"));
        assert!(md.ends_with("\n"));
        assert!(md.contains("| GET | /jobs/&lt;id&gt; | read |"));
    }
    #[test]
    fn query() {
        let (p, q) = split_query("/activity?limit=10&team=%E5%9B%A2%E9%98%9F1&x=&limit=20");
        assert_eq!(p, "/activity");
        assert_eq!(q.get("limit").map(|s| s.as_str()), Some("20"));
        assert_eq!(q.get("team").map(|s| s.as_str()), Some("团队1"));
        assert!(!q.contains_key("x"));
    }
}
