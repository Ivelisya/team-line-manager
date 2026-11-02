//! 生命周期命令：换线路机（move）、搬空线路机（evacuate）、替换节点（replace）。
//! 都是先看预检/校验、确认后才提交作业；作业本身的进度用 `fleetctl job <id> --follow` 看。

use crate::cmds::{self, num, outln, st, Ctx};
use crate::ops::{confirm, shell_quote};
use crate::resolve::Want;
use crate::table;
use anyhow::bail;
use serde_json::{json, Value};

const SHARE_HINT: &str = "入口换到新线路机、端口也会变：用订阅链接的客户端更新订阅后自动换成新入口，用分享链接的要换成新的分享链接（作业完成后 fleetctl share <节点> 取）";

fn find_relay(ctx: &Ctx, q: &str) -> anyhow::Result<Value> {
    let m = ctx.find(q, Want::Live)?;
    if m["kind"] != "relay" {
        bail!("「{}」不是线路机", st(&m, "/name"));
    }
    Ok(m)
}

/// 线路机入口协议的中文
pub fn protocols_cn(p: &str) -> &str {
    match p {
        "reality" => "仅 Reality",
        "anytls" => "仅 AnyTLS（Reality 对公网关闭）",
        _ => "Reality + AnyTLS",
    }
}

/// `fleetctl relay-set <线路机> --protocols both|reality|anytls`：改线路机的入口协议（PATCH /relays/{id}，代数 +1，线路机热更新、不重启 Xray）
pub fn relay_set(ctx: &Ctx, relay: &str, protocols: &str) -> anyhow::Result<u8> {
    let m = find_relay(ctx, relay)?;
    let d = ctx.c.patch(&format!("/relays/{}", st(&m, "/id")), &serde_json::json!({"protocols": protocols}))?;
    ctx.emit(&d, || {
        let p = protocols_cn(&st(&d, "/protocols")).to_string();
        if d["changed"] == true { format!("已把 {} 的入口协议改成 {p}（配置版本 {}，线路机会自己应用）", st(&d, "/name"), st(&d, "/generation")) } else { format!("{} 的入口协议本来就是 {p}", st(&d, "/name")) }
    });
    Ok(0)
}

/// 预览里要改的软路由出口（替换、换线路机、搬空同一个格式）：软路由、状态、出口、现在、改成
pub(crate) fn exit_rows(routers: &[Value], node: Option<&str>) -> Vec<Vec<String>> {
    let mut t = vec![];
    for r in routers {
        let state = st(r, "/state");
        let state = crate::routers::state_cn(&state);
        for e in r["exits"].as_array().cloned().unwrap_or_default() {
            let to = e["error"].as_str().map_or_else(|| st(&e, "/to"), |x| format!("对不上：{x}"));
            // AnyTLS 出口标出来（换过去还是 AnyTLS，端口是新入口的 AnyTLS 端口）
            let exit = if e["proto"] == "anytls" { format!("{}（AnyTLS）", st(&e, "/exit")) } else { st(&e, "/exit") };
            let mut row = vec![st(r, "/name"), state.to_string(), exit, st(&e, "/from"), to];
            if let Some(n) = node {
                row.insert(0, n.to_string());
            }
            t.push(row);
        }
    }
    t
}

const EXITS_HEAD: &str = "要改的软路由出口（按最近一次采集；新入口验收后、摘旧入口前按路由当时的配置再定一次：备份后改，改完重启 OpenClash 并测延迟；任何一台失败全部换回、旧入口不摘）";

pub(crate) fn blockers(v: &Value) -> String { v["blockers"].as_array().map(|a| a.iter().filter_map(|s| s.as_str()).collect::<Vec<_>>().join("；")).unwrap_or_default() }

/// 换走的入口「线路机:端口」（预览与提交结果里的 old_entry）
fn old_addr(v: &Value) -> String { format!("{}:{}", st(v, "/old_entry/relay"), st(v, "/old_entry/port")) }

/// 作业建好的新入口「线路机:端口」（attach_new 这一步的输出）
fn new_addr(ctx: &Ctx, job: &str) -> anyhow::Result<String> {
    let j = ctx.c.get(&format!("/jobs/{job}"))?;
    let s = j["steps"].as_array().into_iter().flatten().find(|s| s["key"] == "attach_new").cloned().unwrap_or_default();
    Ok(format!("{}:{}", st(&s, "/output/relay"), st(&s, "/output/port")))
}

/// 换完之后列出受影响的入口（原地址 → 新地址）和怎么换链接
fn moved_report(ctx: &Ctx, jobs: &[Value]) -> anyhow::Result<()> {
    if ctx.json {
        return Ok(());
    }
    let mut rows = vec![];
    for j in jobs {
        rows.push(vec![st(j, "/name"), old_addr(j), new_addr(ctx, &st(j, "/job_id"))?]);
    }
    outln(format!("已完成，入口变了：\n{}\n{SHARE_HINT}", table::render(&["节点", "原入口", "新入口"], &rows)));
    Ok(())
}

