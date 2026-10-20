//! 运营商归一与接入方式（住宅 / 移动 / 机房 / VPN·代理）判定。全部是启发式，每条结果都带理由和置信度。
//!
//! 判定顺序（先命中先用）：
//! 1. 非公网地址 → unknown（内网 / 保留），标 `private`。
//! 2. 地址属于我方线路机或落地节点（内部地址）：类型照常判定，只在 flags 里标 `internal`，检测器不把它当可疑来源。
//! 3. 精选列表：VPN_ASN（商业 VPN / 代理出口常用网络）→ vpn；CLOUD_ASN（公有云）→ datacenter；
//!    HOSTING_ASN（IDC / 托管）→ datacenter；MOBILE_ASN（蜂窝专用 ASN）→ mobile。都是 high。
//! 4. AS 名称关键词：移动（5G / LTE / WIRELESS / CELLULAR / MOBILITY）→ mobile high；
//!    VPN / PROXY / ANONYM / TOR → vpn medium；
//! 5. 机房关键词（HOSTING / SERVER / CLOUD / DATACENTER / IDC / VPS / COLO / DEDICATED）命中 AS 名称，
//!    或 ASN 在“iptoasn 机房集合”里（加载时对 iptoasn 全部 AS 描述跑同一组关键词得到，约几千个）→ datacenter medium。
//! 6. 中国三大运营商骨干 / 省网 ASN（家宽与手机共用，离线库没有接入方式字段）→ residential low，`alt=mobile`。
//! 7. 固网运营商列表（AT&T 固网、Comcast、Charter、Telmex…）→ residential medium。
//! 8. 其余 → unknown low。
//!
//! 局限：离线库没有 VPN / Tor 出口清单，VPN 只能靠 ASN 归属，“住宅代理”（在家宽 IP 上跑的代理）无法识别。
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
            ConnType::Vpn => "VPN·代理",
            ConnType::Unknown => "未知",
        }
    }
}

pub const CN_CARRIER: &[(&str, &str)] = &[
    ("电信", "中国电信"),
    ("联通", "中国联通"),
    ("移动", "中国移动"),
    ("广电", "中国广电"),
    ("铁通", "中国铁通"),
    ("教育网", "中国教育网"),
];

pub fn asn_carrier(asn: u32) -> Option<&'static str> {
    Some(match asn {
        4134 | 4809 | 4812 | 4811 | 4847 | 23724 | 4816 | 137266 | 131285 => "中国电信",
        4837 | 4808 | 9929 | 17621 | 17622 | 17623 | 17816 | 10099 => "中国联通",
        9808 | 24445 | 24400 | 24444 | 24547 | 38019 | 56040 | 56041 | 56042 | 56044 | 56046 | 56047 | 56048 | 58453 => "中国移动",
        4538 => "中国教育网",
        7018 => "AT&T",
        20057 => "AT&T Mobility",
        8151 => "Telmex",
        979 => "NetLab Global",
        55430 => "StarHub",
        174 => "Cogent",
        _ => return None,
    })
}

/// 公有云。
pub fn cloud_asn(asn: u32) -> Option<&'static str> {
    Some(match asn {
        16509 | 14618 | 8987 => "Amazon AWS",
        15169 | 396982 | 19527 => "Google",
        8075 | 8068 => "Microsoft Azure",
        13335 => "Cloudflare",
        37963 | 45102 => "阿里云",
        45090 | 132203 => "腾讯云",
        55990 | 136907 => "华为云",
        38365 | 55967 => "百度云",
        31898 => "Oracle Cloud",
        14061 => "DigitalOcean",
        20473 => "Vultr (Choopa)",
        63949 => "Akamai Linode",
        16276 => "OVH",
        24940 => "Hetzner",
        51167 => "Contabo",
        12876 => "Scaleway",
        36351 => "IBM Cloud",
        54113 => "Fastly",
        _ => return None,
    })
}

/// IDC / 主机托管（我方线路机所在的 AS979 NetLab Global 也在这里）。
pub fn hosting_asn(asn: u32) -> Option<&'static str> {
    Some(match asn {
        979 => "NetLab Global",
        21859 => "Zenlayer",
        25820 => "IT7 Networks",
        35916 => "Multacom",
        906 => "DMIT",
        36352 => "ColoCrossing",
        40676 => "Psychz",
        8100 => "QuadraNet",
        46844 => "Sharktech",
        3258 => "xTom",
        60781 | 28753 | 16265 => "Leaseweb",
        23724 | 4816 => "中国电信 IDC",
        53667 => "FranTech (BuyVM)",
        62904 => "Eonix",
        _ => return None,
    })
}

