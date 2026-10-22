//! 生命周期命令：换线路机（move）、搬空线路机（evacuate）、恢复已退役的机器（restore）。
//! 都是先看预检/校验、确认后才提交作业；作业本身的进度用 `fleetctl job <id> --follow` 看。

use crate::cmds::{self, outln, st, Ctx};
use crate::ops::{confirm, lookup, parse_upstream};
use crate::resolve::Want;
use crate::table;
use anyhow::{anyhow, bail};
use serde_json::{json, Value};

/// 恢复预检会试 SSH 登录（中控那边最多等 30 秒），默认的 30 秒请求超时会撞线
const RESTORE_PREVIEW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

const SHARE_HINT: &str = "换线路机后分享链接会变：客户端需要换成新的分享链接（作业完成后用 fleetctl share <节点> 取）";

fn find_relay(ctx: &Ctx, q: &str) -> anyhow::Result<Value> {
    let m = ctx.find(q, Want::Live)?;
    if m["kind"] != "relay" {
        bail!("「{}」不是线路机", st(&m, "/name"));
    }
    Ok(m)
}

fn follow_jobs(ctx: &Ctx, d: &Value) -> anyhow::Result<u8> {
    let mut code = 0;
    let jobs: Vec<Value> = match d["jobs"].as_array() {
        Some(a) => a.clone(),
        None => vec![d.clone()],
    };
    for j in jobs {
        code = code.max(cmds::follow_job(ctx, &st(&j, "/job_id"))?);
    }
    Ok(code)
}

/// `fleetctl move <节点> --to <线路机>`
pub fn mv(ctx: &Ctx, node: &str, to: &str, from: Option<&str>, yes: bool, follow: bool) -> anyhow::Result<u8> {
    let n = ctx.find(node, Want::Live)?;
    let (id, name) = (st(&n, "/id"), st(&n, "/name"));
    let target = find_relay(ctx, to)?;
    let mut body = json!({"to_relay_id": target["id"]});
    if let Some(f) = from {
        body["from_relay_id"] = find_relay(ctx, f)?["id"].clone();
    }
    if !ctx.json {
        outln(format!("将把 {name} 换到线路机 {}：先在新线路机建入口并验收，通过后才摘旧入口，换线期间不断网。", st(&target, "/name")));
        outln(SHARE_HINT);
    }
    confirm(&format!("确认把 {name} 换到 {}？", st(&target, "/name")), yes)?;
    let d = ctx.c.post(&format!("/machines/{id}/move"), &body)?;
    ctx.emit(&d, || {
        format!("已提交换线路机作业 {}（{} → {}）\n{}\n看进度：fleetctl job {} --follow", st(&d, "/job_id"), st(&d, "/from"), st(&d, "/to"), st(&d, "/note"), st(&d, "/job_id"))
    });
    if follow {
        let code = cmds::follow_job(ctx, &st(&d, "/job_id"))?;
        if code == 0 && !ctx.json {
            outln(format!("已完成。{SHARE_HINT}"));
        }
        return Ok(code);
    }
    Ok(0)
}

