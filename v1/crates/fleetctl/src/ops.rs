//! 会改动状态的命令：上架、退役、改名、升级、入口操作、远程执行、密钥管理。
//! 危险操作默认要确认：交互终端里问 y/N，非交互环境必须显式 --yes。

use crate::client::{ApiError, Client, Cred};
use crate::cmds::{self, outln, out_raw, st, Ctx};
use crate::resolve::{self, Want};
use crate::table;
use anyhow::{anyhow, bail, Context};
use serde_json::{json, Map, Value};
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// 确认：--yes 直接过；交互终端问一句；非交互且没 --yes 就拒绝（防止脚本/AI 误触）
pub fn confirm(prompt: &str, yes: bool) -> anyhow::Result<()> {
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!("这是危险操作，需要确认：{prompt}。请先向用户确认，再加 --yes 重试");
    }
    eprint!("{prompt} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut l = String::new();
    std::io::stdin().lock().read_line(&mut l)?;
    if matches!(l.trim().to_lowercase().as_str(), "y" | "yes") { Ok(()) } else { bail!("已取消") }
}

fn follow_if(ctx: &Ctx, follow: bool, job_id: &str) -> anyhow::Result<u8> {
    if follow { cmds::follow_job(ctx, job_id) } else { Ok(0) }
}

// ── 上架 ─────────────────────────────────────────────────────────────

pub struct ProvisionOpts {
    pub file: Option<std::path::PathBuf>,
    pub kind: Option<String>,
    pub ip: Option<String>,
    pub ssh_port: Option<u16>,
    pub team: Option<String>,
    pub number: Option<i32>,
    pub label: Option<String>,
    pub relay: Option<String>,
    pub provider: Option<String>,
    pub plan: Option<String>,
    pub service_ref: Option<String>,
    pub note: Option<String>,
    pub ss_port: Option<u16>,
    pub upstream: Option<String>,
    pub ssh_password_env: Option<String>,
    pub due: Option<String>,
    pub price_cents: Option<i64>,
    pub dry_run: bool,
    pub yes: bool,
    pub follow: bool,
}

/// 解析 `socks5://user:pass@host:port` / `http://host:port`
pub fn parse_upstream(s: &str) -> anyhow::Result<Value> {
    let (proto, rest) = s.split_once("://").ok_or_else(|| anyhow!("上游格式应为 socks5://[用户:密码@]主机:端口"))?;
    if proto != "socks5" && proto != "http" {
        bail!("上游协议只支持 socks5 / http");
    }
    let (cred, hp) = match rest.rsplit_once('@') {
        Some((c, h)) => (Some(c), h),
        None => (None, rest),
    };
    let (host, port) = hp.rsplit_once(':').ok_or_else(|| anyhow!("上游缺少端口"))?;
    let port: u16 = port.parse().context("上游端口不是数字")?;
    let mut o = Map::new();
    o.insert("protocol".into(), json!(proto));
    o.insert("host".into(), json!(host));
    o.insert("port".into(), json!(port));
    if let Some(c) = cred {
        let (u, p) = c.split_once(':').unwrap_or((c, ""));
        o.insert("username".into(), json!(u));
        if !p.is_empty() {
            o.insert("password".into(), json!(p));
        }
    }
    Ok(Value::Object(o))
}

pub(crate) fn lookup(list: &Value, q: &str, keys: &[&str], what: &str) -> anyhow::Result<String> {
    let arr = list.as_array().cloned().unwrap_or_default();
    let hits: Vec<&Value> = arr.iter().filter(|x| st(x, "/id") == q || keys.iter().any(|k| st(x, &format!("/{k}")) == q)).collect();
    match hits.len() {
        1 => Ok(st(hits[0], "/id")),
        0 => bail!("没有这个{what}「{q}」"),
        _ => bail!("{what}「{q}」不唯一，请改用 id"),
    }
}

