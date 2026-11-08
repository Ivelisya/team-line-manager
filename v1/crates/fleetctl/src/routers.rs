//! 软路由：routers（列表）、router show / exits（出口历史统计）/ add / bootstrap / deploy / edit / rm，
//! 以及 router sync（出口参数同步到入口当前参数）/ protocol（出口切 AnyTLS / Reality）/ switch（select 组临时切换出口，可到时自动切回）。
//! 接入有两种：`router add` 不给 --host 打印接入命令（在路由上用 root 执行，路由在 NAT 后面、中控连不到它时用）；
//! 给 --host 就一键部署（中控能直接连到路由的 SSH 时用：中控登录上去装好隧道；密码只从标准输入读）。

use crate::cmds::{num, outln, st, Ctx};
use crate::ops::confirm;
use crate::resolve;
use crate::table::{self, bps, bytes, pct};
use anyhow::{anyhow, bail};
use serde_json::{json, Value};
use std::io::BufRead;
use std::time::Duration;

/// 速率：路由还没算出来（null）显示「—」，不是 0
fn rate(v: &Value, ptr: &str) -> String { num(v, ptr).map_or_else(|| "—".to_string(), bps) }

pub(crate) fn state_cn(s: &str) -> &str {
    match s {
        "pending" => "等待接入",
        "online" => "在线",
        "offline" => "离线",
        s => s,
    }
}

fn deploy_cn(s: &str) -> &str {
    match s {
        "running" => "部署中",
        "failed" => "部署失败",
        "done" => "已部署",
        s => s,
    }
}

/// mihomo 的组类型
fn group_kind_cn(k: &str) -> &str {
    match k {
        "Selector" => "select（手动选）",
        "URLTest" => "url-test（自动测速）",
        "Fallback" => "fallback（故障转移）",
        "LoadBalance" => "load-balance（负载均衡）",
        k => k,
    }
}

fn sync_cn(s: &str) -> &str {
    match s {
        "ok" => "一致",
        "drift" => "不一致",
        "unmanaged" => "非 fleet 入口",
        s => s,
    }
}

/// 运行时长：`3 天 4 小时` / `2 小时 5 分钟` / `7 分钟`
fn uptime_cn(s: i64) -> String {
    match (s / 86400, s % 86400 / 3600, s % 3600 / 60) {
        (0, 0, m) => format!("已运行 {m} 分钟"),
        (0, h, m) => format!("已运行 {h} 小时 {m} 分钟"),
        (d, h, _) => format!("已运行 {d} 天 {h} 小时"),
    }
}


/// 找一台路由：id、id 前缀（至少 6 位）、名称（不分大小写）；规则同 `resolve::named`
pub(crate) fn find(all: &[Value], q: &str) -> anyhow::Result<Value> {
    let name = |r: &Value| st(r, "/name");
    resolve::named(all, q, "软路由", &[&name], &name).cloned().map_err(|e| anyhow!("{e}（先 fleetctl routers 看列表）"))
}

/// 路由连不上（离线），但还留着掉线前最后一次读到的数据：这些读数不是现在的，不能当现状显示
fn stale(r: &Value) -> bool { r["state"] == "offline" && (r["summary"].is_object() || r["detail"].is_object()) }

/// 那一刻到现在多久：`不到 1 分钟` / `7 分钟` / `3 小时` / `2 天`
fn since_cn(t: &str) -> Option<String> {
    let t = time::OffsetDateTime::parse(t, &time::format_description::well_known::Rfc3339).ok()?;
    let s = (time::OffsetDateTime::now_utc() - t).whole_seconds().max(0);
    Some(match s {
        0..=59 => "不到 1 分钟".to_string(),
        60..=3599 => format!("{} 分钟", s / 60),
        3600..=86399 => format!("{} 小时", s / 3600),
        _ => format!("{} 天", s / 86400),
    })
}

/// 出口可用 / 总数：只数 fleet 出口（对应面板入口的，告警也只看它们），客户自己加的出口单列在后面；旧中控的摘要没有 fleet 字段，退回全部出口
fn exits_cell(s: &Value) -> String {
    let e = &s["exits"];
    let (alive, total, other) = match e.get("fleet").filter(|f| f.is_object()) {
        Some(f) => (st(f, "/alive"), st(f, "/total"), e["other"].as_i64().unwrap_or(0)),
        None => (st(e, "/alive"), st(e, "/total"), 0),
    };
    let mut cell = format!("{alive}/{total}");
    if let Some(drift) = e["drift"].as_i64().filter(|d| *d > 0) {
        cell.push_str(&format!("（{drift} 个不一致）"));
    }
    if other > 0 {
        cell.push_str(&format!(" · 其他出口 {other}"));
    }
    cell
}

fn row(r: &Value) -> Vec<String> {
    let s = &r["summary"];
    // 离线：出口、客户端、CPU、内存、WAN 都是掉线前的读数，显示 -（最近在线那一列看得到多久没连上）
    let old = stale(r);
    let dash = || "-".to_string();
    let exits = if s.is_object() && !old { exits_cell(s) } else { dash() };
    let wan = match s.get("wan").filter(|w| w.is_object()).filter(|_| !old) {
        Some(w) if w["up"] == false => "断开".to_string(),
        Some(w) => format!("↓{} ↑{}", rate(w, "/rx_bps"), rate(w, "/tx_bps")),
        None => "-".into(),
    };
    let mut state = state_cn(&st(r, "/state")).to_string();
    if let Some(d) = r["deploy"]["state"].as_str().filter(|d| *d != "done") {
        state.push_str(&format!("（{}）", deploy_cn(d)));
    }
    if r["maintenance"] == true {
        state.push_str("（维护中）");
    }
    vec![
        st(r, "/name"),
        r["team"]["name"].as_str().unwrap_or("-").to_string(),
        state,
        exits,
        if old { dash() } else { st(s, "/clients") },
        num(s, "/cpu_pct").filter(|_| !old).map(|c| format!("{c:.0}%")).unwrap_or_else(dash),
        if old { dash() } else { pct(num(s, "/mem_used"), num(s, "/mem_total")) },
        wan,
        table::ts(&st(r, "/last_seen_at")),
        st(r, "/id").chars().take(13).collect(),
    ]
}

const HEAD: [&str; 10] = ["名称", "团队", "状态", "出口通/总", "客户端", "CPU", "内存", "WAN", "最近在线", "id"];

pub fn ls(ctx: &Ctx, team: Option<&str>) -> anyhow::Result<u8> {
    let mut all = ctx.list("/routers")?;
    if let Some(t) = team {
        all.retain(|r| r["team"]["name"].as_str() == Some(t) || st(r, "/team/id") == t);
    }
    // --json：离线的路由 stale: true，summary 里是掉线前最后一次读到的数据
    for r in &mut all {
        r["stale"] = Value::Bool(stale(r));
    }
    let data = Value::Array(all.clone());
    ctx.emit(&data, || {
        if all.is_empty() {
            return "还没有软路由（fleetctl router add <名称> 接入一台）".into();
        }
        let rows: Vec<Vec<String>> = all.iter().map(row).collect();
        let bad: Vec<String> = all.iter().filter_map(|r| r["error"].as_str().map(|e| format!("{}：{e}", st(r, "/name")))).collect();
        let mut s = format!("{}\n共 {} 台", table::render(&HEAD, &rows), rows.len());
        if !bad.is_empty() {
            s.push_str(&format!("\n\n{}", bad.join("\n")));
        }
        s
    });
    Ok(0)
}

