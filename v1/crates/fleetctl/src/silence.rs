//! 静默与维护窗口：ls / add / rm。窗口可以预约（`--start` 给开始时间）；维护前先开维护窗口，免得告警刷屏。
//! 中控只列还没结束的（进行中 + 已预约），按开始时间排，进行中的自然在前。

use crate::cmds::{st, Ctx};
use crate::ops::confirm;
use crate::resolve::Want;
use crate::meta;
use crate::table;
use anyhow::{anyhow, bail};
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::{Duration, OffsetDateTime, PrimitiveDateTime};

const HEAD: [&str; 7] = ["对象", "规则", "时间", "状态", "维护", "备注", "id"];

fn rfc(v: &Value) -> Option<OffsetDateTime> { v.as_str().and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok()) }

/// `ls` / `add` / `rm` 的 `--json`：API 的字段另加 `state`：`active`（进行中）| `scheduled`（已预约，还没开始）
fn item(s: &Value, now: OffsetDateTime) -> Value {
    let started = rfc(&s["starts_at"]).is_some_and(|t| t <= now);
    json!({
        "id": s["id"], "subject_id": s["subject_id"], "subject_name": s["subject_name"], "rule": s["rule"], "rule_name": s["rule_name"],
        "starts_at": s["starts_at"], "ends_at": s["ends_at"], "state": if started { "active" } else { "scheduled" },
        "is_maintenance": s["is_maintenance"], "comment": s["comment"],
    })
}

/// 时长（还剩多久 / 多久后）：按分钟向上取整，`1 天 3 小时` / `3 小时 20 分钟` / `15 分钟`（同网页）
fn span_cn(d: Duration) -> String {
    let m = ((d.whole_seconds() + 59) / 60).max(1);
    match (m / 1440, m % 1440 / 60, m % 60) {
        (0, 0, m) => format!("{m} 分钟"),
        (0, h, 0) => format!("{h} 小时"),
        (0, h, m) => format!("{h} 小时 {m} 分钟"),
        (d, 0, _) => format!("{d} 天"),
        (d, h, _) => format!("{d} 天 {h} 小时"),
    }
}

/// 组织时区（中控 org_timezone，默认北京时间；同其它命令的表格），到分钟
fn bj(t: OffsetDateTime) -> String { t.to_offset(meta::offset()).format(format_description!("[year]-[month]-[day] [hour]:[minute]")).unwrap_or_default() }

/// 「2026-10-23 02:00 → 06:00」：结束与开始同一天时只写时分
fn window(a: OffsetDateTime, b: OffsetDateTime) -> String {
    let (sa, sb) = (bj(a), bj(b));
    let end = match (sa.split_once(' '), sb.split_once(' ')) {
        (Some((da, _)), Some((db, hm))) if da == db => hm,
        _ => &sb,
    };
    format!("{sa} → {end}")
}

/// `item` 的一行。id 只显示前 13 位：中控的 id 是 UUIDv7，前 8 位只是时间（约 65 秒一变），一起建的几条前 8 位相同；到第 13 位是毫秒
fn row(i: &Value, now: OffsetDateTime) -> Vec<String> {
    let (when, state) = match (rfc(&i["starts_at"]), rfc(&i["ends_at"])) {
        (Some(a), Some(b)) if a <= now => (window(a, b), format!("进行中，还剩 {}", span_cn(b - now))),
        (Some(a), Some(b)) => (window(a, b), format!("已预约，{}后开始", span_cn(a - now))),
        _ => ("-".into(), "-".into()),
    };
    vec![
        i["subject_name"].as_str().unwrap_or("全部机器").into(),
        i["rule_name"].as_str().or(i["rule"].as_str()).unwrap_or("全部规则").into(),
        when,
        state,
        if i["is_maintenance"] == true { "是" } else { "否" }.into(),
        i["comment"].as_str().unwrap_or("").into(),
        st(i, "/id").chars().take(13).collect(),
    ]
}