/// 把条目里的人类写法（team/relay/provider/plan 名）换成 id；已经是 *_id 的不动
fn normalize_item(ctx: &Ctx, item: &mut Value, cache: &mut Cache) -> anyhow::Result<()> {
    let Some(o) = item.as_object_mut() else { bail!("上架条目必须是对象") };
    if let Some(t) = o.remove("team").and_then(|v| v.as_str().map(String::from)) {
        o.insert("team_id".into(), json!(lookup(cache.teams(ctx)?, &t, &["name"], "团队")?));
    }
    if let Some(p) = o.remove("provider").and_then(|v| v.as_str().map(String::from)) {
        o.insert("provider_id".into(), json!(lookup(cache.providers(ctx)?, &p, &["name", "code"], "供应商")?));
    }
    if let Some(p) = o.remove("plan").and_then(|v| v.as_str().map(String::from)) {
        o.insert("plan_id".into(), json!(lookup(cache.plans(ctx)?, &p, &["name"], "套餐")?));
    }
    if let Some(r) = o.remove("relay").and_then(|v| v.as_str().map(String::from)) {
        let m = ctx.find(&r, Want::Live)?;
        if m["kind"] != "relay" {
            bail!("「{r}」不是线路机");
        }
        o.insert("relay_id".into(), m["id"].clone());
    }
    Ok(())
}

#[derive(Default)]
pub struct Cache {
    teams: Option<Value>,
    providers: Option<Value>,
    plans: Option<Value>,
}
impl Cache {
    fn teams(&mut self, ctx: &Ctx) -> anyhow::Result<&Value> {
        if self.teams.is_none() {
            self.teams = Some(ctx.c.get("/teams")?);
        }
        Ok(self.teams.as_ref().unwrap())
    }
    fn providers(&mut self, ctx: &Ctx) -> anyhow::Result<&Value> {
        if self.providers.is_none() {
            self.providers = Some(ctx.c.get("/providers")?);
        }
        Ok(self.providers.as_ref().unwrap())
    }
    fn plans(&mut self, ctx: &Ctx) -> anyhow::Result<&Value> {
        if self.plans.is_none() {
            self.plans = Some(ctx.c.get("/plans")?);
        }
        Ok(self.plans.as_ref().unwrap())
    }
}

fn items_from_file(path: &Path) -> anyhow::Result<Vec<Value>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("读取 {}", path.display()))?;
    let v: Value = serde_json::from_str(&text).with_context(|| format!("{} 不是合法 JSON", path.display()))?;
    let items = match v {
        Value::Array(a) => a,
        Value::Object(mut o) => match o.remove("items") {
            Some(Value::Array(a)) => a,
            _ => bail!("文件应是条目数组，或 {{\"items\": [...]}}"),
        },
        _ => bail!("文件应是条目数组，或 {{\"items\": [...]}}"),
    };
    if items.is_empty() {
        bail!("文件里没有条目");
    }
    Ok(items)
}

fn single_item(o: &ProvisionOpts) -> anyhow::Result<Value> {
    let kind = o.kind.clone().ok_or_else(|| anyhow!("缺少 --kind（vps / proxy / relay），或用 --file 批量"))?;
    let ip = o.ip.clone().ok_or_else(|| anyhow!("缺少 --ip"))?;
    let mut m = Map::new();
    m.insert("kind".into(), json!(kind));
    m.insert("ip".into(), json!(ip));
    let mut put = |k: &str, v: Option<Value>| {
        if let Some(v) = v {
            m.insert(k.into(), v);
        }
    };
    put("ssh_port", o.ssh_port.map(|v| json!(v)));
    put("team", o.team.as_ref().map(|v| json!(v)));
    put("number", o.number.map(|v| json!(v)));
    put("label", o.label.as_ref().map(|v| json!(v)));
    put("relay", o.relay.as_ref().map(|v| json!(v)));
    put("provider", o.provider.as_ref().map(|v| json!(v)));
    put("plan", o.plan.as_ref().map(|v| json!(v)));
    put("service_ref", o.service_ref.as_ref().map(|v| json!(v)));
    put("note", o.note.as_ref().map(|v| json!(v)));
    put("ss_port", o.ss_port.map(|v| json!(v)));
    if let Some(u) = &o.upstream {
        put("upstream", Some(parse_upstream(u)?));
    }
    if let Some(var) = &o.ssh_password_env {
        let pw = std::env::var(var).with_context(|| format!("环境变量 {var} 没设置"))?;
        put("login", Some(json!({"user": "root", "password": pw})));
    }
    if let Some(due) = &o.due {
        let mut s = json!({"due_date": due});
        if let Some(p) = o.price_cents {
            s["price_cents"] = json!(p);
        }
        put("subscription", Some(s));
    }
    Ok(Value::Object(m))
}

