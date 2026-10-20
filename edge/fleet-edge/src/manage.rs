//! relayd 管理模式（fleet.relayspec.v1，契约见 fleet/API.md §10 / §10.1）。
//!
//! - 观测：配置文件 sha256 变化时（每 60 s 查一次）和每小时，`POST /v2/ingest/relay/observed` 报受管对象
//!   （入站 `inbound-*`/`in-*`，出站 `ss-*`/`relay-*`/`socks-*`，规则 ruleTag `fleet-*`/`policy-*`）。
//! - 期望：每 10 s `GET /v2/ingest/relay/spec?have=<generation>&wait=0`（单线程，不长轮询）；304 = 无变化，404 = fleetd 未启用管理模式。
//! - 应用（全程持有 flock /run/lock/fleet-xray-config.lock，与 fleet-cutover / onboard 同一把锁）：
//!   读配置 → 按受管 tag 得出增 / 删 / 替换，规则按 §10.1 的锚点规则排好 → **守卫**（非受管对象、outbounds[0] 的 R1 默认 blackhole、
//!   其他配置段逐字不变；留下来的每个入口入站都必须有只按 inboundTag 匹配的路由，否则它的流量会落进 R1 blackhole）→ `xray run -test` →
//!   经 Xray API 逐项生效（出站增/换 → 入站删/换/增 → 规则整表替换 → 出站删），**不重启 Xray** → 用 lsi/lso/lsrules 核对运行态 →
//!   原子写 config.json（旧版本备份在状态目录）。任一步失败按相反顺序撤回，回执 rejected。
//! - 回执：`POST /v2/ingest/relay/spec/ack`，带逐对象结果、前后配置差异（diff）与前后运行态（runtime）；投递失败留在状态文件里重试。
//! - 命令：`restart_xray`（先对磁盘配置 -test，再 systemctl restart，等端口/API/规则就绪）、`reset_traffic`
//!   （GetStats reset=true 只作用于指定计数；relayd 的累计值先补采、不清零；结果 data.counters 作为账本备注）。
use crate::config::ManageCfg;
use crate::util::{now_f64, round2, write_atomic};
use crate::xray::Collector;
use crate::{error, info, warn};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

type R<T> = Result<T, String>;

// ---------- 锁与文件 ----------

pub struct Lock(std::fs::File);

pub fn lock(path: &str, wait: Duration) -> R<Lock> {
    if let Some(d) = Path::new(path).parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let f = std::fs::OpenOptions::new().append(true).create(true).open(path).map_err(|e| format!("打开锁 {path}：{e}"))?;
    let t0 = Instant::now();
    loop {
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Lock(f));
        }
        if t0.elapsed() > wait {
            return Err(format!("拿不到规格锁 {path}（另一个写者正在修改 Xray 配置）"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn read_json(path: &str) -> R<Value> {
    let b = std::fs::read(path).map_err(|e| format!("读 {path}：{e}"))?;
    serde_json::from_slice(&b).map_err(|e| format!("解析 {path}：{e}"))
}

/// 原子写（同目录临时文件 + fsync + rename + fsync 目录），权限 0600
pub fn write_json_durable(path: &str, v: &Value) -> R<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let p = Path::new(path);
    let tmp = p.with_extension("fleet-relayd.tmp");
    let mut data = serde_json::to_vec_pretty(v).map_err(|e| e.to_string())?;
    data.push(b'\n');
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).map_err(|e| format!("写 {}：{e}", tmp.display()))?;
        f.write_all(&data).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, p).map_err(|e| format!("rename {}：{e}", p.display()))?;
    if let Some(d) = p.parent() {
        if let Ok(df) = std::fs::File::open(d) {
            let _ = df.sync_all();
        }
    }
    Ok(())
}



/// 本机正在监听的 TCP 端口（/proc/net/tcp{,6} 状态 0A）
pub fn listening_ports() -> BTreeSet<u16> {
    let mut s = BTreeSet::new();
    for p in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(t) = std::fs::read_to_string(p) {
            for l in t.lines().skip(1) {
                let f: Vec<&str> = l.split_whitespace().collect();
                if f.len() > 3 && f[3] == "0A" {
                    if let Some(port) = f[1].rsplit(':').next().and_then(|h| u16::from_str_radix(h, 16).ok()) {
                        s.insert(port);
                    }
                }
            }
        }
    }
    s
}


