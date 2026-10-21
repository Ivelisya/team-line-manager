//! 团队线路管理 · IP 情报（离线库查询与接入方式判定）。
//!
//! - 读取器：ip2region xdb（向量索引 + 段内二分）、DB-IP City/ASN Lite（maxminddb，mmap）、iptoasn TSV（mmap + 行偏移二分）。
//!   库都 mmap，不读进堆；堆上只有 iptoasn 的行偏移（约 3 MB）和机房 ASN 集合。
//! - 合成：`Dbs::lookup` → `Intel`（运营商、ASN、组织、国家 / 省 / 市、坐标、接入方式 + 理由 + 置信度、来源库）。
//! - 分类清单：`lists`（内置 `data/asn-lists.toml` + 可选覆盖文件），判定顺序见 `classify`。
//! - 热重载：`IpIntel` 比对库文件 mtime，变了就换一代。库目录缺失或部分缺失时降级，`status()` 里说明。
//!
//! 只查本地离线库，绝不把 IP 发往任何在线服务。状态库、检测器、HTTP 都不在这个 crate 里（在 fleet-server）。
pub mod classify;
pub mod iptoasn;
pub mod lists;
pub mod lookup;
pub mod service;
pub mod xdb;

#[cfg(test)]
mod testutil;

pub use classify::ConnType;
pub use lists::Lists;
pub use lookup::{canonical, distance_km, is_global, Dbs, Intel};
pub use service::{IpIntel, EXPECTED_DBS};
