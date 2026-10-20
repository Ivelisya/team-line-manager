//! fleet-probe：relayd 的逐项探测库。
//!
//! - [`types`]：探测结果与推送 JSON 契约（`fleet.probe.v1`），不依赖运行时，fleetd/health 用 `default-features = false` 引入。
//! - 其余模块（`runtime` 特性）：Shadowsocks AEAD / 2022 客户端、经隧道的 HTTPS、TCP/出口/控制路径/STUN/IPv6 检查、
//!   调度器（抖动、并发上限、HL-04 重试）、边缘 SQLite（24 h）。
//!
//! 语义与旧系统 `komari-relay-probe`（probe.py）+ `monitor.py` 对齐，差异见 WIRING.md。
pub mod types;
pub use types::*;

#[cfg(feature = "runtime")]
pub mod ss;
#[cfg(feature = "runtime")]
pub mod http;
#[cfg(feature = "runtime")]
pub mod checks;
#[cfg(feature = "runtime")]
pub mod stun;
#[cfg(feature = "runtime")]
pub mod targets;
#[cfg(feature = "runtime")]
pub mod scheduler;
#[cfg(feature = "runtime")]
pub mod store;
