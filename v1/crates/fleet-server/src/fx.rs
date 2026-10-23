//! 汇率：计费统一按美元（USD）展示。价格照旧按供应商收的币种录入、存库（套餐、订阅、续费记录），
//! 展示时由视图层按这里的汇率折成 USD（`Subscription.monthly_usd_cents` 等、总览的 `monthly_usd`、团队与套餐的合计），前端不自己折算。
//!
//! 汇率存在 app_setting 的 `fx_rates` 一行（JSON，形状见 `Stored`），不需要迁移：
//! - 自动：每天从公开汇率源拉一次（配置 `[fx] url`，默认 frankfurter 的 ECB 参考汇率、base=USD、不要密钥），`auto` 记「1 USD 合多少该币种」的原值；
//!   拉取失败保留上次的值，记 `last_error` 与日志，下一小时再试；
//! - 手动：每个币种可以手动覆盖（`manual`），覆盖后自动更新不改它，取消覆盖后回到自动的值。手动优先于自动；USD 恒为 1，不能覆盖。
//!
//! 折算口径：`usd_cents = round(原币种分 × usd_per_unit)`，四舍五入到分（.5 远离零）。月费先按周期折成每月（`meta::monthly_cents`，
//! 也是四舍五入到分），再折 USD；合计是各台折好的 USD 相加（和卡片上逐台的数字对得上）。没有汇率的币种不折算、不当 1:1，
//! 在合计里单列（`unconverted`），每台的 `monthly_usd_cents` 为 null。
//!
//! 每次组视图读一次库（一行，视图本身有短缓存）；汇率变了（自动更新、手动改）发 `Event::Settings`，作废视图缓存。

use crate::db::audit;
use crate::error::{ApiError, ApiResult};
use crate::state::{AppState, Event};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use time::{Duration, OffsetDateTime};

pub const KEY: &str = "fx_rates";
/// 折算的目标币种
pub const BASE: &str = "USD";
/// 默认的汇率源：frankfurter（ECB 参考汇率，工作日更新一次），免费、不要密钥
pub const DEFAULT_URL: &str = "https://api.frankfurter.dev/v1/latest?base=USD";
/// 自动更新的间隔；失败后隔 `RETRY` 再试
const EVERY: Duration = Duration::hours(24);
const RETRY: Duration = Duration::hours(1);

/// 手动覆盖的一项
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manual {
    /// 1 单位该币种合多少 USD
    pub usd_per_unit: f64,
    pub by: String,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

/// app_setting `fx_rates` 里存的整份
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stored {
    /// 自动拉到的：币种 → 1 USD 合多少该币种（源里的原值）
    #[serde(default)]
    pub auto: BTreeMap<String, f64>,
    /// 源里的数据日期（ECB 的发布日，如 2026-10-22）
    #[serde(default)]
    pub auto_date: Option<String>,
    /// 最近一次拉取成功的时刻
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub fetched_at: Option<OffsetDateTime>,
    /// 最近一次成功拉取用的地址
    #[serde(default)]
    pub source: Option<String>,
    /// 最近一次尝试（成功或失败）的时刻
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_attempt_at: Option<OffsetDateTime>,
    /// 最近一次尝试失败的原因；成功后清掉
    #[serde(default)]
    pub last_error: Option<String>,
    /// 手动覆盖：币种 → 值
    #[serde(default)]
    pub manual: BTreeMap<String, Manual>,
}

/// 一个币种生效的汇率从哪来
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// USD 本身
    Base,
    Manual,
    Auto,
}

impl Stored {
    /// 生效的汇率（1 单位合多少 USD）与来源：USD 恒为 1；手动覆盖优先；否则自动的；都没有为 None（不折算）
    pub fn rate(&self, currency: &str) -> Option<(f64, Source)> {
        let c = currency.trim().to_ascii_uppercase();
        if c == BASE {
            return Some((1.0, Source::Base));
        }
        if let Some(m) = self.manual.get(&c) {
            return Some((m.usd_per_unit, Source::Manual));
        }
        self.auto.get(&c).filter(|v| valid(**v)).map(|per_usd| (1.0 / per_usd, Source::Auto))
    }

