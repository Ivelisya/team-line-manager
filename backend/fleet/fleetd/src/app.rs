//! 进程内共享状态与后台任务：共享轮询器（自适应节奏）、规则评估、告警时钟、通知投递、保留期清理。
use crate::alerts::{Alert, Engine, EngineCfg, GroupState, Observation, Severity, Silence, State};
use crate::auth::Auth;
use crate::catalog::{self, Catalog};
use crate::clock::Clock;
use crate::config::{read_secret, Config};
use crate::komari::Komari;
use crate::legacy::Legacy;
use crate::notify::Notifier;
use crate::rules::{self, PlanIndex};
use crate::sse::Hub;
use crate::store::{self, Store};
use crate::system::SysStats;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::Notify;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    Legacy,
    Komari,
}

pub struct PollRes {
    pub name: &'static str,
    pub key: String,
    pub source: Source,
    pub active_s: u64,
    pub idle_s: Option<u64>,
    pub st: Mutex<PollState>,
}

#[derive(Default, Clone, Serialize)]
pub struct PollState {
    pub polls: u64,
    pub last_attempt: f64,
    pub last_ok: f64,
    pub version: u64,
    #[serde(skip)]
    pub hash: Option<[u8; 32]>,
    pub errors: u64,
    pub consecutive_errors: u32,
    pub last_latency_ms: f64,
    pub last_bytes: usize,
    pub last_error: Option<String>,
}

#[derive(Default, Clone, Serialize)]
pub struct EvalInfo {
    /// 上一次评估时启动宽限是否仍在（alerts.startup_grace_s）
    pub startup_hold: bool,
    pub last_at: f64,
    pub observations: usize,
    pub duration_ms: f64,
    pub skipped: Option<String>,
    pub evaluations: u64,
}

#[derive(Default, Clone, Serialize)]
pub struct UpHealth {
    pub consecutive_failures: u32,
    pub last_ok: f64,
    pub last_error: Option<String>,
}

pub struct App {
    pub cfg: Config,
    pub clock: Arc<dyn Clock>,
    pub store: Store,
    pub legacy: Legacy,
    pub komari: Komari,
    pub auth: Auth,
    pub hub: Arc<Hub>,
    pub engine: Mutex<Engine>,
    pub notifier: Notifier,
    pub catalog: RwLock<Catalog>,
    pub plans: RwLock<PlanIndex>,
    pub started_at: f64,
    pub sys: Mutex<SysStats>,
    pub poll_wake: Notify,
    pub polls: Vec<PollRes>,
    pub eval: Mutex<EvalInfo>,
    pub komari_health: Mutex<UpHealth>,
    pub proxy_requests: std::sync::atomic::AtomicU64,
    pub proxy_hits: std::sync::atomic::AtomicU64,
    pub ipintel: Option<Arc<ipintel::IpIntel>>,
    pub ipintel_state: Mutex<IpIntelState>,
    pub health_shadow: Mutex<HealthShadow>,
    pub ingest: fleetd_ingest::Shared,
    pub native: Mutex<crate::native::NativeHealth>,
    pub onboard: Option<onboard::api::OnboardApi>,
    pub login_limit: crate::accounts::RateLimit,
    /// relayd 最近一次来取期望状态的时间（host_id → t）：60 s 内取过才算“管理模式在线”
    pub relay_polls: Mutex<std::collections::HashMap<String, f64>>,
    pub shadow_cache: Mutex<Option<(f64, Value)>>,
    /// fleetd 会话校验缓存：token sha256 → (缓存到期, 用户名)
    pub fleet_sessions: Mutex<std::collections::HashMap<String, (f64, Option<String>)>>,
    /// 原生旧库与下线闸门比较器
    pub legacy_store: Arc<crate::legacystore::LegacyStore>,
    /// 请求统计：路径 → (次数, 总耗时 ms)
    /// probes：线路机自身探测（probe_land）已读到的 measured_at
    pub relay_probe_cursor: Mutex<f64>,
    /// overview / health 的版本快照（GET /v2/api/v1/delta/{res}）
    pub snapshots: crate::delta::Snapshots,
    /// probes/latest（不带 since）的渲染结果：(探测代, 正文, ETag)；新样本写入时代 +1
    pub probes_gen: std::sync::atomic::AtomicU64,
    pub probes_cache: Mutex<Option<(u64, bytes::Bytes, String)>>,
    pub req_stats: Mutex<std::collections::HashMap<String, (u64, f64, std::collections::VecDeque<f64>)>>,
    /// ipintel 读原生旧库 activity 的 rowid 游标
    pub ipintel_cursor: Mutex<i64>,
    /// 启动后第一批边缘上报到达的时刻（启动宽限用）
    pub first_ingest_at: Mutex<Option<f64>>,
    pub shadow_cmp: Arc<crate::shadowcmp::ShadowCmp>,
}

/// 影子运行：把旧 /overview 喂给移植版 monitor（fleet-health::legacy），逐节点与旧 /health 比较状态与原因码。
/// 只用来核对接线与移植；告警仍然来自旧 /health。
#[derive(Default)]
pub struct HealthShadow {
    pub monitor: fleet_health::Monitor,
    pub evaluations: u64,
    pub compared: u64,
    pub agree: u64,
    pub last_at: f64,
    pub last_ms: f64,
    pub last_disagree: Vec<Value>,
    pub disagree_by_reason: std::collections::BTreeMap<String, u64>,
}

/// 用同一份 /overview 跑移植版 monitor，与旧 /health 的 monitor.nodes 比较（旧系统 monitor 在自己的时刻评估，偶有一步之差）。
pub fn health_shadow_step(app: &App, ov: &Value, health: &Value) {
    let t0 = std::time::Instant::now();
    let Some(assets) = ov.get("assets").and_then(|x| x.as_array()) else { return };
    let relays = ov.get("relays").cloned().unwrap_or(Value::Null);
    let now = ov.get("now").and_then(|x| x.as_f64()).unwrap_or_else(|| app.clock.now());
    let mut sh = app.health_shadow.lock().unwrap();
    let ev = sh.monitor.evaluate(assets, &relays, now);
    // 分钟桶只在影子里没用处：清掉，小时桶留 2 天
    sh.monitor.minutes.clear();
    for h in sh.monitor.hours.values_mut() {
        h.retain(|b, _| *b as f64 >= now - 2.0 * 86400.0);
    }
    let theirs = health.pointer("/monitor/nodes").cloned().unwrap_or(Value::Null);
    let mut dis = vec![];
    let mut agree = 0u64;
    let mut compared = 0u64;
    for n in &ev.nodes {
        let Some(t) = theirs.get(&n.uuid) else { continue };
        compared += 1;
        let ts = t.get("status").and_then(|x| x.as_str()).unwrap_or("");
        let tr = t.get("reason").and_then(|x| x.as_str());
        if ts == n.status && tr == n.reason.as_deref() {
            agree += 1;
        } else {
            let key = format!("{}/{} vs {}/{}", n.status, n.reason.as_deref().unwrap_or("-"), ts, tr.unwrap_or("-"));
            *sh.disagree_by_reason.entry(key).or_default() += 1;
            dis.push(json!({"node": crate::rules::short_id(&n.name), "ours": {"status": n.status, "reason": n.reason},
                "legacy": {"status": ts, "reason": tr}}));
        }
    }
    sh.evaluations += 1;
    sh.compared += compared;
    sh.agree += agree;
    sh.last_at = now;
    sh.last_ms = t0.elapsed().as_secs_f64() * 1000.0;
    sh.last_disagree = dis;
}

