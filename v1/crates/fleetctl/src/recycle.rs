//! 回收站（只读）：退役的终点。面板里的机器到期回收或立即回收后在这里，旧面板的退役机器用 `recycle import` 导进来。

use crate::cmds::{cycle_cn, kind_cn, st, Ctx};
use crate::table;
use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::path::Path;

/// 「2026-10-21」：时间按组织时区（同其它命令）只取日期
fn day(v: &Value, p: &str) -> String {
    let s = table::ts(&st(v, p));
    s.get(..10).unwrap_or(if s.is_empty() { "-" } else { &s }).to_string()
}

/// 「团队2-07（回收时 备用-03）」
fn name_cell(i: &Value) -> String {
    match i["last_name"].as_str().filter(|l| *l != st(i, "/name")) {
        Some(l) => format!("{}（回收时 {l}）", st(i, "/name")),
        None => st(i, "/name"),
    }
}

fn source_cn(i: &Value) -> &'static str { if i["source"] == "legacy" { "旧面板" } else { "面板" } }

/// `recycle ls`：按回收时间倒序。`--json` 是 /recycle-bin 的 data 原样（`{items, total, next_offset}`）
pub fn ls(ctx: &Ctx, source: Option<&str>, q: Option<&str>, limit: i64) -> anyhow::Result<u8> {
    let mut query = vec![("limit", limit.to_string())];
    if let Some(s) = source {
        query.push(("source", s.to_string()));
    }
    if let Some(k) = q {
        query.push(("q", k.to_string()));
    }
    let d = ctx.c.get_q("/recycle-bin", &query)?;
    ctx.emit(&d, || {
        let items = d["items"].as_array().cloned().unwrap_or_default();
        if items.is_empty() {
            return "回收站是空的（或没有匹配的）".into();
        }
        let rows: Vec<Vec<String>> = items
            .iter()
            .map(|i| {
                let pp = [st(i, "/provider"), st(i, "/plan")].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" · ");
                vec![
                    st(i, "/id"),
                    name_cell(i),
                    kind_cn(&st(i, "/kind")).to_string(),
                    st(i, "/ip"),
                    if pp.is_empty() { "-".into() } else { pp },
                    format!("{} → {}", day(i, "/retired_at"), day(i, "/recycled_at")),
                    st(i, "/reason"),
                    source_cn(i).into(),
                ]
            })
            .collect();
        let more = if d["next_offset"].is_null() { String::new() } else { format!("\n共 {} 条，只列了最近 {}（--limit 调大）", st(&d, "/total"), items.len()) };
        format!("{}{more}", table::render(&["id", "名字", "类型", "IP", "供应商 · 套餐", "退役 → 回收", "原因", "来源"], &rows))
    });
    Ok(0)
}

