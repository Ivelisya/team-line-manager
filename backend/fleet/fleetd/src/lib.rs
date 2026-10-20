//! fleetd：团队线路管理的控制面（中控）：原生旧库喂数与旧形状接口、边缘上报、SSE、告警、上架。

// system.rs 直接调 mimalloc 的 mi_collect / mi_process_info：库本身（含测试二进制）也要链上 mimalloc
extern crate mimalloc as _;
pub mod alerts;
pub mod api;
pub mod app;
pub mod auth;
pub mod cache;
pub mod catalog;
pub mod clock;
pub mod config;
pub mod komari;
pub mod legacy;
pub mod notify;
pub mod probes;
pub mod rules;
pub mod sse;
pub mod store;
pub mod system;
pub mod native;
pub mod accounts;
pub mod relayspec;
pub mod shadow;
pub mod compat;
pub mod legacystore;
pub mod legacyhttp;
pub mod komarinative;
pub mod meter;
pub mod vendor;
pub mod shadowcmp;
pub mod delta;
pub mod cpu;
