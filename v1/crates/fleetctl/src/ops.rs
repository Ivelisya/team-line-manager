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

// ── 上架 ─────────────────────────────────────────────────────────────

/// `fleetctl provision` 的参数（clap 直接解析成它，原样交给 provision）
#[derive(clap::Args)]
pub struct ProvisionOpts {
    /// 批量文件（JSON 数组或 {"items":[…]}；条目里 team/relay/provider/plan/host 可直接写名字）
    #[arg(long)]
    pub file: Option<std::path::PathBuf>,
    /// vps | proxy | relay
    #[arg(long)]
    pub kind: Option<String>,
    #[arg(long)]
    pub ip: Option<String>,
    /// 同机 IP 节点：加在这台已有的 VPS 节点（宿主机）上，用它的 SSH 与 agent；--ip 是新的出口 IP（要已配在宿主机网卡上），
    /// 不用填 --ssh-port / --ssh-password-env / --ss-port（SS 端口由中控分配），供应商取宿主机的
    #[arg(long)]
    pub host: Option<String>,
    #[arg(long)]
    pub ssh_port: Option<u16>,
    /// 团队名（节点）
    #[arg(long)]
    pub team: Option<String>,
    /// 团队内编号（不填取最小空号）
    #[arg(long)]
    pub number: Option<i32>,
    /// 线路机名（relay）
    #[arg(long)]
    pub label: Option<String>,
    /// 节点要挂的线路机
    #[arg(long)]
    pub relay: Option<String>,
    #[arg(long)]
    pub provider: Option<String>,
    #[arg(long)]
    pub plan: Option<String>,
    #[arg(long)]
    pub service_ref: Option<String>,
    /// 国家（名字旁的国旗）：ISO 3166-1 两位代码，如 US、JP；不填按 IP 自动定
    #[arg(long)]
    pub country: Option<String>,
    #[arg(long)]
    pub note: Option<String>,
    /// vps 的公网（NAT 映射）SS 端口；节点上 ssserver 固定监听 8388，非 NAT 机不要填
    #[arg(long)]
    pub ss_port: Option<u16>,
    /// 家宽代理（proxy）的上游：socks5://用户:密码@主机:端口
    #[arg(long)]
    pub upstream: Option<String>,
    /// 首次登录用的 root 密码放在哪个环境变量里（不填则用中控的密钥：校验时会试登录，登不进时报错并给出要在供应商面板录入的中控公钥）
    #[arg(long)]
    pub ssh_password_env: Option<String>,
    /// 到期日 YYYY-MM-DD（给了 = 建订阅；选了 --plan 时必填）。订阅其余各项默认跟随套餐 / 供应商，下面几项给了 = 单独设置
    #[arg(long)]
    pub due: Option<String>,
    /// 价格，如 39.46（没有套餐的机器必填）
    #[arg(long)]
    pub price: Option<String>,
    /// 币种代码，如 USD / CNY
    #[arg(long)]
    pub currency: Option<String>,
    /// 计费周期：monthly | quarterly | semiannual | yearly
    #[arg(long)]
    pub cycle: Option<String>,
    /// 账单时区（IANA 名）
    #[arg(long)]
    pub tz: Option<String>,
    /// 流量上限 GB（0 = 不限）
    #[arg(long)]
    pub traffic_gb: Option<String>,
    /// 流量每月几号重置（1–31）
    #[arg(long)]
    pub reset_day: Option<String>,
    /// 只校验，不提交
    #[arg(long)]
    pub dry_run: bool,
    /// 跳过确认
    #[arg(long)]
    pub yes: bool,
    /// 提交后跟随作业日志直到结束
    #[arg(long)]
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

/// 把条目里的人类写法（team/relay/provider/plan/host 名）换成 id；已经是 *_id 的不动
fn normalize_item(ctx: &Ctx, item: &mut Value, cache: &mut Cache) -> anyhow::Result<()> {
    let Some(o) = item.as_object_mut() else { bail!("上架条目必须是对象") };
    if let Some(t) = o.remove("team").and_then(|v| v.as_str().map(String::from)) {
        o.insert("team_id".into(), json!(ctx.team_id(&t)?));
    }
    if let Some(p) = o.remove("provider").and_then(|v| v.as_str().map(String::from)) {
        o.insert("provider_id".into(), json!(st(crate::catalog::find_provider(cache.providers(ctx)?, &p)?, "/id")));
    }
    if let Some(p) = o.remove("plan").and_then(|v| v.as_str().map(String::from)) {
        let (providers, plans) = cache.both(ctx)?;
        o.insert("plan_id".into(), json!(st(crate::catalog::find_plan(plans, providers, &p)?, "/id")));
    }
    if let Some(r) = o.remove("relay").and_then(|v| v.as_str().map(String::from)) {
        let m = ctx.find(&r, Want::Live)?;
        if m["kind"] != "relay" {
            bail!("「{r}」不是线路机");
        }
        o.insert("relay_id".into(), m["id"].clone());
    }
    if let Some(h) = o.remove("host").and_then(|v| v.as_str().map(String::from)) {
        let m = ctx.find(&h, Want::Live)?;
        if m["kind"] != "vps" || !m["host_id"].is_null() {
            bail!("「{h}」不能当宿主机：宿主机要是普通 VPS 节点（不能是线路机、家宽代理或同机 IP 节点）");
        }
        o.insert("host_id".into(), m["id"].clone());
    }
    Ok(())
}

/// 批量上架时供应商 / 套餐只取一次（团队列表走 `Ctx::teams`）
#[derive(Default)]
pub struct Cache {
    providers: Option<Vec<Value>>,
    plans: Option<Vec<Value>>,
}
impl Cache {
    fn providers(&mut self, ctx: &Ctx) -> anyhow::Result<&[Value]> {
        if self.providers.is_none() {
            self.providers = Some(ctx.list("/providers")?);
        }
        Ok(self.providers.as_deref().unwrap_or_default())
    }
    /// (供应商, 套餐)：套餐按「供应商/套餐」写法找时要用到供应商
    fn both(&mut self, ctx: &Ctx) -> anyhow::Result<(&[Value], &[Value])> {
        self.providers(ctx)?;
        if self.plans.is_none() {
            self.plans = Some(ctx.list("/plans")?);
        }
        Ok((self.providers.as_deref().unwrap_or_default(), self.plans.as_deref().unwrap_or_default()))
    }
}

pub(crate) fn items_from_file(path: &Path) -> anyhow::Result<Vec<Value>> {
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
    put("host", o.host.as_ref().map(|v| json!(v)));
    put("ssh_port", o.ssh_port.map(|v| json!(v)));
    put("team", o.team.as_ref().map(|v| json!(v)));
    put("number", o.number.map(|v| json!(v)));
    put("label", o.label.as_ref().map(|v| json!(v)));
    put("relay", o.relay.as_ref().map(|v| json!(v)));
    put("provider", o.provider.as_ref().map(|v| json!(v)));
    put("plan", o.plan.as_ref().map(|v| json!(v)));
    put("service_ref", o.service_ref.as_ref().map(|v| json!(v)));
    put("country", o.country.as_ref().map(|v| json!(v)));
    put("note", o.note.as_ref().map(|v| json!(v)));
    put("ss_port", o.ss_port.map(|v| json!(v)));
    if let Some(u) = &o.upstream {
        put("upstream", Some(parse_upstream(u)?));
    }
    if let Some(var) = &o.ssh_password_env {
        let pw = std::env::var(var).with_context(|| format!("环境变量 {var} 没设置"))?;
        put("login", Some(json!({"user": "root", "password": pw})));
    }
    // 订阅：到期日 + 给了的单独设置（同 subscription set 的写法与校验），其余由中控跟随套餐 / 供应商
    let so = crate::billing::SubSetOpts {
        price: o.price.clone(),
        currency: o.currency.clone(),
        cycle: o.cycle.clone(),
        due: o.due.clone(),
        tz: o.tz.clone(),
        traffic_gb: o.traffic_gb.clone(),
        reset_day: o.reset_day.clone(),
        ..Default::default()
    };
    let s = crate::billing::sub_patch(&so)?;
    match (o.due.is_some(), s.is_empty()) {
        (true, _) => put("subscription", Some(Value::Object(s))),
        (false, false) => bail!("--price / --currency / --cycle / --tz / --traffic-gb / --reset-day 是订阅的字段，要和 --due 一起给"),
        _ => {}
    }
    Ok(Value::Object(m))
}

/// JSON 字符串数组里的各项（不是数组为空）
fn strs(v: &Value) -> Vec<&str> { v.as_array().map(|a| a.iter().filter_map(|e| e.as_str()).collect()).unwrap_or_default() }

/// 检查结果表下面逐条列出的错误与提醒（每条一行，行首是表里的 #；末尾带换行，没有时为空）。
/// 不放进表格：「中控密钥登录不了」带着整行公钥，挤在表里没法看、没法复制
fn check_notes(res: &[Value]) -> String {
    let mut s = String::new();
    for (i, r) in res.iter().enumerate() {
        for e in strs(&r["errors"]) {
            s += &format!("#{} 不通过：{e}\n", i + 1);
        }
        for w in strs(&r["warnings"]) {
            s += &format!("#{} 提醒：{w}\n", i + 1);
        }
    }
    s
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
                    let verdict = match (r["ok"] == true, strs(&r["warnings"]).is_empty()) {
                        (false, _) => "不通过（见下）",
                        (true, false) => "通过（有提醒，见下）",
                        (true, true) => "通过",
                    };
                    vec![(i + 1).to_string(), st(&items[i], "/ip"), st(r, "/name"), st(r, "/number"), verdict.into()]
                })
                .collect();
            format!(
                "{}\n{}{}",
                table::render(&["#", "IP", "名称", "编号", "校验"], &rows),
                check_notes(&res),
                if all_ok { "校验全部通过（--dry-run，未提交）" } else { "校验未通过，未提交" }
            )
        });
        return Ok(if all_ok { 0 } else { 1 });
    }
    if !ctx.json {
        let names: Vec<String> = res.iter().zip(&items).map(|(r, it)| format!("{}（{}）", st(r, "/name"), st(it, "/ip"))).collect();
        outln(format!("{}校验通过，将上架 {} 台：{}", check_notes(&res), res.len(), names.join("、")));
    }
    confirm(&format!("确认上架这 {} 台机器？会登录机器安装软件", res.len()), o.yes)?;
    let d = ctx.c.post("/provision", &body)?;
    ctx.emit(&d, || {
        let rows: Vec<Vec<String>> = d["jobs"].as_array().cloned().unwrap_or_default().iter().map(|j| vec![st(j, "/name"), st(j, "/machine_id"), st(j, "/job_id")]).collect();
        format!("已提交上架作业：\n{}\n看进度：fleetctl job <job_id> --follow", table::render(&["名称", "machine_id", "job_id"], &rows))
    });
    if o.follow { cmds::follow_jobs(ctx, d["jobs"].as_array().into_iter().flatten()) } else { Ok(0) }
}

