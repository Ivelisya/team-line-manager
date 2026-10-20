//! ASN 分类清单：内置默认（`data/asn-lists.toml`，include_str!）+ 可选的覆盖文件。
//!
//! 清单是数据不是代码：旧系统把这些写成硬编码的 match，改一个 ASN 要重新编译。
//! 覆盖文件和默认文件同一个写法，按出现顺序合并（后者盖过前者）；`name = ""` 表示把这些 ASN 从该分类里删掉。

use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// 内置默认清单
pub const DEFAULT_TOML: &str = include_str!("../data/asn-lists.toml");

#[derive(Debug, Clone, Deserialize, Default)]
struct Group {
    asn: Vec<u32>,
    #[serde(default)]
    name: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct Raw {
    #[serde(default)]
    carrier: Vec<Group>,
    #[serde(default)]
    cloud: Vec<Group>,
    #[serde(default)]
    hosting: Vec<Group>,
    #[serde(default)]
    vpn: Vec<Group>,
    #[serde(default)]
    mobile: Vec<Group>,
    #[serde(default)]
    fixed: Vec<Group>,
    #[serde(default)]
    cn_mixed: Vec<u32>,
    #[serde(default)]
    cn_mixed_remove: Vec<u32>,
}

/// `Lists::load` 的结果
pub struct Loaded {
    pub lists: Lists,
    /// 覆盖文件存在且已生效
    pub used: bool,
    /// 覆盖文件存在但无效（已退回内置默认）
    pub error: Option<String>,
}

/// 合并后的清单，查询是 O(1)
#[derive(Debug, Clone, Default)]
pub struct Lists {
    pub carrier: HashMap<u32, String>,
    pub cloud: HashMap<u32, String>,
    pub hosting: HashMap<u32, String>,
    pub vpn: HashMap<u32, String>,
    pub mobile: HashMap<u32, String>,
    pub fixed: HashMap<u32, String>,
    pub cn_mixed: HashSet<u32>,
}

fn merge(dst: &mut HashMap<u32, String>, groups: Vec<Group>) {
    for g in groups {
        for a in g.asn {
            if g.name.trim().is_empty() {
                dst.remove(&a);
            } else {
                dst.insert(a, g.name.clone());
            }
        }
    }
}

impl Lists {
    fn apply(&mut self, r: Raw) {
        merge(&mut self.carrier, r.carrier);
        merge(&mut self.cloud, r.cloud);
        merge(&mut self.hosting, r.hosting);
        merge(&mut self.vpn, r.vpn);
        merge(&mut self.mobile, r.mobile);
        merge(&mut self.fixed, r.fixed);
        self.cn_mixed.extend(r.cn_mixed);
        for a in r.cn_mixed_remove {
            self.cn_mixed.remove(&a);
        }
    }

    /// 只有内置默认
    pub fn builtin() -> Lists {
        let r: Raw = toml::from_str(DEFAULT_TOML).expect("内置 ASN 清单应当能解析");
        let mut l = Lists::default();
        l.apply(r);
        l
    }

    /// 内置默认 + 覆盖文本。覆盖文本解析失败时返回错误（调用方退回默认并记录）。
    pub fn with_overrides_str(text: &str) -> Result<Lists, String> {
        let r: Raw = toml::from_str(text).map_err(|e| format!("覆盖清单解析失败：{e}"))?;
        let mut l = Lists::builtin();
        l.apply(r);
        Ok(l)
    }

    /// 内置默认 + 覆盖文件。文件不存在不算错（`used = false`）；存在但读不了或解析失败时退回内置默认，`error` 说明原因。
    pub fn load(overrides: Option<&Path>) -> Loaded {
        let default = |error: Option<String>| Loaded { lists: Lists::builtin(), used: false, error };
        let Some(p) = overrides else { return default(None) };
        let text = match std::fs::read_to_string(p) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return default(None),
            Err(e) => return default(Some(format!("{}: {e}", p.display()))),
        };
        match Lists::with_overrides_str(&text) {
            Ok(lists) => Loaded { lists, used: true, error: None },
            Err(e) => default(Some(format!("{}: {e}", p.display()))),
        }
    }

