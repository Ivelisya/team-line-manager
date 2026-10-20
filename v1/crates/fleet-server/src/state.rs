use crate::config::Config;
use crate::crypto::SecretBox;
use serde::Serialize;
use sqlx::PgPool;
use std::sync::Arc;
use tokio::sync::broadcast;
use uuid::Uuid;

/// 推给浏览器（SSE）的资源变更通知。前端收到后让对应查询失效重取。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Machine { id: Uuid },
    Machines,
    Relay { id: Uuid },
    Job { id: Uuid },
    Incidents,
    Catalog,
    /// 广播滞后、丢了事件：前端收到后让全部查询失效
    Resync,
}

#[derive(Clone)]
pub struct AppState(pub Arc<Inner>);

pub struct Inner {
    pub cfg: Config,
    pub db: PgPool,
    pub secrets: SecretBox,
    /// 浏览器事件
    pub events: broadcast::Sender<Event>,
    /// 线路机期望状态变化（relay_id）：唤醒 agent 的长轮询
    pub relay_changed: broadcast::Sender<Uuid>,
}

impl std::ops::Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner { &self.0 }
}

impl AppState {
    pub fn new(cfg: Config, db: PgPool, secrets: SecretBox) -> Self {
        let (events, _) = broadcast::channel(256);
        let (relay_changed, _) = broadcast::channel(64);
        Self(Arc::new(Inner { cfg, db, secrets, events, relay_changed }))
    }

    pub fn emit(&self, e: Event) { let _ = self.events.send(e); }

    pub fn relay_bumped(&self, relay_id: Uuid) {
        let _ = self.relay_changed.send(relay_id);
        self.emit(Event::Relay { id: relay_id });
    }
}
