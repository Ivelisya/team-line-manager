//! 三网 PING：线路机往外测线路质量（延迟、丢包），每分钟一轮、每个目标 5 次。只展示，不开告警。
//! - `ping-targets`：全局 8 个槽位（名称、目标）与方式；`ping-targets set <槽位> --name --target`、`ping-targets mode tcp|icmp`
//! - `ping <线路机>`：这台合成后的目标（跟随全局 / 单独设置）与当前读数；`ping <线路机> set|reset|mode` 改这台的覆盖
//! - `show <线路机>` 里也带当前读数（`--json` 时在 `ping` 字段）

use crate::cmds::{num, st, Ctx};
use crate::resolve::Want;
use crate::table;
use anyhow::bail;
use serde_json::{json, Value};

const PATH: &str = "/settings/ping-targets";

fn slot_no(n: u8) -> anyhow::Result<usize> {
    if (1..=8).contains(&n) { Ok(n as usize - 1) } else { bail!("槽位是 1–8") }
}

fn mode_cn(m: &str) -> &str {
    match m {
        "tcp" => "TCP 建连（不写端口连 80）",
        "icmp" => "ICMP（系统 ping，用不了时退回 TCP）",
        m => m,
    }
}

fn global_text(g: &Value) -> String {
    let rows: Vec<Vec<String>> = g["slots"]
        .as_array()
        .map(|a| a.iter().enumerate().map(|(i, s)| vec![(i + 1).to_string(), st(s, "/name"), s["target"].as_str().filter(|t| !t.is_empty()).unwrap_or("（不测）").to_string()]).collect())
        .unwrap_or_default();
    format!("方式：{}；每 {} 秒一轮、每个目标 {} 次\n{}", mode_cn(&st(g, "/mode")), st(g, "/interval_s"), st(g, "/count"), table::render(&["槽位", "名称", "目标"], &rows))
}

/// 全局：不给参数显示
pub fn targets(ctx: &Ctx) -> anyhow::Result<u8> {
    let g = ctx.c.get(PATH)?;
    ctx.emit(&g, || global_text(&g));
    Ok(0)
}

fn put_global(ctx: &Ctx, f: impl FnOnce(&mut Value)) -> anyhow::Result<u8> {
    let mut g = ctx.c.get(PATH)?;
    f(&mut g);
    let d = ctx.c.put(PATH, &json!({"mode": g["mode"], "slots": g["slots"]}))?;
    ctx.emit(&d, || format!("已保存（全部在用线路机一分钟内按新目标测）\n{}", global_text(&d)));
    Ok(0)
}

pub fn targets_set(ctx: &Ctx, slot: u8, name: Option<&str>, target: Option<&str>) -> anyhow::Result<u8> {
    let i = slot_no(slot)?;
    if name.is_none() && target.is_none() {
        bail!("给 --name 或 --target（--target \"\" 清空：这个槽位不测）");
    }
    put_global(ctx, |g| {
        if let Some(n) = name {
            g["slots"][i]["name"] = json!(n);
        }
        if let Some(t) = target {
            g["slots"][i]["target"] = json!(t);
        }
    })
}

pub fn targets_mode(ctx: &Ctx, mode: &str) -> anyhow::Result<u8> { put_global(ctx, |g| g["mode"] = json!(mode)) }

/// 一台线路机的当前读数表：目标、方式、延迟、丢包、来源
pub fn current_table(p: &Value) -> String {
    let src: std::collections::HashMap<u64, String> =
        p["slots"].as_array().map(|a| a.iter().map(|s| (s["slot"].as_u64().unwrap_or(0), st(s, "/source"))).collect()).unwrap_or_default();
    let rows: Vec<Vec<String>> = p["current"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|c| {
                    let slot = c["slot"].as_u64().unwrap_or(0);
                    vec![
                        format!("{}", slot + 1),
                        st(c, "/name"),
                        st(c, "/target"),
                        st(c, "/mode"),
                        num(c, "/cur_ms").map_or_else(|| "-".into(), |m| format!("{m:.0} ms")),
                        num(c, "/cur_loss").map_or_else(|| "-".into(), |l| format!("{l:.0}%")),
                        if src.get(&slot).map(String::as_str) == Some("override") { "单独设置".into() } else { "跟随全局".into() },
                    ]
                })
                .collect()
        })
        .unwrap_or_default();
    if rows.is_empty() {
        return "没有在测的目标".into();
    }
    format!("方式：{}\n{}", mode_cn(&st(p, "/mode")), table::render(&["槽位", "名称", "目标", "实际方式", "延迟（中位）", "丢包", "来源"], &rows))
}