pub fn provision(ctx: &Ctx, o: ProvisionOpts) -> anyhow::Result<u8> {
    let mut items = match &o.file {
        Some(p) => items_from_file(p)?,
        None => vec![single_item(&o)?],
    };
    let mut cache = Cache::default();
    for it in &mut items {
        normalize_item(ctx, it, &mut cache)?;
    }
    let body = json!({"items": items});
    // 先校验（不落库）
    let chk = ctx.c.post("/provision/check", &body)?;
    let res = chk["items"].as_array().cloned().unwrap_or_default();
    let all_ok = !res.is_empty() && res.iter().all(|r| r["ok"] == true);
    if !all_ok || o.dry_run {
        ctx.emit(&chk, || {
            let rows: Vec<Vec<String>> = res
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let errs = r["errors"].as_array().map(|a| a.iter().filter_map(|e| e.as_str()).collect::<Vec<_>>().join("；")).unwrap_or_default();
                    vec![(i + 1).to_string(), st(&items[i], "/ip"), st(r, "/name"), st(r, "/number"), if r["ok"] == true { "通过".into() } else { format!("不通过：{errs}") }]
                })
                .collect();
            format!("{}\n{}", table::render(&["#", "IP", "名称", "编号", "校验"], &rows), if all_ok { "校验全部通过（--dry-run，未提交）" } else { "校验未通过，未提交" })
        });
        return Ok(if all_ok { 0 } else { 1 });
    }
    if !ctx.json {
        let names: Vec<String> = res.iter().zip(&items).map(|(r, it)| format!("{}（{}）", st(r, "/name"), st(it, "/ip"))).collect();
        outln(format!("校验通过，将上架 {} 台：{}", res.len(), names.join("、")));
    }
    confirm(&format!("确认上架这 {} 台机器？会登录机器安装软件", res.len()), o.yes)?;
    let d = ctx.c.post("/provision", &body)?;
    ctx.emit(&d, || {
        let rows: Vec<Vec<String>> = d["jobs"].as_array().cloned().unwrap_or_default().iter().map(|j| vec![st(j, "/name"), st(j, "/machine_id"), st(j, "/job_id")]).collect();
        format!("已提交上架作业：\n{}\n看进度：fleetctl job <job_id> --follow", table::render(&["名称", "machine_id", "job_id"], &rows))
    });
    let mut code = 0;
    if o.follow {
        for j in d["jobs"].as_array().cloned().unwrap_or_default() {
            code = code.max(follow_if(ctx, true, &st(&j, "/job_id"))?);
        }
    }
    Ok(code)
}

// ── 退役 / 删除 / 改名 / 编号 / 升级 ──────────────────────────────────

