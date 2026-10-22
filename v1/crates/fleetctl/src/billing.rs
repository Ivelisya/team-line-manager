//! 费用与续费：billing（订阅、流量额度、最近的续费记录）、renew（登记续费）、subscription set（改订阅）。
//! 默认值与说法同网页抽屉的「费用」页签：续费默认按订阅周期推一个周期、金额取订阅价格；只记账，不会向供应商付款。

use crate::cmds::{cycle_cn, due_cn, num, st, Ctx};
use crate::ops::confirm;
use crate::resolve::Want;
use crate::table::{self, gb, money};
use anyhow::{anyhow, bail};
use serde_json::{json, Map, Value};
use std::ops::RangeInclusive;
use time::macros::format_description;
use time::{Date, Month};

/// billing 列出的续费记录条数（新→旧）
const RECENT: usize = 10;
/// 流量上限是供应商的十进制 GB
const GB: f64 = 1e9;
/// `PUT /machines/{id}/subscription` 的字段：整份替换（另有 traffic_used_gb：没传 = 不改）
const SUB_KEYS: [&str; 9] = ["price_cents", "currency", "cycle", "due_date", "billing_tz", "auto_renew", "remind_days", "traffic_gb", "traffic_reset_day"];
/// 其中可以「单独设置」的：null = 跟随模板（价格、币种、周期、流量上限跟随套餐，账单时区、提前提醒、流量重置日跟随供应商）
const OVR: [&str; 7] = ["price_cents", "currency", "cycle", "billing_tz", "remind_days", "traffic_gb", "traffic_reset_day"];

/// 这一项单独设置了（订阅的 overrides 里有它）
fn overridden(s: &Value, k: &str) -> bool { s["overrides"].as_array().is_some_and(|a| a.iter().any(|x| x == k)) }

/// 跟随时的值（同中控 sub_template / subscription_eff）：重置日按供应商的方式，账单日 = 到期日几号、自然月 = 1 号
fn inherited(tpl: &Value, k: &str, due: &str) -> Value {
    match k {
        "traffic_reset_day" => json!(if tpl["traffic_reset"] == "billing" { date(due).map_or(1, |d| d.day()) } else { 1 }),
        _ => tpl[k].clone(),
    }
}

/// 要提交的订阅的生效值与 overrides：null 或和模板相同的按跟随（同中控 subscription_normalize；流量上限 0 与套餐的 null 都是不限）。
/// 改订阅的确认语按它逐项对比
fn effective(body: &Value, tpl: &Value) -> Value {
    let mut e = body.clone();
    let mut ov = vec![];
    for k in OVR {
        let inh = inherited(tpl, k, &st(body, "/due_date"));
        let same = body[k] == inh || (k == "traffic_gb" && body[k] == 0 && inh.is_null());
        if body[k].is_null() || same {
            e[k] = inh;
        } else {
            ov.push(k);
        }
    }
    e["overrides"] = json!(ov);
    e
}

/// 金额：`39.46` / `39` / `0.5`（最多两位小数，不带正负号）→ 分。接口收整数分，币种是订阅的
fn parse_money(s: &str) -> anyhow::Result<i32> {
    let s = s.trim();
    let (int, frac) = s.split_once('.').unwrap_or((s, "0"));
    let digits = |x: &str| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit());
    if !digits(int) || !digits(frac) || frac.len() > 2 {
        bail!("金额写成 39.46 这样的数（最多两位小数，不能为负）：「{s}」");
    }
    format!("{int}{frac:0<2}").parse().map_err(|_| anyhow!("金额「{s}」太大"))
}

/// `YYYY-MM-DD`
fn date(s: &str) -> Option<Date> { Date::parse(s.trim(), format_description!("[year]-[month]-[day]")).ok() }

/// 计费周期的月数（同网页 CYCLE_MONTHS；中控的计费周期只有这四种）
fn cycle_months(c: &str) -> Option<i64> {
    match c {
        "monthly" => Some(1),
        "quarterly" => Some(3),
        "semiannual" => Some(6),
        "yearly" => Some(12),
        _ => None,
    }
}

/// 加 n 个月，目标月没有那一天时落到月末（1-31 加一个月是 2-28，闰年 2-29），同网页 addMonths。
/// 从原日期一次加够，不逐月累加：1-31 加两个月是 3-31，不是 3-28
fn add_months(d: Date, n: i64) -> anyhow::Result<Date> {
    let m = i64::from(d.year()) * 12 + i64::from(u8::from(d.month())) - 1 + n;
    let month = Month::try_from((m.rem_euclid(12) + 1) as u8)?;
    let year = i32::try_from(m.div_euclid(12)).map_err(|_| anyhow!("新到期日超出范围"))?;
    Date::from_calendar_date(year, month, d.day().min(month.length(year))).map_err(|_| anyhow!("新到期日超出范围"))
}

