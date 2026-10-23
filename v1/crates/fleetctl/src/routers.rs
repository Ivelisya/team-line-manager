//! 软路由：routers（列表）、router show / add / bootstrap / deploy / edit / rm。
//! 接入有两种：`router add` 不给 --host 打印接入命令（在路由上用 root 执行，路由在 NAT 后面、中控连不到它时用）；
//! 给 --host 就一键部署（中控能直接连到路由的 SSH 时用：中控登录上去装好隧道；密码只从标准输入读）。

use crate::cmds::{num, st, Ctx};
use crate::ops::{confirm, lookup};
use crate::table::{self, bps, bytes};
use anyhow::{anyhow, bail};
use serde_json::{json, Value};
use std::io::BufRead;
use std::time::Duration;

fn state_cn(s: &str) -> &str {
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

fn pct(a: Option<f64>, b: Option<f64>) -> String {
    match (a, b) {
        (Some(a), Some(b)) if b > 0.0 => format!("{:.0}%", a * 100.0 / b),
        _ => "-".into(),
    }
}

fn list(ctx: &Ctx) -> anyhow::Result<Vec<Value>> { Ok(ctx.c.get("/routers")?.as_array().cloned().unwrap_or_default()) }

/// 找一台路由：id、id 前缀（至少 6 位）、名称（不分大小写）
fn find(all: &[Value], q: &str) -> anyhow::Result<Value> {
    let q = q.trim();
    let ql = q.to_lowercase();
    let mut hits: Vec<&Value> = all.iter().filter(|r| st(r, "/id") == ql).collect();
    if hits.is_empty() && ql.len() >= 6 && ql.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        hits = all.iter().filter(|r| st(r, "/id").starts_with(&ql)).collect();
    }
    if hits.is_empty() {
        hits = all.iter().filter(|r| st(r, "/name").to_lowercase() == ql).collect();
    }
    match hits.len() {
        1 => Ok(hits[0].clone()),
        0 => bail!("没有找到软路由「{q}」（可用 id、id 前缀、名称；先 fleetctl routers 看列表）"),
        n => {
            let lines: Vec<String> = hits.iter().map(|r| format!("  {}  (id {})", st(r, "/name"), st(r, "/id"))).collect();
            bail!("「{q}」匹配到 {n} 台软路由，请用 id 指定：\n{}", lines.join("\n"))
        }
    }
}

fn row(r: &Value) -> Vec<String> {
    let s = &r["summary"];
    let exits = if s.is_object() {
        let drift = s["exits"]["drift"].as_i64().unwrap_or(0);
        format!("{}/{}{}", st(s, "/exits/alive"), st(s, "/exits/total"), if drift > 0 { format!("（{drift} 个不一致）") } else { String::new() })
    } else {
        "-".into()
    };
    let wan = match s.get("wan").filter(|w| w.is_object()) {
        Some(w) if w["up"] == false => "断开".to_string(),
        Some(w) => format!("↓{} ↑{}", bps(num(w, "/rx_bps").unwrap_or(0.0)), bps(num(w, "/tx_bps").unwrap_or(0.0))),
        None => "-".into(),
    };
    let mut state = state_cn(&st(r, "/state")).to_string();
    if let Some(d) = r["deploy"]["state"].as_str().filter(|d| *d != "done") {
        state.push_str(&format!("（{}）", deploy_cn(d)));
    }
    vec![
        st(r, "/name"),
        r["team"]["name"].as_str().unwrap_or("-").to_string(),
        state,
        exits,
        st(s, "/clients"),
        num(s, "/cpu_pct").map(|c| format!("{c:.0}%")).unwrap_or_else(|| "-".into()),
        pct(num(s, "/mem_used"), num(s, "/mem_total")),
        wan,
        table::ts(&st(r, "/last_seen_at")),
        st(r, "/id").chars().take(13).collect(),
    ]
}

const HEAD: [&str; 10] = ["名称", "团队", "状态", "出口通/总", "客户端", "CPU", "内存", "WAN", "最近在线", "id"];

