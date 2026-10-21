//! 只读与作业类命令：status / ls / show / series / incidents / jobs / entries / audit / logs / watch …

use crate::client::Client;
use crate::resolve::{self, Want};
use crate::table::{self, bps, bytes, money};
use anyhow::{anyhow, bail};
use serde_json::{json, Value};
use std::io::Write;
use std::sync::Mutex;
use std::time::Duration;

pub struct Ctx {
    pub c: Client,
    pub base: String,
    pub json: bool,
    /// 一次命令内只拉一次的列表（`mv` 要解析 3 台机器、`restore` 要解析 2 次团队，不必每次都 GET）。
    /// 命令是一次性进程，缓存不会跨命令过期；写操作之后需要新数据的地方不要走缓存。
    machines_cache: Mutex<Option<Vec<Value>>>,
    teams_cache: Mutex<Option<Value>>,
}

/// 往 stdout 写一行，管道被关闭（如 `| head`）时不 panic
pub fn outln(s: impl AsRef<str>) {
    let mut o = std::io::stdout().lock();
    let _ = writeln!(o, "{}", s.as_ref());
}

pub fn out_raw(s: &str) {
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(s.as_bytes());
    let _ = o.flush();
}

/// 取字符串/数字/布尔字段（JSON Pointer），缺失或 null 给 "-"
pub fn st(v: &Value, ptr: &str) -> String {
    match v.pointer(ptr) {
        Some(Value::String(s)) => s.clone(),
        None | Some(Value::Null) => "-".into(),
        Some(x) => x.to_string(),
    }
}

pub fn num(v: &Value, ptr: &str) -> Option<f64> { v.pointer(ptr).and_then(|x| x.as_f64()) }

fn pct(a: Option<f64>, b: Option<f64>) -> String {
    match (a, b) {
        (Some(a), Some(b)) if b > 0.0 => format!("{:.0}%", a * 100.0 / b),
        _ => "-".into(),
    }
}

/// 计费周期：月 / 季 / 半年 / 年（同网页）
fn cycle_cn(c: &str) -> &str {
    match c {
        "monthly" => "月",
        "quarterly" => "季",
        "semiannual" => "半年",
        "yearly" => "年",
        c => c,
    }
}

/// 到期：N 天后到期 / 今天到期 / 已过期 N 天（同网页与告警）
fn due_cn(days: i64) -> String {
    match days {
        d if d < 0 => format!("已过期 {} 天", -d),
        0 => "今天到期".into(),
        d => format!("{d} 天后到期"),
    }
}

// ── 枚举的中文（同网页；ls 的列保留原值，方便当筛选参数用） ──

pub(crate) fn kind_cn(k: &str) -> &str {
    match k {
        "relay" => "线路机",
        "vps" => "落地机",
        "proxy" => "家宽代理",
        k => k,
    }
}

fn lifecycle_cn(l: &str) -> &str {
    match l {
        "draft" => "草稿",
        "provisioning" => "上架中",
        "active" => "运行中",
        "retiring" => "退役中",
        "retired" => "已退役",
        "failed" => "上架失败",
        l => l,
    }
}

fn health_cn(h: &str) -> &str {
    match h {
        "ok" => "正常",
        "degraded" => "降级",
        "down" => "故障",
        "unknown" => "未知",
        "maintenance" => "维护中",
        "inactive" => "未启用",
        h => h,
    }
}

/// 严重度：同通知里的【严重】【警告】【提示】
fn severity_cn(s: &str) -> &str {
    match s {
        "critical" => "严重",
        "warning" => "警告",
        "info" => "提示",
        s => s,
    }
}

fn job_kind_cn(k: &str) -> &str {
    match k {
        "provision_vps" => "上架落地机",
        "provision_proxy" => "上架家宽代理",
        "provision_relay" => "上架线路机",
        "attach_entry" => "新增入口",
        "move_entry" => "换线路机",
        "restore_vps" => "恢复落地机",
        "restore_proxy" => "恢复家宽代理",
        "restore_relay" => "恢复线路机",
        "retire" => "退役",
        "purge" => "彻底删除",
        "upgrade_agent" => "升级 agent",
        k => k,
    }
}

fn job_state_cn(s: &str) -> &str {
    match s {
        "queued" => "排队中",
        "running" => "进行中",
        "waiting" => "等待中",
        "cancelling" => "取消中",
        "succeeded" => "已完成",
        "failed" => "失败",
        "cancelled" => "已取消",
        s => s,
    }
}

/// 步骤状态（同网页）
fn step_state_cn(s: &str) -> &str {
    match s {
        "pending" => "等待",
        "running" => "进行中",
        "done" => "已完成",
        "skipped" => "已跳过",
        "failed" => "失败",
        "compensated" => "已补偿",
        s => s,
    }
}

