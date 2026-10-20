//! 团队线路管理 · IP 情报（ARCHITECTURE §3.5 的中控部分）。
//!
//! - 读取器：ip2region xdb（向量索引 + 段内二分）、DB-IP City/ASN Lite（maxminddb，mmap）、iptoasn TSV（mmap + 行偏移二分）。
//!   四个库都 mmap，不读进堆；堆上只有 iptoasn 的行偏移（约 3 MB）和机房 ASN 集合。
//! - 合成：`lookup::Dbs::lookup` → `Intel`（运营商、ASN、组织、国家 / 省 / 市、坐标、接入方式 + 理由、来源库）。
//! - 状态：`store`（独立 SQLite 文件）保存情报缓存、节点来源画像、出口 IP 历史、异常事件。
//! - 检测：新运营商、新城市、一号多地、机房 / VPN 来源、出口 IP 变更（critical，不自动结束）。
//! - 输出：`IpIntel::observations()` 给 fleetd 告警引擎；`api::router()` 给 HTTP。
//!
//! 只查离线库，不把任何 IP 发给在线服务。
pub mod api;
pub mod classify;
pub mod iptoasn;
pub mod legacy;
pub mod lookup;
pub mod mem;
pub mod queries;
pub mod service;
pub mod store;
pub mod xdb;

pub use lookup::{Dbs, Intel};
pub use service::{AlertObs, Config, EgressObs, IngestReport, IpIntel, NodeMeta, SourceEvent, RULES};
