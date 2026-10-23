//! 目录：供应商（provider ls / add / set / rm）与套餐（plan ls / add / set / rm）。
//! 两者都是机器订阅的模板：价格、币种、周期、流量上限跟随套餐，账单时区、提前提醒、流量计量与重置跟随供应商，改了立即对没单独设置的机器生效。
//! 上架选的供应商、套餐要先在这里建好。`<供应商>` 写 id、id 前缀、名称或缩写；`<套餐>` 写 id、id 前缀、名称，重名时写「供应商/套餐」。
//! `--json`：ls 是 API 的 data（plan ls 按 --provider / --all 筛过），add / set 是改完后的那一项（同 ls 的一项），rm 是 API 的 data（`{id}`）。

use crate::billing::{is_none, parse_money};
use crate::cmds::{cycle_cn, st, Ctx};
use crate::meta;
use crate::ops::confirm;
use crate::resolve;
use crate::table::{self, money};
use anyhow::{anyhow, bail};
use serde_json::{json, Map, Value};

fn list(ctx: &Ctx, path: &str) -> anyhow::Result<Vec<Value>> { Ok(ctx.c.get(path)?.as_array().cloned().unwrap_or_default()) }

fn s(v: &Value, k: &str) -> String { v[k].as_str().unwrap_or("").to_string() }

// ── 供应商 ────────────────────────────────────────────────────────────────

/// 供应商的可改项（add / set 共用）
#[derive(clap::Args, Default)]
pub struct ProviderArgs {
    /// 缩写（新建必填，如 ATT）
    #[arg(long)]
    pub code: Option<String>,
    /// 账单时区（IANA 名，如 Asia/Shanghai）：名下机器的到期与流量周期按它的日期算；不给 = 跟随组织时区，set 时 none = 改回跟随
    #[arg(long)]
    pub tz: Option<String>,
    /// 控制台地址（http(s)）；set 时 none = 清空
    #[arg(long)]
    pub panel_url: Option<String>,
    /// 流量额度的网卡计量：min（入出取小）| max（取大，新建默认）| sum（相加）| out（只算出站）
    #[arg(long)]
    pub traffic_rule: Option<String>,
    /// 流量周期：month（自然月，新建默认）| billing（账单日 = 到期日几号）| rolling30（滚动 30 天）
    #[arg(long)]
    pub traffic_reset: Option<String>,
    /// 到期前几天提醒（0–60，新建默认 7）
    #[arg(long)]
    pub remind_days: Option<i32>,
}

/// 命令行给了的字段 → 接口的写法（`none` = null：时区改回跟随组织、清空控制台地址）
fn provider_body(name: Option<&str>, a: &ProviderArgs) -> Map<String, Value> {
    let mut b = Map::new();
    let or_null = |v: &str| if is_none(v) { Value::Null } else { json!(v.trim()) };
    if let Some(n) = name {
        b.insert("name".into(), json!(n.trim()));
    }
    if let Some(c) = &a.code {
        b.insert("code".into(), json!(c.trim()));
    }
    if let Some(t) = &a.tz {
        b.insert("timezone".into(), or_null(t));
    }
    if let Some(u) = &a.panel_url {
        b.insert("panel_url".into(), or_null(u));
    }
    if let Some(r) = &a.traffic_rule {
        b.insert("traffic_rule".into(), json!(r.trim()));
    }
    if let Some(r) = &a.traffic_reset {
        b.insert("traffic_reset".into(), json!(r.trim()));
    }
    if let Some(d) = a.remind_days {
        b.insert("remind_days".into(), json!(d));
    }
    b
}

fn tz_cn(v: &Value) -> String { v.as_str().map_or_else(|| format!("跟随组织（{}）", meta::tz_label()), String::from) }