/// 入口状态：active 按是否已应用细分（同网页）
pub(crate) fn entry_state_cn(e: &Value) -> &'static str {
    match e["lifecycle"].as_str().unwrap_or("") {
        "active" if e["applied"] == true => "运行中",
        "active" if e["apply_error"].is_string() => "应用失败",
        "active" | "pending" => "应用中",
        "disabled" => "已停用",
        "removing" => "移除中",
        "removed" => "已移除",
        _ => "-",
    }
}

impl Ctx {
    /// 打印：--json 时输出 data 原样，否则输出人类可读文本
    pub fn emit(&self, data: &Value, human: impl FnOnce() -> String) {
        if self.json {
            outln(serde_json::to_string_pretty(data).unwrap_or_default());
        } else {
            outln(human());
        }
    }

    pub fn new(c: Client, base: String, json: bool) -> Self {
        Self { c, base, json, machines_cache: Mutex::new(None), teams_cache: Mutex::new(None) }
    }

    /// 全部机器（含已退役）。同一命令内只请求一次
    pub fn machines(&self) -> anyhow::Result<Vec<Value>> {
        self.with_machines(|all| all.to_vec())
    }

    fn with_machines<R>(&self, f: impl FnOnce(&[Value]) -> R) -> anyhow::Result<R> {
        let mut cache = self.machines_cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.is_none() {
            *cache = Some(self.c.get_q("/machines", &[("scope", "all".to_string())])?.as_array().cloned().unwrap_or_default());
        }
        Ok(f(cache.as_deref().unwrap_or_default()))
    }

    /// 团队列表。同一命令内只请求一次
    pub fn teams(&self) -> anyhow::Result<Value> {
        let mut cache = self.teams_cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.is_none() {
            *cache = Some(self.c.get("/teams")?);
        }
        Ok(cache.clone().unwrap_or(Value::Null))
    }

    /// 解析机器引用（id、显示名、线路机名、IP）
    pub fn find(&self, q: &str, want: Want) -> anyhow::Result<Value> {
        self.with_machines(|all| resolve::machine(all, q, want).cloned().map_err(|e| anyhow!(e)))?
    }

    pub fn find_id(&self, q: &str, want: Want) -> anyhow::Result<(String, String)> {
        let m = self.find(q, want)?;
        Ok((st(&m, "/id"), st(&m, "/name")))
    }
}

