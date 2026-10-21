//! 访问策略：团队/节点的域名黑白名单。`set` 只改草稿；`publish` 才会生效并应用到线路机。

use crate::cmds::{st, Ctx};
use crate::ops::{confirm, lookup};
use crate::resolve::Want;
use crate::table;
use anyhow::{anyhow, bail};
use serde_json::{json, Value};

pub const MODES: [&str; 5] = ["observe", "allowlist", "denylist", "blocked", "inherit"];

fn mode_cn(m: &str) -> &'static str {
    match m {
        "observe" => "仅观察",
        "allowlist" => "白名单",
        "denylist" => "黑名单",
        "blocked" => "暂停新连接",
        "inherit" => "跟随团队",
        _ => "?",
    }
}

fn status_cn(s: &str) -> &'static str {
    match s {
        "applied" => "已应用",
        "applying" => "进行中",
        "failed" => "失败",
        "gone" => "已退役",
        _ => "?",
    }
}

fn n(v: &Value, ptr: &str) -> i64 { v.pointer(ptr).and_then(Value::as_i64).unwrap_or(0) }

fn domains_of(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// 一个范围的描述：`黑名单 · 3 个域名`
fn brief(mode: &str, domains: usize) -> String {
    if matches!(mode, "allowlist" | "denylist") { format!("{} · {domains} 个域名", mode_cn(mode)) } else { mode_cn(mode).to_string() }
}

/// 找到的范围：取出当前值 / 草稿 / revision 所需的信息
#[derive(Debug, Clone)]
pub struct Target {
    pub scope: &'static str,
    pub id: String,
    pub name: String,
    /// `policy` 对象（mode / domains / note / draft / revision）
    pub policy: Value,
    /// 仅节点：生效值
    pub effective: Option<Value>,
}

/// 在 `GET /policies` 的结果里按名字或 id 找团队或节点（团队名优先；节点名可用显示名、IP 等机器引用）
pub fn resolve_target(ctx: &Ctx, list: &Value, q: &str) -> anyhow::Result<Target> {
    let teams = list["teams"].clone();
    if let Ok(id) = lookup(&teams, q, &["name"], "团队") {
        let t = teams.as_array().and_then(|a| a.iter().find(|t| st(t, "/id") == id)).cloned().unwrap_or(Value::Null);
        return Ok(Target { scope: "team", id, name: st(&t, "/name"), policy: t["policy"].clone(), effective: None });
    }
    let m = ctx.find(q, Want::Live).map_err(|e| anyhow!("{q} 既不是团队也不是节点：{e}"))?;
    if st(&m, "/kind") == "relay" {
        bail!("「{q}」是线路机，访问策略设在团队或节点上");
    }
    let id = st(&m, "/id");
    for t in teams.as_array().into_iter().flatten() {
        if let Some(node) = t["nodes"].as_array().and_then(|a| a.iter().find(|x| st(x, "/id") == id)) {
            return Ok(Target { scope: "node", id, name: st(node, "/name"), policy: node["policy"].clone(), effective: Some(node["effective"].clone()) });
        }
    }
    bail!("「{q}」不在任何团队里，没有访问策略")
}

pub fn ls(ctx: &Ctx) -> anyhow::Result<u8> {
    let d = ctx.c.get("/policies")?;
    ctx.emit(&d, || {
        let mut rows: Vec<Vec<String>> = vec![];
        for t in d["teams"].as_array().into_iter().flatten() {
            let p = &t["policy"];
            rows.push(vec!["团队".into(), st(t, "/name"), brief(&st(p, "/mode"), domains_of(&p["domains"]).len()), draft_brief(p), st(p, "/revision")]);
            for node in t["nodes"].as_array().into_iter().flatten() {
                let (p, e) = (&node["policy"], &node["effective"]);
                let src = match st(e, "/source").as_str() {
                    "team" => "（跟随团队）",
                    "default" => "（默认）",
                    _ => "",
                };
                rows.push(vec![
                    "节点".into(),
                    format!("  {}", st(node, "/name")),
                    format!("{}{src}", brief(&st(e, "/mode"), domains_of(&e["domains"]).len())),
                    draft_brief(p),
                    st(p, "/revision"),
                ]);
            }
        }
        let hint = match d["draft_count"].as_i64() {
            Some(c) if c > 0 => format!("\n有 {c} 份未发布的草稿：fleetctl policy preview 查看影响，policy publish --yes 发布"),
            _ => String::new(),
        };
        format!("{}{hint}", table::render(&["范围", "名称", "生效", "草稿", "版本号"], &rows))
    });
    Ok(0)
}

fn draft_brief(p: &Value) -> String {
    let d = &p["draft"];
    if d.is_null() { "-".into() } else { brief(&st(d, "/mode"), domains_of(&d["domains"]).len()) }
}

pub fn show(ctx: &Ctx, who: &str) -> anyhow::Result<u8> {
    let list = ctx.c.get("/policies")?;
    let t = resolve_target(ctx, &list, who)?;
    let out = json!({"scope": t.scope, "id": t.id, "name": t.name, "policy": t.policy, "effective": t.effective});
    ctx.emit(&out, || {
        let p = &t.policy;
        let mut kv: Vec<(&str, String)> = vec![("范围", if t.scope == "team" { "团队".into() } else { "节点".into() }), ("名称", t.name.clone())];
        if let Some(e) = &t.effective {
            let src = match st(e, "/source").as_str() {
                "node" => "节点自己的策略",
                "team" => "跟随团队",
                _ => "默认",
            };
            kv.push(("生效", format!("{}（{src}）", brief(&st(e, "/mode"), domains_of(&e["domains"]).len()))));
        }
        kv.push(("模式", brief(&st(p, "/mode"), domains_of(&p["domains"]).len())));
        kv.push(("备注", st(p, "/note")));
        kv.push(("版本号", st(p, "/revision")));
        let mut s = table::kv(&kv);
        let doms = domains_of(&p["domains"]);
        if !doms.is_empty() {
            s.push_str(&format!("\n域名（{}）：\n  {}", doms.len(), doms.join("\n  ")));
        }
        let d = &p["draft"];
        if !d.is_null() {
            let dd = domains_of(&d["domains"]);
            s.push_str(&format!("\n\n草稿：{}\n  备注：{}", brief(&st(d, "/mode"), dd.len()), st(d, "/note")));
            if !dd.is_empty() {
                s.push_str(&format!("\n  域名：\n    {}", dd.join("\n    ")));
            }
            s.push_str("\n（草稿尚未发布，fleetctl policy preview 看影响）");
        }
        s
    });
    Ok(0)
}

pub struct SetOpts<'a> {
    pub who: &'a str,
    pub mode: &'a str,
    pub domains: Option<&'a str>,
    pub note: Option<&'a str>,
}

