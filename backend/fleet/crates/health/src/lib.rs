//! fleet-health：fleetd 的综合健康判定。
//!
//! - [`legacy`]：旧 `monitor.py` 的逐行移植（diagnose / transition / evaluate / availability_windows），
//!   同一份旧输入给出与旧系统相同的状态、原因码与 since（tests/ 下两组对照测试）。
//! - [`engine`]：新架构入口。输入 = 节点意图 + 边缘探测结果（fleet-probe）+ nodeagent 遥测 + 线路机新鲜度
//!   + 入口观测；输出 = 每节点综合状态（ok/degraded/fail/unknown/maintenance）、原因码、since、翻转事件、
//!   需开故障的检查、`correlated_node_failure` 聚合、可用率窗口（1/7/30 d）。
pub mod engine;
pub mod legacy;

pub use engine::{Correlation, Engine, EngineConfig, Inputs, NodeProbes, NodeSpec, Output};
pub use legacy::{Check, Evaluation, Monitor, NodeHealth, Window};