pub fn whoami(ctx: &Ctx) -> anyhow::Result<u8> {
    let d = ctx.c.get("/auth/me")?;
    ctx.emit(&d, || {
        let scopes = d["scopes"].as_array().map(|a| a.iter().filter_map(|s| s.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default();
        table::kv(&[("身份", st(&d, "/username")), ("类型", st(&d, "/kind")), ("权限", scopes), ("中控", ctx.base.clone())])
    });
    Ok(0)
}

/// 分页接口（`/jobs`、`/incidents`）返回 `{items, total, next_offset}`：命令只要条目，`--json` 输出仍是数组
fn items(mut v: Value) -> anyhow::Result<Value> { v.get_mut("items").map(Value::take).ok_or_else(|| anyhow!("分页响应里没有 items")) }

pub fn status(ctx: &Ctx) -> anyhow::Result<u8> {
    let ov = ctx.c.get("/overview")?;
    let inc = items(ctx.c.get_q("/incidents", &[("state", "firing".to_string()), ("limit", "500".to_string())])?)?;
    let both = json!({"overview": ov, "incidents": inc});
    ctx.emit(&both, || {
        let bucket = |k: &str| {
            let n = |f: &str| ov.pointer(&format!("/{k}/{f}")).and_then(|x| x.as_i64()).unwrap_or(0);
            format!("共 {} · 正常 {} · 降级 {} · 故障 {} · 未知 {} · 维护 {}", n("total"), n("ok"), n("degraded"), n("down"), n("unknown"), n("maintenance"))
        };
        let cost = ov["monthly_cost"].as_array().map(|a| a.iter().map(|c| money(c["cents"].as_i64().unwrap_or(0), c["currency"].as_str().unwrap_or(""))).collect::<Vec<_>>().join(" + ")).unwrap_or_default();
        let mut s = table::kv(&[
            ("节点", bucket("nodes")),
            ("线路机", bucket("relays")),
            ("agent", format!("在线 {} · 离线 {}", st(&ov, "/online"), st(&ov, "/offline"))),
            ("上架中/退役中", format!("{} / {}", st(&ov, "/provisioning"), st(&ov, "/retiring"))),
            ("7 天内到期", st(&ov, "/due_7d")),
            ("每月费用", if cost.is_empty() { "-".into() } else { cost }),
            ("本月业务流量", format!("下载 {} / 上传 {}", bytes(num(&ov, "/traffic_month/down").unwrap_or(0.0)), bytes(num(&ov, "/traffic_month/up").unwrap_or(0.0)))),
            ("网卡速率", format!("下行 {} / 上行 {}", bps(num(&ov, "/net_rx_bps").unwrap_or(0.0)), bps(num(&ov, "/net_tx_bps").unwrap_or(0.0)))),
        ]);
        let list = inc.as_array().cloned().unwrap_or_default();
        s.push_str(&format!("\n\n告警（进行中 {}）", list.len()));
        if !list.is_empty() {
            s.push('\n');
            s.push_str(&incident_table(&list));
        }
        s
    });
    Ok(0)
}

fn incident_table(list: &[Value]) -> String {
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|i| {
            vec![
                severity_cn(&st(i, "/severity")).to_string(),
                st(i, "/rule_name"),
                if i.get("subject_name").is_some() { st(i, "/subject_name") } else { st(i, "/subject/name") },
                match i["reopen_count"].as_i64() {
                    Some(n) if n > 0 => format!("{}（反复 {} 次）", st(i, "/summary"), n + 1),
                    _ => st(i, "/summary"),
                },
                table::ts_cn(&st(i, "/opened_at")),
                if i["ack_at"].is_null() { "".into() } else { format!("已确认 {}", st(i, "/ack_by")) },
                st(i, "/id"),
            ]
        })
        .collect();
    table::render(&["级别", "规则", "对象", "摘要", "开始", "确认", "id"], &rows)
}

pub fn ls(ctx: &Ctx, team: Option<&str>, kind: Option<&str>, status: Option<&str>, retired: bool) -> anyhow::Result<u8> {
    let scope = if retired { "retired" } else { "live" };
    let all = ctx.c.get_q("/machines", &[("scope", scope.to_string())])?;
    let list: Vec<Value> = all
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|m| team.is_none_or(|t| st(m, "/team/name") == t))
        .filter(|m| kind.is_none_or(|k| st(m, "/kind") == k))
        .filter(|m| status.is_none_or(|s| st(m, "/health/status") == s || st(m, "/lifecycle") == s))
        .collect();
    ctx.emit(&Value::Array(list.clone()), || {
        let rows: Vec<Vec<String>> = list
            .iter()
            .map(|m| {
                let agent = match m.pointer("/agent/online").and_then(|x| x.as_bool()) {
                    Some(true) => "在线",
                    Some(false) => "离线",
                    None => "-",
                };
                vec![
                    st(m, "/name"),
                    st(m, "/kind"),
                    st(m, "/lifecycle"),
                    st(m, "/health/status"),
                    st(m, "/ip"),
                    agent.into(),
                    num(m, "/host/cpu_pct").map(|x| format!("{x:.0}%")).unwrap_or("-".into()),
                    pct(num(m, "/host/mem_used"), num(m, "/host/mem_total")),
                    pct(num(m, "/host/disk_used"), num(m, "/host/disk_total")),
                    m["entries"].as_array().map(|a| a.len().to_string()).unwrap_or("-".into()),
                    st(m, "/open_incidents"),
                    match m.pointer("/active_job/state") {
                        Some(Value::String(s)) => format!("{} {}", job_kind_cn(&st(m, "/active_job/kind")), job_state_cn(s)),
                        _ => "".into(),
                    },
                ]
            })
            .collect();
        format!("{}\n共 {} 台", table::render(&["名称", "种类", "生命周期", "健康", "IP", "agent", "CPU", "内存", "磁盘", "入口", "告警", "作业"], &rows), list.len())
    });
    Ok(0)
}

/// 负载同网页：1 分钟平均负载 ÷ agent 报的核数，折成百分比；容器里不报负载（null）
fn load_cn(load1: Option<f64>, cores: Option<i64>) -> String {
    match (load1, cores) {
        (None, _) => "-".into(),
        (Some(l), Some(c)) if c > 0 => format!("{:.0}%（1 分钟 {l:.2} · {c} 核）", l / c as f64 * 100.0),
        (Some(l), _) => format!("1 分钟 {l:.2}"),
    }
}