pub fn ls(ctx: &Ctx, team: Option<&str>) -> anyhow::Result<u8> {
    let mut all = list(ctx)?;
    if let Some(t) = team {
        all.retain(|r| r["team"]["name"].as_str() == Some(t) || st(r, "/team/id") == t);
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
    let r = find(&list(ctx)?, q)?;
    let d = ctx.c.get(&format!("/routers/{}", st(&r, "/id")))?;
    ctx.emit(&d, || human_show(&d));
    Ok(0)
}

fn human_show(d: &Value) -> String {
    let det = &d["detail"];
    let y = &det["system"];
    let oc = &det["openclash"];
    let mut kv: Vec<(&str, String)> = vec![
        ("名称", st(d, "/name")),
        ("团队", d["team"]["name"].as_str().unwrap_or("-").into()),
        ("状态", state_cn(&st(d, "/state")).into()),
        ("最近在线", table::ts(&st(d, "/last_seen_at"))),
        ("隧道端口", st(d, "/tunnel_port")),
    ];
    if let Some(e) = d["error"].as_str() {
        kv.push(("原因", e.into()));
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
        kv.push(("运行", format!("{up} · {} 个进程 · 温度 {} · 时钟偏差 {} 秒", st(y, "/procs"), num(y, "/temp_c").map(|t| format!("{t:.0}℃")).unwrap_or("-".into()), st(y, "/clock_skew_s"))));
    }
    if oc.is_object() {
        let run = match (oc["enabled"] == true, oc["running"] == true) {
            (_, true) => "运行中",
            (true, false) => "已启用但没在运行",
            (false, false) => "未启用",
        };
        let mut s = format!("{run} · {} · 模式 {} · {} 个连接 · ↑{} ↓{}", st(oc, "/version"), st(oc, "/mode"), st(oc, "/conns"), bps(num(oc, "/up_bps").unwrap_or(0.0)), bps(num(oc, "/down_bps").unwrap_or(0.0)));
        for k in ["error", "config_error"] {
            if let Some(e) = oc[k].as_str() {
                s.push_str(&format!(" · {e}"));
            }
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
                    format!("↓{} ↑{}", bps(num(i, "/rx_bps").unwrap_or(0.0)), bps(num(i, "/tx_bps").unwrap_or(0.0))),
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
                    format!("{}:{}", st(e, "/server"), st(e, "/port")),
                    e["entry"]["name"].as_str().unwrap_or("-").into(),
                    sync_cn(&st(e, "/sync")).into(),
                    alive,
                    st(e, "/conns"),
                    st(e, "/clients"),
                    format!("↓{} ↑{}", bps(num(e, "/down_bps").unwrap_or(0.0)), bps(num(e, "/up_bps").unwrap_or(0.0))),
                ]
            })
            .collect();
        out.push_str(&format!("\n\n出口\n{}", table::render(&["出口", "地址", "对应入口", "参数", "延迟", "连接", "客户端", "速率"], &rows)));
    }
    if let Some(cl) = det["clients"].as_array().filter(|a| !a.is_empty()) {
        let rows: Vec<Vec<String>> = cl
            .iter()
            .map(|c| {
                let ex = c["exits"].as_array().map(|a| a.iter().map(|x| format!("{}×{}", st(x, "/name"), st(x, "/conns"))).collect::<Vec<_>>().join(" ")).unwrap_or_default();
                let rate = format!("↓{} ↑{}", bps(num(c, "/down_bps").unwrap_or(0.0)), bps(num(c, "/up_bps").unwrap_or(0.0)));
                vec![st(c, "/ip"), st(c, "/hostname"), st(c, "/mac"), if c["online"] == true { "在线".into() } else { "离线".into() }, st(c, "/conns"), rate, ex]
            })
            .collect();
        out.push_str(&format!("\n\n客户端\n{}", table::render(&["IP", "主机名", "MAC", "状态", "连接", "速率", "出口"], &rows)));
    }
    out
}

/// 一键部署的目标：密码只从标准输入读一行（不能写在参数里：会进 shell 历史与进程列表）
pub struct SshOpts<'a> {
    pub host: Option<&'a str>,
    pub port: Option<u16>,
    pub user: Option<&'a str>,
    pub password_stdin: bool,
}

impl SshOpts<'_> {
    fn body(&self) -> anyhow::Result<Option<Value>> {
        let Some(host) = self.host else {
            if self.port.is_some() || self.user.is_some() || self.password_stdin {
                bail!("--port / --user / --password-stdin 要和 --host 一起用（一键部署）");
            }
            return Ok(None);
        };
        let mut b = json!({"host": host, "port": self.port.unwrap_or(22), "user": self.user.unwrap_or("root")});
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

    fn target(&self) -> String { format!("{}@{}:{}", self.user.unwrap_or("root"), self.host.unwrap_or("?"), self.port.unwrap_or(22)) }
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

pub struct AddOpts<'a> {
    pub name: &'a str,
    pub team: Option<&'a str>,
    pub note: Option<&'a str>,
    pub ssh: SshOpts<'a>,
    pub yes: bool,
    pub follow: bool,
}

