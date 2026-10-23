//! 费用与续费：billing（订阅、流量额度、最近的续费记录）、renew（登记续费）、subscription set（改订阅）。
//! 默认值与说法同网页抽屉的「费用」页签：续费默认按订阅周期推一个周期、金额取订阅价格；只记账，不会向供应商付款。

use crate::client::ApiError;
use crate::cmds::{cycle_cn, due_at, due_cn, num, st, Ctx};
use crate::meta;
use crate::ops::confirm;
use crate::resolve::Want;
use crate::table::{self, gb, money, money_at, traffic};
use anyhow::{anyhow, bail};
use serde_json::{json, Map, Value};
use time::macros::format_description;
use time::Date;

/// billing 列出的续费记录条数（新→旧）
const RECENT: usize = 10;
/// 流量上限是供应商的十进制 GB
const GB: f64 = 1e9;
/// `PUT /machines/{id}/subscription` 的字段：整份替换
const SUB_KEYS: [&str; 9] = ["price_cents", "currency", "cycle", "due_date", "billing_tz", "auto_renew", "remind_days", "traffic_gb", "traffic_reset_day"];
/// 其中可以「单独设置」的：null = 跟随模板（价格、币种、周期、流量上限跟随套餐，账单时区、提前提醒、流量重置日跟随供应商）
const OVR: [&str; 7] = ["price_cents", "currency", "cycle", "billing_tz", "remind_days", "traffic_gb", "traffic_reset_day"];

/// 这一项单独设置了（订阅的 overrides 里有它）
fn overridden(s: &Value, k: &str) -> bool { s["overrides"].as_array().is_some_and(|a| a.iter().any(|x| x == k)) }