/// `fleetctl move <节点>[,<节点>…] --to <线路机>`：先 GET 预览（要改的软路由出口、不能换的原因），确认后提交。
/// 逗号分隔的多个节点走批量换线路机（POST /moves）：一起改软路由，每台路由只改一次、OpenClash 只重启一次
pub fn mv(ctx: &Ctx, node: &str, to: &str, from: Option<&str>, no_routers: bool, yes: bool, follow: bool) -> anyhow::Result<u8> {
    let names: Vec<&str> = node.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if names.len() > 1 {
        return mv_many(ctx, &names, to, from, no_routers, yes, follow);
    }
    let n = ctx.find(node, Want::Live)?;
    let (id, name) = (st(&n, "/id"), st(&n, "/name"));
    let target = find_relay(ctx, to)?;
    let mut body = json!({"to_relay_id": target["id"]});
    let mut q: Vec<(&str, String)> = vec![("to_relay_id", st(&target, "/id"))];
    if let Some(f) = from {
        body["from_relay_id"] = find_relay(ctx, f)?["id"].clone();
        q.push(("from_relay_id", st(&body, "/from_relay_id")));
    }
    if no_routers {
        body["routers"] = json!(false);
        q.push(("routers", "false".into()));
    }
    let pv = ctx.c.get_q(&format!("/machines/{id}/move"), &q)?;
    if !ctx.json {
        outln(format!("将把 {name}（入口 {}）换到线路机 {}：先在新线路机建入口并验收，通过后才改软路由、摘旧入口，换线期间不断网。", old_addr(&pv), st(&target, "/name")));
        let rows = exit_rows(pv["routers"].as_array().map(Vec::as_slice).unwrap_or_default(), None);
        if no_routers {
            outln(format!("软路由：不改（--no-routers）{}", if rows.is_empty() { String::new() } else { format!("，{} 个出口指向旧入口，摘掉后就断了", rows.len()) }));
        } else if !rows.is_empty() {
            outln(format!("{EXITS_HEAD}：\n{}", table::render(&["软路由", "状态", "出口", "现在", "改成"], &rows)));
        }
        for w in pv["warnings"].as_array().cloned().unwrap_or_default() {
            outln(format!("注意：{}", w.as_str().unwrap_or("")));
        }
        outln(SHARE_HINT);
    } else if !yes {
        // --json 不加 --yes：预览打到 stdout，确认错误在 stderr
        ctx.emit(&pv, String::new);
    }
    if pv["ok"] != true {
        bail!("现在不能换线路机：{}", blockers(&pv));
    }
    confirm(&format!("确认把 {name} 换到 {}？", st(&target, "/name")), yes)?;
    let d = ctx.c.post(&format!("/machines/{id}/move"), &body)?;
    ctx.emit(&d, || {
        format!("已提交换线路机作业 {}（{} → {}）\n{}\n看进度：fleetctl job {} --follow", st(&d, "/job_id"), st(&d, "/from"), st(&d, "/to"), st(&d, "/note"), st(&d, "/job_id"))
    });
    if follow {
        let code = cmds::follow_job(ctx, &st(&d, "/job_id"))?;
        if code == 0 {
            moved_report(ctx, std::slice::from_ref(&d))?;
        }
        return Ok(code);
    }
    Ok(0)
}