pub fn retire(ctx: &Ctx, machine: &str, reason: Option<&str>, no_cleanup: bool, yes: bool, follow: bool) -> anyhow::Result<u8> {
    let m = ctx.find(machine, Want::Live)?;
    let (id, name) = (st(&m, "/id"), st(&m, "/name"));
    let pre = ctx.c.get(&format!("/machines/{id}/retire"))?;
    if !ctx.json {
        outln(format!("将退役 {name}（{}）。", st(&m, "/ip")));
        if let Some(rs) = pre["renames"].as_array().filter(|r| !r.is_empty()) {
            outln("同团队编号会前移：");
            for r in rs {
                outln(format!("  {} → {}", st(r, "/old_name"), st(r, "/new_name")));
            }
        }
    }
    confirm(&format!("确认退役 {name}？会摘掉入口、停掉机器上的服务"), yes)?;
    let mut body = json!({"confirm": name, "cleanup": !no_cleanup});
    if let Some(r) = reason {
        body["reason"] = json!(r);
    }
    let d = ctx.c.post(&format!("/machines/{id}/retire"), &body)?;
    ctx.emit(&d, || format!("已提交退役作业 {}\n看进度：fleetctl job {} --follow", st(&d, "/job_id"), st(&d, "/job_id")));
    follow_if(ctx, follow, &st(&d, "/job_id"))
}

pub fn purge(ctx: &Ctx, machine: &str, yes: bool, follow: bool) -> anyhow::Result<u8> {
    let m = ctx.find(machine, Want::Retired)?;
    let (id, name) = (st(&m, "/id"), st(&m, "/name"));
    confirm(&format!("确认彻底删除已退役的 {name}？机器与遥测数据都会清掉，不可恢复"), yes)?;
    let d = ctx.c.post(&format!("/machines/{id}/purge"), &json!({"confirm": name}))?;
    ctx.emit(&d, || format!("已提交删除作业 {}", st(&d, "/job_id")));
    follow_if(ctx, follow, &st(&d, "/job_id"))
}

pub fn rename(ctx: &Ctx, machine: &str, team: Option<&str>, number: Option<i32>, label: Option<&str>, note: Option<&str>) -> anyhow::Result<u8> {
    if team.is_none() && number.is_none() && label.is_none() && note.is_none() {
        bail!("没有要改的内容：给 --team / --number / --label / --note 之一");
    }
    let (id, _) = ctx.find_id(machine, Want::Live)?;
    let cur = ctx.c.get(&format!("/machines/{id}"))?;
    let mut body = json!({"revision": cur["revision"]});
    if let Some(t) = team {
        body["team_id"] = json!(lookup(&ctx.teams()?, t, &["name"], "团队")?);
    }
    if let Some(n) = number {
        body["number"] = json!(n);
    }
    if let Some(l) = label {
        body["label"] = json!(l);
    }
    if let Some(n) = note {
        body["note"] = json!(n);
    }
    // PATCH 返回改完后的完整视图，新名字直接从里面取
    let d = ctx.c.patch(&format!("/machines/{id}"), &body)?;
    ctx.emit(&d, || format!("{} → {}", st(&cur, "/name"), st(&d, "/name")));
    Ok(0)
}

pub fn renumber(ctx: &Ctx, team: &str, yes: bool) -> anyhow::Result<u8> {
    let tid = lookup(&ctx.teams()?, team, &["name"], "团队")?;
    let pre = ctx.c.get(&format!("/teams/{tid}/renumber"))?;
    let changes = pre["changes"].as_array().cloned().unwrap_or_default();
    if changes.is_empty() {
        ctx.emit(&pre, || format!("{team} 的编号已经连续，无需调整"));
        return Ok(0);
    }
    if !yes {
        ctx.emit(&pre, || {
            let rows: Vec<Vec<String>> = changes.iter().map(|c| vec![st(c, "/old_name"), st(c, "/new_name")]).collect();
            format!("{}\n预览，未生效。加 --yes 执行", table::render(&["现名", "新名"], &rows))
        });
        return Ok(0);
    }
    let d = ctx.c.post(&format!("/teams/{tid}/renumber"), &json!({"token": pre["token"]}))?;
    ctx.emit(&d, || format!("已重新编号 {} 台", changes.len()));
    Ok(0)
}