#[derive(Default, Clone, Serialize)]
pub struct IpIntelState {
    pub backfill: String,
    pub backfill_requests: u64,
    pub backfill_events: u64,
    pub backfill_started_at: f64,
    pub backfill_done_at: f64,
    pub loops: u64,
    pub last_loop_at: f64,
    pub last_error: Option<String>,
    pub anomalies_opened: u64,
}

fn poll_table(cfg: &Config, komari_key: bool) -> Vec<PollRes> {
    let p = &cfg.poll;
    let mk = |name, key: &str, source, active_s, idle_s| PollRes {
        name,
        key: key.to_string(),
        source,
        active_s,
        idle_s,
        st: Mutex::new(PollState::default()),
    };
    let mut v = vec![
        // 原生模式下 overview 由边缘批次推送刷新（legacystore::run），定时轮询只兜底
        mk("overview", "/overview", Source::Legacy, if cfg.legacy_source == "native" { p.active_overview_s.max(30) } else { p.active_overview_s }, Some(p.idle_overview_s)),
        mk("health", "/health", Source::Legacy, if cfg.legacy_source == "native" { p.active_overview_s.max(30) } else { p.active_overview_s }, Some(p.idle_overview_s)),
        mk("panel", "/panel/overview", Source::Legacy, p.active_panel_s, None),
        mk("history", "/history?hours=24", Source::Legacy, p.active_history_s, None),
        mk("incidents", "/incidents", Source::Legacy, p.active_history_s, None),
        mk("behavior", "/behavior", Source::Legacy, p.active_history_s, None),
        mk("activity", "/activity?limit=500", Source::Legacy, p.active_history_s, None),
        mk("renewals", "/renewals", Source::Legacy, p.active_slow_s, None),
        mk("jobs", "/jobs?limit=50", Source::Legacy, p.active_slow_s, None),
        mk("audit", "/audit", Source::Legacy, p.active_slow_s, None),
        mk("provision_defaults", "/provision/defaults", Source::Legacy, 1800, None),
    ];
    if komari_key {
        // 有 Komari 系统 API 密钥时才在后台读 Komari；否则 komari/* 只按需代理（转发请求者的会话）。
        v.push(mk("komari_hosts", "/komari/hosts", Source::Komari, p.active_history_s, None));
        v.push(mk("komari_clients", "/komari/clients", Source::Komari, p.active_slow_s, None));
    }
    v
}