pub fn show(ctx: &Ctx, machine: &str) -> anyhow::Result<u8> {
    let (id, _) = ctx.find_id(machine, Want::Any)?;
    let m = ctx.c.get(&format!("/machines/{id}"))?;
    ctx.emit(&m, || {
        let mut rows = vec![
            ("名称", st(&m, "/name")),
            ("id", st(&m, "/id")),
            ("种类", kind_cn(&st(&m, "/kind")).to_string()),
            ("生命周期", format!("{}{}", lifecycle_cn(&st(&m, "/lifecycle")), m["lifecycle_reason"].as_str().map(|r| format!("（{r}）")).unwrap_or_default())),
            ("健康", health_cn(&st(&m, "/health/status")).to_string()),
            ("IP", format!("{}:{}", st(&m, "/ip"), st(&m, "/ssh_port"))),
            ("供应商/套餐", format!("{} / {}", st(&m, "/provider/name"), st(&m, "/plan/name"))),
            ("备注", st(&m, "/note")),
            ("创建", table::ts_cn(&st(&m, "/created_at"))),
            ("revision", st(&m, "/revision")),
        ];
        if !m["agent"].is_null() {
            rows.push(("agent", format!("{}  版本 {}  最近上报 {}", if m["agent"]["online"] == true { "在线" } else { "离线" }, st(&m, "/agent/version"), table::ts_cn(&st(&m, "/agent/last_report_at")))));
        }
        if !m["host"].is_null() {
            rows.push((
                "主机",
                format!(
                    "CPU {}%  内存 {}  磁盘 {}  负载 {}  连接 {}  网卡 下行 {} / 上行 {}",
                    num(&m, "/host/cpu_pct").map(|x| format!("{x:.0}")).unwrap_or("-".into()),
                    pct(num(&m, "/host/mem_used"), num(&m, "/host/mem_total")),
                    pct(num(&m, "/host/disk_used"), num(&m, "/host/disk_total")),
                    load_cn(num(&m, "/host/load1"), m.pointer("/agent/cores").and_then(|x| x.as_i64())),
                    st(&m, "/host/tcp_conns"),
                    bps(num(&m, "/host/net_rx_bps").unwrap_or(0.0)),
                    bps(num(&m, "/host/net_tx_bps").unwrap_or(0.0)),
                ),
            ));
        }
        if !m["subscription"].is_null() {
            rows.push((
                "订阅",
                format!(
                    "{}（每{}）到期 {}（{}）",
                    money(m["subscription"]["price_cents"].as_i64().unwrap_or(0), &st(&m, "/subscription/currency")),
                    cycle_cn(&st(&m, "/subscription/cycle")),
                    st(&m, "/subscription/due_date"),
                    due_cn(m["subscription"]["due_days"].as_i64().unwrap_or(0))
                ),
            ));
        }
        rows.push(("流量额度", format!("本周期已用 {}（{} 起）", bytes(num(&m, "/quota/used").unwrap_or(0.0)), st(&m, "/quota/period_start"))));
        if !m["relay"].is_null() {
            rows.push(("线路机", format!("{}  配置代数 {} · 已应用 {}  {}", st(&m, "/relay/public_host"), st(&m, "/relay/generation"), st(&m, "/relay/applied_generation"), m["relay"]["apply_error"].as_str().unwrap_or(""))));
        }
        if !m["active_job"].is_null() {
            rows.push(("进行中作业", format!("{} · {}  {}", job_kind_cn(&st(&m, "/active_job/kind")), job_state_cn(&st(&m, "/active_job/state")), st(&m, "/active_job/id"))));
        }
        if !m["failed_job"].is_null() {
            rows.push(("失败作业", format!("{}  {}（fleetctl job 查看，可重试）", job_kind_cn(&st(&m, "/failed_job/kind")), st(&m, "/failed_job/id"))));
        }
        let mut s = table::kv(&rows);
        if let Some(rs) = m.pointer("/health/reasons").and_then(|r| r.as_array()).filter(|r| !r.is_empty()) {
            s.push_str("\n\n健康原因");
            for r in rs {
                s.push_str(&format!("\n  [{}] {}  {}", severity_cn(&st(r, "/severity")), st(r, "/rule_name"), st(r, "/text")));
            }
        }
        if let Some(es) = m["entries"].as_array().filter(|e| !e.is_empty()) {
            s.push_str("\n\n入口\n");
            s.push_str(&entry_table(es));
        }
        s
    });
    Ok(0)
}

fn entry_table(es: &[Value]) -> String {
    let rows: Vec<Vec<String>> = es
        .iter()
        .map(|e| {
            let probe = |k: &str| match e.pointer(&format!("/{k}/ok")).and_then(|x| x.as_bool()) {
                Some(true) => "通".to_string(),
                Some(false) => "不通".to_string(),
                None => "-".to_string(),
            };
            vec![
                st(e, "/id").chars().take(8).collect(),
                format!("{}:{}", st(e, "/relay/name"), st(e, "/port")),
                st(e, "/node/name"),
                entry_state_cn(e).into(),
                probe("tcp"),
                probe("egress"),
                format!("下载 {} / 上传 {}", bytes(num(e, "/traffic/today_down").unwrap_or(0.0)), bytes(num(e, "/traffic/today_up").unwrap_or(0.0))),
                e["apply_error"].as_str().unwrap_or("").to_string(),
            ]
        })
        .collect();
    table::render(&["id", "入口", "节点", "状态", "TCP", "出口", "今日业务流量", "错误"], &rows)
}