fn list(ctx: &Ctx) -> anyhow::Result<Vec<Value>> { Ok(ctx.c.get("/silences")?.as_array().cloned().unwrap_or_default()) }

pub fn ls(ctx: &Ctx) -> anyhow::Result<u8> {
    let now = OffsetDateTime::now_utc();
    let items = Value::Array(list(ctx)?.iter().map(|s| item(s, now)).collect());
    ctx.emit(&items, || {
        let rows: Vec<Vec<String>> = items.as_array().map(Vec::as_slice).unwrap_or_default().iter().map(|i| row(i, now)).collect();
        if rows.is_empty() { "没有进行中或已预约的静默".into() } else { format!("{}\n共 {} 条", table::render(&HEAD, &rows), rows.len()) }
    });
    Ok(0)
}

/// `--for`：`30m` / `4h` / `2d`，可组合（`1h30m`），最长 30 天（同中控）
fn parse_for(s: &str) -> anyhow::Result<Duration> {
    let bad = || anyhow!("--for 写成 30m / 4h / 2d（分钟 / 小时 / 天，可组合，如 1h30m）");
    let (mut total, mut n) = (Duration::ZERO, String::new());
    for c in s.trim().chars() {
        match c {
            '0'..='9' => n.push(c),
            'm' | 'h' | 'd' if !n.is_empty() => {
                let v = i64::from(n.parse::<u32>().map_err(|_| bad())?);
                n.clear();
                total += match c {
                    'm' => Duration::minutes(v),
                    'h' => Duration::hours(v),
                    _ => Duration::days(v),
                };
            }
            _ => return Err(bad()),
        }
    }
    if !n.is_empty() || total.is_zero() {
        return Err(bad());
    }
    if total > Duration::days(30) {
        bail!("--for 最长 30 天；更久的请直接把机器退役");
    }
    Ok(total)
}

/// `--start`：组织时区（默认北京时间）的 `2026-10-23 02:00`（同表格里的时间；日期与时间之间也可以写 `T`，可带秒），或带时区的 RFC 3339
/// （`2026-10-23T02:00:00+09:00`）。不按本机时区：fleetctl 常跑在 UTC 的服务器上，同一句命令换台机器不该换个时刻
fn parse_start(s: &str) -> anyhow::Result<OffsetDateTime> {
    let s = s.trim();
    if let Ok(t) = OffsetDateTime::parse(s, &Rfc3339) {
        return Ok(t);
    }
    let fmts = [
        format_description!("[year]-[month]-[day] [hour]:[minute]"),
        format_description!("[year]-[month]-[day]T[hour]:[minute]"),
        format_description!("[year]-[month]-[day] [hour]:[minute]:[second]"),
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]"),
    ];
    let t = fmts
        .iter()
        .find_map(|f| PrimitiveDateTime::parse(s, f).ok())
        .ok_or_else(|| anyhow!("--start 写成 2026-10-23 02:00（{}），或带时区的 2026-10-23T02:00:00+09:00", meta::tz_label()))?;
    Ok(t.assume_offset(meta::offset()))
}

pub struct AddOpts<'a> {
    pub machine: Option<&'a str>,
    pub all: bool,
    pub rule: Option<&'a str>,
    pub start: Option<&'a str>,
    pub dur: &'a str,
    pub maintenance: bool,
    pub comment: Option<&'a str>,
    pub yes: bool,
}

