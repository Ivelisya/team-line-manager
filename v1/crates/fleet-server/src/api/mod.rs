pub mod access;
pub mod catalog;
pub mod exec;
pub mod ipintel;
pub mod jobs;
pub mod keys;
pub mod lifecycle;
pub mod machines;
pub mod misc;
pub mod notify;
pub mod policy;
pub mod provision;
pub mod relays;

use crate::state::AppState;
use axum::routing::{get, patch, post};
use axum::Router;
use serde::Serialize;
use serde_json::{json, Value};

/// `{"data": …}`
pub fn ok<T: Serialize>(v: T) -> axum::Json<Value> { axum::Json(json!({ "data": v })) }

/// 分页列表的统一形状 `{items, total, next_offset}`：`next_offset` 只在后面还有数据时给出
pub fn paged<T: Serialize>(items: Vec<T>, total: i64, offset: i64) -> axum::Json<Value> {
    let end = offset + items.len() as i64;
    ok(json!({ "items": items, "total": total, "next_offset": (end < total).then_some(end) }))
}

/// PATCH 的可清空字段：`#[serde(default, deserialize_with = "super::some")]` 让没传 = None、传 null = Some(None)
/// （不加的话 serde 把 null 也当成没传，清不掉）
pub fn some<'de, T: serde::Deserialize<'de>, D: serde::Deserializer<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

pub fn router() -> Router<AppState> {
    use crate::auth;
    Router::new()
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/me", get(auth::me))
        // 目录
        .route("/teams", get(catalog::list_teams).post(catalog::create_team))
        .route("/teams/{id}", patch(catalog::patch_team))
        .route("/teams/{id}/renumber", get(machines::renumber_preview).post(machines::renumber_apply))
        .route("/providers", get(catalog::list_providers).post(catalog::create_provider))
        .route("/providers/{id}", patch(catalog::patch_provider))
        .route("/plans", get(catalog::list_plans).post(catalog::create_plan))
        .route("/plans/{id}", patch(catalog::patch_plan))
        // 机器
        .route("/machines", get(machines::list))
        .route("/machines/{id}", get(machines::get_one).patch(machines::patch))
        .route("/machines/{id}/series", get(machines::series))
        .route("/machines/{id}/availability", get(machines::availability))
        .route("/machines/{id}/retire", get(machines::retire_preview).post(machines::retire))
        .route("/machines/{id}/purge", post(machines::purge))
        .route("/machines/{id}/upgrade-agent", post(machines::upgrade_agent))
        .route("/agents/upgrade", post(machines::upgrade_all_agents))
        .route("/machines/{id}/subscription", axum::routing::put(machines::put_subscription))
        .route("/machines/{id}/renewals", get(machines::list_renewals).post(machines::add_renewal))
        .route("/machines/{id}/entries", post(relays::add_entry))
        // 换线路机、搬空线路机、恢复已退役的机器
        .route("/machines/{id}/move", post(lifecycle::move_node))
        .route("/relays/{id}/evacuate", post(lifecycle::evacuate))
        .route("/machines/{id}/restore", get(lifecycle::restore_preview).post(lifecycle::restore))
        // 上架
        .route("/provision/check", post(provision::check))
        .route("/provision", post(provision::create))
        // 线路机与入口
        .route("/relays", get(relays::list))
        .route("/entries/{id}/actions", post(relays::entry_action))
        .route("/entries/{id}/share", get(relays::share))
        // 作业
        .route("/jobs", get(jobs::list))
        .route("/jobs/{id}", get(jobs::get_one))
        .route("/jobs/{id}/logs", get(jobs::logs))
        .route("/jobs/{id}/retry", post(jobs::retry))
        .route("/jobs/{id}/cancel", post(jobs::cancel))
        // 运维：API 密钥、远程执行、日志
        .route("/api-keys", get(keys::list).post(keys::create))
        .route("/api-keys/{id}", axum::routing::delete(keys::revoke))
        .route("/machines/{id}/exec", post(exec::exec))
        .route("/machines/{id}/logs", get(exec::machine_logs))
        .route("/server/logs", get(exec::server_logs))
        // 总览、告警、审计、实时
        // 访问监控
        .route("/access/summary", get(access::summary))
        .route("/access/targets", get(access::targets))
        .route("/access/sources", get(access::sources))
        .route("/access/events", get(access::events))
        .route("/access/new-targets", get(access::new_targets))
        .route("/machines/{id}/access", get(access::machine))
        // 来源 IP 画像
        .route("/ipintel", get(ipintel::batch))
        .route("/ipintel/status", get(ipintel::status))
        .route("/ipintel/{ip}", get(ipintel::one))
        .route("/machines/{id}/sources", get(ipintel::machine_sources))
        .route("/overview", get(misc::overview))
        .route("/overview/series", get(misc::overview_series))
        .route("/probe-beats", get(misc::probe_beats))
        .route("/incidents", get(misc::incidents))
        .route("/incidents/{id}/ack", post(misc::ack_incident))
        .route("/silences", get(misc::list_silences).post(misc::create_silence))
        .route("/silences/{id}", axum::routing::delete(misc::delete_silence))
        .route("/audit", get(misc::audit))
        .route("/stream", get(misc::stream))
        // 告警通知渠道
        .route("/notify-channels", get(notify::list).post(notify::create))
        .route("/notify-channels/{id}", patch(notify::patch).delete(notify::delete))
        .route("/notify-channels/{id}/test", post(notify::test))
        .route("/notify-log", get(notify::log))
        // 访问策略
        .route("/policies", get(policy::list))
        .route("/policies/preview", post(policy::preview))
        .route("/policies/publish", post(policy::publish))
        .route("/policies/discard", post(policy::discard))
        .route("/policies/versions", get(policy::versions))
        .route("/policies/versions/{id}/rollback", post(policy::rollback))
        .route("/policies/{scope}/{id}", axum::routing::put(policy::put_one))
}
