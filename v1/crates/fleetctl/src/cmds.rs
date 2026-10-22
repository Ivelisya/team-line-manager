//! 只读与作业类命令：status / ls / show / series / uptime / incidents / jobs / entries / audit / logs / watch …

use crate::client::{ApiError, Client, NetError};
use crate::meta;
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

/// `--json` 下的错误对象 `{"code","message","status"}`：stderr 的 `{"error": …}` 与 exec 多台时每台的 error 共用。
/// 没拿到中控响应时 code 是 `unreachable` / `timeout`（见 `NetError`），status 为 null
pub fn error_json(e: &anyhow::Error) -> Value {
    if let Some(a) = e.downcast_ref::<ApiError>() {
        let msg = if e.to_string().contains('\n') { format!("{e:#}") } else { a.message.clone() };
        return json!({"code": a.code, "message": msg, "status": a.status});
    }
    let code = e.downcast_ref::<NetError>().map_or("error", |n| n.code);
    json!({"code": code, "message": format!("{e:#}"), "status": null})
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

/// 计费周期：月 / 季 / 半年 / 年（中控 /meta）
pub(crate) fn cycle_cn(c: &str) -> &str { meta::cycle(c) }

/// 到期：N 天后到期 / 今天到期 / 已过期 N 天（同网页与告警）
pub(crate) fn due_cn(days: i64) -> String {
    match days {
        d if d < 0 => format!("已过期 {} 天", -d),
        0 => "今天到期".into(),
        d => format!("{d} 天后到期"),
    }
}

// ── 枚举的中文：取中控 /meta（网页同一份）；ls 的列保留原值，方便当筛选参数用 ──

pub(crate) fn kind_cn(k: &str) -> &str { meta::label("kinds", k) }
fn lifecycle_cn(l: &str) -> &str { meta::label("lifecycles", l) }
fn health_cn(h: &str) -> &str { meta::label("healths", h) }
/// 严重度：同通知里的【严重】【警告】【提示】
fn severity_cn(s: &str) -> &str { meta::severity(s) }
fn job_kind_cn(k: &str) -> &str { meta::label("job_kinds", k) }
fn job_state_cn(s: &str) -> &str { meta::label("job_states", s) }
fn step_state_cn(s: &str) -> &str { meta::label("step_states", s) }

/// 入口状态：active 按是否已应用细分（同网页）——没应用上的按 pending（应用中），应用报错的是「应用失败」
pub(crate) fn entry_state_cn(e: &Value) -> &str {
    match e["lifecycle"].as_str().unwrap_or("") {
        "active" if e["applied"] != true && e["apply_error"].is_string() => "应用失败",
        "active" if e["applied"] != true => meta::label("entry_states", "pending"),
        "" => "-",
        l => meta::label("entry_states", l),
    }
}

impl Ctx {
    /// 打印：--json 时输出 data 原样（紧凑一行：AI 工具按字符数截断输出，缩进和换行白占字符），否则输出人类可读文本
    pub fn emit(&self, data: &Value, human: impl FnOnce() -> String) {
        if self.json {
            outln(data.to_string());
        } else {
            outln(human());
        }
    }

    pub fn new(c: Client, base: String, json: bool) -> Self {
        Self { c, base, json, machines_cache: Mutex::new(None), teams_cache: Mutex::new(None) }
    }

    /// 全部机器（含已退役）的精简列表：id、kind、name、label、ip、lifecycle、team{id,name}。同一命令内只请求一次
    pub fn machines(&self) -> anyhow::Result<Vec<Value>> {
        self.with_machines(|all| all.to_vec())
    }

    /// 解析名字只要几个字段：取 `view=brief`，中控一条查询，不为找一台机器组装全部机器的完整视图
    fn with_machines<R>(&self, f: impl FnOnce(&[Value]) -> R) -> anyhow::Result<R> {
        let mut cache = self.machines_cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.is_none() {
            let q = [("scope", "all".to_string()), ("view", "brief".to_string())];
            *cache = Some(self.c.get_q("/machines", &q)?.as_array().cloned().unwrap_or_default());
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
    let inc = incident_items(ctx, "firing", None)?;
    let both = json!({"overview": ov, "incidents": inc});
    ctx.emit(&both, || {
        let bucket = |k: &str| {
            let n = |f: &str| ov.pointer(&format!("/{k}/{f}")).and_then(|x| x.as_i64()).unwrap_or(0);
            let parts: Vec<String> = ["ok", "degraded", "down", "unknown", "maintenance"].iter().map(|h| format!("{} {}", health_cn(h), n(h))).collect();
            format!("共 {} · {}", n("total"), parts.join(" · "))
        };
        let cost = ov["monthly_cost"].as_array().map(|a| a.iter().map(|c| money(c["cents"].as_i64().unwrap_or(0), c["currency"].as_str().unwrap_or(""))).collect::<Vec<_>>().join(" + ")).unwrap_or_default();
        let mut s = table::kv(&[
            ("节点", bucket("nodes")),
            ("线路机", bucket("relays")),
            ("agent", format!("在线 {} · 离线 {}", st(&ov, "/online"), st(&ov, "/offline"))),
            ("上架中/退役中", format!("{} / {}", st(&ov, "/provisioning"), st(&ov, "/retiring"))),
            // 即将到期 / 已过期：中控按每台的提前提醒天数算（同续费告警）
            (meta::label("due_states", "soon"), st(&ov, "/due_soon")),
            (meta::label("due_states", "overdue"), st(&ov, "/overdue")),
            ("每月费用", if cost.is_empty() { "-".into() } else { cost }),
            ("本月业务流量", format!("下载 {} / 上传 {}", bytes(num(&ov, "/traffic_month/down").unwrap_or(0.0)), bytes(num(&ov, "/traffic_month/up").unwrap_or(0.0)))),
            ("网卡速率", format!("下行 {} / 上行 {}", bps(num(&ov, "/net_rx_bps").unwrap_or(0.0)), bps(num(&ov, "/net_tx_bps").unwrap_or(0.0)))),
        ]);
        let list = inc.as_array().cloned().unwrap_or_default();
        s.push_str(&format!("\n\n告警（进行中 {}）", list.len()));
        if !list.is_empty() {
            s.push('\n');
            s.push_str(&incident_table(&list, false));
        }
        s
    });
    Ok(0)
}

/// 告警列表（state = firing | all，最多 500 条；`subject` 是机器 id 时只看这台），每条是 `incident_item`
fn incident_items(ctx: &Ctx, state: &str, subject: Option<&str>) -> anyhow::Result<Value> {
    let mut q = vec![("state", state.to_string()), ("limit", "500".to_string())];
    if let Some(id) = subject {
        q.push(("subject_id", id.to_string()));
    }
    let d = items(ctx.c.get_q("/incidents", &q)?)?;
    Ok(Value::Array(d.as_array().map(Vec::as_slice).unwrap_or_default().iter().map(incident_item).collect()))
}

/// `incidents` / `status` 的一条告警：与表格同列（另带规则 key、对象 id、状态与恢复时间），`--json` 输出这些而不是完整的告警对象。
/// 对象名取 subject.name：机器已删除时是「（已删除）」；中控自身的告警（central_ 开头）没有 subject、subject_id，对象名是 subject_name「中控」
fn incident_item(i: &Value) -> Value {
    json!({
        "id": i["id"], "severity": i["severity"], "rule": i["rule"], "rule_name": i["rule_name"],
        "subject_id": i["subject_id"], "subject_name": i.pointer("/subject/name").or(i.get("subject_name")),
        "summary": i["summary"], "reopen_count": i["reopen_count"], "state": i["state"],
        "opened_at": i["opened_at"], "resolved_at": i["resolved_at"], "ack_by": i["ack_by"],
    })
}

/// `list` 是 `incident_item` 的输出。`with_state`：列表里有已恢复的（`incidents --all`）时加一列状态（进行中 / 恢复时间）
fn incident_table(list: &[Value], with_state: bool) -> String {
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|i| {
            let mut r = vec![
                severity_cn(&st(i, "/severity")).to_string(),
                st(i, "/rule_name"),
                st(i, "/subject_name"),
                match i["reopen_count"].as_i64() {
                    Some(n) if n > 0 => format!("{}（反复 {} 次）", st(i, "/summary"), n + 1),
                    _ => st(i, "/summary"),
                },
                table::ts(&st(i, "/opened_at")),
            ];
            if with_state {
                r.push(if i["state"] == "resolved" { format!("已恢复 {}", table::ts(&st(i, "/resolved_at"))) } else { "进行中".into() });
            }
            r.push(if i["ack_by"].is_null() { "".into() } else { format!("已确认 {}", st(i, "/ack_by")) });
            r.push(st(i, "/id"));
            r
        })
        .collect();
    let head: &[&str] = if with_state { &["级别", "规则", "对象", "摘要", "开始", "状态", "确认", "id"] } else { &["级别", "规则", "对象", "摘要", "开始", "确认", "id"] };
    table::render(head, &rows)
}

/// `ls --due N`：在用、有订阅、N 天内到期（含已过期）。自动续费的也算（到期列标出来）
fn due_within(m: &Value, n: i64) -> bool {
    st(m, "/lifecycle") == "active" && m.pointer("/subscription/due_days").and_then(Value::as_i64).is_some_and(|d| d <= n)
}

/// `ls --due` 的到期列：`2026-10-25（3 天后到期）`；自动续费的到期前不提醒、过期才告警，标出来
fn due_cell(i: &Value) -> String {
    let auto = if i["auto_renew"] == true { "，自动续费" } else { "" };
    format!("{}（{}{auto}）", st(i, "/due_date"), due_cn(i["due_days"].as_i64().unwrap_or(0)))
}

pub fn ls(ctx: &Ctx, team: Option<&str>, kind: Option<&str>, status: Option<&str>, retired: bool, due: Option<i64>) -> anyhow::Result<u8> {
    let scope = if retired { "retired" } else { "live" };
    let all = ctx.c.get_q("/machines", &[("scope", scope.to_string())])?;
    let mut list: Vec<Value> = all
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|m| team.is_none_or(|t| st(m, "/team/name") == t))
        .filter(|m| kind.is_none_or(|k| st(m, "/kind") == k))
        .filter(|m| status.is_none_or(|s| st(m, "/health/status") == s || st(m, "/lifecycle") == s))
        .filter(|m| due.is_none_or(|n| due_within(m, n)))
        .map(|m| ls_item(&m, due.is_some()))
        .collect();
    // 按还剩几天排（各按自己的账单时区算），过期最久的在前；剩的天数一样再按到期日，都一样保持列表原来的顺序
    if due.is_some() {
        list.sort_by_key(|i| (i["due_days"].as_i64(), i["due_date"].as_str().map(String::from)));
    }
    let items = Value::Array(list);
    ctx.emit(&items, || {
        let list = items.as_array().map(Vec::as_slice).unwrap_or_default();
        if let (Some(n), true) = (due, list.is_empty()) {
            return format!("没有 {n} 天内到期（含已过期）的在用机器");
        }
        let pct_cell = |i: &Value, p: &str| num(i, p).map(|x| format!("{x:.0}%")).unwrap_or("-".into());
        let rows: Vec<Vec<String>> = list
            .iter()
            .map(|i| {
                let agent = match i["agent_online"].as_bool() {
                    Some(true) => "在线",
                    Some(false) => "离线",
                    None => "-",
                };
                let mut r = vec![
                    st(i, "/name"),
                    st(i, "/kind"),
                    st(i, "/lifecycle"),
                    st(i, "/health"),
                    st(i, "/ip"),
                    agent.into(),
                    pct_cell(i, "/cpu_pct"),
                    pct_cell(i, "/mem_pct"),
                    pct_cell(i, "/disk_pct"),
                    st(i, "/entries"),
                    st(i, "/open_incidents"),
                    match i.pointer("/active_job/state") {
                        Some(Value::String(s)) => format!("{} {}", job_kind_cn(&st(i, "/active_job/kind")), job_state_cn(s)),
                        _ => "".into(),
                    },
                ];
                if due.is_some() {
                    r.insert(1, due_cell(i));
                }
                r
            })
            .collect();
        let mut head = vec!["名称", "种类", "生命周期", "健康", "IP", "agent", "CPU", "内存", "磁盘", "入口", "告警", "作业"];
        if due.is_some() {
            head.insert(1, "到期");
        }
        format!("{}\n共 {} 台", table::render(&head, &rows), list.len())
    });
    Ok(0)
}

/// `ls` 的一行：与表格同列（另带 id），百分比取整；`--json` 输出这些而不是完整视图（完整视图用 show 取单台）。
/// `with_due`（`--due`）：表格多一列到期，这里同样多 `due_date` / `due_days` / `auto_renew`；平时不带（上百台时整个列表要多出两成）
fn ls_item(m: &Value, with_due: bool) -> Value {
    let pct = |used: &str, total: &str| match (num(m, used), num(m, total)) {
        (Some(u), Some(t)) if t > 0.0 => Some((u * 100.0 / t).round() as i64),
        _ => None,
    };
    let mut i = json!({
        "id": m["id"], "name": m["name"], "kind": m["kind"], "lifecycle": m["lifecycle"], "health": m.pointer("/health/status"), "ip": m["ip"],
        "agent_online": m.pointer("/agent/online"),
        "cpu_pct": num(m, "/host/cpu_pct").map(|x| x.round() as i64),
        "mem_pct": pct("/host/mem_used", "/host/mem_total"),
        "disk_pct": pct("/host/disk_used", "/host/disk_total"),
        "entries": m["entries"].as_array().map(Vec::len),
        "open_incidents": m["open_incidents"],
        "active_job": m["active_job"],
    });
    if with_due {
        for k in ["due_date", "due_days", "auto_renew"] {
            i[k] = m["subscription"][k].clone();
        }
    }
    i
}

/// 负载同网页：1 分钟平均负载 ÷ agent 报的核数，折成百分比；容器里不报负载（null）
fn load_cn(load1: Option<f64>, cores: Option<i64>) -> String {
    match (load1, cores) {
        (None, _) => "-".into(),
        (Some(l), Some(c)) if c > 0 => format!("{:.0}%（1 分钟 {l:.2} · {c} 核）", l / c as f64 * 100.0),
        (Some(l), _) => format!("1 分钟 {l:.2}"),
    }
}

/// agent 自报的健康数据（`MachineView.agent.health`）：离线队列现有多少份；其余是 agent 进程启动以来的累计次数。
/// 访问日志解析不了 / 丢弃的行数为 0 时不显示
fn agent_health_cn(h: &Value) -> String {
    let n = |k: &str| st(h, &format!("/{k}"));
    let mut s = format!(
        "离线队列待发 {} 份；自 agent 启动起：因离线队列超限被丢弃 {} 份、被中控拒收 {} 份，访问日志轮转漏读 {} 次、被截断 {} 次",
        n("queue_len"),
        n("queue_dropped"),
        n("queue_dead"),
        n("access_rotation_gaps"),
        n("access_truncations")
    );
    for (k, what) in [("access_unparsed", "解析不了"), ("access_dropped_lines", "被丢弃（超长、含非法字符或不完整）")] {
        if let Some(c) = h[k].as_u64().filter(|c| *c > 0) {
            s.push_str(&format!("、{c} 行{what}"));
        }
    }
    if let Some(e) = h["last_apply_error"].as_str() {
        s.push_str(&format!("；最近一次应用配置失败：{e}"));
    }
    if let Some(e) = h["disk_write_error"].as_str() {
        s.push_str(&format!("；报告写不进磁盘、正从内存直接上报：{e}"));
    }
    s
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
            // 同机 IP 节点没有自己的 SSH（登录宿主机），只写 IP，不拼上宿主机的 SSH 端口
            ("IP", if m["host_machine"].is_null() { format!("{}:{}", st(&m, "/ip"), st(&m, "/ssh_port")) } else { st(&m, "/ip") }),
        ];
        // 同机关系：同机 IP 节点的 SSH、agent、主机读数都是宿主机的；宿主机列出它上面的同机 IP 节点
        if !m["host_machine"].is_null() {
            rows.push(("宿主机", format!("{}（SSH、agent、主机读数都用它的）", st(&m, "/host_machine/name"))));
        }
        if let Some(xs) = m["ip_nodes"].as_array().filter(|a| !a.is_empty()) {
            rows.push(("同机 IP 节点", xs.iter().map(|x| st(x, "/name")).collect::<Vec<_>>().join("、")));
        }
        rows.extend([
            ("供应商/套餐", format!("{} / {}", st(&m, "/provider/name"), st(&m, "/plan/name"))),
            ("备注", st(&m, "/note")),
            ("创建", table::ts(&st(&m, "/created_at"))),
            ("revision", st(&m, "/revision")),
        ]);
        if !m["agent"].is_null() {
            rows.push(("agent", format!("{}  版本 {}  最近上报 {}", if m["agent"]["online"] == true { "在线" } else { "离线" }, st(&m, "/agent/version"), table::ts(&st(&m, "/agent/last_report_at")))));
        }
        if let Some(h) = m.pointer("/agent/health").filter(|h| !h.is_null()) {
            rows.push(("agent 自报", agent_health_cn(h)));
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
        rows.push(("流量额度", format!("本周期已用 {}（{} 起）", table::gb(num(&m, "/quota/used").unwrap_or(0.0)), st(&m, "/quota/period_start"))));
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
            s.push_str(&entry_table(&es.iter().map(entry_item).collect::<Vec<_>>()));
        }
        s
    });
    Ok(0)
}