    pub fn usd_per_unit(&self, currency: &str) -> Option<f64> { self.rate(currency).map(|r| r.0) }

    /// 到没到该自动拉取的时候：从没成功过或上次成功满 24 小时，且离上次尝试满 1 小时（失败后每小时重试一次）
    pub fn due(&self, now: OffsetDateTime) -> bool {
        let stale = self.fetched_at.is_none_or(|t| now - t >= EVERY);
        let rested = self.last_attempt_at.is_none_or(|t| now - t >= RETRY);
        stale && rested
    }
}

/// 汇率值是否可用：有限的正数
fn valid(v: f64) -> bool { v.is_finite() && v > 0.0 }

/// 原币种的分折成 USD 分：四舍五入（.5 远离零）
pub fn to_usd_cents(cents: i64, usd_per_unit: f64) -> i64 { (cents as f64 * usd_per_unit).round() as i64 }

/// 折算一笔：(USD 分, 汇率)，没有汇率时都是 None
pub fn convert(fx: &Stored, cents: i64, currency: &str) -> (Option<i64>, Option<f64>) {
    match fx.usd_per_unit(currency) {
        Some(r) => (Some(to_usd_cents(cents, r)), Some(r)),
        None => (None, None),
    }
}

/// 合计里的一个币种
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Part {
    pub currency: String,
    /// 原币种的合计（分）
    pub cents: i64,
    /// 折成的 USD（分，逐台折算后相加）
    pub usd_cents: i64,
    /// 所用汇率：1 单位合多少 USD
    pub usd_per_unit: f64,
}

/// 没有汇率、没折算的币种
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Unconverted {
    pub currency: String,
    pub cents: i64,
}

/// 月费合计（USD）：总览的 `monthly_usd`、团队的 `monthly_usd`
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Total {
    /// 能折算的部分的 USD 合计（分）；不含 `unconverted`
    pub cents: i64,
    /// 各原币种（含 USD）的金额与所用汇率，按币种排
    pub by_currency: Vec<Part>,
    /// 没有汇率的币种：原币种金额单列，不算进 `cents`
    pub unconverted: Vec<Unconverted>,
}

impl Total {
    /// 加一台：原币种的分、折好的 USD 分与汇率（没有汇率为 None）
    pub fn add(&mut self, currency: &str, cents: i64, usd: Option<(i64, f64)>) {
        let c = currency.trim().to_ascii_uppercase();
        match usd {
            Some((u, rate)) => {
                self.cents += u;
                match self.by_currency.iter_mut().find(|p| p.currency == c) {
                    Some(p) => {
                        p.cents += cents;
                        p.usd_cents += u;
                    }
                    None => {
                        self.by_currency.push(Part { currency: c, cents, usd_cents: u, usd_per_unit: rate });
                        self.by_currency.sort_by(|a, b| a.currency.cmp(&b.currency));
                    }
                }
            }
            None => match self.unconverted.iter_mut().find(|p| p.currency == c) {
                Some(p) => p.cents += cents,
                None => {
                    self.unconverted.push(Unconverted { currency: c, cents });
                    self.unconverted.sort_by(|a, b| a.currency.cmp(&b.currency));
                }
            },
        }
    }
}

// ── 读写 ──────────────────────────────────────────────────────────────────

fn parse(v: Value) -> Stored {
    serde_json::from_value(v).unwrap_or_else(|e| {
        // 手工改坏了：当作没有汇率（全部标未折算），不让整个机器视图失败
        tracing::warn!("app_setting 里的 {KEY} 读不了（{e}），按没有汇率处理");
        Stored::default()
    })
}