// ── 退役 / 改名 / 编号 / 升级 ──────────────────────────────────

/// `fleetctl retire <机器>`：默认移进备用团队（机器照常运行，订阅过期满 1 天后中控自动回收；移出备用就是撤销），
/// `--now` 立即回收（退役作业：摘入口、清理、撤令牌、移进回收站）。先打印预览：去向与自动回收的日子、一起走的同机 IP 节点、
/// 还指向它的软路由出口（不会自动改）、原团队的改名；进不了备用的说明原因（只能 --now），--now 会被拒绝的列出阻断原因。`--follow` 只对 --now 有用
pub fn retire(ctx: &Ctx, machine: &str, now: bool, reason: Option<&str>, no_cleanup: bool, yes: bool, follow: bool) -> anyhow::Result<u8> {
    let m = ctx.find(machine, Want::Live)?;
    let (id, name) = (st(&m, "/id"), st(&m, "/name"));
    let pre = ctx.c.get(&format!("/machines/{id}/retire"))?;
    let sp = &pre["spare"];
    if !now && sp["ok"] != true {
        bail!("{name} 不能移进备用：{}。要现在就清理并移进回收站，加 --now", st(sp, "/why"));
    }
    let blockers: Vec<String> = pre["blockers"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect();
    if now && !blockers.is_empty() {
        bail!("{name} 现在不能立即回收：{}", blockers.join("；"));
    }
    let names = |p: &str, to: bool| -> Vec<String> {
        pre.pointer(p)
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|x| if to { format!("{} → {}", st(x, "/name"), st(x, "/to_name")) } else { st(x, "/name") }).collect())
            .unwrap_or_default()
    };
    let ip_nodes = if now { names("/ip_nodes", false) } else { names("/spare/ip_nodes", true) };
    if !ctx.json {
        if now {
            let clean = if no_cleanup { "" } else { "停掉机器上的服务、" };
            outln(format!("将立即回收 {name}（{}）：摘掉入口、{clean}撤销令牌，然后移进回收站（之后约 35 天内自动清除数据）。", st(&m, "/ip")));
            if !ip_nodes.is_empty() {
                outln(format!("它上面的同机 IP 节点一并回收：{}", ip_nodes.join("、")));
            }
        } else {
            outln(format!(
                "将把 {name} 移进备用：→ {}，机器照常运行，{} 起自动回收（账单时区）。在那之前移出备用（fleetctl rename {} --team <团队>）就是撤销退役。",
                st(sp, "/to_name"),
                st(sp, "/recycle_on"),
                st(sp, "/to_name")
            ));
            if !ip_nodes.is_empty() {
                outln(format!("同机 IP 节点一起进备用：{}", ip_nodes.join("，")));
            }
            for n in sp["notes"].as_array().into_iter().flatten() {
                outln(format!("注意：{}", n.as_str().unwrap_or("")));
            }
        }
        let exits: Vec<String> = pre["routers"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|r| r["exits"].as_array().cloned().unwrap_or_default().into_iter().map(move |e| format!("{} · {}（{}）", st(r, "/name"), st(&e, "/exit"), st(&e, "/node"))))
            .collect();
        if !exits.is_empty() {
            outln(format!("还有软路由出口指向它（不会自动改；要换掉先 fleetctl replace）：{}", exits.join("、")));
        }
        if let Some(rs) = pre["renames"].as_array().filter(|r| !r.is_empty()) {
            outln("原团队编号会前移：");
            for r in rs {
                outln(format!("  {} → {}", st(r, "/old_name"), st(r, "/new_name")));
            }
        }
    }
    let also = if ip_nodes.is_empty() { String::new() } else { "（连同同机 IP 节点）".into() };
    let what = if now { "立即回收" } else { "退役（移进备用）" };
    confirm(&format!("确认{what} {name}{also}？"), yes)?;
    let mut body = json!({"confirm": name, "now": now, "cleanup": !no_cleanup});
    if let Some(r) = reason {
        body["reason"] = json!(r);
    }
    let d = ctx.c.post(&format!("/machines/{id}/retire"), &body)?;
    if !now {
        ctx.emit(&d, || {
            let mut s = format!("{} → {}，{} 起自动回收", st(&d, "/from"), st(&d, "/name"), st(&d, "/recycle_on"));
            for x in d["ip_nodes"].as_array().into_iter().flatten() {
                s += &format!("\n一起进备用：{} → {}", st(x, "/from"), st(x, "/name"));
            }
            for n in d["notes"].as_array().into_iter().flatten() {
                s += &format!("\n{}", n.as_str().unwrap_or(""));
            }
            s
        });
        return Ok(0);
    }
    let ip_jobs = d["ip_node_jobs"].as_array().cloned().unwrap_or_default();
    ctx.emit(&d, || {
        let mut s = format!("已提交退役作业 {}（完成后移进回收站）\n看进度：fleetctl job {} --follow", st(&d, "/job_id"), st(&d, "/job_id"));
        for j in &ip_jobs {
            s += &format!("\n一并回收 {}：作业 {}", st(j, "/name"), st(j, "/job_id"));
        }
        s
    });
    if follow { cmds::follow_jobs(ctx, std::iter::once(&d).chain(&ip_jobs)) } else { Ok(0) }
}

