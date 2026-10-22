//! 生命周期命令：换线路机（move）、搬空线路机（evacuate）、替换节点（replace）。
//! 都是先看预检/校验、确认后才提交作业；作业本身的进度用 `fleetctl job <id> --follow` 看。

use crate::cmds::{self, outln, st, Ctx};
use crate::ops::{confirm, lookup};
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

/// 线路机视图里将被搬走的入口：在用、待应用、停用的。直出入口不搬：退役线路机时随之摘除
fn moving_entries(relay: &Value) -> Vec<Value> {
    let es = relay["entries"].as_array().map(Vec::as_slice).unwrap_or_default();
    es.iter().filter(|e| matches!(e["lifecycle"].as_str(), Some("active" | "pending" | "disabled")) && e["direct"] != true).cloned().collect()
}

/// `fleetctl evacuate <线路机> --to <线路机>`
pub fn evacuate(ctx: &Ctx, relay: &str, to: &str, yes: bool, follow: bool) -> anyhow::Result<u8> {
    let src = find_relay(ctx, relay)?;
    let dst = find_relay(ctx, to)?;
    let (sid, sname, dname) = (st(&src, "/id"), st(&src, "/name"), st(&dst, "/name"));
    // 将被搬走的节点：线路机完整视图里的入口列表（find 取的是精简列表，没有入口）
    let moving = moving_entries(&ctx.c.get(&format!("/machines/{sid}"))?);
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

pub struct ReplaceOpts<'a> {
    pub old: &'a str,
    pub new: &'a str,
    pub to: Option<&'a str>,
    pub number: Option<i32>,
    pub no_routers: bool,
    pub yes: bool,
    pub follow: bool,
}

/// `fleetctl replace <旧节点> --with <新节点>`：先 GET 预览（改哪些路由的哪些出口、编号怎么变、旧节点去哪）；
/// 不加 --yes 只打印预览，加了才提交作业
pub fn replace(ctx: &Ctx, o: ReplaceOpts) -> anyhow::Result<u8> {
    let old = ctx.find(o.old, Want::Live)?;
    let new = ctx.find(o.new, Want::Live)?;
    let id = st(&old, "/id");
    let mut q: Vec<(&str, String)> = vec![("new_machine_id", st(&new, "/id"))];
    let mut old_to = json!({});
    if let Some(t) = o.to {
        let tid = lookup(&ctx.teams()?, t, &["name"], "团队")?;
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
            let routers = pv["routers"].as_array().cloned().unwrap_or_default();
            if pv["routers_enabled"] == true && !routers.is_empty() {
                let mut t = vec![];
                for r in &routers {
                    for e in r["exits"].as_array().cloned().unwrap_or_default() {
                        let state = match r["state"].as_str() {
                            Some("online") => "在线",
                            Some("offline") => "离线",
                            _ => "未接入",
                        };
                        t.push(vec![st(r, "/name"), state.to_string(), st(&e, "/exit"), st(&e, "/from"), st(&e, "/to")]);
                    }
                }
                outln(format!("要改的软路由出口（备份后改，改完重启 OpenClash 并测延迟；任何一台失败全部换回）：\n{}", table::render(&["软路由", "状态", "出口", "现在", "改成"], &t)));
            }
            for w in pv["warnings"].as_array().cloned().unwrap_or_default() {
                outln(format!("注意：{}", w.as_str().unwrap_or("")));
            }
        }
    }
    if pv["ok"] != true {
        let why = pv["blockers"].as_array().map(|a| a.iter().filter_map(|s| s.as_str()).collect::<Vec<_>>().join("；")).unwrap_or_default();
        bail!("现在不能替换：{why}");
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