/// 新到期日：给了 `--to` 用它；否则从当前到期日按订阅周期推 `cycles` 个周期（网页「登记续费」默认推一个周期）
fn renew_to(due: Date, cycle: &str, to: Option<&str>, cycles: u32) -> anyhow::Result<Date> {
    let new = match to {
        Some(t) => date(t).ok_or_else(|| anyhow!("--to 写成 YYYY-MM-DD，如 2026-11-25"))?,
        None if cycles == 0 => bail!("--cycles 至少是 1"),
        None => {
            let months = cycle_months(cycle).ok_or_else(|| anyhow!("不认识订阅的计费周期「{cycle}」：用 --to 直接给新到期日"))?;
            add_months(due, months * i64::from(cycles))?
        }
    };
    if new <= due {
        bail!("新到期日 {new} 要晚于当前到期日 {due}");
    }
    Ok(new)
}

/// 费用：`39.46 USD / 月`（同网页）
fn fee(s: &Value) -> String { format!("{} / {}", money(s["price_cents"].as_i64().unwrap_or(0), &st(s, "/currency")), cycle_cn(&st(s, "/cycle"))) }

/// 生效的流量上限：0 / null = 不限
fn traffic_gb_cn(v: &Value) -> String {
    match v.as_i64() {
        None | Some(0) => "不限".into(),
        Some(g) => format!("{g} GB"),
    }
}

/// 来源：单独设置了其中一项 →「单独设置」；否则跟随套餐 / 供应商（机器没有就是默认值）
fn src(s: &Value, m: &Value, keys: &[&str]) -> &'static str {
    let plan = keys.iter().any(|k| matches!(*k, "price_cents" | "currency" | "cycle" | "traffic_gb"));
    match () {
        _ if keys.iter().any(|k| overridden(s, k)) => "（单独设置）",
        _ if plan && !m["plan"].is_null() => "（跟随套餐）",
        _ if !plan && !m["provider"].is_null() => "（跟随供应商）",
        _ => "（默认）",
    }
}

/// 额度怎么量（同网页）：有 agent 的机器按网卡、照供应商的规则；家宽代理与同机 IP 节点是业务流量
fn rule_cn(v: &Value) -> &'static str {
    match v.as_str() {
        Some("min") => "网卡入出取小",
        Some("max") => "网卡入出取大",
        Some("sum") => "网卡入出相加",
        Some("out") => "只算网卡出站",
        _ => "业务流量",
    }
}

/// 流量重置日（生效值）：跟随供应商时注明按账单日还是自然月
fn reset_day_cn(s: &Value, m: &Value) -> String {
    let d = s["traffic_reset_day"].as_i64().unwrap_or(1);
    match m.pointer("/sub_template/traffic_reset").and_then(Value::as_str) {
        _ if overridden(s, "traffic_reset_day") => format!("每月 {d} 号（单独设置）"),
        Some("billing") if !m["provider"].is_null() => format!("每月 {d} 号（跟随供应商：按账单日）"),
        _ if !m["provider"].is_null() => format!("每月 {d} 号（跟随供应商：按自然月）"),
        _ => format!("每月 {d} 号（默认）"),
    }
}