impl App {
    pub fn build(cfg: Config, clock: Arc<dyn Clock>) -> Result<Arc<App>, String> {
        let now = clock.now();
        let legacy_key = match &cfg.legacy.key_file {
            Some(p) => Some(read_secret(p)?),
            None => None,
        };
        let komari_key = match &cfg.komari.api_key_file {
            Some(p) => Some(read_secret(p)?),
            None => None,
        };
        let store = Store::open(&cfg.db_path())?;
        if let Some(path) = &cfg.catalog {
            match catalog::import(&store, path, "system", now) {
                Ok(r) => tracing::info!(plans = r.plans, assets = r.assets, changed = r.changed, "catalog imported"),
                Err(e) => tracing::warn!(error = %e, "catalog import failed; keeping previous copy"),
            }
        }
        let cat = Catalog::load(&store);
        let plans = catalog::plan_index(&cat);
        let mut engine = Engine::new(EngineCfg::from_config(&cfg), now);
        let (alerts, groups, max_id, last_digest, silences) = store.call_blocking(load_engine_state);
        tracing::info!(alerts = alerts.len(), groups = groups.len(), silences = silences.len(), "alert state restored");
        engine.load(alerts, groups, max_id, last_digest);
        engine.set_silences(silences);
        let notifier = Notifier::from_config(&cfg.notify)?;
        let legacy = Legacy::new(&cfg.legacy.base_url, legacy_key, &cfg.legacy.actor, cfg.legacy.concurrency, cfg.legacy.timeout_s, clock.clone());
        let komari = Komari::new(&cfg.komari.base_url, komari_key.clone(), &cfg.auth.cookie_name, cfg.komari.timeout_s, clock.clone());
        let auth = Auth::new(cfg.auth.mode, &cfg.auth.cookie_name, cfg.auth.cache_ttl_s);
        let polls = poll_table(&cfg, komari_key.is_some());
        let ipintel = if cfg.ipintel.enabled {
            match ipintel::IpIntel::open(cfg.ipintel.clone()) {
                Ok(s) => {
                    tracing::info!(status = %s.status()["version"], "ipintel ready");
                    Some(s)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "ipintel disabled");
                    None
                }
            }
        } else {
            None
        };
        let native_restore: Value = store
            .call_blocking(|c| store::meta_get(c, "native_coverage"))
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null);
        let ingest = fleetd_ingest::Ingest::open(fleetd_ingest::IngestCfg { db_path: cfg.data_dir.join("ingest.db"), ..Default::default() })?;
        let onboard = match &cfg.onboard_config {
            Some(p) => match onboard::config::Config::load(p).and_then(|c| onboard::api::OnboardApi::new(Arc::new(c))) {
                Ok(o) => {
                    tracing::info!(config = %p.display(), "onboard pipeline ready");
                    // P11 装 nodeagent：令牌由本进程的 fleetd-ingest 签发/撤销（onboard 不自己打开 ingest.db）
                    Some(o.with_agent_tokens(Arc::new(OnboardAgentTokens(ingest.clone()))))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "onboard disabled");
                    None
                }
            },
            None => None,
        };
        let relay_polls: std::collections::HashMap<String, f64> =
            store.call_blocking(|c| store::meta_get(c, "relay_polls")).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let legacy_store = crate::legacystore::LegacyStore::new(cfg.legacy_db_path());
        if cfg.legacy_source == "native" {
            let _ = legacy.native.set(legacy_store.clone());
        }
        if let Some(ii) = ipintel.clone() {
            // /activity 的 rows、sources 与 /behavior 的 sources：每个来源 IP 附上画像 source_intel（新增字段，原字段不变）
            let _ = legacy.decorate.set(Arc::new(move |_path: &str, v: &mut Value| -> bool {
                let d = &mut v["data"];
                let mut ips: Vec<std::net::IpAddr> = vec![];
                for list in ["rows", "sources"] {
                    for r in d[list].as_array().into_iter().flatten() {
                        if let Some(ip) = r["source_ip"].as_str().and_then(|s| s.parse().ok()) {
                            if !ips.contains(&ip) {
                                ips.push(ip);
                            }
                        }
                    }
                }
                if ips.is_empty() {
                    return false;
                }
                let intel = ii.lookup_many(&ips, crate::clock::SystemClock.now());
                for list in ["rows", "sources"] {
                    if let Some(arr) = d[list].as_array_mut() {
                        for r in arr.iter_mut() {
                            if let Some(ip) = r["source_ip"].as_str().map(String::from) {
                                let x = &intel[ip.as_str()];
                                if x.is_object() {
                                    r["source_intel"] = json!({"isp": x["isp"], "asn": x["asn"], "country": x["country"], "region": x["region"], "city": x["city"],
                                        "place": x["place"], "type": x["type"], "type_word": x["type_word"], "internal": x["internal"]});
                                }
                            }
                        }
                    }
                }
                true
            }));
        }
        Ok(Arc::new(App {
            ingest,
            native: Mutex::new(crate::native::NativeHealth::with_restore(&native_restore)),
            onboard,
            login_limit: Default::default(),
            relay_polls: Mutex::new(relay_polls),
            shadow_cache: Mutex::new(None),
            fleet_sessions: Mutex::new(Default::default()),
            legacy_store,
            ipintel_cursor: Mutex::new(0),
            first_ingest_at: Mutex::new(None),
            req_stats: Mutex::new(Default::default()),
            relay_probe_cursor: Mutex::new(0.0),
            snapshots: Default::default(),
            probes_gen: Default::default(),
            probes_cache: Mutex::new(None),
            shadow_cmp: Arc::new(crate::shadowcmp::ShadowCmp::default()),
            ipintel,
            ipintel_state: Mutex::new(IpIntelState::default()),
            health_shadow: Mutex::new(HealthShadow::default()),
            hub: {
                let h = Arc::new(Hub::new(now as u64));
                // 访问规则（草稿 / 发布 / 回执定版）：写路径与喂数里的回执核对发布
                h.declare("policies");
                h
            },
            engine: Mutex::new(engine),
            notifier,
            catalog: RwLock::new(cat),
            plans: RwLock::new(plans),
            started_at: now,
            sys: Mutex::new(SysStats::default()),
            poll_wake: Notify::new(),
            polls,
            eval: Mutex::new(EvalInfo::default()),
            komari_health: Mutex::new(UpHealth::default()),
            proxy_requests: Default::default(),
            proxy_hits: Default::default(),
            cfg,
            clock,
            store,
            legacy,
            komari,
            auth,
        }))
    }

    /// fleetd 会话校验（结果缓存 30 s；退出、改密码、撤销会话时清缓存）
    pub async fn fleet_session(&self, token: &str, now: f64) -> Option<String> {
        let h = crate::accounts::sha256_hex(token);
        if let Some((until, v)) = self.fleet_sessions.lock().unwrap().get(&h).cloned() {
            if now < until {
                return v;
            }
        }
        let t = token.to_string();
        let v = self.store.call(move |c| crate::accounts::check_session(c, &t, now).map(|x| x.1)).await;
        let mut m = self.fleet_sessions.lock().unwrap();
        if m.len() > 4096 {
            m.clear();
        }
        m.insert(h, (now + 30.0, v.clone()));
        v
    }

    pub fn forget_session(&self, token: &str) {
        self.fleet_sessions.lock().unwrap().remove(&crate::accounts::sha256_hex(token));
    }

    pub fn forget_all_sessions(&self) {
        self.fleet_sessions.lock().unwrap().clear();
    }

    /// 实际已挂上的功能（界面据此决定显示哪些操作，避免去碰 404）
    pub fn features(&self) -> Vec<&'static str> {
        let mut f = vec!["auth", "alerts", "silences", "maintenance", "probes", "edge", "health.native", "legacy.read", "ops", "relay.spec"];
        if !self.managed_relays().is_empty() {
            // 至少一台线路机的 relayd 在线接收期望状态时才开放入口操作
            f.extend(["entries", "entries.enable", "entries.disable", "entries.rename", "entries.rotate_uuid", "entries.share", "entries.reset_traffic",
                "restart_xray", "relay.reorder", "xray_logs"]);
        }
        if self.onboard.is_some() {
            f.push("onboard");
        }
        if self.cfg.legacy_source == "native" {
            // 旧系统下线后：旧形状的写接口原生提供（POST /v2/api/v1/legacy-write/<旧路径>）
            f.extend(["legacy.native", "legacy.write", "assets.edit", "renewals", "node.profile", "incidents.ack", "behavior.ack", "api_keys",
                "policies", "policies.apply"]);
        }
        if self.ipintel.is_some() {
            f.extend(["ipintel", "ipintel.batch", "activity.source_intel", "activity.delta", "delta.overview", "delta.health"]);
        }
        f
    }

    /// 立即重取这些轮询资源（下一次轮询不等周期），用于推送刷新
    pub fn force_poll(&self, names: &[&str]) {
        for p in self.polls.iter().filter(|p| names.contains(&p.name)) {
            p.st.lock().unwrap().last_attempt = 0.0;
        }
        self.poll_wake.notify_one();
    }

    /// relayd 管理模式在线的线路机（host_id）
    pub fn managed_relays(&self) -> Vec<String> {
        let now = self.clock.now();
        self.relay_polls.lock().unwrap().iter().filter(|(_, t)| now - **t <= 60.0).map(|(h, _)| h.clone()).collect()
    }

    pub fn active(&self) -> bool {
        self.hub.clients() > 0
    }

    pub fn poll_res(&self, key: &str) -> Option<&PollRes> {
        self.polls.iter().find(|p| p.key == key)
    }

    /// 按需代理的缓存时长：轮询资源按当前节奏；其余按路径分档，最少 10 s。
    pub fn ttl_for(&self, key: &str) -> f64 {
        if let Some(p) = self.poll_res(key) {
            let iv = if self.active() { p.active_s } else { p.idle_s.unwrap_or(p.active_s) };
            return (iv.max(10) as f64) - 0.5;
        }
        let base = self.cfg.poll.ondemand_ttl_s as f64;
        let path = key.split('?').next().unwrap_or(key);
        let t = if path == "/history" || path == "/behavior" || path.ends_with("/metrics") || path.ends_with("/results") {
            60.0
        } else if path == "/komari/pings" || path == "/provision/defaults" || path == "/schema" || path == "/schema.md" {
            300.0
        } else if path.starts_with("/jobs/") {
            30.0
        } else if path == "/node" {
            // 抽屉打开时的详情：10 s 内同一节点共用一次（旧后端每次都重算整份 overview）
            return 10.0
        } else {
            base
        };
        t.max(base).max(10.0)
    }

    /// 从缓存取某个轮询资源的 data（解析 {"data": …}）。
    pub fn cached_data(&self, key: &str, max_age: f64) -> Option<Value> {
        let e = self.legacy.cache.peek(key)?;
        if e.status != 200 || self.clock.now() - e.fetched_at > max_age {
            return None;
        }
        let v: Value = serde_json::from_slice(&e.body).ok()?;
        v.get("data").cloned()
    }

    pub async fn overview_data(&self) -> Option<Value> {
        let (r, _) = self.legacy.get("/overview", self.ttl_for("/overview")).await;
        let e = r.ok()?;
        if e.status != 200 {
            return None;
        }
        serde_json::from_slice::<Value>(&e.body).ok()?.get("data").cloned()
    }

    pub fn reload_catalog(&self) {
        let cat = Catalog::load(&self.store);
        *self.plans.write().unwrap() = catalog::plan_index(&cat);
        *self.catalog.write().unwrap() = cat;
    }

    pub async fn reload_silences(&self) {
        let s = self.store.call(|c| load_silences(c)).await;
        self.engine.lock().unwrap().set_silences(s);
    }
}

