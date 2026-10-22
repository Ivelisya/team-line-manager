//! 运营商归一与接入方式（住宅 / 移动 / 机房 / VPN / 代理）判定。全部是启发式，每条结果都带理由和置信度。
//! ASN 清单不在这里：见 `lists.rs` 和 `data/asn-lists.toml`（内置默认 + 可选覆盖文件）。
//!
//! 判定顺序（先命中先用）：
//! 1. 非公网地址 → unknown（内网 / 保留），标 `private`。
//! 2. 地址属于我方线路机或落地节点（内部地址）：类型照常判定，只在 flags 里标 `internal`，检测器不把它当可疑来源。
//! 3. 精选列表：vpn（商业 VPN / 代理出口常用网络）→ vpn；cloud（公有云）→ datacenter；
//!    hosting（IDC / 托管）→ datacenter；mobile（蜂窝专用 ASN）→ mobile。都是 high。
//! 4. AS 名称关键词：移动（5G / LTE / WIRELESS / CELLULAR / MOBILITY）→ mobile high；
//!    VPN / PROXY / ANONYM / TOR → vpn medium；
//! 5. 机房关键词（HOSTING / SERVER / CLOUD / DATACENTER / IDC / VPS / COLO / DEDICATED）命中 AS 名称，
//!    或 ASN 在“iptoasn 机房集合”里（加载时对 iptoasn 全部 AS 描述跑同一组关键词得到，约几千个）→ datacenter medium。
//! 6. 中国三大运营商骨干 / 省网 ASN（cn_mixed：家宽与手机共用，离线库没有接入方式字段）→ residential low，`alt=mobile`。
//! 7. 固网运营商列表（fixed：AT&T 固网、Comcast、Charter、Telmex…）→ residential medium。
//! 8. 其余 → unknown low。
//!
//! 局限：离线库没有 VPN / Tor 出口清单，VPN 只能靠 ASN 归属，“住宅代理”（在家宽 IP 上跑的代理）无法识别。
use crate::lists::Lists;
use regex::Regex;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnType {
    Residential,
    Mobile,
    Datacenter,
    Vpn,
    Unknown,
}

impl ConnType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConnType::Residential => "residential",
            ConnType::Mobile => "mobile",
            ConnType::Datacenter => "datacenter",
            ConnType::Vpn => "vpn",
            ConnType::Unknown => "unknown",
        }
    }
    pub fn word(&self) -> &'static str {
        match self {
            ConnType::Residential => "住宅宽带",
            ConnType::Mobile => "移动网络",
            ConnType::Datacenter => "机房",
            ConnType::Vpn => "VPN / 代理",
            ConnType::Unknown => "未知",
        }
    }
    pub fn parse(s: &str) -> ConnType {
        match s {
            "residential" => ConnType::Residential,
            "mobile" => ConnType::Mobile,
            "datacenter" => ConnType::Datacenter,
            "vpn" => ConnType::Vpn,
            _ => ConnType::Unknown,
        }
    }
    /// 机房或 VPN：作为客户来源可疑
    pub fn is_suspicious(&self) -> bool {
        matches!(self, ConnType::Datacenter | ConnType::Vpn)
    }
}

/// ip2region 的运营商中文简称 → 全称
pub const CN_CARRIER: &[(&str, &str)] = &[
    ("电信", "中国电信"),
    ("联通", "中国联通"),
    ("移动", "中国移动"),
    ("广电", "中国广电"),
    ("铁通", "中国铁通"),
    ("教育网", "中国教育网"),
];

/// AS 组织名里的中国运营商（ip2region 没写运营商时用）。
pub fn cn_carrier_from_org(org: &str) -> Option<&'static str> {
    let u = org.to_ascii_uppercase();
    if u.contains("CHINA TELECOM") || u.contains("CHINANET") || u.contains("CHINATELECOM") {
        Some("中国电信")
    } else if u.contains("CHINA UNICOM") || u.contains("CHINA169") || u.contains("CNCGROUP") || u.contains("UNICOM") {
        Some("中国联通")
    } else if u.contains("CHINA MOBILE") || u.contains("CMNET") || u.contains("CHINAMOBILE") {
        Some("中国移动")
    } else {
        None
    }
}

pub fn mobile_kw() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)\b5G\b|\b4G\b|\bLTE\b|MOBILE[- ]NETWORK|WIRELESS|CELLULAR|GPRS|MOBILITY").unwrap())
}

