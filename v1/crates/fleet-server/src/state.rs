use crate::config::Config;
use crate::crypto::SecretBox;
use axum::extract::ws::Utf8Bytes;
use serde::Serialize;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::{broadcast, Notify};
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
    /// 面板设置变了（目前只有访问记录保留期，见 settings.rs）
    Settings,
    /// 软路由：增删改、接入 / 部署进度、状态与摘要变化（采集每轮最多一次）
    Routers,
    /// 实时指标（`live.rs` 拼好的整条 JSON，前端直接写进缓存；各连接共用同一份字节）；只经 `events.send` 发，不走 `emit`
    #[serde(skip)]
    Metrics(Utf8Bytes),
    /// 有会话或 API 密钥被撤销（登出、撤销会话、退出其它设备、改密码、撤销密钥、命令行重设密码）：
    /// 实时流的每个连接马上复核自己的凭据，失效的以 4401 关闭，不等 60 秒一轮。不推给前端；只经 `revoked()` 发
    #[serde(skip)]
    Revoked,
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
    /// 访问监控的查询（几天原始记录上的统计、模糊搜索，一条可能要几秒）单独一个小池：最多 4 个连接、每条语句 20 秒超时。
    /// 并发多了在这里排队（5 秒等不到回 503），占不满主池——上报入库、机器视图照常；汇总、清理等后台任务仍走主池、不限时
    pub access_db: PgPool,
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
    /// 有新的待发通知了（`notify::enqueue` 写进 notify_outbox 之后）：唤醒发送循环（`notify::run_forever`），不等它兜底重查
    pub notify_wake: Notify,
    /// 发送循环起了没有（每个进程一个，`notify::start`）
    pub notify_started: AtomicBool,
    /// 唤醒作业引擎去领取（`wake_jobs`）；引擎空闲时等它，兜底每 800 ms 重查
    pub jobs_wake: Notify,
    /// 作业引擎在本进程里正在推进的作业（领取时排除）
    pub jobs_running: crate::jobs::Running,
    /// 中控收不到上报的时段：判 agent 离线时不算（视图、告警、作业判线路机在线都用它）
    pub report_gaps: crate::views::ReportGaps,
    /// 访问记录保留期（面板里改的）：读写经 `access_retention()` 等，见 settings.rs
    pub(crate) access_ret: std::sync::RwLock<crate::settings::AccessRetention>,
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
        // 没在面板里设过时的默认值；serve 启动时从库里读（settings::load）
        let access_ret = std::sync::RwLock::new(crate::settings::AccessRetention::default_for(&cfg.retention));
        // 连接选项同主池（测试的 search_path 也在里面），懒连接：用到才建
        let access_db = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .after_connect(|c, _| Box::pin(async move { c.execute("set statement_timeout = '20s'").await.map(|_| ()) }))
            .connect_lazy_with((*db.connect_options()).clone());
        Self(Arc::new(Inner {
            cfg,
            db,
            access_db,
            secrets,
            events,
            relay_changed,
            ipintel,
            access_cache,
            live,
            metrics_dirty: AtomicBool::new(false),
            notify_wake: Notify::new(),
            notify_started: AtomicBool::new(false),
            jobs_wake: Notify::new(),
            jobs_running: Default::default(),
            report_gaps: Default::default(),
            access_ret,
        }))
    }

    pub fn emit(&self, e: Event) {
        // 除密钥、软路由外的资源变更都可能改变机器视图
        if !matches!(e, Event::ApiKeys | Event::Routers) {
            self.live.invalidate();
        }
        if matches!(e, Event::Machine { .. } | Event::Machines) {
            // 访问监控的结果里带机器名与团队：改名、重新编号后不等下一轮汇总
            self.access_cache.invalidate();
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

    /// 会话 / 密钥撤销之后调（事务提交之后）：实时流的连接马上复核，见 `Event::Revoked`
    pub fn revoked(&self) {
        let _ = self.events.send(Event::Revoked);
    }

    /// 有作业可领了（建作业、重试、取消，调用方在事务提交之后调）：唤醒空闲的作业引擎，不等它兜底重查
    pub fn wake_jobs(&self) {
        self.jobs_wake.notify_one();
    }

    pub fn relay_bumped(&self, relay_id: Uuid) {
        let _ = self.relay_changed.send(relay_id);
        self.emit(Event::Relay { id: relay_id });
    }
}
