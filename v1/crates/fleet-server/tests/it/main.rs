//! fleet-server 的集成测试：一个测试目标、一次链接（见 common）。只跑某一套：`cargo test -p fleet-server --test it lifecycle::`。
//! 改进程 PATH 放假 ssh 的 replace、routers、upgrade、provision 不在这里，各自单独一个目标。

mod common;

mod access;
mod account;
mod billing;
mod flow;
mod fx;
mod incident;
mod ip_nodes;
mod ipintel;
mod jobs;
mod lifecycle;
mod ops;
mod ping;
mod placement;
mod policy;
mod probe;
mod reports;
mod retention;
mod reverse;
mod spare;
mod subs;
mod telemetry;
mod terminal;
mod traffic;