/// 一台线路机：合成后的目标与当前读数
pub fn show(ctx: &Ctx, relay: &str) -> anyhow::Result<u8> {
    let (id, name) = ctx.find_id(relay, Want::Any)?;
    let p = ctx.c.get(&format!("/machines/{id}/ping"))?;
    ctx.emit(&p, || format!("{name} 的三网 PING（最近一分钟）\n{}", current_table(&p)));
    Ok(0)
}

fn put_override(ctx: &Ctx, relay: &str, f: impl FnOnce(&mut Value)) -> anyhow::Result<u8> {
    let (id, name) = ctx.find_id(relay, Want::Any)?;
    let cur = ctx.c.get(&format!("/machines/{id}/ping"))?;
    let mut o = if cur["override"].is_object() { cur["override"].clone() } else { json!({"mode": null, "slots": [null, null, null, null, null, null, null, null]}) };
    if !o["slots"].is_array() {
        o["slots"] = json!([null, null, null, null, null, null, null, null]);
    }
    while o["slots"].as_array().is_some_and(|a| a.len() < 8) {
        o["slots"].as_array_mut().unwrap().push(Value::Null);
    }
    f(&mut o);
    let d = ctx.c.put(&format!("/machines/{id}/ping"), &json!({"override": o}))?;
    ctx.emit(&d, || format!("已保存 {name} 的三网 PING（一分钟内按新目标测）\n{}", current_table(&d)));
    Ok(0)
}

/// 这台单独设置某个槽位（没给的沿用这台现在的值）
pub fn set(ctx: &Ctx, relay: &str, slot: u8, name: Option<&str>, target: Option<&str>) -> anyhow::Result<u8> {
    let i = slot_no(slot)?;
    if name.is_none() && target.is_none() {
        bail!("给 --name 或 --target（--target \"\" = 这台不测这个槽位）");
    }
    let (id, _) = ctx.find_id(relay, Want::Any)?;
    let cur = ctx.c.get(&format!("/machines/{id}/ping"))?;
    let base = cur["slots"][i].clone();
    put_override(ctx, relay, |o| {
        o["slots"][i] = json!({"name": name.map_or_else(|| st(&base, "/name"), String::from), "target": target.map_or_else(|| st(&base, "/target"), String::from)});
    })
}

/// 改回跟随全局：给槽位只改那一个，不给全部（含方式）
pub fn reset(ctx: &Ctx, relay: &str, slot: Option<u8>) -> anyhow::Result<u8> {
    match slot {
        Some(n) => {
            let i = slot_no(n)?;
            put_override(ctx, relay, |o| o["slots"][i] = Value::Null)
        }
        None => {
            let (id, name) = ctx.find_id(relay, Want::Any)?;
            let d = ctx.c.put(&format!("/machines/{id}/ping"), &json!({"override": null}))?;
            ctx.emit(&d, || format!("{name} 的三网 PING 全部改回跟随全局\n{}", current_table(&d)));
            Ok(0)
        }
    }
}

/// 这台的方式：tcp / icmp / global（跟随全局）
pub fn mode(ctx: &Ctx, relay: &str, mode: &str) -> anyhow::Result<u8> {
    put_override(ctx, relay, |o| o["mode"] = if mode == "global" { Value::Null } else { json!(mode) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_table_wording() {
        let p = json!({"mode": "tcp", "slots": [{"slot": 0, "source": "global"}, {"slot": 1, "source": "override"}], "current": [
            {"slot": 0, "name": "武汉电信", "target": "hb-ct-v4.ip.zstaticcdn.com", "mode": "tcp", "cur_ms": 158.4, "cur_loss": 0.0},
            {"slot": 1, "name": "联通 9929", "target": "203.0.113.9", "mode": "tcp", "cur_ms": null, "cur_loss": 100.0}
        ]});
        let s = current_table(&p);
        assert!(s.contains("158 ms") && s.contains("100%") && s.contains("单独设置") && s.contains("跟随全局"), "{s}");
        assert_eq!(current_table(&json!({"current": []})), "没有在测的目标");
        assert!(slot_no(0).is_err() && slot_no(9).is_err() && slot_no(8).unwrap() == 7);
    }
}
