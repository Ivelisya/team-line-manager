//! 团队线路管理 · 批量上架流水线（阶段 4）。
//!
//! （DD 重装 →）装 SS → 接入线路机，做成批量、可重跑、可断点续跑的标准流程：
//! - `model`   16 步词表（与原型上架流水线页同形）、状态、失败分类
//! - `intake`  CSV 导入、套餐目录解析、静态校验、自动放置
//! - `planner` 计划（只读快照线路机、影响预览、计划哈希）与建批次
//! - `db`      SQLite saga 日志：rows / steps / step_attempts / events / relay_batches / allocations / secrets / assets
//! - `ssh`     系统 ssh + 钉住的主机公钥 + askpass 密码引导
//! - `steps`   节点步骤 P1–P12、P14–P16（每步先观察后动作，幂等）与补偿
//! - `agent`   P11 装 nodeagent：令牌后端（fleetd 注入 / 命令）、安装包校验、远端脚本（rollout.sh 同款）
//! - `relay`   P13 线路机零重启接入（Xray API）+ 原子落盘 + 补偿；出站校验
//! - `guard`   diff 守卫：线路机配置只许追加本批对象
//! - `engine`  调度：批次租约、并发上限、金丝雀、线路机合批、崩溃恢复、取消
//! - `view`    与原型 demo_batch 同形的批次 JSON、CLI 文字矩阵
//! - `api`     fleetd 的 HTTP 处理函数（框架无关）
pub mod agent;
pub mod api;
pub mod config;
pub mod db;
pub mod engine;
pub mod guard;
pub mod intake;
pub mod model;
pub mod planner;
pub mod relay;
pub mod ssh;
pub mod steps;
pub mod view;