/// 商业 VPN / 代理出口集中的网络。这些 AS 也卖普通主机，但在“客户来源”这个场景里出现几乎都是 VPN / 代理。
pub fn vpn_asn(asn: u32) -> Option<&'static str> {
    Some(match asn {
        9009 => "M247（大量商业 VPN 出口）",
        60068 | 212238 => "Datacamp / CDN77（大量商业 VPN 出口）",
        147049 => "PacketHub（VPN 出口）",
        62371 | 209103 => "Proton AG（ProtonVPN）",
        39351 => "31173 Services（Mullvad VPN）",
        136787 => "TEFINCOM（NordVPN）",
        _ => return None,
    })
}

pub fn mobile_asn(asn: u32) -> Option<&'static str> {
    Some(match asn {
        21928 => "T-Mobile US",
        6167 | 22394 => "Verizon Wireless",
        20057 => "AT&T Mobility",
        17547 => "M1",
        4773 => "Singtel Mobile",
        10091 => "StarHub Mobile",
        28403 => "Telcel",
        137266 => "中国电信湖北 5G（武汉）",
        131285 => "中国电信湖北 5G",
        _ => return None,
    })
}

/// 中国三大运营商骨干网 / 省网：家宽和手机流量共用，离线库分不开。
pub fn cn_mixed_asn(asn: u32) -> bool {
    matches!(
        asn,
        4134 | 4812 | 4837 | 4808 | 17621 | 17622 | 17623 | 17816 | 9808 | 24445 | 24400 | 24444 | 24547 | 38019 | 56040 | 56041 | 56042 | 56044
            | 56046 | 56047 | 56048 | 4809 | 4847 | 10099 | 58453
    )
}

pub fn fixed_asn(asn: u32) -> Option<&'static str> {
    Some(match asn {
        7018 => "AT&T 固网",
        7922 => "Comcast",
        20115 | 11427 | 20001 | 10796 | 11351 | 12271 | 33363 => "Charter",
        701 => "Verizon 固网",
        22773 => "Cox",
        209 => "Lumen",
        5650 => "Frontier",
        7029 => "Windstream",
        6128 => "Optimum",
        30036 => "Mediacom",
        11492 => "Cable One",
        8151 => "Telmex 固网",
        28548 => "Megacable",
        17072 => "Totalplay",
        11888 => "izzi",
        55430 => "StarHub 固网（NGNBN）",
        9506 | 7473 => "Singtel 固网",
        4760 => "HKT",
        9269 => "HKBN",
        2516 => "KDDI",
        4713 => "NTT OCN",
        17676 => "SoftBank BB",
        3462 => "中华电信 HiNet",
        _ => return None,
    })
}

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
    if asn != 0 {
        if let Some(n) = vpn_asn(asn) {
            return c(ConnType::Vpn, "high", format!("AS{asn} {n}"));
        }
        if let Some(n) = cloud_asn(asn) {
            return c(ConnType::Datacenter, "high", format!("AS{asn} 属于云厂商 {n}"));
        }
        if let Some(n) = hosting_asn(asn) {
            return c(ConnType::Datacenter, "high", format!("AS{asn} {n} 是 IDC / 主机托管网络"));
        }
        if let Some(n) = mobile_asn(asn) {
            return c(ConnType::Mobile, "high", format!("AS{asn} 是移动运营商 {n} 的蜂窝网络"));
        }
    }
    if mobile_kw().is_match(names) {
        return c(ConnType::Mobile, "high", format!("AS 名称标明移动网络：{names}"));
    }
    if vpn_kw().is_match(names) {
        return c(ConnType::Vpn, "medium", format!("AS 名称含 VPN / 代理字样：{names}"));
    }
    if !cn_mixed_asn(asn) {
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
    if cn_mixed_asn(asn) {
        let mut x = c(
            ConnType::Residential,
            "low",
            format!("AS{asn}（{}）同时承载家宽和手机流量，离线库没有接入方式，按住宅宽带计", i.carrier),
        );
        x.alt = Some(ConnType::Mobile);
        return x;
    }
    if let Some(n) = fixed_asn(asn) {
        return c(ConnType::Residential, "medium", format!("AS{asn} 是固网运营商（{n}），不在机房名单"));
    }
    c(ConnType::Unknown, "low", if asn == 0 { "查不到 ASN".into() } else { format!("AS{asn} 不在任何分类名单") })
}