/// 订阅各项的文字（生效值 + 来源）：billing 的表、改订阅的确认语逐项对比都用它。`m` 是机器（看有没有套餐、供应商）
fn sub_fields(s: &Value, m: &Value) -> [(&'static str, String); 7] {
    [
        ("费用", format!("{}{}", fee(s), src(s, m, &["price_cents", "currency", "cycle"]))),
        ("到期日", st(s, "/due_date")),
        ("自动续费", if s["auto_renew"] == true { "是" } else { "否" }.into()),
        ("提前提醒", format!("{} 天{}", st(s, "/remind_days"), src(s, m, &["remind_days"]))),
        ("账单时区", format!("{}{}", st(s, "/billing_tz"), src(s, m, &["billing_tz"]))),
        ("流量上限", format!("{}{}", traffic_gb_cn(&s["traffic_gb"]), src(s, m, &["traffic_gb"]))),
        ("流量重置日", reset_day_cn(s, m)),
    ]
}

/// 生效的流量上限（GB）：中控算好的 quota.limit_gb（同流量额度告警与网页）；null = 没有上限
fn traffic_limit(m: &Value) -> Option<i64> { m.pointer("/quota/limit_gb").and_then(Value::as_i64).filter(|g| *g > 0) }

/// billing 的流量上限：生效值与来源（quota.limit_source）；同机 IP 节点没设上限是与宿主机共用（宿主机的已用含它的流量）
fn limit_cn(m: &Value, limit: Option<i64>) -> String {
    match (limit, m.pointer("/quota/limit_source").and_then(Value::as_str)) {
        (Some(g), Some("plan")) => format!("{g} GB（跟随套餐「{}」）", st(m, "/plan/name")),
        (Some(g), _) => format!("{g} GB（单独设置）"),
        (None, Some("host")) => format!("与宿主机「{}」共用", st(m, "/host_machine/name")),
        (None, _) => "不限".into(),
    }
}

/// `901.6 GB / 2000 GB（45%），2026-10-13 起，网卡入出取小；已按面板校准（2026-10-22 15:30:00）`
fn used_cn(m: &Value, limit: Option<i64>, pct: Option<i64>, incl: &str) -> String {
    let lim = limit.map(|g| format!(" / {g} GB")).unwrap_or_default();
    let p = pct.map(|p| format!("（{p}%）")).unwrap_or_default();
    let cal = m.pointer("/quota/base_at").and_then(Value::as_str).map(|t| format!("；已按面板校准（{}）", table::ts(t))).unwrap_or_default();
    let used = gb(num(m, "/quota/used").unwrap_or(0.0));
    format!("{used}{lim}{p}，{} 起，{}{incl}{cal}", st(m, "/quota/period_start"), rule_cn(&m["quota"]["rule"]))
}

fn billing_text(name: &str, m: &Value, limit: Option<i64>, pct: Option<i64>, renewals: &[Value], total: usize) -> String {
    let s = &m["subscription"];
    let mut rows = vec![("机器", name.to_string())];
    if !s.is_null() {
        let [fee, (due, d), (auto, a), remind, tz, _, reset] = sub_fields(s, m);
        let a = if s["auto_renew"] == true { "是（只影响提醒：到期前不提醒，过期才告警）".into() } else { a };
        rows.extend([fee, (due, format!("{d}（{}）", due_cn(s["due_days"].as_i64().unwrap_or(0)))), (auto, a), remind, tz, ("流量上限", limit_cn(m, limit)), reset]);
    } else {
        rows.push(("流量上限", limit_cn(m, limit)));
    }
    let incl = if m["ip_nodes"].as_array().is_some_and(|a| !a.is_empty()) { "，含同机 IP 节点" } else { "" };
    rows.push(("本周期已用", used_cn(m, limit, pct, incl)));
    let out = table::kv(&rows);
    if s.is_null() {
        let price = if m["plan"].is_null() { " --price <金额>" } else { "" };
        let set = format!("fleetctl subscription set {name} --due <YYYY-MM-DD>{price}");
        return if m["host_machine"].is_null() {
            format!("{out}\n\n还没有订阅（费用、到期日），不参与到期提醒与费用统计。填写：{set}")
        } else {
            format!("{out}\n\n同机 IP 节点默认不单独记费用，费用记在宿主机「{}」；供应商对附加 IP 另收费时才单独填：{set}", st(m, "/host_machine/name"))
        };
    }
    if renewals.is_empty() {
        return out + "\n\n续费记录：暂无";
    }
    let cur = st(s, "/currency");
    let rows: Vec<Vec<String>> = renewals
        .iter()
        .map(|r| {
            vec![
                table::ts(&st(r, "/created_at")),
                format!("{} → {}", st(r, "/old_due"), st(r, "/new_due")),
                r["amount_cents"].as_i64().map_or("-".into(), |c| money(c, r["currency"].as_str().unwrap_or(&cur))),
                st(r, "/actor"),
                st(r, "/note"),
            ]
        })
        .collect();
    let head = if total > renewals.len() { format!("最近 {} 条，共 {total} 条", renewals.len()) } else { format!("共 {total} 条") };
    format!("{out}\n\n续费记录（{head}）\n{}", table::render(&["登记时间", "到期日", "金额", "操作者", "备注"], &rows))
}

/// `billing <机器>`：订阅 + 流量额度 + 最近的续费记录。`--json`：`{id, name, host_machine, subscription, sub_template, traffic_limit_gb, quota, quota_pct, renewals, renewal_count}`
pub fn billing(ctx: &Ctx, machine: &str) -> anyhow::Result<u8> {
    // 已退役的机器也能看当时的订阅与续费记录
    let (id, name) = ctx.find_id(machine, Want::Any)?;
    let m = ctx.c.get(&format!("/machines/{id}"))?;
    let rs = ctx.c.get(&format!("/machines/{id}/renewals"))?;
    let all = rs.as_array().map(Vec::as_slice).unwrap_or_default();
    let recent = &all[..all.len().min(RECENT)];
    let limit = traffic_limit(&m);
    // 向下取整（同流量额度告警）：还没用满时不会写成 100%
    let pct = limit.map(|g| (num(&m, "/quota/used").unwrap_or(0.0) * 100.0 / (g as f64 * GB)).floor() as i64);
    let d = json!({
        "id": id, "name": name, "host_machine": m["host_machine"], "subscription": m["subscription"], "sub_template": m["sub_template"],
        "traffic_limit_gb": limit, "quota": m["quota"], "quota_pct": pct, "renewals": recent, "renewal_count": all.len(),
    });
    ctx.emit(&d, || billing_text(&name, &m, limit, pct, recent, all.len()));
    Ok(0)
}

pub struct RenewOpts<'a> {
    pub machine: &'a str,
    pub to: Option<&'a str>,
    pub cycles: u32,
    pub amount: Option<&'a str>,
    pub note: Option<&'a str>,
    pub yes: bool,
}

