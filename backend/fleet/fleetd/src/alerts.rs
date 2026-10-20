//! 告警引擎 v1（ARCHITECTURE §3.11 的子集，语义对齐 Alertmanager）。
//!
//! - 实例：rule + labels（指纹）+ annotations（summary/evidence）；pending → firing → resolved，另有 ack。
//! - 分组：group_by = [relay, cause]；group_wait（critical 10 s / 其余 30 s）、group_interval 5 min、
//!   repeat_interval（critical 30 min 直到确认；warning 4 h）；info 只进每天 09:00（北京）的摘要。
//! - 抑制：relay_down ⇒ 该线路机名下节点告警；collector_stale / monitor_stale ⇒ 全部节点告警；
//!   correlated_node_failure 把成员节点的告警聚合进自己的一条通知（成员标记 aggregated），自身不被抑制。
//! - 静默与维护窗口：标签匹配 + 时间段；被压住的告警照常记录状态，只是不发通知；自行恢复的进摘要。
//!
//! 引擎是纯状态机：时间只从调用方传入（测试用假时钟），持久化由调用方取 dirty 集合落库。
use chrono::{Datelike, TimeZone};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

pub type Labels = BTreeMap<String, String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    pub fn parse(s: &str) -> Option<Severity> {
        match s {
            "info" => Some(Severity::Info),
            "warning" => Some(Severity::Warning),
            "critical" => Some(Severity::Critical),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }
    pub fn word(&self) -> &'static str {
        match self {
            Severity::Info => "信息",
            Severity::Warning => "警告",
            Severity::Critical => "严重",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Pending,
    Firing,
    Resolved,
}

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Firing => "firing",
            State::Resolved => "resolved",
        }
    }
    pub fn parse(s: &str) -> Option<State> {
        match s {
            "pending" => Some(State::Pending),
            "firing" => Some(State::Firing),
            "resolved" => Some(State::Resolved),
            _ => None,
        }
    }
}