pub fn upgrade_agent(ctx: &Ctx, machine: Option<&str>, all: bool, yes: bool, follow: bool) -> anyhow::Result<u8> {
    let d = if all {
        confirm("确认升级所有在用机器上的 agent？会逐台重启 agent", yes)?;
        ctx.c.post("/agents/upgrade", &json!({}))?
    } else {
        let q = machine.ok_or_else(|| anyhow!("给机器名，或用 --all"))?;
        let (id, _) = ctx.find_id(q, Want::Live)?;
        let j = ctx.c.post(&format!("/machines/{id}/upgrade-agent"), &json!({}))?;
        json!({"jobs": [{"machine_id": id, "job_id": j["job_id"]}]})
    };
    ctx.emit(&d, || {
        let jobs = d["jobs"].as_array().cloned().unwrap_or_default();
        let skipped = d["skipped"].as_array().map_or(0, |s| s.len());
        let skip = if skipped > 0 { format!("，跳过 {skipped} 台（有进行中的作业）") } else { "（有进行中作业的机器会被跳过）".into() };
        format!("已提交 {} 个升级作业{skip}\n{}", jobs.len(), jobs.iter().map(|j| format!("  {}", st(j, "/job_id"))).collect::<Vec<_>>().join("\n"))
    });
    let mut code = 0;
    if follow {
        for j in d["jobs"].as_array().cloned().unwrap_or_default() {
            code = code.max(cmds::follow_job(ctx, &st(&j, "/job_id"))?);
        }
    }
    Ok(code)
}

// ── 入口 ─────────────────────────────────────────────────────────────

pub fn share(ctx: &Ctx, entry: &str) -> anyhow::Result<u8> {
    let es = cmds::all_entries(ctx)?;
    let e = resolve::entry(&es, entry).map_err(|e| anyhow!(e))?;
    let d = ctx.c.get(&format!("/entries/{}/share", st(e, "/id")))?;
    ctx.emit(&d, || st(&d, "/url"));
    Ok(0)
}

pub fn entry_action(ctx: &Ctx, action: &str, entry: &str, yes: bool) -> anyhow::Result<u8> {
    if !matches!(action, "enable" | "disable" | "rotate") {
        bail!("动作只能是 enable / disable / rotate");
    }
    let es = cmds::all_entries(ctx)?;
    let e = resolve::entry(&es, entry).map_err(|e| anyhow!(e))?;
    let label = format!("{}:{}", st(e, "/relay/name"), st(e, "/port"));
    let mut body = json!({"action": action});
    if action == "rotate" {
        confirm(&format!("确认轮换 {label}（→ {}）的凭据？旧分享链接会立即失效", st(e, "/node/name")), yes)?;
        body["confirm"] = json!(label);
    } else if action == "disable" {
        confirm(&format!("确认停用入口 {label}（→ {}）？", st(e, "/node/name")), yes)?;
    }
    let d = ctx.c.post(&format!("/entries/{}/actions", st(e, "/id")), &body)?;
    let done = match action {
        "enable" => "启用",
        "disable" => "停用",
        _ => "轮换凭据",
    };
    ctx.emit(&d, || format!("已{done} {label}（期望状态代数 {}）", st(&d, "/generation")));
    Ok(0)
}

// ── 远程执行 ─────────────────────────────────────────────────────────

fn shell_quote(a: &str) -> String {
    if !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c)) {
        a.to_string()
    } else {
        format!("'{}'", a.replace('\'', "'\\''"))
    }
}

/// 一个参数：原样当脚本（可含管道、重定向）；多个参数：逐个加引号后用空格连接
pub fn join_command(args: &[String]) -> String {
    if args.len() == 1 { args[0].clone() } else { args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ") }
}

pub struct ExecOpts<'a> {
    pub machine: Option<&'a str>,
    pub team: Option<&'a str>,
    pub timeout: u64,
    pub script: Option<&'a Path>,
    pub command: &'a [String],
}

struct ExecTarget {
    id: String,
    name: String,
}

struct ExecResult {
    name: String,
    id: String,
    data: anyhow::Result<Value>,
}

