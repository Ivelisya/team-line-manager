//! 访问监控：哪个节点访问了哪些域名 / IP、来源地址、新出现的目标、原始记录。

use crate::cmds::{count_note, st, Ctx};
use crate::resolve::Want;
use crate::table;

// `fleetctl access` 的参数（clap 直接解析成它）
#[derive(clap::Args)]
pub struct AccessOpts {
    /// summary（按节点汇总）| targets（域名与 IP）| new（新出现的目标）| sources（来源地址）| events（原始记录）
    #[arg(default_value = "targets", value_parser = ["summary", "targets", "new", "sources", "events"])]
    pub view: String,
    /// 只看某台机器（id / 显示名 / IP）
    #[arg(long)]
    pub node: Option<String>,
    #[arg(long)]
    pub team: Option<String>,
    /// 1h | 24h | 7d | 30d，不能比汇总的保留期长（见 settings retention）；events 固定取最近记录
    #[arg(long, default_value = "24h")]
    pub window: String,
    /// 搜索域名（events / sources 也匹配来源 IP）
    #[arg(long)]
    pub search: Option<String>,
    /// 只看被阻止的连接
    #[arg(long)]
    pub blocked: bool,
    /// domain | ip
    #[arg(long)]
    pub kind: Option<String>,
    /// 每个视图最多列几条（默认 50，服务端上限 500）；summary 是列几个节点
    #[arg(long, default_value_t = 50)]
    pub limit: i64,
}

pub fn access(ctx: &Ctx, o: AccessOpts) -> anyhow::Result<u8> {
    let mut q: Vec<(&str, String)> = vec![];
    if let Some(n) = o.node.as_deref() {
        q.push(("node", ctx.find_id(n, Want::Any)?.0));
    }
    if let Some(t) = o.team.as_deref() {
        q.push(("team", ctx.team_id(t)?));
    }
    if let Some(s) = o.search.as_deref() {
        q.push(("search", s.to_string()));
    }
    if o.blocked {
        q.push(("action", "blocked".into()));
    }
    if let Some(k) = o.kind.as_deref() {
        q.push(("kind", k.to_string()));
    }
    let lim = ("limit", o.limit.to_string());
    match o.view.as_str() {
        "summary" => {
            q.push(("window", o.window.to_string()));
            let mut d = ctx.c.get_q("/access/summary", &q)?;
            // 中控按连接数降序给全部节点：只留前 --limit 个（KPI 仍是全部的）
            let all = d["nodes"].as_array().map_or(0, Vec::len);
            if let Some(a) = d["nodes"].as_array_mut() {
                a.truncate(o.limit.max(1) as usize);
            }
            ctx.emit(&d, || {
                let k = &d["kpi"];
                let nodes = d["nodes"].as_array().cloned().unwrap_or_default();
                let more = if nodes.len() < all { format!("\n只列连接数最多的 {} 个（共 {all} 个，--limit 调整）", nodes.len()) } else { String::new() };
                let rows: Vec<Vec<String>> = nodes
                    .iter()
                    // 来源数：24h 以上带搜索 / 类型 / 处理结果筛选时中控给 null（算不出），显示「-」
                    .map(|n| vec![st(n, "/name"), st(n, "/team"), st(n, "/conns"), st(n, "/domains"), st(n, "/sources"), st(n, "/blocked"), table::ts(&st(n, "/last_ts"))])
                    .collect();
                format!(
                    "自 {} 起：有活动的节点 {} · 连接 {} · 访问目标 {} · 已阻止 {}\n{}",
                    table::ts(&st(&d, "/window_start")),
                    st(k, "/active_nodes"),
                    st(k, "/conns"),
                    st(k, "/unique_targets"),
                    st(k, "/blocked"),
                    table::render(&["节点", "团队", "连接", "域名", "来源", "阻止", "最近"], &rows)
                ) + &more
            });
        }
        "targets" | "new" => {
            q.push(("window", o.window.to_string()));
            q.push(lim);
            let new = o.view == "new";
            let d = ctx.c.get_q(if new { "/access/new-targets" } else { "/access/targets" }, &q)?;
            let items = d["items"].clone();
            let total = d["total"].as_i64();
            ctx.emit(&d, || {
                let list = items.as_array().cloned().unwrap_or_default();
                if list.is_empty() {
                    return "这段时间没有访问记录".into();
                }
                let rows: Vec<Vec<String>> = list
                    .iter()
                    .map(|t| {
                        let total = if new { st(t, "/total") } else { st(t, "/conns") };
                        vec![st(t, "/dest_host"), st(t, "/kind"), st(t, "/name"), total, st(t, "/sources"), st(t, "/blocked"), table::ts(&st(t, "/first_seen")), table::ts(&st(t, "/last_seen"))]
                    })
                    .collect();
                format!("{}\n{}", table::render(&["目标", "类型", "节点", "连接", "来源", "阻止", "首次", "最近"], &rows), count_note(rows.len(), total, o.limit.clamp(1, 500), "条", "--limit 调整，最大 500"))
            });
        }
        "sources" => {
            q.push(("window", o.window.to_string()));
            q.push(lim);
            let d = ctx.c.get_q("/access/sources", &q)?;
            ctx.emit(&d, || {
                let list = d["items"].as_array().cloned().unwrap_or_default();
                let rows: Vec<Vec<String>> = list.iter().map(|s| vec![st(s, "/src_ip"), st(s, "/name"), st(s, "/conns"), table::ts(&st(s, "/last_ts"))]).collect();
                format!("{}\n{}", table::render(&["来源 IP", "节点", "连接", "最近"], &rows), count_note(rows.len(), None, o.limit.clamp(1, 500), "条", "--limit 调整，最大 500"))
            });
        }
        "events" => {
            q.push(lim);
            let d = ctx.c.get_q("/access/events", &q)?;
            ctx.emit(&d, || {
                let list = d["items"].as_array().cloned().unwrap_or_default();
                let rows: Vec<Vec<String>> = list
                    .iter()
                    .map(|e| {
                        let port = st(e, "/dest_port");
                        let res = if e["blocked"].as_bool() == Some(true) { "已阻止" } else { "已转发" };
                        vec![table::ts(&st(e, "/ts")), st(e, "/name"), st(e, "/src_ip"), format!("{}:{port}", st(e, "/dest_host")), st(e, "/network"), res.into()]
                    })
                    .collect();
                format!("{}\n{}", table::render(&["时间", "节点", "来源", "目标", "协议", "结果"], &rows), count_note(rows.len(), None, o.limit.clamp(1, 500), "条", "--limit 调整，最大 500"))
            });
        }
        _ => unreachable!("clap 限定了视图名"),
    }
    Ok(0)
}