/// `fleetctl evacuate <线路机> --to <线路机>`
pub fn evacuate(ctx: &Ctx, relay: &str, to: &str, yes: bool, follow: bool) -> anyhow::Result<u8> {
    let src = find_relay(ctx, relay)?;
    let dst = find_relay(ctx, to)?;
    let (sid, sname, dname) = (st(&src, "/id"), st(&src, "/name"), st(&dst, "/name"));
    // 将被搬走的节点（线路机视图里自带入口列表）。直出入口不搬：退役线路机时随之摘除
    let moving: Vec<Value> = src["entries"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|e| matches!(e["lifecycle"].as_str(), Some("active" | "pending" | "disabled")) && e["direct"] != true)
        .collect();
    if !ctx.json {
        if moving.is_empty() {
            outln(format!("{sname} 上没有在用的入口，无需搬空。"));
        } else {
            let rows: Vec<Vec<String>> = moving.iter().map(|e| vec![st(e, "/node/name"), st(e, "/port"), cmds::entry_state_cn(e).to_string()]).collect();
            outln(format!("将把 {sname} 上的 {} 个节点换到 {dname}：\n{}", moving.len(), table::render(&["节点", "端口", "入口状态"], &rows)));
            outln(SHARE_HINT);
        }
    }
    if moving.is_empty() {
        let d = json!({"jobs": [], "skipped": [], "from": sname, "to": dname});
        if ctx.json {
            ctx.emit(&d, String::new);
        }
        return Ok(0);
    }
    confirm(&format!("确认把 {sname} 上的 {} 个节点搬到 {dname}？", moving.len()), yes)?;
    let d = ctx.c.post(&format!("/relays/{sid}/evacuate"), &json!({"to_relay_id": dst["id"]}))?;
    ctx.emit(&d, || {
        let jobs = d["jobs"].as_array().cloned().unwrap_or_default();
        let mut s = format!("已提交 {} 个换线路机作业：\n", jobs.len());
        let rows: Vec<Vec<String>> = jobs.iter().map(|j| vec![st(j, "/name"), st(j, "/job_id")]).collect();
        s += &table::render(&["节点", "job_id"], &rows);
        if let Some(sk) = d["skipped"].as_array().filter(|a| !a.is_empty()) {
            s += "\n跳过（需要先处理）：";
            for k in sk {
                s += &format!("\n  {}：{}", st(k, "/name"), st(k, "/reason"));
            }
        }
        s
    });
    let skipped = d["skipped"].as_array().map(|a| a.len()).unwrap_or(0);
    let mut code = if follow { follow_jobs(ctx, &d)? } else { 0 };
    if skipped > 0 {
        code = code.max(1);
    }
    Ok(code)
}

pub struct RestoreOpts<'a> {
    pub machine: &'a str,
    pub team: Option<&'a str>,
    pub number: Option<i32>,
    pub relay: Option<&'a str>,
    pub label: Option<&'a str>,
    pub upstream: Option<&'a str>,
    pub ss_port: Option<u16>,
    pub password_env: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub reset_host_key: bool,
    pub no_probe: bool,
    pub yes: bool,
    pub follow: bool,
}