/// 全部入口（取线路机上挂的，避免节点和线路机两边重复）
pub fn all_entries(ctx: &Ctx) -> anyhow::Result<Vec<Value>> {
    let ms = ctx.c.get_q("/machines", &[("scope", "live".to_string())])?;
    Ok(ms.as_array().cloned().unwrap_or_default().into_iter().filter(|m| m["kind"] == "relay").flat_map(|m| m["entries"].as_array().cloned().unwrap_or_default()).collect())
}

pub fn entries(ctx: &Ctx, machine: Option<&str>) -> anyhow::Result<u8> {
    let mut es = all_entries(ctx)?;
    if let Some(q) = machine {
        let m = ctx.find(q, Want::Live)?;
        let id = st(&m, "/id");
        es.retain(|e| st(e, "/relay/id") == id || st(e, "/node/id") == id);
    }
    ctx.emit(&Value::Array(es.clone()), || format!("{}\n共 {} 个入口", entry_table(&es), es.len()));
    Ok(0)
}

pub fn series(ctx: &Ctx, machine: &str, range: &str, metric: &str) -> anyhow::Result<u8> {
    let (id, name) = ctx.find_id(machine, Want::Live)?;
    let d = ctx.c.get_q(&format!("/machines/{id}/series"), &[("metric", metric.to_string()), ("range", range.to_string())])?;
    ctx.emit(&d, || {
        let pts = d["points"].as_array().cloned().unwrap_or_default();
        let cols: &[&str] = match metric {
            "traffic" => &["up", "down", "conns"],
            "probe" => &["tcp_ok", "tcp_ms", "egress_ok", "egress_ms"],
            _ => &["cpu_pct", "mem_pct", "disk_pct", "load1", "net_rx_bps", "net_tx_bps"],
        };
        let fmt_n = |c: &str, n: f64| -> String {
            if c.ends_with("_bps") { bps(n) } else if c == "up" || c == "down" { bytes(n) } else if c.ends_with("_pct") { format!("{n:.0}%") } else { format!("{n:.2}") }
        };
        let fmt = |c: &str, p: &Value| -> String {
            match p.get(c) {
                None | Some(Value::Null) => "-".into(),
                Some(Value::Bool(b)) => if *b { "通" } else { "不通" }.into(),
                Some(x) => fmt_n(c, x.as_f64().unwrap_or(0.0)),
            }
        };
        let mut head = format!("{name}  {metric}  {range}  分辨率 {}  共 {} 点", st(&d, "/resolution"), pts.len());
        if pts.is_empty() {
            return head + "\n（没有数据）";
        }
        // 汇总：数值列的平均与最大
        let mut sum_rows = vec![];
        for c in cols {
            let vals: Vec<f64> = pts.iter().filter_map(|p| p.get(*c).and_then(|x| x.as_f64())).collect();
            if vals.is_empty() {
                continue;
            }
            let avg = vals.iter().sum::<f64>() / vals.len() as f64;
            let max = vals.iter().cloned().fold(f64::MIN, f64::max);
            sum_rows.push(vec![c.to_string(), fmt_n(c, avg), fmt_n(c, max), fmt_n(c, vals[vals.len() - 1])]);
        }
        head.push_str("\n\n");
        head.push_str(&table::render(&["指标", "平均", "最大", "最新"], &sum_rows));
        // 均匀抽样最多 12 行
        let step = pts.len().div_ceil(12).max(1);
        let sampled: Vec<&Value> = pts.iter().step_by(step).collect();
        let rows: Vec<Vec<String>> = sampled.iter().map(|p| std::iter::once(table::ts(&st(p, "/ts"))).chain(cols.iter().map(|c| fmt(c, p))).collect()).collect();
        let mut hdr = vec!["时间(UTC)"];
        hdr.extend(cols.iter());
        head.push_str("\n\n");
        head.push_str(&table::render(&hdr, &rows));
        head
    });
    Ok(0)
}

pub fn incidents(ctx: &Ctx, all: bool) -> anyhow::Result<u8> {
    let d = items(ctx.c.get_q("/incidents", &[("state", if all { "all" } else { "firing" }.to_string()), ("limit", "500".to_string())])?)?;
    ctx.emit(&d, || {
        let list = d.as_array().cloned().unwrap_or_default();
        if list.is_empty() { "没有告警".to_string() } else { format!("{}\n共 {} 条", incident_table(&list), list.len()) }
    });
    Ok(0)
}

pub fn ack(ctx: &Ctx, id: &str) -> anyhow::Result<u8> {
    let d = ctx.c.post(&format!("/incidents/{id}/ack"), &json!({}))?;
    ctx.emit(&d, || format!("已确认告警 {id}"));
    Ok(0)
}

