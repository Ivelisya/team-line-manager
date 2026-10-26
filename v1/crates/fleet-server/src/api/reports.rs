//! 报表：月度可用率汇总、月报（口径见 reports.rs）

use super::ok;
use crate::auth::Actor;
use crate::error::{ApiJson, ApiQuery, ApiResult};
use crate::reports;
use crate::state::AppState;
use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
pub struct UptimeQ {
    /// `YYYY-MM`，默认本月（算到今天）
    month: Option<String>,
    /// 近几天（含今天，1–90），和 month 二选一
    days: Option<i64>,
}

/// 每个在用节点的出口 / TCP 成功率（出口为主）、断线次数与最长断线、窗口内告警次数，再按供应商汇总（平均、最差，从差到好）
pub async fn uptime(_a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<UptimeQ>) -> ApiResult<Json<Value>> {
    let w = reports::window(&st, q.month.as_deref(), q.days).await?;
    Ok(ok(reports::uptime(&st, &w).await?))
}

#[derive(Deserialize)]
pub struct MonthlyQ {
    /// `YYYY-MM`，默认上个月
    month: Option<String>,
}

pub async fn monthly(_a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<MonthlyQ>) -> ApiResult<Json<Value>> {
    Ok(ok(reports::monthly(&st, q.month.as_deref()).await?))
}

#[derive(Deserialize, Default)]
pub struct SendIn {
    month: Option<String>,
}

/// 立即发一次月报（测试用）：只发给开了「月报」的已启用渠道，写审计
pub async fn send(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<SendIn>) -> ApiResult<Json<Value>> {
    Ok(ok(reports::send(&st, &a.name, b.month.as_deref()).await?))
}