pub fn add(ctx: &Ctx, o: AddOpts<'_>) -> anyhow::Result<u8> {
    let dur = parse_for(o.dur)?;
    // 同中控：API 密钥（fleetctl 用的就是）建的全部机器静默 / 维护最长 24 小时
    if o.all && dur > Duration::hours(24) {
        bail!("--all（全部机器）最长 24 小时：API 密钥不能把整个监控关得更久；更长的请登录网页用账号建");
    }
    if o.maintenance && o.rule.is_some() {
        bail!("维护窗口针对整台机器，不能只选一条规则：只想静默这条告警就去掉 --maintenance");
    }
    let now = OffsetDateTime::now_utc();
    let start = match o.start {
        Some(s) => parse_start(s)?,
        None => now,
    };
    // 当前这一分钟还算数（同网页）；更早的多半是时区写错了，不悄悄变成「从现在开始」
    if start + Duration::minutes(1) <= now {
        bail!("--start 的时间（{} {}）已经过去了；要马上开始就不写 --start", meta::tz_label(), bj(start));
    }
    let end = start + dur;
    let subject = match (o.machine, o.all) {
        (Some(_), true) => bail!("<机器> 和 --all 只能给一个"),
        (None, false) => bail!("给机器（id / 显示名 / 线路机名 / IP），或用 --all 静默全部机器"),
        (Some(q), false) => Some(ctx.find(q, Want::Live)?),
        (None, true) => None,
    };
    if o.all {
        let act = if o.maintenance { "给全部机器开维护窗口" } else { "静默全部机器" };
        let alerts = o.rule.map(|r| format!("「{r}」这条告警")).unwrap_or_else(|| "告警".into());
        confirm(&format!("确认{act}（{}）？这段时间里所有机器的{alerts}都不会发出", window(start, end)), o.yes)?;
    }
    let body = json!({
        "subject_id": subject.as_ref().map(|m| m["id"].clone()), "rule": o.rule,
        "starts_at": start.format(&Rfc3339)?, "ends_at": end.format(&Rfc3339)?,
        "is_maintenance": o.maintenance, "comment": o.comment.unwrap_or(""),
    });
    let id = st(&ctx.c.post("/silences", &body)?, "/id");
    // 回读：对象名、规则中文名以中控为准，输出与 ls 同形
    let s = list(ctx)?.into_iter().find(|s| s["id"] == id.as_str()).ok_or_else(|| anyhow!("刚建的静默 {id} 不在列表里"))?;
    let now = OffsetDateTime::now_utc();
    let it = item(&s, now);
    ctx.emit(&it, || {
        let r = row(&it, now);
        let undo = if it["state"] == "scheduled" { "取消预约" } else { "提前结束" };
        format!("已新建{}：\n{}\n{undo}：fleetctl silence rm {}", what(&it), table::render(&HEAD, std::slice::from_ref(&r)), r[6])
    });
    Ok(0)
}

fn what(i: &Value) -> &'static str { if i["is_maintenance"] == true { "维护窗口" } else { "静默" } }

