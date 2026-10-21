//! 访问监控：哪个节点访问了哪些域名 / IP、来源地址、新出现的目标、原始记录。

use crate::cmds::{st, Ctx};
use crate::resolve::Want;
use crate::table;
use serde_json::Value;

pub struct AccessOpts<'a> {
    pub view: &'a str,
    pub node: Option<&'a str>,
    pub team: Option<&'a str>,
    pub window: &'a str,
    pub search: Option<&'a str>,
    pub blocked: bool,
    pub kind: Option<&'a str>,
    pub limit: i64,
}

fn num(v: &Value, ptr: &str) -> String { v.pointer(ptr).and_then(Value::as_i64).map(|n| n.to_string()).unwrap_or_default() }

pub fn access(ctx: &Ctx, o: AccessOpts) -> anyhow::Result<u8> {
    let mut q: Vec<(&str, String)> = vec![];
    if let Some(n) = o.node {
        q.push(("node", ctx.find_id(n, Want::Any)?.0));
    }
    if let Some(t) = o.team {
        q.push(("team", crate::ops::lookup(&ctx.teams()?, t, &["name"], "团队")?));
    }
    if let Some(s) = o.search {
        q.push(("search", s.to_string()));
    }
    if o.blocked {
        q.push(("action", "blocked".into()));
    }
    if let Some(k) = o.kind {
        q.push(("kind", k.to_string()));
    }
    let lim = ("limit", o.limit.to_string());
    match o.view {
        "summary" => {
            q.push(("window", o.window.to_string()));
            let d = ctx.c.get_q("/access/summary", &q)?;
            ctx.emit(&d, || {
                let k = &d["kpi"];
                let rows: Vec<Vec<String>> = d["nodes"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    // 来源数：24h 以上带搜索 / 类型 / 处理结果筛选时中控给 null（算不出），显示「-」
                    .map(|n| vec![st(n, "/name"), st(n, "/team"), num(n, "/conns"), num(n, "/domains"), st(n, "/sources"), num(n, "/blocked"), table::ts(&st(n, "/last_ts"))])
                    .collect();
                format!(
                    "自 {} 起：有活动的节点 {} · 连接 {} · 访问目标 {} · 已阻止 {}\n{}",
                    table::ts(&st(&d, "/window_start")),
                    num(k, "/active_nodes"),
                    num(k, "/conns"),
                    num(k, "/targets"),
                    num(k, "/blocked"),
                    table::render(&["节点", "团队", "连接", "域名", "来源", "阻止", "最近(UTC)"], &rows)
                )
            });
        }
        "targets" | "new" => {
            q.push(("window", o.window.to_string()));
            q.push(lim);
            let new = o.view == "new";
            let d = ctx.c.get_q(if new { "/access/new-targets" } else { "/access/targets" }, &q)?;
            let items = d["items"].clone();
            ctx.emit(&d, || {
                let list = items.as_array().cloned().unwrap_or_default();
                if list.is_empty() {
                    return "这段时间没有访问记录".into();
                }
                let rows: Vec<Vec<String>> = list
                    .iter()
                    .map(|t| {
                        let total = if new { num(t, "/total") } else { num(t, "/conns") };
                        vec![st(t, "/dest_host"), st(t, "/kind"), st(t, "/name"), total, num(t, "/sources"), num(t, "/blocked"), table::ts(&st(t, "/first_seen")), table::ts(&st(t, "/last_seen"))]
                    })
                    .collect();
                format!("{}\n共 {} 条", table::render(&["目标", "类型", "节点", "连接", "来源", "阻止", "首次(UTC)", "最近(UTC)"], &rows), rows.len())
            });
        }
        "sources" => {
            q.push(("window", o.window.to_string()));
            q.push(lim);
            let d = ctx.c.get_q("/access/sources", &q)?;
            ctx.emit(&d, || {
                let list = d["items"].as_array().cloned().unwrap_or_default();
                let rows: Vec<Vec<String>> = list.iter().map(|s| vec![st(s, "/src_ip"), st(s, "/name"), num(s, "/conns"), table::ts(&st(s, "/last_ts"))]).collect();
                format!("{}\n共 {} 条", table::render(&["来源 IP", "节点", "连接", "最近(UTC)"], &rows), rows.len())
            });
        }
        _ => {
            q.push(lim);
            let d = ctx.c.get_q("/access/events", &q)?;
            ctx.emit(&d, || {
                let list = d["items"].as_array().cloned().unwrap_or_default();
                let rows: Vec<Vec<String>> = list
                    .iter()
                    .map(|e| {
                        let port = num(e, "/dest_port");
                        let res = if e["blocked"].as_bool() == Some(true) { "已阻止" } else { "已转发" };
                        vec![table::ts(&st(e, "/ts")), st(e, "/name"), st(e, "/src_ip"), format!("{}:{port}", st(e, "/dest_host")), st(e, "/network"), res.into()]
                    })
                    .collect();
                format!("{}\n共 {} 条", table::render(&["时间(UTC)", "节点", "来源", "目标", "协议", "结果"], &rows), rows.len())
            });
        }
    }
    Ok(0)
}