pub fn rename(
    ctx: &Ctx,
    machine: &str,
    team: Option<&str>,
    number: Option<i32>,
    label: Option<&str>,
    note: Option<&str>,
    country: Option<&str>,
) -> anyhow::Result<u8> {
    if team.is_none() && number.is_none() && label.is_none() && note.is_none() && country.is_none() {
        bail!("没有要改的内容：给 --team / --number / --label / --note / --country 之一");
    }
    let (id, _) = ctx.find_id(machine, Want::Live)?;
    let cur = ctx.c.get(&format!("/machines/{id}"))?;
    let mut body = json!({"revision": cur["revision"]});
    if let Some(t) = team {
        body["team_id"] = json!(ctx.team_id(t)?);
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
    // 国家：auto = 改回按 IP 自动（null）；代码由中控按 ISO 表校验
    if let Some(c) = country {
        body["country"] = if c.eq_ignore_ascii_case("auto") { Value::Null } else { json!(c) };
    }
    // PATCH 返回改完后的完整视图，新名字直接从里面取
    let d = ctx.c.patch(&format!("/machines/{id}"), &body)?;
    ctx.emit(&d, || format!("{} → {}", st(&cur, "/name"), st(&d, "/name")));
    Ok(0)
}

pub fn renumber(ctx: &Ctx, team: &str, yes: bool) -> anyhow::Result<u8> {
    let tid = ctx.team_id(team)?;
    let pre = ctx.c.get(&format!("/teams/{tid}/renumber"))?;
    let changes = pre["changes"].as_array().cloned().unwrap_or_default();
    if changes.is_empty() {
        ctx.emit(&pre, || format!("{team} 的编号已经连续，无需调整"));
        return Ok(0);
    }
    if !yes {
        ctx.emit(&pre, || {
            let rows: Vec<Vec<String>> = changes.iter().map(|c| vec![st(c, "/old_name"), st(c, "/new_name")]).collect();
            let notes: String = pre["notes"].as_array().into_iter().flatten().filter_map(Value::as_str).map(|n| format!("\n注意：{n}")).collect();
            format!("{}{notes}\n预览，未生效。加 --yes 执行", table::render(&["现名", "新名"], &rows))
        });
        return Ok(0);
    }
    let d = ctx.c.post(&format!("/teams/{tid}/renumber"), &json!({"token": pre["token"]}))?;
    ctx.emit(&d, || format!("已重新编号 {} 台", changes.len()));
    Ok(0)
}

/// `team ls`：团队列表。`--json` 是 /teams 的 data 原样（`[{id,name,sort_order,spare,node_count}]`）；备用团队只有一个，系统管理
pub fn team_ls(ctx: &Ctx) -> anyhow::Result<u8> {
    let d = ctx.teams()?;
    ctx.emit(&d, || {
        let all = d.as_array().cloned().unwrap_or_default();
        if all.is_empty() {
            return "还没有团队".into();
        }
        let rows: Vec<Vec<String>> = all
            .iter()
            .map(|t| vec![st(t, "/name"), st(t, "/sort_order"), st(t, "/node_count"), if t["spare"] == true { "是（退役的机器在这里，到期 +1 天自动回收）".into() } else { "-".into() }, st(t, "/id")])
            .collect();
        table::render(&["名称", "排序", "在用节点", "备用", "id"], &rows)
    });
    Ok(0)
}

/// `team set <团队> --name <新名>`：改团队名（备用团队也能改名；显示名跟着变，线路机备注刷新，节点不断线）。
/// `--json` 输出改完后的这个团队（同 team ls 的一项）
pub fn team_set(ctx: &Ctx, team: &str, name: &str, yes: bool) -> anyhow::Result<u8> {
    let tid = ctx.team_id(team)?;
    confirm(&format!("确认把团队「{team}」改名为「{name}」？团队里的机器显示名跟着变（{name}-01…）"), yes)?;
    ctx.c.patch(&format!("/teams/{tid}"), &json!({"name": name}))?;
    let all = ctx.c.get("/teams")?;
    let t = all.as_array().and_then(|a| a.iter().find(|t| t["id"] == tid.as_str())).cloned().unwrap_or(Value::Null);
    ctx.emit(&t, || format!("已把「{team}」改名为「{}」", st(&t, "/name")));
    Ok(0)
}

pub fn upgrade_agent(ctx: &Ctx, machine: Option<&str>, all: bool, yes: bool, follow: bool) -> anyhow::Result<u8> {
    let d = if all {
        confirm("确认升级所有在用机器上的 agent？先升一台做金丝雀，成功后线路机逐台、VPS 节点并发；新版本起不来、没上报或线路机 Xray 不正常会自动换回旧版本，出在金丝雀或线路机上时没开始的都取消", yes)?;
        ctx.c.post("/agents/upgrade", &json!({}))?
    } else {
        let q = machine.ok_or_else(|| anyhow!("给机器名，或用 --all"))?;
        let (id, _) = ctx.find_id(q, Want::Live)?;
        let j = ctx.c.post(&format!("/machines/{id}/upgrade-agent"), &json!({}))?;
        json!({"jobs": [j]})
    };
    ctx.emit(&d, || {
        let jobs = d["jobs"].as_array().cloned().unwrap_or_default();
        let skipped = d["skipped"].as_array().map_or(0, |s| s.len());
        let skip = if skipped > 0 { format!("，跳过 {skipped} 台（有进行中的作业）") } else { "（有进行中作业的机器会被跳过）".into() };
        // 全部升级：先说顺序，作业按执行顺序列出
        let order = d["order"].as_str().map(|o| format!("\n{o}")).unwrap_or_default();
        let rows: Vec<String> = jobs.iter().map(|j| format!("  {}  {}", st(j, "/job_id"), st(j, "/name"))).collect();
        format!("已提交 {} 个升级作业{skip}{order}\n{}", jobs.len(), rows.join("\n"))
    });
    if follow { cmds::follow_jobs(ctx, d["jobs"].as_array().into_iter().flatten()) } else { Ok(0) }
}

/// 一台的批量作业提交结果（`path` = 接口 /machines/{id}/ 后面的部分）：Ok(Some(作业)) 已提交；`skip_conflict` 时 409（有进行中的作业、同机 IP 节点、家宽代理…）记进 `skipped` 返回 Ok(None)
fn bulk_submit(ctx: &Ctx, path: &str, (id, name): &(String, String), body: &Value, skip_conflict: bool, skipped: &mut Vec<Value>) -> anyhow::Result<Option<Value>> {
    match ctx.c.post(&format!("/machines/{id}/{path}"), body) {
        Ok(d) => Ok(Some(json!({"machine_id": id, "name": name, "job_id": d["job_id"]}))),
        Err(e) => match e.downcast_ref::<ApiError>() {
            Some(a) if skip_conflict && a.status == 409 => {
                skipped.push(json!({"machine_id": id, "name": name, "reason": a.message}));
                Ok(None)
            }
            _ => Err(e),
        },
    }
}

/// 批量作业的目标：--team / --all 取在用的 VPS 节点（`relays` 时连线路机一起；同机 IP 节点随宿主机，家宽代理没有机器），
/// 线路机在前；否则按机器名（逗号分隔）。`what` 用在「没有可…的机器」的提示里
fn bulk_targets(ctx: &Ctx, machine: Option<&str>, team: Option<&str>, all: bool, relays: bool, what: &str) -> anyhow::Result<Vec<(String, String)>> {
    let mut targets: Vec<(String, String)> = vec![];
    if all || team.is_some() {
        let list = ctx.machines()?;
        targets = list
            .iter()
            .filter(|m| {
                let kind = st(m, "/kind");
                st(m, "/lifecycle") == "active" && (kind == "vps" || (relays && kind == "relay")) && m["host_id"].is_null() && team.is_none_or(|t| st(m, "/team/name") == t)
            })
            .map(|m| (st(m, "/id"), st(m, "/name")))
            .collect();
        if targets.is_empty() {
            bail!("没有可{what}的在用机器{}", team.map(|t| format!("（团队「{t}」）")).unwrap_or_default());
        }
    } else {
        let q = machine.ok_or_else(|| anyhow!("给机器名（可逗号分隔多台），或用 --team / --all"))?;
        for q in q.split(',').map(str::trim).filter(|q| !q.is_empty()) {
            let (id, name) = ctx.find_id(q, Want::Live)?;
            if !targets.iter().any(|t| t.0 == id) {
                targets.push((id, name));
            }
        }
    }
    Ok(targets)
}

/// 给 `targets` 逐台提交 `path` 作业（body 相同），`what` 是动作名（写进提示）。`batch`（--team / --all）时先做一台并跟到结束，
/// 它失败就停（其余不提交），成功后其余逐台提交；409 的机器（有进行中的作业）跳过并列出。返回的是退出码
fn bulk_jobs(ctx: &Ctx, path: &str, what: &str, targets: &[(String, String)], body: &Value, batch: bool, follow: bool) -> anyhow::Result<u8> {
    let (mut jobs, mut skipped): (Vec<Value>, Vec<Value>) = (vec![], vec![]);
    let many = targets.len() > 1;
    // 批量：第一台提交成功的做金丝雀，跟到结束再提交其余
    let (mut next, mut followed) = (0, 0);
    if batch && many {
        while next < targets.len() && jobs.is_empty() {
            jobs.extend(bulk_submit(ctx, path, &targets[next], body, true, &mut skipped)?);
            next += 1;
        }
        if let Some(j) = jobs.first() {
            if !ctx.json {
                outln(format!("先给「{}」{what}，等它做完再提交其余 {} 台", st(j, "/name"), targets.len() - next));
            }
            let code = cmds::follow_job(ctx, &st(j, "/job_id"))?;
            followed = 1;
            if code != 0 {
                let d = json!({"jobs": jobs, "skipped": skipped, "stopped": true});
                ctx.emit(&d, || format!("「{}」{what}没有成功，其余 {} 台没有提交", st(&jobs[0], "/name"), targets.len() - next));
                return Ok(code);
            }
        }
    }
    for t in &targets[next..] {
        jobs.extend(bulk_submit(ctx, path, t, body, many, &mut skipped)?);
    }
    let d = json!({"jobs": jobs, "skipped": skipped});
    ctx.emit(&d, || {
        let mut s = format!("已提交 {} 个{what}作业", jobs.len());
        for j in &jobs {
            s.push_str(&format!("\n  {}  {}", st(j, "/job_id"), st(j, "/name")));
        }
        if !skipped.is_empty() {
            s.push_str(&format!("\n跳过 {} 台：", skipped.len()));
            for k in &skipped {
                s.push_str(&format!("\n  {}  {}", st(k, "/name"), st(k, "/reason")));
            }
        }
        s
    });
    if follow { cmds::follow_jobs(ctx, &jobs[followed..]) } else { Ok(0) }
}

/// `fleetctl tune <机器>[,<机器>…] | --team <团队> | --all`：重做系统调优（作业 tune，不重启 ssserver / Xray）。
/// --team / --all 只取在用的线路机和 VPS 节点（同机 IP 节点随宿主机，家宽代理没有机器），线路机在前：先做一台并跟到结束，
/// 它失败就停（其余不提交），成功后其余逐台提交；409 的机器（有进行中的作业）跳过并列出。--all 要确认（--yes）。
/// `rollback`：撤销列出的机器的调优（同一个作业类型，参数 rollback），要确认（--yes）
pub fn tune(ctx: &Ctx, machine: Option<&str>, team: Option<&str>, all: bool, rollback: bool, yes: bool, follow: bool) -> anyhow::Result<u8> {
    if all {
        confirm("确认给所有在用的线路机和 VPS 节点重做系统调优？先做一台并等它做完，成功后其余逐台提交；不重启 ssserver / Xray、不断客户连接", yes)?;
    }
    let targets = bulk_targets(ctx, machine, team, all, true, "调优")?;
    if rollback {
        let names: Vec<&str> = targets.iter().map(|t| t.1.as_str()).collect();
        confirm(
            &format!("确认撤销 {} 的系统调优？按机器上第一次调优前记下的原值写回内核参数、删掉调优写的 sysctl 配置；不重启 ssserver / Xray", names.join("、")),
            yes,
        )?;
    }
    bulk_jobs(ctx, "tune", if rollback { "撤销调优" } else { "调优" }, &targets, &json!({"rollback": rollback}), all || team.is_some(), follow)
}

/// `fleetctl ss-refresh <机器>[,<机器>…] | --team <团队> | --all`：重新下发节点的 SS 配置（作业 ss_refresh）——升级中控后，把新的
/// ssserver 配置（例如出站目标黑名单）推到存量节点；配置有变化才重启 ssserver（该节点连接断开几秒），最后经入口验收出口。
/// --team / --all 只取在用的 VPS 节点（同机 IP 节点随宿主机一起刷，线路机和家宽代理不适用），做法同 tune：先做一台并跟到结束，成功后其余逐台提交。--all 要确认（--yes）
pub fn ss_refresh(ctx: &Ctx, machine: Option<&str>, team: Option<&str>, all: bool, yes: bool, follow: bool) -> anyhow::Result<u8> {
    if all {
        confirm("确认给所有在用的 VPS 节点重新下发 SS 配置？先做一台并等它做完，成功后其余逐台提交；配置有变化的节点会重启 ssserver，该节点的连接会断开几秒", yes)?;
    }
    let targets = bulk_targets(ctx, machine, team, all, false, "下发")?;
    bulk_jobs(ctx, "ss-refresh", "重新下发 SS 配置", &targets, &json!({}), all || team.is_some(), follow)
}

// ── 入口 ─────────────────────────────────────────────────────────────

pub fn share(ctx: &Ctx, entry: &str) -> anyhow::Result<u8> {
    let es = cmds::all_entries(ctx)?;
    let e = resolve::entry(&es, entry).map_err(|e| anyhow!(e))?;
    let d = ctx.c.get(&format!("/entries/{}/share", st(e, "/id")))?;
    ctx.emit(&d, || st(&d, "/url"));
    Ok(0)
}

/// 线路机直出入口：节点就是线路机自己，出口是线路机自己的 IP。走「新增入口」作业（建入口 → 等应用 → 出口验收）
pub fn add_direct(ctx: &Ctx, relay: &str, yes: bool, follow: bool) -> anyhow::Result<u8> {
    let m = ctx.find(relay, Want::Live)?;
    let (id, name) = (st(&m, "/id"), st(&m, "/name"));
    if st(&m, "/kind") != "relay" {
        bail!("「{name}」不是线路机：直出入口只能加在线路机上");
    }
    confirm(&format!("确认给 {name} 加直出入口（从 {} 出去，新分享链接名「{name}-直出」）？", st(&m, "/ip")), yes)?;
    let d = ctx.c.post(&format!("/machines/{id}/entries"), &json!({"relay_id": id}))?;
    let job = st(&d, "/job_id");
    ctx.emit(&d, || format!("已提交新增直出入口作业 {job}\n看进度：fleetctl job {job} --follow；完成后 fleetctl share {name}-直出 取分享链接"));
    if follow { cmds::follow_job(ctx, &job) } else { Ok(0) }
}

/// `fleetctl entry` 的动作
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum EntryAction {
    Enable,
    Disable,
    /// 轮换凭据（旧分享链接立即失效）
    Rotate,
    /// 给线路机加直出入口（这时 <入口> 写线路机）
    AddDirect,
}

pub fn entry_action(ctx: &Ctx, action: EntryAction, entry: &str, yes: bool, follow: bool) -> anyhow::Result<u8> {
    let (api, done) = match action {
        EntryAction::Enable => ("enable", "启用"),
        EntryAction::Disable => ("disable", "停用"),
        EntryAction::Rotate => ("rotate", "轮换凭据"),
        EntryAction::AddDirect => return add_direct(ctx, entry, yes, follow),
    };
    let es = cmds::all_entries(ctx)?;
    let e = resolve::entry(&es, entry).map_err(|e| anyhow!(e))?;
    let label = format!("{}:{}", st(e, "/relay/name"), st(e, "/port"));
    let mut body = json!({"action": api});
    // 轮换、停用之后指向这个入口的软路由出口就连不上了（不会自动改路由）：确认时列出来
    let routers = match action {
        EntryAction::Rotate | EntryAction::Disable => ctx.c.get(&format!("/entries/{}/routers", st(e, "/id")))?,
        _ => json!([]),
    };
    let note = router_note(&routers, done);
    match action {
        EntryAction::Rotate => {
            confirm(&format!("确认轮换 {label}（→ {}）的凭据？旧分享链接会立即失效{note}", st(e, "/node/name")), yes)?;
            body["confirm"] = json!(label);
        }
        EntryAction::Disable => confirm(&format!("确认停用入口 {label}（→ {}）？{note}", st(e, "/node/name")), yes)?,
        _ => {}
    }
    let d = ctx.c.post(&format!("/entries/{}/actions", st(e, "/id")), &body)?;
    ctx.emit(&d, || format!("已{done} {label}（期望状态代数 {}）{}", st(&d, "/generation"), router_note(&d["router_exits"], done)));
    Ok(0)
}

/// 指向这个入口的软路由出口（`GET /entries/{id}/routers`）：「。这些软路由出口指向它，{停用}后会连不上：路由A 的 节点01；…」，没有为空
fn router_note(rs: &Value, done: &str) -> String {
    let list: Vec<String> = rs
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| format!("{} 的 {}", st(r, "/name"), r["exits"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join("、")))
        .collect();
    if list.is_empty() {
        return String::new();
    }
    // 轮换后路由上的旧凭据与入口对不上（会开 router_exit_drift），用 router sync 一键换成新的；停用没有可换的，只能改指向（repoint）
    let fix = if done == "轮换凭据" {
        let cmds: Vec<String> = rs.as_array().into_iter().flatten().map(|r| format!("fleetctl router sync {}", st(r, "/name"))).collect();
        format!("。同步到新凭据：{}", cmds.join("；"))
    } else {
        "。要让它们继续可用，用 fleetctl repoint 改指向别的入口".to_string()
    };
    format!("。这些软路由出口指向它，{done}后会连不上（不会自动改路由）：{}{fix}", list.join("；"))
}

// ── 远程执行 ─────────────────────────────────────────────────────────

pub fn shell_quote(a: &str) -> String {
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

// `fleetctl exec` 的参数（clap 直接解析成它）
#[derive(clap::Args)]
pub struct ExecOpts {
    /// 机器，可逗号分隔多台
    pub machine: Option<String>,
    #[arg(long)]
    pub team: Option<String>,
    /// 超时秒数（1-600）
    #[arg(long, default_value_t = 60)]
    pub timeout: u64,
    /// 从文件读脚本
    #[arg(long)]
    pub script: Option<std::path::PathBuf>,
    /// `--` 之后是要执行的命令
    #[arg(last = true)]
    pub command: Vec<String>,
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

/// `--team` 的执行目标：团队里在用的线路机与 VPS 节点（家宽代理没有 SSH）。同机 IP 节点登录的是宿主机：按登录的那台机器（host_id，没有就是自己）
/// 去重，同一台物理机只执行一次——宿主机也在团队里时留宿主机，否则留第一个同机节点
fn team_targets(all: &[Value], team: &str) -> Vec<ExecTarget> {
    let members: Vec<&Value> = all.iter().filter(|m| st(m, "/team/name") == team && st(m, "/lifecycle") == "active" && st(m, "/kind") != "proxy").collect();
    let ids: Vec<String> = members.iter().map(|m| st(m, "/id")).collect();
    let mut seen = std::collections::HashSet::new();
    members
        .into_iter()
        .filter(|m| match m["host_id"].as_str() {
            Some(h) => !ids.iter().any(|i| i == h) && seen.insert(h.to_string()),
            None => true,
        })
        .map(|m| ExecTarget { id: st(m, "/id"), name: st(m, "/name") })
        .collect()
}

fn run_one(ctx: &Ctx, t: &ExecTarget, command: &str, timeout: u64) -> ExecResult {
    // 中控最坏要 timeout + 45 秒多才回：排队等执行名额最多 30 秒、SSH 本地兜底 timeout + 15 秒，再加写审计。多留 15 秒
    let data = ctx
        .c
        .post_t(&format!("/machines/{}/exec", t.id), &json!({"command": command, "timeout_s": timeout}), Duration::from_secs(timeout + 60));
    ExecResult { name: t.name.clone(), id: t.id.clone(), data }
}

/// 单台执行的退出码沿用远端的；远端不是 0..=255（被信号杀死时 ssh 报 -1，或没有退出码）一律算 1，不能当成功
pub(crate) fn exit_code_of(d: &Value) -> u8 {
    d["exit_code"].as_i64().and_then(|c| u8::try_from(c).ok()).unwrap_or(1)
}

/// exec / logs 这类接口返回 {exit_code, stdout, stderr, truncated, timed_out}：`--json` 原样输出，否则 stdout 原样、stderr 走 stderr；
/// 返回退出码。`timeout` 是 exec 给的超时秒数（超时被终止时提示）
pub(crate) fn print_result(ctx: &Ctx, d: &Value, timeout: Option<u64>) -> u8 {
    if ctx.json {
        outln(d.to_string());
    } else {
        out_raw(d["stdout"].as_str().unwrap_or(""));
        let e = d["stderr"].as_str().unwrap_or("");
        if !e.is_empty() {
            eprint!("{e}");
        }
        if d["truncated"] == true {
            eprintln!("\n[输出被截断，每个流最多保留 256 KiB]");
        }
        if let (true, Some(t)) = (d["timed_out"] == true, timeout) {
            eprintln!("[命令超时（{t} 秒）被终止]");
        }
    }
    exit_code_of(d)
}

pub fn exec(ctx: &Ctx, o: ExecOpts) -> anyhow::Result<u8> {
    let command = match (o.script.as_deref(), o.command.is_empty()) {
        (Some(p), true) => std::fs::read_to_string(p).with_context(|| format!("读取 {}", p.display()))?,
        (None, false) => join_command(&o.command),
        (Some(_), false) => bail!("--script 和 `-- 命令` 只能给一个"),
        (None, true) => bail!("没有命令：fleetctl exec <机器> -- <命令...>"),
    };
    if !(1..=600).contains(&o.timeout) {
        bail!("--timeout 要在 1 到 600 秒之间");
    }
    let all = ctx.machines()?;
    let mut targets: Vec<ExecTarget> = vec![];
    if let Some(team) = o.team.as_deref() {
        targets = team_targets(&all, team);
        if targets.is_empty() {
            bail!("团队「{team}」下没有可执行命令的在用机器");
        }
    }
    if let Some(list) = o.machine.as_deref() {
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
        return Ok(print_result(ctx, &d, Some(o.timeout)));
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
        outln(Value::Array(arr).to_string());
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
/// 返回会话与密码（新建密钥要再带一次密码）
fn keys_client(ctx: &Ctx, a: &KeyAuth) -> anyhow::Result<Option<(Client, String)>> {
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
    Ok(Some((Client::new(&ctx.base, Cred::Cookie(cookie))?, pw)))
}

fn hint_403(e: anyhow::Error) -> anyhow::Error {
    match e.downcast_ref::<ApiError>() {
        Some(a) if a.status == 403 => anyhow!("{a}\n密钥管理只能由登录用户操作：加 --user <用户名>（密码用环境变量 FLEET_PASSWORD），或到网页「设置 → API 密钥」里操作"),
        _ => e,
    }
}

/// `f` 拿到会话与登录用的密码（没给 --user 时是现有令牌、密码为空）
fn with_session<T>(ctx: &Ctx, a: &KeyAuth, f: impl FnOnce(&Client, &str) -> anyhow::Result<T>) -> anyhow::Result<T> {
    let sess = keys_client(ctx, a)?;
    let (c, pw) = sess.as_ref().map(|(c, p)| (c, p.as_str())).unwrap_or((&ctx.c, ""));
    let r = f(c, pw).map_err(hint_403);
    if let Some((s, _)) = &sess {
        let _ = s.post("/auth/logout", &json!({}));
    }
    r
}

pub fn keys_ls(ctx: &Ctx, a: &KeyAuth) -> anyhow::Result<u8> {
    let d = with_session(ctx, a, |c, _| c.get("/api-keys"))?;
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
        if rows.is_empty() { "没有 API 密钥".into() } else { table::render(&["名称", "前缀", "权限", "创建", "过期", "最近使用", "状态", "id"], &rows) }
    });
    Ok(0)
}

pub fn keys_create(ctx: &Ctx, a: &KeyAuth, name: &str, scopes: &str, days: Option<i64>) -> anyhow::Result<u8> {
    let scopes: Vec<&str> = scopes.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    let mut body = json!({"name": name, "scopes": scopes});
    if let Some(d) = days {
        body["expires_days"] = json!(d);
    }
    // 中控要求新建密钥时再带一次当前密码（开了两步验证的账户还要验证码，命令行登录不支持两步验证，请到网页上建）
    let d = with_session(ctx, a, |c, pw| {
        body["password"] = json!(pw);
        c.post("/api-keys", &body)
    })?;
    ctx.emit(&d, || format!("已创建密钥 {name}（权限 {}）\ntoken（只显示这一次，请立刻保存）：\n{}", scopes.join(","), st(&d, "/token")));
    Ok(0)
}

pub fn keys_revoke(ctx: &Ctx, a: &KeyAuth, which: &str) -> anyhow::Result<u8> {
    let d = with_session(ctx, a, |c, _| {
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
    fn entry_router_note() {
        let rs = json!([{"id": "a", "name": "路由A", "state": "online", "exits": ["节点01", "节点01备"]}, {"id": "b", "name": "路由B", "state": "offline", "exits": ["x"]}]);
        assert_eq!(router_note(&rs, "轮换凭据"), "。这些软路由出口指向它，轮换凭据后会连不上（不会自动改路由）：路由A 的 节点01、节点01备；路由B 的 x。同步到新凭据：fleetctl router sync 路由A；fleetctl router sync 路由B");
        assert_eq!(router_note(&json!([]), "停用"), "");
        assert_eq!(router_note(&Value::Null, "停用"), "", "旧中控的结果里没有 router_exits");
    }

    /// 检查结果的错误与提醒逐条一行列在表下，行首是表里的 #；都没有时为空
    #[test]
    fn provision_check_notes() {
        let res = vec![
            json!({"ok": true, "errors": [], "warnings": []}),
            json!({"ok": false, "errors": ["中控密钥登录不了 1.2.3.4:22（密钥被拒）…：ssh-ed25519 AAAA fleet"], "warnings": []}),
            json!({"ok": true, "errors": [], "warnings": ["系统是 Ubuntu 20.04，已过标准支持期"]}),
        ];
        assert_eq!(
            check_notes(&res),
            "#2 不通过：中控密钥登录不了 1.2.3.4:22（密钥被拒）…：ssh-ed25519 AAAA fleet\n#3 提醒：系统是 Ubuntu 20.04，已过标准支持期\n"
        );
        assert_eq!(check_notes(&res[..1]), "");
    }

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

    #[test]
    fn team_exec_runs_once_per_host() {
        let m = |id: &str, team: &str, kind: &str, lc: &str, host: Option<&str>| json!({"id": id, "name": id, "team": {"name": team}, "kind": kind, "lifecycle": lc, "host_id": host});
        let all = [
            m("h", "团队1", "vps", "active", None),
            m("x", "团队1", "vps", "active", Some("h")),      // 宿主机同在团队里：跳过
            m("y", "团队1", "vps", "active", Some("other")),  // 宿主机在别的团队：照常执行（登录的是宿主机）
            m("y2", "团队1", "vps", "active", Some("other")), // 和 y 同一台宿主机：只执行一次
            m("p", "团队1", "proxy", "active", None),
            m("r", "团队1", "vps", "retired", None),
            m("z", "团队2", "vps", "active", None),
        ];
        assert_eq!(team_targets(&all, "团队1").iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["h", "y"]);
    }

    /// 远端退出码 0..=255 原样返回；-1（被信号杀死）、越界、缺失都算 1
    #[test]
    fn exec_exit_code() {
        let codes: Vec<u8> = [json!(0), json!(3), json!(255), json!(-1), json!(256), json!(null)].into_iter().map(|c| exit_code_of(&json!({"exit_code": c}))).collect();
        assert_eq!(codes, [0, 3, 255, 1, 1, 1]);
    }
}