/// 批量换线路机：几个节点一起换到同一台线路机（各一个作业，同批一起改软路由）；有一个不能换就都不提交
fn mv_many(ctx: &Ctx, names: &[&str], to: &str, from: Option<&str>, no_routers: bool, yes: bool, follow: bool) -> anyhow::Result<u8> {
    let mut ids = vec![];
    for n in names {
        let (id, _) = ctx.find_id(n, Want::Live)?;
        if ids.contains(&id) {
            bail!("节点「{n}」重复了");
        }
        ids.push(id);
    }
    let target = find_relay(ctx, to)?;
    let tname = st(&target, "/name");
    let mut body = json!({"machine_ids": ids, "to_relay_id": target["id"]});
    let mut q: Vec<(&str, String)> = vec![("machine_ids", ids.join(",")), ("to_relay_id", st(&target, "/id"))];
    if let Some(f) = from {
        body["from_relay_id"] = find_relay(ctx, f)?["id"].clone();
        q.push(("from_relay_id", st(&body, "/from_relay_id")));
    }
    if no_routers {
        body["routers"] = json!(false);
        q.push(("routers", "false".into()));
    }
    let pv = ctx.c.get_q("/moves", &q)?;
    let jobs = pv["jobs"].as_array().cloned().unwrap_or_default();
    if !ctx.json {
        let rows: Vec<Vec<String>> = jobs.iter().map(|j| vec![st(j, "/name"), old_addr(j)]).collect();
        outln(format!(
            "将把 {} 个节点换到线路机 {tname}：各自先在新线路机建入口并验收，都通过后一起改软路由，再摘旧入口，换线期间不断网。\n{}",
            names.len(),
            table::render(&["节点", "原入口"], &rows)
        ));
        let mut ex = vec![];
        for j in &jobs {
            ex.extend(exit_rows(j["routers"].as_array().map(Vec::as_slice).unwrap_or_default(), Some(&st(j, "/name"))));
        }
        if no_routers {
            outln(format!("软路由：不改（--no-routers）{}", if ex.is_empty() { String::new() } else { format!("，{} 个出口指向这些入口，摘掉后就断了", ex.len()) }));
        } else if !ex.is_empty() {
            outln(format!("{EXITS_HEAD}；每台软路由只改一次：\n{}", table::render(&["节点", "软路由", "状态", "出口", "现在", "改成"], &ex)));
            for line in restart_lines(&pv) {
                outln(format!("注意：{line}"));
            }
        }
        for line in offline_lines(&pv) {
            outln(format!("不能换：{line}"));
        }
        for w in pv["warnings"].as_array().cloned().unwrap_or_default() {
            outln(format!("注意：{}", w.as_str().unwrap_or("")));
        }
        outln(SHARE_HINT);
    } else if !yes {
        ctx.emit(&pv, String::new);
    }
    if pv["ok"] != true {
        // 非 json 模式离线软路由上面已逐条打印过，这里只放 blockers，不再重复；json 模式加了 --yes 时没打印预览，原因要自己带全
        bail!("现在不能换线路机（一个都没提交）：{}", moves_why(&pv, ctx.json));
    }
    confirm(&format!("确认把这 {} 个节点换到 {tname}？", names.len()), yes)?;
    let d = ctx.c.post("/moves", &body)?;
    let jobs = d["jobs"].as_array().cloned().unwrap_or_default();
    ctx.emit(&d, || {
        let rows: Vec<Vec<String>> = jobs.iter().map(|j| vec![st(j, "/name"), old_addr(j), st(j, "/job_id")]).collect();
        format!("已提交 {} 个换线路机作业（同批一起改软路由）：\n{}", jobs.len(), table::render(&["节点", "原入口", "job_id"], &rows))
    });
    if follow {
        let code = cmds::follow_jobs(ctx, &jobs)?;
        if code == 0 {
            moved_report(ctx, &jobs)?;
        }
        return Ok(code);
    }
    Ok(0)
}

/// 批量换线路机预览的 offline_routers：不在线的软路由，一台一条（它挡住的节点在 skipped 里，不在 blockers 里）
fn offline_lines(pv: &Value) -> Vec<String> {
    pv["offline_routers"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|o| {
            let why = o["error"].as_str().map(|e| format!("（{e}）")).unwrap_or_default();
            let exits: Vec<String> = o["exits"].as_array().into_iter().flatten().map(|e| format!("{}（{}）", st(e, "/exit"), st(e, "/node"))).collect();
            format!("软路由 {} {}{why}：出口 {} 要等它在线再换，或加 --no-routers 不改软路由", st(o, "/name"), if o["state"] == "offline" { "离线" } else { "还没接入" }, exits.join("、"))
        })
        .collect()
}

/// 批量换线路机现在不能提交的原因：别的原因（blockers，带节点名）。
/// 非 json 模式离线的软路由已由 offline_lines 逐条打印，不在这里重复，只有它时写「见上」；
/// json 模式预览什么都没输出，离线软路由也拼进原因里
fn moves_why(pv: &Value, json: bool) -> String {
    let mut v: Vec<String> = pv["blockers"].as_array().into_iter().flatten().filter_map(|s| s.as_str().map(String::from)).collect();
    if json {
        v.extend(offline_lines(pv));
    }
    if v.is_empty() {
        "软路由不在线，见上".into()
    } else {
        v.join("；")
    }
}

/// 搬空、批量换线路机预览的 router_restarts：同批一起改，每台路由只重启 1 次 OpenClash（断开这台路由上的全部连接）
fn restart_lines(pv: &Value) -> Vec<String> {
    pv["router_restarts"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| format!("软路由 {} 的 {} 个节点的出口一起改，OpenClash 重启 1 次（断开它上面全部连接）", st(r, "/name"), st(r, "/nodes")))
        .collect()
}

/// 线路机视图里将被搬走的入口：在用、待应用、停用的。直出入口不搬：退役线路机时随之摘除
fn moving_entries(relay: &Value) -> Vec<Value> {
    let es = relay["entries"].as_array().map(Vec::as_slice).unwrap_or_default();
    es.iter().filter(|e| matches!(e["lifecycle"].as_str(), Some("active" | "pending" | "disabled")) && e["direct"] != true).cloned().collect()
}