/// 草稿优先，其次生效值
fn base_of(p: &Value) -> &Value { if p["draft"].is_null() { p } else { &p["draft"] } }

pub fn set(ctx: &Ctx, o: SetOpts) -> anyhow::Result<u8> {
    if !MODES.contains(&o.mode) {
        bail!("模式只能是 {}（得到「{}」）", MODES.join("、"), o.mode);
    }
    let list = ctx.c.get("/policies")?;
    let t = resolve_target(ctx, &list, o.who)?;
    if o.mode == "inherit" && t.scope == "team" {
        bail!("团队没有可继承的上一级，模式不能是 inherit");
    }
    let base = base_of(&t.policy);
    // 没写 --domains / --note 就沿用草稿（没有草稿则生效值）里的
    let domains: Value = match o.domains {
        Some(s) => json!(s),
        None => base["domains"].clone(),
    };
    let note = match o.note {
        Some(n) => n.to_string(),
        None => base["note"].as_str().unwrap_or("").to_string(),
    };
    let body = json!({"mode": o.mode, "domains": domains, "note": note, "expected_revision": t.policy["revision"]});
    let r = ctx.c.call(reqwest::Method::PUT, &format!("/policies/{}/{}", t.scope, t.id), &[], Some(&body), std::time::Duration::from_secs(30))?;
    ctx.emit(&r, || {
        let d = &r["policy"]["draft"];
        let tail = if d.is_null() { "草稿与生效值相同，没有未发布的改动".to_string() } else { format!("已保存为草稿：{}", brief(&st(d, "/mode"), domains_of(&d["domains"]).len())) };
        format!("{} {}：{tail}\n草稿还没有生效。fleetctl policy preview 看影响，policy publish --yes 发布", if t.scope == "team" { "团队" } else { "节点" }, t.name)
    });
    Ok(0)
}