fn run_one(ctx: &Ctx, t: &ExecTarget, command: &str, timeout: u64) -> ExecResult {
    let data = ctx
        .c
        .post_t(&format!("/machines/{}/exec", t.id), &json!({"command": command, "timeout_s": timeout}), Duration::from_secs(timeout + 40));
    ExecResult { name: t.name.clone(), id: t.id.clone(), data }
}

pub fn exec(ctx: &Ctx, o: ExecOpts<'_>) -> anyhow::Result<u8> {
    let command = match (o.script, o.command.is_empty()) {
        (Some(p), true) => std::fs::read_to_string(p).with_context(|| format!("读取 {}", p.display()))?,
        (None, false) => join_command(o.command),
        (Some(_), false) => bail!("--script 和 `-- 命令` 只能给一个"),
        (None, true) => bail!("没有命令：fleetctl exec <机器> -- <命令...>"),
    };
    if !(1..=600).contains(&o.timeout) {
        bail!("--timeout 要在 1 到 600 秒之间");
    }
    let all = ctx.machines()?;
    let mut targets: Vec<ExecTarget> = vec![];
    if let Some(team) = o.team {
        for m in all.iter().filter(|m| st(m, "/team/name") == team && st(m, "/lifecycle") == "active" && st(m, "/kind") != "proxy") {
            targets.push(ExecTarget { id: st(m, "/id"), name: st(m, "/name") });
        }
        if targets.is_empty() {
            bail!("团队「{team}」下没有可执行命令的在用机器");
        }
    }
    if let Some(list) = o.machine {
        for q in list.split(',').map(str::trim).filter(|q| !q.is_empty()) {
            let m = resolve::machine(&all, q, Want::Live).map_err(|e| anyhow!(e))?;
            if !targets.iter().any(|t| t.id == st(m, "/id")) {
                targets.push(ExecTarget { id: st(m, "/id"), name: st(m, "/name") });
            }
        }
    }
    if targets.is_empty() {
        bail!("没有指定机器：给 <机器>（可逗号分隔多台）或 --team");
    }

    // 单台：stdout/stderr 原样转出，退出码沿用远端的
    if targets.len() == 1 {
        let r = run_one(ctx, &targets[0], &command, o.timeout);
        // 错误原样交给 main：--json 时带上 ApiError 的 code / status
        let d = r.data?;
        if ctx.json {
            outln(serde_json::to_string_pretty(&d)?);
        } else {
            out_raw(d["stdout"].as_str().unwrap_or(""));
            let e = d["stderr"].as_str().unwrap_or("");
            if !e.is_empty() {
                eprint!("{e}");
            }
            if d["truncated"] == true {
                eprintln!("\n[输出被截断，每个流最多保留 256 KiB]");
            }
            if d["timed_out"] == true {
                eprintln!("[命令超时（{} 秒）被终止]", o.timeout);
            }
        }
        return Ok(d["exit_code"].as_i64().unwrap_or(1).clamp(0, 255) as u8);
    }

    // 多台：并发 4，按给定顺序汇总
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<ExecResult>>> = Mutex::new((0..targets.len()).map(|_| None).collect());
    std::thread::scope(|s| {
        for _ in 0..4.min(targets.len()) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= targets.len() {
                    break;
                }
                let r = run_one(ctx, &targets[i], &command, o.timeout);
                results.lock().unwrap()[i] = Some(r);
            });
        }
    });
    let results: Vec<ExecResult> = results.into_inner().unwrap().into_iter().flatten().collect();
    let code_of = |r: &ExecResult| r.data.as_ref().ok().and_then(|d| d["exit_code"].as_i64());
    let failed = results.iter().filter(|r| code_of(r) != Some(0)).count();
    if ctx.json {
        let arr: Vec<Value> = results
            .iter()
            .map(|r| match &r.data {
                Ok(d) => {
                    let mut v = d.clone();
                    v["machine"] = json!(r.name);
                    v["id"] = json!(r.id);
                    v
                }
                Err(e) => json!({"machine": r.name, "id": r.id, "exit_code": null, "error": cmds::error_json(e)}),
            })
            .collect();
        outln(serde_json::to_string_pretty(&arr)?);
    } else {
        for r in &results {
            match &r.data {
                Ok(d) => {
                    outln(format!("== {} (exit {}, {} ms) ==", r.name, st(d, "/exit_code"), st(d, "/duration_ms")));
                    out_raw(d["stdout"].as_str().unwrap_or(""));
                    if !d["stdout"].as_str().unwrap_or("").ends_with('\n') && !d["stdout"].as_str().unwrap_or("").is_empty() {
                        outln("");
                    }
                    let e = d["stderr"].as_str().unwrap_or("");
                    if !e.is_empty() {
                        outln(format!("[stderr]\n{}", e.trim_end()));
                    }
                }
                Err(e) => outln(format!("== {} 失败 ==\n{e:#}", r.name)),
            }
        }
        let rows: Vec<Vec<String>> = results
            .iter()
            .map(|r| match &r.data {
                Ok(d) => vec![r.name.clone(), st(d, "/exit_code"), format!("{} ms", st(d, "/duration_ms")), if d["timed_out"] == true { "超时".into() } else { "".into() }],
                Err(e) => vec![r.name.clone(), "-".into(), "-".into(), format!("{e:#}")],
            })
            .collect();
        outln(format!("\n汇总：{} 台，成功 {}，失败 {}\n{}", results.len(), results.len() - failed, failed, table::render(&["机器", "退出码", "耗时", "备注"], &rows)));
    }
    Ok(if failed == 0 { 0 } else { 1 })
}