/// 规则在一次评估里给出的“此刻成立的条件”。
#[derive(Clone, Debug)]
pub struct Observation {
    pub rule: String,
    pub labels: Labels,
    pub severity: Severity,
    pub summary: String,
    pub evidence: Value,
    /// 数据源给出的开始时刻（例如旧 monitor 的 since、主机最后上报时间），用于跨重启计算 for。
    pub active_since: Option<f64>,
    pub for_s: f64,
    pub notify_resolved: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Alert {
    pub id: i64,
    pub fingerprint: String,
    pub rule: String,
    pub severity: Severity,
    pub state: State,
    pub labels: Labels,
    pub summary: String,
    pub evidence: Value,
    pub pending_since: f64,
    pub fired_at: Option<f64>,
    pub resolved_at: Option<f64>,
    pub last_seen: f64,
    pub ack_at: Option<f64>,
    pub ack_by: Option<String>,
    pub suppressed: Option<String>,
    pub group_key: String,
    pub notified_state: Option<State>,
    pub notify_resolved: bool,
    pub digest_done: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Matcher {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub negate: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Silence {
    pub id: i64,
    /// silence | maintenance
    pub kind: String,
    pub matchers: Vec<Matcher>,
    pub starts_at: f64,
    pub ends_at: f64,
    pub tz: String,
    pub comment: String,
    pub created_by: String,
}

impl Silence {
    pub fn matches(&self, labels: &Labels) -> bool {
        !self.matchers.is_empty()
            && self.matchers.iter().all(|m| {
                let v = labels.get(&m.name).map(String::as_str).unwrap_or("");
                (v == m.value) != m.negate
            })
    }
    pub fn active(&self, now: f64) -> bool {
        self.starts_at <= now && now < self.ends_at
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct GroupState {
    pub first_change_at: Option<f64>,
    pub last_flush_at: Option<f64>,
    pub last_notify_at: Option<f64>,
    /// 上次发送时组里是否还有 firing（没有的话下一次变化按 group_wait 处理）
    pub open: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Outgoing {
    /// group | repeat | digest
    pub kind: String,
    pub severity: Severity,
    pub group_key: Option<String>,
    pub title: String,
    pub body: String,
    pub link: String,
    pub alert_ids: Vec<i64>,
    pub alerts: Vec<Value>,
    pub ts: f64,
}

#[derive(Clone, Debug)]
pub struct EngineCfg {
    pub group_wait_critical: f64,
    pub group_wait: f64,
    pub group_interval: f64,
    pub repeat_critical: f64,
    pub repeat_warning: f64,
    pub digest_hour: u32,
    pub digest_tz: Tz,
    pub ui_base: String,
}

impl EngineCfg {
    pub fn from_config(c: &crate::config::Config) -> EngineCfg {
        EngineCfg {
            group_wait_critical: c.alerts.group_wait_critical_s as f64,
            group_wait: c.alerts.group_wait_s as f64,
            group_interval: c.alerts.group_interval_s as f64,
            repeat_critical: c.alerts.repeat_critical_s as f64,
            repeat_warning: c.alerts.repeat_warning_s as f64,
            digest_hour: c.alerts.digest_hour,
            digest_tz: c.alerts.digest_tz.parse::<Tz>().unwrap_or(chrono_tz::Asia::Shanghai),
            ui_base: c.ui_base_url.clone(),
        }
    }
}

impl Default for EngineCfg {
    fn default() -> Self {
        EngineCfg {
            group_wait_critical: 10.0,
            group_wait: 30.0,
            group_interval: 300.0,
            repeat_critical: 1800.0,
            repeat_warning: 14400.0,
            digest_hour: 9,
            digest_tz: chrono_tz::Asia::Shanghai,
            ui_base: "https://38.58.57.22/#/".into(),
        }
    }
}

pub struct Engine {
    pub cfg: EngineCfg,
    alerts: BTreeMap<i64, Alert>,
    by_fp: HashMap<String, i64>,
    groups: BTreeMap<String, GroupState>,
    silences: Vec<Silence>,
    next_id: i64,
    pub last_digest_at: f64,
    pub next_digest_at: f64,
    dirty_alerts: BTreeSet<i64>,
    dirty_groups: BTreeSet<String>,
    /// 变化计数（SSE 的 alerts 资源版本依据）
    pub changes: u64,
}

pub fn fingerprint(rule: &str, labels: &Labels) -> String {
    let mut s = String::from(rule);
    for (k, v) in labels {
        s.push('|');
        s.push_str(k);
        s.push('=');
        s.push_str(v);
    }
    s
}

pub fn group_key(labels: &Labels, rule: &str) -> String {
    let relay = labels.get("relay").map(String::as_str).filter(|s| !s.is_empty()).unwrap_or("-");
    let cause = labels.get("cause").map(String::as_str).filter(|s| !s.is_empty()).unwrap_or(rule);
    format!("relay={relay}|cause={cause}")
}

pub fn rule_title(rule: &str) -> &'static str {
    match rule {
        "node_health_fail" => "节点故障",
        "node_health_degraded" => "节点降级",
        "egress_mismatch" => "出口 IP 不一致",
        "host_offline" => "主机离线",
        "host_resource" => "主机资源",
        "host_sync_stale" => "主机数据同步停滞",
        "relay_down" => "线路机宕机",
        "relay_mgmt_down" => "线路机管理通道中断",
        "relay_xray_down" => "线路机 Xray 未运行",
        "relay_egress_mismatch" => "线路机出口 IP 不一致",
        "relay_tcp_down" => "线路机主节点不可达",
        "relay_probe_stalled" => "线路机自测停滞",
        "egress_route_leak" => "出口泄漏到线路机",
        "collector_stale" => "采集器停滞",
        "monitor_stale" => "健康评估停滞",
        "correlated_node_failure" => "关联故障",
        "renewal_due" => "续费到期",
        "quota" => "流量配额",
        "legacy_source_down" => "数据源不可达",
        "channel_failing" => "通知渠道连续失败",
        r if r.starts_with("ipintel_") => ipintel::service::rule_title(r).unwrap_or("IP 情报"),
        _ => "告警",
    }
}

/// 原因 / 规则代码 → 中文短标签（界面“原因”列、通知标题都用它；未知代码原样返回）
pub const CAUSE_LABELS: &[(&str, &str)] = &[
    ("node_health_fail", "节点故障"), ("node_health_degraded", "节点降级"), ("egress_mismatch", "出口不一致"), ("host_offline", "主机离线"),
    ("host_resource", "资源紧张"), ("host_sync_stale", "主机数据停滞"), ("relay_down", "线路机宕机"), ("relay_mgmt_down", "管理通道中断"),
    ("relay_xray_down", "Xray 未运行"), ("relay_egress_mismatch", "线路机出口不一致"), ("relay_tcp_down", "主节点不可达"), ("relay_probe_stalled", "自测停滞"), ("egress_route_leak", "出口泄漏"), ("collector_stale", "采集停滞"), ("monitor_stale", "评估停滞"), ("correlated_node_failure", "关联故障"),
    ("renewal_due", "续费到期"), ("quota", "流量配额"), ("legacy_source_down", "数据源不可达"), ("legacy_unreachable", "数据源不可达"),
    ("komari_unreachable", "Komari 不可达"), ("channel_failing", "通知失败"),
    ("ipintel_new_isp", "新运营商"), ("ipintel_new_city", "新城市"), ("ipintel_shared_identity", "多地接入"),
    ("ipintel_datacenter_source", "机房来源"), ("ipintel_egress_ip_changed", "出口 IP 变更"),
    ("native_host_offline", "主机离线"), ("native_host_resource", "资源紧张"), ("xray_process_down", "Xray 进程退出"),
    ("xray_api_unreachable", "Xray 接口不可达"), ("relayd_silent", "relayd 无上报"), ("probe_stalled", "自测停滞"),
    // 健康判定的 reason
    ("timeout", "超时"), ("auth", "认证失败"), ("upstream_down", "上游不通"), ("policy_blocked", "规则拦截"), ("probe_offline", "探针离线"),
    ("stale", "数据过期"), ("unstable", "不稳定"), ("retrying", "重试中"), ("unknown", "未知"), ("mem", "内存"), ("disk", "磁盘"), ("cpu", "CPU"),
];

pub fn cause_label(code: &str) -> String {
    if let Some((_, l)) = CAUSE_LABELS.iter().find(|(c, _)| *c == code) {
        return l.to_string();
    }
    if let Some(r) = code.strip_prefix("ipintel_") {
        return ipintel::service::rule_title(&format!("ipintel_{r}")).unwrap_or("IP 情报").to_string();
    }
    code.to_string()
}

pub fn fmt_time(t: f64, tz: Tz) -> String {
    tz.timestamp_opt(t as i64, 0)
        .single()
        .map(|d| d.format("%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| format!("{t:.0}"))
}

pub fn fmt_duration(s: f64) -> String {
    let s = s.max(0.0) as i64;
    if s < 60 {
        format!("{s} 秒")
    } else if s < 3600 {
        format!("{} 分 {} 秒", s / 60, s % 60)
    } else if s < 86400 {
        format!("{} 小时 {} 分", s / 3600, (s % 3600) / 60)
    } else {
        format!("{} 天 {} 小时", s / 86400, (s % 86400) / 3600)
    }
}

/// 下一次摘要时刻：tz 时区里严格晚于 now 的 hour:00。
pub fn next_digest_after(now: f64, hour: u32, tz: Tz) -> f64 {
    let local = tz.timestamp_opt(now as i64, 0).single().unwrap();
    let mut date = local.date_naive();
    loop {
        let naive = date.and_hms_opt(hour, 0, 0).unwrap();
        if let Some(t) = tz.from_local_datetime(&naive).earliest() {
            let ts = t.timestamp() as f64;
            if ts > now {
                return ts;
            }
        }
        date = date.succ_opt().unwrap();
    }
}

fn evidence_text(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut parts = vec![];
            for (k, x) in m.iter() {
                if parts.len() >= 8 {
                    break;
                }
                let s = match x {
                    Value::Null => continue,
                    Value::String(s) => s.clone(),
                    Value::Array(a) if a.len() > 6 => format!("[{} 项]", a.len()),
                    Value::Object(_) => continue,
                    other => other.to_string(),
                };
                parts.push(format!("{k}={s}"));
            }
            parts.join(" ")
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

impl Engine {
    pub fn new(cfg: EngineCfg, now: f64) -> Engine {
        let next = next_digest_after(now, cfg.digest_hour, cfg.digest_tz);
        Engine {
            cfg,
            alerts: BTreeMap::new(),
            by_fp: HashMap::new(),
            groups: BTreeMap::new(),
            silences: vec![],
            next_id: 1,
            last_digest_at: now,
            next_digest_at: next,
            dirty_alerts: BTreeSet::new(),
            dirty_groups: BTreeSet::new(),
            changes: 0,
        }
    }

    /// 从数据库恢复：未结束的告警、待发恢复通知或待进摘要的告警、分组状态。
    pub fn load(&mut self, alerts: Vec<Alert>, groups: Vec<(String, GroupState)>, max_id: i64, last_digest_at: Option<f64>) {
        self.next_id = self.next_id.max(max_id + 1);
        for a in alerts {
            if a.state != State::Resolved {
                self.by_fp.insert(a.fingerprint.clone(), a.id);
            }
            self.alerts.insert(a.id, a);
        }
        for (k, g) in groups {
            self.groups.insert(k, g);
        }
        if let Some(t) = last_digest_at {
            self.last_digest_at = t;
            self.next_digest_at = next_digest_after(t, self.cfg.digest_hour, self.cfg.digest_tz);
        }
    }

    pub fn set_silences(&mut self, s: Vec<Silence>) {
        self.silences = s;
    }

    pub fn silences(&self) -> &[Silence] {
        &self.silences
    }

    pub fn get(&self, id: i64) -> Option<&Alert> {
        self.alerts.get(&id)
    }

    pub fn active(&self) -> impl Iterator<Item = &Alert> {
        self.alerts.values().filter(|a| a.state != State::Resolved)
    }

    pub fn all_in_memory(&self) -> impl Iterator<Item = &Alert> {
        self.alerts.values()
    }

    /// 取出需要落库的告警与分组（分组为 None 表示删除）。
    pub fn take_dirty(&mut self) -> (Vec<Alert>, Vec<(String, Option<GroupState>)>) {
        let ids = std::mem::take(&mut self.dirty_alerts);
        let mut out = vec![];
        for id in ids {
            if let Some(a) = self.alerts.get(&id) {
                out.push(a.clone());
            }
        }
        // 已结束且已结算的告警在落库后从内存移除
        let settled: Vec<i64> = self
            .alerts
            .values()
            .filter(|a| a.state == State::Resolved && self.settled(a))
            .map(|a| a.id)
            .collect();
        for id in settled {
            self.alerts.remove(&id);
        }
        let gk = std::mem::take(&mut self.dirty_groups);
        let groups = gk.into_iter().map(|k| { let g = self.groups.get(&k).cloned(); (k, g) }).collect();
        (out, groups)
    }

    fn settled(&self, a: &Alert) -> bool {
        let needs_resolve_note = a.notified_state == Some(State::Firing) && a.notify_resolved;
        !needs_resolve_note && a.digest_done
    }

    fn touch(&mut self, id: i64) {
        self.dirty_alerts.insert(id);
        self.changes += 1;
    }

    /// 用一次完整评估的结果更新状态。`scope` 是这次实际评估过的规则集合：不在 scope 里的规则
    /// （例如数据源不可用时的节点规则）既不新建也不恢复，保持原状。
    pub fn evaluate(&mut self, now: f64, obs: Vec<Observation>, scope: &HashSet<String>) {
        self.evaluate_held(now, obs, scope, &HashSet::new())
    }

    /// `hold`：此刻状态未知（例如所属线路机失联、旧 monitor 判 unknown）的节点短名——这些节点上已有的告警
    /// 保持原状，不因“这次没看到”而恢复（没有数据 ≠ 恢复）。
    pub fn evaluate_held(&mut self, now: f64, obs: Vec<Observation>, scope: &HashSet<String>, hold: &HashSet<String>) {
        let mut seen: HashSet<String> = HashSet::new();
        for o in obs {
            let fp = fingerprint(&o.rule, &o.labels);
            seen.insert(fp.clone());
            if let Some(&id) = self.by_fp.get(&fp) {
                let mut changed = false;
                let a = self.alerts.get_mut(&id).unwrap();
                a.last_seen = now;
                if a.severity != o.severity {
                    a.severity = o.severity;
                    changed = true;
                }
                if a.summary != o.summary {
                    a.summary = o.summary.clone();
                    changed = true;
                }
                if a.evidence != o.evidence {
                    a.evidence = o.evidence.clone();
                    // 证据变化不算状态变化，但要落库
                    self.dirty_alerts.insert(id);
                }
                if a.state == State::Pending && now - a.pending_since >= o.for_s {
                    a.state = State::Firing;
                    a.fired_at = Some(now);
                    a.digest_done = false;
                    changed = true;
                }
                if changed {
                    self.touch(id);
                }
            } else {
                let since = o.active_since.map(|s| s.min(now)).unwrap_or(now);
                let firing = now - since >= o.for_s;
                let id = self.next_id;
                self.next_id += 1;
                let a = Alert {
                    id,
                    fingerprint: fp.clone(),
                    rule: o.rule.clone(),
                    severity: o.severity,
                    state: if firing { State::Firing } else { State::Pending },
                    group_key: group_key(&o.labels, &o.rule),
                    labels: o.labels,
                    summary: o.summary,
                    evidence: o.evidence,
                    pending_since: since,
                    fired_at: if firing { Some(now) } else { None },
                    resolved_at: None,
                    last_seen: now,
                    ack_at: None,
                    ack_by: None,
                    suppressed: None,
                    notified_state: None,
                    notify_resolved: o.notify_resolved,
                    digest_done: !firing,
                };
                self.alerts.insert(id, a);
                self.by_fp.insert(fp, id);
                self.touch(id);
            }
        }
        let gone: Vec<i64> = self
            .alerts
            .values()
            .filter(|a| a.state != State::Resolved && scope.contains(&a.rule) && !seen.contains(&a.fingerprint))
            .filter(|a| !a.labels.get("node").map(|n| hold.contains(n)).unwrap_or(false))
            .map(|a| a.id)
            .collect();
        for id in gone {
            let a = self.alerts.get_mut(&id).unwrap();
            self.by_fp.remove(&a.fingerprint);
            if a.state == State::Pending {
                // 没到 for 就消失：记为结束，不通知、不进摘要
                a.state = State::Resolved;
                a.resolved_at = Some(now);
                a.digest_done = true;
            } else {
                a.state = State::Resolved;
                a.resolved_at = Some(now);
                if a.notified_state == Some(State::Firing) {
                    a.digest_done = true;
                } else {
                    // 自行恢复（或被压住期间恢复）：只进摘要
                    a.digest_done = false;
                }
            }
            self.touch(id);
        }
        self.recompute_suppression(now);
    }

    pub fn ack(&mut self, id: i64, actor: &str, now: f64) -> Result<Alert, String> {
        let a = self.alerts.get_mut(&id).ok_or_else(|| "告警不存在或已结束".to_string())?;
        if a.state == State::Resolved {
            return Err("告警已结束，无需确认".into());
        }
        if a.ack_at.is_none() {
            a.ack_at = Some(now);
            a.ack_by = Some(actor.to_string());
        }
        let out = a.clone();
        self.touch(id);
        Ok(out)
    }

    fn recompute_suppression(&mut self, now: f64) {
        let mut relay_down: HashMap<String, i64> = HashMap::new();
        let mut global_stale: Option<i64> = None;
        let mut aggregated: HashMap<String, i64> = HashMap::new();
        let mut mismatch: HashMap<String, i64> = HashMap::new();
        for a in self.alerts.values() {
            if a.state != State::Firing {
                continue;
            }
            match a.rule.as_str() {
                "egress_mismatch" => {
                    if let Some(n) = a.labels.get("node") {
                        mismatch.entry(n.clone()).or_insert(a.id);
                    }
                }
                "relay_down" => {
                    if let Some(r) = a.labels.get("relay") {
                        relay_down.insert(r.clone(), a.id);
                    }
                }
                "collector_stale" | "monitor_stale" => global_stale = Some(global_stale.unwrap_or(a.id)),
                "correlated_node_failure" => {
                    if let Some(Value::Array(members)) = a.evidence.get("members") {
                        for m in members {
                            if let Some(n) = m.get("node").and_then(|x| x.as_str()) {
                                aggregated.entry(n.to_string()).or_insert(a.id);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        let active: Vec<Silence> = self.silences.iter().filter(|s| s.active(now)).cloned().collect();
        let ids: Vec<i64> = self.alerts.keys().copied().collect();
        for id in ids {
            let a = &self.alerts[&id];
            if a.state == State::Resolved {
                continue;
            }
            let kind = a.labels.get("kind").map(String::as_str).unwrap_or("");
            let relay = a.labels.get("relay").cloned().unwrap_or_default();
            let mut s: Option<String> = None;
            if let Some(sil) = active.iter().find(|x| x.matches(&a.labels)) {
                s = Some(format!("{}:{}", sil.kind, sil.id));
            } else if a.rule != "correlated_node_failure" {
                if a.rule == "ipintel_egress_ip_changed" && a.labels.get("node").and_then(|n| mismatch.get(n)).is_some() {
                    // 同一节点的出口 IP 不一致已经在报（旧健康检查，直接证据）：情报侧的“出口 IP 变更”并入它
                    s = Some(format!("inhibited:{}", mismatch[&a.labels["node"]]));
                } else if kind == "node" {
                    if let Some(g) = global_stale {
                        s = Some(format!("inhibited:{g}"));
                    } else if let Some(r) = relay_down.get(&relay) {
                        s = Some(format!("inhibited:{r}"));
                    } else if let Some(c) = a.labels.get("node").and_then(|n| aggregated.get(n)) {
                        s = Some(format!("aggregated:{c}"));
                    }
                } else if kind == "relay" && a.rule != "relay_down" {
                    if let Some(r) = relay_down.get(&relay) {
                        s = Some(format!("inhibited:{r}"));
                    } else if a.rule == "relay_mgmt_down" {
                        if let Some(g) = global_stale {
                            s = Some(format!("inhibited:{g}"));
                        }
                    }
                }
            }
            if self.alerts[&id].suppressed != s {
                self.alerts.get_mut(&id).unwrap().suppressed = s;
                self.touch(id);
            }
        }
    }

    fn group_wait(&self, sev: Severity) -> f64 {
        if sev == Severity::Critical {
            self.cfg.group_wait_critical
        } else {
            self.cfg.group_wait
        }
    }

    /// 定时调用（每 1–5 s）：到点的分组发通知、重复提醒、每日摘要。
    pub fn tick(&mut self, now: f64) -> Vec<Outgoing> {
        self.recompute_suppression(now);
        let mut out = vec![];
        // 已通知过 firing、但规则不要恢复通知的告警：静默结算
        let silent: Vec<i64> = self
            .alerts
            .values()
            .filter(|a| a.state == State::Resolved && a.notified_state == Some(State::Firing) && !a.notify_resolved)
            .map(|a| a.id)
            .collect();
        for id in silent {
            self.alerts.get_mut(&id).unwrap().notified_state = Some(State::Resolved);
            self.touch(id);
        }
        let mut keys: BTreeSet<String> = self.groups.keys().cloned().collect();
        for a in self.alerts.values() {
            if a.severity >= Severity::Warning && a.fired_at.is_some() {
                keys.insert(a.group_key.clone());
            }
        }
        for key in keys {
            let (firing, new_firing, resolved, max_new, unacked_firing, any_open, first_change) = {
                let members: Vec<&Alert> = self
                    .alerts
                    .values()
                    .filter(|a| a.group_key == key && a.severity >= Severity::Warning && a.fired_at.is_some())
                    .collect();
                let firing: Vec<i64> = members
                    .iter()
                    .filter(|a| a.state == State::Firing && a.suppressed.is_none())
                    .map(|a| a.id)
                    .collect();
                let new_firing: Vec<i64> = members
                    .iter()
                    .filter(|a| a.state == State::Firing && a.suppressed.is_none() && a.notified_state != Some(State::Firing))
                    .map(|a| a.id)
                    .collect();
                let resolved: Vec<i64> = members
                    .iter()
                    .filter(|a| a.state == State::Resolved && a.notified_state == Some(State::Firing) && a.notify_resolved)
                    .map(|a| a.id)
                    .collect();
                let max_new = members
                    .iter()
                    .filter(|a| new_firing.contains(&a.id))
                    .map(|a| a.severity)
                    .max()
                    .unwrap_or(Severity::Warning);
                let unacked: Vec<Severity> = members
                    .iter()
                    .filter(|a| a.state == State::Firing && a.suppressed.is_none() && a.ack_at.is_none() && a.notified_state == Some(State::Firing))
                    .map(|a| a.severity)
                    .collect();
                let any_open = members.iter().any(|a| a.state != State::Resolved);
                // 组内“变化”开始的时刻：最早一条新 firing 的触发时间 / 最早一条待发恢复的恢复时间
                let first_change = members
                    .iter()
                    .filter_map(|a| {
                        if new_firing.contains(&a.id) {
                            a.fired_at
                        } else if resolved.contains(&a.id) {
                            a.resolved_at
                        } else {
                            None
                        }
                    })
                    .fold(f64::INFINITY, f64::min);
                (firing, new_firing, resolved, max_new, unacked, any_open, first_change)
            };
            let g = self.groups.get(&key).cloned().unwrap_or_default();
            let mut g2 = g.clone();
            let mut flush_kind: Option<&str> = None;
            if !new_firing.is_empty() || !resolved.is_empty() {
                let first = if first_change.is_finite() { first_change.min(now) } else { now };
                g2.first_change_at = Some(first);
                let due = if !g2.open || g2.last_flush_at.is_none() {
                    if new_firing.is_empty() {
                        first // 只有恢复：立刻发
                    } else {
                        first + self.group_wait(max_new)
                    }
                } else {
                    let base = g2.last_flush_at.unwrap() + self.cfg.group_interval;
                    base.max(first + if new_firing.is_empty() { 0.0 } else { self.group_wait(max_new) })
                };
                if now >= due {
                    flush_kind = Some("group");
                }
            } else if unacked_firing.is_empty() {
                g2.first_change_at = None;
            } else {
                g2.first_change_at = None;
                let rep = if unacked_firing.iter().any(|s| *s == Severity::Critical) {
                    self.cfg.repeat_critical
                } else {
                    self.cfg.repeat_warning
                };
                if let Some(last) = g2.last_notify_at {
                    if now >= last + rep {
                        flush_kind = Some("repeat");
                    }
                }
            }
            if let Some(kind) = flush_kind {
                let msg = self.render_group(&key, kind, &firing, &new_firing, &resolved, now);
                out.push(msg);
                for id in &firing {
                    let a = self.alerts.get_mut(id).unwrap();
                    a.notified_state = Some(State::Firing);
                    a.digest_done = true;
                }
                for id in &resolved {
                    self.alerts.get_mut(id).unwrap().notified_state = Some(State::Resolved);
                }
                for id in firing.iter().chain(resolved.iter()) {
                    self.touch(*id);
                }
                g2.last_flush_at = Some(now);
                g2.last_notify_at = Some(now);
                g2.first_change_at = None;
                g2.open = !firing.is_empty();
            }
            let pending_after = any_open || (!resolved.is_empty() && flush_kind.is_none());
            if !pending_after {
                if self.groups.remove(&key).is_some() {
                    self.dirty_groups.insert(key.clone());
                }
                continue;
            }
            if g2 != g || !self.groups.contains_key(&key) {
                self.groups.insert(key.clone(), g2);
                self.dirty_groups.insert(key.clone());
            }
        }
        if now >= self.next_digest_at {
            out.push(self.render_digest(now));
            self.last_digest_at = now;
            self.next_digest_at = next_digest_after(now, self.cfg.digest_hour, self.cfg.digest_tz);
            self.changes += 1;
        }
        out
    }

    pub fn link_for(&self, labels: &Labels) -> String {
        let base = &self.cfg.ui_base;
        if let Some(n) = labels.get("node") {
            return format!("{base}nodes/{n}");
        }
        if let Some(r) = labels.get("relay_id") {
            return format!("{base}lines/{r}");
        }
        format!("{base}alerts")
    }

    fn alert_json(&self, a: &Alert) -> Value {
        let cause = a.labels.get("cause").cloned().unwrap_or_else(|| a.rule.clone());
        json!({"id": a.id, "rule": a.rule, "rule_label": cause_label(&a.rule), "cause_label": cause_label(&cause), "severity": a.severity, "state": a.state, "labels": a.labels,
               "summary": a.summary, "evidence": a.evidence, "since": a.pending_since, "fired_at": a.fired_at,
               "resolved_at": a.resolved_at, "ack_by": a.ack_by, "link": self.link_for(&a.labels)})
    }

    fn render_group(&self, key: &str, kind: &str, firing: &[i64], new_firing: &[i64], resolved: &[i64], now: f64) -> Outgoing {
        let tz = self.cfg.digest_tz;
        let all: Vec<&Alert> = firing.iter().chain(resolved.iter()).map(|id| &self.alerts[id]).collect();
        let sev = firing.iter().map(|id| self.alerts[id].severity).max().unwrap_or(Severity::Info);
        let sev = if firing.is_empty() { Severity::Info } else { sev };
        let first = all.first().unwrap();
        let relay = first.labels.get("relay").cloned().unwrap_or_else(|| "-".into());
        let cause = first.labels.get("cause").cloned().unwrap_or_else(|| first.rule.clone());
        let head = if firing.is_empty() { "已恢复".to_string() } else { format!("[{}]", sev.word()) };
        let title = format!("{head} {} · {} · {}", rule_title(&first.rule), if relay == "-" { "全局".into() } else { relay.clone() }, cause_label(&cause));
        let mut body = String::new();
        body.push_str(&title);
        body.push('\n');
        let old = firing.len() - new_firing.len();
        let mut stat = vec![];
        if !new_firing.is_empty() {
            stat.push(format!("新增 {}", new_firing.len()));
        }
        if old > 0 {
            stat.push(format!("持续 {old}"));
        }
        if !resolved.is_empty() {
            stat.push(format!("恢复 {}", resolved.len()));
        }
        if kind == "repeat" {
            stat.push("重复提醒（未确认）".into());
        }
        body.push_str(&stat.join("，"));
        body.push('\n');
        for id in firing {
            let a = &self.alerts[id];
            body.push_str(&format!("\n● [{}] {}：{}\n", a.severity.word(), rule_title(&a.rule), a.summary));
            let ev = evidence_text(&a.evidence);
            if !ev.is_empty() {
                body.push_str(&format!("  证据：{ev}\n"));
            }
            body.push_str(&format!(
                "  开始 {} · 已持续 {}{}\n  {}\n",
                fmt_time(a.pending_since, tz),
                fmt_duration(now - a.pending_since),
                if a.ack_by.is_some() { " · 已确认" } else { "" },
                self.link_for(&a.labels)
            ));
        }
        for id in resolved {
            let a = &self.alerts[id];
            body.push_str(&format!(
                "\n✓ 已恢复 {}：{}（持续 {}）\n",
                rule_title(&a.rule),
                a.summary,
                fmt_duration(a.resolved_at.unwrap_or(now) - a.pending_since)
            ));
        }
        body.push_str(&format!("\n事项队列：{}alerts", self.cfg.ui_base));
        Outgoing {
            kind: kind.into(),
            severity: sev,
            group_key: Some(key.into()),
            title,
            link: if all.len() == 1 { self.link_for(&first.labels) } else { format!("{}alerts", self.cfg.ui_base) },
            body,
            alert_ids: all.iter().map(|a| a.id).collect(),
            alerts: all.iter().map(|a| self.alert_json(a)).collect(),
            ts: now,
        }
    }

    fn render_digest(&mut self, now: f64) -> Outgoing {
        let tz = self.cfg.digest_tz;
        let date = tz.timestamp_opt(now as i64, 0).single().map(|d| format!("{}-{:02}-{:02}", d.year(), d.month(), d.day())).unwrap_or_default();
        let firing: Vec<&Alert> = self.alerts.values().filter(|a| a.state == State::Firing).collect();
        let crit = firing.iter().filter(|a| a.severity == Severity::Critical).count();
        let warn = firing.iter().filter(|a| a.severity == Severity::Warning).count();
        let info_open: Vec<&Alert> = firing.iter().copied().filter(|a| a.severity == Severity::Info).collect();
        let items: Vec<i64> = self
            .alerts
            .values()
            .filter(|a| !a.digest_done && a.fired_at.is_some())
            .map(|a| a.id)
            .collect();
        let mut body = format!("每日摘要 {date}（{}:00 {}）\n", self.cfg.digest_hour, tz.name());
        body.push_str(&format!("当前告警：严重 {crit}，警告 {warn}，信息 {}\n", info_open.len()));
        let recovered: Vec<&Alert> = items.iter().map(|id| &self.alerts[id]).filter(|a| a.state == State::Resolved).collect();
        let infos: Vec<&Alert> = info_open.iter().copied().collect();
        if !infos.is_empty() {
            body.push_str("\n进行中的信息级事项：\n");
            for a in &infos {
                body.push_str(&format!("· {}：{}\n", rule_title(&a.rule), a.summary));
            }
        }
        if !recovered.is_empty() {
            body.push_str("\n自上次摘要以来自行恢复（未单独通知）：\n");
            for a in &recovered {
                body.push_str(&format!(
                    "· [{}] {}：{}（{} 起，持续 {}）\n",
                    a.severity.word(),
                    rule_title(&a.rule),
                    a.summary,
                    fmt_time(a.pending_since, tz),
                    fmt_duration(a.resolved_at.unwrap_or(now) - a.pending_since)
                ));
            }
        }
        if infos.is_empty() && recovered.is_empty() && crit + warn == 0 {
            body.push_str("无待处理事项。\n");
        }
        body.push_str(&format!("\n事项队列：{}alerts", self.cfg.ui_base));
        let mut ids: Vec<i64> = infos.iter().map(|a| a.id).collect();
        ids.extend(recovered.iter().map(|a| a.id));
        let alerts: Vec<Value> = ids.iter().map(|id| self.alert_json(&self.alerts[id])).collect();
        for id in &items {
            self.alerts.get_mut(id).unwrap().digest_done = true;
            self.touch(*id);
        }
        Outgoing {
            kind: "digest".into(),
            severity: Severity::Info,
            group_key: None,
            title: format!("每日摘要 {date}"),
            body,
            link: format!("{}alerts", self.cfg.ui_base),
            alert_ids: ids,
            alerts,
            ts: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lab(pairs: &[(&str, &str)]) -> Labels {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn node_obs(node: &str, relay: &str, cause: &str, sev: Severity, since: Option<f64>, for_s: f64) -> Observation {
        Observation {
            rule: if sev == Severity::Critical { "node_health_fail" } else { "node_health_degraded" }.into(),
            labels: lab(&[("kind", "node"), ("node", node), ("relay", relay), ("cause", cause)]),
            severity: sev,
            summary: format!("{node} {cause}"),
            evidence: json!({"rc": 35}),
            active_since: since,
            for_s,
            notify_resolved: true,
        }
    }

    fn scope(rules: &[&str]) -> HashSet<String> {
        rules.iter().map(|s| s.to_string()).collect()
    }

    fn all_scope() -> HashSet<String> {
        scope(&["node_health_fail", "node_health_degraded", "relay_down", "relay_mgmt_down", "correlated_node_failure",
                "collector_stale", "renewal_due", "quota", "host_offline"])
    }

    // 2026-09-24 00:00 UTC = 08:00 北京
    const T0: f64 = 1_790_208_000.0;

    fn engine() -> Engine {
        Engine::new(EngineCfg::default(), T0)
    }

    #[test]
    fn critical_group_wait_then_notify_then_resolve() {
        let mut e = engine();
        e.evaluate(T0, vec![node_obs("t1-31", "线路机2", "auth", Severity::Critical, Some(T0 - 5.0), 0.0)], &all_scope());
        let a = e.active().next().unwrap().clone();
        assert_eq!(a.state, State::Firing);
        assert!(e.tick(T0 + 5.0).is_empty(), "group_wait 10 s 未到");
        let out = e.tick(T0 + 10.0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, "group");
        assert_eq!(out[0].severity, Severity::Critical);
        assert!(out[0].body.contains("t1-31"));
        assert!(!out[0].body.contains("客户"));
        assert!(e.tick(T0 + 60.0).is_empty(), "不重复");
        // 恢复：立即发恢复通知（组已发过 firing，按 group_interval 对齐）
        e.evaluate(T0 + 120.0, vec![], &all_scope());
        assert!(e.tick(T0 + 121.0).is_empty(), "group_interval 5 min 内不发");
        let out = e.tick(T0 + 10.0 + 300.0);
        assert_eq!(out.len(), 1);
        assert!(out[0].title.starts_with("已恢复"), "{}", out[0].title);
        let (dirty, groups) = e.take_dirty();
        assert!(dirty.iter().any(|a| a.id == a.id && a.state == State::Resolved));
        assert!(groups.iter().any(|(_, g)| g.is_none()), "组在恢复后删除");
        assert_eq!(e.all_in_memory().count(), 0);
    }

    #[test]
    fn grouping_by_relay_and_cause() {
        let mut e = engine();
        let obs = vec![
            node_obs("t1-01", "线路机2", "auth", Severity::Critical, None, 0.0),
            node_obs("t1-02", "线路机2", "auth", Severity::Critical, None, 0.0),
            node_obs("t2-01", "线路机1", "auth", Severity::Critical, None, 0.0),
            node_obs("t1-03", "线路机2", "timeout", Severity::Critical, None, 0.0),
        ];
        e.evaluate(T0, obs, &all_scope());
        let out = e.tick(T0 + 10.0);
        assert_eq!(out.len(), 3, "三个组：线路机2/auth、线路机1/auth、线路机2/timeout");
        let g = out.iter().find(|o| o.group_key.as_deref() == Some("relay=线路机2|cause=auth")).unwrap();
        assert_eq!(g.alert_ids.len(), 2);
    }

    #[test]
    fn warning_uses_30s_wait_and_degraded_for_5min() {
        let mut e = engine();
        // degraded 刚开始：pending
        e.evaluate(T0, vec![node_obs("t1-21", "线路机2", "unstable", Severity::Warning, Some(T0), 300.0)], &all_scope());
        assert_eq!(e.active().next().unwrap().state, State::Pending);
        e.evaluate(T0 + 299.0, vec![node_obs("t1-21", "线路机2", "unstable", Severity::Warning, Some(T0), 300.0)], &all_scope());
        assert_eq!(e.active().next().unwrap().state, State::Pending);
        e.evaluate(T0 + 300.0, vec![node_obs("t1-21", "线路机2", "unstable", Severity::Warning, Some(T0), 300.0)], &all_scope());
        assert_eq!(e.active().next().unwrap().state, State::Firing);
        assert!(e.tick(T0 + 320.0).is_empty());
        assert_eq!(e.tick(T0 + 330.0).len(), 1);
        // 5 min 内消失的 degraded：不通知、不进摘要
        let mut e = engine();
        e.evaluate(T0, vec![node_obs("t2-01", "线路机1", "x", Severity::Warning, Some(T0), 300.0)], &all_scope());
        e.evaluate(T0 + 126.0, vec![], &all_scope());
        assert!(e.tick(T0 + 1000.0).is_empty());
        e.take_dirty();
        assert_eq!(e.all_in_memory().count(), 0);
    }

    #[test]
    fn source_since_survives_restart() {
        let mut e = engine();
        // 旧 monitor 说 6 分钟前就降级了：立即 firing
        e.evaluate(T0, vec![node_obs("t1-21", "线路机2", "unstable", Severity::Warning, Some(T0 - 360.0), 300.0)], &all_scope());
        assert_eq!(e.active().next().unwrap().state, State::Firing);
    }

    #[test]
    fn repeat_until_ack() {
        let mut e = engine();
        let o = || vec![node_obs("t1-31", "线路机2", "auth", Severity::Critical, None, 0.0)];
        e.evaluate(T0, o(), &all_scope());
        assert_eq!(e.tick(T0 + 10.0).len(), 1);
        e.evaluate(T0 + 1000.0, o(), &all_scope());
        assert!(e.tick(T0 + 1000.0).is_empty());
        let out = e.tick(T0 + 10.0 + 1800.0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, "repeat");
        let id = out[0].alert_ids[0];
        e.ack(id, "admin", T0 + 2000.0).unwrap();
        let not_digest = |v: Vec<Outgoing>| v.into_iter().filter(|o| o.kind != "digest").count();
        assert_eq!(not_digest(e.tick(T0 + 10.0 + 3600.0 + 1.0)), 0, "确认后不再重复");
        assert_eq!(not_digest(e.tick(T0 + 10.0 + 7200.0)), 0);
    }

    #[test]
    fn warning_repeat_4h() {
        let mut e = engine();
        e.evaluate(T0, vec![node_obs("t1-21", "线路机2", "unstable", Severity::Warning, Some(T0 - 400.0), 300.0)], &all_scope());
        assert_eq!(e.tick(T0 + 30.0).len(), 1);
        assert!(e.tick(T0 + 30.0 + 1800.0).is_empty());
        assert_eq!(e.tick(T0 + 30.0 + 14400.0)[0].kind, "repeat");
    }

    #[test]
    fn relay_down_inhibits_its_nodes_only() {
        let mut e = engine();
        let relay = Observation {
            rule: "relay_down".into(),
            labels: lab(&[("kind", "relay"), ("relay", "线路机2"), ("cause", "relay_down")]),
            severity: Severity::Critical,
            summary: "线路机2 主机离线且管理通道中断".into(),
            evidence: json!({}),
            active_since: None,
            for_s: 0.0,
            notify_resolved: true,
        };
        e.evaluate(
            T0,
            vec![
                relay,
                node_obs("t1-01", "线路机2", "stale", Severity::Critical, None, 0.0),
                node_obs("t2-01", "线路机1", "auth", Severity::Critical, None, 0.0),
            ],
            &all_scope(),
        );
        let out = e.tick(T0 + 10.0);
        let keys: Vec<_> = out.iter().map(|o| o.group_key.clone().unwrap()).collect();
        assert!(keys.contains(&"relay=线路机2|cause=relay_down".to_string()));
        assert!(keys.contains(&"relay=线路机1|cause=auth".to_string()));
        assert!(!keys.contains(&"relay=线路机2|cause=stale".to_string()), "被 relay_down 抑制");
        let n = e.active().find(|a| a.labels.get("node").map(String::as_str) == Some("t1-01")).unwrap();
        assert!(n.suppressed.as_deref().unwrap().starts_with("inhibited:"));
        // relay 恢复后，仍在故障的节点告警被放出来
        e.evaluate(T0 + 60.0, vec![node_obs("t1-01", "线路机2", "stale", Severity::Critical, None, 0.0), node_obs("t2-01", "线路机1", "auth", Severity::Critical, None, 0.0)], &all_scope());
        let out = e.tick(T0 + 75.0);
        assert!(out.iter().any(|o| o.group_key.as_deref() == Some("relay=线路机2|cause=stale")));
    }

    #[test]
    fn silence_and_maintenance() {
        let mut e = engine();
        e.set_silences(vec![Silence {
            id: 7,
            kind: "maintenance".into(),
            matchers: vec![Matcher { name: "relay".into(), value: "线路机1".into(), negate: false }],
            starts_at: T0 - 10.0,
            ends_at: T0 + 600.0,
            tz: "Asia/Shanghai".into(),
            comment: "换机房".into(),
            created_by: "admin".into(),
        }]);
        e.evaluate(T0, vec![node_obs("t2-01", "线路机1", "auth", Severity::Critical, None, 0.0)], &all_scope());
        assert!(e.tick(T0 + 60.0).is_empty());
        assert_eq!(e.active().next().unwrap().suppressed.as_deref(), Some("maintenance:7"));
        // 窗口结束仍在故障：发出
        e.evaluate(T0 + 600.0, vec![node_obs("t2-01", "线路机1", "auth", Severity::Critical, None, 0.0)], &all_scope());
        assert_eq!(e.tick(T0 + 611.0).len(), 1);
    }

    #[test]
    fn suppressed_then_self_resolved_goes_to_digest() {
        let mut e = engine();
        e.set_silences(vec![Silence {
            id: 1,
            kind: "silence".into(),
            matchers: vec![Matcher { name: "node".into(), value: "t1-31".into(), negate: false }],
            starts_at: T0 - 1.0,
            ends_at: T0 + 3600.0,
            tz: "UTC".into(),
            comment: String::new(),
            created_by: "a".into(),
        }]);
        e.evaluate(T0, vec![node_obs("t1-31", "线路机2", "auth", Severity::Critical, None, 0.0)], &all_scope());
        e.evaluate(T0 + 100.0, vec![], &all_scope());
        assert!(e.tick(T0 + 200.0).is_empty());
        // 09:00 北京 = T0 + 3600
        let out = e.tick(T0 + 3600.0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, "digest");
        assert!(out[0].body.contains("自行恢复"), "{}", out[0].body);
        assert_eq!(out[0].alert_ids.len(), 1);
        e.take_dirty();
        assert_eq!(e.all_in_memory().count(), 0);
    }

    #[test]
    fn info_only_in_digest_at_0900_beijing() {
        let mut e = engine();
        let o = Observation {
            rule: "renewal_due".into(),
            labels: lab(&[("kind", "renewal"), ("asset", "t1-01"), ("relay", "线路机2"), ("cause", "renewal_due"), ("threshold", "7")]),
            severity: Severity::Info,
            summary: "团队1-01 还有 7 天到期（2026-10-01）".into(),
            evidence: json!({"days": 7}),
            active_since: None,
            for_s: 0.0,
            notify_resolved: false,
        };
        e.evaluate(T0, vec![o], &all_scope());
        assert!(e.tick(T0 + 100.0).is_empty(), "info 不立即通知");
        assert_eq!(next_digest_after(T0, 9, chrono_tz::Asia::Shanghai), T0 + 3600.0);
        assert!(e.tick(T0 + 3599.0).is_empty());
        let out = e.tick(T0 + 3600.0);
        assert_eq!(out.len(), 1);
        assert!(out[0].body.contains("7 天到期"));
        // 下一次摘要在次日 09:00
        assert_eq!(e.next_digest_at, T0 + 3600.0 + 86400.0);
    }

    #[test]
    fn digest_time_is_beijing_not_utc() {
        // 2026-09-24 02:00 UTC = 10:00 北京 → 下一次是 09-25 09:00 北京 = 09-25 01:00 UTC
        let t = T0 + 2.0 * 3600.0;
        assert_eq!(next_digest_after(t, 9, chrono_tz::Asia::Shanghai), T0 + 86400.0 + 3600.0);
    }

    #[test]
    fn correlated_aggregates_members() {
        let mut e = engine();
        let corr = Observation {
            rule: "correlated_node_failure".into(),
            labels: lab(&[("kind", "correlated"), ("relay", "线路机2"), ("cause", "correlated_node_failure"), ("scope", "relay")]),
            severity: Severity::Critical,
            summary: "线路机2 名下 3 个节点在 2 分钟内相继故障".into(),
            evidence: json!({"members": [{"node": "t1-01"}, {"node": "t1-02"}, {"node": "t1-03"}]}),
            active_since: None,
            for_s: 0.0,
            notify_resolved: true,
        };
        e.evaluate(
            T0,
            vec![
                corr,
                node_obs("t1-01", "线路机2", "auth", Severity::Critical, None, 0.0),
                node_obs("t1-02", "线路机2", "timeout", Severity::Critical, None, 0.0),
                node_obs("t1-03", "线路机2", "upstream_down", Severity::Critical, None, 0.0),
            ],
            &all_scope(),
        );
        let out = e.tick(T0 + 10.0);
        assert_eq!(out.len(), 1, "只发一条关联故障");
        assert_eq!(out[0].group_key.as_deref(), Some("relay=线路机2|cause=correlated_node_failure"));
        assert!(e.active().filter(|a| a.rule != "correlated_node_failure").all(|a| a.suppressed.as_deref().unwrap_or("").starts_with("aggregated:")));
    }

    #[test]
    fn scope_limits_resolution() {
        let mut e = engine();
        e.evaluate(T0, vec![node_obs("t1-31", "线路机2", "auth", Severity::Critical, None, 0.0)], &all_scope());
        // 数据源不可用时只评估 legacy_source_down：节点告警保持
        e.evaluate(T0 + 60.0, vec![], &scope(&["legacy_source_down"]));
        assert_eq!(e.active().count(), 1);
    }

    #[test]
    fn quota_step_does_not_notify_resolved() {
        let mut e = engine();
        let q = |t: &str| Observation {
            rule: "quota".into(),
            labels: lab(&[("kind", "quota"), ("relay", "线路机1"), ("cause", "quota"), ("threshold", t)]),
            severity: Severity::Warning,
            summary: format!("线路机1 出站已用 {t}%"),
            evidence: json!({}),
            active_since: None,
            for_s: 0.0,
            notify_resolved: false,
        };
        e.evaluate(T0, vec![q("80")], &all_scope());
        assert_eq!(e.tick(T0 + 30.0).len(), 1);
        e.evaluate(T0 + 100.0, vec![q("85")], &all_scope());
        let out = e.tick(T0 + 30.0 + 300.0);
        assert_eq!(out.len(), 1);
        assert!(out[0].body.contains("85%"));
        assert!(!out[0].body.contains("已恢复"), "阈值升级不发恢复");
    }

    #[test]
    fn egress_changed_inhibited_by_mismatch_same_node() {
        let mut e = engine();
        let mk = |rule: &str, node: &str| Observation {
            rule: rule.into(),
            labels: lab(&[("kind", "node"), ("node", node), ("relay", "线路机2"), ("cause", rule)]),
            severity: Severity::Critical,
            summary: rule.into(),
            evidence: json!({}),
            active_since: None,
            for_s: 0.0,
            notify_resolved: true,
        };
        e.evaluate(T0, vec![mk("egress_mismatch", "t1-05"), mk("ipintel_egress_ip_changed", "t1-05"), mk("ipintel_egress_ip_changed", "t1-06")],
            &scope(&["egress_mismatch", "ipintel_egress_ip_changed"]));
        let out = e.tick(T0 + 10.0);
        let keys: Vec<_> = out.iter().map(|o| o.alert_ids.len()).collect();
        assert_eq!(out.len(), 2, "{keys:?}");
        let ch = e.active().find(|a| a.rule == "ipintel_egress_ip_changed" && a.labels["node"] == "t1-05").unwrap();
        assert!(ch.suppressed.as_deref().unwrap().starts_with("inhibited:"));
        let other = e.active().find(|a| a.rule == "ipintel_egress_ip_changed" && a.labels["node"] == "t1-06").unwrap();
        assert!(other.suppressed.is_none());
    }

    #[test]
    fn ack_errors() {
        let mut e = engine();
        assert!(e.ack(99, "a", T0).is_err());
    }
}
