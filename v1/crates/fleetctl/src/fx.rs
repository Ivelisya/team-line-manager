//! 计费统一按 USD：中控按汇率把月费折成 USD（`subscription.monthly_usd_cents`、总览的 `monthly_usd`），这里只负责写成文字；
//! 以及 `settings fx`：看汇率、手动覆盖某个币种、取消覆盖、立即从汇率源更新。

use crate::cmds::Ctx;
use crate::ops::confirm;
use crate::table::{self, money, money_at};
use anyhow::bail;
use serde_json::{json, Value};

const PATH: &str = "/settings/fx-rates";

/// 汇率写成「1 CNY = 0.1404 USD」（最多 6 位有效小数，去掉末尾的 0）
pub(crate) fn rate_cn(cur: &str, usd_per_unit: f64) -> String {
    let s = format!("{usd_per_unit:.6}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("1 {cur} = {s} USD")
}

/// 订阅的费用后面补的 USD 折算：「；折合 37.50 USD/月（1 CNY = 0.125 USD）」。USD 按月付的不补；
/// 没有汇率的写「未折算」
pub(crate) fn sub_usd_cn(s: &Value) -> String {
    let Some(obj) = s.as_object() else { return String::new() };
    let cur = obj.get("currency").and_then(Value::as_str).unwrap_or("");
    let monthly = obj.get("cycle").and_then(Value::as_str) == Some("monthly");
    match (s["monthly_usd_cents"].as_i64(), s["usd_per_unit"].as_f64()) {
        _ if cur == "USD" && monthly => String::new(),
        (Some(u), _) if cur == "USD" => format!("；折合 {}/月", money(u, "USD")),
        (Some(u), Some(r)) => format!("；折合 {}/月（{}）", money(u, "USD"), rate_cn(cur, r)),
        _ => format!("；未折算（没有 {cur} 的汇率，fleetctl settings fx {cur} --usd <汇率> 设置）"),
    }
}

/// `ls --due` 的月费列：「37.50 USD」，原币种不是 USD 时括注原价；没有汇率写原价并标未折算
pub(crate) fn monthly_cell(s: &Value) -> String {
    if s.is_null() {
        return "-".into();
    }
    let cur = s["currency"].as_str().unwrap_or("");
    let orig = money_at(s, "/monthly_cents", cur);
    match s["monthly_usd_cents"].as_i64() {
        Some(u) if cur == "USD" => money(u, "USD"),
        Some(u) => format!("{}（{orig}）", money(u, "USD")),
        None => format!("{orig}（未折算）"),
    }
}

/// `status` 的每月费用：USD 合计，括注各原币种的金额与汇率；没有汇率的单列。合计不含不续费的备用机（spare_unbilled），有的另附一句小计。
/// 合计按几台算的读中控的 `paid_machines`（旧中控没有就不写）
pub(crate) fn cost_cn(ov: &Value) -> String {
    let sp = &ov["monthly_usd_spare"];
    let total = total_cn(&ov["monthly_usd"]);
    let total = match ov["paid_machines"].as_i64() {
        Some(n) if total != "-" => format!("{total}（在用 {n} 台）"),
        _ => total,
    };
    match sp["count"].as_i64().filter(|n| *n > 0) {
        Some(n) => format!("{total}；另有不续费的备用 {n} 台 {}，不在合计里", total_cn(sp)),
        None => total,
    }
}

/// USD 合计（中控的 Total：`{cents, by_currency, unconverted}`，如总览的月费、待付清单、批量续费的合计）写成文字
pub(crate) fn total_cn(u: &Value) -> String {
    let by = u["by_currency"].as_array().map(Vec::as_slice).unwrap_or_default();
    let un = u["unconverted"].as_array().map(Vec::as_slice).unwrap_or_default();
    if by.is_empty() && un.is_empty() {
        return "-".into();
    }
    let mut s = money(u["cents"].as_i64().unwrap_or(0), "USD");
    let other: Vec<String> = by
        .iter()
        .filter(|p| p["currency"] != "USD")
        .map(|p| {
            let c = p["currency"].as_str().unwrap_or("");
            format!("{} ≈ {}，{}", money(p["cents"].as_i64().unwrap_or(0), c), money(p["usd_cents"].as_i64().unwrap_or(0), "USD"), rate_cn(c, p["usd_per_unit"].as_f64().unwrap_or(0.0)))
        })
        .collect();
    if !other.is_empty() {
        s.push_str(&format!("（含 {}）", other.join("；")));
    }
    if !un.is_empty() {
        let xs: Vec<String> = un.iter().map(|p| money(p["cents"].as_i64().unwrap_or(0), p["currency"].as_str().unwrap_or(""))).collect();
        s.push_str(&format!("；未折算（没有汇率）：{}", xs.join(" + ")));
    }
    s
}

/// 汇率表：在用的币种在前；来源写自动（数据日期）/ 手动（谁、什么时候）
fn text(v: &Value) -> String {
    let date = v["rates_date"].as_str().unwrap_or("-");
    let mut head = table::kv(&[
        ("自动更新", if v["auto"] == true { format!("每天（{}）", v["url"].as_str().unwrap_or("")) } else { "已在配置里关闭（[fx] auto = false）".into() }),
        ("上次更新", v["fetched_at"].as_str().map_or("从未成功".into(), |t| format!("{}（数据日期 {date}）", table::ts(t)))),
    ]);
    if let Some(e) = v["last_error"].as_str() {
        head.push_str(&format!("\n最近一次更新失败（{}）：{e}；沿用上次的汇率", table::ts(v["last_attempt_at"].as_str().unwrap_or("-"))));
    }
    let rows: Vec<Vec<String>> = v["rates"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|r| r["in_use"] == true || r["source"] == "manual")
        .map(|r| {
            let c = r["currency"].as_str().unwrap_or("");
            let (rate, inv) = match r["usd_per_unit"].as_f64() {
                Some(x) => (rate_cn(c, x), format!("1 USD = {:.4} {c}", 1.0 / x)),
                None => ("未折算".into(), "-".into()),
            };
            let src = match r["source"].as_str() {
                Some("manual") => format!("手动（{} {}）", r["manual"]["by"].as_str().unwrap_or(""), table::ts(r["manual"]["at"].as_str().unwrap_or("-"))),
                Some("auto") => format!("自动（{date}）"),
                Some("base") => "基准".into(),
                _ => "没有汇率".into(),
            };
            vec![c.to_string(), rate, inv, src, if r["in_use"] == true { "在用" } else { "" }.into()]
        })
        .collect();
    let others = v["rates"].as_array().map_or(0, |a| a.iter().filter(|r| r["in_use"] != true && r["source"] != "manual").count());
    let mut s = format!("{head}\n\n{}", table::render(&["币种", "汇率", "反向", "来源", ""], &rows));
    if others > 0 {
        s.push_str(&format!("\n另有 {others} 个没在用的币种有自动汇率（--json 看全部）"));
    }
    if let Some(m) = v["missing"].as_array().filter(|m| !m.is_empty()) {
        let m: Vec<&str> = m.iter().filter_map(Value::as_str).collect();
        s.push_str(&format!("\n在用但没有汇率、费用不折算：{}（fleetctl settings fx <币种> --usd <汇率> 手动设置）", m.join("、")));
    }
    s
}

// `fleetctl settings fx` 的参数（clap 直接解析成它）
#[derive(clap::Args)]
pub struct FxOpts {
    /// 币种代码，如 CNY
    pub currency: Option<String>,
    /// 1 单位该币种合多少 USD，如 0.1404
    #[arg(long)]
    pub usd: Option<f64>,
    /// 1 USD 合多少该币种，如 7.12（和 --usd 二选一）
    #[arg(long)]
    pub per_usd: Option<f64>,
    /// 取消手动汇率，改回自动更新的值
    #[arg(long)]
    pub auto: bool,
    /// 立即从汇率源更新
    #[arg(long)]
    pub refresh: bool,
    #[arg(long)]
    pub yes: bool,
}

/// `settings fx`：不带参数看汇率；`--refresh` 立即更新；`<币种> --usd 0.14` / `--per-usd 7.1` 手动覆盖；`<币种> --auto` 取消覆盖
pub fn fx(ctx: &Ctx, o: FxOpts) -> anyhow::Result<u8> {
    if o.refresh {
        if o.currency.is_some() || o.usd.is_some() || o.per_usd.is_some() || o.auto {
            bail!("--refresh 单独用");
        }
        let d = ctx.c.post(&format!("{PATH}/refresh"), &json!({}))?;
        ctx.emit(&d, || format!("已从汇率源更新\n{}", text(&d)));
        return Ok(0);
    }
    let Some(cur) = o.currency.as_deref().map(|c| c.trim().to_ascii_uppercase()) else {
        if o.usd.is_some() || o.per_usd.is_some() || o.auto {
            bail!("要给币种，如 fleetctl settings fx CNY --usd 0.14");
        }
        let d = ctx.c.get(PATH)?;
        ctx.emit(&d, || text(&d));
        return Ok(0);
    };
    let path = format!("{PATH}/{cur}");
    let d = match (o.usd, o.per_usd, o.auto) {
        (Some(u), None, false) => {
            confirm(&format!("确认把 {cur} 的汇率手动设为 {}？之后自动更新不改它，直到 --auto 取消", rate_cn(&cur, u)), o.yes)?;
            ctx.c.put(&path, &json!({"usd_per_unit": u}))?
        }
        (None, Some(p), false) => {
            confirm(&format!("确认把 {cur} 的汇率手动设为 1 USD = {p} {cur}？之后自动更新不改它，直到 --auto 取消"), o.yes)?;
            ctx.c.put(&path, &json!({"per_usd": p}))?
        }
        (None, None, true) => {
            confirm(&format!("确认取消 {cur} 的手动汇率、改回自动更新的值？"), o.yes)?;
            ctx.c.delete(&path)?
        }
        (None, None, false) => bail!("给 --usd <1 {cur} 合多少 USD>、--per-usd <1 USD 合多少 {cur}> 或 --auto（取消手动）"),
        _ => bail!("--usd、--per-usd、--auto 只能给一个"),
    };
    ctx.emit(&d, || format!("已修改 {cur} 的汇率\n{}", text(&d)));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usd_wording() {
        assert_eq!(rate_cn("CNY", 0.125), "1 CNY = 0.125 USD");
        assert_eq!(rate_cn("JPY", 0.0063671), "1 JPY = 0.006367 USD");
        let s = |cur: &str, cycle: &str, usd: Value, rate: Value| json!({"currency": cur, "cycle": cycle, "monthly_cents": 29999, "monthly_usd_cents": usd, "usd_per_unit": rate});
        assert_eq!(sub_usd_cn(&s("CNY", "monthly", json!(3750), json!(0.125))), "；折合 37.50 USD/月（1 CNY = 0.125 USD）");
        assert_eq!(sub_usd_cn(&s("USD", "monthly", json!(29999), json!(1.0))), "");
        assert_eq!(sub_usd_cn(&s("USD", "quarterly", json!(10000), json!(1.0))), "；折合 100.00 USD/月");
        assert!(sub_usd_cn(&s("XYZ", "monthly", Value::Null, Value::Null)).starts_with("；未折算（没有 XYZ 的汇率"));
        assert_eq!(monthly_cell(&s("CNY", "monthly", json!(3750), json!(0.125))), "37.50 USD（299.99 CNY）");
        assert_eq!(monthly_cell(&s("XYZ", "monthly", Value::Null, Value::Null)), "299.99 XYZ（未折算）");
        assert_eq!(monthly_cell(&Value::Null), "-");
    }

    #[test]
    fn status_cost() {
        let ov = json!({"monthly_usd": {"cents": 100618, "unconverted": [{"currency": "XYZ", "cents": 500}],
                "by_currency": [{"currency": "CNY", "cents": 29999, "usd_cents": 3750, "usd_per_unit": 0.125}, {"currency": "USD", "cents": 96868, "usd_cents": 96868, "usd_per_unit": 1.0}]}});
        assert_eq!(cost_cn(&ov), "1006.18 USD（含 299.99 CNY ≈ 37.50 USD，1 CNY = 0.125 USD）；未折算（没有汇率）：5.00 XYZ");
        assert_eq!(cost_cn(&json!({"monthly_usd": {"cents": 0, "by_currency": [], "unconverted": []}})), "-");
        let sp = json!({"monthly_usd": {"cents": 100, "by_currency": [{"currency": "USD", "cents": 100, "usd_cents": 100, "usd_per_unit": 1.0}], "unconverted": []},
            "monthly_usd_spare": {"count": 7, "cents": 23876, "by_currency": [{"currency": "USD", "cents": 23876, "usd_cents": 23876, "usd_per_unit": 1.0}], "unconverted": []}});
        assert_eq!(cost_cn(&sp), "1.00 USD；另有不续费的备用 7 台 238.76 USD，不在合计里");
        let mut paid = sp.clone();
        paid["paid_machines"] = json!(12);
        assert_eq!(cost_cn(&paid), "1.00 USD（在用 12 台）；另有不续费的备用 7 台 238.76 USD，不在合计里");
    }
}
