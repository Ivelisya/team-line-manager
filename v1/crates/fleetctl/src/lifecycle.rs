//! 生命周期命令：换线路机（move）、搬空线路机（evacuate）、替换节点（replace）。
//! 都是先看预检/校验、确认后才提交作业；作业本身的进度用 `fleetctl job <id> --follow` 看。

use crate::cmds::{self, outln, st, Ctx};
use crate::ops::confirm;
use crate::resolve::Want;
use crate::table;
use anyhow::bail;
use serde_json::{json, Value};

const SHARE_HINT: &str = "换线路机后分享链接会变：客户端需要换成新的分享链接（作业完成后到节点的入口里复制；命令行用 fleetctl share <节点> 取）";

fn find_relay(ctx: &Ctx, q: &str) -> anyhow::Result<Value> {
    let m = ctx.find(q, Want::Live)?;
    if m["kind"] != "relay" {
        bail!("「{}」不是线路机", st(&m, "/name"));
    }
    Ok(m)
}

/// 预览里要改的软路由出口（替换、换线路机、搬空同一个格式）：软路由、状态、出口、现在、改成
fn exit_rows(routers: &[Value], node: Option<&str>) -> Vec<Vec<String>> {
    let mut t = vec![];
    for r in routers {
        let state = st(r, "/state");
        let state = crate::routers::state_cn(&state);
        for e in r["exits"].as_array().cloned().unwrap_or_default() {
            let to = e["error"].as_str().map_or_else(|| st(&e, "/to"), |x| format!("对不上：{x}"));
            let mut row = vec![st(r, "/name"), state.to_string(), st(&e, "/exit"), st(&e, "/from"), to];
            if let Some(n) = node {
                row.insert(0, n.to_string());
            }
            t.push(row);
        }
    }
    t
}

const EXITS_HEAD: &str = "要改的软路由出口（按最近一次采集；新入口验收后、摘旧入口前按路由当时的配置再定一次：备份后改，改完重启 OpenClash 并测延迟；任何一台失败全部换回、旧入口不摘）";

fn blockers(v: &Value) -> String { v["blockers"].as_array().map(|a| a.iter().filter_map(|s| s.as_str()).collect::<Vec<_>>().join("；")).unwrap_or_default() }

/// `fleetctl move <节点> --to <线路机>`：先 GET 预览（要改的软路由出口、不能换的原因），确认后提交
pub fn mv(ctx: &Ctx, node: &str, to: &str, from: Option<&str>, no_routers: bool, yes: bool, follow: bool) -> anyhow::Result<u8> {
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
        outln(format!("将把 {name} 换到线路机 {}：先在新线路机建入口并验收，通过后才改软路由、摘旧入口，换线期间不断网。", st(&target, "/name")));
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
        if code == 0 && !ctx.json {
            outln(format!("已完成。{SHARE_HINT}"));
        }
        return Ok(code);
    }
    Ok(0)
}

/// 搬空预览的 router_restarts：每个节点一个作业、各自重启一次 OpenClash，每次断开这台路由上的全部连接
fn restart_lines(pv: &Value) -> Vec<String> {
    pv["router_restarts"].as_array().into_iter().flatten().map(|r| format!("软路由 {} 会重启 {} 次（每次断开它上面全部连接）", st(r, "/name"), st(r, "/times"))).collect()
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
    if !ctx.json {
        if moving.is_empty() {
            outln(format!("{sname} 上没有在用的入口，无需搬空。"));
        } else {
            let rows: Vec<Vec<String>> = moving.iter().map(|e| vec![st(e, "/node/name"), st(e, "/port"), cmds::entry_state_cn(e).to_string()]).collect();
            outln(format!("将把 {sname} 上的 {} 个节点换到 {dname}：\n{}", moving.len(), table::render(&["节点", "端口", "入口状态"], &rows)));
            let pv = ctx.c.get_q(&format!("/relays/{sid}/evacuate"), &q)?;
            let mut ex = vec![];
            for j in pv["jobs"].as_array().cloned().unwrap_or_default() {
                ex.extend(exit_rows(j["routers"].as_array().map(Vec::as_slice).unwrap_or_default(), Some(&st(&j, "/name"))));
            }
            if no_routers {
                outln(format!("软路由：不改（--no-routers）{}", if ex.is_empty() { String::new() } else { format!("，{} 个出口指向这些入口，摘掉后就断了", ex.len()) }));
            } else if !ex.is_empty() {
                outln(format!("{EXITS_HEAD}；同一台软路由一次只改一个节点，其余排队：\n{}", table::render(&["节点", "软路由", "状态", "出口", "现在", "改成"], &ex)));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evacuate_preview_says_router_restarts() {
        let pv = json!({"router_restarts": [{"id": "a", "name": "团队2-路由A", "times": 3}, {"id": "b", "name": "路由B", "times": 1}]});
        assert_eq!(restart_lines(&pv), ["软路由 团队2-路由A 会重启 3 次（每次断开它上面全部连接）", "软路由 路由B 会重启 1 次（每次断开它上面全部连接）"]);
        assert!(restart_lines(&json!({})).is_empty(), "旧中控没有这个字段");
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