/// `entries` 的一行：与表格同列（id 完整、流量是字节数），`--json` 输出这些而不是完整的入口视图
fn entry_item(e: &Value) -> Value {
    json!({
        "id": e["id"], "relay": e.pointer("/relay/name"), "port": e["port"], "node": e.pointer("/node/name"),
        "lifecycle": e["lifecycle"], "applied": e["applied"], "apply_error": e["apply_error"],
        "tcp_ok": e.pointer("/tcp/ok"), "egress_ok": e.pointer("/egress/ok"),
        "today_down": e.pointer("/traffic/today_down"), "today_up": e.pointer("/traffic/today_up"),
    })
}

/// `items` 是 `entry_item` 的输出
fn entry_table(items: &[Value]) -> String {
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|e| {
            let probe = |k: &str| match e[k].as_bool() {
                Some(true) => "通".to_string(),
                Some(false) => "不通".to_string(),
                None => "-".to_string(),
            };
            vec![
                st(e, "/id").chars().take(8).collect(),
                format!("{}:{}", st(e, "/relay"), st(e, "/port")),
                st(e, "/node"),
                entry_state_cn(e).into(),
                probe("tcp_ok"),
                probe("egress_ok"),
                format!("下载 {} / 上传 {}", bytes(num(e, "/today_down").unwrap_or(0.0)), bytes(num(e, "/today_up").unwrap_or(0.0))),
                e["apply_error"].as_str().unwrap_or("").to_string(),
            ]
        })
        .collect();
    table::render(&["id", "入口", "节点", "状态", "TCP", "出口", "今日业务流量", "错误"], &rows)
}

