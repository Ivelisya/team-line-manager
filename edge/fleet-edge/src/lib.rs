//! fleet-edge：nodeagent 与 relayd 共用的库。
//!
//! - `hostmon`：/proc + statvfs 采样（只读）
//! - `outbox`：有界磁盘环形队列（≤ 2 MB，满了丢最旧并计数）
//! - `push`：HTTPS 上报（rustls + webpki 根证书，指数退避 + 抖动）
//! - `runner`：单线程主循环（sigtimedwait 当定时器，SIGTERM 优雅退出）
//! - `xray`：relayd 的 Xray StatsService 只读客户端（QueryStats，reset=false），进程与连接统计
//! - `probe`：relayd 内嵌的 L1/L2 探测（fleet-probe），结果放在批次顶层 "probe"
//! - `accesslog`：Xray 访问日志 tail（fleet.access.v1），批次顶层 "access"
//! - `manage`：管理模式（fleet.relayspec.v1），从 fleetd 拉期望状态、经 Xray API 生效、回执
//! - `child`：短命子进程（xray CLI）带超时运行、持续读输出、保证回收（seccomp 下不能 kill）
//! - `policy`：旧 policy.py 的逐字移植（访问规则编译与 digest），供对照与测试
pub mod accesslog;
pub mod child;
pub mod config;
pub mod hostmon;
pub mod log;
pub mod manage;
pub mod outbox;
pub mod pathcheck;
pub mod policy;
pub mod probe;
pub mod push;
pub mod relay;
pub mod runner;
pub mod util;
pub mod xray;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// fleetd 在上报回执里给的“有人在看”提示（`"live": true`）：到这个 unix 秒之前按实时节奏跑（全链路检查 60 s）
pub static LIVE_UNTIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