/// 金额：`39.46` / `39` / `0.5`（最多两位小数，不带正负号）→ 分。接口收整数分，币种是订阅的
pub(crate) fn parse_money(s: &str) -> anyhow::Result<i32> {
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

/// 登记续费的请求体：给了 `--to` 交新到期日；否则交周期数，由中控按订阅周期从当前到期日推（小月按月末，同网页；周期数的范围中控校验）
fn renew_body(to: Option<&str>, cycles: u32, amount: i64, note: Option<&str>) -> anyhow::Result<Value> {
    let mut b = json!({"amount_cents": amount, "note": note});
    match to {
        Some(t) => b["new_due"] = json!(date(t).ok_or_else(|| anyhow!("--to 写成 YYYY-MM-DD，如 2026-11-25"))?.to_string()),
        None => b["cycles"] = json!(cycles),
    }
    Ok(b)
}

/// 费用：`39.46 USD / 月`（同网页）
fn fee(s: &Value) -> String { format!("{} / {}", money_at(s, "/price_cents", &st(s, "/currency")), cycle_cn(&st(s, "/cycle"))) }

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

/// 额度怎么量（中控 /meta 的 traffic_rules）：有 agent 的机器按网卡、照供应商的规则；家宽代理与同机 IP 节点没有自己的网卡，
/// 按业务流量下载加上传（不跟供应商的规则）
fn rule_cn(v: &Value) -> &str { v.as_str().map_or("按业务流量，下载加上传", |r| meta::label("traffic_rules", r)) }

/// 流量重置日（生效值）：跟随供应商时注明按账单日还是自然月；供应商按滚动 30 天的没有重置日
fn reset_day_cn(s: &Value, m: &Value) -> String {
    let d = s["traffic_reset_day"].as_i64().unwrap_or(1);
    match m.pointer("/sub_template/traffic_reset").and_then(Value::as_str) {
        _ if overridden(s, "traffic_reset_day") => format!("每月 {d} 号（单独设置）"),
        Some("rolling30") => "不按月重置：滚动 30 天（跟随供应商）".into(),
        Some(r) if !m["provider"].is_null() => format!("每月 {d} 号（跟随供应商：按{}）", meta::label("traffic_resets", r)),
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

/// 时间戳的月-日（组织时区）：「10-22 起」（总览的本月业务流量，同网页）
pub(crate) fn md_of(t: &str) -> String { table::day(t).chars().skip(5).collect() }

/// 按哪几块网卡计（agent 报的；旧 agent 没报为空）：`，按 ens3 计`。按业务流量算的不写（billing、show 共用）
pub(crate) fn nic_cn(m: &Value) -> String {
    let names: Vec<&str> = m.pointer("/agent/nic_ifaces").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    if m["quota"]["rule"].is_null() || names.is_empty() { String::new() } else { format!("，按 {} 计", names.join(" + ")) }
}

/// 有额度时的已用：`1035.3 GB / 2000 GB（51%），2026-10-13 → 2026-11-13，按 ens3 网卡，取下行和上行中较小的；接入后实测 71.2 GB，加上接入前的面板读数 964.1 GB（2026-11-13 重置后只算实测）`。
/// 周期中途上架、没有接入前读数的写「从 2026-10-22 开始统计，之前的没算进来」；滚动 30 天写起点
fn used_cn(m: &Value, limit: Option<i64>, pct: Option<i64>, incl: &str) -> String {
    let q = &m["quota"];
    let lim = limit.map(|g| format!(" / {g} GB")).unwrap_or_default();
    let p = pct.map(|p| format!("（{p}%）")).unwrap_or_default();
    let span = match q["period_end"].as_str() {
        Some(e) => format!("{} → {e}", st(q, "/period_start")),
        None => format!("{} 起", st(q, "/period_start")),
    };
    let note = match (num(q, "/base"), q["counted_from"].as_str()) {
        // used = 接入前读数 + 接入后网卡实测：拆开写，免得和只统计接入后的本月流量对不上
        (Some(b), _) => format!(
            "；接入后实测 {}，加上接入前的面板读数 {}{}",
            gb(num(q, "/used").unwrap_or(0.0) - b),
            gb(b),
            q["period_end"].as_str().map(|e| format!("（{e} 重置后只算实测）")).unwrap_or_default()
        ),
        (None, Some(t)) => format!("；从 {} 开始统计，之前的没算进来", table::day(t)),
        _ => String::new(),
    };
    // 有网卡名时先说按哪块网卡，再说怎么取（同网页）：「按 ens3 网卡，取下行和上行中较小的」
    let nics: Vec<&str> = m.pointer("/agent/nic_ifaces").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let basis = if q["rule"].is_null() || nics.is_empty() { rule_cn(&q["rule"]).to_string() } else { format!("按 {} 网卡，{}", nics.join("、"), rule_cn(&q["rule"])) };
    format!("{}{lim}{p}，{span}，{basis}{incl}{note}", gb(num(q, "/used").unwrap_or(0.0)))
}

/// 本月流量（组织时区，十进制自动单位，同网页）：有 agent 的按网卡 `网卡 ↓ 1.1 TB / ↑ 108.0 GB`，家宽代理、同机 IP 节点按业务（下载 / 上传，含已移除入口）；
/// 本月中途上架的带「（2026-10-22 起）」。billing 的不限流量行、show 共用
pub(crate) fn month_cn(m: &Value) -> String {
    let n = |p: &str| num(m, p).unwrap_or(0.0);
    let v = if m["nic"].is_object() {
        format!("网卡 ↓ {} / ↑ {}", traffic(n("/nic/month_rx")), traffic(n("/nic/month_tx")))
    } else {
        let es = m["entries"].as_array().map(Vec::as_slice).unwrap_or_default();
        let sum = |k: &str| es.iter().filter_map(|e| e["traffic"][k].as_f64()).sum::<f64>() + n(&format!("/removed_traffic/{k}"));
        format!("业务 下载 {} / 上传 {}", traffic(sum("month_down")), traffic(sum("month_up")))
    };
    let from = m["month_counted_from"].as_str().map(|t| format!("（{} 起）", table::day(t))).unwrap_or_default();
    format!("{v}{from}")
}

/// 备用团队里在用的机器（`spare`，即已退役、等到期回收）：哪天退役（组织时区）、原名、哪天自动回收（`recycle_on`，账单时区）。
/// 不是备用、不在用时为 None（billing、show 共用）
pub(crate) fn spare_cn(m: &Value) -> Option<String> {
    if m["spare"] != true || m["lifecycle"] != "active" {
        return None;
    }
    let when = match m["spare_since"].as_str() {
        Some(t) => format!("已于 {} 退役", table::day(t)),
        None => "已退役".into(),
    };
    let from = m["spare_from"].as_str().map(|f| format!("（原 {f}）")).unwrap_or_default();
    Some(match m["recycle_on"].as_str() {
        Some(d) => format!("{when}{from}，{d} 起自动回收（账单时区）"),
        None => format!("{when}{from}，没有订阅，不会自动回收：要清理用 fleetctl retire --now"),
    })
}

fn billing_text(name: &str, m: &Value, limit: Option<i64>, pct: Option<i64>, renewals: &[Value], total: usize) -> String {
    let s = &m["subscription"];
    let mut rows = vec![("机器", name.to_string())];
    if !s.is_null() {
        let [(fee, f), (due, d), (auto, a), remind, tz, _, reset] = sub_fields(s, m);
        // 费用后面补折成的 USD 月费（中控按汇率算好；原币种照录入的写）
        let fee = (fee, format!("{f}{}", crate::fx::sub_usd_cn(s)));
        let a = if s["auto_renew"] == true { "是（到期后中控按周期自动顺延到期日、记一条续费记录；到期前不提醒）".into() } else { a };
        rows.extend([fee, (due, format!("{d}（{}）", due_at(s, "/due_days"))), (auto, a), remind, tz, ("流量上限", limit_cn(m, limit)), reset]);
        // 紧跟在到期那一行后面
        if let Some(sp) = spare_cn(m) {
            rows.insert(3, ("备用", sp));
        }
    } else {
        rows.push(("流量上限", limit_cn(m, limit)));
    }
    let incl = if m["ip_nodes"].as_array().is_some_and(|a| !a.is_empty()) { "，含同机 IP 节点" } else { "" };
    // 有额度的才有「本周期」；不限流量的只写本月；同机 IP 节点没设上限的与宿主机共用（流量上限那一行已写明）
    match limit {
        Some(_) => rows.push((if m["quota"]["mode"] == "rolling30" { "近 30 天已用" } else { "本周期已用" }, used_cn(m, limit, pct, incl))),
        None if m["host_machine"].is_null() => rows.push(("本月流量", format!("流量不限 · 本月 {}", month_cn(m)))),
        None => {}
    }
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

/// `billing <机器>`：订阅 + 流量额度 + 最近的续费记录。`--json`：`{id, name, host_machine, spare, subscription, sub_template, traffic_limit_gb, quota, quota_pct, renewals, renewal_count}`
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
        "id": id, "name": name, "host_machine": m["host_machine"], "spare": m["spare"], "subscription": m["subscription"], "sub_template": m["sub_template"],
        "traffic_limit_gb": limit, "quota": m["quota"], "quota_pct": pct, "renewals": recent, "renewal_count": all.len(),
    });
    ctx.emit(&d, || billing_text(&name, &m, limit, pct, recent, all.len()));
    Ok(0)
}

// `fleetctl renew` 的参数（clap 直接解析成它）
#[derive(clap::Args)]
pub struct RenewOpts {
    pub machine: String,
    /// 新到期日 YYYY-MM-DD（不写则按订阅周期推 --cycles 个周期，目标月没有那一天时落到月末）
    #[arg(long, conflicts_with = "cycles")]
    pub to: Option<String>,
    /// 按订阅的计费周期推几个周期（一次付了几期）
    #[arg(long, default_value_t = 1)]
    pub cycles: u32,
    /// 实付金额，如 39.46（币种同订阅；不写 = 订阅价格 × 周期数，给 --to 时是一个周期的价格）
    #[arg(long)]
    pub amount: Option<String>,
    #[arg(long)]
    pub note: Option<String>,
    #[arg(long)]
    pub yes: bool,
}

/// 没给 --amount 时的实付金额：订阅价格 × 周期数（给了 --to 按一期算）。订阅里没有价格就报错，不按 0 元登记
fn default_amount(s: &Value, name: &str, has_to: bool, cycles: u32) -> anyhow::Result<i64> {
    let price = s["price_cents"].as_i64().ok_or_else(|| anyhow!("中控没有返回 {name} 的订阅价格（price_cents），不按 0 元登记：用 --amount 指定实付金额"))?;
    Ok(price * i64::from(if has_to { 1 } else { cycles }))
}

/// `renew <机器>`：登记一次续费（到期日顺延、续费类告警下一轮评估恢复）。`--json` 输出这条续费记录（同 billing 的 renewals[]）
pub fn renew(ctx: &Ctx, o: RenewOpts) -> anyhow::Result<u8> {
    let (id, name) = ctx.find_id(&o.machine, Want::Live)?;
    let m = ctx.c.get(&format!("/machines/{id}"))?;
    let s = &m["subscription"];
    if s.is_null() {
        bail!("{name} 还没有订阅（费用、到期日），不能登记续费：先用 fleetctl subscription set {name} --price <金额> --due <YYYY-MM-DD> 填写");
    }
    let due = date(&st(s, "/due_date")).ok_or_else(|| anyhow!("中控返回的到期日不是 YYYY-MM-DD：{}", st(s, "/due_date")))?;
    let cur = st(s, "/currency");
    let amount = match o.amount.as_deref() {
        Some(a) => i64::from(parse_money(a)?),
        // 默认是订阅价格（同网页）；--cycles 推几个周期就是几期的钱
        None => default_amount(s, &name, o.to.is_some(), o.cycles)?,
    };
    let note = o.note.as_deref().map(str::trim).filter(|n| !n.is_empty());
    let note_cn = note.map(|n| format!("，备注「{n}」")).unwrap_or_default();
    let body = renew_body(o.to.as_deref(), o.cycles, amount, note)?;
    // 新到期日由中控算：推一个周期的就是订阅里的 next_due
    let target = match (o.to.as_deref(), o.cycles) {
        (Some(t), _) => format!("到期 {due} → {}", t.trim()),
        (None, 1) => format!("到期 {due} → {}", st(s, "/next_due")),
        (None, n) => format!("到期 {due} 按订阅周期（{}）顺延 {n} 期", cycle_cn(&st(s, "/cycle"))),
    };
    confirm(&format!("确认给 {name} 登记续费：{target}，实付 {}{note_cn}？只记录「已续费」、顺延到期日，不会向供应商付款", money(amount, &cur)), o.yes)?;
    let res = ctx.c.post(&format!("/machines/{id}/renewals"), &body)?;
    let rid = st(&res, "/id");
    let new = date(&st(&res, "/new_due")).ok_or_else(|| anyhow!("中控返回的新到期日不是 YYYY-MM-DD：{}", st(&res, "/new_due")))?;
    // 回读：到期日以中控为准（登记时中控锁住订阅、取当时的到期日作旧值）
    let all = ctx.c.get(&format!("/machines/{id}/renewals"))?;
    let r = all.as_array().and_then(|a| a.iter().find(|r| r["id"] == rid.as_str())).ok_or_else(|| anyhow!("刚登记的续费 {rid} 不在续费记录里"))?;
    // due_days = 到期日 − 账单时区的今天，新的到期日顺延了几天就加几天
    let days = s["due_days"].as_i64().map(|d| d + (new - due).whole_days());
    ctx.emit(r, || {
        let paid = r["amount_cents"].as_i64().map_or("-".into(), |c| money(c, &cur));
        format!("已登记续费：{name} 到期 {} → {}（{}），实付 {paid}{note_cn}", st(r, "/old_due"), st(r, "/new_due"), days.map_or("-".into(), due_cn))
    });
    Ok(0)
}

// `fleetctl subscription set` 的参数（clap 直接解析成它；上架的订阅各项也借它转成接口写法，见 `sub_patch`）
#[derive(clap::Args, Default)]
pub struct SubSetOpts {
    pub machine: String,
    /// 价格，如 39.46（币种的主单位，最多两位小数）；none = 跟随套餐
    #[arg(long)]
    pub price: Option<String>,
    /// 币种代码，如 USD / CNY；none = 跟随套餐
    #[arg(long)]
    pub currency: Option<String>,
    /// 计费周期：monthly | quarterly | semiannual | yearly；none = 跟随套餐
    #[arg(long)]
    pub cycle: Option<String>,
    /// 到期日 YYYY-MM-DD（账单时区的日期）。续费请用 renew，会留续费记录
    #[arg(long)]
    pub due: Option<String>,
    /// 到期自动续费：true | false（到期后中控按周期自动顺延到期日、记一条续费记录，到期前不提醒；不会真的扣款）
    #[arg(long)]
    pub auto_renew: Option<bool>,
    /// 到期前几天开始提醒（0–60）；none = 跟随供应商
    #[arg(long)]
    pub remind_days: Option<String>,
    /// 账单时区（IANA 名）：到期与流量周期按它的日期算；none = 跟随供应商
    #[arg(long)]
    pub tz: Option<String>,
    /// 流量上限 GB（供应商的十进制 GB；0 = 不限）；none = 跟随套餐（同机 IP 节点没有套餐 = 与宿主机共用）
    #[arg(long)]
    pub traffic_gb: Option<String>,
    /// 流量每月几号重置（1–31，小月没有这一天按月末）；none = 跟随供应商（按账单日 = 到期日几号，或按自然月 = 1 号）
    #[arg(long)]
    pub reset_day: Option<String>,
    #[arg(long)]
    pub yes: bool,
}

/// `none`（不分大小写）= null：单独设置的字段改回跟随
pub(crate) fn is_none(s: &str) -> bool { s.trim().eq_ignore_ascii_case("none") }

/// `N`（整数）或 `none`（= null）；不是整数报 `bad`。范围由中控校验
fn int_or_none(s: &str, bad: &str) -> anyhow::Result<Value> {
    if is_none(s) {
        return Ok(Value::Null);
    }
    s.trim().parse::<i32>().map(|n| json!(n)).map_err(|_| anyhow!("{bad}"))
}

/// 命令行给了的字段，换成接口的写法：`none` = null（改回跟随套餐 / 供应商）、价格换成分、到期日换成 `YYYY-MM-DD`。
/// 只做这些写法转换；值合不合法（币种、周期、范围、时区）由中控校验，报错原样给出。上架（provision）的订阅也用它
pub(crate) fn sub_patch(o: &SubSetOpts) -> anyhow::Result<Map<String, Value>> {
    let mut p = Map::new();
    if let Some(v) = &o.price {
        p.insert("price_cents".into(), if is_none(v) { Value::Null } else { json!(parse_money(v)?) });
    }
    if let Some(c) = &o.currency {
        p.insert("currency".into(), if is_none(c) { Value::Null } else { json!(c.trim()) });
    }
    if let Some(c) = &o.cycle {
        p.insert("cycle".into(), if is_none(c) { Value::Null } else { json!(c.trim()) });
    }
    if let Some(d) = &o.due {
        p.insert("due_date".into(), json!(date(d).ok_or_else(|| anyhow!("--due 写成 YYYY-MM-DD，如 2026-11-25"))?.to_string()));
    }
    if let Some(b) = o.auto_renew {
        p.insert("auto_renew".into(), json!(b));
    }
    if let Some(v) = &o.remind_days {
        p.insert("remind_days".into(), int_or_none(v, "--remind-days 写整数（到期前几天提醒），或 none（跟随供应商）")?);
    }
    if let Some(z) = &o.tz {
        p.insert("billing_tz".into(), if is_none(z) { Value::Null } else { json!(z.trim()) });
    }
    if let Some(v) = &o.traffic_gb {
        p.insert("traffic_gb".into(), int_or_none(v, "--traffic-gb 写流量上限的 GB 数（整数，0 = 不限），或 none（跟随套餐）")?);
    }
    if let Some(v) = &o.reset_day {
        p.insert("traffic_reset_day".into(), int_or_none(v, "--reset-day 写整数（每月几号），或 none（跟随供应商：按账单日或自然月）")?);
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

/// 改了哪些（`费用 39.46 USD / 月（跟随套餐） → 45.00 USD / 月（单独设置）`）；新建时列出全部。
/// `eff` 是中控预览（`POST …/subscription/preview`）返回的规整后的生效值：和模板相同的按跟随、账单日与重置日怎么算都是中控的规则，这里只对比
fn sub_changes(m: &Value, eff: &Value) -> Vec<String> {
    let cur = &m["subscription"];
    if cur.is_null() {
        sub_fields(eff, m).into_iter().map(|(k, v)| format!("{k} {v}")).collect()
    } else {
        sub_fields(cur, m).into_iter().zip(sub_fields(eff, m)).filter(|(a, b)| a.1 != b.1).map(|((k, a), (_, b))| format!("{k} {a} → {b}")).collect()
    }
}

/// 预览是独立路由：旧中控没有它（404），不会像 query 参数那样被忽略、直接把订阅改了；这时报错退出，不提交
fn preview_effective(r: anyhow::Result<Value>) -> anyhow::Result<Value> {
    match r {
        Err(e) if e.downcast_ref::<ApiError>().is_some_and(|a| a.status == 404) => bail!("中控版本较旧，先升级中控再用 subscription set"),
        r => Ok(r?["effective"].clone()),
    }
}

/// `subscription set <机器>`：先读现有订阅、只改给了的字段再整份提交；可单独设置的字段给 none 改回跟随。
/// `--json` 输出改完的订阅（同 show 的 subscription，含 overrides）
pub fn subscription_set(ctx: &Ctx, o: SubSetOpts) -> anyhow::Result<u8> {
    let patch = sub_patch(&o)?;
    if patch.is_empty() {
        bail!("没有要改的内容：给 --price / --currency / --cycle / --due / --auto-renew / --remind-days / --tz / --traffic-gb / --reset-day 之一");
    }
    let (id, name) = ctx.find_id(&o.machine, Want::Live)?;
    let m = ctx.c.get(&format!("/machines/{id}"))?;
    let cur = &m["subscription"];
    let body = merge_sub(cur, patch)?;
    // 先让中控预览：校验与规整都在中控（不通过就是中控的报错），回来的是改完之后的生效值
    let eff = preview_effective(ctx.c.post(&format!("/machines/{id}/subscription/preview"), &body))?;
    let changes = sub_changes(&m, &eff).join("；");
    if changes.is_empty() {
        ctx.emit(cur, || format!("{name} 的订阅没有变化，未提交"));
        return Ok(0);
    }
    let act = if cur.is_null() { "设置" } else { "修改" };
    confirm(&format!("确认{act} {name} 的订阅：{changes}？只改中控的记录，到期提醒、流量额度告警与费用统计按新值算"), o.yes)?;
    ctx.c.put(&format!("/machines/{id}/subscription"), &body)?;
    // 回读：due_days 由中控按账单时区算
    let s = ctx.c.get(&format!("/machines/{id}"))?["subscription"].take();
    ctx.emit(&s, || format!("已{act} {name} 的订阅：{changes}\n到期日 {}（{}）", st(&s, "/due_date"), due_at(&s, "/due_days")));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spare_hint_only_for_active_spare() {
        let m = |spare: bool, lc: &str, on: Value| {
            json!({"spare": spare, "lifecycle": lc, "recycle_on": on, "spare_from": "团队2-07", "spare_since": "2026-10-21T23:36:00Z"})
        };
        let on = json!("2026-10-22");
        // 退役日按组织时区（默认北京时间）：UTC 10-21 23:36 是北京 10-22
        assert_eq!(spare_cn(&m(true, "active", on.clone())).as_deref(), Some("已于 2026-10-22 退役（原 团队2-07），2026-10-22 起自动回收（账单时区）"));
        assert!(spare_cn(&m(true, "active", Value::Null)).unwrap().contains("没有订阅，不会自动回收"));
        assert_eq!(spare_cn(&m(false, "active", on.clone())), None);
        assert_eq!(spare_cn(&m(true, "retired", on)), None);
        // 替换下来之前就在备用团队里的（没有 spare_from / spare_since）
        assert_eq!(spare_cn(&json!({"spare": true, "lifecycle": "active", "recycle_on": "2026-10-22"})).as_deref(), Some("已退役，2026-10-22 起自动回收（账单时区）"));
    }

    #[test]
    fn preview_404_means_old_server() {
        let old = anyhow::Error::from(ApiError { status: 404, code: "not_found".into(), message: "没有这个接口".into() });
        assert_eq!(preview_effective(Err(old)).unwrap_err().to_string(), "中控版本较旧，先升级中控再用 subscription set");
        let bad = anyhow::Error::from(ApiError { status: 400, code: "bad_request".into(), message: "cycle 不对".into() });
        assert!(preview_effective(Err(bad)).unwrap_err().to_string().contains("cycle 不对"));
        assert_eq!(preview_effective(Ok(json!({"dry_run": true, "effective": {"price_cents": 1}}))).unwrap(), json!({"price_cents": 1}));
    }

    #[test]
    fn money_in_yuan_to_cents() {
        assert_eq!(["39.46", "39", "0.5", " 12.30 ", "0"].map(|s| parse_money(s).unwrap()), [3946, 3900, 50, 1230, 0]);
        for bad in ["", "abc", "-1", "+5", "1.234", "39.", ".5", "1,000", "1e3", "1.2.3", "99999999999"] {
            assert!(parse_money(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn renew_sends_cycles_or_date() {
        // 推几个周期交给中控算（小月按月末、一次加够）；--to 原样交日期
        assert_eq!(renew_body(None, 3, 11838, None).unwrap(), json!({"amount_cents": 11838, "note": null, "cycles": 3}));
        assert_eq!(renew_body(Some(" 2027-01-01 "), 1, 100, Some("账单 #1")).unwrap(), json!({"amount_cents": 100, "note": "账单 #1", "new_due": "2027-01-01"}));
        // 周期数的范围由中控校验；日期写法不对在本地就报
        assert!(renew_body(Some("2026/11/25"), 1, 1, None).is_err());
        // 默认金额是订阅价格 × 周期数；订阅里没有价格不能当 0 元记账
        let sub = json!({"price_cents": 3946});
        assert_eq!((default_amount(&sub, "x", false, 3).unwrap(), default_amount(&sub, "x", true, 3).unwrap()), (11838, 3946));
        assert!(default_amount(&json!({"currency": "USD"}), "x", false, 1).unwrap_err().to_string().contains("不按 0 元登记"));
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

    /// 当前订阅的副本，再改几项：模拟中控预览（`POST …/subscription/preview`）回来的生效值
    fn eff(cur: &Value, set: Value) -> Value {
        let mut e = cur.clone();
        for (k, v) in set.as_object().unwrap() {
            e[k] = v.clone();
        }
        e
    }

    fn sopts(f: impl FnOnce(&mut SubSetOpts)) -> SubSetOpts {
        let mut o = SubSetOpts::default();
        f(&mut o);
        o
    }

    #[test]
    fn subscription_set_merges_given_fields() {
        let m = machine();
        let cur = &m["subscription"];
        // 跟随的提交 null（不把套餐的值抄进订阅），单独设置的照旧；due_days 是中控算的，不提交
        let o = sopts(|o| o.traffic_gb = Some("500".into()));
        let b = merge_sub(cur, sub_patch(&o).unwrap()).unwrap();
        let want = json!({"price_cents": 4200, "currency": null, "cycle": null, "due_date": "2026-10-25", "billing_tz": null,
            "auto_renew": false, "remind_days": null, "traffic_gb": 500, "traffic_reset_day": null});
        assert_eq!(b, want);
        // 改前 → 改后：只对比现有订阅与中控预览回来的生效值
        let after = eff(cur, json!({"traffic_gb": 500, "overrides": ["price_cents", "traffic_gb"]}));
        assert_eq!(sub_changes(&m, &after), ["流量上限 2000 GB（跟随套餐） → 500 GB（单独设置）"]);
        // none = 改回跟随
        let b = merge_sub(cur, sub_patch(&sopts(|o| o.price = Some("none".into()))).unwrap()).unwrap();
        assert!(b["price_cents"].is_null());
        let after = eff(cur, json!({"price_cents": 3946, "overrides": []}));
        assert_eq!(sub_changes(&m, &after), ["费用 42.00 USD / 月（单独设置） → 39.46 USD / 月（跟随套餐）"]);
        // 预览回来和现在一样：没有变化，不提交
        let same = sopts(|o| {
            o.price = Some("42".into());
            o.cycle = Some("monthly".into());
            o.currency = Some("usd".into());
            o.reset_day = Some("25".into());
        });
        assert!(merge_sub(cur, sub_patch(&same).unwrap()).is_ok() && sub_changes(&m, &eff(cur, json!({}))).is_empty());
        // 按账单日重置的跟随值随到期日变：怎么算是中控的事，这里只显示
        let b = merge_sub(cur, sub_patch(&sopts(|o| o.due = Some("2026-11-30".into()))).unwrap()).unwrap();
        assert_eq!(b["due_date"], "2026-11-30");
        let after = eff(cur, json!({"due_date": "2026-11-30", "traffic_reset_day": 30}));
        assert_eq!(sub_changes(&m, &after), ["到期日 2026-10-25 → 2026-11-30", "流量重置日 每月 25 号（跟随供应商：按账单日） → 每月 30 号（跟随供应商：按账单日）"]);
        // 没有订阅：新建只要到期日，其余跟随（不写默认值）
        let mut fresh = machine();
        fresh["subscription"] = Value::Null;
        assert!(merge_sub(&Value::Null, sub_patch(&sopts(|o| o.price = Some("39.46".into()))).unwrap()).is_err());
        let o = sopts(|o| {
            o.due = Some("2026-11-25".into());
            o.auto_renew = Some(true);
        });
        let b = merge_sub(&Value::Null, sub_patch(&o).unwrap()).unwrap();
        let want = json!({"price_cents": null, "currency": null, "cycle": null, "due_date": "2026-11-25", "billing_tz": null,
            "auto_renew": true, "remind_days": null, "traffic_gb": null, "traffic_reset_day": null});
        assert_eq!(b, want);
        let created = json!({"price_cents": 3946, "currency": "USD", "cycle": "monthly", "due_date": "2026-11-25", "billing_tz": "America/Los_Angeles",
            "auto_renew": true, "remind_days": 7, "traffic_gb": 2000, "traffic_reset_day": 25, "overrides": []});
        assert_eq!(sub_changes(&fresh, &created)[..3], ["费用 39.46 USD / 月（跟随套餐）", "到期日 2026-11-25", "自动续费 是"]);
        // 写法转换不了的在提交前就报；值合不合法（币种、周期、范围）由中控校验
        let bad = [
            sopts(|o| o.due = Some("2026-11-31".into())),
            sopts(|o| o.price = Some("1.999".into())),
            sopts(|o| o.remind_days = Some("abc".into())),
            sopts(|o| o.traffic_gb = Some("1.5".into())),
            sopts(|o| o.reset_day = Some("x".into())),
        ];
        for o in &bad {
            assert!(sub_patch(o).is_err());
        }
        let all_none = sopts(|o| {
            o.price = Some("none".into());
            o.currency = Some("None".into());
            o.cycle = Some("none".into());
            o.tz = Some("none".into());
            o.remind_days = Some("none".into());
            o.traffic_gb = Some("0".into());
            o.reset_day = Some("none".into());
        });
        let want = json!({"price_cents": null, "currency": null, "cycle": null, "billing_tz": null, "remind_days": null, "traffic_gb": 0, "traffic_reset_day": null});
        assert_eq!(sub_patch(&all_none).unwrap(), *want.as_object().unwrap());
        // 值原样交给中控（只去掉首尾空白）
        let o = sopts(|o| {
            o.currency = Some(" usd ".into());
            o.cycle = Some("month".into());
            o.remind_days = Some("61".into());
        });
        assert_eq!(Value::Object(sub_patch(&o).unwrap()), json!({"currency": "usd", "cycle": "month", "remind_days": 61}));
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
        // 已用同供应商面板：十进制 GB；写起止、规则、按哪块网卡；有接入前读数的注明到哪天为止
        let q = json!({"agent": {"nic_ifaces": ["ens3"]}, "quota": {"used": 901_600_000_000i64, "period_start": "2026-10-13", "period_end": "2026-11-13", "rule": "min",
            "base": 869_700_000_000i64, "base_at": "2026-10-22T07:30:00Z", "counted_from": null}});
        assert_eq!(
            used_cn(&q, Some(2000), Some(45), ""),
            "901.6 GB / 2000 GB（45%），2026-10-13 → 2026-11-13，按 ens3 网卡，取下行和上行中较小的；接入后实测 31.9 GB，加上接入前的面板读数 869.7 GB（2026-11-13 重置后只算实测）"
        );
        // 滚动 30 天、周期中途上架（日期按组织时区）；业务流量不写网卡
        let q = json!({"agent": {"nic_ifaces": ["eth0"]}, "quota": {"used": 31_900_000, "period_start": "2026-09-24", "period_end": null, "rule": null, "base": null, "counted_from": "2026-10-21T17:10:00Z"}});
        assert_eq!(used_cn(&q, Some(1000), Some(0), ""), "0.03 GB / 1000 GB（0%），2026-09-24 起，按业务流量，下载加上传；从 2026-10-22 开始统计，之前的没算进来");
        // 不限流量只写本月：有 agent 按网卡，家宽代理按业务（含已移除入口）
        let m = json!({"nic": {"month_rx": 1_044_400_000_000i64, "month_tx": 108_000_000_000i64}, "month_counted_from": "2026-10-21T17:10:00Z"});
        assert_eq!(month_cn(&m), "网卡 ↓ 1.0 TB / ↑ 108.0 GB（2026-10-22 起）");
        let m = json!({"nic": null, "entries": [{"traffic": {"month_down": 3_000_000_000i64, "month_up": 1_000_000_000i64}}], "removed_traffic": {"month_down": 500_000_000, "month_up": 0}});
        assert_eq!(month_cn(&m), "业务 下载 3.5 GB / 上传 1.0 GB");
        // 总览的本月业务流量只标月-日（组织时区）
        assert_eq!(md_of("2026-10-21T17:10:00Z"), "10-22");
    }
}