// ─── 告警状态持久化 ─────────────────────────────────────────────────────────────────────────────────

pub fn alert_from_row(r: &rusqlite::Row) -> rusqlite::Result<Alert> {
    let labels: String = r.get("labels")?;
    let evidence: String = r.get("evidence")?;
    let ns: Option<String> = r.get("notified_state")?;
    Ok(Alert {
        id: r.get("id")?,
        fingerprint: r.get("fingerprint")?,
        rule: r.get("rule")?,
        severity: Severity::parse(&r.get::<_, String>("severity")?).unwrap_or(Severity::Warning),
        state: State::parse(&r.get::<_, String>("state")?).unwrap_or(State::Resolved),
        labels: serde_json::from_str(&labels).unwrap_or_default(),
        summary: r.get("summary")?,
        evidence: serde_json::from_str(&evidence).unwrap_or(Value::Null),
        pending_since: r.get("pending_since")?,
        fired_at: r.get("fired_at")?,
        resolved_at: r.get("resolved_at")?,
        last_seen: r.get("last_seen")?,
        ack_at: r.get("ack_at")?,
        ack_by: r.get("ack_by")?,
        suppressed: r.get("suppressed")?,
        group_key: r.get("group_key")?,
        notified_state: ns.as_deref().and_then(State::parse),
        notify_resolved: r.get::<_, i64>("notify_resolved")? != 0,
        digest_done: r.get::<_, i64>("digest_done")? != 0,
    })
}

pub fn persist_alert(c: &Connection, a: &Alert) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO alerts(id,fingerprint,rule,severity,state,labels,summary,evidence,pending_since,fired_at,resolved_at,last_seen,
           ack_at,ack_by,suppressed,group_key,notified_state,notify_resolved,digest_done)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)
         ON CONFLICT(id) DO UPDATE SET severity=excluded.severity,state=excluded.state,summary=excluded.summary,
           evidence=excluded.evidence,fired_at=excluded.fired_at,resolved_at=excluded.resolved_at,last_seen=excluded.last_seen,
           ack_at=excluded.ack_at,ack_by=excluded.ack_by,suppressed=excluded.suppressed,notified_state=excluded.notified_state,
           digest_done=excluded.digest_done",
        params![a.id, a.fingerprint, a.rule, a.severity.as_str(), a.state.as_str(), serde_json::to_string(&a.labels).unwrap(),
            a.summary, a.evidence.to_string(), a.pending_since, a.fired_at, a.resolved_at, a.last_seen, a.ack_at, a.ack_by,
            a.suppressed, a.group_key, a.notified_state.map(|s| s.as_str()), a.notify_resolved as i64, a.digest_done as i64],
    )?;
    Ok(())
}

pub fn persist_dirty(c: &mut Connection, alerts: &[Alert], groups: &[(String, Option<GroupState>)], last_digest: Option<f64>) -> rusqlite::Result<()> {
    let tx = c.transaction()?;
    for a in alerts {
        persist_alert(&tx, a)?;
    }
    for (k, g) in groups {
        match g {
            Some(g) => {
                tx.execute(
                    "INSERT INTO alert_groups(key,data) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET data=excluded.data",
                    params![k, serde_json::to_string(g).unwrap()],
                )?;
            }
            None => {
                tx.execute("DELETE FROM alert_groups WHERE key=?", [k])?;
            }
        }
    }
    if let Some(t) = last_digest {
        store::meta_set(&tx, "last_digest_at", &t.to_string())?;
    }
    tx.commit()
}

pub fn load_silences(c: &Connection) -> Vec<Silence> {
    let mut st = c
        .prepare("SELECT id,kind,matchers,starts_at,ends_at,tz,comment,created_by FROM silences WHERE deleted_at IS NULL")
        .unwrap();
    st.query_map([], |r| {
        let m: String = r.get(2)?;
        Ok(Silence {
            id: r.get(0)?,
            kind: r.get(1)?,
            matchers: serde_json::from_str(&m).unwrap_or_default(),
            starts_at: r.get(3)?,
            ends_at: r.get(4)?,
            tz: r.get(5)?,
            comment: r.get(6)?,
            created_by: r.get(7)?,
        })
    })
    .unwrap()
    .filter_map(Result::ok)
    .collect()
}