fn take_entries(mut m: Value) -> Vec<Value> {
    match m["entries"].take() {
        Value::Array(a) => a,
        _ => vec![],
    }
}

/// 全部入口（完整的入口视图，取线路机上挂的，避免节点和线路机两边重复）
pub fn all_entries(ctx: &Ctx) -> anyhow::Result<Vec<Value>> {
    let Value::Array(relays) = ctx.c.get("/relays")? else { return Ok(vec![]) };
    Ok(relays.into_iter().flat_map(take_entries).collect())
}

pub fn entries(ctx: &Ctx, machine: Option<&str>) -> anyhow::Result<u8> {
    let es = match machine {
        // 节点：它的入口；线路机：挂在它上面的入口。只取这一台的视图
        Some(q) => {
            let (id, _) = ctx.find_id(q, Want::Live)?;
            take_entries(ctx.c.get(&format!("/machines/{id}"))?)
        }
        None => all_entries(ctx)?,
    };
    let items = Value::Array(es.iter().map(entry_item).collect());
    ctx.emit(&items, || {
        let list = items.as_array().map(Vec::as_slice).unwrap_or_default();
        format!("{}\n共 {} 个入口", entry_table(list), list.len())
    });
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
        let mut hdr = vec!["时间"];
        hdr.extend(cols.iter());
        head.push_str("\n\n");
        head.push_str(&table::render(&hdr, &rows));
        head
    });
    Ok(0)
}