/// 供应商一个字段给人看的写法
fn provider_field(k: &str, v: &Value) -> String {
    match k {
        "timezone" => tz_cn(v),
        "traffic_rule" => v.as_str().map_or("-".into(), |r| meta::label("traffic_rules", r).to_string()),
        "traffic_reset" => v.as_str().map_or("-".into(), |r| meta::label("traffic_resets", r).to_string()),
        "panel_url" => v.as_str().unwrap_or("-").to_string(),
        _ => match v {
            Value::String(x) => x.clone(),
            Value::Null => "-".into(),
            x => x.to_string(),
        },
    }
}

const PROVIDER_CN: [(&str, &str); 7] =
    [("name", "名称"), ("code", "缩写"), ("timezone", "账单时区"), ("panel_url", "控制台"), ("traffic_rule", "流量计量"), ("traffic_reset", "流量重置"), ("remind_days", "提前提醒天数")];

pub(crate) fn find_provider<'a>(all: &'a [Value], q: &str) -> anyhow::Result<&'a Value> {
    let name = |v: &Value| s(v, "name");
    let code = |v: &Value| s(v, "code");
    let label = |v: &Value| format!("{}（缩写 {}）", s(v, "name"), s(v, "code"));
    resolve::named(all, q, "供应商", &[&name, &code], &label).map_err(|e| anyhow!("{e}（先 fleetctl provider ls 看列表）"))
}

pub fn provider_ls(ctx: &Ctx) -> anyhow::Result<u8> {
    let d = ctx.c.get("/providers")?;
    ctx.emit(&d, || {
        let all = d.as_array().cloned().unwrap_or_default();
        if all.is_empty() {
            return "还没有供应商（fleetctl provider add <名称> --code <缩写> 新建）".into();
        }
        // 每家的套餐数（在用 / 归档），取不到就不显示
        let plans = list(ctx, "/plans").unwrap_or_default();
        let rows: Vec<Vec<String>> = all
            .iter()
            .map(|p| {
                let mine: Vec<&Value> = plans.iter().filter(|x| x["provider_id"] == p["id"]).collect();
                let archived = mine.iter().filter(|x| !x["archived_at"].is_null()).count();
                let n = if archived > 0 { format!("{}（另有 {archived} 个归档）", mine.len() - archived) } else { mine.len().to_string() };
                vec![
                    s(p, "name"),
                    s(p, "code"),
                    tz_cn(&p["timezone"]),
                    provider_field("traffic_rule", &p["traffic_rule"]),
                    provider_field("traffic_reset", &p["traffic_reset"]),
                    st(p, "/remind_days"),
                    n,
                    st(p, "/panel_url"),
                    st(p, "/id"),
                ]
            })
            .collect();
        table::render(&["名称", "缩写", "账单时区", "流量计量", "流量重置", "提醒天数", "套餐", "控制台", "id"], &rows)
    });
    Ok(0)
}

/// 新建：只给 --code 就行，其余用中控的默认（跟随组织时区、流量取大、自然月、提前 7 天提醒）
pub fn provider_add(ctx: &Ctx, name: &str, a: &ProviderArgs) -> anyhow::Result<u8> {
    if a.code.as_deref().is_none_or(|c| c.trim().is_empty()) {
        bail!("新建供应商要给 --code（缩写，如 ATT）");
    }
    let body = provider_body(Some(name), a);
    let id = st(&ctx.c.post("/providers", &Value::Object(body))?, "/id");
    let p = list(ctx, "/providers")?.into_iter().find(|p| p["id"] == id.as_str()).unwrap_or(json!({"id": id}));
    ctx.emit(&p, || format!("已新建供应商「{}」（缩写 {}，id {id}）；建它的套餐：fleetctl plan add <名称> --provider {} --price … --currency … --cycle …", s(&p, "name"), s(&p, "code"), s(&p, "code")));
    Ok(0)
}

/// 改了哪些：`[(中文名, 旧, 新)]`，和现在一样的不列
/// `show(字段, 值, 是不是新值)`
fn changes(old: &Value, body: &Map<String, Value>, names: &[(&str, &str)], show: &dyn Fn(&str, &Value, bool) -> String) -> Vec<(String, String, String)> {
    names
        .iter()
        .filter_map(|(k, cn)| {
            let new = body.get(*k)?;
            let cur = &old[*k];
            (cur != new).then(|| (cn.to_string(), show(k, cur, false), show(k, new, true)))
        })
        .collect()
}