/// `fleetctl restore <已退役机器>`：先 GET 预检，再提交
pub fn restore(ctx: &Ctx, o: RestoreOpts) -> anyhow::Result<u8> {
    let m = ctx.find(o.machine, Want::Retired)?;
    let id = st(&m, "/id");
    let mut q: Vec<(&str, String)> = vec![];
    if let Some(t) = o.team {
        q.push(("team_id", lookup(&ctx.teams()?, t, &["name"], "团队")?));
    }
    if let Some(n) = o.number {
        q.push(("number", n.to_string()));
    }
    if let Some(l) = o.label {
        q.push(("label", l.to_string()));
    }
    if o.no_probe {
        q.push(("probe", "false".into()));
    }
    // 主机指纹与退役时不同：带了 --reset-host-key 只提醒，不阻断
    if o.reset_host_key {
        q.push(("reset_host_key", "true".into()));
    }
    let pv = ctx.c.call(reqwest::Method::GET, &format!("/machines/{id}/restore"), &q, None, RESTORE_PREVIEW_TIMEOUT)?;
    if !ctx.json {
        let mut rows = vec![
            ("机器", format!("{}（{}，{}）", st(&pv, "/old_name"), cmds::kind_cn(&st(&pv, "/kind")), st(&pv, "/ip"))),
            ("恢复后名字", format!("{} → {}", st(&pv, "/old_name"), st(&pv, "/name"))),
            ("退役原因", st(&pv, "/retired_reason")),
        ];
        if pv["ssh"].is_object() {
            let login = match pv["ssh"]["key_login"].as_str().unwrap_or("") {
                "ok" => "能登录".to_string(),
                "denied" => "被拒绝（需要 root 密码）".to_string(),
                "unreachable" => "连不上".to_string(),
                _ => format!("出错：{}", st(&pv, "/ssh/detail")),
            };
            rows.push(("中控密钥登录", login));
            if !pv["ssh"]["host_key"].is_null() {
                rows.push(("主机指纹", st(&pv, "/ssh/host_key")));
            }
        }
        // 退役时留下的出站参数（不含秘密），恢复时默认沿用
        if pv["outbound"].is_object() {
            let o = &pv["outbound"];
            let v = if o["protocol"] == "shadowsocks" {
                format!("SS 端口 {}", st(o, "/port"))
            } else {
                let user = o["username"].as_str().map(|u| format!("{u}:***@")).unwrap_or_default();
                format!("{}://{user}{}:{}", st(o, "/protocol"), st(o, "/host"), st(o, "/port"))
            };
            rows.push(("出站（沿用）", v));
        }
        outln(table::kv(&rows));
        for w in pv["warnings"].as_array().cloned().unwrap_or_default() {
            outln(format!("注意：{}", w.as_str().unwrap_or("")));
        }
    }
    if pv["restorable"] != true {
        let why = pv["blockers"].as_array().map(|a| a.iter().filter_map(|s| s.as_str()).collect::<Vec<_>>().join("；")).unwrap_or_default();
        bail!("现在不能恢复：{why}");
    }

    let mut body = json!({});
    if let Some(t) = o.team {
        body["team_id"] = json!(lookup(&ctx.teams()?, t, &["name"], "团队")?);
    }
    if let Some(n) = o.number {
        body["number"] = json!(n);
    }
    if let Some(l) = o.label {
        body["label"] = json!(l);
    }
    if let Some(r) = o.reason {
        body["reason"] = json!(r);
    }
    if let Some(p) = o.ss_port {
        body["ss_port"] = json!(p);
    }
    if o.reset_host_key {
        body["reset_host_key"] = json!(true);
    }
    if pv["needs"]["relay"] == true {
        let Some(r) = o.relay else {
            let hint = if pv["last_relay"].is_object() { format!("（上次接的是 {}）", st(&pv, "/last_relay/name")) } else { String::new() };
            bail!("节点要接入一台线路机：加 --relay <线路机名>{hint}");
        };
        body["relay_id"] = find_relay(ctx, r)?["id"].clone();
    }
    if pv["needs"]["upstream"] == true && o.upstream.is_none() {
        bail!("这台家宽代理退役时没留下上游参数：加 --upstream socks5://用户:密码@主机:端口");
    }
    if pv["needs"]["upstream_password"] == true && o.upstream.is_none() {
        bail!("上游有用户名，密码退役时已清掉：加 --upstream {}://用户:密码@{}:{}", st(&pv, "/outbound/protocol"), st(&pv, "/outbound/host"), st(&pv, "/outbound/port"));
    }
    if let Some(u) = o.upstream {
        body["upstream"] = parse_upstream(u)?;
    }
    let need_pw = pv["needs"]["password"] == "required";
    if o.password_env.is_some() || need_pw {
        let var = o.password_env.ok_or_else(|| anyhow!("中控密钥登录不了这台机器，需要 root 密码：把它放进环境变量，加 --password-env <变量名>"))?;
        let pw = std::env::var(var).map_err(|_| anyhow!("环境变量 {var} 没有设置"))?;
        if pw.is_empty() {
            bail!("环境变量 {var} 是空的");
        }
        body["login"] = json!({"password": pw});
    }
    confirm(&format!("确认恢复 {} 为 {}？机器会重新走一遍上架收敛，编号接在团队末尾", st(&pv, "/old_name"), st(&pv, "/name")), o.yes)?;
    let d = ctx.c.post(&format!("/machines/{id}/restore"), &body)?;
    ctx.emit(&d, || {
        format!("已提交恢复作业 {}（{} → {}）\n看进度：fleetctl job {} --follow", st(&d, "/job_id"), st(&d, "/old_name"), st(&d, "/name"), st(&d, "/job_id"))
    });
    if o.follow {
        return cmds::follow_job(ctx, &st(&d, "/job_id"));
    }
    Ok(0)
}