#[allow(clippy::type_complexity)]
fn load_engine_state(c: &mut Connection) -> (Vec<Alert>, Vec<(String, GroupState)>, i64, Option<f64>, Vec<Silence>) {
    let mut st = c
        .prepare(
            "SELECT * FROM alerts WHERE state!='resolved' OR digest_done=0 OR (notified_state='firing' AND notify_resolved=1)",
        )
        .unwrap();
    let alerts: Vec<Alert> = st.query_map([], alert_from_row).unwrap().filter_map(Result::ok).collect();
    let mut st = c.prepare("SELECT key,data FROM alert_groups").unwrap();
    let groups = st
        .query_map([], |r| {
            let d: String = r.get(1)?;
            Ok((r.get::<_, String>(0)?, serde_json::from_str::<GroupState>(&d).unwrap_or_default()))
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    let max_id: i64 = c.query_row("SELECT coalesce(max(id),0) FROM alerts", [], |r| r.get(0)).unwrap_or(0);
    let last_digest = store::meta_get(c, "last_digest_at").and_then(|s| s.parse().ok());
    (alerts, groups, max_id, last_digest, load_silences(c))
}

pub fn alert_json(a: &Alert, ui_base: &str) -> Value {
    let link = if let Some(n) = a.labels.get("node") {
        format!("{ui_base}nodes/{n}")
    } else if let Some(r) = a.labels.get("relay_id") {
        format!("{ui_base}lines/{r}")
    } else {
        format!("{ui_base}alerts")
    };
    json!({"id": a.id, "rule": a.rule, "title": crate::alerts::rule_title(&a.rule), "severity": a.severity, "state": a.state,
        "labels": a.labels, "summary": a.summary, "evidence": a.evidence, "since": a.pending_since, "fired_at": a.fired_at,
        "resolved_at": a.resolved_at, "last_seen": a.last_seen, "ack_at": a.ack_at, "ack_by": a.ack_by, "acked": a.ack_at.is_some(),
        "suppressed": a.suppressed, "group_key": a.group_key, "notified": a.notified_state, "link": link})
}

// ─── 后台任务 ──────────────────────────────────────────────────────────────────────────────────────

pub fn spawn_background(app: &Arc<App>) {
    app.ingest.spawn_maintenance();
    tokio::spawn(poller(app.clone()));
    {
        // ingest 组提交：每秒提交一次（见 fleetd_ingest::Ingest::flush）
        let ing = app.ingest.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_secs(1));
            loop {
                iv.tick().await;
                let i2 = ing.clone();
                let _ = tokio::task::spawn_blocking(move || i2.flush()).await;
            }
        });
    }
    tokio::spawn(alert_clock(app.clone()));
    tokio::spawn(housekeeping(app.clone()));
    if let Some(url) = app.cfg.notify.healthchecks_url.clone() {
        tokio::spawn(deadman(app.clone(), url));
    }
    if let Some(ii) = app.ipintel.clone() {
        tokio::spawn(ipintel_loop(app.clone(), ii));
    }
    if app.onboard.is_some() {
        tokio::spawn(onboard_events(app.clone()));
    }
    tokio::spawn(crate::legacystore::run(app.clone(), app.legacy_store.clone()));
    if app.cfg.legacy_source != "native" {
        // 旧系统还在：逐路径比较原生 vs 旧（下线闸门）
        tokio::spawn(crate::shadowcmp::run(app.clone(), app.legacy_store.clone(), app.shadow_cmp.clone(), 60));
    }
}

// ─── IP 情报 ──────────────────────────────────────────────────────────────────────────────────────

async fn fetch_json(app: &App, key: &str, max_age: f64) -> Option<Value> {
    let (r, _) = app.legacy.get(key, max_age).await;
    let e = r.ok()?;
    if e.status != 200 {
        return None;
    }
    serde_json::from_slice(&e.body).ok()
}

/// 启动回填：每个节点最近 N 天（≤ 14）按天取样（旧接口 /activity 每页最多 500 条），
/// 串行、限速，与共享轮询器共用到旧后端的并发上限 2。完成后记进 meta，重启不重复回填。
async fn ipintel_backfill(app: &Arc<App>, ii: &Arc<ipintel::IpIntel>) {
    let days = app.cfg.ipintel_backfill_days as i64;
    if days == 0 {
        app.ipintel_state.lock().unwrap().backfill = "disabled".into();
        return;
    }
    let done: Option<f64> = app.store.call(|c| store::meta_get(c, "ipintel_backfill_done_at").and_then(|s| s.parse().ok())).await;
    let now = app.clock.now();
    if let Some(t) = done {
        if now - t < days as f64 * 86400.0 {
            let mut st = app.ipintel_state.lock().unwrap();
            st.backfill = "done-earlier".into();
            st.backfill_done_at = t;
            return;
        }
    }
    let Some(h) = fetch_json(app, "/health", 90.0).await else {
        app.ipintel_state.lock().unwrap().backfill = "skipped: /health 不可用".into();
        return;
    };
    let nodes = ipintel::legacy::health_nodes(&h);
    {
        let ii2 = ii.clone();
        let n2 = nodes.clone();
        let _ = tokio::task::spawn_blocking(move || ii2.upsert_nodes(n2, now)).await;
    }
    {
        let mut st = app.ipintel_state.lock().unwrap();
        st.backfill = "running".into();
        st.backfill_started_at = now;
    }
    tracing::info!(nodes = nodes.len(), days, "ipintel backfill started");
    for n in nodes.iter().filter(|n| n.team.as_deref() != Some("线路机")) {
        // 按天取样（旧接口每页最多 500 条，且每个请求都会在旧后端上按窗口做聚合，代价随窗口变大）：
        // 最近 3 天每天一页，更早的隔一天一页，最后一页落在最早那天 → 14 天 8 页/节点
        let mut sample: Vec<i64> = vec![0, 1, 2, 4, 6, 8, 10, 13];
        sample.retain(|d| *d < days);
        for d in sample {
            let before = (now - d as f64 * 86400.0).floor();
            let seconds = ((d + 1) * 86400).min(14 * 86400);
            let key = crate::legacy::normalize(
                "/activity",
                Some(&format!("asset={}&limit=500&seconds={}&before={}", n.node, seconds, before as i64)),
            )
            .unwrap();
            let v = fetch_json(app, &key, 3600.0).await;
            // 回填结果不留在代理缓存里（量大、没人会再读）
            app.legacy.cache.purge_key(&key);
            let st_requests = 1;
            let mut events = 0;
            if let Some(v) = v {
                let ev = ipintel::legacy::activity_events(&v);
                events = ev.len();
                let ii2 = ii.clone();
                let now2 = app.clock.now();
                let _ = tokio::task::spawn_blocking(move || ii2.ingest_sources(ev, now2)).await;
            }
            {
                let mut st = app.ipintel_state.lock().unwrap();
                st.backfill_requests += st_requests;
                st.backfill_events += events as u64;
            }
            // 低优先级：没人查看时 8 s 一个请求，有人查看时 15 s（a395 实测每请求约 0.26 s 旧后端 CPU）
            tokio::time::sleep(Duration::from_secs(if app.active() { 15 } else { 8 })).await;
        }
    }
    let t = app.clock.now();
    let _ = app.store.call(move |c| store::meta_set(c, "ipintel_backfill_done_at", &t.to_string())).await;
    let mut st = app.ipintel_state.lock().unwrap();
    st.backfill = "done".into();
    st.backfill_done_at = t;
    tracing::info!(requests = st.backfill_requests, events = st.backfill_events, "ipintel backfill done");
}