fn diff_text(c: &[(String, String, String)]) -> String { c.iter().map(|(k, a, b)| format!("{k} {a} → {b}")).collect::<Vec<_>>().join("、") }

pub fn provider_set(ctx: &Ctx, q: &str, name: Option<&str>, a: &ProviderArgs, yes: bool) -> anyhow::Result<u8> {
    let body = provider_body(name, a);
    if body.is_empty() {
        bail!("没有要改的内容：给 --name / --code / --tz / --panel-url / --traffic-rule / --traffic-reset / --remind-days 之一");
    }
    let all = list(ctx, "/providers")?;
    let old = find_provider(&all, q)?.clone();
    let id = st(&old, "/id");
    let ch = changes(&old, &body, &PROVIDER_CN, &|k, v, _| provider_field(k, v));
    if ch.is_empty() {
        ctx.emit(&old, || format!("「{}」没有变化，未提交", s(&old, "name")));
        return Ok(0);
    }
    let follow = ["timezone", "traffic_rule", "traffic_reset", "remind_days"].iter().any(|k| body.contains_key(*k) && body[*k] != old[*k]);
    let note = if follow { "；名下机器没单独设置这几项的立即跟着变（到期提醒、流量周期与已用按新的算）" } else { "" };
    confirm(&format!("确认修改供应商「{}」：{}{note}？", s(&old, "name"), diff_text(&ch)), yes)?;
    ctx.c.patch(&format!("/providers/{id}"), &Value::Object(body))?;
    let p = list(ctx, "/providers")?.into_iter().find(|p| p["id"] == id.as_str()).unwrap_or(Value::Null);
    ctx.emit(&p, || format!("已修改供应商「{}」：{}", s(&p, "name"), diff_text(&ch)));
    Ok(0)
}

pub fn provider_rm(ctx: &Ctx, q: &str, yes: bool) -> anyhow::Result<u8> {
    let all = list(ctx, "/providers")?;
    let p = find_provider(&all, q)?;
    let name = s(p, "name");
    confirm(&format!("确认删除供应商「{name}」？还有套餐（含已归档的）或机器（含已退役的）时中控会拒绝"), yes)?;
    let d = ctx.c.delete(&format!("/providers/{}", st(p, "/id")))?;
    ctx.emit(&d, || format!("已删除供应商「{name}」"));
    Ok(0)
}

// ── 套餐 ──────────────────────────────────────────────────────────────────

/// 套餐的可改项（add / set 共用）。规格与流量上限给 N，set 时给 none = 清空
#[derive(clap::Args, Default)]
pub struct PlanArgs {
    /// 供应商（id / 名称 / 缩写）：add 必填；set 时是换供应商（有机器用着的套餐不能换）
    #[arg(long)]
    pub provider: Option<String>,
    /// CPU 核数
    #[arg(long)]
    pub cpu: Option<String>,
    /// 内存 MB
    #[arg(long)]
    pub mem_mb: Option<String>,
    /// 磁盘 GB
    #[arg(long)]
    pub disk_gb: Option<String>,
    /// 带宽 Mbps
    #[arg(long)]
    pub bandwidth_mbps: Option<String>,
    /// 每个流量周期的额度，十进制 GB（同供应商面板）；0 或 none = 不限
    #[arg(long)]
    pub traffic_gb: Option<String>,
    /// 每个计费周期的价格，如 299.99（币种的主单位，最多两位小数）
    #[arg(long)]
    pub price: Option<String>,
    /// 币种代码，如 CNY / USD
    #[arg(long)]
    pub currency: Option<String>,
    /// 计费周期：monthly | quarterly | semiannual | yearly
    #[arg(long)]
    pub cycle: Option<String>,
    /// 备注（≤ 2000 字）
    #[arg(long)]
    pub notes: Option<String>,
}