/// 成功率：全部成功才写 100%，否则最多写到 99.99%（1439/1440 不会四舍五入成 100%）；没有探测为 -（同网页）
fn success_pct(p: Option<f64>) -> String {
    match p {
        None => "-".into(),
        Some(p) if p >= 100.0 => "100%".into(),
        Some(p) => format!("{:.2}%", p.min(99.99)),
    }
}

/// `uptime` 的文字：第一行整段的成功率，下面每天一行（旧→新，最后一天是今天、还没过完）
fn uptime_text(name: &str, d: &Value) -> String {
    let n = |x: &Value, k: &str| x[k].as_i64().unwrap_or(0);
    let rate = |ok: i64, total: i64| (total > 0).then(|| 100.0 * ok as f64 / total as f64);
    let count = |ok: i64, total: i64| if total > 0 { format!("{ok}/{total}") } else { "-".into() };
    let items = d["items"].as_array().map(Vec::as_slice).unwrap_or_default();
    let rows: Vec<Vec<String>> = items
        .iter()
        .enumerate()
        .map(|(i, x)| {
            let (t, t_ok, e, e_ok) = (n(x, "tcp_total"), n(x, "tcp_ok"), n(x, "egress_total"), n(x, "egress_ok"));
            let date = if i + 1 == items.len() { format!("{}（今天）", st(x, "/date")) } else { st(x, "/date") };
            vec![date, success_pct(rate(t_ok, t)), count(t_ok, t), success_pct(rate(e_ok, e)), count(e_ok, e)]
        })
        .collect();
    format!(
        "{name} 近 {} 天：TCP {} · 出口 {}\n{}",
        st(d, "/days"),
        success_pct(num(d, "/tcp_pct")),
        success_pct(num(d, "/egress_pct")),
        table::render(&["日期", "TCP", "TCP 成功/次数", "出口", "出口成功/次数"], &rows)
    )
}