/// `rm <id 前缀>`：只在进行中和已预约的里找，前缀要唯一
pub fn rm(ctx: &Ctx, q: &str) -> anyhow::Result<u8> {
    let q = q.trim().to_lowercase();
    if q.is_empty() {
        bail!("给要删的静默的 id 前缀（fleetctl silence ls 的 id 列）");
    }
    let all = list(ctx)?;
    let hits: Vec<&Value> = all.iter().filter(|s| st(s, "/id").starts_with(&q)).collect();
    let now = OffsetDateTime::now_utc();
    let s = match hits.len() {
        1 => hits[0],
        0 => bail!("没有 id 以「{q}」开头的静默（只认进行中和已预约的，先 fleetctl silence ls 看）"),
        n => {
            let lines: Vec<String> = hits.iter().map(|s| format!("  {}  {}", st(s, "/id"), row(s, now)[..3].join("  "))).collect();
            bail!("「{q}」匹配到 {n} 条静默，请给更长的 id：\n{}", lines.join("\n"))
        }
    };
    ctx.c.delete(&format!("/silences/{}", st(s, "/id")))?;
    let it = item(s, now);
    ctx.emit(&it, || {
        let done = if it["state"] == "scheduled" { format!("已取消预约的{}（还没开始，不影响现在的告警）", what(&it)) } else { format!("已删除{}，从现在起照常告警和通知", what(&it)) };
        // 表里是删除前的那一行（状态列还写着「进行中」），说明一句，别读成告警已恢复
        format!("{done}。删除前的记录：\n{}", table::render(&HEAD, &[row(&it, now)]))
    });
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn duration_units_and_combinations() {
        assert_eq!(parse_for("30m").unwrap(), Duration::minutes(30));
        assert_eq!(parse_for("4h").unwrap(), Duration::hours(4));
        assert_eq!(parse_for(" 2d ").unwrap(), Duration::days(2));
        assert_eq!(parse_for("1h30m").unwrap(), Duration::minutes(90));
        assert_eq!(parse_for("1d12h").unwrap(), Duration::hours(36));
        assert_eq!(parse_for("30d").unwrap(), Duration::days(30));
        for bad in ["", "4", "h", "4x", "0h", "-1h", "4 h", "1.5h", "4H", "99999999999d", "30d1m", "3000000d"] {
            assert!(parse_for(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn start_is_beijing_time_unless_it_carries_an_offset() {
        crate::meta::test_init();
        for s in ["2026-10-23 02:00", "2026-10-23T02:00", " 2026-10-23 02:00:00 "] {
            assert_eq!(parse_start(s).unwrap(), datetime!(2026-10-22 18:00 UTC), "{s}");
        }
        assert_eq!(parse_start("2026-10-23 02:00:30").unwrap(), datetime!(2026-10-22 18:00:30 UTC));
        // 带时区的按它自己的时区
        assert_eq!(parse_start("2026-10-23T02:00:00+09:00").unwrap(), datetime!(2026-10-22 17:00 UTC));
        assert_eq!(parse_start("2026-10-22T18:00:00Z").unwrap(), datetime!(2026-10-22 18:00 UTC));
        for bad in ["2026/10/23 02:00", "02:00", "2026-10-23", "2026-13-01 00:00", "2026-10-23 2:00", "明天 2 点"] {
            assert!(parse_start(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn window_state_and_span_wording() {
        crate::meta::test_init();
        let now = datetime!(2026-10-22 06:00 UTC); // 北京时间 14:00
        assert_eq!(window(now, now + Duration::hours(4)), "2026-10-22 14:00 → 18:00");
        assert_eq!(window(now, now + Duration::hours(12)), "2026-10-22 14:00 → 2026-10-23 02:00");
        assert_eq!(
            [Duration::seconds(30), Duration::hours(4) - Duration::milliseconds(500), Duration::minutes(200), Duration::hours(26) + Duration::minutes(5), Duration::days(2)].map(span_cn),
            ["1 分钟", "4 小时", "3 小时 20 分钟", "1 天 2 小时", "2 天"]
        );
        let s = |a: OffsetDateTime, b: OffsetDateTime| json!({"id": "0192e3a1-7f2c-7abc-8def-0123456789ab", "subject_name": null, "rule": "agent_offline", "rule_name": "agent 离线",
            "starts_at": a.format(&Rfc3339).unwrap(), "ends_at": b.format(&Rfc3339).unwrap(), "is_maintenance": true, "comment": "换内核"});
        let cur = s(now - Duration::hours(1), now + Duration::minutes(200));
        assert_eq!(item(&cur, now)["state"], "active");
        assert_eq!(row(&cur, now), ["全部机器", "agent 离线", "2026-10-22 13:00 → 17:20", "进行中，还剩 3 小时 20 分钟", "是", "换内核", "0192e3a1-7f2c"]);
        let pre = s(now + Duration::hours(12), now + Duration::hours(16));
        assert_eq!(item(&pre, now)["state"], "scheduled");
        assert_eq!(row(&pre, now)[2..4], ["2026-10-23 02:00 → 06:00", "已预约，12 小时后开始"]);
    }
}