pub fn show(ctx: &Ctx, q: &str) -> anyhow::Result<u8> {
    let r = find(&ctx.list("/routers")?, q)?;
    let id = st(&r, "/id");
    let mut d = ctx.c.get(&format!("/routers/{id}"))?;
    d["stale"] = Value::Bool(stale(&d));
    // 末尾带最近 5 条配置变更（--json 的 data 仍是详情原样，要变更用 router changes --json）
    let recent = if ctx.json { Value::Null } else { ctx.c.get_q(&format!("/routers/{id}/changes"), &[("limit", "5".to_string())])? };
    ctx.emit(&d, || {
        let mut s = human_show(&d);
        if let Some(list) = recent.as_array().filter(|a| !a.is_empty()) {
            s.push_str(&format!("

最近配置变更（fleetctl router changes {} 看全部）
{}", crate::ops::shell_quote(&st(&d, "/name")), changes_human(list)));
        }
        s
    });
    Ok(0)
}

/// 一条变更的来源：中控作业（类型名 + 作业 id，fleetctl job 能看）/ 路由上改的
fn change_source(c: &Value) -> String {
    match c["source"].as_str() {
        Some("job") => format!("中控作业「{}」{}", crate::meta::label("job_kinds", &st(c, "/job_kind")), st(c, "/job_id")),
        _ => "路由上改的".to_string(),
    }
}

/// 出口的一条增删改：`节点01  server:port relay.test:20003 → relay.test:20010；凭据变了`
fn change_item(x: &Value) -> String {
    let what = match x["op"].as_str() {
        Some("added") => format!("新增 {}", st(x, "/to")),
        Some("removed") => format!("删除 {}", st(x, "/from")),
        _ => {
            let mut parts = vec![];
            if x["from"].is_string() {
                parts.push(format!("server:port {} → {}", st(x, "/from"), st(x, "/to")));
            }
            if x["creds"] == true {
                parts.push("凭据变了".to_string());
            }
            parts.join("；")
        }
    };
    format!("  {}  {what}", st(x, "/exit"))
}

/// 变更列表（新的在前）：每条一行时间 + 来源 + 新旧 sha（前 8 位），下面缩进列出出口的增删改
fn changes_human(list: &[Value]) -> String {
    let short = |c: &Value, k: &str| st(c, k).chars().take(8).collect::<String>();
    list.iter()
        .map(|c| {
            let mut lines = vec![format!("{}  {}  配置 {} → {}", table::ts(&st(c, "/at")), change_source(c), short(c, "/old_sha"), short(c, "/new_sha"))];
            lines.extend(c["diff"].as_array().into_iter().flatten().map(change_item));
            match (c["diff"].as_array().is_none_or(Vec::is_empty), c["note"].as_str()) {
                (_, Some(n)) => lines.push(format!("  （{n}）")),
                (true, None) => lines.push("  出口没有变化（改的是出口以外的部分）".to_string()),
                _ => {}
            }
            lines.join("
")
        })
        .collect::<Vec<_>>()
        .join("
")
}

/// `router changes <路由> [--limit N]`：配置变更时间线（新的在前）
pub fn changes(ctx: &Ctx, q: &str, limit: Option<u32>) -> anyhow::Result<u8> {
    let r = find(&ctx.list("/routers")?, q)?;
    let query: Vec<(&str, String)> = limit.map(|n| ("limit", n.to_string())).into_iter().collect();
    let d = ctx.c.get_q(&format!("/routers/{}/changes", st(&r, "/id")), &query)?;
    ctx.emit(&d, || match d.as_array().filter(|a| !a.is_empty()) {
        Some(list) => format!("{}
{}
共 {} 条", st(&r, "/name"), changes_human(list), list.len()),
        None => format!("{} 还没有记录到配置变更（中控发现配置文件变了才记，记录从中控升级后开始）", st(&r, "/name")),
    });
    Ok(0)
}

fn human_show(d: &Value) -> String {
    let mut out = human_show_body(d);
    if stale(d) {
        // 掉线前最后一次读到的时间：详情的 collected_at，没有就用最近在线
        let at = d["detail"]["collected_at"].as_str().or_else(|| d["last_seen_at"].as_str());
        let when = at.and_then(since_cn).map_or_else(|| "掉线前最后一次".to_string(), |s| format!("{s}前"));
        out = format!("以下是{when}读到的（路由现在连不上，读数不是现状）\n\n{out}");
    }
    out
}

fn human_show_body(d: &Value) -> String {
    let det = &d["detail"];
    let y = &det["system"];
    let oc = &det["openclash"];
    let mut kv: Vec<(&str, String)> = vec![
        ("名称", st(d, "/name")),
        ("团队", d["team"]["name"].as_str().unwrap_or("-").into()),
        ("状态", format!("{}{}", state_cn(&st(d, "/state")), if d["maintenance"] == true { "（维护中：不开新告警，开着的保持）" } else { "" })),
        ("最近在线", table::ts(&st(d, "/last_seen_at"))),
        ("隧道端口", st(d, "/tunnel_port")),
    ];
    // 在线时的 error = 连得上，但采集出错（快照是上一份）
    if let Some(e) = d["error"].as_str() {
        kv.push((if d["state"] == "online" { "采集出错" } else { "原因" }, e.into()));
    }
    if d["deploy"].is_object() {
        let dep = &d["deploy"];
        let mut s = format!("{} · {}@{}:{}", deploy_cn(&st(dep, "/state")), st(dep, "/user"), st(dep, "/host"), st(dep, "/port"));
        if let Some(step) = dep["step"].as_str() {
            s.push_str(&format!(" · 正在{step}"));
        }
        if let Some(e) = dep["error"].as_str() {
            s.push_str(&format!(" · {e}"));
        }
        kv.push(("一键部署", s));
    }
    if let Some(exp) = d["bootstrap_expires_at"].as_str() {
        kv.push(("接入命令", format!("还没用，{} 前有效", table::ts(exp))));
    }
    if y.is_object() {
        kv.push(("系统", format!("{} · 内核 {}", st(y, "/os"), st(y, "/kernel"))));
        let load = y["load"].as_array().map(|l| l.iter().map(|x| x.as_f64().map(|v| format!("{v:.2}")).unwrap_or("-".into())).collect::<Vec<_>>().join(" / ")).unwrap_or_default();
        kv.push(("CPU", format!("{} · {} 核 · 使用 {} · 负载 {load}", st(y, "/cpu_model"), st(y, "/cores"), num(y, "/cpu_pct").map(|c| format!("{c:.0}%")).unwrap_or("-".into()))));
        kv.push(("内存", format!("{} / {}（{}）", bytes(num(y, "/mem_used").unwrap_or(0.0)), bytes(num(y, "/mem_total").unwrap_or(0.0)), pct(num(y, "/mem_used"), num(y, "/mem_total")))));
        kv.push(("存储", format!("{} {} / {}（{}）", st(y, "/disk_path"), bytes(num(y, "/disk_used").unwrap_or(0.0)), bytes(num(y, "/disk_total").unwrap_or(0.0)), pct(num(y, "/disk_used"), num(y, "/disk_total")))));
        kv.push(("连接表", format!("{} / {}（{}）", st(y, "/conntrack"), st(y, "/conntrack_max"), pct(num(y, "/conntrack"), num(y, "/conntrack_max")))));
        let up = num(y, "/uptime_s").map(|s| uptime_cn(s as i64)).unwrap_or("-".into());
        kv.push(("运行", format!("{up} · {} 个进程 · 温度 {} · 时钟偏差 {} 秒（中控减路由，正数 = 路由慢）", st(y, "/procs"), num(y, "/temp_c").map(|t| format!("{t:.0}℃")).unwrap_or("-".into()), st(y, "/clock_skew_s"))));
    }
    if oc.is_object() {
        let run = match (oc["enabled"] == true, oc["running"] == true) {
            (_, true) => "运行中",
            (true, false) => "已启用但没在运行",
            (false, false) => "未启用",
        };
        let mut s = format!("{run} · {} · 模式 {} · {} 个连接 · ↑{} ↓{}", st(oc, "/version"), st(oc, "/mode"), st(oc, "/conns"), rate(oc, "/up_bps"), rate(oc, "/down_bps"));
        for k in ["error", "config_error"] {
            if let Some(e) = oc[k].as_str() {
                s.push_str(&format!(" · {e}"));
            }
        }
        if oc["config_stale"] == true {
            s.push_str("（出口按上次读到的配置对应入口）");
        }
        if let Some(e) = oc["delay_error"].as_str() {
            s.push_str(&format!(" · 最近一次延迟测试没做成：{e}"));
        }
        kv.push(("OpenClash", s));
        kv.push(("配置", format!("{}（{}）", st(oc, "/config_path"), table::ts(&st(oc, "/config_mtime")))));
    }
    let mut out = table::kv(&kv);
    if let Some(ifs) = det["interfaces"].as_array().filter(|a| !a.is_empty()) {
        let rows: Vec<Vec<String>> = ifs
            .iter()
            .map(|i| {
                vec![
                    st(i, "/name"),
                    st(i, "/device"),
                    if i["up"] == true { "up".into() } else { "down".into() },
                    st(i, "/proto"),
                    st(i, "/ip"),
                    format!("↓{} ↑{}", rate(i, "/rx_bps"), rate(i, "/tx_bps")),
                ]
            })
            .collect();
        out.push_str(&format!("\n\n接口\n{}", table::render(&["接口", "设备", "状态", "协议", "IPv4", "速率"], &rows)));
    }
    if let Some(ex) = det["exits"].as_array().filter(|a| !a.is_empty()) {
        let rows: Vec<Vec<String>> = ex
            .iter()
            .map(|e| {
                let alive = match (e["alive"].as_bool(), e["fail_streak"].as_i64().unwrap_or(0)) {
                    (Some(true), _) => format!("通 {}ms", st(e, "/delay_ms")),
                    (Some(false), n) if n >= 2 => format!("不通（连续 {n} 次）"),
                    (Some(false), _) => "不通".into(),
                    _ => "-".into(),
                };
                vec![
                    st(e, "/name"),
                    proto_cn(e),
                    format!("{}:{}", st(e, "/server"), st(e, "/port")),
                    e["entry"]["name"].as_str().unwrap_or("-").into(),
                    sync_cn(&st(e, "/sync")).into(),
                    alive,
                    st(e, "/conns"),
                    st(e, "/clients"),
                    format!("↓{} ↑{}", rate(e, "/down_bps"), rate(e, "/up_bps")),
                ]
            })
            .collect();
        out.push_str(&format!("\n\n出口\n{}", table::render(&["出口", "协议", "地址", "对应入口", "参数", "延迟", "连接", "客户端", "速率"], &rows)));
    }
    if let Some(gs) = det["groups"].as_array().filter(|a| !a.is_empty()) {
        let sw = d["switches"].as_array().cloned().unwrap_or_default();
        let rows: Vec<Vec<String>> = gs
            .iter()
            .map(|g| {
                let s = sw.iter().find(|s| s["group"] == g["name"]);
                let mut note = s.map_or_else(String::new, |s| {
                    let back = match (s["defer_why"].as_str(), s["back_at"].as_str()) {
                        (Some(_), next) => format!("，等原出口恢复后再切回，下次检查 {}", next.map(table::ts).unwrap_or_default()),
                        (None, Some(t)) => format!("，{} 自动切回", table::ts(t)),
                        (None, None) => String::new(),
                    };
                    format!("临时切换中（原来是 {}{back}）", st(s, "/from"))
                });
                if g["used"] == false {
                    note.push_str(if note.is_empty() { "未被规则引用（切了不影响流量）" } else { "；未被规则引用" });
                }
                vec![st(g, "/name"), group_kind_cn(&st(g, "/type")).into(), st(g, "/now"), g["all"].as_array().map_or(0, Vec::len).to_string(), note]
            })
            .collect();
        out.push_str(&format!("\n\n策略组（select 组可以临时切换：fleetctl router switch）\n{}", table::render(&["组", "类型", "当前出口", "成员", "临时切换"], &rows)));
        // 推迟自动切回的原因放表外（表里放不下）
        for s in sw.iter().filter(|s| s["defer_why"].is_string()) {
            out.push_str(&format!("\n{}：{}", st(s, "/group"), st(s, "/defer_why")));
        }
    }
    if let Some(cl) = det["clients"].as_array().filter(|a| !a.is_empty()) {
        let rows: Vec<Vec<String>> = cl
            .iter()
            .map(|c| {
                let ex = c["exits"].as_array().map(|a| a.iter().map(|x| format!("{}×{}", st(x, "/name"), st(x, "/conns"))).collect::<Vec<_>>().join(" ")).unwrap_or_default();
                let speed = format!("↓{} ↑{}", rate(c, "/down_bps"), rate(c, "/up_bps"));
                vec![st(c, "/ip"), st(c, "/hostname"), st(c, "/mac"), if c["online"] == true { "在线".into() } else { "离线".into() }, st(c, "/conns"), speed, ex]
            })
            .collect();
        out.push_str(&format!("\n\n客户端\n{}", table::render(&["IP", "主机名", "MAC", "状态", "连接", "速率", "出口"], &rows)));
    }
    out
}

// ── 出口历史：router exits ──────────────────────────────────────────────

/// 一个出口在所选范围内的统计（`GET /routers/{id}/exits/metrics` 的点，按时间顺序）。口径按点的粒度 `step` 分：
/// - 测试 / 失败次数：各点的 `tests` / `tests - tests_ok` 相加（中控每 5 分钟测一轮，没通的当场重测一次、重测通了算通，一轮算一次），三种粒度都准。
///   点里没有 `tests` / `tests_ok` 当作中控返回有误，报错，不当成 0 次。
/// - 连续两轮失败（`streaks`）：按时间顺序，一轮 = 原始点（30 秒，1h）里测了的那一点，或一个 5 分钟点（6h / 24h：这 5 分钟里有一次没通就算没通）；
///   没测的（`alive` 为 null）跳过、不打断；连着 ≥2 轮不通算一段（true / 开头 → false → false），同告警 router_exits_down 的「连续两次不通」。
///   小时点（7d / 30d）只有这一小时测了几次、通了几次，看不出先后：为 None。
/// - 延迟（`delays`）：通了的那几次。原始点是每次的延迟；5 分钟点是这 5 分钟里通了的平均（一般就一次）；小时点是这一小时的平均（最大值因此偏小）
#[derive(Debug, PartialEq, Default)]
struct ExitStats {
    tests: i64,
    fails: i64,
    streaks: Option<i64>,
    delays: Vec<f64>,
}

fn exit_stats(step: i64, points: &[Value]) -> anyhow::Result<ExitStats> {
    let hourly = step >= 3600;
    let mut x = ExitStats { streaks: (!hourly).then_some(0), ..Default::default() };
    let mut run = 0;
    for p in points {
        let alive = p["alive"].as_bool();
        let (Some(n), Some(ok)) = (p["tests"].as_i64(), p["tests_ok"].as_i64()) else { bail!("中控返回的出口指标点缺 tests / tests_ok，数不出测试次数") };
        x.tests += n;
        x.fails += n - ok;
        if let Some(d) = p["delay_ms"].as_f64() {
            x.delays.push(d);
        }
        if let Some(c) = x.streaks.as_mut() {
            match alive {
                Some(false) => {
                    run += 1;
                    if run == 2 {
                        *c += 1;
                    }
                }
                Some(true) => run = 0,
                None => {}
            }
        }
    }
    Ok(x)
}

/// 中位数（偶数个取中间两个的平均）与最大值，四舍五入到整毫秒；没有为 null
fn delay_json(v: &[f64]) -> (Value, Value) {
    let mut v: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return (Value::Null, Value::Null);
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    let med = if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 };
    (json!(med.round() as i64), json!(v[n - 1].round() as i64))
}

/// 成功率（%，一位小数）；没有测试为 null
fn success_pct(tests: i64, fails: i64) -> Value {
    if tests == 0 {
        return Value::Null;
    }
    json!(((tests - fails) as f64 * 1000.0 / tests as f64).round() / 10.0)
}

/// `router exits --json` 的 data：每个出口一行统计（指标里有的，加上现在配置里有、范围内还没有点的），`total` 是合计。
/// `current` 是最近一次采集的出口（`router show` 的 detail.exits；从没采集成功过为空），给「当前连接」与「还在不在配置里」
fn exits_report(range: &str, metrics: &Value, current: &[Value]) -> anyhow::Result<Value> {
    let step = metrics["step_s"].as_i64().unwrap_or(300);
    let series = metrics["exits"].as_array().cloned().unwrap_or_default();
    let mut names: Vec<String> = series.iter().map(|e| st(e, "/name")).collect();
    for e in current {
        let n = st(e, "/name");
        if !names.contains(&n) {
            names.push(n);
        }
    }
    let (mut all, mut tot) = (vec![], ExitStats { streaks: (step < 3600).then_some(0), ..Default::default() });
    let mut conns_sum = 0;
    for name in &names {
        let pts = series.iter().find(|e| e["name"] == name.as_str()).and_then(|e| e["points"].as_array()).cloned().unwrap_or_default();
        let x = exit_stats(step, &pts).map_err(|e| anyhow!("出口「{name}」：{e}"))?;
        let now = current.iter().find(|e| e["name"] == name.as_str());
        let conns = now.and_then(|e| e["conns"].as_i64());
        conns_sum += conns.unwrap_or(0);
        let (med, max) = delay_json(&x.delays);
        all.push(json!({
            "name": name, "tests": x.tests, "fails": x.fails, "success_pct": success_pct(x.tests, x.fails), "fail_streaks": x.streaks,
            "delay_median_ms": med, "delay_max_ms": max, "conns": conns, "current": now.is_some(),
        }));
        tot.tests += x.tests;
        tot.fails += x.fails;
        tot.streaks = tot.streaks.zip(x.streaks).map(|(a, b)| a + b);
        tot.delays.extend(x.delays);
    }
    let (med, max) = delay_json(&tot.delays);
    Ok(json!({
        "range": range, "step_s": step, "exits": all,
        "total": {"exits": names.len(), "tests": tot.tests, "fails": tot.fails, "success_pct": success_pct(tot.tests, tot.fails), "fail_streaks": tot.streaks,
                  "delay_median_ms": med, "delay_max_ms": max, "conns": conns_sum},
    }))
}

fn exits_human(d: &Value, snapshot: bool) -> String {
    let ms = |v: &Value| v.as_i64().map_or_else(|| "-".to_string(), |x| format!("{x} ms"));
    let pct = |v: &Value| v.as_f64().map_or_else(|| "-".to_string(), |x| format!("{x}%"));
    let row = |name: String, e: &Value| {
        vec![name, st(e, "/tests"), st(e, "/fails"), pct(&e["success_pct"]), st(e, "/fail_streaks"), ms(&e["delay_median_ms"]), ms(&e["delay_max_ms"]), st(e, "/conns")]
    };
    let exits = d["exits"].as_array().cloned().unwrap_or_default();
    if exits.is_empty() {
        return format!("{} 内没有出口数据（路由还没接入、一直离线，或 OpenClash 没有代理出口）", st(d, "/range"));
    }
    let mut rows: Vec<Vec<String>> = exits
        .iter()
        .map(|e| row(if snapshot && e["current"] == false { format!("{}（已不在配置里）", st(e, "/name")) } else { st(e, "/name") }, e))
        .collect();
    let t = &d["total"];
    rows.push(row(format!("合计（{} 个出口）", st(t, "/exits")), t));
    let how = match d["step_s"].as_i64() {
        Some(30) => "原始点：每 5 分钟测一轮（没通的当场重测一次，重测通了算通），连续两轮失败 = 按时间连着 ≥2 轮不通算一段（同告警 router_exits_down）",
        Some(300) => "5 分钟一点：一点一般是一轮测试，这 5 分钟里有一次没通算这一轮没通；连续两轮失败 = 连着 ≥2 点不通算一段；延迟是每点通了的平均",
        _ => "小时汇总：测试、失败次数是准的；看不出先后，连续两轮失败不统计（要看用 --range 24h）；延迟是每小时的平均，最大值偏小",
    };
    format!(
        "{}\n{}：{how}。成功率、延迟只算测了的；当前连接是最近一次采集的",
        table::render(&["出口", "测试", "失败", "成功率", "连续两轮失败", "延迟中位", "延迟最大", "当前连接"], &rows),
        st(d, "/range")
    )
}

/// `router exits <路由> [--range 1h|6h|24h|7d|30d]`：每个出口在这段时间里的测试次数、失败次数、成功率、连续两轮失败的次数、延迟中位数与最大值、当前连接数
pub fn exits(ctx: &Ctx, q: &str, range: &str) -> anyhow::Result<u8> {
    let r = find(&ctx.list("/routers")?, q)?;
    let id = st(&r, "/id");
    let m = ctx.c.get_q(&format!("/routers/{id}/exits/metrics"), &[("range", range.to_string())])?;
    let det = ctx.c.get(&format!("/routers/{id}"))?;
    let current = det["detail"]["exits"].as_array().cloned();
    let mut d = exits_report(range, &m, current.as_deref().unwrap_or_default())?;
    d["router"] = json!({"id": id, "name": r["name"]});
    ctx.emit(&d, || format!("{}\n{}", st(&r, "/name"), exits_human(&d, current.is_some())));
    Ok(0)
}

/// 一键部署的目标（中控要能直接连到路由的 SSH）。密码只从标准输入读，不收参数：写在参数里会进 shell 历史与进程列表
#[derive(clap::Args)]
pub struct SshOpts {
    /// 路由的 IP 或主机名（中控连得到的）；给了就一键部署
    #[arg(long)]
    pub host: Option<String>,
    /// 路由的 SSH 端口（默认 22）
    #[arg(long)]
    pub port: Option<u16>,
    /// 登录用户（默认 root）
    #[arg(long)]
    pub user: Option<String>,
    /// 从标准输入读一行密码（不给就用中控自己的密钥登录：路由上已经授权过中控公钥时）
    #[arg(long)]
    pub password_stdin: bool,
}

impl SshOpts {
    fn body(&self) -> anyhow::Result<Option<Value>> {
        let Some(host) = self.host.as_deref() else {
            if self.port.is_some() || self.user.is_some() || self.password_stdin {
                bail!("--port / --user / --password-stdin 要和 --host 一起用（一键部署）");
            }
            return Ok(None);
        };
        let mut b = json!({"host": host, "port": self.port.unwrap_or(22), "user": self.user.as_deref().unwrap_or("root")});
        if self.password_stdin {
            let mut l = String::new();
            std::io::stdin().lock().read_line(&mut l)?;
            let pw = l.trim_end_matches(['\r', '\n']);
            if pw.is_empty() {
                bail!("--password-stdin 读到的密码是空的");
            }
            b["password"] = json!(pw);
        }
        Ok(Some(b))
    }

    fn target(&self) -> String { format!("{}@{}:{}", self.user.as_deref().unwrap_or("root"), self.host.as_deref().unwrap_or("?"), self.port.unwrap_or(22)) }
}

fn deploy_prompt(name: &str, s: &SshOpts) -> String {
    format!(
        "确认让中控{}登录 {} 给「{name}」装上 fleet-tunnel（只新增：隧道密钥、中控登录公钥、/etc/init.d/fleet-tunnel；不改 root 密码、dropbear / LuCI 监听、OpenClash、防火墙）？",
        if s.password_stdin { "用这个密码" } else { "用自己的密钥" },
        s.target()
    )
}

/// 等一键部署结束，边等边打印步骤；失败退出码 1
fn follow_deploy(ctx: &Ctx, id: &str) -> anyhow::Result<u8> {
    let mut last = String::new();
    for _ in 0..600 {
        let d = ctx.c.get(&format!("/routers/{id}"))?;
        let dep = &d["deploy"];
        let now = format!("{} {}", st(dep, "/state"), st(dep, "/step"));
        if now != last && !ctx.json {
            if let Some(step) = dep["step"].as_str() {
                eprintln!("{step}…");
            }
            last = now;
        }
        match dep["state"].as_str() {
            Some("done") => {
                ctx.emit(&d, || format!("「{}」已部署好，隧道已连上（半分钟内采集到第一份状态）", st(&d, "/name")));
                return Ok(0);
            }
            Some("failed") => {
                if ctx.json {
                    eprintln!("{}", json!({"error": {"code": "deploy_failed", "message": dep["error"], "status": null}}));
                } else {
                    eprintln!("部署失败：{}", st(dep, "/error"));
                    eprintln!("中控连不到路由时改用接入命令：fleetctl router bootstrap {id} --yes，再在路由上执行打印出来的命令");
                }
                return Ok(1);
            }
            _ => std::thread::sleep(Duration::from_millis(1500)),
        }
    }
    bail!("等了 15 分钟部署还没结束：fleetctl router show {id} 看进度")
}

// `fleetctl router add` 的参数（clap 直接解析成它）
#[derive(clap::Args)]
pub struct AddOpts {
    pub name: String,
    #[arg(long)]
    pub team: Option<String>,
    #[arg(long)]
    pub note: Option<String>,
    #[command(flatten)]
    pub ssh: SshOpts,
    /// 一键部署要确认
    #[arg(long)]
    pub yes: bool,
    /// 一键部署：等它结束（失败退出码 1）
    #[arg(long)]
    pub follow: bool,
}

pub fn add(ctx: &Ctx, o: AddOpts) -> anyhow::Result<u8> {
    let mut body = json!({"name": o.name, "note": o.note.as_deref().unwrap_or("")});
    if let Some(t) = o.team.as_deref() {
        body["team_id"] = json!(ctx.team_id(t)?);
    }
    if o.ssh.host.is_some() {
        confirm(&deploy_prompt(&o.name, &o.ssh), o.yes)?;
    }
    if let Some(ssh) = o.ssh.body()? {
        body["ssh"] = ssh;
    }
    let d = ctx.c.post("/routers", &body)?;
    let id = st(&d, "/id");
    if o.ssh.host.is_some() {
        if o.follow {
            return follow_deploy(ctx, &id);
        }
        ctx.emit(&d, || format!("已新建「{}」，正在一键部署：fleetctl router show {id} 看进度（或加 --follow 等它结束）", o.name));
        return Ok(0);
    }
    ctx.emit(&d, || {
        format!(
            "已新建「{}」。在路由上用 root 执行（{} 前有效，只能用一次）：\n\n  {}\n\n执行后半分钟内面板显示在线；过期了用 fleetctl router bootstrap {id} --yes 重新生成",
            o.name,
            table::ts(&st(&d, "/bootstrap/expires_at")),
            st(&d, "/bootstrap/cmd")
        )
    });
    Ok(0)
}

pub fn bootstrap(ctx: &Ctx, q: &str, yes: bool) -> anyhow::Result<u8> {
    let r = find(&ctx.list("/routers")?, q)?;
    let name = st(&r, "/name");
    confirm(&format!("确认给「{name}」重新生成接入命令？它现在的隧道密钥立刻作废（隧道换端口，要在路由上执行新的命令才连得回来）"), yes)?;
    let d = ctx.c.post(&format!("/routers/{}/bootstrap", st(&r, "/id")), &json!({}))?;
    ctx.emit(&d, || format!("在「{name}」上用 root 执行（{} 前有效，只能用一次）：\n\n  {}", table::ts(&st(&d, "/expires_at")), st(&d, "/cmd")));
    Ok(0)
}

pub fn deploy(ctx: &Ctx, q: &str, ssh: SshOpts, yes: bool, follow: bool) -> anyhow::Result<u8> {
    if ssh.host.is_none() {
        bail!("一键部署要给 --host（路由的 IP 或主机名，中控要连得到它的 SSH）");
    }
    let r = find(&ctx.list("/routers")?, q)?;
    let (id, name) = (st(&r, "/id"), st(&r, "/name"));
    confirm(&deploy_prompt(&name, &ssh), yes)?;
    let body = ssh.body()?.ok_or_else(|| anyhow!("缺 --host"))?;
    let d = ctx.c.post(&format!("/routers/{id}/deploy"), &body)?;
    if follow {
        return follow_deploy(ctx, &id);
    }
    ctx.emit(&d, || format!("正在给「{name}」一键部署：fleetctl router show {id} 看进度（或加 --follow 等它结束）"));
    Ok(0)
}

pub fn edit(ctx: &Ctx, q: &str, name: Option<&str>, team: Option<&str>, note: Option<&str>) -> anyhow::Result<u8> {
    if name.is_none() && team.is_none() && note.is_none() {
        bail!("没有要改的内容：给 --name / --team / --note 之一（--team none 清掉团队）");
    }
    let r = find(&ctx.list("/routers")?, q)?;
    let mut body = json!({});
    if let Some(n) = name {
        body["name"] = json!(n);
    }
    match team {
        Some("none") => body["team_id"] = Value::Null,
        Some(t) => body["team_id"] = json!(ctx.team_id(t)?),
        None => {}
    }
    if let Some(n) = note {
        body["note"] = json!(n);
    }
    let d = ctx.c.patch(&format!("/routers/{}", st(&r, "/id")), &body)?;
    ctx.emit(&d, || format!("已修改：{} · 团队 {} · 备注 {}", st(&d, "/name"), d["team"]["name"].as_str().unwrap_or("-"), st(&d, "/note")));
    Ok(0)
}

pub fn rm(ctx: &Ctx, q: &str, yes: bool, force: bool) -> anyhow::Result<u8> {
    let r = find(&ctx.list("/routers")?, q)?;
    let name = st(&r, "/name");
    let forced = if force { "；有没做完的作业改过它的出口的话，作业不再改回它（它上面被改过的出口要自己处理）" } else { "" };
    confirm(&format!("确认删除软路由「{name}」？它的隧道公钥立刻撤销、状态与指标一起删掉，不可恢复{forced}"), yes)?;
    let path = format!("/routers/{}{}", st(&r, "/id"), if force { "?force=true" } else { "" });
    let d = ctx.c.delete(&path)?;
    ctx.emit(&d, || format!("已删除「{name}」{}。路由上中控加的东西没有动，不再接入的话在路由上执行：\n  {}", rm_jobs_note(&d), st(&d, "/cleanup")));
    Ok(0)
}

/// `router sync <路由> [--exit <出口>]…`：参数和对应入口不一致的出口改成入口现在的参数（作业 router_sync：备份、改、重启 OpenClash、测延迟，失败自动改回）。
/// 先 GET 预览；不加 --yes 只打印预览
pub fn sync(ctx: &Ctx, q: &str, exits: &[String], force: bool, yes: bool, follow: bool) -> anyhow::Result<u8> {
    let r = find(&ctx.list("/routers")?, q)?;
    let id = st(&r, "/id");
    let mut query = vec![("exits", exits.join(","))];
    if force {
        query.push(("force", "true".into()));
    }
    let pv = ctx.c.get_q(&format!("/routers/{id}/sync"), &query)?;
    if !ctx.json || !yes {
        if ctx.json {
            ctx.emit(&pv, String::new);
        } else {
            let t = crate::lifecycle::exit_rows(pv["routers"].as_array().map(Vec::as_slice).unwrap_or_default(), None);
            if !t.is_empty() {
                outln(format!("要同步的出口（server:port 不变，换成入口现在的 uuid / SNI / 公钥 / short-id；备份后改，改完重启 OpenClash 并测延迟，失败自动改回）：\n{}", table::render(&["软路由", "状态", "出口", "现在", "改成"], &t)));
            }
            for s in pv["skipped"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                outln(format!("同步不了（改指向别的入口：fleetctl repoint）：{s}"));
            }
            for u in pv["untrusted"].as_array().into_iter().flatten() {
                outln(format!("未自动同步（从没对上过这个入口，可能是路由上有人加的；确认是自己的就 --exit 点名）：{}（{}）", st(u, "/exit"), st(u, "/entry")));
            }
            for u in pv["need_force"].as_array().into_iter().flatten() {
                outln(format!("{}（{}）从没对上过，入口是{}：{}", st(u, "/exit"), st(u, "/entry"), st(u, "/why"), if force { "已加 --force，照同步" } else { "要同步加 --force" }));
            }
            for w in pv["warnings"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                outln(format!("注意：{w}"));
            }
        }
    }
    if pv["ok"] != true {
        bail!("现在不能同步：{}", crate::lifecycle::blockers(&pv));
    }
    if !yes {
        if !ctx.json {
            outln("以上是预览，没有改动。确认无误后加 --yes 执行（--follow 跟进度）");
        }
        return Ok(0);
    }
    let d = ctx.c.post(&format!("/routers/{id}/sync"), &json!({"exits": exits, "force": force}))?;
    ctx.emit(&d, || format!("已提交同步作业 {}（{}）\n看进度：fleetctl job {} --follow", st(&d, "/job_id"), st(&r, "/name"), st(&d, "/job_id")));
    if follow {
        return crate::cmds::follow_job(ctx, &st(&d, "/job_id"));
    }
    Ok(0)
}

/// `router remove-exits <路由> --exit <出口>…`：出口连同指向它的分流规则、策略组里的引用一起从配置里删掉（作业 router_exit_rm：
/// 备份、删、重启 OpenClash、确认生效，失败整份换回）。先 GET 预览；不加 --yes 只打印预览
pub fn remove_exits(ctx: &Ctx, q: &str, exits: &[String], force: bool, yes: bool, follow: bool) -> anyhow::Result<u8> {
    let r = find(&ctx.list("/routers")?, q)?;
    let id = st(&r, "/id");
    let exits: Vec<String> = exits.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let mut query = vec![("exits", exits.join(","))];
    if force {
        query.push(("force", "true".into()));
    }
    let pv = ctx.c.get_q(&format!("/routers/{id}/exits/remove"), &query)?;
    if !ctx.json || !yes {
        if ctx.json {
            ctx.emit(&pv, String::new);
        } else {
            let rows: Vec<Vec<String>> = pv["exits"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|e| {
                    let addr = match (e["server"].as_str(), e["port"].as_u64()) {
                        (Some(s), Some(p)) => format!("{s}:{p}"),
                        _ => "-".into(),
                    };
                    vec![st(e, "/exit"), e["entry"].as_str().unwrap_or("（不是面板的入口）").to_string(), addr, e["clients"].as_i64().unwrap_or(0).to_string()]
                })
                .collect();
            if !rows.is_empty() {
                outln(format!(
                    "「{}」要删的出口（连同指向它们的分流规则和策略组里的引用；备份后删，删完重启 OpenClash，失败整份换回）：
{}",
                    st(&pv, "/router/name"),
                    table::render(&["出口", "对应入口", "地址", "客户端"], &rows)
                ));
            }
            for w in pv["warnings"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                outln(format!("注意：{w}"));
            }
        }
    }
    if pv["ok"] != true {
        bail!("现在不能删：{}", crate::lifecycle::blockers(&pv));
    }
    if !yes {
        if !ctx.json {
            outln("以上是预览，没有改动。确认无误后加 --yes 执行（--follow 跟进度）");
        }
        return Ok(0);
    }
    let d = ctx.c.post(&format!("/routers/{id}/exits/remove"), &json!({"exits": exits, "force": force}))?;
    ctx.emit(&d, || format!("已提交删出口作业 {}（{}）
看进度：fleetctl job {} --follow", st(&d, "/job_id"), st(&r, "/name"), st(&d, "/job_id")));
    if follow {
        return crate::cmds::follow_job(ctx, &st(&d, "/job_id"));
    }
    Ok(0)
}

/// 出口协议一个词：快照的 proto（reality / anytls），旧中控没有时退回 mihomo 报的类型
fn proto_cn(e: &Value) -> String {
    match e["proto"].as_str() {
        Some("anytls") => "AnyTLS".into(),
        Some("reality") => "Reality".into(),
        _ => e["type"].as_str().unwrap_or("-").to_string(),
    }
}

/// `router protocol` 预览的文字：要切的出口（出口、对应入口、线路机、现在 → 改成），跳过的一行一个写原因
fn protocol_human(pv: &Value) -> String {
    let to = if pv["to"] == "anytls" { "AnyTLS" } else { "Reality" };
    let mut out = vec![];
    let rows: Vec<Vec<String>> = pv["exits"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            let from = if e["from_proto"] == "anytls" { "AnyTLS" } else { "Reality" };
            vec![st(e, "/exit"), st(e, "/entry"), st(e, "/relay"), format!("{from} {}", st(e, "/from")), format!("{to} {}", st(e, "/to"))]
        })
        .collect();
    if !rows.is_empty() {
        out.push(format!(
            "「{}」要切成 {to} 的出口（名字、分组、规则不变；备份后改，改完重启 OpenClash 并测延迟，任何失败全部改回）：\n{}",
            st(pv, "/router/name"),
            table::render(&["出口", "对应入口", "线路机", "现在", "改成"], &rows)
        ));
    }
    for s in pv["skipped"].as_array().into_iter().flatten() {
        out.push(format!("跳过 {}（{}，线路机 {}）：{}", st(s, "/exit"), st(s, "/entry"), st(s, "/relay"), st(s, "/why")));
    }
    for w in pv["warnings"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        out.push(format!("注意：{w}"));
    }
    out.join("\n")
}

/// `router protocol <路由> --to anytls|reality [--exit <出口>]…`：指向面板入口的出口整块改写成另一种协议（作业 router_protocol：
/// 备份、改写、重启 OpenClash、测延迟，失败全部改回）。先 GET 预览；不加 --yes 只打印预览
pub fn protocol(ctx: &Ctx, q: &str, to: &str, exits: &[String], yes: bool, follow: bool) -> anyhow::Result<u8> {
    let r = find(&ctx.list("/routers")?, q)?;
    let id = st(&r, "/id");
    let pv = ctx.c.get_q(&format!("/routers/{id}/protocol"), &[("to", to.to_string()), ("exits", exits.join(","))])?;
    if !ctx.json || !yes {
        if ctx.json {
            ctx.emit(&pv, String::new);
        } else {
            let s = protocol_human(&pv);
            if !s.is_empty() {
                outln(s);
            }
        }
    }
    if pv["ok"] != true {
        bail!("现在不能切：{}", crate::lifecycle::blockers(&pv));
    }
    if !yes {
        if !ctx.json {
            outln("以上是预览，没有改动。确认无误后加 --yes 执行（--follow 跟进度）");
        }
        return Ok(0);
    }
    let d = ctx.c.post(&format!("/routers/{id}/protocol"), &json!({"to": to, "exits": exits}))?;
    ctx.emit(&d, || format!("已提交切协议作业 {}（{}）\n看进度：fleetctl job {} --follow", st(&d, "/job_id"), st(&r, "/name"), st(&d, "/job_id")));
    if follow {
        return crate::cmds::follow_job(ctx, &st(&d, "/job_id"));
    }
    Ok(0)
}

/// `router switch <路由> <组> <出口> [--for 4h]` / `router switch <路由> <组> --back`：select 组经 mihomo API 临时切到组里另一个出口
/// （不重启 OpenClash、不断连接），`--for` 到时自动切回；`--back` 切回第一次切之前的出口
pub fn switch(ctx: &Ctx, q: &str, group: &str, to: Option<&str>, dur: Option<&str>, back: bool) -> anyhow::Result<u8> {
    let r = find(&ctx.list("/routers")?, q)?;
    let (id, name) = (st(&r, "/id"), st(&r, "/name"));
    let d = match (back, to) {
        (true, None) if dur.is_none() => ctx.c.post(&format!("/routers/{id}/switch/back"), &json!({"group": group}))?,
        (true, _) => bail!("--back 不带出口和 --for"),
        (false, None) => bail!("要给切到哪个出口（组里已有的），或 --back 切回"),
        (false, Some(to)) => {
            let secs = dur.map(crate::silence::parse_for).transpose()?.map(|d| d.whole_seconds());
            ctx.c.post(&format!("/routers/{id}/switch"), &json!({"group": group, "to": to, "back_after_s": secs}))?
        }
    };
    ctx.emit(&d, || {
        let mut s = format!("「{name}」的策略组「{group}」：{} → {}", st(&d, "/from"), st(&d, "/to"));
        if d["ended"] == true {
            s.push_str("（已切回原来的出口）");
        } else if let Some(t) = d["back_at"].as_str() {
            s.push_str(&format!("，{} 自动切回 {}", table::ts(t), st(&d, "/original")));
        } else {
            s.push_str(&format!("。不会自动切回：要切回时 fleetctl router switch {} {} --back", crate::ops::shell_quote(&name), crate::ops::shell_quote(&group)));
        }
        s.push_str("。已有的连接不受影响，新连接走新出口；OpenClash 重启后一般保留当前选择，到时仍由中控切回");
        s
    });
    Ok(0)
}

/// 强制删除时摘掉了哪些作业的改回：「（作业 x、y 不再改回它，可以重试收尾或取消）」，没有为空
fn rm_jobs_note(d: &Value) -> String {
    let ids: Vec<&str> = d["jobs"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if ids.is_empty() {
        return String::new();
    }
    format!("（作业 {} 不再改回它，可以重试收尾或取消）", ids.join("、"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routers() -> Vec<Value> {
        vec![
            json!({"id": "01a162a9-39c4-752c-b647-e61f34ef48bd", "name": "团队2-路由A", "state": "online"}),
            json!({"id": "01a162a9-4000-752c-b647-e61f34ef48bd", "name": "团队2-路由B", "state": "pending"}),
        ]
    }

    /// 变更时间线：作业来源写类型与作业 id、路由上改的单独标明；出口新增 / 删除 / 地址与凭据变了；比不出差异时写原因
    #[test]
    fn changes_timeline_text() {
        let list = [
            json!({"at": "2026-10-24T04:30:56Z", "old_sha": "aaaaaaaa11", "new_sha": "bbbbbbbb22", "source": "job", "job_id": "J1", "job_kind": "router_sync",
                   "diff": [{"exit": "节点01", "op": "changed", "from": "a:1", "to": "b:2", "creds": true}, {"exit": "节点03", "op": "added", "to": "c:3"}, {"exit": "节点02", "op": "removed", "from": "d:4"}]}),
            json!({"at": "2026-10-24T04:00:00Z", "old_sha": "cccccccc33", "new_sha": "aaaaaaaa11", "source": "router", "diff": [], "note": null}),
        ];
        let s = changes_human(&list);
        assert!(s.contains("J1  配置 aaaaaaaa → bbbbbbbb") && s.contains("  节点01  server:port a:1 → b:2；凭据变了
  节点03  新增 c:3
  节点02  删除 d:4"), "{s}");
        assert!(s.contains("路由上改的  配置 cccccccc → aaaaaaaa
  出口没有变化"), "{s}");
    }

    /// 在线却带原因 = 连得上但采集出错；配置读不了时写明出口按上次的配置对应；延迟测试没做成写原因；时钟偏差写明符号
    #[test]
    fn show_collect_error_and_stale_config() {
        let d = json!({
            "name": "R", "state": "online", "error": "采集脚本出错（退出码 1）：x",
            "detail": {"system": {"os": "OpenWrt", "clock_skew_s": 3}, "openclash": {"enabled": true, "running": true, "config_error": "配置文件解析不了：y", "config_stale": true, "delay_error": "延迟测试：ssh 执行超过 45 秒"}}
        });
        let s = human_show(&d);
        assert!(s.contains("采集出错") && s.contains("采集脚本出错（退出码 1）：x") && !s.contains("原因"), "{s}");
        assert!(s.contains("配置文件解析不了：y（出口按上次读到的配置对应入口） · 最近一次延迟测试没做成：延迟测试：ssh 执行超过 45 秒"), "{s}");
        assert!(s.contains("时钟偏差 3 秒（中控减路由，正数 = 路由慢）"), "{s}");
    }

    /// 策略组表：没有规则用的组写「未被规则引用」；切换中的写原来是谁，原出口还没恢复而推迟的写下次检查与原因
    #[test]
    fn show_groups_unused_and_deferred() {
        let d = json!({
            "name": "R", "state": "online",
            "detail": {"groups": [
                {"name": "GLOBAL", "type": "Selector", "now": "DIRECT", "all": ["DIRECT", "节点01"], "used": false},
                {"name": "手动选择", "type": "Selector", "now": "DIRECT", "all": ["DIRECT", "节点01"], "used": true},
                {"name": "旧快照", "type": "Selector", "now": "节点01", "all": ["节点01"]},
            ]},
            "switches": [{"group": "手动选择", "from": "节点01", "to": "DIRECT", "back_at": "2026-10-24T04:30:56Z", "defer_why": "原出口「节点01」现在不通"}],
        });
        let s = human_show(&d);
        assert!(s.contains("未被规则引用（切了不影响流量）"), "{s}");
        assert!(s.contains("临时切换中（原来是 节点01，等原出口恢复后再切回，下次检查 ") && s.contains("\n手动选择：原出口「节点01」现在不通"), "{s}");
        assert_eq!(s.matches("未被规则引用").count(), 1, "{s}");
    }

    fn summary(exits: Value) -> Value {
        json!({"exits": exits, "clients": 7, "cpu_pct": 12.4, "mem_used": 1, "mem_total": 4, "wan": {"up": true, "rx_bps": 1000, "tx_bps": 2000}})
    }

    /// 切协议预览：要切的出口写对应入口、线路机、现在 → 改成（协议 + 地址）；跳过的写原因；出口表的协议列
    #[test]
    fn protocol_preview_text() {
        let pv = json!({"router": {"name": "团队2-路由A"}, "to": "anytls",
            "exits": [{"exit": "节点01", "entry": "团队2-01 · 线路机2:20003", "relay": "线路机2", "from": "relay.test:20003", "to": "relay.test:40003", "from_proto": "reality", "to_proto": "anytls"}],
            "skipped": [{"exit": "节点02", "entry": "团队2-02 · 线路机1:20004", "relay": "线路机1", "why": "线路机 线路机1 只开 Reality"}],
            "warnings": ["改完要重启 OpenClash：经这台路由的连接会全部断开、马上重连"]});
        let s = protocol_human(&pv);
        assert!(s.starts_with("「团队2-路由A」要切成 AnyTLS 的出口") && s.contains("Reality relay.test:20003") && s.contains("AnyTLS relay.test:40003") && s.contains("线路机2"), "{s}");
        assert!(s.contains("\n跳过 节点02（团队2-02 · 线路机1:20004，线路机 线路机1）：线路机 线路机1 只开 Reality\n注意：改完要重启 OpenClash"), "{s}");
        assert_eq!((proto_cn(&json!({"proto": "anytls"})), proto_cn(&json!({"proto": "reality", "type": "Vless"})), proto_cn(&json!({"type": "Shadowsocks"}))), ("AnyTLS".into(), "Reality".into(), "Shadowsocks".to_string()));
    }

    /// 出口通/总只数 fleet 出口（客户自己加的单列「其他出口」）；旧中控的摘要没有 fleet 字段，退回全部出口
    #[test]
    fn exits_cell_counts_fleet_exits_apart() {
        let new = summary(json!({"total": 20, "alive": 19, "dead": ["机场A"], "managed": 18, "drift": 1, "fleet": {"total": 18, "alive": 18, "dead": []}, "other": 2}));
        assert_eq!(exits_cell(&new), "18/18（1 个不一致） · 其他出口 2");
        let old = summary(json!({"total": 20, "alive": 19, "dead": ["机场A"], "managed": 18, "drift": 0}));
        assert_eq!(exits_cell(&old), "19/20");
        let r = json!({"name": "R", "state": "online", "summary": new});
        assert_eq!(row(&r)[3], "18/18（1 个不一致） · 其他出口 2");
    }

    /// 离线的路由：出口、客户端、CPU、内存、WAN 速率显示 -（掉线前的读数不当现状）；--json 的 stale 标出来；show 顶部写多久前读到的
    #[test]
    fn offline_router_readings_are_stale() {
        let s = summary(json!({"total": 2, "alive": 2, "dead": [], "managed": 2, "drift": 0}));
        let off = json!({"name": "R", "state": "offline", "summary": s, "last_seen_at": "2026-10-24T04:30:56Z"});
        let on = json!({"name": "R", "state": "online", "summary": s});
        let pending = json!({"name": "R", "state": "pending", "summary": null});
        assert!(stale(&off) && !stale(&on) && !stale(&pending));
        // 名称 团队 状态 | 出口 客户端 CPU 内存 WAN | 最近在线
        assert_eq!(row(&off)[3..8], ["-", "-", "-", "-", "-"]);
        assert_eq!(row(&on)[3..8], ["2/2", "7", "12%", "25%", "↓1.0 Kbps ↑2.0 Kbps"].map(String::from)[..], "{:?}", row(&on));
        let d = json!({"name": "R", "state": "offline", "last_seen_at": "2026-10-24T04:30:56Z", "detail": {"collected_at": "2026-10-24T04:30:56Z", "system": {"os": "OpenWrt"}}});
        let text = human_show(&d);
        assert!(text.starts_with("以下是") && text.contains("前读到的（路由现在连不上，读数不是现状）\n\n"), "{text}");
        assert!(!human_show(&json!({"name": "R", "state": "online", "detail": {"system": {"os": "OpenWrt"}}})).contains("以下是"));
        assert_eq!(since_cn("2999-01-01T00:00:00Z").as_deref(), Some("不到 1 分钟"));
    }

    #[test]
    fn find_by_id_prefix_and_name() {
        let all = routers();
        assert_eq!(find(&all, "团队2-路由b").unwrap()["state"], "pending", "名称不分大小写");
        assert!(find(&all, "01a162a9").unwrap_err().to_string().contains("匹配到 2 个软路由"));
    }

    #[test]
    fn ssh_options_need_host() {
        let o = SshOpts { host: None, port: Some(2222), user: None, password_stdin: false };
        assert!(o.body().is_err());
        let o = SshOpts { host: Some("10.0.0.1".into()), port: None, user: None, password_stdin: false };
        assert_eq!(o.body().unwrap().unwrap(), json!({"host": "10.0.0.1", "port": 22, "user": "root"}));
        assert_eq!(o.target(), "root@10.0.0.1:22");
    }

    /// 原始点（1h）：没测的点（alive null）跳过、不打断；连着 ≥2 轮不通算一段
    #[test]
    fn raw_points_streaks() {
        let p = |a: Option<bool>, d: Option<i64>| json!({"alive": a, "delay_ms": d, "tests": i64::from(a.is_some()), "tests_ok": i64::from(a == Some(true))});
        let (t, f) = (Some(true), Some(false));
        let pts = [p(t, Some(80)), p(None, None), p(f, None), p(None, None), p(f, None), p(f, None), p(t, Some(120)), p(f, None), p(t, Some(100)), p(f, None), p(f, None)];
        let x = exit_stats(30, &pts).unwrap();
        assert_eq!((x.tests, x.fails, x.streaks), (9, 6, Some(2)), "f·f·f 算一段，单独一次不算，结尾的 f·f 算一段");
        assert_eq!(delay_json(&x.delays), (json!(100), json!(120)));
        assert_eq!(exit_stats(30, &[]).unwrap(), ExitStats { streaks: Some(0), ..Default::default() });
    }

    /// 5 分钟点：次数按 tests / tests_ok（一点可能 0 或 2 次），一点算一轮；小时点：次数准、连续失败为 None；点里缺 tests 报错
    #[test]
    fn aggregated_points() {
        let pts = [
            json!({"alive": true, "delay_ms": 90.5, "tests": 1, "tests_ok": 1}),
            json!({"alive": false, "delay_ms": 70.0, "tests": 2, "tests_ok": 1}),
            json!({"alive": null, "delay_ms": null, "tests": 0, "tests_ok": 0}),
            json!({"alive": false, "delay_ms": null, "tests": 1, "tests_ok": 0}),
        ];
        let x = exit_stats(300, &pts).unwrap();
        assert_eq!((x.tests, x.fails, x.streaks), (4, 2, Some(1)));
        assert_eq!(delay_json(&x.delays), (json!(80), json!(91)), "偶数个取中间两个的平均");
        let hours = [json!({"alive": false, "alive_pct": 91.6, "delay_ms": 85.0, "tests": 12, "tests_ok": 11}), json!({"alive": true, "alive_pct": 100.0, "delay_ms": 95.0, "tests": 11, "tests_ok": 11})];
        let x = exit_stats(3600, &hours).unwrap();
        assert_eq!((x.tests, x.fails, x.streaks), (23, 1, None));
        assert!(exit_stats(3600, &[json!({"alive": true, "alive_pct": 100.0, "delay_ms": 95.0})]).unwrap_err().to_string().contains("缺 tests"));
    }

    #[test]
    fn exits_report_and_table() {
        let m = json!({"step_s": 30, "exits": [
            {"name": "节点01", "points": [{"alive": false, "tests": 1, "tests_ok": 0}, {"alive": false, "tests": 1, "tests_ok": 0}, {"alive": true, "delay_ms": 60, "tests": 1, "tests_ok": 1}]},
            {"name": "旧出口", "points": [{"alive": true, "delay_ms": 200, "tests": 1, "tests_ok": 1}]},
        ]});
        let current = [json!({"name": "节点01", "conns": 5}), json!({"name": "节点02", "conns": 0})];
        let d = exits_report("1h", &m, &current).unwrap();
        assert_eq!(d["exits"][0], json!({"name": "节点01", "tests": 3, "fails": 2, "success_pct": 33.3, "fail_streaks": 1, "delay_median_ms": 60, "delay_max_ms": 60, "conns": 5, "current": true}));
        assert_eq!((d["exits"][1]["conns"].is_null(), &d["exits"][1]["current"]), (true, &json!(false)));
        assert_eq!(d["exits"][2], json!({"name": "节点02", "tests": 0, "fails": 0, "success_pct": null, "fail_streaks": 0, "delay_median_ms": null, "delay_max_ms": null, "conns": 0, "current": true}));
        assert_eq!(d["total"], json!({"exits": 3, "tests": 4, "fails": 2, "success_pct": 50.0, "fail_streaks": 1, "delay_median_ms": 130, "delay_max_ms": 200, "conns": 5}));
        let t = exits_human(&d, true);
        let lines: Vec<&str> = t.lines().collect();
        assert!(lines[1].starts_with("节点01") && lines[1].contains("33.3%") && lines[1].contains("60 ms"), "{t}");
        assert!(lines[2].starts_with("旧出口（已不在配置里）"), "{t}");
        assert!(lines[4].starts_with("合计（3 个出口）") && lines[4].contains("50%"), "{t}");
        // 小时汇总：连续失败为 null，表格写「-」
        let h = exits_report("7d", &json!({"step_s": 3600, "exits": [{"name": "节点01", "points": [{"alive": true, "tests": 12, "tests_ok": 12, "delay_ms": 80.0}]}]}), &[]).unwrap();
        assert_eq!((h["exits"][0]["fail_streaks"].is_null(), h["total"]["fail_streaks"].is_null(), h["total"]["success_pct"].as_f64()), (true, true, Some(100.0)));
        assert!(exits_human(&h, false).contains("看不出先后"));
    }
}