// ── 密钥管理（只认会话：要 --user 登录） ───────────────────────────────

pub struct KeyAuth {
    pub user: Option<String>,
    pub password_stdin: bool,
}

/// 密钥管理接口只允许会话用户。给了 --user 就用用户名密码临时登录（密码来自 FLEET_PASSWORD 或标准输入），否则用现有令牌（会被中控拒绝）。
fn keys_client(ctx: &Ctx, a: &KeyAuth) -> anyhow::Result<Option<Client>> {
    let Some(user) = &a.user else { return Ok(None) };
    let pw = if a.password_stdin {
        let mut l = String::new();
        std::io::stdin().lock().read_line(&mut l)?;
        l.trim_end_matches(['\r', '\n']).to_string()
    } else {
        std::env::var("FLEET_PASSWORD").context("需要密码：设置环境变量 FLEET_PASSWORD，或加 --password-stdin 从标准输入读")?
    };
    let anon = Client::new(&ctx.base, Cred::None)?;
    let cookie = anon.login_session(user, &pw)?;
    Ok(Some(Client::new(&ctx.base, Cred::Cookie(cookie))?))
}

fn hint_403(e: anyhow::Error) -> anyhow::Error {
    match e.downcast_ref::<ApiError>() {
        Some(a) if a.status == 403 => anyhow!("{a}\n密钥管理只能由登录用户操作：加 --user <用户名>（密码用环境变量 FLEET_PASSWORD），或到网页「设置 → API 密钥」里操作"),
        _ => e,
    }
}

fn with_session<T>(ctx: &Ctx, a: &KeyAuth, f: impl FnOnce(&Client) -> anyhow::Result<T>) -> anyhow::Result<T> {
    let sess = keys_client(ctx, a)?;
    let c = sess.as_ref().unwrap_or(&ctx.c);
    let r = f(c).map_err(hint_403);
    if let Some(s) = &sess {
        let _ = s.post("/auth/logout", &json!({}));
    }
    r
}