fn preview_data(ctx: &Ctx) -> anyhow::Result<Value> { ctx.c.post("/policies/preview", &json!({})) }

fn render_preview(d: &Value) -> String {
    let mut s = String::new();
    let count = n(d, "/draft_count");
    if count == 0 {
        return "没有未发布的草稿".into();
    }
    s.push_str(&format!("草稿 {count} 份，将影响 {} 台线路机；基于最近 {} 小时的访问汇总模拟\n", n(d, "/affected_relays"), n(d, "/window_hours")));
    let changes: Vec<Vec<String>> = d["changes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            vec![
                if st(c, "/scope") == "team" { "团队".into() } else { "节点".into() },
                st(c, "/name"),
                brief(&st(c, "/from/mode"), n(c, "/from/domains") as usize),
                brief(&st(c, "/to/mode"), n(c, "/to/domains") as usize),
            ]
        })
        .collect();
    if !changes.is_empty() {
        s.push_str(&table::render(&["范围", "名称", "现在", "改为"], &changes));
        s.push('\n');
    }
    let nodes = d["nodes"].as_array().cloned().unwrap_or_default();
    if nodes.is_empty() {
        s.push_str("没有节点会被阻止连接");
    } else {
        let rows: Vec<Vec<String>> = nodes
            .iter()
            .map(|x| {
                let top: Vec<String> = x["domains"].as_array().into_iter().flatten().take(5).map(|h| format!("{}({})", st(h, "/host"), n(h, "/conns"))).collect();
                vec![st(x, "/name"), mode_cn(&st(x, "/mode")).to_string(), n(x, "/total_conns").to_string(), n(x, "/would_block").to_string(), top.join(" ")]
            })
            .collect();
        s.push_str(&table::render(&["节点", "模式", "24h 连接", "将阻止", "主要被阻止的域名"], &rows));
    }
    s
}

pub fn preview(ctx: &Ctx) -> anyhow::Result<u8> {
    let d = preview_data(ctx)?;
    ctx.emit(&d, || render_preview(&d));
    Ok(0)
}

/// 先预览拿到 draft_hash（也让操作者看到影响），确认后带着这个 hash 发布 / 丢弃
fn with_hash(ctx: &Ctx, yes: bool, ask: &str, path: &str) -> anyhow::Result<u8> {
    let pv = preview_data(ctx)?;
    let Some(hash) = pv["draft_hash"].as_str().map(String::from) else { bail!("没有未发布的草稿") };
    if !ctx.json {
        crate::cmds::outln(render_preview(&pv));
    }
    confirm(ask, yes)?;
    let r = ctx.c.post(path, &json!({"draft_hash": hash}))?;
    ctx.emit(&r, || {
        if path.ends_with("publish") {
            format!("已发布为版本 {}，状态：{}（线路机应用情况：fleetctl policy versions）", st(&r, "/seq"), status_cn(&st(&r, "/status")))
        } else {
            format!("已丢弃 {} 份草稿", st(&r, "/discarded"))
        }
    });
    Ok(0)
}

pub fn publish(ctx: &Ctx, yes: bool) -> anyhow::Result<u8> {
    with_hash(ctx, yes, "确认发布全部草稿？会立即改写受影响线路机的路由规则，被阻止的连接马上生效", "/policies/publish")
}

pub fn discard(ctx: &Ctx, yes: bool) -> anyhow::Result<u8> { with_hash(ctx, yes, "确认丢弃全部草稿？", "/policies/discard") }