pub fn sha256_hex(b: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

// ---------- 受管对象 ----------

pub fn is_entry_tag(t: &str) -> bool {
    if let Some(d) = t.strip_prefix("inbound-") {
        return (2..=5).contains(&d.len()) && d.bytes().all(|c| c.is_ascii_digit());
    }
    t.strip_prefix("in-").is_some_and(|r| !r.is_empty())
}
fn managed_out(t: &str) -> bool {
    t.starts_with("ss-") || t.starts_with("relay-") || t.starts_with("socks-")
}
fn rule_tag(r: &Value) -> &str {
    r["ruleTag"].as_str().unwrap_or("")
}
fn managed_rule(r: &Value) -> bool {
    let t = rule_tag(r);
    t.starts_with("fleet-") || t.starts_with("policy-")
}
fn tag(v: &Value) -> &str {
    v["tag"].as_str().unwrap_or("")
}

pub fn managed_of(c: &Value) -> Value {
    let arr = |k: &str| c[k].as_array().cloned().unwrap_or_default();
    json!({
        "inbounds": arr("inbounds").into_iter().filter(|i| is_entry_tag(tag(i))).collect::<Vec<_>>(),
        "outbounds": arr("outbounds").into_iter().filter(|o| managed_out(tag(o))).collect::<Vec<_>>(),
        "routing_rules": c["routing"]["rules"].as_array().cloned().unwrap_or_default().into_iter().filter(managed_rule).collect::<Vec<_>>(),
    })
}

const MATCH_KEYS: &[&str] = &["domain", "ip", "source", "sourcePort", "port", "network", "protocol", "user", "attrs", "localPort", "localIP", "vlessRoute", "process"];

fn is_plain_route(r: &Value, t: &str) -> bool {
    r["inboundTag"].as_array().is_some_and(|a| a.iter().any(|x| x == t))
        && (r.get("outboundTag").is_some_and(|o| o.is_string()) || r.get("balancerTag").is_some_and(|o| o.is_string()))
        && MATCH_KEYS.iter().all(|k| r.get(*k).is_none_or(|v| v.is_null() || v == &json!([]) || v == &json!("")))
}

/// 由当前配置 + 期望受管集合生成候选配置（§10.1 的放置规则）
pub fn build_candidate(cur: &Value, want: &Value) -> R<Value> {
    let mut c = cur.clone();
    for (key, pred) in [("inbounds", is_entry_tag as fn(&str) -> bool), ("outbounds", managed_out as fn(&str) -> bool)] {
        let want_list: Vec<Value> = want[key].as_array().cloned().unwrap_or_default();
        for w in &want_list {
            if !pred(tag(w)) {
                return Err(format!("spec 的 {key} 含非受管 tag {:?}", tag(w)));
            }
        }
        // 受管对象按 spec 的顺序依次填进文件里原本属于受管对象的位置（非受管对象原位不动），
        // 多出来的（新增）追加在末尾：纯调序也会得到不同的候选，按 spec 的顺序改写文件
        let mut uniq: Vec<Value> = vec![];
        let mut seen = BTreeSet::new();
        for w in want_list {
            if seen.insert(tag(&w).to_string()) {
                uniq.push(w);
            }
        }
        let mut it = uniq.into_iter();
        let mut out = vec![];
        for o in cur[key].as_array().cloned().unwrap_or_default() {
            if !pred(tag(&o)) {
                out.push(o);
            } else if let Some(w) = it.next() {
                out.push(w);
            }
        }
        out.extend(it);
        c[key] = Value::Array(out);
    }
    let want_rules: Vec<Value> = want["routing_rules"].as_array().cloned().unwrap_or_default();
    for r in &want_rules {
        if !managed_rule(r) {
            return Err(format!("spec 的规则 ruleTag {:?} 不是 fleet-/policy- 前缀", rule_tag(r)));
        }
    }
    let unmanaged: Vec<Value> = cur["routing"]["rules"].as_array().cloned().unwrap_or_default().into_iter().filter(|r| !managed_rule(r)).collect();
    let pol: Vec<Value> = want_rules.iter().filter(|r| rule_tag(r).starts_with("policy-")).cloned().collect();
    let fl: Vec<Value> = want_rules.iter().filter(|r| rule_tag(r).starts_with("fleet-")).cloned().collect();
    let anchor = unmanaged
        .iter()
        .position(|r| r["inboundTag"].as_array().is_some_and(|a| a.iter().any(|t| t.as_str().is_some_and(is_entry_tag))))
        .unwrap_or(unmanaged.len());
    let mut rules = unmanaged[..anchor].to_vec();
    rules.extend(pol);
    rules.extend_from_slice(&unmanaged[anchor..]);
    rules.extend(fl);
    if !c["routing"].is_object() {
        c["routing"] = json!({});
    }
    c["routing"]["rules"] = Value::Array(rules);
    Ok(c)
}

/// 守卫 + 差异。返回 (diff, 结果)。
pub fn guard(before: &Value, after: &Value) -> (Value, R<()>) {
    let idx = |c: &Value, k: &str| -> Vec<(String, Value)> { c[k].as_array().into_iter().flatten().map(|x| (tag(x).to_string(), x.clone())).collect() };
    let (bi, ai) = (idx(before, "inbounds"), idx(after, "inbounds"));
    let (bo, ao) = (idx(before, "outbounds"), idx(after, "outbounds"));
    let classify = |b: &[(String, Value)], a: &[(String, Value)]| {
        let mut ch = vec![];
        let mut add = vec![];
        let mut rm = vec![];
        let mut same = 0;
        for (t, v) in b {
            match a.iter().find(|x| &x.0 == t) {
                Some((_, w)) if w == v => same += 1,
                Some(_) => ch.push(t.clone()),
                None => rm.push(t.clone()),
            }
        }
        for (t, _) in a {
            if !b.iter().any(|x| &x.0 == t) {
                add.push(t.clone());
            }
        }
        json!({"changed": ch, "added": add, "removed": rm, "identical": same})
    };
    let order = |v: &[(String, Value)]| -> Vec<String> { v.iter().map(|x| x.0.clone()).collect() };
    let tagset = |v: &[(String, Value)]| -> BTreeSet<String> { v.iter().map(|x| x.0.clone()).collect() };
    let um_in = |v: &[(String, Value)]| -> Vec<Value> { v.iter().filter(|x| !is_entry_tag(&x.0)).map(|x| x.1.clone()).collect() };
    let um_out = |v: &[(String, Value)]| -> Vec<Value> { v.iter().filter(|x| !managed_out(&x.0)).map(|x| x.1.clone()).collect() };
    let rules = |c: &Value| -> Vec<Value> { c["routing"]["rules"].as_array().cloned().unwrap_or_default() };
    let um_rules = |c: &Value| -> Vec<Value> { rules(c).into_iter().filter(|r| !managed_rule(r)).collect() };
    let mrules = |c: &Value, p: &str| -> Vec<Value> { rules(c).into_iter().filter(|r| rule_tag(r).starts_with(p)).collect() };
    let rdiff = |p: &str| {
        let (b, a) = (mrules(before, p), mrules(after, p));
        let tags = |v: &[Value]| -> Vec<String> { v.iter().map(|r| rule_tag(r).to_string()).collect() };
        json!({
            "added": tags(&a).into_iter().filter(|t| !tags(&b).contains(t)).collect::<Vec<_>>(),
            "removed": tags(&b).into_iter().filter(|t| !tags(&a).contains(t)).collect::<Vec<_>>(),
            "changed": a.iter().filter(|r| b.iter().any(|x| rule_tag(x) == rule_tag(r) && x != *r)).map(|r| rule_tag(r).to_string()).collect::<Vec<_>>(),
        })
    };
    let strip_routing = |c: &Value| {
        let mut r = c["routing"].clone();
        if let Some(o) = r.as_object_mut() {
            o.remove("rules");
        }
        r
    };
    let other: Vec<String> = {
        let mut keys: BTreeSet<String> = BTreeSet::new();
        for c in [before, after] {
            if let Some(o) = c.as_object() {
                keys.extend(o.keys().cloned());
            }
        }
        keys.into_iter().filter(|k| !["inbounds", "outbounds", "routing"].contains(&k.as_str()) && before[k.as_str()] != after[k.as_str()]).collect()
    };
    let r1_b = before["outbounds"][0].clone();
    let r1_a = after["outbounds"][0].clone();
    let unrouted: Vec<String> =
        ai.iter().filter(|(t, _)| is_entry_tag(t)).filter(|(t, _)| !rules(after).iter().any(|r| is_plain_route(r, t))).map(|x| x.0.clone()).collect();
    let diff = json!({
        "config_sha_before": crate::policy::digest(before)[..16].to_string(),
        "config_sha_after": crate::policy::digest(after)[..16].to_string(),
        "inbounds": classify(&bi, &ai), "outbounds": classify(&bo, &ao),
        "inbounds_order": {"reordered": order(&bi) != order(&ai) && tagset(&bi) == tagset(&ai), "before": order(&bi), "after": order(&ai)},
        "outbounds_order": {"reordered": order(&bo) != order(&ao) && tagset(&bo) == tagset(&ao)},
        "unmanaged_inbounds_identical": um_in(&bi) == um_in(&ai),
        "unmanaged_outbounds_identical": um_out(&bo) == um_out(&ao),
        "r1_default_outbound_identical": r1_b == r1_a, "r1_default_outbound": r1_a["tag"],
        "unmanaged_rules_identical": um_rules(before) == um_rules(after),
        "explicit_rules": rdiff("fleet-"), "policy_rules": rdiff("policy-"),
        "routing_other_identical": strip_routing(before) == strip_routing(after), "other_sections_changed": other,
        "entries_without_route": unrouted,
    });
    let mut p = vec![];
    if diff["unmanaged_inbounds_identical"] != true {
        p.push("非受管入站（api 等）变化了".to_string());
    }
    if diff["unmanaged_outbounds_identical"] != true {
        p.push("非受管出站（direct/blocked/R1 默认等）变化了".into());
    }
    if r1_b != r1_a {
        p.push("outbounds[0]（R1 默认出站）变化了".into());
    }
    if diff["unmanaged_rules_identical"] != true {
        p.push("非受管路由规则或其相对顺序变化了".into());
    }
    if diff["routing_other_identical"] != true || !other.is_empty() {
        p.push(format!("其他配置段变化了：{other:?}"));
    }
    if !unrouted.is_empty() {
        p.push(format!("这些入口没有只按 inboundTag 匹配的路由，流量会落进 R1 blackhole：{unrouted:?}"));
    }
    (diff, if p.is_empty() { Ok(()) } else { Err(p.join("；")) })
}

// ---------- 管理器 ----------

pub struct Manager {
    cfg: ManageCfg,
    host_id: String,
    url_spec: String,
    url_ack: String,
    url_observed: String,
    token_file: PathBuf,
    agent: ureq::Agent,
    state_path: PathBuf,
    backup_dir: PathBuf,
    pub generation: u64,
    pending_acks: Vec<Value>,
    rejected: Option<(String, String)>, // (spec_sha256, 当时的文件 sha)
    done_cmds: Vec<String>,
    pub last_ack: Option<Value>,
    last_poll: Option<Instant>,
    observed_sha: Option<String>,
    observed_at: Option<Instant>,
    obs_checked: Option<Instant>,
    pub disabled_by_server: bool,
    pub ready: Option<R<()>>,
    pub runtime_check: Option<Value>,
    last_check: Option<Instant>,
}

struct Step {
    object: &'static str,
    tag: String,
    op: &'static str,
    undo: Vec<Vec<String>>, // xray api 参数序列（撤回时执行）
}

impl Manager {
    pub fn new(cfg: ManageCfg, host_id: &str, endpoint: &str, token_file: &Path, state_dir: &Path, http_timeout_s: u64) -> Manager {
        let ep = endpoint.trim_end_matches('/');
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout(Duration::from_secs(http_timeout_s))
            // 每 10 s 一次 spec 轮询：复用 keep-alive 连接，省掉每次的 TLS 握手（连接被关掉时这次轮询失败，下个周期重连）
            .max_idle_connections(1)
            .max_idle_connections_per_host(1)
            .redirects(0)
            .user_agent(concat!("fleet-edge-relayd/", env!("CARGO_PKG_VERSION")))
            .build();
        let mut m = Manager {
            cfg,
            host_id: host_id.to_string(),
            url_spec: format!("{ep}/v2/ingest/relay/spec"),
            url_ack: format!("{ep}/v2/ingest/relay/spec/ack"),
            url_observed: format!("{ep}/v2/ingest/relay/observed"),
            token_file: token_file.to_path_buf(),
            agent,
            state_path: state_dir.join("manage.json"),
            backup_dir: state_dir.join("config-backups"),
            generation: 0,
            pending_acks: vec![],
            rejected: None,
            done_cmds: vec![],
            last_ack: None,
            last_poll: None,
            observed_sha: None,
            observed_at: None,
            obs_checked: None,
            disabled_by_server: false,
            ready: None,
            runtime_check: None,
            last_check: None,
        };
        if let Ok(b) = std::fs::read(&m.state_path) {
            if let Ok(v) = serde_json::from_slice::<Value>(&b) {
                m.generation = v["generation"].as_u64().unwrap_or(0);
                m.pending_acks = v["pending_acks"].as_array().cloned().unwrap_or_default();
                m.done_cmds = v["done_cmds"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(|s| s.to_string())).collect();
                m.last_ack = v.get("last_ack").cloned().filter(|x| !x.is_null());
                if let (Some(a), Some(b)) = (v["rejected"][0].as_str(), v["rejected"][1].as_str()) {
                    m.rejected = Some((a.into(), b.into()));
                }
            }
        }
        m
    }

    fn save(&self) {
        let v = json!({"generation": self.generation, "pending_acks": self.pending_acks, "done_cmds": self.done_cmds,
            "last_ack": self.last_ack, "rejected": self.rejected.as_ref().map(|(a, b)| json!([a, b]))});
        let _ = write_atomic(&self.state_path, v.to_string().as_bytes());
    }

    fn token(&self) -> R<String> {
        std::fs::read_to_string(&self.token_file).map(|s| s.trim().to_string()).map_err(|e| format!("读令牌：{e}"))
    }

    pub fn self_check(&mut self) {
        let r = (|| -> R<()> {
            let _l = lock(&self.cfg.lock, Duration::from_secs(5))?;
            let p = Path::new(&self.cfg.config).with_extension("fleet-relayd.probe");
            std::fs::write(&p, b"").map_err(|e| format!("配置目录不可写：{e}"))?;
            std::fs::remove_file(&p).map_err(|e| e.to_string())?;
            read_json(&self.cfg.config)?;
            Ok(())
        })();
        match &r {
            Ok(()) => info!("manage_ready", "config={} lock={} generation={}", crate::log::q(&self.cfg.config), crate::log::q(&self.cfg.lock), self.generation),
            Err(e) => warn!("manage_not_ready", "err={}", crate::log::q(e)),
        }
        self.ready = Some(r);
    }

    pub fn state_json(&self) -> Value {
        json!({"enabled": true, "server_enabled": !self.disabled_by_server, "spec_generation": self.generation,
            "pending_acks": self.pending_acks.len(),
            "ready": match &self.ready { Some(Ok(())) => json!(true), Some(Err(e)) => json!(e), None => Value::Null },
            "last_ack": self.last_ack.as_ref().map(|a| json!({"generation": a["generation"], "status": a["status"], "applied_at": a["applied_at"], "error": a["error"]})),
            "runtime_check": self.runtime_check})
    }

    fn exe(&self, col: &Collector) -> R<String> {
        self.cfg.xray_bin.clone().or_else(|| col.xray_exe()).ok_or_else(|| "找不到 xray 可执行文件（进程不在且未配置 manage.xray_bin）".into())
    }

    fn xray(&self, exe: &str, args: &[String], timeout: Duration) -> R<String> {
        let asset = self.cfg.asset_dir.clone().or_else(|| Path::new(exe).parent().map(|p| p.display().to_string())).unwrap_or_default();
        let mut cmd = Command::new(exe);
        cmd.args(args).env("XRAY_LOCATION_ASSET", asset);
        // 边跑边读输出（lsi 在 central 超过 64 KiB 管道缓冲）；超时由 PDEATHSIG 杀掉并回收（seccomp 下 kill 不可用）
        let out = crate::child::run(cmd, timeout).map_err(|e| match e {
            crate::child::RunError::Timeout => format!("xray {} 超时", args[..args.len().min(2)].join(" ")),
            e => format!("执行 xray：{e}"),
        })?;
        let so = String::from_utf8_lossy(&out.stdout).to_string();
        if !out.status.success() {
            let all = format!("{so}{}", String::from_utf8_lossy(&out.stderr));
            let tail: String = all.chars().rev().take(500).collect::<Vec<_>>().into_iter().rev().collect();
            return Err(format!("xray {} 失败：{}", args[..args.len().min(2)].join(" "), tail.trim()));
        }
        Ok(so)
    }

    fn api(&self, exe: &str, addr: &str, argv: &[String]) -> R<Value> {
        let mut args = vec!["api".to_string(), argv[0].clone(), format!("--server={addr}")];
        args.extend_from_slice(&argv[1..]);
        let out = self.xray(exe, &args, Duration::from_secs(15))?;
        if out.trim().is_empty() {
            return Ok(json!({}));
        }
        Ok(serde_json::from_str(&out).unwrap_or_else(|_| json!({"raw": out.trim()})))
    }

    fn tmp(&self, name: &str, v: &Value) -> R<String> {
        let p = Path::new(&self.cfg.config).with_extension(format!("fleet-relayd.{name}.json"));
        write_json_durable(&p.display().to_string(), v)?;
        Ok(p.display().to_string())
    }

    /// `xray run -test`：在候选配置的副本上跑，副本把 log.access / log.error 设成 "none"——
    /// -test 会按配置打开日志文件写入，而 relayd 的沙箱里 /var/log 只读；日志路径不属于本次变更，真实配置保持原样。
    fn test_config(&self, exe: &str, cand: &Value) -> R<()> {
        let mut t = cand.clone();
        if t.get("log").is_some_and(|l| l.is_object()) {
            t["log"]["access"] = json!("none");
            t["log"]["error"] = json!("none");
        }
        let p = self.tmp("candidate", &t)?;
        let r = self.xray(exe, &["run".into(), "-test".into(), "-c".into(), p.clone()], Duration::from_secs(90));
        let _ = std::fs::remove_file(&p);
        r.map(|_| ())
    }

    fn runtime_view(&self, exe: &str, addr: &str) -> R<Value> {
        let lsi = self.api(exe, addr, &["lsi".into()])?;
        let lso = self.api(exe, addr, &["lso".into()])?;
        let lr = self.api(exe, addr, &["lsrules".into()])?;
        let mut ins: Vec<String> = lsi["inbounds"].as_array().into_iter().flatten().map(|i| tag(i).to_string()).collect();
        ins.sort();
        let mut outs: Vec<String> = lso["outbounds"].as_array().into_iter().flatten().map(|o| tag(o).to_string()).collect();
        outs.sort();
        let rules: Vec<Value> = lr["rules"].as_array().into_iter().flatten().map(|r| r["tag"].clone()).collect();
        Ok(json!({"inbounds": ins, "outbounds": outs, "rules": rules}))
    }

    fn spec_view(c: &Value) -> Value {
        let mut ins: Vec<String> = c["inbounds"].as_array().into_iter().flatten().map(|i| tag(i).to_string()).collect();
        ins.sort();
        let mut outs: Vec<String> = c["outbounds"].as_array().into_iter().flatten().map(|o| tag(o).to_string()).collect();
        if c.get("metrics").is_some_and(|m| !m.is_null()) {
            outs.push("metrics_out".into());
        }
        outs.sort();
        let rules: Vec<Value> = c["routing"]["rules"].as_array().into_iter().flatten().map(|r| r["outboundTag"].clone()).collect();
        json!({"inbounds": ins, "outbounds": outs, "rules": rules})
    }

    fn view_matches(rv: &Value, sv: &Value) -> bool {
        rv["inbounds"] == sv["inbounds"] && rv["rules"] == sv["rules"] && {
            // lso 不一定列出 metrics_out 等内部出站：只要求期望的出站都在
            let r: BTreeSet<String> = rv["outbounds"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(|s| s.to_string())).collect();
            sv["outbounds"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).filter(|t| *t != "metrics_out").all(|t| r.contains(t))
        }
    }

    /// 启动时与每 10 min：运行态与磁盘配置是否一致
    pub fn periodic_check(&mut self, col: &Collector) {
        if self.last_check.is_some_and(|t| t.elapsed() < Duration::from_secs(600)) {
            return;
        }
        self.last_check = Some(Instant::now());
        let r = (|| -> R<Value> {
            let exe = self.exe(col)?;
            let c = read_json(&self.cfg.config)?;
            let rv = self.runtime_view(&exe, &col.api())?;
            let sv = Self::spec_view(&c);
            Ok(json!({"ts": round2(now_f64()), "match": Self::view_matches(&rv, &sv), "rules_match": rv["rules"] == sv["rules"], "inbounds_match": rv["inbounds"] == sv["inbounds"]}))
        })();
        if let Err(e) = &r {
            // 同样的错误只记一次（原来完全不记日志：central 上 lsi 每次超时却看不出来）
            if self.runtime_check.as_ref().and_then(|v| v["error"].as_str()) != Some(e.as_str()) {
                warn!("manage_runtime_check_failed", "err={}", crate::log::q(e));
            }
        }
        self.runtime_check = Some(r.unwrap_or_else(|e| json!({"ts": round2(now_f64()), "error": e})));
        if self.runtime_check.as_ref().is_some_and(|v| v["match"] == false) {
            warn!("manage_runtime_mismatch", "detail={}", crate::log::q(&self.runtime_check.as_ref().unwrap().to_string()));
        }
    }

    // ---------- 主循环入口 ----------

    pub fn tick(&mut self, col: &mut Collector) {
        self.flush_acks();
        self.maybe_observe();
        if self.last_poll.is_some_and(|t| t.elapsed() < Duration::from_secs(self.cfg.poll_s)) {
            return;
        }
        self.last_poll = Some(Instant::now());
        let Ok(tok) = self.token() else { return };
        let r = self
            .agent
            .get(&self.url_spec)
            .query("have", &self.generation.to_string())
            .query("wait", "0")
            .set("Authorization", &format!("Bearer {tok}"))
            .call();
        let spec: Value = match r {
            Ok(resp) if resp.status() == 304 => {
                self.disabled_by_server = false;
                return;
            }
            Ok(resp) => match resp.into_json() {
                Ok(v) => v,
                Err(e) => {
                    warn!("manage_spec_bad_json", "err={}", crate::log::q(&e.to_string()));
                    return;
                }
            },
            Err(ureq::Error::Status(404, _)) => {
                if !self.disabled_by_server {
                    info!("manage_server_disabled", "note={}", crate::log::q("fleetd 尚未提供 /v2/ingest/relay/spec，只观测"));
                }
                self.disabled_by_server = true;
                return;
            }
            Err(ureq::Error::Status(304, _)) => return,
            Err(e) => {
                warn!("manage_spec_poll_failed", "err={}", crate::log::q(&e.to_string().chars().take(200).collect::<String>()));
                return;
            }
        };
        self.disabled_by_server = false;
        if spec["schema"] != "fleet.relayspec.v1" {
            warn!("manage_spec_bad_schema", "schema={}", spec["schema"]);
            return;
        }
        self.handle_spec(&spec, col);
        self.flush_acks();
    }

    fn maybe_observe(&mut self) {
        if self.disabled_by_server || self.obs_checked.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
            return;
        }
        self.obs_checked = Some(Instant::now());
        let Ok(bytes) = std::fs::read(&self.cfg.config) else { return };
        let sha = sha256_hex(&bytes);
        let hourly = self.observed_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(3600));
        if self.observed_sha.as_deref() == Some(sha.as_str()) && !hourly {
            return;
        }
        let Ok(c) = serde_json::from_slice::<Value>(&bytes) else { return };
        let body = json!({"schema": "fleet.relayobs.v1", "relay": self.host_id, "config_sha256": sha, "generation": self.generation,
            "managed": managed_of(&c), "observed_at": round2(now_f64())});
        let Ok(tok) = self.token() else { return };
        match self.agent.post(&self.url_observed).set("Authorization", &format!("Bearer {tok}")).send_json(body) {
            Ok(_) => {
                info!("manage_observed_sent", "config_sha256={} generation={}", &sha[..16], self.generation);
                self.observed_sha = Some(sha);
                self.observed_at = Some(Instant::now());
            }
            Err(ureq::Error::Status(404, _)) => self.disabled_by_server = true,
            Err(e) => warn!("manage_observed_failed", "err={}", crate::log::q(&e.to_string().chars().take(200).collect::<String>())),
        }
    }

    fn flush_acks(&mut self) {
        if self.pending_acks.is_empty() {
            return;
        }
        let Ok(tok) = self.token() else { return };
        while let Some(a) = self.pending_acks.first().cloned() {
            match self.agent.post(&self.url_ack).set("Authorization", &format!("Bearer {tok}")).send_json(a) {
                Ok(_) => {
                    self.pending_acks.remove(0);
                    self.save();
                }
                Err(ureq::Error::Status(c, _)) if (400..500).contains(&c) && c != 404 && c != 429 => {
                    warn!("manage_ack_rejected", "status={c}");
                    self.pending_acks.remove(0);
                    self.save();
                }
                Err(_) => break,
            }
        }
    }

    fn handle_spec(&mut self, spec: &Value, col: &mut Collector) {
        let generation = spec["generation"].as_u64().unwrap_or(0);
        let spec_sha = spec["spec_sha256"].as_str().unwrap_or("").to_string();
        if generation == 0 {
            // 未接管（跟随文件）；但接管前也可能有待执行命令（例如 restart_xray）：只执行命令，不碰配置，回执 generation 0
            let mut cmd_results = vec![];
            for c in spec["commands"].as_array().into_iter().flatten() {
                let id = c["id"].as_str().unwrap_or("").to_string();
                if id.is_empty() || self.done_cmds.contains(&id) {
                    continue;
                }
                let r = if c["expires_at"].as_f64().is_some_and(|e| e > 0.0 && e < now_f64()) {
                    json!({"id": id, "ok": false, "error": "expired", "finished_at": round2(now_f64())})
                } else {
                    self.run_command(c, col)
                };
                self.done_cmds.push(id);
                cmd_results.push(r);
            }
            if !cmd_results.is_empty() {
                let sha = std::fs::read(&self.cfg.config).map(|b| sha256_hex(&b)).unwrap_or_default();
                let a = json!({"generation": 0, "spec_sha256": null, "status": "applied", "xray_restarted": cmd_results.iter().any(|r| r["ok"] == true && r["data"]["restart_ms"].is_number()),
                    "config_sha256_before": sha, "config_sha256_after": sha, "applied_at": round2(now_f64()), "results": [], "commands": cmd_results});
                self.pending_acks.push(a);
                self.save();
            }
            return;
        }
        let file_sha_now = std::fs::read(&self.cfg.config).map(|b| sha256_hex(&b)).unwrap_or_default();
        let same_rejected = self.rejected.as_ref().is_some_and(|(s, f)| *s == spec_sha && *f == file_sha_now);
        let mut ack = if generation == self.generation && self.last_ack.as_ref().is_some_and(|a| a["spec_sha256"] == spec_sha.as_str()) {
            None // 已应用过（只有命令）
        } else if same_rejected {
            None // 同一份 spec 在同一份文件上已拒绝过：等 fleetd 收编后重发
        } else {
            Some(self.apply_spec(spec, col))
        };
        let spec_ok = match &ack {
            Some(a) => a["status"] == "applied",
            None => !same_rejected,
        };
        // 命令
        let mut cmd_results = vec![];
        for c in spec["commands"].as_array().into_iter().flatten() {
            let id = c["id"].as_str().unwrap_or("").to_string();
            if id.is_empty() || self.done_cmds.contains(&id) {
                continue;
            }
            let r = if c["expires_at"].as_f64().is_some_and(|e| e > 0.0 && e < now_f64()) {
                json!({"id": id, "ok": false, "error": "expired", "finished_at": round2(now_f64())})
            } else if !spec_ok {
                json!({"id": id, "ok": false, "error": "skipped: spec rejected", "finished_at": round2(now_f64())})
            } else {
                self.run_command(c, col)
            };
            self.done_cmds.push(id);
            if self.done_cmds.len() > 200 {
                self.done_cmds.remove(0);
            }
            cmd_results.push(r);
        }
        if ack.is_none() && !cmd_results.is_empty() {
            // 只有命令：带上一次回执的信息，重复回执幂等
            let mut a = self.last_ack.clone().unwrap_or_else(|| json!({"generation": generation, "spec_sha256": spec_sha, "status": "applied"}));
            a["applied_at"] = json!(round2(now_f64()));
            ack = Some(a);
        }
        if let Some(mut a) = ack {
            a["commands"] = Value::Array(cmd_results);
            if a["status"] == "applied" {
                self.generation = generation;
                self.rejected = None;
            } else {
                self.rejected = Some((spec_sha.clone(), std::fs::read(&self.cfg.config).map(|b| sha256_hex(&b)).unwrap_or_default()));
            }
            self.last_ack = Some(a.clone());
            self.pending_acks.push(a);
            self.save();
            col.force_reload_config();
            self.observed_sha = None; // 下个周期重报观测
            self.last_check = None;
            self.periodic_check(col);
        }
    }

    fn apply_spec(&mut self, spec: &Value, col: &mut Collector) -> Value {
        let generation = spec["generation"].as_u64().unwrap_or(0);
        let spec_sha = spec["spec_sha256"].as_str().unwrap_or("").to_string();
        let t0 = now_f64();
        let mut ack = json!({"generation": generation, "spec_sha256": spec_sha, "status": "rejected", "xray_restarted": false, "results": []});
        let res = self.apply_inner(spec, col, &mut ack);
        ack["applied_at"] = json!(round2(now_f64()));
        ack["duration_ms"] = json!(((now_f64() - t0) * 1000.0).round());
        match res {
            Ok(()) => {
                ack["status"] = json!("applied");
                info!("manage_spec_applied", "generation={generation} results={}", ack["results"].as_array().map(|a| a.len()).unwrap_or(0));
            }
            Err(e) => {
                ack["error"] = json!(e);
                error!("manage_spec_rejected", "generation={generation} error={}", crate::log::q(&e));
            }
        }
        ack
    }

    fn apply_inner(&mut self, spec: &Value, col: &mut Collector, ack: &mut Value) -> R<()> {
        if let Some(Err(e)) = &self.ready {
            return Err(format!("not_ready: {e}"));
        }
        let _l = lock(&self.cfg.lock, Duration::from_secs(60))?;
        let bytes = std::fs::read(&self.cfg.config).map_err(|e| format!("读配置：{e}"))?;
        let sha_before = sha256_hex(&bytes);
        ack["config_sha256_before"] = json!(sha_before);
        ack["config_sha256_after"] = json!(sha_before);
        let before: Value = serde_json::from_slice(&bytes).map_err(|e| format!("解析配置：{e}"))?;
        let cand = build_candidate(&before, &spec["managed"])?;
        if cand == before {
            // 文件已经是期望状态（首次收编，或上次写完文件还没回执就退出了）
            ack["diff"] = json!({"noop": true});
            return Ok(());
        }
        if spec["base_config_sha256"].as_str().is_some_and(|b| b != sha_before) {
            return Err("base_changed".into());
        }
        let (diff, g) = guard(&before, &cand);
        ack["diff"] = diff;
        g.map_err(|e| format!("guard: {e}"))?;
        let exe = self.exe(col)?;
        let addr = col.api();
        self.test_config(&exe, &cand).map_err(|e| format!("xray -test: {e}"))?;
        let rv_before = self.runtime_view(&exe, &addr)?;
        // 操作前补采，入站被移除/替换期间计数不丢
        let _ = col.fold_now();
        let find = |c: &Value, k: &str, t: &str| c[k].as_array().and_then(|a| a.iter().find(|x| tag(x) == t).cloned());
        let tags = |c: &Value, k: &str, pred: fn(&str) -> bool| -> Vec<String> {
            c[k].as_array().into_iter().flatten().map(|x| tag(x).to_string()).filter(|t| pred(t)).collect()
        };
        let mut steps: Vec<Step> = vec![];
        let mut results: Vec<Value> = vec![];
        let mut files: Vec<String> = vec![];
        let run = |m: &Self, st: Step, argvs: Vec<Vec<String>>, results: &mut Vec<Value>, steps: &mut Vec<Step>| -> R<()> {
            for a in &argvs {
                if let Err(e) = m.api(&exe, &addr, a) {
                    results.push(json!({"object": st.object, "tag": st.tag, "op": st.op, "ok": false, "error": e}));
                    // 这一步可能只做了一半（rmi 成功、adi 失败）：它的撤回也要执行
                    steps.push(st);
                    return Err(e);
                }
            }
            results.push(json!({"object": st.object, "tag": st.tag, "op": st.op, "ok": true}));
            steps.push(st);
            Ok(())
        };
        let mut tmpn = 0;
        let mut mk = |m: &Self, key: &str, v: &Value, files: &mut Vec<String>| -> R<String> {
            tmpn += 1;
            let p = m.tmp(&format!("step{tmpn}"), &json!({key: [v]}))?;
            files.push(p.clone());
            Ok(p)
        };
        let outcome = (|| -> R<()> {
            // 1. 出站：新增 / 替换
            for t in tags(&cand, "outbounds", managed_out) {
                let new = find(&cand, "outbounds", &t).unwrap();
                match find(&before, "outbounds", &t) {
                    None => {
                        let f = mk(self, "outbounds", &new, &mut files)?;
                        run(self, Step { object: "outbound", tag: t.clone(), op: "add", undo: vec![vec!["rmo".into(), t.clone()]] }, vec![vec!["ado".into(), f]], &mut results, &mut steps)?;
                    }
                    Some(old) if old != new => {
                        let (fo, fnew) = (mk(self, "outbounds", &old, &mut files)?, mk(self, "outbounds", &new, &mut files)?);
                        run(
                            self,
                            Step { object: "outbound", tag: t.clone(), op: "replace", undo: vec![vec!["rmo".into(), t.clone()], vec!["ado".into(), fo]] },
                            vec![vec!["rmo".into(), t.clone()], vec!["ado".into(), fnew]],
                            &mut results,
                            &mut steps,
                        )?;
                    }
                    _ => {}
                }
            }
            // 2. 入站：删除 / 替换 / 新增
            for t in tags(&before, "inbounds", is_entry_tag) {
                let old = find(&before, "inbounds", &t).unwrap();
                match find(&cand, "inbounds", &t) {
                    None => {
                        let fo = mk(self, "inbounds", &old, &mut files)?;
                        run(self, Step { object: "inbound", tag: t.clone(), op: "remove", undo: vec![vec!["adi".into(), fo]] }, vec![vec!["rmi".into(), t.clone()]], &mut results, &mut steps)?;
                    }
                    Some(new) if new != old => {
                        let (fo, fnew) = (mk(self, "inbounds", &old, &mut files)?, mk(self, "inbounds", &new, &mut files)?);
                        run(
                            self,
                            Step { object: "inbound", tag: t.clone(), op: "replace", undo: vec![vec!["rmi".into(), t.clone()], vec!["adi".into(), fo]] },
                            vec![vec!["rmi".into(), t.clone()], vec!["adi".into(), fnew]],
                            &mut results,
                            &mut steps,
                        )?;
                    }
                    _ => {}
                }
            }
            for t in tags(&cand, "inbounds", is_entry_tag) {
                if find(&before, "inbounds", &t).is_none() {
                    let fnew = mk(self, "inbounds", &find(&cand, "inbounds", &t).unwrap(), &mut files)?;
                    run(self, Step { object: "inbound", tag: t.clone(), op: "add", undo: vec![vec!["rmi".into(), t.clone()]] }, vec![vec!["adi".into(), fnew]], &mut results, &mut steps)?;
                }
            }
            // 3. 规则：整表替换（顺序由候选配置决定）
            if cand["routing"]["rules"] != before["routing"]["rules"] {
                let fo = self.tmp("rules-old", &json!({"routing": {"rules": before["routing"]["rules"]}}))?;
                let fnew = self.tmp("rules-new", &json!({"routing": {"rules": cand["routing"]["rules"]}}))?;
                files.push(fo.clone());
                files.push(fnew.clone());
                run(self, Step { object: "rule", tag: "*".into(), op: "replace", undo: vec![vec!["adrules".into(), fo]] }, vec![vec!["adrules".into(), fnew]], &mut results, &mut steps)?;
            }
            // 4. 出站：删除（规则已不再引用）
            for t in tags(&before, "outbounds", managed_out) {
                if find(&cand, "outbounds", &t).is_none() {
                    let fo = mk(self, "outbounds", &find(&before, "outbounds", &t).unwrap(), &mut files)?;
                    run(self, Step { object: "outbound", tag: t.clone(), op: "remove", undo: vec![vec!["ado".into(), fo]] }, vec![vec!["rmo".into(), t.clone()]], &mut results, &mut steps)?;
                }
            }
            // 5. 核对运行态
            let rv = self.runtime_view(&exe, &addr)?;
            let sv = Self::spec_view(&cand);
            if !Self::view_matches(&rv, &sv) {
                return Err(format!("运行态与候选配置不一致：runtime={rv} spec={sv}"));
            }
            ack["runtime"] = json!({"before": rv_before, "after": rv});
            // 6. 原子写文件（旧版本先备份）
            let _ = std::fs::create_dir_all(&self.backup_dir);
            let bk = self.backup_dir.join(format!("config-{}-gen{}.json", now_f64() as u64, spec["generation"]));
            std::fs::write(&bk, &bytes).map_err(|e| format!("备份：{e}"))?;
            prune_backups(&self.backup_dir, 30);
            write_json_durable(&self.cfg.config, &cand)?;
            Ok(())
        })();
        if let Err(e) = outcome {
            // 撤回：相反顺序
            let mut undo_err = vec![];
            for st in steps.iter().rev() {
                for a in &st.undo {
                    if let Err(x) = self.api(&exe, &addr, a) {
                        // 单项撤回失败（例如对象本来就没加上）继续撤其他的
                        undo_err.push(format!("{} {}: {x}", st.object, st.tag));
                    }
                }
            }
            let back = self.runtime_view(&exe, &addr).map(|r| Self::view_matches(&r, &Self::spec_view(&before))).unwrap_or(false);
            ack["results"] = Value::Array(results);
            for f in &files {
                let _ = std::fs::remove_file(f);
            }
            ack["runtime"] = json!({"before": rv_before, "rolled_back_to_before": back, "undo_errors": undo_err});
            return Err(format!("{e}；已撤回{}", if back { "，运行态回到应用前" } else { "，但运行态未能确认回到应用前，请人工核对" }));
        }
        for f in &files {
            let _ = std::fs::remove_file(f);
        }
        ack["results"] = Value::Array(results);
        ack["config_sha256_after"] = json!(std::fs::read(&self.cfg.config).map(|b| sha256_hex(&b)).unwrap_or_default());
        Ok(())
    }

    fn run_command(&mut self, c: &Value, col: &mut Collector) -> Value {
        let id = c["id"].as_str().unwrap_or("");
        let kind = c["kind"].as_str().unwrap_or("");
        info!("manage_command", "id={} kind={}", crate::log::q(id), crate::log::q(kind));
        let r: R<Value> = match kind {
            "restart_xray" => self.cmd_restart(col),
            "reset_traffic" => self.cmd_reset(&c["args"], col),
            // 只读：xray_logs 的结果放在 result 字段（fleetd 0.6.2 约定）
            "xray_logs" => {
                return match self.cmd_logs(&c["args"]) {
                    Ok(r) => json!({"id": id, "ok": true, "error": null, "finished_at": round2(now_f64()), "result": r}),
                    Err(e) => json!({"id": id, "ok": false, "error": e, "finished_at": round2(now_f64())}),
                };
            }
            other => Err(format!("不支持的命令 {other}")),
        };
        match r {
            Ok(data) => json!({"id": id, "ok": true, "error": null, "finished_at": round2(now_f64()), "data": data}),
            Err(e) => {
                error!("manage_command_failed", "id={} err={}", crate::log::q(id), crate::log::q(&e));
                json!({"id": id, "ok": false, "error": e, "finished_at": round2(now_f64())})
            }
        }
    }

    fn cmd_restart(&mut self, col: &mut Collector) -> R<Value> {
        let _l = lock(&self.cfg.lock, Duration::from_secs(60))?;
        let cfg = read_json(&self.cfg.config)?;
        let exe = self.exe(col)?;
        self.test_config(&exe, &cfg).map_err(|e| format!("磁盘上的配置 xray -test 不通过，拒绝重启：{e}"))?;
        let _ = col.fold_now();
        let t0 = Instant::now();
        let mut args = self.cfg.systemctl.clone();
        args.push("restart".into());
        args.push(self.cfg.unit.clone());
        let st = Command::new(&args[0]).args(&args[1..]).stdin(Stdio::null()).output().map_err(|e| format!("执行 systemctl：{e}"))?;
        if !st.status.success() {
            return Err(format!("systemctl restart 失败：{}", String::from_utf8_lossy(&st.stderr).trim()));
        }
        let ports: BTreeSet<u16> = cfg["inbounds"].as_array().into_iter().flatten().filter_map(|i| i["port"].as_u64().map(|p| p as u16)).collect();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
        let mut up = false;
        while t0.elapsed() < Duration::from_secs(30) {
            if ports.is_subset(&listening_ports()) && crate::xray::query_stats(&rt, &col.api()).is_ok() {
                up = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let ms = t0.elapsed().as_millis() as u64;
        let _ = col.xray_running();
        let exe2 = self.exe(col)?;
        let rv = self.runtime_view(&exe2, &col.api())?;
        let ok = up && Self::view_matches(&rv, &Self::spec_view(&cfg));
        if !ok {
            return Err(format!("重启后 {ms} ms 内端口/API/规则未全部就绪（ports_up={up}）"));
        }
        Ok(json!({"restart_ms": ms, "ports": ports.len(), "runtime": rv}))
    }

    /// 只读：Xray 错误日志 / 访问日志的最后 N 行（从文件末尾最多读 256 KiB，每行 ≤ 2000 字符）。
    /// 路径取自运行中配置的 log 段；错误日志没配文件（空串 / none，写到 stdout）时读 fleet-xray 的 journal。
    fn cmd_logs(&self, args: &Value) -> R<Value> {
        let which = args["which"].as_str().unwrap_or("error");
        if which != "error" && which != "access" {
            return Err("which 只能是 error 或 access".into());
        }
        let n = args["lines"].as_u64().unwrap_or(100).clamp(1, 500) as usize;
        let cfg = read_json(&self.cfg.config)?;
        let path = cfg["log"][which].as_str().unwrap_or("").to_string();
        let cut = |l: &str| -> String { l.chars().take(2000).collect() };
        if path.is_empty() || path == "none" {
            if which == "access" {
                return Err(format!("配置里没有访问日志文件（log.access = {path:?}）"));
            }
            let out = Command::new("journalctl")
                .args(["-u", &self.cfg.unit, "-n", &n.to_string(), "-o", "short-iso", "--no-pager"])
                .stdin(Stdio::null())
                .output()
                .map_err(|e| format!("读 journal：{e}"))?;
            if !out.status.success() {
                return Err(format!("journalctl 失败：{}", String::from_utf8_lossy(&out.stderr).trim()));
            }
            let lines: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.starts_with("-- ")).map(cut).collect();
            return Ok(json!({"lines": lines, "truncated": false, "file": format!("journal:{}", self.cfg.unit), "size": null}));
        }
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(&path).map_err(|e| format!("打开 {path}：{e}"))?;
        let size = f.metadata().map(|m| m.len()).unwrap_or(0);
        let start = size.saturating_sub(256 * 1024);
        f.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
        let mut buf = Vec::with_capacity((size - start) as usize);
        f.take(256 * 1024).read_to_end(&mut buf).map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&buf);
        let mut all: Vec<&str> = text.split('\n').collect();
        if all.last() == Some(&"") {
            all.pop(); // 以换行结尾
        } else if !all.is_empty() {
            all.pop(); // 末尾是正在写的半行
        }
        let mut truncated = false;
        if start > 0 && !all.is_empty() {
            all.remove(0); // 起点落在行中间
            truncated = true;
        }
        if all.len() > n {
            truncated = true;
        }
        let lines: Vec<String> = all[all.len().saturating_sub(n)..].iter().map(|l| cut(l)).collect();
        Ok(json!({"lines": lines, "truncated": truncated, "file": path, "size": size}))
    }

    fn cmd_reset(&mut self, args: &Value, col: &mut Collector) -> R<Value> {
        let mut names: Vec<String> = args["names"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(|s| s.to_string())).collect();
        if let Some(t) = args["tag"].as_str() {
            if !is_entry_tag(t) {
                return Err("tag 必须是入口入站（inbound-<端口> 或 in-*）".into());
            }
            names.push(format!("inbound>>>{t}>>>traffic>>>uplink"));
            names.push(format!("inbound>>>{t}>>>traffic>>>downlink"));
            if args["users"].as_bool().unwrap_or(true) {
                for e in col.inbound_users(t) {
                    names.push(format!("user>>>{e}>>>traffic>>>uplink"));
                    names.push(format!("user>>>{e}>>>traffic>>>downlink"));
                }
            }
        }
        if names.is_empty() {
            return Err("没有要重置的计数（给 tag 或 names）".into());
        }
        for n in &names {
            let ok = n.split(">>>").count() == 4 && (n.starts_with("inbound>>>") || n.starts_with("user>>>")) && n.contains(">>>traffic>>>");
            let entry_ok = !n.starts_with("inbound>>>") || n.split(">>>").nth(1).is_some_and(is_entry_tag);
            if !ok || !entry_ok {
                return Err(format!("只允许重置入口入站或用户的流量计数：{n}"));
            }
        }
        names.dedup();
        let counters = col.reset_counters(&names)?;
        let note = json!({"type": "traffic_reset", "at": round2(now_f64()), "counters": counters});
        col.notes.push(note.clone());
        Ok(json!({"counters": counters}))
    }
}