pub fn vpn_kw() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)\bVPN\b|\bPROXY\b|ANONYM|\bTOR\b|PRIVATE INTERNET ACCESS|EXPRESSVPN|NORDVPN|SURFSHARK").unwrap())
}

pub fn hosting_kw() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?i)HOSTING|\bHOST\b|SERVER|CLOUD|DATA ?CENT(?:ER|RE)|\bIDC\b|\bVPS\b|\bCOLO|DEDICATED|-SDN\b").unwrap()
    })
}

fn edu_kw() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)研究所|大学|学院|教育网|CERNET|CSTNET|ACADEMY OF SCIENCES|UNIVERSITY").unwrap())
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Classification {
    #[serde(rename = "type")]
    pub conn_type: ConnType,
    /// high | medium | low
    pub confidence: String,
    pub reason: String,
    /// 另一种可能（例如中国运营商混合 ASN 的 mobile）
    pub alt: Option<ConnType>,
}

pub struct ClassifyIn<'a> {
    pub is_global: bool,
    pub is_ula: bool,
    pub internal: Option<&'a str>,
    pub asn: Option<u32>,
    /// AS 组织 / iptoasn 描述 / ip2region 运营商，拼在一起做关键词匹配
    pub names: &'a str,
    pub isp_raw: Option<&'a str>,
    pub carrier: &'a str,
    pub hosting_scan: Option<&'a std::collections::HashSet<u32>>,
    pub lists: &'a Lists,
}

