//! saga 引擎：批次租约（同一时刻只有一个执行者，崩溃后租约过期即可接管）→ 按行推进步骤（并发上限、金丝雀先行）
//! → 就绪的行按线路机合批（窗口 / 凑满 / 同线路机无人在路上）→ 一次零重启接入 → 各行继续验收与上线。
//! 所有状态都在 SQLite：进程随时可以被杀，resume 从库里的状态继续；每一步都先观察后动作，所以重跑是安全的。

use crate::agent::{self, AgentBin, AgentTokens};
use crate::config::Config;
use crate::db::{now_ms, Db, Row};
use crate::model::{cell, next_step, row as rs, step_index, step_name, ErrClass, Outcome};
use crate::relay::{self, MemberResult};
use crate::steps::{prepare_ssserver, Cx, SsBin};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone)]
pub struct Engine {
    pub cfg: Arc<Config>,
    pub db: Db,
    pub owner: String,
    ss: Arc<Mutex<Option<Arc<SsBin>>>>,
    /// P11 的 nodeagent 安装包（配了 [agent] 才有）
    edge: Arc<Mutex<Option<Arc<AgentBin>>>>,
    /// P11 的令牌后端：fleetd 注入（with_agent_tokens）优先，否则按 [agent].token_cmd
    tokens: Option<Arc<dyn AgentTokens>>,
    askpass: PathBuf,
    /// 演练用崩溃注入：FLEET_ONB_CRASH="P13"（线路机批次 API 已加、未落盘时崩）或 "P9@团队9-03"（该行该步做完、未记账时崩）。
    crash: Option<String>,
}

pub fn log(msg: &str) {
    let t = chrono_like();
    eprintln!("{t} {msg}");
}
fn chrono_like() -> String {
    let ms = now_ms();
    let s = (ms / 1000) % 86400;
    format!("{:02}:{:02}:{:02}.{:03}Z", s / 3600, (s / 60) % 60, s % 60, ms % 1000)
}

impl Engine {
    pub fn new(cfg: Arc<Config>, db: Db) -> Result<Engine, String> {
        std::fs::create_dir_all(&cfg.work_dir).map_err(|e| format!("建 {}：{e}", cfg.work_dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&cfg.work_dir, std::fs::Permissions::from_mode(0o700));
        }
        let askpass = crate::ssh::ensure_askpass(&cfg.work_dir).map_err(|e| e.to_string())?;
        let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim().to_string();
        let tokens = agent::from_config(cfg.agent.as_ref());
        Ok(Engine {
            cfg,
            db,
            owner: format!("{}@{}#{}", std::process::id(), host, &crate::steps::uuid_v4()[..8]),
            ss: Arc::new(Mutex::new(None)),
            edge: Arc::new(Mutex::new(None)),
            tokens,
            askpass,
            crash: std::env::var("FLEET_ONB_CRASH").ok().filter(|s| !s.is_empty()),
        })
    }

    /// fleetd 注入自己的 fleetd-ingest 作为令牌后端（None = 保持按配置得出的后端）。
    pub fn with_agent_tokens(mut self, t: Option<Arc<dyn AgentTokens>>) -> Engine {
        if t.is_some() {
            self.tokens = t;
        }
        self
    }

    /// 配了 [agent] 时准备并缓存 nodeagent 安装包（按 SHA256SUMS 校验）；没配返回 None。
    fn edgebin(&self) -> Result<Option<Arc<AgentBin>>, String> {
        let Some(a) = self.cfg.agent.as_ref() else { return Ok(None) };
        let mut g = self.edge.lock().unwrap();
        if g.is_none() {
            *g = Some(Arc::new(agent::prepare(a)?));
        }
        Ok(g.clone())
    }

    fn ssbin(&self) -> Result<Arc<SsBin>, String> {
        let mut g = self.ss.lock().unwrap();
        if g.is_none() {
            *g = Some(Arc::new(prepare_ssserver(&self.cfg)?));
        }
        Ok(g.clone().unwrap())
    }