fn prune_backups(dir: &Path, keep: usize) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        v.sort();
        for old in v.iter().take(v.len().saturating_sub(keep)) {
            let _ = std::fs::remove_file(old);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Value {
        json!({"log": {"access": "/x"}, "inbounds": [
            {"tag": "api", "port": 62789, "listen": "127.0.0.1", "protocol": "tunnel"},
            {"tag": "inbound-20001", "port": 20001, "protocol": "vless", "settings": {"clients": [{"id": "a", "email": "node-00"}]}},
            {"tag": "inbound-20002", "port": 20002, "protocol": "vless", "settings": {"clients": [{"id": "b", "email": "node-26"}]}}],
          "outbounds": [{"tag": "fleet-default-deny", "protocol": "blackhole"}, {"tag": "direct", "protocol": "freedom"}, {"tag": "blocked", "protocol": "blackhole"},
                        {"tag": "ss-1-2-3-4", "protocol": "shadowsocks"}],
          "routing": {"domainStrategy": "AsIs", "rules": [
            {"type": "field", "inboundTag": ["api"], "outboundTag": "api"},
            {"type": "field", "ip": ["geoip:private"], "outboundTag": "blocked"},
            {"type": "field", "inboundTag": ["inbound-20002"], "outboundTag": "ss-1-2-3-4"},
            {"type": "field", "inboundTag": ["inbound-20001"], "outboundTag": "direct", "ruleTag": "fleet-explicit-inbound-20001"}]}})
    }

    #[test]
    fn noop_roundtrip() {
        let c = cfg();
        let cand = build_candidate(&c, &managed_of(&c)).unwrap();
        assert_eq!(cand, c);
        assert!(guard(&c, &cand).1.is_ok());
    }

    #[test]
    fn disable_policy_and_guards() {
        let c = cfg();
        // 停用 20002（入站从受管集合里拿掉），加一条策略
        let mut m = managed_of(&c);
        m["inbounds"].as_array_mut().unwrap().retain(|i| i["tag"] != "inbound-20002");
        m["routing_rules"].as_array_mut().unwrap().push(json!({"type": "field", "inboundTag": ["inbound-20001"], "ruleTag": "policy-u1-default", "outboundTag": "blocked"}));
        let cand = build_candidate(&c, &m).unwrap();
        let (d, g) = guard(&c, &cand);
        assert!(g.is_ok(), "{g:?}");
        assert_eq!(d["inbounds"]["removed"], json!(["inbound-20002"]));
        assert_eq!(d["inbounds"]["identical"], 2);
        let order: Vec<&str> = cand["routing"]["rules"].as_array().unwrap().iter().map(|r| r["ruleTag"].as_str().unwrap_or(r["outboundTag"].as_str().unwrap())).collect();
        assert_eq!(order, vec!["api", "blocked", "policy-u1-default", "ss-1-2-3-4", "fleet-explicit-inbound-20001"]);
        // 拿掉直连显式规则 → 20001 没有路由 → 拒绝
        let mut m2 = managed_of(&c);
        m2["routing_rules"] = json!([]);
        let (_, g) = guard(&c, &build_candidate(&c, &m2).unwrap());
        assert!(g.unwrap_err().contains("R1 blackhole"));
        // spec 里夹带非受管对象 → 拒绝
        let mut m3 = managed_of(&c);
        m3["outbounds"].as_array_mut().unwrap().push(json!({"tag": "direct", "protocol": "freedom"}));
        assert!(build_candidate(&c, &m3).is_err());
        let mut m4 = managed_of(&c);
        m4["routing_rules"].as_array_mut().unwrap().push(json!({"type": "field", "ip": ["1.1.1.1"], "outboundTag": "direct"}));
        assert!(build_candidate(&c, &m4).is_err());
        // 新出站追加在末尾，outbounds[0] 不动
        let mut m5 = managed_of(&c);
        m5["outbounds"].as_array_mut().unwrap().push(json!({"tag": "ss-9-9-9-9", "protocol": "shadowsocks"}));
        let cand5 = build_candidate(&c, &m5).unwrap();
        assert_eq!(cand5["outbounds"][0]["tag"], "fleet-default-deny");
        assert_eq!(cand5["outbounds"][4]["tag"], "ss-9-9-9-9");
        assert!(guard(&c, &cand5).1.is_ok());
        // 纯调序：候选与原配置不同（要改写文件），每个入站内容逐字相同，守卫通过并报告 reordered
        let mut m6 = managed_of(&c);
        m6["inbounds"].as_array_mut().unwrap().reverse();
        let cand6 = build_candidate(&c, &m6).unwrap();
        assert_ne!(cand6, c);
        let tags6: Vec<&str> = cand6["inbounds"].as_array().unwrap().iter().map(|i| i["tag"].as_str().unwrap()).collect();
        assert_eq!(tags6, vec!["api", "inbound-20002", "inbound-20001"]);
        let (d6, g6) = guard(&c, &cand6);
        assert!(g6.is_ok());
        assert_eq!(d6["inbounds_order"]["reordered"], true);
        assert_eq!(d6["inbounds"]["identical"], 3);
        // 启用时 spec 里放回原位置 → 文件里也回到原位置
        let mut m7 = managed_of(&c);
        let first = m7["inbounds"].as_array_mut().unwrap().remove(0);
        let c7 = build_candidate(&c, &m7).unwrap();
        let mut m8 = managed_of(&c7);
        m8["inbounds"].as_array_mut().unwrap().insert(0, first);
        assert_eq!(build_candidate(&c7, &m8).unwrap(), c);
    }
}
