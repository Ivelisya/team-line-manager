//! 面板设置接口：访问记录保留期（逻辑见 `crate::settings`）。GET 要 read，PUT 要 write。

use super::ok;
use crate::auth::Actor;
use crate::error::{ApiError, ApiJson, ApiResult};
use crate::settings::{self, AccessRetention, MAX_DAYS, MIN_DAYS};
use crate::state::{AppState, Event};
use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

/// `{raw_days, hourly_days, min, max, profile_days}`：`profile_days` 是来源画像实际留多少天（不短于 ipintel 学习期，只读）
fn view(st: &AppState) -> Value {
    let r = st.access_retention();
    json!({"raw_days": r.raw_days, "hourly_days": r.hourly_days, "min": MIN_DAYS, "max": MAX_DAYS, "profile_days": st.profile_days()})
}

pub async fn get_access_retention(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> { Ok(ok(view(&st))) }

/// 与 GET 同形，只认 raw_days / hourly_days（其余字段带了也不管）
#[derive(Deserialize)]
pub struct RetentionIn {
    raw_days: i64,
    hourly_days: i64,
}

pub async fn put_access_retention(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<RetentionIn>) -> ApiResult<Json<Value>> {
    let new = AccessRetention::checked(b.raw_days, b.hourly_days).map_err(ApiError::bad_request)?;
    let old = settings::save_access_retention(&st, &a.name, new).await?;
    if old != new {
        st.emit(Event::Settings);
        // 调短了的话超出的马上删，不等每小时的维护；删得多时要一会儿，放到后台，不拖住这个请求
        let s = st.clone();
        tokio::spawn(async move {
            if let Err(e) = settings::enforce(&s).await {
                tracing::error!("按新的访问记录保留期清理失败（每小时的维护会再清）：{e:#}");
            }
        });
    }
    Ok(ok(view(&st)))
}