pub fn keys_ls(ctx: &Ctx, a: &KeyAuth) -> anyhow::Result<u8> {
    let d = with_session(ctx, a, |c| c.get("/api-keys"))?;
    ctx.emit(&d, || {
        let rows: Vec<Vec<String>> = d
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|k| {
                let scopes = k["scopes"].as_array().map(|a| a.iter().filter_map(|s| s.as_str()).collect::<Vec<_>>().join(",")).unwrap_or_default();
                vec![
                    st(k, "/name"),
                    st(k, "/prefix"),
                    scopes,
                    table::ts(&st(k, "/created_at")),
                    if k["expires_at"].is_null() { "永不".into() } else { table::ts(&st(k, "/expires_at")) },
                    if k["last_used_at"].is_null() { "-".into() } else { table::ts(&st(k, "/last_used_at")) },
                    if k["revoked_at"].is_null() { "有效".into() } else { "已撤销".into() },
                    st(k, "/id"),
                ]
            })
            .collect();
        if rows.is_empty() { "没有 API 密钥".into() } else { table::render(&["名称", "前缀", "权限", "创建(UTC)", "过期", "最近使用", "状态", "id"], &rows) }
    });
    Ok(0)
}

pub fn keys_create(ctx: &Ctx, a: &KeyAuth, name: &str, scopes: &str, days: Option<i64>) -> anyhow::Result<u8> {
    let scopes: Vec<&str> = scopes.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    let mut body = json!({"name": name, "scopes": scopes});
    if let Some(d) = days {
        body["expires_days"] = json!(d);
    }
    let d = with_session(ctx, a, |c| c.post("/api-keys", &body))?;
    ctx.emit(&d, || format!("已创建密钥 {name}（权限 {}）\ntoken（只显示这一次，请立刻保存）：\n{}", scopes.join(","), st(&d, "/token")));
    Ok(0)
}

pub fn keys_revoke(ctx: &Ctx, a: &KeyAuth, which: &str) -> anyhow::Result<u8> {
    let d = with_session(ctx, a, |c| {
        let list = c.get("/api-keys")?;
        let hits: Vec<Value> = list.as_array().cloned().unwrap_or_default().into_iter().filter(|k| k["revoked_at"].is_null() && (st(k, "/id") == which || st(k, "/prefix") == which || st(k, "/name") == which)).collect();
        match hits.len() {
            1 => c.delete(&format!("/api-keys/{}", st(&hits[0], "/id"))),
            0 => bail!("没有名为/前缀/id 为「{which}」的有效密钥"),
            n => bail!("「{which}」匹配到 {n} 把密钥，请改用 id（fleetctl keys ls 查看）"),
        }
    })?;
    ctx.emit(&d, || format!("已撤销密钥 {which}"));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_single_arg_is_raw_script() {
        assert_eq!(join_command(&["ps aux | head -5".into()]), "ps aux | head -5");
    }

    #[test]
    fn join_many_args_quotes_unsafe() {
        let a: Vec<String> = ["journalctl", "-u", "fleet agent", "--since", "it's"].iter().map(|s| s.to_string()).collect();
        assert_eq!(join_command(&a), "journalctl -u 'fleet agent' --since 'it'\\''s'");
        assert_eq!(join_command(&["echo".into(), "".into()]), "echo ''");
        assert_eq!(join_command(&["ls".into(), "/var/log".into()]), "ls /var/log");
    }

    #[test]
    fn upstream_parsing() {
        let v = parse_upstream("socks5://bob:p%40ss@1.2.3.4:1080").unwrap();
        assert_eq!((v["protocol"].as_str(), v["host"].as_str(), v["port"].as_u64(), v["username"].as_str(), v["password"].as_str()), (Some("socks5"), Some("1.2.3.4"), Some(1080), Some("bob"), Some("p%40ss")));
        let v = parse_upstream("http://proxy.example.com:8080").unwrap();
        assert!(v.get("username").is_none() && v["host"] == "proxy.example.com");
        assert!(parse_upstream("ftp://x:1").is_err());
        assert!(parse_upstream("socks5://nohostport").is_err());
        assert!(parse_upstream("socks5://h:abc").is_err());
    }
}