/// 当前存的汇率（没有时为空：只有 USD 能折算）
pub async fn current<'e, E: sqlx::PgExecutor<'e>>(db: E) -> sqlx::Result<Stored> {
    let v: Option<Value> = sqlx::query_scalar("select value from app_setting where key = $1").bind(KEY).fetch_optional(db).await?;
    Ok(v.map(parse).unwrap_or_default())
}

/// 在事务里锁住这一行读出来（没有就先插一行空的）：手动改与自动更新不互相覆盖
async fn lock(tx: &mut sqlx::PgConnection) -> sqlx::Result<Stored> {
    sqlx::query("insert into app_setting (key, value, updated_by) values ($1, '{}', 'system') on conflict (key) do nothing").bind(KEY).execute(&mut *tx).await?;
    let v: Value = sqlx::query_scalar("select value from app_setting where key = $1 for update").bind(KEY).fetch_one(&mut *tx).await?;
    Ok(parse(v))
}

async fn write(tx: &mut sqlx::PgConnection, s: &Stored, actor: &str) -> sqlx::Result<()> {
    sqlx::query("update app_setting set value = $2, updated_at = now(), updated_by = $3 where key = $1")
        .bind(KEY)
        .bind(json!(s))
        .bind(actor)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// 校验手动给的汇率：有限的正数、不离谱（1 单位合 1e-6–1e6 USD）
pub fn check_rate(v: f64) -> Result<f64, String> {
    if valid(v) && (1e-6..=1e6).contains(&v) {
        Ok(v)
    } else {
        Err(format!("汇率「{v}」不对：写一个正数，如 1 CNY = 0.1404 USD"))
    }
}

/// 设置（`Some`）或取消（`None`）一个币种的手动汇率；写审计 `settings.fx_rate`（`{currency, from:{usd_per_unit, source}|null, to:{usd_per_unit, source}|null}`）。
/// 返回改完的整份；没有变化时什么都不写
pub async fn set_manual(st: &AppState, actor: &str, currency: &str, rate: Option<f64>) -> ApiResult<Stored> {
    let c = crate::meta::currency(currency).map_err(ApiError::bad_request)?;
    if c == BASE {
        return Err(ApiError::bad_request("USD 是折算的目标币种，汇率恒为 1，不能设置"));
    }
    let rate = rate.map(check_rate).transpose().map_err(ApiError::bad_request)?;
    let mut tx = st.db.begin().await?;
    let mut s = lock(&mut tx).await?;
    let was = s.rate(&c);
    match rate {
        Some(r) if s.manual.get(&c).is_some_and(|m| m.usd_per_unit == r) => return Ok(s),
        Some(r) => {
            s.manual.insert(c.clone(), Manual { usd_per_unit: r, by: actor.to_string(), at: OffsetDateTime::now_utc() });
        }
        None if !s.manual.contains_key(&c) => return Ok(s),
        None => {
            s.manual.remove(&c);
        }
    }
    let now = s.rate(&c);
    write(&mut tx, &s, actor).await?;
    let side = |r: Option<(f64, Source)>| r.map(|(v, src)| json!({"usd_per_unit": v, "source": src}));
    audit(&mut tx, actor, "settings.fx_rate", "setting", None, json!({"currency": c, "from": side(was), "to": side(now)})).await?;
    tx.commit().await?;
    Ok(s)
}

/// 汇率源的回包：frankfurter 的 `{base, date, rates}`；也认 open.er-api.com 那样的 `{base_code, time_last_update_utc, rates}`。
/// 只收 base = USD；rates 里的值是「1 USD 合多少该币种」，不是正数的丢掉
pub fn parse_source(v: &Value) -> Result<(BTreeMap<String, f64>, Option<String>), String> {
    let base = v["base"].as_str().or(v["base_code"].as_str()).unwrap_or(BASE);
    if !base.eq_ignore_ascii_case(BASE) {
        return Err(format!("汇率源的基准币种是 {base}，要 USD（地址里带 base=USD）"));
    }
    let rates = v["rates"].as_object().ok_or("汇率源的回包里没有 rates")?;
    let map: BTreeMap<String, f64> = rates
        .iter()
        .filter_map(|(k, x)| Some((crate::meta::currency(k).ok()?, x.as_f64().filter(|f| valid(*f))?)))
        .filter(|(k, _)| k != BASE)
        .collect();
    if map.is_empty() {
        return Err("汇率源的回包里没有可用的汇率".into());
    }
    let date = v["date"].as_str().or(v["time_last_update_utc"].as_str()).map(String::from);
    Ok((map, date))
}

async fn fetch(url: &str) -> anyhow::Result<(BTreeMap<String, f64>, Option<String>)> {
    let c = reqwest::Client::builder().timeout(std::time::Duration::from_secs(20)).user_agent("fleet-server").build()?;
    let res = c.get(url).send().await?;
    let status = res.status();
    if !status.is_success() {
        anyhow::bail!("汇率源回 HTTP {status}");
    }
    let v: Value = res.json().await.map_err(|e| anyhow::anyhow!("汇率源的回包不是 JSON：{e}"))?;
    parse_source(&v).map_err(anyhow::Error::msg)
}

/// 从配置的汇率源拉一次：成功则替换自动的值（手动覆盖的不动）；失败保留上次的值、记下原因。两种情况都发 `Event::Settings`
pub async fn refresh(st: &AppState) -> anyhow::Result<Stored> {
    let url = st.cfg.fx.url.clone();
    let got = fetch(&url).await;
    let now = OffsetDateTime::now_utc();
    let mut tx = st.db.begin().await?;
    let mut s = lock(&mut tx).await?;
    s.last_attempt_at = Some(now);
    let out = match got {
        Ok((rates, date)) => {
            tracing::info!("汇率已更新：{} 个币种（数据日期 {}）", rates.len(), date.as_deref().unwrap_or("-"));
            s.auto = rates;
            s.auto_date = date;
            s.fetched_at = Some(now);
            s.source = Some(url);
            s.last_error = None;
            Ok(())
        }
        Err(e) => {
            tracing::warn!("汇率更新失败（保留上次的值，一小时后再试）：{e:#}");
            s.last_error = Some(format!("{e:#}"));
            Err(e)
        }
    };
    write(&mut tx, &s, "system").await?;
    tx.commit().await?;
    st.emit(Event::Settings);
    out.map(|()| s)
}

/// 后台：每小时看一次，到点（`Stored::due`）就拉。配置里关了自动更新（`[fx] auto = false`）时直接返回
pub async fn run_forever(st: AppState) {
    if !st.cfg.fx.auto {
        tracing::info!("汇率自动更新已在配置里关闭（[fx] auto = false）");
        std::future::pending::<()>().await;
    }
    let mut t = tokio::time::interval(std::time::Duration::from_secs(3600));
    loop {
        t.tick().await;
        match current(&st.db).await {
            Ok(s) if s.due(OffsetDateTime::now_utc()) => {
                // 失败已在 refresh 里记了
                let _ = refresh(&st).await;
            }
            Ok(_) => {}
            Err(e) => tracing::error!("读汇率失败：{e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn stored() -> Stored {
        let mut s = Stored::default();
        s.auto.insert("CNY".into(), 7.0);
        s.auto.insert("EUR".into(), 0.8);
        s
    }

    #[test]
    fn usd_is_one_manual_wins_missing_is_none() {
        let mut s = stored();
        assert_eq!(s.rate("usd"), Some((1.0, Source::Base)));
        assert_eq!(s.rate("EUR"), Some((1.25, Source::Auto)));
        assert_eq!(s.rate("JPY"), None, "没有汇率不当 1:1");
        s.manual.insert("CNY".into(), Manual { usd_per_unit: 0.14, by: "admin".into(), at: OffsetDateTime::UNIX_EPOCH });
        assert_eq!(s.rate("CNY"), Some((0.14, Source::Manual)), "手动覆盖优先于自动");
        // 自动的值是坏的（0、负数）：当没有
        s.auto.insert("HKD".into(), 0.0);
        assert_eq!(s.rate("HKD"), None);
    }

    #[test]
    fn rounding_half_away_from_zero() {
        // ¥299.99 × (1/7) = 42.855… → 42.86
        assert_eq!(to_usd_cents(29999, 1.0 / 7.0), 4286);
        assert_eq!((to_usd_cents(1, 0.5), to_usd_cents(3, 0.5), to_usd_cents(1, 0.4999)), (1, 2, 0));
        assert_eq!(to_usd_cents(96868, 1.0), 96868);
        assert_eq!(convert(&stored(), 100, "XYZ"), (None, None));
        assert_eq!(convert(&stored(), 100, "eur"), (Some(125), Some(1.25)));
    }

    #[test]
    fn total_sums_per_machine_and_lists_unconverted() {
        let s = stored();
        let mut t = Total::default();
        for (cur, cents) in [("CNY", 29999), ("USD", 50000), ("USD", 46868), ("XYZ", 100), ("CNY", 1), ("XYZ", 5)] {
            let (u, r) = convert(&s, cents, cur);
            t.add(cur, cents, u.zip(r));
        }
        // CNY 逐台折：4286 + 0
        assert_eq!(t.cents, 4286 + 96868);
        assert_eq!(t.by_currency, vec![
            Part { currency: "CNY".into(), cents: 30000, usd_cents: 4286, usd_per_unit: 1.0 / 7.0 },
            Part { currency: "USD".into(), cents: 96868, usd_cents: 96868, usd_per_unit: 1.0 },
        ]);
        assert_eq!(t.unconverted, vec![Unconverted { currency: "XYZ".into(), cents: 105 }]);
    }

    #[test]
    fn due_daily_retry_hourly() {
        let now = datetime!(2026-10-23 12:00 UTC);
        assert!(Stored::default().due(now), "从没拉过");
        let ok = Stored { fetched_at: Some(now - Duration::hours(3)), last_attempt_at: Some(now - Duration::hours(3)), ..Default::default() };
        assert!(!ok.due(now));
        let old = Stored { fetched_at: Some(now - Duration::hours(25)), last_attempt_at: Some(now - Duration::hours(25)), ..Default::default() };
        assert!(old.due(now));
        // 刚失败过：一小时后再试
        let failed = Stored { last_attempt_at: Some(now - Duration::minutes(20)), ..old.clone() };
        assert!(!failed.due(now) && failed.due(now + Duration::minutes(41)));
    }

    #[test]
    fn source_formats() {
        let (m, d) = parse_source(&json!({"amount": 1.0, "base": "USD", "date": "2026-10-22", "rates": {"CNY": 6.6743, "EUR": 0.89, "b4d": 1, "JPY": 0, "USD": 1}})).unwrap();
        assert_eq!((m.len(), m["CNY"], d.as_deref()), (2, 6.6743, Some("2026-10-22")));
        let (m, _) = parse_source(&json!({"result": "success", "base_code": "USD", "rates": {"CNY": 7.1}})).unwrap();
        assert_eq!(m["CNY"], 7.1);
        assert!(parse_source(&json!({"base": "EUR", "rates": {"CNY": 7.8}})).unwrap_err().contains("USD"));
        assert!(parse_source(&json!({"base": "USD"})).is_err());
        assert!(parse_source(&json!({"base": "USD", "rates": {}})).is_err());
    }

    #[test]
    fn manual_rate_bounds() {
        assert_eq!(check_rate(0.1404), Ok(0.1404));
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, 1e7] {
            assert!(check_rate(bad).is_err(), "{bad}");
        }
    }
}
