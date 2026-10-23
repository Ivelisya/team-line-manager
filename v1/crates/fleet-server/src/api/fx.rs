//! 汇率设置接口（逻辑见 `crate::fx`）：GET 要 read；手动覆盖、取消、立即更新要 write。

use super::ok;
use crate::auth::Actor;
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::fx::{self, Stored, BASE};
use crate::state::{AppState, Event};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeSet;

/// 在用的币种：没归档的套餐、没退役的机器的订阅（生效值）
async fn in_use(st: &AppState) -> sqlx::Result<BTreeSet<String>> {
    let v: Vec<String> = sqlx::query_scalar(
        "select currency from plan where archived_at is null
         union select e.currency from subscription_eff e join machine m on m.id = e.machine_id where m.lifecycle <> 'retired'",
    )
    .fetch_all(&st.db)
    .await?;
    Ok(v.into_iter().map(|c| c.trim().to_ascii_uppercase()).collect())
}

/// 整份汇率设置（折算凭据）：每个币种生效的汇率、来源、自动与手动各自的值；在用的币种排前面
fn view(st: &AppState, s: &Stored, used: &BTreeSet<String>) -> Value {
    let mut all: BTreeSet<String> = used.clone();
    all.extend(s.auto.keys().cloned());
    all.extend(s.manual.keys().cloned());
    all.insert(BASE.into());
    let mut rows: Vec<(bool, String)> = all.into_iter().map(|c| (!used.contains(&c), c)).collect();
    rows.sort();
    let rates: Vec<Value> = rows
        .into_iter()
        .map(|(_, c)| {
            let r = s.rate(&c);
            json!({
                "currency": c,
                "usd_per_unit": r.map(|x| x.0),
                "per_usd": r.map(|x| 1.0 / x.0),
                "source": r.map(|x| x.1),
                "auto_per_usd": s.auto.get(&c),
                "manual": s.manual.get(&c),
                "in_use": used.contains(&c),
            })
        })
        .collect();
    json!({
        "base": BASE,
        "auto": st.cfg.fx.auto,
        "url": st.cfg.fx.url,
        "fetched_at": s.fetched_at.map(crate::meta::fmt_ts),
        "rates_date": s.auto_date,
        "source": s.source,
        "last_attempt_at": s.last_attempt_at.map(crate::meta::fmt_ts),
        "last_error": s.last_error,
        "rates": rates,
        "missing": used.iter().filter(|c| s.rate(c).is_none()).collect::<Vec<_>>(),
    })
}

async fn full(st: &AppState, s: &Stored) -> ApiResult<Value> { Ok(view(st, s, &in_use(st).await?)) }

pub async fn get_rates(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let s = fx::current(&st.db).await?;
    Ok(ok(full(&st, &s).await?))
}

/// 手动汇率：`usd_per_unit`（1 单位合多少 USD）与 `per_usd`（1 USD 合多少该币种）给一个
#[derive(Deserialize)]
pub struct ManualIn {
    usd_per_unit: Option<f64>,
    per_usd: Option<f64>,
}

pub async fn put_manual(a: Actor, State(st): State<AppState>, ApiPath(currency): ApiPath<String>, ApiJson(b): ApiJson<ManualIn>) -> ApiResult<Json<Value>> {
    let rate = match (b.usd_per_unit, b.per_usd) {
        (Some(r), None) => r,
        (None, Some(p)) => fx::check_rate(p).map(|p| 1.0 / p).map_err(ApiError::bad_request)?,
        _ => return Err(ApiError::bad_request("给 usd_per_unit（1 单位合多少 USD）或 per_usd（1 USD 合多少该币种）其中一个")),
    };
    let s = fx::set_manual(&st, &a.name, &currency, Some(rate)).await?;
    st.emit(Event::Settings);
    Ok(ok(full(&st, &s).await?))
}

/// 取消手动汇率：回到自动更新的值（没有自动的就是未折算）
pub async fn delete_manual(a: Actor, State(st): State<AppState>, ApiPath(currency): ApiPath<String>) -> ApiResult<Json<Value>> {
    let s = fx::set_manual(&st, &a.name, &currency, None).await?;
    st.emit(Event::Settings);
    Ok(ok(full(&st, &s).await?))
}

/// 立即从汇率源更新一次（不等每天的自动更新；配置里关了自动更新也能点）。失败回 502，保留上次的值
pub async fn refresh(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    match fx::refresh(&st).await {
        Ok(s) => Ok(ok(full(&st, &s).await?)),
        Err(e) => Err(ApiError::new(StatusCode::BAD_GATEWAY, "fx_fetch_failed", format!("汇率更新失败（保留上次的值）：{e:#}"))),
    }
}