    fn cx(&self, row: Row) -> Result<Cx, String> {
        let rc = self.cfg.relay(&row.relay).ok_or_else(|| format!("线路机 {} 不在配置里", row.relay))?.clone();
        // 安装包有问题时 edge 为 None：P11 会自己再准备一次并把原因记在格子上（补偿不需要安装包）
        Ok(Cx { cfg: self.cfg.clone(), db: self.db.clone(), row, rc, askpass: self.askpass.clone(), ss: self.ssbin()?, edge: self.edgebin().ok().flatten(), tokens: self.tokens.clone() })
    }

    fn set_row(&self, r: &Row, state: &str, step: &str, reason: Option<&str>) {
        let _ = self.db.set_row(&r.id, state, step, reason);
        let _ = self.db.event(&r.batch_id, Some(&r.id), "node.state", json!({"name": r.spec.short_name, "state": state, "current_step": step, "reason": reason}));
    }

    fn crash_point(&self, step: &str, row: &Row) {
        if let Some(c) = &self.crash {
            if c == &format!("{step}@{}", row.spec.short_name) {
                log(&format!("!! 崩溃注入：{} 的 {step} 已执行、未记账，进程立即退出", row.spec.short_name));
                std::process::abort();
            }
        }
    }

    // ── 一行：从 current_step 往后跑，直到需要合批、停下等人、或上线 ─────────────
    async fn run_row(&self, row_id: String) {
        loop {
            let Ok(Some(row)) = self.db.row(&row_id) else { return };
            if !rs::runnable(&row.state) {
                return;
            }
            // 取消请求：不再开始新步骤（手上的步骤已经做完），交回批次循环补偿
            let reqs = self.db.cancel_requests(&row.batch_id).unwrap_or_default();
            if reqs.iter().any(|x| x == "*" || *x == row.spec.short_name || *x == row.spec.name) {
                return;
            }
            let step = row.current_step.clone();
            if step == "P13" {
                self.set_row(&row, rs::READY_FOR_RELAY, "P13", Some("等待线路机合批"));
                log(&format!("{} 就绪，等 {} 合批", row.spec.short_name, row.relay));
                return;
            }
            let st = self.db.step(&row.id, &step).ok().flatten();
            if st.as_ref().is_some_and(|s| s.status == cell::SUCCEEDED || s.status == cell::SKIPPED) {
                self.advance(&row, &step);
                continue;
            }
            if row.state != rs::RUNNING {
                self.set_row(&row, rs::RUNNING, &step, None);
            }
            let cx = match self.cx(row.clone()) {
                Ok(c) => c,
                Err(e) => {
                    let _ = self.db.step_finish(&row.id, &step, cell::FAILED, None, Some("bug"), Some(&e));
                    self.set_row(&row, rs::FAILED, &step, Some(&e));
                    return;
                }
            };
            let mut attempt_no = 0u32;
            let outcome = loop {
                let attempt = self.db.step_start(&row.id, &step).unwrap_or(0);
                let _ = self.db.event(&row.batch_id, Some(&row.id), "step.started", json!({"step": step, "attempt": attempt}));
                let t0 = now_ms();
                let o = cx.exec(&step).await;
                let retryable = matches!(&o, Outcome::Fail(ErrClass::Transient | ErrClass::Relay, _));
                if retryable && attempt_no + 1 < self.cfg.retry_max {
                    attempt_no += 1;
                    let wait = self.cfg.retry_base_ms * (1u64 << (attempt_no - 1).min(6));
                    if let Outcome::Fail(c, m) = &o {
                        let d = format!("重试 {}/{}：{m}", attempt_no, self.cfg.retry_max - 1);
                        let _ = self.db.step_finish(&row.id, &step, cell::RUNNING, None, Some(c.as_str()), Some(&d));
                        log(&format!("{} {step} {}（{} ms 后重试）", row.spec.short_name, d, wait));
                    }
                    tokio::time::sleep(Duration::from_millis(wait)).await;
                    continue;
                }
                log(&format!("{} {step} {} → {}（{} ms）", row.spec.short_name, step_name(&step), brief(&o), now_ms() - t0));
                break o;
            };
            self.crash_point(&step, &row);
            let (status, out, class, detail): (&str, Option<Value>, Option<&str>, Option<String>) = match &outcome {
                Outcome::Done(v) => (cell::SUCCEEDED, Some(v.clone()), None, v.get("note").and_then(|x| x.as_str()).map(String::from)),
                Outcome::Skipped(m) => (cell::SKIPPED, None, None, Some(m.clone())),
                Outcome::WaitInput(m) => (cell::WAITING_INPUT, None, Some("input"), Some(m.clone())),
                Outcome::Blocked(c, m) => (cell::BLOCKED, None, Some(c.as_str()), Some(m.clone())),
                Outcome::Fail(c, m) => (cell::FAILED, None, Some(c.as_str()), Some(m.clone())),
            };
            let _ = self.db.step_finish(&row.id, &step, status, out.as_ref(), class, detail.as_deref());
            let _ = self.db.event(&row.batch_id, Some(&row.id), "step.finished", json!({"step": step, "status": status, "error_class": class, "detail": detail}));
            match outcome {
                Outcome::Done(_) | Outcome::Skipped(_) => self.advance(&row, &step),
                Outcome::WaitInput(m) => return self.set_row(&row, rs::WAITING_INPUT, &step, Some(&m)),
                Outcome::Blocked(_, m) => return self.set_row(&row, rs::BLOCKED, &step, Some(&m)),
                Outcome::Fail(_, m) => return self.set_row(&row, rs::FAILED, &step, Some(&m)),
            }
        }
    }

