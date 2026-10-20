// 测试共用：离线库目录来自 IPINTEL_DB_DIR（a395：~/team-ops-eval/ipintel-db，由 deploy/fetch-dbs.sh 下载）。
#![allow(dead_code)]
use std::path::PathBuf;

pub fn db_dir() -> Option<PathBuf> {
    let d = PathBuf::from(std::env::var("IPINTEL_DB_DIR").ok()?);
    if d.join("ip2region_v4.xdb").exists() {
        Some(d)
    } else {
        None
    }
}

pub fn fixtures_dir() -> Option<PathBuf> {
    let d = PathBuf::from(std::env::var("IPINTEL_FIXTURES").ok()?);
    if d.join("health.json").exists() {
        Some(d)
    } else {
        None
    }
}

#[macro_export]
macro_rules! need_dbs {
    () => {
        match common::db_dir() {
            Some(d) => d,
            None => {
                eprintln!("跳过：没有设置 IPINTEL_DB_DIR（离线库目录）");
                return;
            }
        }
    };
}
