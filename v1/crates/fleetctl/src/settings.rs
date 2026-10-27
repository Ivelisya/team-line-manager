//! 面板设置：访问记录保留期（`settings retention`）。访问监控照常记录，访问记录只留这么久，到期中控自动删除。

use crate::cmds::Ctx;
use crate::ops::confirm;
use crate::table;
use anyhow::bail;
use serde_json::{json, Value};

const PATH: &str = "/settings/access-retention";

fn days(v: &Value, k: &str) -> i64 { v[k].as_i64().unwrap_or(0) }

/// 人看的：两个天数各管什么；来源画像按学习期多留时说一句
fn text(v: &Value) -> String {
    let (hourly, profile) = (days(v, "hourly_days"), days(v, "profile_days"));
    let mut s = table::kv(&[
        ("原始记录", format!("{} 天（每一条访问：时间、来源 IP、目标）", days(v, "raw_days"))),
        ("汇总", format!("{hourly} 天（小时汇总、新目标、来源画像；访问页最长能看这么久）")),
    ]);
    if profile > hourly {
        s.push_str(&format!("\n来源画像（运营商、城市等）不短于 IP 情报的学习期，留 {profile} 天"));
    }
    s
}

/// 「原始记录 7 → 3 天，汇总 31 天」（同审计的摘要）
fn changes(old: (i64, i64), new: (i64, i64)) -> String {
    let one = |label: &str, a: i64, b: i64| if a == b { format!("{label} {b} 天") } else { format!("{label} {a} → {b} 天") };
    format!("{}，{}", one("原始记录", old.0, new.0), one("汇总", old.1, new.1))
}

/// 不给 `raw` / `hourly`：显示当前的。给了：只改给了的那个（另一个沿用现在的），先确认再提交；调短会删数据
pub fn retention(ctx: &Ctx, raw: Option<i64>, hourly: Option<i64>, yes: bool) -> anyhow::Result<u8> {
    let cur = ctx.c.get(PATH)?;
    if raw.is_none() && hourly.is_none() {
        ctx.emit(&cur, || text(&cur));
        return Ok(0);
    }
    let old = (days(&cur, "raw_days"), days(&cur, "hourly_days"));
    let new = (raw.unwrap_or(old.0), hourly.unwrap_or(old.1));
    let (min, max) = (days(&cur, "min"), days(&cur, "max"));
    if ![new.0, new.1].iter().all(|d| (min..=max).contains(d)) {
        bail!("保留天数只能是 {min}–{max} 天");
    }
    if new.1 < new.0 {
        bail!("汇总是从原始记录算出来的，要留得和原始记录一样久或更久：原始记录 {} 天时，--hourly 至少 {}", new.0, new.0);
    }
    if new == old {
        ctx.emit(&cur, || format!("保留期没有变化，未提交\n{}", text(&cur)));
        return Ok(0);
    }
    let what = if new.0 < old.0 || new.1 < old.1 {
        "超出新期限的访问记录会在几分钟内删掉，删了找不回来"
    } else {
        "已经删掉的找不回来，之后的记录按新期限保留"
    };
    let c = changes(old, new);
    confirm(&format!("确认修改访问记录保留期：{c}？{what}"), yes)?;
    let d = ctx.c.put(PATH, &json!({"raw_days": new.0, "hourly_days": new.1}))?;
    ctx.emit(&d, || format!("已修改访问记录保留期：{c}\n{}", text(&d)));
    Ok(0)
}

/// 新入口默认模式（`settings entry-mode`）：不给 `mode` 显示当前的；给了先确认再改
pub fn entry_mode(ctx: &Ctx, mode: Option<&str>, yes: bool) -> anyhow::Result<u8> {
    const P: &str = "/settings/entry-mode";
    let cur = ctx.c.get(P)?;
    let label = |v: &Value| format!("{}{}", v["label"].as_str().unwrap_or("?"), if v["set"] == true { "" } else { "（没设过，按内置默认）" });
    let Some(m) = mode else {
        ctx.emit(&cur, || format!("新入口默认模式：{}
只影响之后上架、加入口新建的入口；存量入口用 fleetctl entry reverse 逐个切", label(&cur)));
        return Ok(0);
    };
    let m = match m.trim() {
        "normal" | "普通" => "normal",
        "reverse" | "穿透" => "reverse",
        x => bail!("模式只能是 normal（普通）或 reverse（穿透），给的是「{x}」"),
    };
    let zh = if m == "reverse" { "穿透" } else { "普通" };
    confirm(&format!("确认把新入口默认模式改成「{zh}」（现在是{}）？只影响之后新建的入口", label(&cur)), yes)?;
    let d = ctx.c.put(P, &json!({"default": m}))?;
    ctx.emit(&d, || format!("新入口默认模式：{}", label(&d)));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wording() {
        assert_eq!(changes((7, 31), (3, 31)), "原始记录 7 → 3 天，汇总 31 天");
        assert_eq!(changes((7, 31), (7, 60)), "原始记录 7 天，汇总 31 → 60 天");
        let t = text(&json!({"raw_days": 1, "hourly_days": 3, "profile_days": 7}));
        assert!(t.contains("1 天") && t.contains("3 天") && t.ends_with("留 7 天"), "{t}");
        assert!(!text(&json!({"raw_days": 7, "hourly_days": 31, "profile_days": 31})).contains("学习期"));
    }
}