fn job_row(j: &Value) -> Vec<String> {
    vec![
        st(j, "/id"),
        job_kind_cn(&st(j, "/kind")).to_string(),
        st(j, "/target/name"),
        job_state_cn(&st(j, "/state")).to_string(),
        format!("{}/{}", st(j, "/step"), st(j, "/step_count")),
        st(j, "/created_by"),
        table::ts(&st(j, "/created_at")),
        j["error"].as_str().unwrap_or("").to_string(),
    ]
}

pub fn jobs(ctx: &Ctx, active: bool, limit: i64) -> anyhow::Result<u8> {
    let d = items(ctx.c.get_q("/jobs", &[("state", if active { "active" } else { "all" }.to_string()), ("limit", limit.to_string())])?)?;
    ctx.emit(&d, || {
        let list = d.as_array().cloned().unwrap_or_default();
        if list.is_empty() {
            return "没有作业".to_string();
        }
        let rows: Vec<Vec<String>> = list.iter().map(job_row).collect();
        format!("{}\n共 {} 个", table::render(&["id", "种类", "目标", "状态", "步骤", "创建者", "创建(UTC)", "错误"], &rows), rows.len())
    });
    Ok(0)
}

/// 作业 id：完整 UUID 直接用；否则当前缀在最近作业里找
pub fn job_id(ctx: &Ctx, q: &str) -> anyhow::Result<String> {
    if q.len() == 36 {
        return Ok(q.to_string());
    }
    let list = items(ctx.c.get_q("/jobs", &[("state", "all".to_string()), ("limit", "200".to_string())])?)?;
    let hits: Vec<String> = list.as_array().cloned().unwrap_or_default().iter().map(|j| st(j, "/id")).filter(|id| id.starts_with(&q.to_lowercase())).collect();
    match hits.len() {
        1 => Ok(hits[0].clone()),
        0 => bail!("最近 200 个作业里没有 id 以「{q}」开头的"),
        n => bail!("「{q}」匹配到 {n} 个作业，请给更长的 id"),
    }
}

fn log_line(l: &Value) -> String { format!("{}  {:<5} {}", table::ts(&st(l, "/ts")), st(l, "/level"), st(l, "/msg")) }

pub fn job(ctx: &Ctx, id: &str, follow: bool) -> anyhow::Result<u8> {
    let id = job_id(ctx, id)?;
    if follow {
        return follow_job(ctx, &id);
    }
    let mut j = ctx.c.get(&format!("/jobs/{id}"))?;
    let logs = ctx.c.get(&format!("/jobs/{id}/logs"))?;
    j["logs"] = logs.clone();
    ctx.emit(&j, || {
        let mut s = table::kv(&[
            ("作业", st(&j, "/id")),
            ("种类", job_kind_cn(&st(&j, "/kind")).to_string()),
            ("目标", st(&j, "/target/name")),
            ("状态", format!("{}  步骤 {}/{}", job_state_cn(&st(&j, "/state")), st(&j, "/step"), st(&j, "/step_count"))),
            ("创建", format!("{}  {}", table::ts(&st(&j, "/created_at")), st(&j, "/created_by"))),
            ("错误", st(&j, "/error")),
        ]);
        if let Some(steps) = j["steps"].as_array() {
            let rows: Vec<Vec<String>> = steps.iter().map(|x| vec![st(x, "/idx"), st(x, "/title"), step_state_cn(&st(x, "/state")).to_string(), x["error"].as_str().unwrap_or("").to_string()]).collect();
            s.push_str("\n\n");
            s.push_str(&table::render(&["#", "步骤", "状态", "错误"], &rows));
        }
        let ls = logs.as_array().cloned().unwrap_or_default();
        if !ls.is_empty() {
            s.push_str("\n\n日志\n");
            s.push_str(&ls.iter().map(log_line).collect::<Vec<_>>().join("\n"));
        }
        s
    });
    Ok(if j["state"] == "failed" { 1 } else { 0 })
}

/// follow 时连续这么多次取进度失败才放弃
const FOLLOW_MAX_ERRORS: u32 = 3;

/// 值得重试的失败：网络错误、5xx、408/429；其余 4xx（令牌无权限、作业不存在…）重试没用，直接报
fn follow_error_is_transient(e: &anyhow::Error) -> bool {
    match e.downcast_ref::<crate::client::ApiError>() {
        Some(a) => a.status >= 500 || matches!(a.status, 408 | 429),
        None => true,
    }
}

