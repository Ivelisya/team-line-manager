//! 每日流量：按组织时区的自然日，全部节点每天的业务流量（下载 / 上传）、节点排行、线路机网卡计费量。

use crate::cmds::{st, Ctx};
use crate::resolve::Want;
use crate::table;
use serde_json::Value;

// `fleetctl traffic` 的参数（clap 直接解析成它）
#[derive(clap::Args)]
pub struct TrafficOpts {
    /// 近几天（含今天，最多 92）
    #[arg(long, default_value_t = 30)]
    pub days: i64,
    /// 只看这个团队的节点（团队名或 id）
    #[arg(long)]
    pub team: Option<String>,
    /// 只看这台机器的每日明细（id / 显示名 / IP）；线路机是经过它的全部业务流量，另列网卡
    #[arg(long)]
    pub node: Option<String>,
}

fn n(v: &Value, k: &str) -> f64 { v[k].as_f64().unwrap_or(0.0) }
fn t(v: &Value, k: &str) -> String { table::traffic(n(v, k)) }

/// 每天一行：日期、下载、上传、合计
fn days_table(list: &[Value]) -> String {
    let rows: Vec<Vec<String>> = list.iter().map(|d| vec![st(d, "/date"), t(d, "down"), t(d, "up"), table::traffic(n(d, "down") + n(d, "up"))]).collect();
    table::render(&["日期", "下载", "上传", "合计"], &rows)
}

pub fn traffic(ctx: &Ctx, o: TrafficOpts) -> anyhow::Result<u8> {
    let mut q: Vec<(&str, String)> = vec![("days", o.days.to_string())];
    if let Some(tm) = o.team.as_deref() {
        q.push(("team", ctx.team_id(tm)?));
    }
    let node = o.node.as_deref().map(|x| ctx.find(x, Want::Any)).transpose()?;
    if let Some(m) = &node {
        q.push(("machine", st(m, "/id")));
    }
    let d = ctx.c.get_q("/traffic/daily", &q)?;
    ctx.emit(&d, || {
        let arr = |k: &str| d[k].as_array().cloned().unwrap_or_default();
        let (total, nodes, relays) = (arr("total"), arr("nodes"), arr("relays"));
        let days = total.len().max(1) as f64;
        let sum = |k: &str| total.iter().map(|x| n(x, k)).sum::<f64>();
        let (down, up) = (sum("down"), sum("up"));
        let head = format!(
            "近 {} 天（{}，今天算到目前为止）业务流量：下载 {} · 上传 {} · 合计 {} · 日均 {}",
            total.len(),
            crate::meta::tz_label(),
            table::traffic(down),
            table::traffic(up),
            table::traffic(down + up),
            table::traffic((down + up) / days)
        );
        if let Some(m) = &node {
            let mut s = format!("{} 每日业务流量\n{head}\n{}", st(m, "/name"), days_table(&total));
            // 线路机：另列每天的网卡收发与计费量
            if let Some(r) = relays.iter().find(|r| r["id"] == m["id"]) {
                let rows: Vec<Vec<String>> = r["daily"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|x| vec![st(x, "/date"), t(x, "rx"), t(x, "tx"), t(x, "billed")])
                    .collect();
                s += &format!("\n\n网卡（计费规则 {}）\n{}", crate::meta::label("traffic_rules", r["rule"].as_str().unwrap_or("max")), table::render(&["日期", "下行", "上行", "计费量"], &rows));
            }
            return s;
        }
        let top: Vec<Vec<String>> = nodes
            .iter()
            .take(20)
            .map(|x| {
                let sum = n(x, "down") + n(x, "up");
                let lc = if x["lifecycle"] == "retired" { "（已退役）" } else { "" };
                vec![format!("{}{lc}", st(x, "/name")), st(x, "/team/name"), t(x, "down"), t(x, "up"), table::traffic(sum), table::traffic(sum / days)]
            })
            .collect();
        let more = if nodes.len() > 20 { format!("（共 {} 个，前 20）", nodes.len()) } else { String::new() };
        let rl: Vec<Vec<String>> = relays
            .iter()
            .map(|r| vec![st(r, "/name"), t(r, "billed"), crate::meta::label("traffic_rules", r["rule"].as_str().unwrap_or("max")).to_string(), t(r, "rx"), t(r, "tx"), table::traffic(n(r, "down") + n(r, "up"))])
            .collect();
        format!(
            "{head}\n{}\n\n节点按区间合计{more}\n{}\n\n线路机（网卡计费量按供应商规则逐天算后相加）\n{}",
            days_table(&total),
            table::render(&["节点", "团队", "下载", "上传", "合计", "日均"], &top),
            table::render(&["线路机", "计费量", "规则", "下行", "上行", "经过的业务流量"], &rl)
        )
    });
    Ok(0)
}
