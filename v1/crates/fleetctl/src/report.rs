//! 报表：月度可用率汇总（`report uptime`）与月报（`report monthly`），口径见中控 reports.rs。

use crate::cmds::{outln, st, Ctx};
use crate::ops::confirm;
use crate::table;
use clap::Subcommand;
use serde_json::{json, Value};

#[derive(Subcommand)]
pub enum ReportCmd {
    /// 月度可用率汇总：先按供应商（平均 / 最差出口成功率，从差到好），再列每个在用节点（出口 / TCP 成功率、断线次数与最长断线、告警次数，从差到好）。
    /// 默认本月（算到今天）；--month 看某个月，--days 看近几天（含今天），二选一。成功率取自小时汇总的探测，窗口内没有探测的写「-」
    Uptime {
        /// YYYY-MM
        #[arg(long, value_parser = month_arg, conflicts_with = "days")]
        month: Option<String>,
        /// 近几天（含今天），1–90
        #[arg(long, value_parser = clap::value_parser!(i64).range(1..=90))]
        days: Option<i64>,
    },
    /// 月报：默认打印上个月的摘要文字（总流量、月费、USD/GB 最高、可用率最差、告警最多的规则、到期）；--month 指定月份（流量明细只留近 92 天）。
    /// --send 立即发一次（测试用，平时每月 1 号自动发）：只发给开了「月报」的已启用通知渠道，没有这样的渠道会报错；要确认（--yes）
    Monthly {
        /// YYYY-MM，默认上个月
        #[arg(long, value_parser = month_arg)]
        month: Option<String>,
        #[arg(long)]
        send: bool,
        #[arg(long, requires = "send")]
        yes: bool,
    },
}

/// `YYYY-MM`（月份 01–12）；中控还会校验范围
fn month_arg(s: &str) -> Result<String, String> {
    let ok = s.len() == 7 && s.is_char_boundary(4) && s.as_bytes()[4] == b'-' && s[..4].bytes().all(|b| b.is_ascii_digit()) && s[5..].parse::<u32>().is_ok_and(|m| (1..=12).contains(&m));
    if ok { Ok(s.to_string()) } else { Err("月份写成 YYYY-MM，例如 2026-09".into()) }
}

/// 成功率（0–100）：没有探测写「-」；不到 100 的最多写 99.99%（同月报文字，不把 99.996 写成 100%）
fn pct_cell(v: &Value, k: &str) -> String {
    match v[k].as_f64() {
        None => "-".into(),
        Some(p) if p >= 100.0 => "100%".into(),
        Some(p) => format!("{:.2}%", p.min(99.99)),
    }
}

fn hours(h: i64) -> String { if h > 0 { format!("{h} 小时") } else { "-".into() } }

/// `report uptime`：`--json` 是 /reports/uptime 的 data 原样（`{month, days, from, to, tz, items, providers}`）
pub fn uptime(ctx: &Ctx, month: Option<&str>, days: Option<i64>) -> anyhow::Result<u8> {
    let mut q: Vec<(&str, String)> = vec![];
    if let Some(m) = month {
        q.push(("month", m.to_string()));
    }
    if let Some(d) = days {
        q.push(("days", d.to_string()));
    }
    let d = ctx.c.get_q("/reports/uptime", &q)?;
    ctx.emit(&d, || {
        let arr = |k: &str| d[k].as_array().cloned().unwrap_or_default();
        let (items, providers) = (arr("items"), arr("providers"));
        let span = match d["month"].as_str() {
            Some(m) => format!("{m} 月"),
            None => format!("近 {} 天", st(&d, "/days")),
        };
        let head = format!("{span}：{} 至 {}（{}）· 在用节点 {} 个", st(&d, "/from"), st(&d, "/to"), crate::meta::tz_label(), items.len());
        if items.is_empty() {
            return format!("{head}\n没有在用节点");
        }
        let prov: Vec<Vec<String>> = providers
            .iter()
            .map(|p| {
                let worst = if p["worst"].is_object() { format!("{}（{}）", st(p, "/worst/name"), pct_cell(p, "worst_egress_pct")) } else { "-".into() };
                vec![st(p, "/name"), st(p, "/nodes"), st(p, "/with_data"), pct_cell(p, "avg_egress_pct"), worst, pct_cell(p, "avg_tcp_pct"), pct_cell(p, "worst_tcp_pct"), st(p, "/outages"), st(p, "/incidents")]
            })
            .collect();
        let rows: Vec<Vec<String>> = items
            .iter()
            .map(|i| {
                vec![st(i, "/name"), st(i, "/team/name"), st(i, "/provider/name"), pct_cell(i, "egress_pct"), pct_cell(i, "tcp_pct"), st(i, "/outages"), hours(i["longest_outage_h"].as_i64().unwrap_or(0)), st(i, "/incidents")]
            })
            .collect();
        format!(
            "{head}\n\n按供应商（从差到好；平均 / 最差只看窗口内有探测的节点；断线次数是各节点之和）\n{}\n\n按节点（出口成功率从差到好，没有探测的在最后；断线 = 出口探测失败 3 次以上的整点小时，连着的算一次；告警 = 窗口内可用性相关的告警次数）\n{}",
            table::render(&["供应商", "节点", "有数据", "平均出口", "最差出口（节点）", "平均 TCP", "最差 TCP", "断线", "告警"], &prov),
            table::render(&["节点", "团队", "供应商", "出口", "TCP", "断线", "最长断线", "告警"], &rows)
        )
    });
    Ok(0)
}

/// `report monthly`：打印月报文字；`--json` 是 /reports/monthly 的 data 原样。`--send` 确认后立即发一次（`--json` 是 `{month, channels, text}`）
pub fn monthly(ctx: &Ctx, month: Option<&str>, send: bool, yes: bool) -> anyhow::Result<u8> {
    let mut body = json!({});
    if let Some(m) = month {
        body["month"] = json!(m);
    }
    if !send {
        let q: Vec<(&str, String)> = month.map(|m| ("month", m.to_string())).into_iter().collect();
        let d = ctx.c.get_q("/reports/monthly", &q)?;
        ctx.emit(&d, || st(&d, "/text"));
        return Ok(0);
    }
    let what = month.map_or("上个月".to_string(), |m| format!("{m} 月"));
    confirm(&format!("确认现在把{what}的月报发给所有开了「月报」的已启用通知渠道？"), yes)?;
    let d = ctx.c.post("/reports/monthly/send", &body)?;
    ctx.emit(&d, || {
        outln(st(&d, "/text"));
        format!("已发送 {} 月的月报，渠道 {} 个（发送结果见通知日志）", st(&d, "/month"), st(&d, "/channels"))
    });
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_and_percent_formats() {
        assert!(month_arg("2026-09").is_ok() && month_arg("2026-12").is_ok());
        for bad in ["2026-13", "2026-00", "2026-9", "26-09", "2026/09", "九月-2026", "２０２６-09"] {
            assert!(month_arg(bad).is_err(), "{bad}");
        }
        let v = json!({"a": 99.996, "b": 100.0, "c": 87.456, "d": null});
        assert_eq!((pct_cell(&v, "a").as_str(), pct_cell(&v, "b").as_str(), pct_cell(&v, "c").as_str(), pct_cell(&v, "d").as_str()), ("99.99%", "100%", "87.46%", "-"));
    }
}