async fn ipintel_loop(app: Arc<App>, ii: Arc<ipintel::IpIntel>) {
    let scope: HashSet<String> = ipintel::RULES.iter().map(|s| s.to_string()).collect();
    // 先等共享轮询器取到第一份 /overview、/health
    tokio::time::sleep(Duration::from_secs(5)).await;
    let bf_app = app.clone();
    let bf_ii = ii.clone();
    tokio::spawn(async move { ipintel_backfill(&bf_app, &bf_ii).await });
    let mut tick: u64 = 0;
    loop {
        let now = app.clock.now();
        // /health 与共享轮询器同一份缓存（90 s 内不重取）；/activity?limit=500 同 key
        let h = match app.cached_data("/health", 90.0) {
            Some(d) => Some(json!({"data": d})),
            None => fetch_json(&app, "/health", 25.0).await,
        };
        let a = if app.cfg.legacy_source == "native" {
            // 原生：直接按 rowid 游标读原生旧库的全部新访问记录（不经旧接口渲染、不触发 24 h 聚合）
            let ls = app.legacy_store.clone();
            let cur = *app.ipintel_cursor.lock().unwrap();
            let r = tokio::task::spawn_blocking(move || -> Option<(i64, Value)> {
                let _span = crate::cpu::Span::new("ipintel.read");
                let c = ls.reader()?;
                let start = if cur > 0 { cur } else { c.query_row("SELECT coalesce(max(rowid),0)-500 FROM activity", [], |r| r.get(0)).ok()? };
                let mut st = c.prepare("SELECT rowid,id,ts,asset,team,relay,source_ip,client FROM activity WHERE rowid>?1 ORDER BY rowid LIMIT 20000").ok()?;
                let mut last = start;
                let rows: Vec<Value> = st
                    .query_map([start], |r| {
                        Ok((r.get::<_, i64>(0)?, json!({"id": r.get::<_, Option<String>>(1)?, "ts": r.get::<_, Option<f64>>(2)?, "asset": r.get::<_, Option<String>>(3)?,
                            "team": r.get::<_, Option<String>>(4)?, "relay": r.get::<_, Option<String>>(5)?, "source_ip": r.get::<_, Option<String>>(6)?,
                            "client": r.get::<_, Option<String>>(7)?})))
                    })
                    .ok()?
                    .flatten()
                    .map(|(rid, v)| {
                        last = last.max(rid);
                        v
                    })
                    .collect();
                Some((last, json!({"data": {"rows": rows}})))
            })
            .await
            .ok()
            .flatten();
            r.map(|(last, v)| {
                *app.ipintel_cursor.lock().unwrap() = last;
                v
            })
        } else {
            fetch_json(&app, "/activity?limit=500", 25.0).await
        };
        // 线路机公网 IP 从旧系统资产里自动加入内部地址（配置里也有默认三台）
        if let Some(ov) = app.cached_data("/overview", 600.0) {
            for x in ov.get("assets").and_then(|v| v.as_array()).into_iter().flatten() {
                if x.get("team").and_then(|t| t.as_str()) == Some("线路机") {
                    if let Some(ip) = x.get("ip").and_then(|v| v.as_str()).and_then(|v| v.parse().ok()) {
                        ii.set_internal(ip, x.get("name").and_then(|v| v.as_str()).unwrap_or("线路机"));
                    }
                }
            }
        }
        let ii2 = ii.clone();
        let t = tick;
        let res = tokio::task::spawn_blocking(move || {
            let _span = crate::cpu::Span::new("ipintel.ingest");
            let mut opened = 0usize;
            let mut errs = vec![];
            if let Some(h) = &h {
                if let Err(e) = ii2.upsert_nodes(ipintel::legacy::health_nodes(h), now) {
                    errs.push(e);
                }
                // 出口 IP 变化不再作为异常（2026-09-24：节点只看连通性，IP 变化由喂数自动更新资产）：不再 ingest_egress
            }
            if let Some(a) = &a {
                match ii2.ingest_sources(ipintel::legacy::activity_events(a), now) {
                    Ok(r) => opened += r.anomalies_opened.len(),
                    Err(e) => errs.push(e),
                }
            }
            if t % 2 == 0 {
                if let Err(e) = ii2.maintain(now) {
                    errs.push(e);
                }
            }
            let obs = ii2.observations(now);
            (opened, errs, obs)
        })
        .await;
        if let Ok((opened, errs, obs)) = res {
            match obs {
                Ok(obs) => {
                    let obs: Vec<Observation> = obs
                        .into_iter()
                        .map(|o| Observation {
                            rule: o.rule,
                            labels: o.labels,
                            severity: Severity::parse(&o.severity).unwrap_or(Severity::Warning),
                            summary: o.summary,
                            evidence: o.evidence,
                            active_since: o.active_since,
                            for_s: o.for_s,
                            notify_resolved: o.notify_resolved,
                        })
                        .collect();
                    app.engine.lock().unwrap().evaluate(now, obs, &scope);
                }
                Err(e) => tracing::warn!(error = %e, "ipintel observations failed"),
            }
            let mut st = app.ipintel_state.lock().unwrap();
            st.loops += 1;
            st.last_loop_at = now;
            st.anomalies_opened += opened as u64;
            st.last_error = errs.first().cloned();
            if opened > 0 {
                app.hub.publish("ipintel", Some("/v2/api/v1/ipintel/anomalies".into()), now);
            }
        }
        if tick % 20 == 10 {
            let ii2 = ii.clone();
            if let Ok(Some(r)) = tokio::task::spawn_blocking(move || ii2.reload_if_changed()).await {
                tracing::info!(result = ?r, "ipintel databases reloaded");
            }
        }
        tick += 1;
        // 有人查看 30 s，否则 60 s（1x 约 2.7 条/s，/activity?limit=500 在 60 s 内仍是全量）
        let wait = if app.active() { 30 } else { 60 };
        tokio::time::sleep(Duration::from_secs(wait)).await;
    }
}

async fn poller(app: Arc<App>) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            _ = app.poll_wake.notified() => {}
        }
        let active = app.active();
        let mut core = false;
        for p in &app.polls {
            let Some(iv) = (if active { Some(p.active_s) } else { p.idle_s }) else { continue };
            let now = app.clock.now();
            {
                let mut st = p.st.lock().unwrap();
                if now - st.last_attempt < iv as f64 - 0.5 {
                    continue;
                }
                st.last_attempt = now;
            }
            let r = match p.source {
                Source::Legacy => app.legacy.refresh(&p.key).await,
                Source::Komari => app.komari.legacy_shape(&p.key, None, -1.0).await.0,
            };
            let now = app.clock.now();
            let (publish, changed_version) = {
                let mut st = p.st.lock().unwrap();
                st.polls += 1;
                match &r {
                    Ok(e) if e.status == 200 => {
                        st.last_ok = now;
                        st.consecutive_errors = 0;
                        st.last_latency_ms = e.latency_ms;
                        st.last_bytes = e.body.len();
                        if st.hash != Some(e.hash) {
                            // 第一次取到（之前没有哈希）不算“变化”：否则第一个 SSE 客户端连上时所有资源都会推一遍
                            let first = st.hash.is_none();
                            st.hash = Some(e.hash);
                            st.version += 1;
                            (!first, true)
                        } else {
                            (false, false)
                        }
                    }
                    Ok(e) => {
                        st.errors += 1;
                        st.consecutive_errors += 1;
                        st.last_error = Some(format!("HTTP {}", e.status));
                        (false, false)
                    }
                    Err(err) => {
                        st.errors += 1;
                        st.consecutive_errors += 1;
                        st.last_error = Some(err.clone());
                        (false, false)
                    }
                }
            };
            let ver = if publish { Some(app.hub.publish(p.name, Some(format!("/v2/api/legacy{}", p.key)), now).version) } else { None };
            if changed_version && (p.name == "overview" || p.name == "health") {
                // 增量接口的快照：按 SSE 版本号存（首次取到时用当前版本）
                if let Ok(e) = &r {
                    if let Ok(mut v) = serde_json::from_slice::<Value>(&e.body) {
                        let data = v.get_mut("data").map(Value::take).unwrap_or(Value::Null);
                        if !data.is_null() {
                            app.snapshots.put(p.name, ver.unwrap_or_else(|| app.hub.version(p.name)), data);
                        }
                    }
                }
            }
            if changed_version {
                if p.name == "overview" || p.name == "health" {
                    core = true;
                }
            }
        }
        let since_eval = app.clock.now() - app.eval.lock().unwrap().last_at;
        let stale_eval = since_eval > app.cfg.poll.idle_overview_s as f64;
        // 告警评估（要解析整份 overview / health）最多 30 s 一次：界面刷新再快，告警判定也不必跟着
        if (core && since_eval >= 30.0) || stale_eval {
            let max_age = 3.0 * app.cfg.poll.idle_overview_s as f64 + 15.0;
            let ov = app.cached_data("/overview", max_age);
            if core {
                if let Some(ov) = &ov {
                    record_probes(&app, ov).await;
                }
            }
            evaluate_rules(&app, ov);
            let st = app.native.lock().unwrap().persist_state().to_string();
            let _ = app.store.call(move |c| store::meta_set(c, "native_coverage", &st)).await;
        }
    }
}