pub fn uptime(ctx: &Ctx, machine: &str, days: i64) -> anyhow::Result<u8> {
    // 已退役的机器也能看退役前的
    let (id, name) = ctx.find_id(machine, Want::Any)?;
    let d = ctx.c.get_q(&format!("/machines/{id}/availability"), &[("days", days.to_string())])?;
    ctx.emit(&d, || uptime_text(&name, &d));
    Ok(0)
}

pub fn incidents(ctx: &Ctx, all: bool, machine: Option<&str>) -> anyhow::Result<u8> {
    // 告警历史也要看已退役的机器
    let id = machine.map(|m| ctx.find_id(m, Want::Any)).transpose()?.map(|(id, _)| id);
    let d = incident_items(ctx, if all { "all" } else { "firing" }, id.as_deref())?;
    ctx.emit(&d, || {
        let list = d.as_array().cloned().unwrap_or_default();
        if list.is_empty() { "没有告警".to_string() } else { format!("{}\n共 {} 条", incident_table(&list, all), list.len()) }
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
        format!("{}\n共 {} 个", table::render(&["id", "种类", "目标", "状态", "步骤", "创建者", "创建", "错误"], &rows), rows.len())
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
    Ok(job_end(ctx, &j))
}

/// 作业的退出码：失败 / 取消为 1，`--json` 时 stderr 另给 `{"error":{"code":"job_failed"|"job_cancelled",…}}`；其余为 0
fn job_end(ctx: &Ctx, j: &Value) -> u8 {
    let (code, what) = match j["state"].as_str() {
        Some("failed") => ("job_failed", "作业失败"),
        Some("cancelled") => ("job_cancelled", "作业已取消"),
        _ => return 0,
    };
    if ctx.json {
        let msg = match j["error"].as_str().filter(|e| !e.is_empty()) {
            Some(e) => format!("{what}：{e}"),
            None => what.to_string(),
        };
        eprintln!("{}", json!({"error": {"code": code, "message": msg, "status": null}}));
    }
    1
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
            return Ok(job_end(ctx, &j));
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

pub fn audit(ctx: &Ctx, target: Option<&str>, limit: i64, before: Option<i64>) -> anyhow::Result<u8> {
    let mut q = vec![("limit", limit.to_string())];
    if let Some(b) = before {
        q.push(("before_id", b.to_string()));
    }
    if let Some(t) = target {
        let (id, _) = ctx.find_id(t, Want::Any)?;
        q.push(("target_id", id));
    }
    let d = ctx.c.get_q("/audit", &q)?;
    ctx.emit(&d, || {
        let rows: Vec<Vec<String>> = d.as_array().cloned().unwrap_or_default().iter().map(|a| vec![table::ts(&st(a, "/ts")), st(a, "/actor"), st(a, "/title"), st(a, "/summary")]).collect();
        let last = d.as_array().and_then(|a| a.last()).and_then(|a| a["id"].as_i64());
        match last {
            None => "没有审计记录".into(),
            // 服务端把 limit 夹到 1..=1000
            Some(id) if rows.len() as i64 >= limit.clamp(1, 1000) => format!("{}\n更早的记录：--before {id}", table::render(&["时间", "操作者", "操作", "摘要"], &rows)),
            Some(_) => table::render(&["时间", "操作者", "操作", "摘要"], &rows),
        }
    });
    Ok(0)
}

/// 日志类接口返回 {exit_code, stdout, stderr}：stdout 原样输出，stderr 走 stderr
fn print_text_result(ctx: &Ctx, d: &Value) -> u8 {
    if ctx.json {
        outln(d.to_string());
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

/// 组织时区（默认北京时间）当前的时分秒（同 `table::ts`）
fn now_hms() -> String {
    let t = time::OffsetDateTime::now_utc().to_offset(meta::offset());
    format!("{:02}:{:02}:{:02}", t.hour(), t.minute(), t.second())
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
                        outln(format!("{}  {:<10} {}", now_hms(), st(&ev, "/type"), event_summary(&ev)));
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
    fn error_json_keeps_api_code_and_status() {
        assert_eq!(error_json(&api(403)), json!({"code": "x", "message": "m", "status": 403}));
        assert_eq!(error_json(&anyhow!("坏了")), json!({"code": "error", "message": "坏了", "status": null}));
        // 网络错误：超时和连不上在 --json 里分得开（超时的操作可能已执行，不能盲目重试）
        let n: anyhow::Error = NetError { code: "timeout", message: "等中控响应超过 30 秒".into() }.into();
        assert_eq!(error_json(&n.context("取作业进度")), json!({"code": "timeout", "message": "取作业进度: 等中控响应超过 30 秒", "status": null}));
    }

    #[test]
    fn due_and_cycle_wording() {
        crate::meta::test_init();
        assert_eq!((due_cn(3), due_cn(0), due_cn(-2)), ("3 天后到期".to_string(), "今天到期".to_string(), "已过期 2 天".to_string()));
        assert_eq!((cycle_cn("monthly"), cycle_cn("semiannual"), cycle_cn("yearly")), ("月", "半年", "年"));
    }

    #[test]
    fn ls_due_filter_and_cell() {
        let m = |lc: &str, due_days: i64| json!({"lifecycle": lc, "subscription": {"due_date": "2026-10-20", "due_days": due_days, "auto_renew": false}});
        assert!(due_within(&m("active", 7), 7) && due_within(&m("active", -2), 0));
        // 不在用的、到期还远的、没有订阅的不列
        assert!(!due_within(&m("active", 8), 7) && !due_within(&m("provisioning", 1), 7) && !due_within(&json!({"lifecycle": "active", "subscription": null}), 7));
        assert_eq!(due_cell(&ls_item(&m("active", -2), true)), "2026-10-20（已过期 2 天）");
        let auto = ls_item(&json!({"subscription": {"due_date": "2026-10-25", "due_days": 3, "auto_renew": true}}), true);
        assert_eq!(due_cell(&auto), "2026-10-25（3 天后到期，自动续费）");
        // 到期三项只在 --due 时带，平时的精简对象不变
        assert!(ls_item(&m("active", 3), false).get("due_days").is_none());
    }

    #[test]
    fn load_as_percent_of_cores() {
        assert_eq!(load_cn(Some(0.5), Some(2)), "25%（1 分钟 0.50 · 2 核）");
        assert_eq!(load_cn(Some(0.5), None), "1 分钟 0.50");
        assert_eq!(load_cn(None, Some(2)), "-");
    }

    #[test]
    fn agent_health_wording() {
        let mut h = json!({"queue_len": 3, "queue_dropped": 0, "queue_dead": 1, "access_unparsed": 0, "access_dropped_lines": 0, "access_rotation_gaps": 2, "access_truncations": 0});
        assert_eq!(agent_health_cn(&h), "离线队列待发 3 份；自 agent 启动起：因离线队列超限被丢弃 0 份、被中控拒收 1 份，访问日志轮转漏读 2 次、被截断 0 次");
        h["access_unparsed"] = json!(4);
        h["last_apply_error"] = json!("xray 配置校验失败");
        assert!(agent_health_cn(&h).ends_with("被截断 0 次、4 行解析不了；最近一次应用配置失败：xray 配置校验失败"));
        h["disk_write_error"] = json!("No space left on device (os error 28)");
        assert!(agent_health_cn(&h).ends_with("；报告写不进磁盘、正从内存直接上报：No space left on device (os error 28)"));
    }

    #[test]
    fn uptime_wording() {
        assert_eq!([success_pct(Some(100.0)), success_pct(Some(99.9993)), success_pct(Some(99.861)), success_pct(None)], ["100%", "99.99%", "99.86%", "-"]);
        let d = json!({"days": 2, "tcp_pct": 99.930_555, "egress_pct": null, "items": [
            {"date": "2026-10-21", "tcp_total": 0, "tcp_ok": 0, "egress_total": 0, "egress_ok": 0},
            {"date": "2026-10-22", "tcp_total": 1440, "tcp_ok": 1439, "egress_total": 0, "egress_ok": 0}]});
        let t = uptime_text("团队1-06", &d);
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[0], "团队1-06 近 2 天：TCP 99.93% · 出口 -");
        assert_eq!(lines[2].split_whitespace().collect::<Vec<_>>(), ["2026-10-21", "-", "-", "-", "-"]);
        assert_eq!(lines[3].split_whitespace().collect::<Vec<_>>(), ["2026-10-22（今天）", "99.93%", "1439/1440", "-", "-"]);
    }

    #[test]
    fn ctx_caches_are_empty_until_used() {
        let ctx = Ctx::new(Client::new("http://127.0.0.1:1", crate::client::Cred::None).unwrap(), "http://127.0.0.1:1".into(), false);
        assert!(ctx.machines_cache.lock().unwrap().is_none() && ctx.teams_cache.lock().unwrap().is_none());
    }
}