/// 轮询作业日志直到结束；成功退出码 0，失败/取消为 1
pub fn follow_job(ctx: &Ctx, id: &str) -> anyhow::Result<u8> {
    let mut after = 0i64;
    let mut last = String::new();
    let mut errs = 0u32;
    loop {
        let polled = ctx.c.get(&format!("/jobs/{id}")).and_then(|j| Ok((j, ctx.c.get_q(&format!("/jobs/{id}/logs"), &[("after", after.to_string())])?)));
        let (j, logs) = match polled {
            Ok(x) => {
                errs = 0;
                x
            }
            Err(e) => {
                // 作业在服务端照常跑，一次网络抖动不该让命令退出、让用户以为作业没了
                errs += 1;
                if !follow_error_is_transient(&e) || errs >= FOLLOW_MAX_ERRORS {
                    return Err(e.context(format!("取作业进度失败（连续 {errs} 次）；作业在服务端不受影响，可稍后用 fleetctl job {id} --follow 继续看")));
                }
                eprintln!("取作业进度失败（第 {errs}/{FOLLOW_MAX_ERRORS} 次），稍后重试：{e:#}");
                std::thread::sleep(Duration::from_secs(2 * errs as u64));
                continue;
            }
        };
        for l in logs.as_array().cloned().unwrap_or_default() {
            after = after.max(l["id"].as_i64().unwrap_or(0));
            if ctx.json {
                outln(json!({"log": l}).to_string());
            } else {
                outln(log_line(&l));
            }
        }
        let state = st(&j, "/state");
        if state != last {
            if !ctx.json {
                outln(format!("── 状态 {}  步骤 {}/{}", job_state_cn(&state), st(&j, "/step"), st(&j, "/step_count")));
            }
            last = state.clone();
        }
        if matches!(state.as_str(), "succeeded" | "failed" | "cancelled") {
            if ctx.json {
                outln(json!({"job": j}).to_string());
            } else if state != "succeeded" {
                outln(format!("作业{}：{}", if state == "failed" { "失败" } else { "已取消" }, st(&j, "/error")));
            }
            return Ok(if state == "succeeded" { 0 } else { 1 });
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

pub fn job_cancel(ctx: &Ctx, id: &str) -> anyhow::Result<u8> {
    let id = job_id(ctx, id)?;
    let d = ctx.c.post(&format!("/jobs/{id}/cancel"), &json!({}))?;
    ctx.emit(&d, || format!("已取消作业 {id}"));
    Ok(0)
}

pub fn job_retry(ctx: &Ctx, id: &str, reset_host_key: bool, password_env: Option<&str>) -> anyhow::Result<u8> {
    let id = job_id(ctx, id)?;
    let mut body = json!({});
    if reset_host_key {
        body["reset_host_key"] = json!(true);
    }
    if let Some(var) = password_env {
        let pw = std::env::var(var).map_err(|_| anyhow!("环境变量 {var} 没有设置"))?;
        if pw.is_empty() {
            bail!("环境变量 {var} 是空的");
        }
        body["ssh_password"] = json!(pw);
    }
    let d = ctx.c.post(&format!("/jobs/{id}/retry"), &body)?;
    ctx.emit(&d, || format!("已重试作业 {id}"));
    Ok(0)
}

pub fn audit(ctx: &Ctx, target: Option<&str>, limit: i64) -> anyhow::Result<u8> {
    let mut q = vec![("limit", limit.to_string())];
    if let Some(t) = target {
        let (id, _) = ctx.find_id(t, Want::Any)?;
        q.push(("target_id", id));
    }
    let d = ctx.c.get_q("/audit", &q)?;
    ctx.emit(&d, || {
        let rows: Vec<Vec<String>> = d.as_array().cloned().unwrap_or_default().iter().map(|a| vec![table::ts(&st(a, "/ts")), st(a, "/actor"), st(a, "/title"), st(a, "/summary")]).collect();
        if rows.is_empty() { "没有审计记录".into() } else { table::render(&["时间(UTC)", "操作者", "操作", "摘要"], &rows) }
    });
    Ok(0)
}

/// 日志类接口返回 {exit_code, stdout, stderr}：stdout 原样输出，stderr 走 stderr
fn print_text_result(ctx: &Ctx, d: &Value) -> u8 {
    if ctx.json {
        outln(serde_json::to_string_pretty(d).unwrap_or_default());
    } else {
        out_raw(d["stdout"].as_str().unwrap_or(""));
        let e = d["stderr"].as_str().unwrap_or("");
        if !e.is_empty() {
            eprint!("{e}");
        }
        if d["truncated"] == true {
            eprintln!("\n[输出被截断，只显示了前 256 KiB]");
        }
    }
    if d["exit_code"].as_i64().unwrap_or(0) == 0 { 0 } else { 1 }
}

pub fn logs(ctx: &Ctx, machine: &str, unit: &str, lines: u32) -> anyhow::Result<u8> {
    let (id, _) = ctx.find_id(machine, Want::Live)?;
    let d = ctx.c.get_q(&format!("/machines/{id}/logs"), &[("unit", unit.to_string()), ("lines", lines.to_string())])?;
    Ok(print_text_result(ctx, &d))
}

pub fn server_logs(ctx: &Ctx, lines: u32) -> anyhow::Result<u8> {
    let d = ctx.c.get_q("/server/logs", &[("lines", lines.to_string())])?;
    Ok(print_text_result(ctx, &d))
}

fn utc_hms() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("{:02}:{:02}:{:02}", (secs / 3600) % 24, (secs / 60) % 60, secs % 60)
}

/// 文本模式的一行摘要：metrics 只报条数，其余列出字段
fn event_summary(ev: &Value) -> String {
    let Some(o) = ev.as_object() else { return String::new() };
    if ev["type"] == "metrics" {
        let n = |k: &str| o.get(k).and_then(Value::as_object).map_or(0, |m| m.len());
        return format!("machines={} entries={}{}", n("machines"), n("entries"), if o.contains_key("overview") { " overview" } else { "" });
    }
    o.iter().filter(|(k, _)| *k != "type").map(|(k, v)| format!("{k}={}", v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))).collect::<Vec<_>>().join(" ")
}

/// 订阅实时流（WebSocket /stream），逐条打印消息（--json 为每行一个 JSON）。断线自动重连；
/// 会话/密钥失效（关闭码 4401，或重连时 401/403）直接报错退出，不再重连。
pub fn watch(ctx: &Ctx, count: Option<usize>, metrics: bool) -> anyhow::Result<u8> {
    let mut seen = 0usize;
    let mut first = true;
    loop {
        match ctx.c.ws("/stream") {
            Ok(mut ws) => {
                first = false;
                loop {
                    let text = match ws.read() {
                        Ok(tungstenite::Message::Text(t)) => t,
                        Ok(tungstenite::Message::Close(f)) => {
                            if let Some(f) = f {
                                // 4401 = 中控复核发现会话/密钥已失效（fleet-server CLOSE_REVOKED）
                                if u16::from(f.code) == 4401 {
                                    bail!("中控关闭了实时流：{}（4401），请重新登录或换有效的 API 密钥", f.reason);
                                }
                                eprintln!("中控关闭了实时流：{}（{}）", f.reason, u16::from(f.code));
                            }
                            break;
                        }
                        Ok(_) => continue,
                        Err(_) => break,
                    };
                    let Ok(ev) = serde_json::from_str::<Value>(&text) else { continue };
                    if ev["type"] == "metrics" && !metrics {
                        continue;
                    }
                    if ctx.json {
                        outln(ev.to_string());
                    } else {
                        outln(format!("{}  {:<10} {}", utc_hms(), st(&ev, "/type"), event_summary(&ev)));
                    }
                    seen += 1;
                    if count.is_some_and(|c| seen >= c) {
                        return Ok(0);
                    }
                }
                eprintln!("实时流断开，3 秒后重连…");
            }
            Err(e) if first || e.downcast_ref::<crate::client::ApiError>().is_some_and(|a| matches!(a.status, 401 | 403)) => return Err(e),
            Err(e) => eprintln!("重连失败：{e}"),
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ApiError;

    fn api(status: u16) -> anyhow::Error {
        ApiError { status, code: "x".into(), message: "m".into() }.into()
    }

    #[test]
    fn follow_retries_only_transient_failures() {
        assert!(follow_error_is_transient(&anyhow!("连不上中控 http://x：timeout")));
        assert!(follow_error_is_transient(&api(502)));
        assert!(follow_error_is_transient(&api(429)));
        assert!(!follow_error_is_transient(&api(401)));
        assert!(!follow_error_is_transient(&api(404)));
        // 外面包了 context 也认得出底下的 ApiError
        assert!(!follow_error_is_transient(&api(403).context("读取")));
        assert!(follow_error_is_transient(&api(503).context("读取")));
    }

    #[test]
    fn due_and_cycle_wording() {
        assert_eq!((due_cn(3), due_cn(0), due_cn(-2)), ("3 天后到期".to_string(), "今天到期".to_string(), "已过期 2 天".to_string()));
        assert_eq!((cycle_cn("monthly"), cycle_cn("semiannual"), cycle_cn("yearly")), ("月", "半年", "年"));
    }

    #[test]
    fn load_as_percent_of_cores() {
        assert_eq!(load_cn(Some(0.5), Some(2)), "25%（1 分钟 0.50 · 2 核）");
        assert_eq!(load_cn(Some(0.5), None), "1 分钟 0.50");
        assert_eq!(load_cn(None, Some(2)), "-");
    }

    #[test]
    fn ctx_caches_are_empty_until_used() {
        let ctx = Ctx::new(Client::new("http://127.0.0.1:1", crate::client::Cred::None).unwrap(), "http://127.0.0.1:1".into(), false);
        assert!(ctx.machines_cache.lock().unwrap().is_none() && ctx.teams_cache.lock().unwrap().is_none());
    }
}
