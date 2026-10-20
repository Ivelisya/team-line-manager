//! 可热重载的查询入口：持有当前这一代 `Dbs`，库文件（或覆盖清单）的 mtime / 大小变了就重新打开并替换。
//!
//! 库缺失或部分缺失时照常工作：`Dbs::open` 把缺的记进 errors，查询只用剩下的库；`status()` 里说明缺哪个。

use crate::lists::Lists;
use crate::lookup::{Dbs, Intel};
use serde_json::{json, Value};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

/// 完整加载时的库数量：ip2region v4/v6、DB-IP city/asn、iptoasn
pub const EXPECTED_DBS: usize = 5;

#[derive(Clone, Debug)]
struct OverrideState {
    used: bool,
    error: Option<String>,
}

pub struct IpIntel {
    dir: PathBuf,
    overrides: Option<PathBuf>,
    cur: RwLock<Arc<Dbs>>,
    ov: Mutex<OverrideState>,
    sig: Mutex<String>,
}

fn stat(p: &Path) -> String {
    match std::fs::metadata(p) {
        Ok(m) => {
            let mt = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
            format!("{}:{}:{mt}", p.display(), m.len())
        }
        Err(_) => format!("{}:-", p.display()),
    }
}

/// 库目录和覆盖清单的签名：任何一个文件的路径、大小、mtime 变了，签名就变
pub fn signature(dir: &Path, overrides: Option<&Path>) -> String {
    let mut v = vec![stat(&dir.join("ip2region_v4.xdb")), stat(&dir.join("ip2region_v6.xdb")), stat(&dir.join("ip2asn-combined.tsv"))];
    for prefix in ["dbip-city-lite", "dbip-asn-lite"] {
        match crate::lookup::newest(dir, prefix, ".mmdb") {
            Some(p) => v.push(stat(&p)),
            None => v.push(format!("{prefix}:-")),
        }
    }
    if let Some(o) = overrides {
        v.push(stat(o));
    }
    v.join("|")
}

impl IpIntel {
    /// 打开（同步、可能要百毫秒量级：给 iptoasn 建行偏移并扫一遍 AS 描述）。目录不存在也不报错。
    pub fn open(dir: impl Into<PathBuf>, overrides: Option<PathBuf>) -> IpIntel {
        let dir = dir.into();
        let (dbs, ov, sig) = Self::build(&dir, overrides.as_deref());
        IpIntel { dir, overrides, cur: RwLock::new(Arc::new(dbs)), ov: Mutex::new(ov), sig: Mutex::new(sig) }
    }

    fn build(dir: &Path, overrides: Option<&Path>) -> (Dbs, OverrideState, String) {
        let sig = signature(dir, overrides);
        let loaded = Lists::load(overrides);
        if let Some(e) = &loaded.error {
            tracing::warn!("ipintel 覆盖清单无效，退回内置默认：{e}");
        }
        let ov = OverrideState { used: loaded.used, error: loaded.error };
        let dbs = Dbs::open(dir, loaded.lists);
        (dbs, ov, sig)
    }

