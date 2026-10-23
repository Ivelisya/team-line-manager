//! 三网 PING 的接口：全局目标（设置）、一台线路机的覆盖与读数、卡片的心跳格、抽屉的曲线。逻辑见 `crate::ping`。

use super::ok;
use crate::auth::Actor;
use crate::error::{ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::ping::{self, Global, Override};
use crate::state::AppState;
use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

async fn global_view(st: &AppState) -> ApiResult<Value> {
    let g = ping::global(&st.db).await?;
    Ok(json!({"mode": g.mode, "slots": g.slots, "modes": ping::MODES, "interval_s": ping::INTERVAL_S, "count": ping::COUNT,
              "tcp_default_port": fleet_proto::PING_TCP_DEFAULT_PORT, "defaults": Global::default()}))
}

/// `{mode, slots: [{name, target}] × 8, modes, interval_s, count, tcp_default_port, defaults}`
pub async fn get_targets(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> { Ok(ok(global_view(&st).await?)) }

pub async fn put_targets(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<Global>) -> ApiResult<Json<Value>> {
    ping::save_global(&st, &a.name, b).await?;
    Ok(ok(global_view(&st).await?))
}

pub async fn get_machine(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> { Ok(ok(ping::machine_view(&st.db, id).await?)) }

/// `{override: null | {mode?, slots?: [null | {name, target}]}}`：null 或空的 = 全部跟随全局
#[derive(Deserialize)]
pub struct OverrideIn {
    #[serde(rename = "override")]
    over: Option<Override>,
}

pub async fn put_machine(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<OverrideIn>) -> ApiResult<Json<Value>> {
    ping::save_override(&st, &a.name, id, b.over).await?;
    Ok(ok(ping::machine_view(&st.db, id).await?))
}

pub async fn beats(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> { Ok(ok(ping::beats(&st.db).await?)) }

#[derive(Deserialize)]
pub struct RangeQ {
    range: Option<String>,
}

pub async fn series(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiQuery(q): ApiQuery<RangeQ>) -> ApiResult<Json<Value>> {
    Ok(ok(ping::series(&st.db, id, q.range.as_deref().unwrap_or("24h")).await?))
}