pub fn versions(ctx: &Ctx) -> anyhow::Result<u8> {
    let d = ctx.c.get("/policies/versions")?;
    ctx.emit(&d, || {
        let list = d.as_array().cloned().unwrap_or_default();
        if list.is_empty() {
            return "还没有发布过任何版本".into();
        }
        let rows: Vec<Vec<String>> = list
            .iter()
            .map(|v| {
                let rel = v["relays"].as_array().cloned().unwrap_or_default();
                let ok = rel.iter().filter(|r| matches!(st(r, "/status").as_str(), "applied" | "gone")).count();
                let kind = if st(v, "/kind") == "rollback" { format!("回滚到 {}", st(v, "/rollback_of_seq")) } else { "发布".into() };
                let mut status = status_cn(&st(v, "/status")).to_string();
                if v["is_current"].as_bool() == Some(true) {
                    status.push_str("（当前）");
                }
                let changes: Vec<String> = v["changes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(3)
                    .map(|c| format!("{}→{}", st(c, "/name"), mode_cn(&st(c, "/to/mode"))))
                    .collect();
                vec![st(v, "/seq"), kind, st(v, "/actor"), table::ts(&st(v, "/created_at")), status, format!("{ok}/{}", rel.len()), changes.join("，")]
            })
            .collect();
        let mut s = table::render(&["版本", "类型", "操作者", "时间(UTC)", "状态", "线路机", "变更"], &rows);
        let bad: Vec<String> = list
            .iter()
            .flat_map(|v| v["relays"].as_array().cloned().unwrap_or_default().into_iter().map(move |r| (st(v, "/seq"), r)))
            .filter(|(_, r)| st(r, "/status") == "failed")
            .map(|(seq, r)| format!("  版本 {seq} · {}：{}", st(&r, "/name"), st(&r, "/error")))
            .collect();
        if !bad.is_empty() {
            s.push_str(&format!("\n应用失败的线路机：\n{}", bad.join("\n")));
        }
        s
    });
    Ok(0)
}

/// `3` / `v3` / 版本 id → 版本对象
pub fn pick_version<'a>(list: &'a [Value], q: &str) -> Result<&'a Value, String> {
    let q = q.trim();
    let num: Option<i64> = q.trim_start_matches(['v', 'V']).parse().ok();
    list.iter()
        .find(|v| st(v, "/id") == q || num.is_some_and(|k| v["seq"].as_i64() == Some(k)))
        .ok_or_else(|| format!("没有这个版本「{q}」（只保留最近 30 个；先 fleetctl policy versions 看列表）"))
}

pub fn rollback(ctx: &Ctx, version: &str, yes: bool) -> anyhow::Result<u8> {
    let d = ctx.c.get("/policies/versions")?;
    let list = d.as_array().cloned().unwrap_or_default();
    let v = pick_version(&list, version).map_err(|e| anyhow!(e))?;
    if v["rollbackable"].as_bool() == Some(false) {
        bail!("版本 {} 和当前生效的规则一致，不需要回滚", st(v, "/seq"));
    }
    confirm(&format!("确认把访问策略回滚到版本 {}？会作为新版本发布，立即改写线路机路由规则", st(v, "/seq")), yes)?;
    let r = ctx.c.post(&format!("/policies/versions/{}/rollback", st(v, "/id")), &json!({}))?;
    ctx.emit(&r, || format!("已回滚：新版本 {}，状态：{}", st(&r, "/seq"), status_cn(&st(&r, "/status"))));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_refs() {
        let vs = vec![json!({"id": "aaa", "seq": 3}), json!({"id": "bbb", "seq": 2})];
        assert_eq!(pick_version(&vs, "3").unwrap()["id"], "aaa");
        assert_eq!(pick_version(&vs, "v2").unwrap()["id"], "bbb");
        assert_eq!(pick_version(&vs, "bbb").unwrap()["seq"], 2);
        assert!(pick_version(&vs, "9").unwrap_err().contains("没有这个版本"));
    }

    #[test]
    fn brief_texts() {
        assert_eq!(brief("denylist", 3), "黑名单 · 3 个域名");
        assert_eq!(brief("blocked", 0), "暂停新连接");
        assert_eq!(draft_brief(&json!({"draft": null})), "-");
        assert_eq!(draft_brief(&json!({"draft": {"mode": "allowlist", "domains": ["a.com"]}})), "白名单 · 1 个域名");
    }

    #[test]
    fn base_prefers_draft() {
        let p = json!({"mode": "observe", "draft": {"mode": "blocked"}});
        assert_eq!(base_of(&p)["mode"], "blocked");
        let p = json!({"mode": "observe", "draft": null});
        assert_eq!(base_of(&p)["mode"], "observe");
    }
}