/// 规格类字段：命令行参数名、接口字段、中文名
const SPECS: [(&str, &str, &str); 5] =
    [("--cpu", "cpu_cores", "CPU"), ("--mem-mb", "mem_mb", "内存 MB"), ("--disk-gb", "disk_gb", "磁盘 GB"), ("--bandwidth-mbps", "bandwidth_mbps", "带宽 Mbps"), ("--traffic-gb", "traffic_gb", "流量 GB")];

/// 命令行给了的字段 → 接口的写法（供应商另外解析成 provider_id）。规格给 N（非负整数）或 none（= null）
fn plan_body(name: Option<&str>, a: &PlanArgs) -> anyhow::Result<Map<String, Value>> {
    let mut b = Map::new();
    if let Some(n) = name {
        b.insert("name".into(), json!(n.trim()));
    }
    let vals = [&a.cpu, &a.mem_mb, &a.disk_gb, &a.bandwidth_mbps, &a.traffic_gb];
    for ((flag, k, _), v) in SPECS.iter().zip(vals) {
        let Some(v) = v else { continue };
        let x = if is_none(v) {
            Value::Null
        } else {
            json!(v.trim().parse::<i32>().ok().filter(|n| *n >= 0).ok_or_else(|| anyhow!("{flag} 写非负整数，或 none 清空：「{v}」"))?)
        };
        b.insert((*k).into(), x);
    }
    if let Some(p) = &a.price {
        b.insert("price_cents".into(), json!(parse_money(p)?));
    }
    if let Some(c) = &a.currency {
        b.insert("currency".into(), json!(c.trim().to_uppercase()));
    }
    if let Some(c) = &a.cycle {
        b.insert("cycle".into(), json!(c.trim()));
    }
    if let Some(n) = &a.notes {
        b.insert("notes".into(), json!(n));
    }
    Ok(b)
}

/// 规格：`2 核 · 2048 MB · 40 GB · 1000 Mbps · 2000 GB/周期`，没填的项不写；流量上限 null / 0 写「流量不限」
fn spec_cn(p: &Value) -> String {
    let n = |k: &str| p[k].as_i64();
    let mut v = vec![];
    if let Some(c) = n("cpu_cores") {
        v.push(format!("{c} 核"));
    }
    if let Some(m) = n("mem_mb") {
        v.push(format!("{m} MB"));
    }
    if let Some(d) = n("disk_gb") {
        v.push(format!("{d} GB 盘"));
    }
    if let Some(b) = n("bandwidth_mbps") {
        v.push(format!("{b} Mbps"));
    }
    v.push(match n("traffic_gb") {
        Some(t) if t > 0 => format!("流量 {t} GB"),
        _ => "流量不限".into(),
    });
    v.join(" · ")
}

fn price_cn(p: &Value) -> String { format!("{} / {}", money(p["price_cents"].as_i64().unwrap_or(0), &s(p, "currency")), cycle_cn(&s(p, "cycle"))) }

/// 套餐一个字段给人看的写法（价格带币种：改币种时旧价格按旧币种写）
fn plan_field(k: &str, v: &Value, cur: &str) -> String {
    match (k, v) {
        ("traffic_gb", Value::Null) => "不限".into(),
        ("traffic_gb", v) if v.as_i64() == Some(0) => "不限".into(),
        (_, Value::Null) => "-".into(),
        ("price_cents", v) => money(v.as_i64().unwrap_or(0), cur),
        ("cycle", v) => cycle_cn(v.as_str().unwrap_or("")).to_string(),
        ("archived", Value::Bool(true)) => "归档".into(),
        ("archived", _) => "在用".into(),
        (_, Value::String(x)) => x.clone(),
        (_, x) => x.to_string(),
    }
}

/// 「供应商/套餐」：重名套餐的写法，也用在候选列表、确认语与输出里（供应商不在列表里时写它的 id）
fn plan_label(p: &Value, providers: &[Value]) -> String {
    let pv = providers.iter().find(|x| x["id"] == p["provider_id"]).map_or_else(|| st(p, "/provider_id"), |x| s(x, "name"));
    format!("{pv}/{}", s(p, "name"))
}