    pub fn asn_carrier(&self, asn: u32) -> Option<&str> {
        self.carrier.get(&asn).map(String::as_str)
    }
    pub fn cloud_asn(&self, asn: u32) -> Option<&str> {
        self.cloud.get(&asn).map(String::as_str)
    }
    pub fn hosting_asn(&self, asn: u32) -> Option<&str> {
        self.hosting.get(&asn).map(String::as_str)
    }
    pub fn vpn_asn(&self, asn: u32) -> Option<&str> {
        self.vpn.get(&asn).map(String::as_str)
    }
    pub fn mobile_asn(&self, asn: u32) -> Option<&str> {
        self.mobile.get(&asn).map(String::as_str)
    }
    pub fn fixed_asn(&self, asn: u32) -> Option<&str> {
        self.fixed.get(&asn).map(String::as_str)
    }
    pub fn cn_mixed_asn(&self, asn: u32) -> bool {
        self.cn_mixed.contains(&asn)
    }

    /// 各清单条目数（status 展示）
    pub fn counts(&self) -> serde_json::Value {
        serde_json::json!({
            "carrier": self.carrier.len(), "cloud": self.cloud.len(), "hosting": self.hosting.len(), "vpn": self.vpn.len(),
            "mobile": self.mobile.len(), "fixed": self.fixed.len(), "cn_mixed": self.cn_mixed.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_parses_and_matches_old_tables() {
        let l = Lists::builtin();
        assert_eq!(l.asn_carrier(4134), Some("中国电信"));
        assert_eq!(l.asn_carrier(9929), Some("中国联通"));
        assert_eq!(l.cloud_asn(15169), Some("Google"));
        assert_eq!(l.hosting_asn(979), Some("NetLab Global"));
        assert!(l.vpn_asn(9009).unwrap().starts_with("M247"));
        assert_eq!(l.mobile_asn(21928), Some("T-Mobile US"));
        assert_eq!(l.fixed_asn(7922), Some("Comcast"));
        assert!(l.cn_mixed_asn(4837));
        assert!(!l.cn_mixed_asn(23724));
        assert_eq!(l.cn_mixed.len(), 25);
    }

    #[test]
    fn overrides_add_replace_remove() {
        let l = Lists::with_overrides_str(
            r#"
            cn_mixed = [64500]
            cn_mixed_remove = [4134]
            [[vpn]]
            asn = [64511]
            name = "某 VPN"
            [[cloud]]
            asn = [15169]
            name = ""
            [[carrier]]
            asn = [4134]
            name = "电信（改名）"
            "#,
        )
        .unwrap();
        assert_eq!(l.vpn_asn(64511), Some("某 VPN"));
        assert_eq!(l.cloud_asn(15169), None, "name 为空表示删除");
        assert_eq!(l.asn_carrier(4134), Some("电信（改名）"));
        assert!(l.cn_mixed_asn(64500));
        assert!(!l.cn_mixed_asn(4134));
        assert!(l.cn_mixed_asn(4837), "没提到的保持默认");
        assert!(Lists::with_overrides_str("vpn = 3").is_err());
    }

    #[test]
    fn load_missing_and_broken_file() {
        let d = tempfile::tempdir().unwrap();
        let r = Lists::load(Some(&d.path().join("nope.toml")));
        assert!(!r.used && r.error.is_none());
        assert!(r.lists.cloud_asn(16509).is_some());
        let bad = d.path().join("bad.toml");
        std::fs::write(&bad, "[[vpn]]\nasn = \"x\"").unwrap();
        let r = Lists::load(Some(&bad));
        let msg = r.error.unwrap();
        assert!(msg.contains("bad.toml"), "{msg}");
        assert!(!r.used && r.lists.vpn_asn(9009).is_some(), "坏文件退回默认");
    }
}