/// `renew <机器>`：登记一次续费（到期日顺延、续费类告警下一轮评估恢复）。`--json` 输出这条续费记录（同 billing 的 renewals[]）
pub fn renew(ctx: &Ctx, o: RenewOpts<'_>) -> anyhow::Result<u8> {
    let (id, name) = ctx.find_id(o.machine, Want::Live)?;
    let m = ctx.c.get(&format!("/machines/{id}"))?;
    let s = &m["subscription"];
    if s.is_null() {
        bail!("{name} 还没有订阅（费用、到期日），不能登记续费：先用 fleetctl subscription set {name} --price <金额> --due <YYYY-MM-DD> 填写");
    }
    let due = date(&st(s, "/due_date")).ok_or_else(|| anyhow!("中控返回的到期日不是 YYYY-MM-DD：{}", st(s, "/due_date")))?;
    let new = renew_to(due, &st(s, "/cycle"), o.to, o.cycles)?;
    let cur = st(s, "/currency");
    let amount = match o.amount {
        Some(a) => i64::from(parse_money(a)?),
        // 默认是订阅价格（同网页）；--cycles 推几个周期就是几期的钱
        None => s["price_cents"].as_i64().unwrap_or(0) * i64::from(if o.to.is_some() { 1 } else { o.cycles }),
    };
    let note = o.note.map(str::trim).filter(|n| !n.is_empty());
    let note_cn = note.map(|n| format!("，备注「{n}」")).unwrap_or_default();
    confirm(&format!("确认给 {name} 登记续费：到期 {due} → {new}，实付 {}{note_cn}？只记录「已续费」、顺延到期日，不会向供应商付款", money(amount, &cur)), o.yes)?;
    let rid = st(&ctx.c.post(&format!("/machines/{id}/renewals"), &json!({"new_due": new.to_string(), "amount_cents": amount, "note": note}))?, "/id");
    // 回读：到期日以中控为准（登记时中控锁住订阅、取当时的到期日作旧值）
    let all = ctx.c.get(&format!("/machines/{id}/renewals"))?;
    let r = all.as_array().and_then(|a| a.iter().find(|r| r["id"] == rid.as_str())).ok_or_else(|| anyhow!("刚登记的续费 {rid} 不在续费记录里"))?;
    // due_days = 到期日 − 账单时区的今天，新的到期日顺延了几天就加几天
    let days = s["due_days"].as_i64().unwrap_or(0) + (new - due).whole_days();
    ctx.emit(r, || {
        let paid = r["amount_cents"].as_i64().map_or("-".into(), |c| money(c, &cur));
        format!("已登记续费：{name} 到期 {} → {}（{}），实付 {paid}{note_cn}", st(r, "/old_due"), st(r, "/new_due"), due_cn(days))
    });
    Ok(0)
}

#[derive(Default)]
pub struct SubSetOpts<'a> {
    pub machine: &'a str,
    pub price: Option<&'a str>,
    pub currency: Option<&'a str>,
    pub cycle: Option<&'a str>,
    pub due: Option<&'a str>,
    pub auto_renew: Option<bool>,
    pub remind_days: Option<&'a str>,
    pub tz: Option<&'a str>,
    pub traffic_gb: Option<&'a str>,
    pub reset_day: Option<&'a str>,
    pub used: Option<&'a str>,
    pub yes: bool,
}