pub(crate) fn find_plan<'a>(all: &'a [Value], providers: &[Value], q: &str) -> anyhow::Result<&'a Value> {
    let name = |v: &Value| s(v, "name");
    let full = |v: &Value| plan_label(v, providers);
    let label = |v: &Value| format!("{}{}", plan_label(v, providers), if v["archived_at"].is_null() { "" } else { "（已归档）" });
    resolve::named(all, q, "套餐", &[&name, &full], &label).map_err(|e| anyhow!("{e}（先 fleetctl plan ls --all 看列表；重名时写「供应商/套餐」）"))
}

pub fn plan_ls(ctx: &Ctx, provider: Option<&str>, all: bool) -> anyhow::Result<u8> {
    let providers = list(ctx, "/providers")?;
    let mut plans = list(ctx, "/plans")?;
    if let Some(q) = provider {
        let pid = find_provider(&providers, q)?["id"].clone();
        plans.retain(|p| p["provider_id"] == pid);
    }
    let archived = plans.iter().filter(|p| !p["archived_at"].is_null()).count();
    if !all {
        plans.retain(|p| p["archived_at"].is_null());
    }
    let d = Value::Array(plans.clone());
    ctx.emit(&d, || {
        if plans.is_empty() {
            let more = if archived > 0 && !all { format!("（另有 {archived} 个已归档的，--all 列出）") } else { String::new() };
            return format!("没有套餐{more}（fleetctl plan add <名称> --provider <供应商> --price … --currency … --cycle … 新建）");
        }
        let rows: Vec<Vec<String>> = plans
            .iter()
            .map(|p| {
                let pv = providers.iter().find(|x| x["id"] == p["provider_id"]).map_or_else(|| "-".into(), |x| s(x, "name"));
                let state = if p["archived_at"].is_null() { "在用".into() } else { format!("归档于 {}", table::ts(&s(p, "archived_at"))) };
                vec![pv, s(p, "name"), spec_cn(p), price_cn(p), st(p, "/machine_count"), state, s(p, "notes"), st(p, "/id")]
            })
            .collect();
        let mut out = table::render(&["供应商", "名称", "规格", "价格", "在用机器", "状态", "备注", "id"], &rows);
        if archived > 0 && !all {
            out.push_str(&format!("\n另有 {archived} 个已归档的套餐（--all 列出）"));
        }
        out
    });
    Ok(0)
}

pub fn plan_add(ctx: &Ctx, name: &str, a: &PlanArgs) -> anyhow::Result<u8> {
    let missing: Vec<&str> = [("--provider", a.provider.is_none()), ("--price", a.price.is_none()), ("--currency", a.currency.is_none()), ("--cycle", a.cycle.is_none())]
        .into_iter()
        .filter_map(|(f, m)| m.then_some(f))
        .collect();
    if !missing.is_empty() {
        bail!("新建套餐要给 {}（价格写主单位，如 --price 299.99 --currency CNY --cycle monthly）", missing.join(" "));
    }
    let mut body = plan_body(Some(name), a)?;
    // 新建时 none 就是不填
    body.retain(|_, v| !v.is_null());
    let providers = list(ctx, "/providers")?;
    let pv = find_provider(&providers, a.provider.as_deref().unwrap_or_default())?;
    body.insert("provider_id".into(), pv["id"].clone());
    let id = st(&ctx.c.post("/plans", &Value::Object(body))?, "/id");
    let p = list(ctx, "/plans")?.into_iter().find(|p| p["id"] == id.as_str()).unwrap_or(json!({"id": id}));
    ctx.emit(&p, || format!("已新建套餐「{}」：{} · {}（id {id}）", plan_label(&p, &providers), price_cn(&p), spec_cn(&p)));
    Ok(0)
}

const PLAN_CN: [(&str, &str); 12] = [
    ("name", "名称"),
    ("provider_id", "供应商"),
    ("cpu_cores", "CPU"),
    ("mem_mb", "内存 MB"),
    ("disk_gb", "磁盘 GB"),
    ("bandwidth_mbps", "带宽 Mbps"),
    ("traffic_gb", "流量 GB"),
    ("price_cents", "价格"),
    ("currency", "币种"),
    ("cycle", "周期"),
    ("notes", "备注"),
    ("archived", "状态"),
];