/// 探测历史：每次 overview 变化时记下各节点的最新探测结果（按 measured_at 去重）。
pub async fn record_probes(app: &App, ov: &Value) {
    let mut samples = crate::probes::extract(ov);
    // 线路机自身（relayd → probe_land）：取上次之后的新行
    let since = { *app.relay_probe_cursor.lock().unwrap() }.max(app.clock.now() - 1800.0);
    let rs = crate::probes::relay_samples(&app.ingest, ov, since);
    if let Some(t) = rs.iter().map(|s| s.ts).reduce(f64::max) {
        *app.relay_probe_cursor.lock().unwrap() = t;
    }
    samples.extend(rs);
    if samples.is_empty() {
        return;
    }
    match app.store.call(move |c| crate::probes::record(c, &samples)).await {
        Ok(n) if n > 0 => {
            app.probes_gen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            app.hub.publish("probes", Some("/v2/api/v1/probes/latest".into()), app.clock.now());
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "record probes failed"),
    }
}

/// overview + health → 规则 → 引擎。数据过旧（两者任一超过 3 个周期）时跳过，告警保持原状。
pub fn evaluate_rules(app: &App, ov: Option<Value>) {
    let _span = crate::cpu::Span::new("evaluate_rules");
    let t0 = std::time::Instant::now();
    let max_age = 3.0 * app.cfg.poll.idle_overview_s as f64 + 15.0;
    let (Some(ov), Some(h)) = (ov, app.cached_data("/health", max_age)) else {
        let mut e = app.eval.lock().unwrap();
        e.skipped = Some("overview/health 不可用或过旧".into());
        e.last_at = app.clock.now();
        return;
    };
    if app.cfg.legacy_source != "native" {
        // 旧系统下线后 /health 本身就是原生移植版算的，不再需要这层影子比较
        health_shadow_step(app, &ov, &h);
    }
    let now = app.clock.now();
    // 启动宽限（alerts.startup_grace_s）：刚启动、数据还不是启动后产生的 → 「还没数据」类规则不开也不恢复
    let startup = rules::startup_hold(app.started_at, *app.first_ingest_at.lock().unwrap(), app.cfg.legacy_source == "native", &ov, &h, now,
        app.cfg.alerts.startup_grace_s as f64, app.cfg.alerts.startup_grace_max_s as f64);
    // 原生（relayd / nodeagent）数据：逐线路机的判定来源切换、relay_xray_down、原生主机告警
    let (native_h, extra) = {
        let first = !app.native.lock().unwrap().loaded_once();
        let after = app.native.lock().unwrap().last_land();
        let snap = crate::native::read_snapshot(&app.ingest, after, first, now);
        let nh = app.native.lock().unwrap().step(&app.cfg.edge, &snap, &ov, &h, now);
        let mut extra = crate::native::relay_xray_observations(&app.cfg.edge, &snap, nh.as_ref().unwrap_or(&h), now);
        let hold = startup || now - app.started_at < app.cfg.edge.startup_hold_s as f64;
        extra.extend(crate::native::host_observations(&app.ingest, &app.cfg.edge, now, hold));
        let relay_uuids: Vec<(String, String)> = ov["assets"].as_array().into_iter().flatten()
            .filter(|a| a["team"] == "线路机").filter_map(|a| Some((a["uuid"].as_str()?.to_string(), a["name"].as_str()?.to_string()))).collect();
        extra.extend(crate::native::relay_self_observations(&app.ingest, &app.cfg.edge, &relay_uuids, now));
        extra.extend(crate::native::egress_leak_observations(&ov, now));
        (nh, (extra, hold))
    };
    let (extra, hold) = extra;
    let h = native_h.unwrap_or(h);
    let mut d = {
        let plans = app.plans.read().unwrap();
        rules::derive(&ov, &h, &plans, &app.cfg.alerts, now)
    };
    if startup {
        d.hold_startup();
    }
    d.obs.extend(extra);
    for r in crate::native::NATIVE_RULES {
        if !(hold && *r == "native_host_offline") {
            d.scope.insert(r.to_string());
        }
    }
    let n = d.obs.len();
    app.engine.lock().unwrap().evaluate_held(now, d.obs, &d.scope, &d.hold);
    let mut e = app.eval.lock().unwrap();
    e.last_at = now;
    e.observations = n;
    e.duration_ms = t0.elapsed().as_secs_f64() * 1000.0;
    e.skipped = None;
    e.evaluations += 1;
    e.startup_hold = startup;
}

/// 数据源自身的告警：/overview 取不到或过旧（原生模式 = 原生旧库渲染；旧模式 = 旧后端），或 Komari 不可达。
pub fn source_observations(app: &App, now: f64) -> Vec<Observation> {
    let mut out = vec![];
    let mut push = |cause: &str, summary: String, ev: Value| {
        let mut l = crate::alerts::Labels::new();
        l.insert("kind".into(), "system".into());
        l.insert("cause".into(), cause.into());
        out.push(Observation {
            rule: "legacy_source_down".into(),
            labels: l,
            severity: Severity::Critical,
            summary,
            evidence: ev,
            active_since: None,
            for_s: 60.0,
            notify_resolved: true,
        });
    };
    if let Some(p) = app.poll_res("/overview") {
        let st = p.st.lock().unwrap().clone();
        let iv = p.idle_s.unwrap_or(60) as f64;
        let stale = if st.last_ok > 0.0 { now - st.last_ok > 3.0 * iv + 30.0 } else { now - app.started_at > 3.0 * iv + 30.0 };
        if st.consecutive_errors >= 3 || stale {
            let what = if app.cfg.legacy_source == "native" { "fleetd 渲染不出 /overview（原生旧库）".to_string() } else { format!("fleetd 读不到旧后端（{}）", app.cfg.legacy.base_url) };
            push("legacy_unreachable", format!("{what}：{}", st.last_error.clone().unwrap_or_else(|| "数据过旧".into())),
                json!({"consecutive_errors": st.consecutive_errors, "last_ok": st.last_ok, "last_error": st.last_error}));
        }
    }
    let k = app.komari_health.lock().unwrap().clone();
    if app.cfg.komari.enabled && k.consecutive_failures >= 3 {
        push("komari_unreachable", format!("fleetd 连不上 Komari（{}）：{}", app.cfg.komari.base_url, k.last_error.clone().unwrap_or_default()),
            json!({"consecutive_failures": k.consecutive_failures, "last_ok": k.last_ok, "last_error": k.last_error}));
    }
    out
}