/// `fleetctl evacuate <线路机> --to <线路机>`
pub fn evacuate(ctx: &Ctx, relay: &str, to: &str, no_routers: bool, yes: bool, follow: bool) -> anyhow::Result<u8> {
    let src = find_relay(ctx, relay)?;
    let dst = find_relay(ctx, to)?;
    let (sid, sname, dname) = (st(&src, "/id"), st(&src, "/name"), st(&dst, "/name"));
    let mut q: Vec<(&str, String)> = vec![("to_relay_id", st(&dst, "/id"))];
    if no_routers {
        q.push(("routers", "false".into()));
    }
    // 将被搬走的节点：线路机完整视图里的入口列表（find 取的是精简列表，没有入口）
    let moving = moving_entries(&ctx.c.get(&format!("/machines/{sid}"))?);
    // 预览：--json 不加 --yes 时原样打到 stdout（确认错误在 stderr）；加了 --yes 直接提交，不多请求一次
    let pv = if moving.is_empty() || (ctx.json && yes) { Value::Null } else { ctx.c.get_q(&format!("/relays/{sid}/evacuate"), &q)? };
    if ctx.json {
        if !pv.is_null() {
            ctx.emit(&pv, String::new);
        }
    } else {
        if moving.is_empty() {
            outln(format!("{sname} 上没有在用的入口，无需搬空。"));
        } else {
            let rows: Vec<Vec<String>> = moving.iter().map(|e| vec![st(e, "/node/name"), st(e, "/port"), cmds::entry_state_cn(e).to_string()]).collect();
            outln(format!("将把 {sname} 上的 {} 个节点换到 {dname}：\n{}", moving.len(), table::render(&["节点", "端口", "入口状态"], &rows)));
            let mut ex = vec![];
            for j in pv["jobs"].as_array().cloned().unwrap_or_default() {
                ex.extend(exit_rows(j["routers"].as_array().map(Vec::as_slice).unwrap_or_default(), Some(&st(&j, "/name"))));
            }
            if no_routers {
                outln(format!("软路由：不改（--no-routers）{}", if ex.is_empty() { String::new() } else { format!("，{} 个出口指向这些入口，摘掉后就断了", ex.len()) }));
            } else if !ex.is_empty() {
                outln(format!("{EXITS_HEAD}；各节点验收完新入口后一起改，每台软路由只改一次：\n{}", table::render(&["节点", "软路由", "状态", "出口", "现在", "改成"], &ex)));
                for line in restart_lines(&pv) {
                    outln(format!("注意：{line}"));
                }
            }
            for k in pv["skipped"].as_array().cloned().unwrap_or_default() {
                outln(format!("会跳过 {}：{}", st(&k, "/name"), st(&k, "/reason")));
            }
            for w in pv["warnings"].as_array().cloned().unwrap_or_default() {
                outln(format!("注意：{}", w.as_str().unwrap_or("")));
            }
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
    let mut body = json!({"to_relay_id": dst["id"]});
    if no_routers {
        body["routers"] = json!(false);
    }
    let d = ctx.c.post(&format!("/relays/{sid}/evacuate"), &body)?;
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
    // 提交结果是 {jobs:[…]} 或单个作业
    let mut code = if follow { cmds::follow_jobs(ctx, d["jobs"].as_array().map_or(std::slice::from_ref(&d), Vec::as_slice))? } else { 0 };
    if skipped > 0 {
        code = code.max(1);
    }
    Ok(code)
}

/// 搬迁建议的入口合成一条可直接执行的批量 `fleetctl move A,B,C --to X`（每台软路由只重启一次）；没有要搬的为 None。
/// 有节点在别处还有在用入口时带 --from（换的都是这台线路机上的入口，对其余节点也成立）
fn rebalance_command(d: &Value) -> Option<String> {
    let ms = d["moves"].as_array().filter(|m| !m.is_empty())?;
    let nodes: Vec<String> = ms.iter().map(|m| shell_quote(&st(m, "/node/name"))).collect();
    let from = if ms.iter().any(|m| m["needs_from"] == true) { format!(" --from {}", shell_quote(&st(d, "/relay/name"))) } else { String::new() };
    Some(format!("fleetctl move {} --to {}{from}", nodes.join(","), shell_quote(&st(d, "/target/name"))))
}

/// `fleetctl rebalance <线路机> [--target <线路机>] [--exclude <节点>[,<节点>…]]`：这台线路机按近 7 天的速度重置前会用满的话，
/// 挑出搬走后能撑到重置日的最少入口和目标线路机，只打印建议与可直接执行的 move 命令，不执行
pub fn rebalance(ctx: &Ctx, relay: &str, target: Option<&str>, exclude: &[String]) -> anyhow::Result<u8> {
    let src = find_relay(ctx, relay)?;
    let mut q: Vec<(&str, String)> = vec![];
    if let Some(t) = target {
        q.push(("target_relay_id", st(&find_relay(ctx, t)?, "/id")));
    }
    if !exclude.is_empty() {
        let ids = exclude.iter().map(|x| ctx.find_id(x, Want::Live).map(|(id, _)| id)).collect::<anyhow::Result<Vec<_>>>()?;
        q.push(("exclude", ids.join(",")));
    }
    let mut d = ctx.c.get_q(&format!("/relays/{}/rebalance", st(&src, "/id")), &q)?;
    let cmd = rebalance_command(&d);
    d["command"] = json!(cmd);
    ctx.emit(&d, || {
        let g = |p: &str| table::gb(num_of(&d, p));
        let name = st(&d, "/relay/name");
        let pct = |p: &str| d.pointer(p).and_then(Value::as_f64).map_or(String::new(), |x| format!("{x}%"));
        let mut s = format!("{name} 本周期已用 {} / {} GB，近 7 天日均 {}", g("/used"), st(&d, "/limit_gb"), d["daily_avg"].as_f64().map_or("暂无".into(), table::gb));
        match d["forecast_full_at"].as_str() {
            Some(t) => s += &format!("，预计 {} 用满（重置 {}）", crate::billing::md_of(t), st(&d, "/period_end").chars().skip(5).collect::<String>()),
            None if !d["projected_pct"].is_null() => s += &format!("，到重置日预计用到 {}", pct("/projected_pct")),
            None => {}
        }
        s.push('\n');
        if let Some(r) = d["reason"].as_str().filter(|_| d["moves"].as_array().is_none_or(|m| m.is_empty())) {
            return s + r;
        }
        let rows: Vec<Vec<String>> = d["moves"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| vec![st(m, "/node/name"), st(m, "/port"), table::gb(num_of(m, "/period_bytes")), table::gb(num_of(m, "/daily_bytes"))])
            .collect();
        if d["over_limit"] == true {
            s += &format!("已超出额度，搬走下面 {} 个入口，日均降到 {}（原来的 20% 以下，到重置日前不再大量增长）：\n", rows.len(), table::gb(num_of(&d, "/after/daily_avg")));
        } else {
            s += &format!("搬走下面 {} 个入口，日均降到 {}，到重置日预计用到 {}（现在 {}）：\n", rows.len(), table::gb(num_of(&d, "/after/daily_avg")), pct("/after/projected_pct"), pct("/projected_pct"));
        }
        s += &table::render(&["节点", "端口", "本周期流量", "近 7 天日均"], &rows);
        // 没指定目标时按放置矩阵选的：写出被搬节点到它的往返平均
        let rtt = d.pointer("/target/avg_rtt_ms").and_then(Value::as_f64).map_or(String::new(), |v| format!("（被搬节点到它的往返平均 {}）", ms_text(v)));
        s += &format!(
            "\n目标 {}{rtt}：本周期已用 {} / {} GB，到重置日预计 {} → 接收后 {}",
            st(&d, "/target/name"),
            g("/target/used"),
            st(&d, "/target/limit_gb"),
            pct("/target/projected_pct"),
            pct("/target_after/projected_pct")
        );
        if let Some(r) = d["reason"].as_str() {
            s += &format!("\n注意：{r}");
        }
        s += "\n\n只是建议，没有执行。要搬就执行这一条（一起换，每台软路由只重启一次；先预览再确认，加 --yes 跳过确认）：\n";
        s += cmd.as_deref().unwrap_or("");
        s += &format!("\n{SHARE_HINT}");
        s
    });
    Ok(0)
}

fn num_of(v: &Value, p: &str) -> f64 { num(v, p).unwrap_or(0.0) }

/// 往返：同机房、本机不到 0.5 ms 的写「<1 ms」（同网页），不写成像没测到的「0 ms」
fn ms_text(v: f64) -> String {
    if v.round() < 1.0 { "<1 ms".into() } else { format!("{v:.0} ms") }
}

/// 放置矩阵的一格：往返中位数，成功率不到 100% 时带上；测过但全不通写「不通」，没数据「—」
fn rtt_cell(c: &Value) -> String {
    if c["samples"].as_i64().unwrap_or(0) == 0 {
        return "—".into();
    }
    let ms = c["p50_ms"].as_f64().map(ms_text);
    match (ms, c["success_pct"].as_f64()) {
        (None, _) => "不通".into(),
        (Some(m), Some(s)) if s < 100.0 => format!("{m} · {s}%"),
        (Some(m), _) => m,
    }
}

/// `fleetctl placement [--team] [--node]`：各节点到每台线路机近 24 小时的往返（中位数、成功率），`*` 节点所在的线路机（多入口的有几个）、
/// `→` 建议换到；末尾是建议换线的批量命令（每台目标线路机一条）与多入口节点带 --from 的单条命令。只读，不执行。换线前先看它，不要手写 ping 脚本
pub fn placement(ctx: &Ctx, team: Option<&str>, node: Option<&str>) -> anyhow::Result<u8> {
    let mut q: Vec<(&str, String)> = team.map(|t| ("team", t.to_string())).into_iter().collect();
    if let Some(n) = node {
        q.push(("node", ctx.find_id(n, Want::Live)?.0));
    }
    let d = ctx.c.get_q("/placement", &q)?;
    ctx.emit(&d, || {
        let relays: Vec<Value> = d["relays"].as_array().cloned().unwrap_or_default();
        let mut nodes: Vec<Value> = d["nodes"].as_array().cloned().unwrap_or_default();
        let singles: Vec<Value> = d["single_suggestions"].as_array().cloned().unwrap_or_default();
        if nodes.is_empty() {
            return "没有在用的 VPS 节点".into();
        }
        nodes.sort_by_key(|n| (n["suggest"] != true, st(n, "/name")));
        let mut head: Vec<String> = vec!["节点".into()];
        head.extend(relays.iter().map(|r| st(r, "/name")));
        head.extend(["近 7 天 TCP 不通告警".into(), "建议".into()]);
        let rows: Vec<Vec<String>> = nodes
            .iter()
            .map(|n| {
                let mut row = vec![st(n, "/name")];
                for (r, c) in relays.iter().zip(n["cells"].as_array().into_iter().flatten()) {
                    let mark = if n["entry_relay_ids"].as_array().is_some_and(|a| a.contains(&r["id"])) { "*" } else if n["suggest"] == true && n["best_relay_id"] == r["id"] { "→" } else { "" };
                    row.push(format!("{mark}{}", rtt_cell(c)));
                }
                row.push(format!("{} 次", n["tcp_fail_7d"].as_i64().unwrap_or(0)));
                let to = relays.iter().find(|r| r["id"] == n["best_relay_id"]).map(|r| st(r, "/name")).unwrap_or_default();
                row.push(match (n["suggest"] == true, n["current_relay_id"].is_null()) {
                    (true, _) => format!("{} → {to}", st(n, "/reason")),
                    (false, true) if n["entries"].as_i64().unwrap_or(0) > 1 => {
                        if singles.iter().any(|s| s["node"]["id"] == n["id"]) { "有多个入口，见下面的单条建议".into() } else { "有多个入口，不建议".into() }
                    }
                    (false, true) if n["entries"].as_i64().unwrap_or(0) == 1 => "还有待应用 / 停用的入口，不建议".into(),
                    (false, true) => "没有在用入口".into(),
                    _ => String::new(),
                });
                row
            })
            .collect();
        let heads: Vec<&str> = head.iter().map(String::as_str).collect();
        let mut s = format!(
            "近 {} 小时线路机到节点的往返（中位数 · 成功率，100% 不写；每 {} 分钟测一轮）。* 当前线路机，→ 建议换到\n{}",
            d["window_h"],
            d["interval_s"].as_i64().unwrap_or(300) / 60,
            table::render(&heads, &rows)
        );
        let mut cmds: Vec<String> = d["suggestions"].as_array().into_iter().flatten().map(|g| st(g, "/command")).collect();
        // 多入口节点的单条建议（带 --from），不并进批量命令
        cmds.extend(singles.iter().map(|g| format!("{}  # 换掉 {} 上的入口，{}", st(g, "/command"), st(g, "/from/name"), st(g, "/reason"))));
        if cmds.is_empty() {
            s += &format!("\n没有要换的节点：当前线路机比最优慢不到 {} ms、成功率不低于 {}%，或更快的那台额度放不下", num_of(&d, "/min_gain_ms"), num_of(&d, "/min_success_pct"));
        } else {
            s += "\n\n建议换线（只是建议，没有执行；每条一起换、每台软路由只重启一次，先预览再确认）：\n";
            s += &cmds.join("\n");
            s += &format!("\n{SHARE_HINT}");
        }
        s
    });
    Ok(0)
}

// `fleetctl replace` 的参数（clap 直接解析成它）
#[derive(clap::Args)]
pub struct ReplaceOpts {
    /// 旧节点（id / 显示名 / IP）
    pub old: String,
    /// 新节点
    #[arg(long = "with")]
    pub new: String,
    /// 旧节点去哪个团队（默认：同团队的备用团队）
    #[arg(long)]
    pub to: Option<String>,
    /// 旧节点在那个团队里的编号（默认第一个空号）
    #[arg(long)]
    pub number: Option<i32>,
    /// 不改软路由，只交换编号
    #[arg(long)]
    pub no_routers: bool,
    #[arg(long)]
    pub yes: bool,
    #[arg(long)]
    pub follow: bool,
}

/// `fleetctl replace <旧节点> --with <新节点>`：先 GET 预览（改哪些路由的哪些出口、编号怎么变、旧节点去哪）；
/// 不加 --yes 只打印预览，加了才提交作业
pub fn replace(ctx: &Ctx, o: ReplaceOpts) -> anyhow::Result<u8> {
    let old = ctx.find(&o.old, Want::Live)?;
    let new = ctx.find(&o.new, Want::Live)?;
    let id = st(&old, "/id");
    let mut q: Vec<(&str, String)> = vec![("new_machine_id", st(&new, "/id"))];
    let mut old_to = json!({});
    if let Some(t) = o.to.as_deref() {
        let tid = ctx.team_id(t)?;
        old_to["team_id"] = json!(tid);
        q.push(("team_id", tid));
    }
    if let Some(n) = o.number {
        old_to["number"] = json!(n);
        q.push(("number", n.to_string()));
    }
    if o.no_routers {
        q.push(("routers", "false".into()));
    }
    let pv = ctx.c.get_q(&format!("/machines/{id}/replace"), &q)?;
    if !ctx.json || !o.yes {
        let team = |p: &str| if pv.pointer(p).is_some_and(|t| t["spare"] == true) { "（备用）" } else { "" };
        let mut rows = vec![
            ("新节点", format!("{} → {}", st(&pv, "/new/name"), st(&pv, "/new_name"))),
            ("旧节点", format!("{} → {}{}", st(&pv, "/old/name"), st(&pv, "/old_to/name"), team("/old_to/team"))),
        ];
        let renames: Vec<String> = pv["renames"].as_array().cloned().unwrap_or_default().iter().map(|r| format!("{} → {}", st(r, "/old_name"), st(r, "/new_name"))).collect();
        if !renames.is_empty() {
            rows.push(("编号压缩", renames.join("，")));
        }
        if pv["routers_enabled"] != true {
            rows.push(("软路由", "不改（--no-routers）".into()));
        }
        if ctx.json {
            ctx.emit(&pv, String::new);
        } else {
            outln(table::kv(&rows));
            let t = exit_rows(pv["routers"].as_array().map(Vec::as_slice).unwrap_or_default(), None);
            if pv["routers_enabled"] == true && !t.is_empty() {
                outln(format!("要改的软路由出口（备份后改，改完重启 OpenClash 并测延迟；任何一台失败全部换回）：\n{}", table::render(&["软路由", "状态", "出口", "现在", "改成"], &t)));
            }
            for w in pv["warnings"].as_array().cloned().unwrap_or_default() {
                outln(format!("注意：{}", w.as_str().unwrap_or("")));
            }
        }
    }
    if pv["ok"] != true {
        bail!("现在不能替换：{}", blockers(&pv));
    }
    if !o.yes {
        if !ctx.json {
            outln("以上是预览，没有改动。确认无误后加 --yes 执行（--follow 跟进度）");
        }
        return Ok(0);
    }
    let mut body = json!({"new_machine_id": new["id"], "old_to": old_to});
    if o.no_routers {
        body["routers"] = json!(false);
    }
    let d = ctx.c.post(&format!("/machines/{id}/replace"), &body)?;
    ctx.emit(&d, || {
        format!("已提交替换作业 {}（{} 由 {} 接替，旧节点改为 {}）\n看进度：fleetctl job {} --follow", st(&d, "/job_id"), st(&d, "/old/name"), st(&d, "/new/name"), st(&d, "/old_to/name"), st(&d, "/job_id"))
    });
    if o.follow {
        return cmds::follow_job(ctx, &st(&d, "/job_id"));
    }
    Ok(0)
}

/// `fleetctl repoint <旧入口> --to <新入口> [--router <路由>]…`：软路由上指向旧入口的出口改成新入口，入口都不动。
/// 先 GET 预览（改哪些路由的哪些出口、不能改的原因）；不加 --yes 只打印预览，加了才提交作业
pub fn repoint(ctx: &Ctx, from: &str, to: &str, routers: &[String], yes: bool, follow: bool) -> anyhow::Result<u8> {
    let es = cmds::all_entries(ctx)?;
    let old = crate::resolve::entry(&es, from).map_err(|e| anyhow::anyhow!(e))?;
    let new = crate::resolve::entry(&es, to).map_err(|e| anyhow::anyhow!(e))?;
    let id = st(old, "/id");
    let mut rids = vec![];
    if !routers.is_empty() {
        let all = ctx.list("/routers")?;
        for q in routers {
            rids.push(st(&crate::routers::find(&all, q)?, "/id"));
        }
    }
    let mut q: Vec<(&str, String)> = vec![("to_entry_id", st(new, "/id"))];
    if !rids.is_empty() {
        q.push(("routers", rids.join(",")));
    }
    let pv = ctx.c.get_q(&format!("/entries/{id}/repoint"), &q)?;
    if !ctx.json || !yes {
        if ctx.json {
            ctx.emit(&pv, String::new);
        } else {
            outln(table::kv(&[("旧入口", st(&pv, "/old/name")), ("新入口", st(&pv, "/new/name"))]));
            let t = exit_rows(pv["routers"].as_array().map(Vec::as_slice).unwrap_or_default(), None);
            if !t.is_empty() {
                outln(format!("要改的软路由出口（备份后改，改完重启 OpenClash 并测延迟；任何一台失败全部换回）：\n{}", table::render(&["软路由", "状态", "出口", "现在", "改成"], &t)));
            }
            for w in pv["warnings"].as_array().cloned().unwrap_or_default() {
                outln(format!("注意：{}", w.as_str().unwrap_or("")));
            }
        }
    }
    if pv["ok"] != true {
        bail!("现在不能改：{}", blockers(&pv));
    }
    if !yes {
        if !ctx.json {
            outln("以上是预览，没有改动。确认无误后加 --yes 执行（--follow 跟进度）");
        }
        return Ok(0);
    }
    let d = ctx.c.post(&format!("/entries/{id}/repoint"), &json!({"to_entry_id": new["id"], "router_ids": rids}))?;
    ctx.emit(&d, || format!("已提交改出口指向作业 {}（{} → {}）\n看进度：fleetctl job {} --follow", st(&d, "/job_id"), st(&d, "/old/name"), st(&d, "/new/name"), st(&d, "/job_id")));
    if follow {
        return cmds::follow_job(ctx, &st(&d, "/job_id"));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evacuate_preview_says_router_restarts() {
        let pv = json!({"router_restarts": [{"id": "a", "name": "团队2-路由A", "times": 1, "nodes": 3}, {"id": "b", "name": "路由B", "times": 1, "nodes": 1}]});
        assert_eq!(restart_lines(&pv), ["软路由 团队2-路由A 的 3 个节点的出口一起改，OpenClash 重启 1 次（断开它上面全部连接）", "软路由 路由B 的 1 个节点的出口一起改，OpenClash 重启 1 次（断开它上面全部连接）"]);
        assert!(restart_lines(&json!({})).is_empty(), "旧中控没有这个字段");
    }

    #[test]
    fn moves_preview_merges_offline_routers() {
        let pv = json!({"ok": false, "blockers": ["团队2-04：还有进行中的作业"], "offline_routers": [
            {"id": "a", "name": "团队2-路由B", "state": "offline", "error": "经隧道连不上路由：refused", "exits": [{"node": "团队2-01", "exit": "节点01"}, {"node": "团队2-04", "exit": "节点04"}]},
            {"id": "b", "name": "路由C", "state": "pending", "error": null, "exits": [{"node": "团队2-02", "exit": "节点02"}]}]});
        assert_eq!(
            offline_lines(&pv),
            ["软路由 团队2-路由B 离线（经隧道连不上路由：refused）：出口 节点01（团队2-01）、节点04（团队2-04） 要等它在线再换，或加 --no-routers 不改软路由",
             "软路由 路由C 还没接入：出口 节点02（团队2-02） 要等它在线再换，或加 --no-routers 不改软路由"]
        );
        // 离线软路由已由 offline_lines 逐条打印，失败原因里不再重复
        assert_eq!(moves_why(&pv, false), "团队2-04：还有进行中的作业");
        assert_eq!(moves_why(&json!({"ok": false, "blockers": [], "offline_routers": [{"id": "a"}]}), false), "软路由不在线，见上");
        // json 模式什么都没打印过：离线软路由拼在 blockers 后面，只有它时也不写「见上」
        let why = moves_why(&pv, true);
        assert!(why.starts_with("团队2-04：还有进行中的作业；软路由 团队2-路由B 离线"), "{why}");
        assert!(why.contains("软路由 路由C 还没接入"), "{why}");
        let only = moves_why(&json!({"ok": false, "blockers": [], "offline_routers": [{"id": "a", "name": "路由A", "state": "offline", "exits": [{"node": "团队2-01", "exit": "节点01"}]}]}), true);
        assert!(only.starts_with("软路由 路由A 离线") && !only.contains("见上"), "{only}");
        assert!(offline_lines(&json!({"ok": true})).is_empty(), "旧中控没有这个字段");
    }

    /// 搬空的入口来自线路机的完整视图（`GET /machines/{id}`）：精简列表没有 entries，曾因此永远「无需搬空」
    #[test]
    fn evacuate_moves_live_non_direct_entries() {
        let relay = json!({"id": "r1", "kind": "relay", "entries": [
            {"port": 20000, "lifecycle": "active", "direct": false, "node": {"name": "团队1-01"}},
            {"port": 20001, "lifecycle": "pending", "direct": false},
            {"port": 20002, "lifecycle": "disabled", "direct": false},
            {"port": 20003, "lifecycle": "removing", "direct": false},
            {"port": 20004, "lifecycle": "active", "direct": true},
        ]});
        let ports: Vec<i64> = moving_entries(&relay).iter().filter_map(|e| e["port"].as_i64()).collect();
        assert_eq!(ports, [20000, 20001, 20002]);
        assert!(moving_entries(&json!({"id": "r1", "kind": "relay"})).is_empty(), "精简视图没有 entries：不会搬任何东西");
    }
}