/// `recycle show <id|名字|IP>`：一条的全部列与回收时的快照。名字、IP 对上多条时列出候选
pub fn show(ctx: &Ctx, which: &str) -> anyhow::Result<u8> {
    let which = which.trim();
    let id = match which.parse::<i64>() {
        Ok(id) => id,
        Err(_) => {
            let d = ctx.c.get_q("/recycle-bin", &[("q", which.to_string()), ("limit", "500".into())])?;
            let items = d["items"].as_array().cloned().unwrap_or_default();
            let hit: Vec<&Value> = items.iter().filter(|i| st(i, "/name") == which || st(i, "/last_name") == which || st(i, "/ip") == which).collect();
            match hit.as_slice() {
                [one] => one["id"].as_i64().unwrap_or_default(),
                [] => bail!("回收站里没有「{which}」（可用 id、退役前或回收时的名字、IP；fleetctl recycle ls -q {which} 看相近的）"),
                many => {
                    let lines: Vec<String> = many.iter().map(|i| format!("  #{}  {}  {}  {}", st(i, "/id"), name_cell(i), st(i, "/ip"), day(i, "/recycled_at"))).collect();
                    bail!("「{which}」对上了 {} 条，用 id 指定：\n{}", many.len(), lines.join("\n"));
                }
            }
        }
    };
    let d = ctx.c.get(&format!("/recycle-bin/{id}"))?;
    ctx.emit(&d, || {
        let money = match (d["price_cents"].as_i64(), d["currency"].as_str()) {
            (Some(c), Some(cur)) => format!("{:.2} {cur} / {}", c as f64 / 100.0, cycle_cn(&st(&d, "/cycle"))),
            _ => "-".into(),
        };
        let gb = |p: &str| d.pointer(p).and_then(Value::as_i64).map(|b| table::traffic(b as f64)).unwrap_or("-".into());
        let rows = vec![
            ("名字", name_cell(&d)),
            ("类型", kind_cn(&st(&d, "/kind")).to_string()),
            ("团队", st(&d, "/team")),
            ("IP", st(&d, "/ip")),
            ("宿主机", st(&d, "/host_name")),
            ("供应商 · 套餐", format!("{} · {}", st(&d, "/provider"), st(&d, "/plan"))),
            ("供应商编号", st(&d, "/service_ref")),
            ("价格", money),
            ("上架 → 退役 → 回收", format!("{} → {} → {}", day(&d, "/activated_at"), day(&d, "/retired_at"), day(&d, "/recycled_at"))),
            ("到期日", st(&d, "/due_date")),
            ("原因", st(&d, "/reason")),
            ("业务流量", format!("下载 {} / 上传 {}", gb("/traffic_down"), gb("/traffic_up"))),
            ("网卡流量", format!("下行 {} / 上行 {}", gb("/nic_rx"), gb("/nic_tx"))),
            ("来源", format!("{}（{}）", source_cn(&d), st(&d, "/source_id"))),
            ("操作人", st(&d, "/actor")),
        ];
        format!("{}\n快照（--json 看全部）：{} 个入口记录、{} 条续费记录", table::kv(&rows), d["snapshot"]["entries"].as_array().map_or(0, Vec::len), d["snapshot"]["renewals"].as_array().map_or(0, Vec::len))
    });
    Ok(0)
}

/// `recycle import --file <json>`：旧面板的退役机器导入回收站。文件是 `{"items": [...]}` 或直接一个数组（每项见 API.md）。
/// 不加 --yes 只预演：列出会导入、会跳过（导入过的、对上面板里还没回收的机器）的；加了才写
pub fn import(ctx: &Ctx, file: &Path, yes: bool) -> anyhow::Result<u8> {
    let text = std::fs::read_to_string(file).with_context(|| format!("读取 {}", file.display()))?;
    let v: Value = serde_json::from_str(&text).with_context(|| format!("{} 不是合法 JSON", file.display()))?;
    let items = match v {
        Value::Array(a) => Value::Array(a),
        Value::Object(mut o) => o.remove("items").filter(Value::is_array).context("JSON 里要有 items 数组")?,
        _ => bail!("JSON 要是数组，或带 items 数组的对象"),
    };
    let d = ctx.c.post("/recycle-bin/import", &json!({"items": items, "confirm": yes}))?;
    let inserted = d["inserted"].as_array().cloned().unwrap_or_default();
    let skipped = d["skipped"].as_array().cloned().unwrap_or_default();
    ctx.emit(&d, || {
        let mut s = format!("{}导入 {} 条，跳过 {} 条", if yes { "已" } else { "预演：会" }, inserted.len(), skipped.len());
        if !inserted.is_empty() {
            let rows: Vec<Vec<String>> = inserted.iter().map(|i| vec![st(i, "/source_id"), st(i, "/name"), st(i, "/id")]).collect();
            s += &format!("\n{}", table::render(&["source_id", "名字", "回收站 id"], &rows));
        }
        if !skipped.is_empty() {
            let rows: Vec<Vec<String>> = skipped.iter().map(|i| vec![st(i, "/source_id"), st(i, "/name"), st(i, "/why")]).collect();
            s += &format!("\n跳过：\n{}", table::render(&["source_id", "名字", "原因"], &rows));
        }
        if !yes {
            s += "\n以上是预演，没有写入。确认名单后加 --yes 导入";
        }
        s
    });
    Ok(0)
}