    fn advance(&self, row: &Row, step: &str) {
        match next_step(step) {
            Some(n) => {
                let _ = self.db.set_row(&row.id, rs::RUNNING, n, None);
            }
            None => {
                self.set_row(row, rs::ACTIVE, step, None);
                log(&format!("✓ {} 已上线", row.spec.short_name));
            }
        }
    }

    // ── 线路机批次 ──────────────────────────────────────────────────────────
    async fn run_relay_batch(&self, batch_id: String, relay_name: String, members: Vec<Row>) {
        let Some(rc) = self.cfg.relay(&relay_name).cloned() else { return };
        let names: Vec<String> = members.iter().map(|r| r.spec.short_name.clone()).collect();
        let (rb_id, no) = match self.db.relay_batch_create(&batch_id, &relay_name, &names) {
            Ok(x) => x,
            Err(e) => return log(&format!("建线路机批次失败：{e}")),
        };
        let _ = self.db.event(&batch_id, None, "relay_batch.updated", json!({"relay": relay_name, "batch_no": no, "status": "merging", "members": names}));
        for r in &members {
            self.set_row(r, rs::IN_RELAY_BATCH, "P13", Some(&format!("{relay_name} 第 {no} 批合并接入中")));
            let _ = self.db.step_start(&r.id, "P13");
        }
        let mut entries = vec![];
        let mut bad = vec![];
        for r in &members {
            match self.cx(r.clone()).and_then(|c| c.entry().map_err(|e| e.msg)) {
                Ok(e) => entries.push(e),
                Err(e) => bad.push((r.clone(), e)),
            }
        }
        for (r, e) in bad {
            let _ = self.db.step_finish(&r.id, "P13", cell::FAILED, None, Some("bug"), Some(&e));
            self.set_row(&r, rs::FAILED, "P13", Some(&e));
        }
        let remote = relay::relay_remote(&rc, &self.cfg.ssh_bin);
        let crash = self.crash.as_deref() == Some("P13");
        log(&format!("{relay_name} 第 {no} 批：{} 台合并接入（{}）", entries.len(), names.join("、")));
        let mut result = relay::attach_batch(&remote, &rc, &entries, crash).await;
        if let Err(e) = &result {
            if e.msg.starts_with("CRASH_INJECTED") {
                log(&format!("!! 崩溃注入：{}，进程立即退出", e.msg));
                std::process::abort();
            }
        }
        // 整批失败：逐台单独接入，把坏的那台隔离出来，好的照常上线
        // (行, 结果, 是否冲突, 报告)
        let mut singles: Vec<(String, Result<(), String>, bool, Option<Value>)> = vec![];
        if let (Err(e), true) = (&result, entries.len() > 1) {
            log(&format!("{relay_name} 第 {no} 批整体失败（{}），改为逐台接入以隔离问题节点", e.msg));
            for en in &entries {
                match relay::attach_batch(&remote, &rc, std::slice::from_ref(en), false).await {
                    Ok(rep) => {
                        match rep.members.first() {
                            Some((_, MemberResult::Conflict(m))) => singles.push((en.row_id.clone(), Err(m.clone()), true, Some(rep.report))),
                            _ => singles.push((en.row_id.clone(), Ok(()), false, Some(rep.report))),
                        }
                    }
                    Err(e) => singles.push((en.row_id.clone(), Err(e.msg), false, None)),
                }
            }
            result = Err(e.clone());
        }
        let mut done = vec![];
        let mut report = json!({});
        let mut apply = |row_id: &str, ok: Result<(), String>, conflict: bool, rep: Option<&Value>| {
            let Some(r) = members.iter().find(|m| m.id == row_id) else { return };
            match ok {
                Ok(()) => {
                    let _ = self.db.step_finish(&r.id, "P13", cell::SUCCEEDED, Some(&json!({"relay": relay_name, "relay_batch": rb_id, "report": rep})), None, None);
                    let _ = self.db.set_row(&r.id, rs::RUNNING, "P14", None);
                    done.push(r.spec.short_name.clone());
                }
                Err(m) if conflict && m.contains("端口") => {
                    // 端口被别人占了：作废预留，从登记重新分配（不影响分享链接，因为还没交付）
                    let _ = self.db.drop_allocation(&r.id);
                    for s in ["P10", "P12", "P13"] {
                        let _ = self.db.step_set(&r.id, s, cell::QUEUED, Some(&format!("端口冲突，重新分配：{m}")));
                    }
                    let _ = self.db.set_row(&r.id, rs::RUNNING, "P10", Some(&m));
                }
                Err(m) => {
                    let st = if conflict { cell::BLOCKED } else { cell::FAILED };
                    let _ = self.db.step_finish(&r.id, "P13", st, None, Some(if conflict { "conflict" } else { "relay" }), Some(&m));
                    self.set_row(r, if conflict { rs::BLOCKED } else { rs::FAILED }, "P13", Some(&m));
                }
            }
        };
        match &result {
            Ok(rep) => {
                report = rep.report.clone();
                for (rid, mr) in &rep.members {
                    match mr {
                        MemberResult::Attached => apply(rid, Ok(()), false, Some(&rep.report)),
                        MemberResult::Conflict(m) => apply(rid, Err(m.clone()), true, None),
                    }
                }
            }
            Err(e) if singles.is_empty() => {
                for en in &entries {
                    apply(&en.row_id, Err(e.msg.clone()), false, None);
                }
                report = json!({"error": e.msg});
            }
            Err(e) => {
                report = json!({"error": e.msg, "isolated": singles.iter().map(|(id, r, c, _)| json!({"row": id, "ok": r.is_ok(), "conflict": c, "error": r.as_ref().err()})).collect::<Vec<_>>(),
                                "singles": singles.iter().filter_map(|s| s.3.clone()).collect::<Vec<_>>()});
                for (rid, r, conflict, rep) in singles {
                    apply(&rid, r, conflict, rep.as_ref());
                }
            }
        }
        let status = if done.len() == names.len() { "done" } else if done.is_empty() { "failed" } else { "partial" };
        let _ = self.db.relay_batch_finish(&rb_id, status, &names, &report, result.as_ref().err().map(|e| e.msg.as_str()));
        let _ = self.db.event(&batch_id, None, "relay_batch.updated", json!({"relay": relay_name, "batch_no": no, "status": status, "members": names, "attached": done,
            "xray_restarted": report["xray"]["restarted"]}));
        log(&format!("{relay_name} 第 {no} 批：{status}，接入 {} 台，Xray 重启={}", done.len(), report["xray"]["restarted"]));
    }