/// `archive`：Some(true) 归档、Some(false) 取消归档
pub fn plan_set(ctx: &Ctx, q: &str, name: Option<&str>, a: &PlanArgs, archive: Option<bool>, yes: bool) -> anyhow::Result<u8> {
    let mut body = plan_body(name, a)?;
    if let Some(ar) = archive {
        body.insert("archived".into(), json!(ar));
    }
    if body.is_empty() && a.provider.is_none() {
        bail!("没有要改的内容：给 --name / --provider / --cpu / --mem-mb / --disk-gb / --bandwidth-mbps / --traffic-gb / --price / --currency / --cycle / --notes / --archive / --unarchive 之一");
    }
    let providers = list(ctx, "/providers")?;
    let all = list(ctx, "/plans")?;
    let mut old = find_plan(&all, &providers, q)?.clone();
    let id = st(&old, "/id");
    if let Some(pq) = &a.provider {
        body.insert("provider_id".into(), find_provider(&providers, pq)?["id"].clone());
    }
    old["archived"] = json!(!old["archived_at"].is_null());
    // 价格按币种写：旧的用旧币种，新的用改后的币种
    let new_cur = body.get("currency").and_then(Value::as_str).map_or_else(|| s(&old, "currency"), String::from);
    let old_cur = s(&old, "currency");
    let pv_name = |v: &Value| providers.iter().find(|x| x["id"] == *v).map_or_else(|| st(v, ""), |x| s(x, "name"));
    let show = |k: &str, v: &Value, new: bool| -> String {
        match k {
            "provider_id" => pv_name(v),
            _ => plan_field(k, v, if new { &new_cur } else { &old_cur }),
        }
    };
    let ch = changes(&old, &body, &PLAN_CN, &show);
    if ch.is_empty() {
        if let Some(o) = old.as_object_mut() {
            o.remove("archived");
        }
        ctx.emit(&old, || format!("「{}」没有变化，未提交", plan_label(&old, &providers)));
        return Ok(0);
    }
    let n = old["machine_count"].as_i64().unwrap_or(0);
    let tpl = ["price_cents", "currency", "cycle", "traffic_gb"].iter().any(|k| body.contains_key(*k) && body[*k] != old[*k]);
    let note = match (tpl, n) {
        (true, n) if n > 0 => format!("；用它的 {n} 台在用机器没单独设置价格 / 币种 / 周期 / 流量上限的立即跟着变"),
        _ => String::new(),
    };
    confirm(&format!("确认修改套餐「{}」：{}{note}？", plan_label(&old, &providers), diff_text(&ch)), yes)?;
    ctx.c.patch(&format!("/plans/{id}"), &Value::Object(body))?;
    let p = list(ctx, "/plans")?.into_iter().find(|p| p["id"] == id.as_str()).unwrap_or(Value::Null);
    ctx.emit(&p, || format!("已修改套餐「{}」：{}", plan_label(&p, &providers), diff_text(&ch)));
    Ok(0)
}