pub fn add(ctx: &Ctx, o: AddOpts<'_>) -> anyhow::Result<u8> {
    let mut body = json!({"name": o.name, "note": o.note.unwrap_or("")});
    if let Some(t) = o.team {
        body["team_id"] = json!(lookup(&ctx.teams()?, t, &["name"], "团队")?);
    }
    if o.ssh.host.is_some() {
        confirm(&deploy_prompt(o.name, &o.ssh), o.yes)?;
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
    let r = find(&list(ctx)?, q)?;
    let name = st(&r, "/name");
    confirm(&format!("确认给「{name}」重新生成接入命令？它现在的隧道密钥立刻作废（隧道换端口，要在路由上执行新的命令才连得回来）"), yes)?;
    let d = ctx.c.post(&format!("/routers/{}/bootstrap", st(&r, "/id")), &json!({}))?;
    ctx.emit(&d, || format!("在「{name}」上用 root 执行（{} 前有效，只能用一次）：\n\n  {}", table::ts(&st(&d, "/expires_at")), st(&d, "/cmd")));
    Ok(0)
}

pub fn deploy(ctx: &Ctx, q: &str, ssh: SshOpts<'_>, yes: bool, follow: bool) -> anyhow::Result<u8> {
    if ssh.host.is_none() {
        bail!("一键部署要给 --host（路由的 IP 或主机名，中控要连得到它的 SSH）");
    }
    let r = find(&list(ctx)?, q)?;
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
    let r = find(&list(ctx)?, q)?;
    let mut body = json!({});
    if let Some(n) = name {
        body["name"] = json!(n);
    }
    match team {
        Some("none") => body["team_id"] = Value::Null,
        Some(t) => body["team_id"] = json!(lookup(&ctx.teams()?, t, &["name"], "团队")?),
        None => {}
    }
    if let Some(n) = note {
        body["note"] = json!(n);
    }
    let d = ctx.c.patch(&format!("/routers/{}", st(&r, "/id")), &body)?;
    ctx.emit(&d, || format!("已修改：{} · 团队 {} · 备注 {}", st(&d, "/name"), d["team"]["name"].as_str().unwrap_or("-"), st(&d, "/note")));
    Ok(0)
}

pub fn rm(ctx: &Ctx, q: &str, yes: bool) -> anyhow::Result<u8> {
    let r = find(&list(ctx)?, q)?;
    let name = st(&r, "/name");
    confirm(&format!("确认删除软路由「{name}」？它的隧道公钥立刻撤销、状态与指标一起删掉，不可恢复"), yes)?;
    let d = ctx.c.delete(&format!("/routers/{}", st(&r, "/id")))?;
    ctx.emit(&d, || format!("已删除「{name}」。路由上中控加的东西没有动，不再接入的话在路由上执行：\n  {}", st(&d, "/cleanup")));
    Ok(0)
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

    #[test]
    fn find_by_id_prefix_and_name() {
        let all = routers();
        assert_eq!(find(&all, "团队2-路由b").unwrap()["state"], "pending", "名称不分大小写");
        assert_eq!(find(&all, "01a162a9-39").unwrap()["name"], "团队2-路由A");
        assert!(find(&all, "01a162a9").unwrap_err().to_string().contains("匹配到 2 台"));
        assert!(find(&all, "路由C").unwrap_err().to_string().contains("没有找到"));
    }

    #[test]
    fn list_row_wording() {
        let r = json!({
            "id": "01a162a9-39c4-752c-b647-e61f34ef48bd", "name": "R", "team": {"name": "团队2"}, "state": "offline", "last_seen_at": "2026-10-22T08:10:54Z",
            "deploy": {"state": "failed"},
            "summary": {"cpu_pct": 3.2, "mem_used": 1, "mem_total": 4, "clients": 3, "exits": {"alive": 4, "total": 5, "drift": 1}, "wan": {"up": true, "rx_bps": 40000, "tx_bps": 12000}}
        });
        assert_eq!(row(&r), ["R", "团队2", "离线（部署失败）", "4/5（1 个不一致）", "3", "3%", "25%", "↓40.0 Kbps ↑12.0 Kbps", "2026-10-22 16:10:54", "01a162a9-39c4"]);
        assert_eq!(row(&json!({"id": "x", "name": "P", "state": "pending", "summary": null}))[2..5], ["等待接入", "-", "-"]);
    }

    #[test]
    fn ssh_options_need_host() {
        let o = SshOpts { host: None, port: Some(2222), user: None, password_stdin: false };
        assert!(o.body().is_err());
        let o = SshOpts { host: Some("10.0.0.1"), port: None, user: None, password_stdin: false };
        assert_eq!(o.body().unwrap().unwrap(), json!({"host": "10.0.0.1", "port": 22, "user": "root"}));
        assert_eq!(o.target(), "root@10.0.0.1:22");
    }

    #[test]
    fn uptime_wording() {
        assert_eq!([uptime_cn(420), uptime_cn(7500), uptime_cn(3 * 86400 + 4 * 3600 + 59)], ["已运行 7 分钟", "已运行 2 小时 5 分钟", "已运行 3 天 4 小时"]);
    }
}
