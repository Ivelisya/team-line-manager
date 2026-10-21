use crate::config::Config;
use crate::crypto::SecretBox;
use axum::extract::ws::Utf8Bytes;
use serde::Serialize;
use sqlx::PgPool;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::broadcast;
use uuid::Uuid;

/// 推给浏览器（WebSocket）的消息。除 `Metrics` 外都是资源变更通知，前端收到后让对应查询失效重取。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Machine { id: Uuid },
    /// 作业推进带来的机器变化：前端只重取这台机器的详情；列表里的 active_job 由 metrics 推送
    MachineJob { id: Uuid },
    Machines,
    Relay { id: Uuid },
    /// 作业变化。`target` 不发给前端：emit 时顺带发目标机器的 MachineJob 事件。
    /// 改了机器生命周期的地方（上架、退役、恢复、取消补偿、作业结束）另发 Machines
    Job {
        id: Uuid,
        #[serde(skip)]
        target: Option<Uuid>,
    },
    Incidents,
    Catalog,
    /// 广播滞后、丢了事件：前端收到后让全部查询失效
    Resync,
    ApiKeys,
    /// 访问策略的草稿/版本变化
    Policies,
    /// 实时指标（`live.rs` 拼好的整条 JSON，前端直接写进缓存；各连接共用同一份字节）；只经 `events.send` 发，不走 `emit`
    #[serde(skip)]
    Metrics(Utf8Bytes),
}

impl Event {
    pub fn to_text(&self) -> Utf8Bytes {
        match self {
            Event::Metrics(s) => s.clone(),
            e => serde_json::to_string(e).unwrap_or_default().into(),
        }
    }
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
    /// 来源 IP 画像：离线库 + 内部地址表（库缺失时降级）
    pub ipintel: crate::ipintel::Service,
    /// 访问监控聚合结果的短 TTL 缓存
    pub access_cache: crate::access::QueryCache,
    /// 在用机器视图的短缓存（列表、线路机、总览、实时指标共用）
    pub live: crate::views::LiveCache,
    /// 有新上报、实时指标该重新比对了（`live::run_forever` 消费）
    pub metrics_dirty: AtomicBool,
    /// 告警通知发送队列，每个渠道一个、有容量上限（`notify::enqueue` 第一次给某渠道入队时建好并启动它的发送任务；
    /// 渠道被删除或停用后，发送任务发现时把自己移出这里并退出）
    pub notify_queues: std::sync::Mutex<std::collections::HashMap<Uuid, crate::notify::Queue>>,
}

impl std::ops::Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner { &self.0 }
}

impl AppState {
    pub fn new(cfg: Config, db: PgPool, secrets: SecretBox) -> Self {
        let (events, _) = broadcast::channel(256);
        let (relay_changed, _) = broadcast::channel(64);
        let ipintel = crate::ipintel::Service::new(&cfg.ipintel);
        let access_cache = crate::access::QueryCache::new(std::time::Duration::from_secs(cfg.access_cache_secs));
        let live = crate::views::LiveCache::default();
        Self(Arc::new(Inner { cfg, db, secrets, events, relay_changed, ipintel, access_cache, live, metrics_dirty: AtomicBool::new(false), notify_queues: Default::default() }))
    }

    pub fn emit(&self, e: Event) {
        // 除密钥外的资源变更都可能改变机器视图
        if !matches!(e, Event::ApiKeys) {
            self.live.invalidate();
        }
        if let Event::Job { target: Some(t), .. } = e {
            let _ = self.events.send(Event::MachineJob { id: t });
            // 列表卡片的 active_job 走实时指标比对（最多 3 s 一次），不让每一步都整表重取
            self.metrics_dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if matches!(e, Event::Incidents) {
            // 静默增删、告警开关：卡片上的「维护中」、健康同样走实时指标比对，前端不为它整表重取
            self.metrics_dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let _ = self.events.send(e);
    }

    pub fn relay_bumped(&self, relay_id: Uuid) {
        let _ = self.relay_changed.send(relay_id);
        self.emit(Event::Relay { id: relay_id });
    }
}