async fn alert_clock(app: Arc<App>) {
    let src_scope: HashSet<String> = ["legacy_source_down".to_string()].into();
    let ch_scope: HashSet<String> = ["channel_failing".to_string()].into();
    let mut last_changes = u64::MAX;
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let now = app.clock.now();
        let src = source_observations(&app, now);
        let chf = app.notifier.failing_observations();
        let (outs, dirty, groups, digest, changes) = {
            let mut e = app.engine.lock().unwrap();
            e.evaluate(now, src, &src_scope);
            e.evaluate(now, chf, &ch_scope);
            let before = e.last_digest_at;
            let outs = e.tick(now);
            let (d, g) = e.take_dirty();
            let digest = if e.last_digest_at != before { Some(e.last_digest_at) } else { None };
            (outs, d, g, digest, e.changes)
        };
        if !dirty.is_empty() || !groups.is_empty() || digest.is_some() {
            if let Err(err) = app.store.call(move |c| persist_dirty(c, &dirty, &groups, digest)).await {
                tracing::error!(error = %err, "persist alert state failed");
            }
        }
        for out in outs {
            tracing::info!(kind = %out.kind, severity = out.severity.as_str(), title = %out.title, shadow = app.notifier.is_shadow(), "notification");
            let rows = app.notifier.deliver(&out, now).await;
            let _ = app
                .store
                .call(move |c| {
                    for r in &rows {
                        store::notify_insert(c, r)?;
                    }
                    Ok::<_, rusqlite::Error>(())
                })
                .await;
            app.hub.publish("notify", Some("/v2/api/v1/notify-log".into()), now);
        }
        if changes != last_changes {
            if last_changes != u64::MAX {
                app.hub.publish("alerts", Some("/v2/api/v1/alerts".into()), now);
            }
            last_changes = changes;
        }
    }
}

async fn housekeeping(app: Arc<App>) {
    let mut n: u64 = 0;
    loop {
        {
            app.sys.lock().unwrap().sample();
        }
        {
            // relayd 取期望状态的时间：落盘，fleetd 重启后管理模式立即可用（relayd 失败后会退避，不一定 10 s 内再来）
            let polls: std::collections::HashMap<String, f64> = app.relay_polls.lock().unwrap().clone();
            let v = serde_json::to_string(&polls).unwrap_or_default();
            let _ = app.store.call(move |c| store::meta_set(c, "relay_polls", &v)).await;
        }
        if n % 6 == 0 && app.cfg.komari.enabled {
            // Komari 可达性（未登录的 /api/me，开销极小）
            let r = app.komari.me(None).await;
            let now = app.clock.now();
            let mut k = app.komari_health.lock().unwrap();
            match r {
                Ok(_) => {
                    k.consecutive_failures = 0;
                    k.last_ok = now;
                }
                Err(e) => {
                    k.consecutive_failures += 1;
                    k.last_error = Some(e);
                }
            }
        }
        {
            let now = app.clock.now();
            if let Ok(k) = app.store.call(move |c| crate::relayspec::expire_ops(c, now)).await {
                if k > 0 {
                    app.hub.publish("ops", Some("/v2/api/v1/ops".into()), now);
                }
            }
        }
        if n % 360 == 0 {
            let now = app.clock.now();
            match app.store.call(move |c| store::retention(c, now)).await {
                Ok((a, nl, au)) if a + nl + au > 0 => tracing::info!(alerts = a, notify_log = nl, audit = au, "retention"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "retention failed"),
            }
            match app.store.call(move |c| crate::probes::retention(c, now)).await {
                Ok(n) if n > 0 => tracing::info!(rows = n, "probe retention"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "probe retention failed"),
            }
            app.legacy.cache.purge_older_than(now - 900.0);
            app.komari.cache.purge_older_than(now - 900.0);
        }
        n += 1;
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

async fn deadman(app: Arc<App>, url: String) {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap();
    loop {
        let ok = client.get(&url).send().await.map(|r| r.status().is_success()).unwrap_or(false);
        if !ok {
            tracing::warn!("healthchecks ping failed");
        }
        tokio::time::sleep(Duration::from_secs(app.cfg.notify.healthchecks_interval_s.max(30))).await;
    }
}

pub fn get_alert_row(c: &Connection, id: i64) -> Option<Alert> {
    c.query_row("SELECT * FROM alerts WHERE id=?", [id], alert_from_row).optional().ok().flatten()
}

/// 上架批次有新事件时推 SSE（资源 onboard，path 指向该批次），页面不用轮询
async fn onboard_events(app: Arc<App>) {
    let mut last: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut first = true;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let Some(ob) = &app.onboard else { return };
        let db = ob.db.clone();
        let Ok(Ok(heads)) = tokio::task::spawn_blocking(move || db.event_heads()).await else { continue };
        let now = app.clock.now();
        for (batch, id) in heads {
            let prev = last.insert(batch.clone(), id);
            if !first && prev != Some(id) {
                app.hub.publish("onboard", Some(format!("/v2/api/v1/onboard/batches/{batch}")), now);
            }
        }
        first = false;
    }
}

/// 上架 P11 的令牌后端：签发/撤销走 fleetd-ingest，核对（令牌是否有效、第一批是否已接收）只读同一个库。
struct OnboardAgentTokens(fleetd_ingest::Shared);

impl onboard::agent::AgentTokens for OnboardAgentTokens {
    fn issue(&self, host_id: &str) -> Result<String, String> {
        self.0.issue_token(host_id, onboard::agent::TOKEN_NAME)
    }
    fn revoke(&self, host_id: &str) -> Result<usize, String> {
        self.0.revoke_tokens(host_id, None)
    }
    fn view(&self, host_id: &str) -> Result<Option<onboard::agent::HostView>, String> {
        self.0.with_db(|c| onboard::agent::HostView::query(c, host_id)).map(Some)
    }
    fn describe(&self) -> String {
        "fleetd 进程内 fleetd-ingest".into()
    }
}