    // ── 批次调度 ────────────────────────────────────────────────────────────
    pub async fn run_batch(&self, batch_id: &str) -> Result<Value, String> {
        let b = self.db.batch(batch_id)?.ok_or("批次不存在")?;
        if b.status == "cancelled" {
            return Err("批次已取消".into());
        }
        let ttl = (self.cfg.lease_ttl_s * 1000) as i64;
        self.db.acquire_lease(batch_id, &self.owner, ttl, false)??;
        if let Err(e) = self.ssbin().and_then(|_| self.edgebin().map_err(|m| format!("nodeagent 安装包（[agent].dist）：{m}"))) {
            // 启动前置条件不满足：批次标 attention 并把原因记下来（不只写日志），界面才看得到
            let msg = format!("批次没能开始：{e}");
            let _ = self.db.set_batch_error(batch_id, Some(&msg));
            let _ = self.db.set_batch_status(batch_id, "attention");
            let _ = self.db.event(batch_id, None, "batch.updated", json!({"status": "attention", "error": msg}));
            let _ = self.db.release_lease(batch_id, &self.owner);
            return Err(msg);
        }
        let _ = self.db.set_batch_error(batch_id, None);
        // 上次崩溃留下的现场：线路机批次标中断；合批中的行退回就绪（接入幂等，重新组批即可）
        let n = self.db.relay_batches_interrupt(batch_id)?;
        for r in self.db.rows(batch_id)? {
            if r.state == rs::IN_RELAY_BATCH || r.state == rs::READY_FOR_RELAY {
                self.set_row(&r, rs::READY_FOR_RELAY, "P13", Some("恢复：重新组批"));
            } else if r.state == rs::RUNNING || r.state == rs::WAITING_GATE {
                // 正在跑的那一步会从 check 重新开始
                let _ = self.db.set_row(&r.id, rs::QUEUED, &r.current_step, None);
            }
        }
        if n > 0 {
            log(&format!("恢复：{n} 个线路机批次上次没跑完，成员重新组批（接入按 tag 幂等）"));
        }
        self.db.set_batch_status(batch_id, "running")?;
        let _ = self.db.event(batch_id, None, "batch.updated", json!({"status": "running", "owner": self.owner}));
        // 租约心跳
        let hb = {
            let (db, id, owner) = (self.db.clone(), batch_id.to_string(), self.owner.clone());
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis((ttl / 3) as u64)).await;
                    let _ = db.renew_lease(&id, &owner, ttl);
                }
            })
        };
        let opts = self.db.batch(batch_id)?.unwrap().options;
        let concurrency = opts["concurrency"].as_u64().map(|x| x as usize).unwrap_or(self.cfg.concurrency).max(1);
        let canary_on = opts["canary"].as_bool().unwrap_or(self.cfg.canary);
        let window_ms = (opts["relay_window_s"].as_u64().unwrap_or(self.cfg.relay_window_s) * 1000) as i64;
        let batch_max = self.cfg.relay_batch_max;
        let inflight: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let relay_busy: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let mut handles = vec![];
        let mut cancel_all = false;
        loop {
            // 运行中收到的取消：整批取消就不再启动新步骤/新合批；单行取消等它手上的步骤结束后就地补偿
            let reqs = self.db.cancel_requests(batch_id)?;
            if reqs.iter().any(|x| x == "*") && !cancel_all {
                cancel_all = true;
                let _ = self.db.event(batch_id, None, "batch.updated", json!({"status": "cancelling"}));
            }
            for name in reqs.iter().filter(|x| *x != "*") {
                let busy = inflight.lock().unwrap().clone();
                let rows = self.db.rows(batch_id)?;
                let target = rows.iter().find(|r| &r.spec.short_name == name || &r.spec.name == name);
                let in_relay = target.is_some_and(|r| r.state == rs::IN_RELAY_BATCH);
                if target.is_some_and(|r| busy.contains(&r.id)) || in_relay {
                    continue;
                }
                let _ = self.cancel_rows(batch_id, Some(name.as_str())).await;
                let _ = self.db.take_cancel_request(batch_id, name);
            }
            if cancel_all {
                let any_inflight = !inflight.lock().unwrap().is_empty() || !relay_busy.lock().unwrap().is_empty();
                if !any_inflight {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
                continue;
            }
            let rows = self.db.rows(batch_id)?;
            let busy_rows = inflight.lock().unwrap().clone();
            // 金丝雀闸门
            let canary = rows.iter().find(|r| r.is_canary);
            let gate_open = !canary_on
                || canary.is_none()
                || canary.is_some_and(|c| {
                    c.state == rs::ACTIVE
                        || (self.cfg.canary_gate == "P12" && (matches!(c.state.as_str(), rs::READY_FOR_RELAY | rs::IN_RELAY_BATCH) || step_index(&c.current_step) > step_index("P12")))
                });
            let canary_stuck = canary.is_some_and(|c| rs::stopped(&c.state) && c.state != rs::ACTIVE);
            for r in rows.iter().filter(|r| rs::runnable(&r.state) && !busy_rows.contains(&r.id)) {
                if canary_on && !gate_open && !r.is_canary {
                    if r.state != rs::WAITING_GATE {
                        let c = canary.unwrap();
                        let why = if canary_stuck { format!("金丝雀 {} 停在 {}：处理后 resume，或 resume --no-canary", c.spec.short_name, c.current_step) } else { format!("等待金丝雀 {} 走完 {}", c.spec.short_name, self.cfg.canary_gate) };
                        self.set_row(r, rs::WAITING_GATE, &r.current_step, Some(&why));
                    }
                    continue;
                }
                if inflight.lock().unwrap().len() >= concurrency {
                    break;
                }
                inflight.lock().unwrap().insert(r.id.clone());
                let (me, id, inf) = (self.clone(), r.id.clone(), inflight.clone());
                handles.push(tokio::spawn(async move {
                    me.run_row(id.clone()).await;
                    inf.lock().unwrap().remove(&id);
                }));
            }
            // 线路机合批
            let now = now_ms();
            let mut by_relay: BTreeMap<String, Vec<Row>> = BTreeMap::new();
            for r in rows.iter().filter(|r| r.state == rs::READY_FOR_RELAY) {
                by_relay.entry(r.relay.clone()).or_default().push(r.clone());
            }
            for (relay_name, ready) in by_relay {
                if relay_busy.lock().unwrap().contains(&relay_name) {
                    continue;
                }
                // 还在路上的同线路机成员（排队/执行中、还没到 P13）；等闸门、等人、失败的不算，不拖住别人
                let coming = rows.iter().filter(|r| r.relay == relay_name && (r.state == rs::RUNNING || r.state == rs::QUEUED) && step_index(&r.current_step) < step_index("P13")).count();
                let oldest = ready.iter().filter_map(|r| r.ready_since).min().unwrap_or(now);
                let fire = coming == 0 || now - oldest >= window_ms || ready.len() >= batch_max;
                if !fire {
                    continue;
                }
                let members: Vec<Row> = ready.into_iter().take(batch_max).collect();
                relay_busy.lock().unwrap().insert(relay_name.clone());
                for m in &members {
                    let _ = self.db.set_row(&m.id, rs::IN_RELAY_BATCH, "P13", Some("合批中"));
                }
                let (me, bid, rn, busy) = (self.clone(), batch_id.to_string(), relay_name.clone(), relay_busy.clone());
                handles.push(tokio::spawn(async move {
                    me.run_relay_batch(bid, rn.clone(), members).await;
                    busy.lock().unwrap().remove(&rn);
                }));
            }
            let rows = self.db.rows(batch_id)?;
            let any_inflight = !inflight.lock().unwrap().is_empty() || !relay_busy.lock().unwrap().is_empty();
            let runnable = rows.iter().any(|r| (r.state == rs::QUEUED || r.state == rs::RUNNING) || (r.state == rs::WAITING_GATE && gate_open));
            let ready = rows.iter().any(|r| r.state == rs::READY_FOR_RELAY || r.state == rs::IN_RELAY_BATCH);
            if !any_inflight && !runnable && !ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        for h in handles {
            let _ = h.await;
        }
        if cancel_all {
            let out = self.cancel_rows(batch_id, None).await;
            let _ = self.db.take_cancel_request(batch_id, "*");
            hb.abort();
            self.db.release_lease(batch_id, &self.owner)?;
            let _ = self.db.event(batch_id, None, "batch.updated", json!({"status": "cancelled"}));
            return out.map(|v| json!({"status": "cancelled", "rows": v}));
        }
        hb.abort();
        let rows = self.db.rows(batch_id)?;
        let active = rows.iter().filter(|r| r.state == rs::ACTIVE).count();
        let cancelled = rows.iter().filter(|r| r.state == rs::CANCELLED).count();
        let status = if active + cancelled == rows.len() { "finished" } else { "attention" };
        self.db.set_batch_status(batch_id, status)?;
        self.db.release_lease(batch_id, &self.owner)?;
        let _ = self.db.event(batch_id, None, "batch.updated", json!({"status": status}));
        Ok(json!({"status": status, "active": active, "total": rows.len(),
            "attention": rows.iter().filter(|r| !matches!(r.state.as_str(), rs::ACTIVE | rs::CANCELLED)).map(|r| json!({"name": r.spec.short_name, "state": r.state, "step": r.current_step, "reason": r.reason})).collect::<Vec<_>>()}))
    }

    // ── 人工输入 / 重试 ─────────────────────────────────────────────────────
    /// resume --row：补密码、确认 DD 人工闸门、重试失败或风险停止的行（风险停止要 force）。
    pub fn provide(&self, batch_id: &str, row_name: &str, password_file: Option<String>, force: bool) -> Result<String, String> {
        let rows = self.db.rows(batch_id)?;
        let r = rows.iter().find(|r| r.spec.short_name == row_name || r.spec.name == row_name || r.id == row_name).ok_or(format!("批次里没有 {row_name}"))?;
        if !matches!(r.state.as_str(), rs::WAITING_INPUT | rs::BLOCKED | rs::FAILED | rs::WAITING_GATE) {
            return Err(format!("{row_name} 现在是 {}，不需要输入", r.state));
        }
        if r.state == rs::BLOCKED && !force {
            return Err(format!("{row_name} 是风险停止（{}）：确认已人工处置后加 --force", r.reason.clone().unwrap_or_default()));
        }
        let mut spec = r.spec.clone();
        if let Some(p) = password_file {
            let p = std::fs::canonicalize(&p).map_err(|e| format!("密码文件 {p}：{e}"))?;
            spec.password_file = Some(p.display().to_string());
            spec.auth = "password".into();
            self.db.set_row_spec(&r.id, &spec)?;
        }
        let step = r.current_step.clone();
        if step == "P4" {
            self.db.step_outputs_merge(&r.id, "P4", &json!({"ack": true, "ack_at": now_ms()}))?;
        }
        self.db.step_set(&r.id, &step, cell::QUEUED, None)?;
        self.db.set_row(&r.id, rs::QUEUED, &step, None)?;
        let _ = self.db.event(batch_id, Some(&r.id), "node.state", json!({"name": r.spec.short_name, "state": "queued", "current_step": step, "reason": "人工继续"}));
        Ok(format!("{} 将从 {} {} 继续", r.spec.short_name, step, step_name(&step)))
    }

    // ── 重新接入：把本批已上线的入口按原参数（同端口、同密钥、同 UUID）再加回线路机 ─────────
    /// 用于线路机配置被别的流程整份重建之后（例如 fleet-cutover 从 x-ui.db 重新生成规格）。接入按 tag 幂等：
    /// 已在的跳过，缺的补上并落盘，分享链接不变；同样零重启、同样过 diff 守卫。
    pub async fn reattach(&self, batch_id: &str) -> Result<Value, String> {
        let ttl = (self.cfg.lease_ttl_s * 1000) as i64;
        self.db.acquire_lease(batch_id, &self.owner, ttl, false)??;
        let mut by_relay: BTreeMap<String, Vec<Row>> = BTreeMap::new();
        for r in self.db.rows(batch_id)?.into_iter().filter(|r| r.state == rs::ACTIVE) {
            by_relay.entry(r.relay.clone()).or_default().push(r);
        }
        let mut out = vec![];
        for (relay_name, rows) in by_relay {
            let rc = self.cfg.relay(&relay_name).ok_or(format!("线路机 {relay_name} 不在配置里"))?.clone();
            let mut entries = vec![];
            for r in &rows {
                entries.push(self.cx(r.clone())?.entry().map_err(|e| e.msg)?);
            }
            let rep = relay::attach_batch(&relay::relay_remote(&rc, &self.cfg.ssh_bin), &rc, &entries, false).await;
            match rep {
                Ok(rep) => {
                    log(&format!("{relay_name}：重新接入 {} 个入口，新增 {:?}，已在 {}", entries.len(), rep.report["diff"]["new_inbounds"], rep.report["api_skipped_already_present"]));
                    out.push(json!({"relay": relay_name, "members": rows.iter().map(|r| r.spec.short_name.clone()).collect::<Vec<_>>(),
                        "conflicts": rep.members.iter().filter_map(|(id, m)| match m { MemberResult::Conflict(c) => Some(json!({"row": id, "error": c})), _ => None }).collect::<Vec<_>>(),
                        "report": rep.report}));
                }
                Err(e) => out.push(json!({"relay": relay_name, "error": e.msg})),
            }
        }
        self.db.release_lease(batch_id, &self.owner)?;
        Ok(json!(out))
    }

    // ── 取消 / 放弃：对已完成、可补偿的步骤逆序补偿 ────────────────────────────
    pub async fn cancel(&self, batch_id: &str, row_name: Option<&str>) -> Result<Value, String> {
        let ttl = (self.cfg.lease_ttl_s * 1000) as i64;
        self.db.acquire_lease(batch_id, &self.owner, ttl, false)??;
        let r = self.cancel_rows(batch_id, row_name).await;
        self.db.release_lease(batch_id, &self.owner)?;
        r
    }

    /// 补偿并标取消（调用方持有租约）
    async fn cancel_rows(&self, batch_id: &str, row_name: Option<&str>) -> Result<Value, String> {
        let mut out = vec![];
        for r in self.db.rows(batch_id)? {
            if let Some(n) = row_name {
                if r.spec.short_name != n && r.spec.name != n {
                    continue;
                }
            }
            if r.state == rs::ACTIVE {
                out.push(json!({"name": r.spec.short_name, "result": "已上线，不取消（走下线流程）"}));
                continue;
            }
            if r.state == rs::CANCELLED {
                continue;
            }
            let steps = self.db.steps(&r.id)?;
            // P11 只要真正执行过（失败/超时的也算：节点上可能已装上、令牌可能已签发）就要补偿；补偿本身幂等
            let undoable = |s: &crate::db::StepRec| {
                s.status == cell::SUCCEEDED
                    || (s.step == "P13" && s.status == cell::RUNNING)
                    || (s.step == "P10" && s.status != cell::QUEUED)
                    || (s.step == "P11" && s.attempt > 0 && !matches!(s.status.as_str(), cell::SKIPPED | cell::COMPENSATED))
            };
            // 没有需要补偿的步骤（例如批次从未开始）就不需要执行环境：直接标取消
            let cx = if steps.iter().any(undoable) { Some(self.cx(r.clone())?) } else { None };
            let mut notes = vec![];
            for s in steps.iter().rev() {
                if !undoable(s) {
                    continue;
                }
                let Some(cx) = cx.as_ref() else { continue };
                match cx.undo(&s.step).await {
                    Ok(m) if !m.is_empty() => {
                        notes.push(format!("{}：{m}", s.step));
                        let _ = self.db.step_set(&r.id, &s.step, cell::COMPENSATED, Some(&m));
                    }
                    Ok(_) => {}
                    Err(e) => notes.push(format!("{} 补偿失败：{}", s.step, e.msg)),
                }
            }
            for s in &steps {
                if s.status == cell::QUEUED || s.status == cell::WAITING_INPUT {
                    let _ = self.db.step_set(&r.id, &s.step, "cancelled", None);
                }
            }
            self.set_row(&r, rs::CANCELLED, &r.current_step, Some("已取消"));
            out.push(json!({"name": r.spec.short_name, "result": notes}));
        }
        if row_name.is_none() {
            self.db.set_batch_status(batch_id, "cancelled")?;
        }
        Ok(json!(out))
    }
}

fn brief(o: &Outcome) -> String {
    match o {
        Outcome::Done(_) => "成功".into(),
        Outcome::Skipped(m) => format!("跳过（{m}）"),
        Outcome::WaitInput(m) if m.starts_with("等待输入") => m.clone(),
        Outcome::WaitInput(m) => format!("等待输入：{m}"),
        Outcome::Blocked(c, m) => format!("风险停止[{}]：{m}", c.as_str()),
        Outcome::Fail(c, m) => format!("失败[{}]：{m}", c.as_str()),
    }
}