pub fn classify(i: &ClassifyIn) -> Classification {
    let c = |t: ConnType, conf: &str, why: String| Classification { conn_type: t, confidence: conf.into(), reason: why, alt: None };
    if !i.is_global {
        let what = if i.is_ula { "唯一本地（ULA）" } else { "保留 / 内网" };
        return c(ConnType::Unknown, "high", format!("{what}地址，不在公网路由表里"));
    }
    // 我方内部地址（线路机 / 落地节点出口）不改变网络类型，只在 flags 里标 internal，检测器据此跳过
    let _ = i.internal;
    let asn = i.asn.unwrap_or(0);
    let names = i.names;
    let l = i.lists;
    if asn != 0 {
        if let Some(n) = l.vpn_asn(asn) {
            return c(ConnType::Vpn, "high", format!("AS{asn} {n}"));
        }
        if let Some(n) = l.cloud_asn(asn) {
            return c(ConnType::Datacenter, "high", format!("AS{asn} 属于云厂商 {n}"));
        }
        if let Some(n) = l.hosting_asn(asn) {
            return c(ConnType::Datacenter, "high", format!("AS{asn} {n} 是 IDC / 主机托管网络"));
        }
        if let Some(n) = l.mobile_asn(asn) {
            return c(ConnType::Mobile, "high", format!("AS{asn} 是移动运营商 {n} 的蜂窝网络"));
        }
    }
    if mobile_kw().is_match(names) {
        return c(ConnType::Mobile, "high", format!("AS 名称标明移动网络：{names}"));
    }
    if vpn_kw().is_match(names) {
        return c(ConnType::Vpn, "medium", format!("AS 名称含 VPN / 代理字样：{names}"));
    }
    if !l.cn_mixed_asn(asn) {
        if hosting_kw().is_match(names) {
            return c(ConnType::Datacenter, "medium", format!("AS 名称含机房关键词：{names}"));
        }
        if asn != 0 && i.hosting_scan.map(|s| s.contains(&asn)).unwrap_or(false) {
            return c(ConnType::Datacenter, "medium", format!("AS{asn} 在 iptoasn 机房名单里（AS 描述含机房关键词）"));
        }
        if edu_kw().is_match(names) {
            return c(ConnType::Unknown, "medium", format!("教育 / 科研网：{}", i.isp_raw.unwrap_or(names)));
        }
    }
    if l.cn_mixed_asn(asn) {
        let mut x = c(
            ConnType::Residential,
            "low",
            format!("AS{asn}（{}）同时承载家宽和手机流量，离线库没有接入方式，按住宅宽带计", i.carrier),
        );
        x.alt = Some(ConnType::Mobile);
        return x;
    }
    if let Some(n) = l.fixed_asn(asn) {
        return c(ConnType::Residential, "medium", format!("AS{asn} 是固网运营商（{n}），不在机房名单"));
    }
    c(ConnType::Unknown, "low", if asn == 0 { "查不到 ASN".into() } else { format!("AS{asn} 不在任何分类名单") })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn run(l: &Lists, asn: Option<u32>, names: &str, scan: Option<&HashSet<u32>>) -> Classification {
        classify(&ClassifyIn { is_global: true, is_ula: false, internal: None, asn, names, isp_raw: None, carrier: "测试", hosting_scan: scan, lists: l })
    }

    #[test]
    fn decision_order() {
        let l = Lists::builtin();
        // 精选清单优先于名称关键词：M247 的名称里没有 VPN 字样也判 vpn
        let x = run(&l, Some(9009), "M247 Europe SRL", None);
        assert_eq!((x.conn_type, x.confidence.as_str()), (ConnType::Vpn, "high"));
        let x = run(&l, Some(15169), "GOOGLE", None);
        assert_eq!((x.conn_type, x.confidence.as_str()), (ConnType::Datacenter, "high"));
        assert!(x.reason.contains("Google"), "{}", x.reason);
        let x = run(&l, Some(979), "NETLAB", None);
        assert_eq!(x.conn_type, ConnType::Datacenter);
        let x = run(&l, Some(21928), "T-MOBILE-AS21928", None);
        assert_eq!(x.conn_type, ConnType::Mobile);
        // 名称关键词
        assert_eq!(run(&l, Some(64999), "ACME WIRELESS", None).conn_type, ConnType::Mobile);
        let x = run(&l, Some(64999), "Free VPN Service", None);
        assert_eq!((x.conn_type, x.confidence.as_str()), (ConnType::Vpn, "medium"));
        let x = run(&l, Some(64999), "Some Cloud Hosting Ltd", None);
        assert_eq!((x.conn_type, x.confidence.as_str()), (ConnType::Datacenter, "medium"));
        // 机房集合
        let scan: HashSet<u32> = [64998].into_iter().collect();
        assert_eq!(run(&l, Some(64998), "Plain Name", Some(&scan)).conn_type, ConnType::Datacenter);
        assert_eq!(run(&l, Some(64997), "Plain Name", Some(&scan)).conn_type, ConnType::Unknown);
        // 国内混合 ASN：即使名字里有 SERVER 之类关键词也不判机房
        let x = run(&l, Some(4837), "CHINA UNICOM SERVER NETWORK", None);
        assert_eq!((x.conn_type, x.confidence.as_str(), x.alt), (ConnType::Residential, "low", Some(ConnType::Mobile)));
        // 固网
        assert_eq!(run(&l, Some(7922), "COMCAST-7922", None).conn_type, ConnType::Residential);
        // 其余
        assert_eq!(run(&l, Some(64999), "Whatever", None).conn_type, ConnType::Unknown);
        let x = run(&l, None, "", None);
        assert_eq!(x.reason, "查不到 ASN");
    }

    #[test]
    fn private_addresses_are_unknown_high() {
        let l = Lists::builtin();
        let x = classify(&ClassifyIn { is_global: false, is_ula: true, internal: None, asn: Some(15169), names: "GOOGLE", isp_raw: None, carrier: "x", hosting_scan: None, lists: &l });
        assert_eq!((x.conn_type, x.confidence.as_str()), (ConnType::Unknown, "high"));
        assert!(x.reason.contains("ULA"));
    }

    #[test]
    fn overrides_change_the_verdict() {
        let l = Lists::with_overrides_str("[[vpn]]\nasn=[64999]\nname=\"自建 VPN\"\n[[cloud]]\nasn=[15169]\nname=\"\"").unwrap();
        assert_eq!(run(&l, Some(64999), "Whatever", None).conn_type, ConnType::Vpn);
        // Google 被从云清单删掉后，只剩名称关键词（这里名称里没有）→ unknown
        assert_eq!(run(&l, Some(15169), "GOOGLE", None).conn_type, ConnType::Unknown);
    }

    #[test]
    fn carrier_from_org_and_keywords() {
        assert_eq!(cn_carrier_from_org("CHINANET-BACKBONE"), Some("中国电信"));
        assert_eq!(cn_carrier_from_org("China Unicom Beijing"), Some("中国联通"));
        assert_eq!(cn_carrier_from_org("China Mobile Communications"), Some("中国移动"));
        assert_eq!(cn_carrier_from_org("Acme"), None);
        assert!(hosting_kw().is_match("Foo Data Centre Ltd"));
        assert!(!hosting_kw().is_match("Comcast Cable"));
        assert_eq!(ConnType::parse("vpn"), ConnType::Vpn);
        assert_eq!(ConnType::parse("???"), ConnType::Unknown);
    }
}