    pub fn current(&self) -> Arc<Dbs> {
        self.cur.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 缓存键用的版本串（各库版本拼接；一个库都没有时是 "none"）
    pub fn version(&self) -> String {
        self.current().version.clone()
    }

    pub fn loaded(&self) -> usize {
        self.current().loaded()
    }

    pub fn lookup(&self, ip: IpAddr, internal: Option<&str>) -> Intel {
        self.current().lookup(ip, internal)
    }

    /// 库文件或覆盖清单变了就重载，返回是否换了一代。会阻塞（读盘、建索引），在异步代码里用 spawn_blocking。
    pub fn reload_if_changed(&self) -> bool {
        let now = signature(&self.dir, self.overrides.as_deref());
        if *self.sig.lock().unwrap_or_else(|e| e.into_inner()) == now {
            return false;
        }
        let (dbs, ov, sig) = Self::build(&self.dir, self.overrides.as_deref());
        tracing::info!("ipintel 库已重载：加载 {}/{}，版本 {}", dbs.loaded(), EXPECTED_DBS, dbs.version);
        *self.cur.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(dbs);
        *self.ov.lock().unwrap_or_else(|e| e.into_inner()) = ov;
        *self.sig.lock().unwrap_or_else(|e| e.into_inner()) = sig;
        true
    }

    /// 给接口用的状态：各库是否加载、版本 / 日期、条目数、缺什么
    pub fn status(&self) -> Value {
        let d = self.current();
        let ov = self.ov.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let loaded = d.loaded();
        let mut files: Vec<Value> = d
            .files
            .iter()
            .map(|f| {
                let entries: Value = match f.id {
                    "iptoasn" => d.i2a.as_ref().map(|r| json!(r.rows)).unwrap_or(Value::Null),
                    _ => Value::Null,
                };
                json!({"id": f.id, "loaded": true, "path": f.path, "bytes": f.bytes, "mtime": f.mtime, "build": f.build, "license": f.license, "entries": entries})
            })
            .collect();
        // 没加载的库也列出来，前端一眼看出缺哪个
        for id in ["ip2region_v4", "ip2region_v6", "dbip_city", "dbip_asn", "iptoasn"] {
            if !d.files.iter().any(|f| f.id == id) {
                files.push(json!({"id": id, "loaded": false}));
            }
        }
        json!({
            "loaded": loaded,
            "expected": EXPECTED_DBS,
            "installed": loaded > 0,
            "db_dir": self.dir.display().to_string(),
            "version": d.version,
            "files": files,
            "errors": d.errors,
            "hosting_asns": d.hosting_scan.len(),
            "lists": d.lists.counts(),
            "overrides": {
                "path": self.overrides.as_ref().map(|p| p.display().to_string()),
                "used": ov.used,
                "error": ov.error,
            },
            "mem": crate::mem::rss(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{build_xdb, TSV};

    #[test]
    fn missing_dir_degrades_with_status() {
        let d = tempfile::tempdir().unwrap();
        let s = IpIntel::open(d.path().join("not-here"), None);
        assert_eq!(s.loaded(), 0);
        assert_eq!(s.version(), "none");
        let st = s.status();
        assert_eq!(st["installed"], false);
        assert_eq!(st["errors"].as_array().unwrap().len(), 5);
        assert_eq!(st["files"].as_array().unwrap().len(), 5);
        // 没库也能查：只是什么都不知道
        let x = s.lookup("8.8.8.8".parse().unwrap(), None);
        assert!(x.asn.is_none() && x.sources.is_empty());
    }

    #[test]
    fn partial_dir_and_hot_reload() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("ip2asn-combined.tsv"), TSV).unwrap();
        let s = IpIntel::open(d.path(), None);
        assert_eq!(s.loaded(), 1);
        let st = s.status();
        assert_eq!(st["installed"], true);
        assert_eq!(st["errors"].as_array().unwrap().len(), 4);
        assert_eq!(st["files"].as_array().unwrap().iter().find(|f| f["id"] == "iptoasn").unwrap()["entries"], 5);
        assert_eq!(s.lookup("8.8.8.8".parse().unwrap(), None).asn, Some(15169));
        assert!(!s.reload_if_changed(), "没变不重载");
        // 补一个库：签名变了，下一次检查就重载，版本串也变
        let v1 = s.version();
        std::fs::write(
            d.path().join("ip2region_v4.xdb"),
            build_xdb(false, &[("8.8.8.0".parse().unwrap(), "8.8.8.255".parse().unwrap(), "美国|0|0|0|US")]),
        )
        .unwrap();
        assert!(s.reload_if_changed());
        assert_eq!(s.loaded(), 2);
        assert_ne!(s.version(), v1);
        assert_eq!(s.lookup("8.8.8.8".parse().unwrap(), None).country.as_deref(), Some("US"));
    }

    #[test]
    fn overrides_file_is_picked_up_and_bad_file_reported() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("ip2asn-combined.tsv"), TSV).unwrap();
        let ov = d.path().join("ov.toml");
        let s = IpIntel::open(d.path(), Some(ov.clone()));
        assert_eq!(s.status()["overrides"]["used"], false);
        // 把 8.8.8.0 所在的 AS15169 从云清单里删掉、改成 vpn
        std::fs::write(&ov, "[[cloud]]\nasn=[15169]\nname=\"\"\n[[vpn]]\nasn=[15169]\nname=\"测试 VPN\"\n").unwrap();
        assert!(s.reload_if_changed());
        assert_eq!(s.status()["overrides"]["used"], true);
        assert_eq!(s.lookup("8.8.8.8".parse().unwrap(), None).conn_type, crate::classify::ConnType::Vpn);
        std::fs::write(&ov, "[[vpn]\nbroken").unwrap();
        assert!(s.reload_if_changed());
        let st = s.status();
        assert_eq!(st["overrides"]["used"], false);
        assert!(st["overrides"]["error"].as_str().unwrap().contains("ov.toml"));
        // 退回内置默认：15169 又是云厂商
        assert_eq!(s.lookup("8.8.8.8".parse().unwrap(), None).conn_type, crate::classify::ConnType::Datacenter);
    }
}