/// `none`（不分大小写）= null：单独设置的字段改回跟随
fn is_none(s: &str) -> bool { s.trim().eq_ignore_ascii_case("none") }

/// `N` 或 `none`（= null）；N 不在 `range` 里报 `bad`
fn int_or_none(s: &str, range: RangeInclusive<i32>, bad: &str) -> anyhow::Result<Value> {
    if is_none(s) {
        return Ok(Value::Null);
    }
    s.trim().parse::<i32>().ok().filter(|n| range.contains(n)).map(|n| json!(n)).ok_or_else(|| anyhow!("{bad}"))
}

/// 命令行给了的字段，换成接口的写法，提交前先校验。可单独设置的字段给 none = null（改回跟随套餐 / 供应商）。
/// 上架（provision）的订阅也用它
pub(crate) fn sub_patch(o: &SubSetOpts<'_>) -> anyhow::Result<Map<String, Value>> {
    let mut p = Map::new();
    if let Some(v) = o.price {
        p.insert("price_cents".into(), if is_none(v) { Value::Null } else { json!(parse_money(v)?) });
    }
    if let Some(c) = o.currency {
        let c = c.trim().to_uppercase();
        let v = match c.as_str() {
            "NONE" => Value::Null,
            _ if c.len() == 3 && c.bytes().all(|b| b.is_ascii_alphabetic()) => json!(c),
            _ => bail!("--currency 写 3 个字母的币种代码，如 USD / CNY；none = 跟随套餐"),
        };
        p.insert("currency".into(), v);
    }
    if let Some(c) = o.cycle {
        let v = match c.trim() {
            c if is_none(c) => Value::Null,
            c if cycle_months(c).is_some() => json!(c),
            _ => bail!("--cycle 只能是 monthly / quarterly / semiannual / yearly（月 / 季 / 半年 / 年）；none = 跟随套餐"),
        };
        p.insert("cycle".into(), v);
    }
    if let Some(d) = o.due {
        p.insert("due_date".into(), json!(date(d).ok_or_else(|| anyhow!("--due 写成 YYYY-MM-DD，如 2026-11-25"))?.to_string()));
    }
    if let Some(b) = o.auto_renew {
        p.insert("auto_renew".into(), json!(b));
    }
    if let Some(v) = o.remind_days {
        p.insert("remind_days".into(), int_or_none(v, 0..=60, "--remind-days 写 0–60（到期前几天提醒），或 none（跟随供应商）")?);
    }
    if let Some(z) = o.tz {
        p.insert("billing_tz".into(), if is_none(z) { Value::Null } else { json!(z.trim()) });
    }
    if let Some(v) = o.traffic_gb {
        p.insert("traffic_gb".into(), int_or_none(v, 0..=i32::MAX, "--traffic-gb 写流量上限的 GB 数（0 = 不限），或 none（跟随套餐）")?);
    }
    if let Some(v) = o.reset_day {
        p.insert("traffic_reset_day".into(), int_or_none(v, 1..=31, "--reset-day 写 1–31（每月几号），或 none（跟随供应商：按账单日或自然月）")?);
    }
    if let Some(v) = o.used {
        let g = if is_none(v) {
            Value::Null
        } else {
            v.trim().parse::<f64>().ok().filter(|g| g.is_finite() && *g >= 0.0).map(|g| json!(g)).ok_or_else(|| anyhow!("--used 写供应商面板上本周期已用的 GB 数（如 901.6），或 none（清掉校准）"))?
        };
        p.insert("traffic_used_gb".into(), g);
    }
    Ok(p)
}

/// PUT 是整份替换：以现有订阅为底、只改给了的字段；单独设置过的照旧提交，跟随的提交 null（不把套餐的值抄进订阅）。
/// 没有订阅时新建，只要到期日（没有套餐的机器中控还要价格）
fn merge_sub(cur: &Value, patch: Map<String, Value>) -> anyhow::Result<Value> {
    let mut b = if cur.is_null() {
        if !patch.contains_key("due_date") {
            bail!("这台机器还没有订阅，新建要给 --due（价格、币种、周期、流量上限跟随套餐，时区、提醒、流量重置跟随供应商；没有套餐的机器还要给 --price）");
        }
        Value::Object(OVR.iter().map(|k| (k.to_string(), Value::Null)).collect())
    } else {
        Value::Object(
            SUB_KEYS.iter().map(|k| (k.to_string(), if OVR.contains(k) && !overridden(cur, k) { Value::Null } else { cur[*k].clone() })).collect(),
        )
    };
    for (k, v) in patch {
        b[k] = v;
    }
    Ok(b)
}

