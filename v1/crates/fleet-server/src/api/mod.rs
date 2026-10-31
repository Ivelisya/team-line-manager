pub mod access;
pub mod account;
pub mod audit;
pub mod batches;
pub mod billing;
pub mod catalog;
pub mod client_subs;
pub mod exec;
pub mod fx;
pub mod incidents;
pub mod ipintel;
pub mod jobs;
pub mod keys;
pub mod lifecycle;
pub mod machines;
pub mod misc;
pub mod notify;
pub mod ping;
pub mod policy;
pub mod probe_points;
pub mod provision;
pub mod recycle;
pub mod relays;
pub mod reports;
pub mod replace;
pub mod repoint;
pub mod router_ops;
pub mod routers;
pub mod settings;
pub mod stream;
pub mod terminal;

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
        // 账户安全（只给会话用户）
        .route("/account/totp", get(account::totp_status))
        .route("/account/totp/setup", post(account::totp_setup))
        .route("/account/totp/enable", post(account::totp_enable))
        .route("/account/totp/disable", post(account::totp_disable))
        .route("/account/totp/recovery-codes", post(account::recovery_regenerate))
        .route("/account/sessions", get(account::sessions))
        .route("/account/sessions/revoke-others", post(account::revoke_other_sessions))
        .route("/account/sessions/{id}", axum::routing::delete(account::revoke_session))
        .route("/account/password", post(account::change_password))
        // 目录
        .route("/teams", get(catalog::list_teams).post(catalog::create_team))
        .route("/teams/{id}", patch(catalog::patch_team).delete(catalog::delete_team))
        .route("/teams/{id}/renumber", get(machines::renumber_preview).post(machines::renumber_apply))
        // 客户批次账与客户到期页：管理接口要登录；公开接口按令牌取（不登录，没有 Actor 提取器），见 batches.rs
        .route("/teams/{id}/batches", get(batches::list).post(batches::create))
        .route("/batches/{id}", patch(batches::patch).delete(batches::delete))
        .route("/batches/{id}/renew", post(batches::renew))
        .route("/batch-periods/{id}", patch(batches::patch_period).delete(batches::delete_period))
        .route("/teams/{id}/customer-page", post(batches::page_create).patch(batches::page_patch).delete(batches::page_delete))
        .route("/public/customer/{token}", get(batches::public_view))
        .route("/providers", get(catalog::list_providers).post(catalog::create_provider))
        .route("/providers/{id}", patch(catalog::patch_provider).delete(catalog::delete_provider))
        .route("/plans", get(catalog::list_plans).post(catalog::create_plan))
        .route("/plans/{id}", patch(catalog::patch_plan).delete(catalog::delete_plan))
        // 机器
        .route("/machines", get(machines::list))
        .route("/machines/{id}", get(machines::get_one).patch(machines::patch))
        .route("/machines/{id}/rename-preview", get(machines::rename_preview))
        .route("/machines/{id}/series", get(machines::series))
        .route("/machines/{id}/availability", get(machines::availability))
        .route("/machines/{id}/reach-quality", get(probe_points::relay_quality))
        // 退役：默认移进备用团队，now = 立即回收（作业）；回收站只读，旧面板的退役机器从这里导入
        .route("/machines/{id}/retire", get(machines::retire_preview).post(machines::retire))
        .route("/recycle-bin", get(recycle::list))
        .route("/recycle-bin/import", post(recycle::import))
        .route("/recycle-bin/{id}", get(recycle::get_one))
        .route("/machines/{id}/upgrade-agent", post(machines::upgrade_agent))
        .route("/machines/{id}/tune", post(machines::tune))
        .route("/machines/{id}/ss-refresh", post(machines::ss_refresh))
        .route("/machines/{id}/speedtest", post(machines::speedtest))
        .route("/agents/upgrade", post(machines::upgrade_all_agents))
        .route("/machines/{id}/subscription", axum::routing::put(machines::put_subscription))
        .route("/machines/{id}/subscription/preview", post(machines::preview_subscription))
        .route("/machines/{id}/renewals", get(machines::list_renewals).post(machines::add_renewal))
        // 待付清单（按供应商 × 到期日）与批量登记续费（一个事务、一条审计，dry_run 只预览）
        .route("/billing/due", get(billing::due))
        .route("/renewals/batch", post(billing::renew_batch))
        .route("/machines/{id}/entries", post(relays::add_entry))
        // 换线路机（单个 / 批量）、搬空线路机：GET 预览（含要改的软路由出口），POST 建作业
        .route("/machines/{id}/move", get(lifecycle::move_preview).post(lifecycle::move_node))
        .route("/moves", get(lifecycle::moves_preview).post(lifecycle::moves))
        .route("/relays/{id}/evacuate", get(lifecycle::evacuate_preview).post(lifecycle::evacuate))
        // 搬迁建议（只读）：按近 7 天的速度重置前会用满的线路机，挑出搬走后能撑到重置日的最少入口
        .route("/relays/{id}/rebalance", get(lifecycle::rebalance))
        // 放置矩阵（只读）：各节点到每台线路机的往返，当前与最优线路机，换线建议
        .route("/placement", get(lifecycle::placement))
        // 替换节点：GET 预览，POST 建作业（改软路由出口 → 交换编号）
        .route("/machines/{id}/replace", get(replace::preview).post(replace::replace))
        // 改出口指向：软路由上指向这个入口的出口改成另一个入口（GET 预览，POST 建作业）
        .route("/entries/{id}/repoint", get(repoint::preview).post(repoint::repoint))
        // 上架
        .route("/provision/check", post(provision::check))
        .route("/provision", post(provision::create))
        // 线路机与入口
        .route("/relays", get(relays::list))
        .route("/entries/{id}/actions", post(relays::entry_action))
        .route("/entries/{id}/routers", get(relays::entry_routers))
        .route("/entries/{id}/share", get(relays::share))
        .route("/entries/share", post(relays::share_batch))
        // 客户端订阅（拉取的 /sub/{令牌} 不登录，挂在根上，见 lib.rs）
        .route("/client-subs", get(client_subs::list).post(client_subs::create))
        .route("/client-subs/{id}", get(client_subs::get_one).patch(client_subs::patch).delete(client_subs::delete))
        .route("/client-subs/{id}/rotate", post(client_subs::rotate))
        // 作业
        .route("/jobs", get(jobs::list))
        .route("/jobs/prune", post(jobs::prune))
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
        // 网页终端（只给会话用户）
        .route("/terminal/tickets", post(terminal::create_ticket))
        .route("/terminal/ws", get(terminal::ws))
        // 总览、告警、审计、实时
        // 访问监控
        .route("/access/summary", get(access::summary))
        .route("/access/targets", get(access::targets))
        .route("/access/sources", get(access::sources))
        .route("/access/events", get(access::events))
        .route("/access/new-targets", get(access::new_targets))
        .route("/machines/{id}/access", get(access::machine))
        // 访问记录保留期
        .route("/settings/access-retention", get(settings::get_access_retention).put(settings::put_access_retention))
        // 汇率（计费折成 USD）
        .route("/settings/fx-rates", get(fx::get_rates))
        .route("/settings/fx-rates/refresh", post(fx::refresh))
        .route("/settings/fx-rates/{currency}", axum::routing::put(fx::put_manual).delete(fx::delete_manual))
        // 来源 IP 画像
        .route("/ipintel", get(ipintel::batch))
        .route("/ipintel/status", get(ipintel::status))
        .route("/ipintel/{ip}", get(ipintel::one))
        .route("/machines/{id}/sources", get(ipintel::machine_sources))
        .route("/meta", get(misc::meta))
        .route("/overview", get(misc::overview))
        .route("/overview/series", get(misc::overview_series))
        // 每日流量（只读）：按组织时区的自然日，全部、各节点、各线路机
        .route("/traffic/daily", get(misc::traffic_daily))
        // 报表：月度可用率汇总（只读）、月报（预览与立即发一次）
        .route("/reports/uptime", get(reports::uptime))
        .route("/reports/monthly", get(reports::monthly))
        .route("/reports/monthly/send", post(reports::send))
        .route("/probe-beats", get(misc::probe_beats))
        .route("/incidents", get(incidents::incidents))
        .route("/incidents/{id}/ack", post(incidents::ack_incident))
        .route("/silences", get(incidents::list_silences).post(incidents::create_silence))
        .route("/silences/{id}", axum::routing::delete(incidents::delete_silence))
        .route("/audit", get(audit::audit))
        .route("/audit/ssh", post(exec::audit_ssh))
        .route("/stream", get(stream::stream))
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
        .route("/policies/versions/{id}/rollback", get(policy::rollback_preview).post(policy::rollback))
        .route("/policies/{scope}/{id}", axum::routing::put(policy::put_one))
        // 软路由（router-bootstrap 是给路由取接入脚本的，不要登录，靠一次性令牌）
        .route("/routers", get(routers::list).post(routers::create))
        .route("/routers/{id}", get(routers::get_one).patch(routers::patch).delete(routers::delete))
        .route("/routers/{id}/metrics", get(routers::metrics))
        .route("/routers/{id}/exits/metrics", get(routers::exit_metrics))
        .route("/routers/{id}/changes", get(routers::changes))
        .route("/routers/{id}/bootstrap", post(routers::bootstrap))
        .route("/routers/{id}/deploy", post(routers::deploy_one))
        .route("/routers/{id}/sync", get(router_ops::sync_preview).post(router_ops::sync))
        .route("/routers/{id}/switch", post(router_ops::switch_to))
        .route("/routers/{id}/switch/back", post(router_ops::switch_back))
        .route("/router-bootstrap/{token}", get(routers::bootstrap_script))
        // 探测点（probe-bootstrap 是给探测点取安装脚本的，不要登录，靠一次性令牌）
        .route("/probe-points", get(probe_points::list).post(probe_points::create))
        .route("/probe-points/{id}", get(probe_points::get_one).patch(probe_points::patch).delete(probe_points::delete))
        .route("/probe-points/{id}/install", post(probe_points::reinstall))
        .route("/probe-points/{id}/upgrade", post(probe_points::upgrade))
        .route("/settings/reach-source", get(probe_points::get_source).put(probe_points::put_source))
        .route("/probe-bootstrap/{token}", get(probe_points::bootstrap_script))
        // 三网 PING（线路机往外测线路质量）
        .route("/settings/ping-targets", get(ping::get_targets).put(ping::put_targets))
        .route("/machines/{id}/ping", get(ping::get_machine).put(ping::put_machine))
        .route("/machines/{id}/ping-series", get(ping::series))
        .route("/ping-beats", get(ping::beats))
}