pub fn plan_rm(ctx: &Ctx, q: &str, yes: bool) -> anyhow::Result<u8> {
    let providers = list(ctx, "/providers")?;
    let all = list(ctx, "/plans")?;
    let p = find_plan(&all, &providers, q)?;
    let label = plan_label(p, &providers);
    confirm(&format!("确认删除套餐「{label}」？还有机器（含已退役的）用着时中控会拒绝，不再上架的用 fleetctl plan set {label} --archive"), yes)?;
    let d = ctx.c.delete(&format!("/plans/{}", st(p, "/id")))?;
    ctx.emit(&d, || format!("已删除套餐「{label}」"));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_fields() {
        let a = ProviderArgs { code: Some(" ATT ".into()), tz: Some("none".into()), panel_url: Some("https://p.example.com".into()), remind_days: Some(5), ..Default::default() };
        assert_eq!(
            Value::Object(provider_body(Some("ATTVPS"), &a)),
            json!({"name": "ATTVPS", "code": "ATT", "timezone": null, "panel_url": "https://p.example.com", "remind_days": 5})
        );
        let a = ProviderArgs { tz: Some("Asia/Tokyo".into()), traffic_rule: Some("min".into()), traffic_reset: Some("billing".into()), panel_url: Some("None".into()), ..Default::default() };
        assert_eq!(Value::Object(provider_body(None, &a)), json!({"timezone": "Asia/Tokyo", "traffic_rule": "min", "traffic_reset": "billing", "panel_url": null}));
        assert!(provider_body(None, &ProviderArgs::default()).is_empty());
    }

    #[test]
    fn provider_diff_lists_only_changes() {
        crate::meta::test_init();
        let old = json!({"name": "甲", "code": "A", "timezone": null, "panel_url": null, "traffic_rule": "max", "traffic_reset": "month", "remind_days": 7});
        let a = ProviderArgs { code: Some("A".into()), tz: Some("Asia/Tokyo".into()), traffic_rule: Some("min".into()), remind_days: Some(7), ..Default::default() };
        let ch = changes(&old, &provider_body(None, &a), &PROVIDER_CN, &|k, v, _| provider_field(k, v));
        assert_eq!(diff_text(&ch), "账单时区 跟随组织（北京时间） → Asia/Tokyo、流量计量 下行、上行取较大 → 下行、上行取较小");
    }

    #[test]
    fn plan_fields() {
        let a = PlanArgs {
            cpu: Some("2".into()),
            mem_mb: Some("none".into()),
            traffic_gb: Some("0".into()),
            price: Some("299.99".into()),
            currency: Some(" cny ".into()),
            cycle: Some("monthly".into()),
            ..Default::default()
        };
        assert_eq!(
            Value::Object(plan_body(Some("基础版"), &a).unwrap()),
            json!({"name": "基础版", "cpu_cores": 2, "mem_mb": null, "traffic_gb": 0, "price_cents": 29999, "currency": "CNY", "cycle": "monthly"})
        );
        for bad in [PlanArgs { cpu: Some("-1".into()), ..Default::default() }, PlanArgs { disk_gb: Some("40G".into()), ..Default::default() }, PlanArgs { price: Some("1.999".into()), ..Default::default() }] {
            assert!(plan_body(None, &bad).is_err());
        }
        let e = plan_body(None, &PlanArgs { bandwidth_mbps: Some("x".into()), ..Default::default() }).unwrap_err().to_string();
        assert!(e.contains("--bandwidth-mbps"), "{e}");
    }

    #[test]
    fn plan_wording() {
        crate::meta::test_init();
        let p = json!({"cpu_cores": 2, "mem_mb": 2048, "disk_gb": null, "bandwidth_mbps": 1000, "traffic_gb": 2000, "price_cents": 29999, "currency": "CNY", "cycle": "quarterly"});
        assert_eq!(spec_cn(&p), "2 核 · 2048 MB · 1000 Mbps · 流量 2000 GB");
        assert_eq!(price_cn(&p), "299.99 CNY / 季");
        assert_eq!(spec_cn(&json!({"traffic_gb": 0})), "流量不限");
        let old = json!({"name": "基础版", "price_cents": 500, "currency": "USD", "cycle": "monthly", "traffic_gb": null, "notes": "", "archived": false});
        let mut body = plan_body(None, &PlanArgs { price: Some("6".into()), currency: Some("cny".into()), traffic_gb: Some("none".into()), ..Default::default() }).unwrap();
        body.insert("archived".into(), json!(true));
        let show = |k: &str, v: &Value, new: bool| plan_field(k, v, if new { "CNY" } else { "USD" });
        assert_eq!(diff_text(&changes(&old, &body, &PLAN_CN, &show)), "价格 5.00 USD → 6.00 CNY、币种 USD → CNY、状态 在用 → 归档");
    }
}