/// 改了哪些（`费用 39.46 USD / 月（跟随套餐） → 45.00 USD / 月（单独设置）`）；新建时列出全部。按面板校准不在现有订阅里，给了就列。
/// 新值按 `effective` 算生效值（和模板相同的按跟随，同中控）
fn sub_changes(m: &Value, new: &Value) -> Vec<String> {
    let cur = &m["subscription"];
    let mut eff = effective(new, &m["sub_template"]);
    if eff["auto_renew"].is_null() {
        eff["auto_renew"] = json!(false);
    }
    let mut v: Vec<String> = if cur.is_null() {
        sub_fields(&eff, m).into_iter().map(|(k, v)| format!("{k} {v}")).collect()
    } else {
        sub_fields(cur, m).into_iter().zip(sub_fields(&eff, m)).filter(|(a, b)| a.1 != b.1).map(|((k, a), (_, b))| format!("{k} {a} → {b}")).collect()
    };
    match new.get("traffic_used_gb") {
        Some(Value::Null) => v.push("清掉按面板的校准".into()),
        Some(g) => v.push(format!("按面板校准本周期已用为 {}（之后按网卡流量累加）", gb(g.as_f64().unwrap_or(0.0) * GB))),
        None => {}
    }
    v
}

/// `subscription set <机器>`：先读现有订阅、只改给了的字段再整份提交；可单独设置的字段给 none 改回跟随。
/// `--json` 输出改完的订阅（同 show 的 subscription，含 overrides）
pub fn subscription_set(ctx: &Ctx, o: SubSetOpts<'_>) -> anyhow::Result<u8> {
    let patch = sub_patch(&o)?;
    if patch.is_empty() {
        bail!("没有要改的内容：给 --price / --currency / --cycle / --due / --auto-renew / --remind-days / --tz / --traffic-gb / --reset-day / --used 之一");
    }
    let (id, name) = ctx.find_id(o.machine, Want::Live)?;
    let m = ctx.c.get(&format!("/machines/{id}"))?;
    let cur = &m["subscription"];
    let body = merge_sub(cur, patch)?;
    let changes = sub_changes(&m, &body).join("；");
    if changes.is_empty() {
        ctx.emit(cur, || format!("{name} 的订阅没有变化，未提交"));
        return Ok(0);
    }
    let act = if cur.is_null() { "设置" } else { "修改" };
    confirm(&format!("确认{act} {name} 的订阅：{changes}？只改中控的记录，到期提醒、流量额度告警与费用统计按新值算"), o.yes)?;
    ctx.c.put(&format!("/machines/{id}/subscription"), &body)?;
    // 回读：due_days 由中控按账单时区算
    let s = ctx.c.get(&format!("/machines/{id}"))?["subscription"].take();
    ctx.emit(&s, || format!("已{act} {name} 的订阅：{changes}\n到期日 {}（{}）", st(&s, "/due_date"), due_cn(s["due_days"].as_i64().unwrap_or(0))));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    #[test]
    fn money_in_yuan_to_cents() {
        assert_eq!(["39.46", "39", "0.5", " 12.30 ", "0"].map(|s| parse_money(s).unwrap()), [3946, 3900, 50, 1230, 0]);
        for bad in ["", "abc", "-1", "+5", "1.234", "39.", ".5", "1,000", "1e3", "1.2.3", "99999999999"] {
            assert!(parse_money(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn cycles_push_due_date_like_web() {
        let push = |due: Date, cycle: &str, cycles: u32| renew_to(due, cycle, None, cycles).unwrap();
        // 目标月没有那一天按月末（同网页 addMonths）
        assert_eq!(push(date!(2026-01-31), "monthly", 1), date!(2026-02-28));
        assert_eq!(push(date!(2028-01-31), "monthly", 1), date!(2028-02-29));
        assert_eq!(push(date!(2026-11-30), "quarterly", 1), date!(2027-02-28));
        assert_eq!(push(date!(2026-08-31), "semiannual", 1), date!(2027-02-28));
        assert_eq!(push(date!(2028-02-29), "yearly", 1), date!(2029-02-28));
        assert_eq!(push(date!(2026-12-15), "monthly", 1), date!(2027-01-15));
        // 几个周期从原日期一次加够，不逐月累加（1-31 推两个月是 3-31）
        assert_eq!(push(date!(2026-01-31), "monthly", 2), date!(2026-03-31));
        assert_eq!(push(date!(2026-10-25), "quarterly", 4), date!(2027-10-25));
        assert_eq!(renew_to(date!(2026-10-25), "monthly", Some(" 2027-01-01 "), 1).unwrap(), date!(2027-01-01));
        // 不晚于当前到期日、写法不对、0 个周期、推出日期范围（不 panic）、不认识的周期都报错
        for (to, cycles, cycle) in [(Some("2026-10-25"), 1, "monthly"), (Some("2026-10-01"), 1, "monthly"), (Some("2026/11/25"), 1, "monthly"), (None, 0, "monthly"), (None, u32::MAX, "yearly"), (None, 1, "weekly")] {
            assert!(renew_to(date!(2026-10-25), cycle, to, cycles).is_err(), "{to:?} {cycles} {cycle}");
        }
    }

    /// 机器：套餐 Mini（2000 GB、39.46 USD / 月），供应商按账单日重置、洛杉矶时区、提前 7 天；订阅单独设置了价格
    fn machine() -> Value {
        json!({
            "plan": {"name": "Mini", "traffic_gb": 2000}, "provider": {"name": "GOMAMI"}, "host_machine": null,
            "sub_template": {"price_cents": 3946, "currency": "USD", "cycle": "monthly", "billing_tz": "America/Los_Angeles", "remind_days": 7, "traffic_gb": 2000, "traffic_reset": "billing"},
            "subscription": {"price_cents": 4200, "currency": "USD", "cycle": "monthly", "due_date": "2026-10-25", "due_days": 3, "billing_tz": "America/Los_Angeles",
                "auto_renew": false, "remind_days": 7, "traffic_gb": 2000, "traffic_reset_day": 25, "overrides": ["price_cents"]},
        })
    }

    #[test]
    fn subscription_set_merges_given_fields() {
        let m = machine();
        let cur = &m["subscription"];
        // 跟随的提交 null（不把套餐的值抄进订阅），单独设置的照旧；due_days 是中控算的，不提交
        let o = SubSetOpts { traffic_gb: Some("500"), ..Default::default() };
        let b = merge_sub(cur, sub_patch(&o).unwrap()).unwrap();
        let want = json!({"price_cents": 4200, "currency": null, "cycle": null, "due_date": "2026-10-25", "billing_tz": null,
            "auto_renew": false, "remind_days": null, "traffic_gb": 500, "traffic_reset_day": null});
        assert_eq!(b, want);
        assert_eq!(sub_changes(&m, &b), ["流量上限 2000 GB（跟随套餐） → 500 GB（单独设置）"]);
        // none = 改回跟随
        let b = merge_sub(cur, sub_patch(&SubSetOpts { price: Some("none"), ..Default::default() }).unwrap()).unwrap();
        assert!(b["price_cents"].is_null());
        assert_eq!(sub_changes(&m, &b), ["费用 42.00 USD / 月（单独设置） → 39.46 USD / 月（跟随套餐）"]);
        // 填的和套餐一样：按跟随（同中控），没有变化
        let same = SubSetOpts { price: Some("42"), cycle: Some("monthly"), currency: Some("usd"), reset_day: Some("25"), ..Default::default() };
        assert!(sub_changes(&m, &merge_sub(cur, sub_patch(&same).unwrap()).unwrap()).is_empty());
        // 按账单日重置的跟随值随到期日变（小月按月末的事中控按周期算）
        let b = merge_sub(cur, sub_patch(&SubSetOpts { due: Some("2026-11-30"), ..Default::default() }).unwrap()).unwrap();
        assert_eq!(sub_changes(&m, &b), ["到期日 2026-10-25 → 2026-11-30", "流量重置日 每月 25 号（跟随供应商：按账单日） → 每月 30 号（跟随供应商：按账单日）"]);
        // 按面板校准：不在现有订阅里，给了就提交（GB，可带小数），none = 清掉
        let b = merge_sub(cur, sub_patch(&SubSetOpts { used: Some("901.6"), ..Default::default() }).unwrap()).unwrap();
        assert_eq!((b["traffic_used_gb"].as_f64(), b["price_cents"].as_i64()), (Some(901.6), Some(4200)));
        assert_eq!(sub_changes(&m, &b), ["按面板校准本周期已用为 901.6 GB（之后按网卡流量累加）"]);
        let b = merge_sub(cur, sub_patch(&SubSetOpts { used: Some("none"), ..Default::default() }).unwrap()).unwrap();
        assert_eq!(sub_changes(&m, &b), ["清掉按面板的校准"]);
        // 没有订阅：新建只要到期日，其余跟随（不写默认值）
        let mut fresh = machine();
        fresh["subscription"] = Value::Null;
        assert!(merge_sub(&Value::Null, sub_patch(&SubSetOpts { price: Some("39.46"), ..Default::default() }).unwrap()).is_err());
        let b = merge_sub(&Value::Null, sub_patch(&SubSetOpts { due: Some("2026-11-25"), auto_renew: Some(true), ..Default::default() }).unwrap()).unwrap();
        let want = json!({"price_cents": null, "currency": null, "cycle": null, "due_date": "2026-11-25", "billing_tz": null,
            "auto_renew": true, "remind_days": null, "traffic_gb": null, "traffic_reset_day": null});
        assert_eq!(b, want);
        assert_eq!(sub_changes(&fresh, &b)[..3], ["费用 39.46 USD / 月（跟随套餐）", "到期日 2026-11-25", "自动续费 是"]);
        // 写法不对的在提交前就报
        let bad = [
            SubSetOpts { cycle: Some("month"), ..Default::default() },
            SubSetOpts { remind_days: Some("61"), ..Default::default() },
            SubSetOpts { reset_day: Some("32"), ..Default::default() },
            SubSetOpts { reset_day: Some("0"), ..Default::default() },
            SubSetOpts { traffic_gb: Some("-1"), ..Default::default() },
            SubSetOpts { due: Some("2026-11-31"), ..Default::default() },
            SubSetOpts { currency: Some("US"), ..Default::default() },
            SubSetOpts { price: Some("1.999"), ..Default::default() },
            SubSetOpts { used: Some("-1"), ..Default::default() },
            SubSetOpts { used: Some("901.6GB"), ..Default::default() },
        ];
        for o in &bad {
            assert!(sub_patch(o).is_err());
        }
        let all_none = SubSetOpts { price: Some("none"), currency: Some("None"), cycle: Some("none"), tz: Some("none"), remind_days: Some("none"), traffic_gb: Some("0"), reset_day: Some("none"), ..Default::default() };
        let want = json!({"price_cents": null, "currency": null, "cycle": null, "billing_tz": null, "remind_days": null, "traffic_gb": 0, "traffic_reset_day": null});
        assert_eq!(sub_patch(&all_none).unwrap(), *want.as_object().unwrap());
    }

    #[test]
    fn billing_fields_show_source() {
        let m = machine();
        let f = sub_fields(&m["subscription"], &m);
        assert_eq!(f[0].1, "42.00 USD / 月（单独设置）");
        assert_eq!((f[4].1.as_str(), f[5].1.as_str(), f[6].1.as_str()), ("America/Los_Angeles（跟随供应商）", "2000 GB（跟随套餐）", "每月 25 号（跟随供应商：按账单日）"));
    }

    #[test]
    fn limit_and_billing_wording() {
        let q = |limit: Value, source: &str| json!({"plan": {"name": "Mini"}, "host_machine": {"id": "h", "name": "团队1-03"}, "quota": {"limit_gb": limit, "limit_source": source}});
        let m = q(json!(2000), "plan");
        assert_eq!((traffic_limit(&m), limit_cn(&m, traffic_limit(&m))), (Some(2000), "2000 GB（跟随套餐「Mini」）".into()));
        let m = q(json!(500), "subscription");
        assert_eq!(limit_cn(&m, traffic_limit(&m)), "500 GB（单独设置）");
        let m = q(Value::Null, "host");
        assert_eq!(limit_cn(&m, traffic_limit(&m)), "与宿主机「团队1-03」共用");
        let m = q(Value::Null, "none");
        assert_eq!(limit_cn(&m, traffic_limit(&m)), "不限");
        // 已用同供应商面板：十进制 GB；校准过的注明
        let q = json!({"ip_nodes": [], "quota": {"used": 901_600_000_000i64, "period_start": "2026-10-13", "rule": "min", "base": 869_700_000_000i64, "base_at": "2026-10-22T07:30:00Z"}});
        assert_eq!(used_cn(&q, Some(2000), Some(45), ""), "901.6 GB / 2000 GB（45%），2026-10-13 起，网卡入出取小；已按面板校准（2026-10-22 15:30:00）");
        let q = json!({"quota": {"used": 31_900_000, "period_start": "2026-10-01", "rule": null, "base_at": null}});
        assert_eq!(used_cn(&q, None, None, ""), "0.03 GB，2026-10-01 起，业务流量");
    }
}
