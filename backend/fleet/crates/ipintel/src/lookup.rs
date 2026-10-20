//! 四个离线库合成一条 IP 情报。
//!
//! 优先级（与原型 tools/ipintel_remote.py 相同口径）：
//! - **国家**：ip2region 的国家代码 → DB-IP City → iptoasn 注册国家。
//! - **省 / 市**：中国地址（country = CN）只用 ip2region（运营商级的国内数据，省市准）；境外地址用 DB-IP City
//!   （有州 / 城市与坐标），DB-IP 缺省时才退回 ip2region。**同一条结果不混用两库的省和市**。
//! - **ASN / AS 组织**：DB-IP ASN Lite（月更）→ iptoasn（日更，补空）；两库 ASN 不一致时标 `asn_conflict`。
//! - **运营商 isp**：ip2region 的“电信 / 联通 / 移动…”归一成“中国电信 / 中国联通 / 中国移动…” → 已知 ASN 的运营商名
//!   → AS 组织短名 → ip2region 原文 → iptoasn 描述。
//! - **坐标**：中国地址：ip2region 的市在小型地名表里 → 用地名表；否则 DB-IP 的省与 ip2region 一致时用 DB-IP 坐标；
//!   否则省会坐标。境外：DB-IP 坐标。`coord_basis` 标明来源。坐标只用于“两地相距多远”，精度到城市级。
//! - **接入方式**：见 classify.rs。
use crate::classify::{self, Classification, ClassifyIn};
use crate::iptoasn::{AsnRow, IpToAsn};
use crate::xdb::{parse_region, Region, Xdb};
use maxminddb::Reader;
use memmap2::Mmap;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Intel {
    pub ip: String,
    pub v: u8,
    pub asn: Option<u32>,
    pub as_org: Option<String>,
    pub isp: String,
    pub isp_basis: String,
    pub country: Option<String>,
    pub country_name: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
    pub geo_basis: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub coord_basis: Option<String>,
    #[serde(rename = "type")]
    pub conn_type: classify::ConnType,
    pub type_word: String,
    pub type_confidence: String,
    pub type_reason: String,
    pub type_alt: Option<classify::ConnType>,
    pub internal: Option<String>,
    pub flags: Vec<String>,
    pub sources: Vec<String>,
    pub dbs: DbRaw,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct DbRaw {
    pub ip2region: Option<Region>,
    pub dbip_city: Option<DbipCity>,
    pub dbip_asn: Option<DbipAsn>,
    pub iptoasn: Option<AsnRow>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct DbipCity {
    pub country: Option<String>,
    pub country_name: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct DbipAsn {
    pub asn: Option<u32>,
    pub org: Option<String>,
}

// ── mmdb 里要的字段（自定义 serde 结构，跨 maxminddb 版本稳定） ──
#[derive(Deserialize, Default)]
struct MNames<'a> {
    #[serde(borrow, default)]
    en: Option<&'a str>,
    #[serde(rename = "zh-CN", borrow, default)]
    zh: Option<&'a str>,
}
#[derive(Deserialize, Default)]
struct MPlace<'a> {
    #[serde(borrow, default)]
    iso_code: Option<&'a str>,
    #[serde(borrow, default)]
    names: Option<MNames<'a>>,
}
#[derive(Deserialize, Default)]
struct MLoc {
    #[serde(default)]
    latitude: Option<f64>,
    #[serde(default)]
    longitude: Option<f64>,
}
#[derive(Deserialize, Default)]
struct MCity<'a> {
    #[serde(borrow, default)]
    country: Option<MPlace<'a>>,
    #[serde(borrow, default)]
    subdivisions: Option<Vec<MPlace<'a>>>,
    #[serde(borrow, default)]
    city: Option<MPlace<'a>>,
    #[serde(default)]
    location: Option<MLoc>,
}
#[derive(Deserialize, Default)]
struct MAsn<'a> {
    #[serde(default)]
    autonomous_system_number: Option<u32>,
    #[serde(borrow, default)]
    autonomous_system_organization: Option<&'a str>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DbFile {
    pub id: &'static str,
    pub path: String,
    pub bytes: u64,
    pub mtime: i64,
    pub build: Option<String>,
    pub license: &'static str,
}

pub struct Dbs {
    pub x4: Option<Xdb>,
    pub x6: Option<Xdb>,
    pub city: Option<Reader<Mmap>>,
    pub asn: Option<Reader<Mmap>>,
    pub i2a: Option<IpToAsn>,
    pub hosting_scan: HashSet<u32>,
    pub files: Vec<DbFile>,
    /// 各库版本拼成的串，写进缓存行；库换版后旧缓存按版本失效
    pub version: String,
    pub errors: Vec<String>,
}

fn file_meta(p: &Path) -> (u64, i64) {
    std::fs::metadata(p)
        .map(|m| {
            let mt = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0);
            (m.len(), mt)
        })
        .unwrap_or((0, 0))
}

/// 目录里按前缀找最新的文件（dbip-city-lite-YYYY-MM.mmdb 取字典序最大；也接受不带日期的固定名）。
fn newest(dir: &Path, prefix: &str, suffix: &str) -> Option<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            n.starts_with(prefix) && n.ends_with(suffix)
        })
        .collect();
    v.sort();
    v.pop()
}

impl Dbs {
    /// 打开目录里的四个库。缺哪个就跳过哪个（记进 errors），查询照常用剩下的库。
    pub fn open(dir: &Path) -> Dbs {
        let mut errors = Vec::new();
        let mut files = Vec::new();
        let mut ver = Vec::new();
        let mut x = |id: &'static str, name: &str| -> Option<Xdb> {
            let p = dir.join(name);
            match Xdb::open(&p) {
                Ok(x) => {
                    let (b, m) = file_meta(&p);
                    ver.push(format!("{id}:{}", x.created_at));
                    files.push(DbFile { id, path: p.display().to_string(), bytes: b, mtime: m, build: Some(x.created_at.to_string()), license: "Apache-2.0" });
                    Some(x)
                }
                Err(e) => {
                    errors.push(e);
                    None
                }
            }
        };
        let x4 = x("ip2region_v4", "ip2region_v4.xdb");
        let x6 = x("ip2region_v6", "ip2region_v6.xdb");
        let mut mm = |id: &'static str, prefix: &str| -> Option<Reader<Mmap>> {
            let Some(p) = newest(dir, prefix, ".mmdb") else {
                errors.push(format!("{}: 找不到 {prefix}*.mmdb", dir.display()));
                return None;
            };
            // SAFETY: fetch-dbs.sh 用 rename 原子替换，映射着的旧文件不会被改写或截断。
            match unsafe { Reader::open_mmap(&p) } {
                Ok(r) => {
                    let (b, m) = file_meta(&p);
                    ver.push(format!("{id}:{}", r.metadata().build_epoch));
                    files.push(DbFile { id, path: p.display().to_string(), bytes: b, mtime: m, build: Some(r.metadata().build_epoch.to_string()), license: "CC BY 4.0（IP Geolocation by DB-IP）" });
                    Some(r)
                }
                Err(e) => {
                    errors.push(format!("{}: {e}", p.display()));
                    None
                }
            }
        };
        let city = mm("dbip_city", "dbip-city-lite");
        let asn = mm("dbip_asn", "dbip-asn-lite");
        let i2a_path = dir.join("ip2asn-combined.tsv");
        let i2a = match IpToAsn::open(&i2a_path) {
            Ok(r) => {
                let (b, m) = file_meta(&i2a_path);
                ver.push(format!("iptoasn:{m}"));
                files.push(DbFile { id: "iptoasn", path: i2a_path.display().to_string(), bytes: b, mtime: m, build: Some(format!("{} 行", r.rows)), license: "PDDL 1.0" });
                Some(r)
            }
            Err(e) => {
                errors.push(e);
                None
            }
        };
        let hosting_scan = i2a
            .as_ref()
            .map(|r| {
                r.scan_asns(|asn, desc| {
                    !classify::cn_mixed_asn(asn)
                        && classify::mobile_asn(asn).is_none()
                        && classify::fixed_asn(asn).is_none()
                        && !classify::mobile_kw().is_match(desc)
                        && classify::hosting_kw().is_match(desc)
                })
            })
            .unwrap_or_default();
        Dbs { x4, x6, city, asn, i2a, hosting_scan, files, version: ver.join(","), errors }
    }

    pub fn loaded(&self) -> usize {
        self.x4.is_some() as usize + self.x6.is_some() as usize + self.city.is_some() as usize + self.asn.is_some() as usize + self.i2a.is_some() as usize
    }

    fn raw(&self, ip: IpAddr) -> DbRaw {
        let x = if ip.is_ipv4() { self.x4.as_ref() } else { self.x6.as_ref() };
        let ip2region = x.map(|x| parse_region(x.search(ip)));
        let dbip_city = self.city.as_ref().and_then(|r| {
            let c: MCity = r.lookup(ip).ok()?.decode().ok()??;
            let sub = c.subdivisions.as_ref().and_then(|v| v.first());
            Some(DbipCity {
                country: c.country.as_ref().and_then(|p| p.iso_code).map(String::from),
                country_name: c.country.as_ref().and_then(|p| p.names.as_ref()).and_then(|n| n.zh.or(n.en)).map(String::from),
                region: sub.and_then(|p| p.names.as_ref()).and_then(|n| n.en).map(String::from),
                city: c.city.as_ref().and_then(|p| p.names.as_ref()).and_then(|n| n.en).map(String::from),
                lat: c.location.as_ref().and_then(|l| l.latitude),
                lon: c.location.as_ref().and_then(|l| l.longitude),
            })
        });
        let dbip_asn = self.asn.as_ref().and_then(|r| {
            let a: MAsn = r.lookup(ip).ok()?.decode().ok()??;
            Some(DbipAsn { asn: a.autonomous_system_number, org: a.autonomous_system_organization.map(String::from) })
        });
        let iptoasn = self.i2a.as_ref().and_then(|r| r.lookup(ip));
        DbRaw { ip2region, dbip_city, dbip_asn, iptoasn }
    }

    /// 合成情报。`internal`：该地址属于我方哪台机器（线路机 / 落地节点），由调用方给出。
    pub fn lookup(&self, ip: IpAddr, internal: Option<&str>) -> Intel {
        let ip = canonical(ip);
        let raw = self.raw(ip);
        let i2r = raw.ip2region.clone().unwrap_or_default();
        let dc = raw.dbip_city.clone().unwrap_or_default();
        let da = raw.dbip_asn.clone().unwrap_or_default();
        let ia = raw.iptoasn.clone();
        let mut flags: Vec<String> = Vec::new();
        let mut sources: Vec<String> = Vec::new();
        if i2r.raw.is_some() && (i2r.country.is_some() || i2r.isp.is_some()) {
            sources.push("ip2region".into());
        }
        if raw.dbip_city.is_some() {
            sources.push("dbip_city".into());
        }
        if raw.dbip_asn.is_some() {
            sources.push("dbip_asn".into());
        }
        if ia.is_some() {
            sources.push("iptoasn".into());
        }
        let asn = da.asn.or(ia.as_ref().map(|r| r.asn));
        if let (Some(a), Some(b)) = (da.asn, ia.as_ref().map(|r| r.asn)) {
            if a != b {
                flags.push("asn_conflict".into());
            }
        }
        let as_org = da.org.clone().or_else(|| ia.as_ref().and_then(|r| r.desc.clone()));
        let country = i2r.country.clone().or(dc.country.clone()).or(ia.as_ref().and_then(|r| r.country.clone()));
        // 运营商
        let isp_raw = i2r.isp.clone();
        let (isp, isp_basis) = if let Some(c) = isp_raw.as_deref().and_then(|r| classify::CN_CARRIER.iter().find(|(k, _)| *k == r).map(|x| x.1)) {
            if let Some(ac) = asn.and_then(classify::asn_carrier) {
                if ac != c && !ac.starts_with(c) {
                    flags.push("isp_conflict".into());
                }
            }
            (c.to_string(), "ip2region")
        } else if let Some(c) = asn.and_then(classify::asn_carrier) {
            if isp_raw.is_some() && country.as_deref() == Some("CN") {
                flags.push("isp_conflict".into());
            }
            (c.to_string(), "asn")
        } else if !is_global(ip) {
            ("内网地址".to_string(), "reserved")
        } else if let (Some(r), true) = (isp_raw.clone(), country.as_deref() == Some("CN")) {
            // 国内非三大运营商：ip2region 的中文名（阿里、腾讯、百度…）比英文 AS 组织名好读
            (r, "ip2region")
        } else if let Some(c) = as_org.as_deref().and_then(classify::cn_carrier_from_org) {
            (c.to_string(), "as_org")
        } else if let Some(o) = as_org.as_deref().map(short_org) {
            (o, "as_org")
        } else if let Some(r) = isp_raw.clone() {
            (r, "ip2region")
        } else {
            ("未知".to_string(), "none")
        };
        // 地点
        let (country_name, region, city, geo_basis) = if country.as_deref() == Some("CN") {
            (Some("中国".to_string()), i2r.province.clone(), i2r.city.clone(), Some("ip2region".to_string()))
        } else if dc.country.is_some() && (dc.country == country || country.is_none()) && (dc.region.is_some() || dc.city.is_some()) {
            (
                dc.country_name.clone().or(i2r.country_name.clone()),
                dc.region.as_deref().map(zh_place),
                dc.city.as_deref().map(|c| zh_place(city_key(c))),
                Some("dbip".to_string()),
            )
        } else if i2r.country.is_some() || i2r.province.is_some() {
            (i2r.country_name.clone().or(dc.country_name.clone()), i2r.province.clone(), i2r.city.clone(), Some("ip2region".to_string()))
        } else {
            (dc.country_name.clone(), None, None, dc.country.as_ref().map(|_| "dbip".to_string()))
        };
        // 两库一致性（只记标记，不改结果）
        if let (Some(a), Some(b)) = (&i2r.country, &dc.country) {
            if a != b {
                flags.push("geo_conflict".into());
            }
        } else if country.as_deref() == Some("CN") {
            if let (Some(p), Some(dp)) = (&i2r.province, dc.region.as_deref().and_then(cn_province_from_en)) {
                if !same_province(p, dp) {
                    flags.push("geo_conflict".into());
                }
            }
        }
        // 坐标
        let (lat, lon, coord_basis) = if country.as_deref() == Some("CN") {
            if let Some((la, lo)) = city.as_deref().and_then(cn_city_coord) {
                (Some(la), Some(lo), Some("gazetteer"))
            } else if dc.lat.is_some() && dc.region.as_deref().and_then(cn_province_from_en).zip(region.as_deref()).map(|(a, b)| same_province(a, b)).unwrap_or(false) {
                (dc.lat, dc.lon, Some("dbip"))
            } else if let Some((la, lo)) = region.as_deref().and_then(cn_province_capital) {
                (Some(la), Some(lo), Some("province_capital"))
            } else {
                (dc.lat, dc.lon, dc.lat.map(|_| "dbip"))
            }
        } else {
            (dc.lat, dc.lon, dc.lat.map(|_| "dbip"))
        };
        let ia_desc = ia.as_ref().and_then(|r| r.desc.clone()).unwrap_or_default();
        let names = [as_org.clone().unwrap_or_default(), ia_desc, isp_raw.clone().unwrap_or_default()].join(" ");
        let is_ula = matches!(ip, IpAddr::V6(a) if (a.segments()[0] & 0xfe00) == 0xfc00);
        let cls: Classification = classify::classify(&ClassifyIn {
            is_global: is_global(ip),
            is_ula,
            internal,
            asn,
            names: names.trim(),
            isp_raw: isp_raw.as_deref(),
            carrier: &isp,
            hosting_scan: Some(&self.hosting_scan),
        });
        if !is_global(ip) {
            flags.push("private".into());
        }
        if internal.is_some() {
            flags.push("internal".into());
        }
        if country.is_some() && country.as_deref() != Some("CN") && internal.is_none() {
            flags.push("foreign".into());
        }
        flags.sort();
        flags.dedup();
        Intel {
            ip: ip.to_string(),
            v: if ip.is_ipv4() { 4 } else { 6 },
            asn,
            as_org,
            isp,
            isp_basis: isp_basis.into(),
            country,
            country_name,
            region,
            city,
            geo_basis,
            lat,
            lon,
            coord_basis: coord_basis.map(String::from),
            type_word: cls.conn_type.word().into(),
            conn_type: cls.conn_type,
            type_confidence: cls.confidence,
            type_reason: cls.reason,
            type_alt: cls.alt,
            internal: internal.map(String::from),
            flags,
            sources,
            dbs: raw,
        }
    }
}

/// IPv4-mapped IPv6（::ffff:a.b.c.d）按 IPv4 查。
pub fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(a) => a.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        x => x,
    }
}

/// std 的 is_global 还没稳定，这里按 IANA 特殊地址表的常用部分实现。
pub fn is_global(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            let o = a.octets();
            !(a.is_private()
                || a.is_loopback()
                || a.is_link_local()
                || a.is_broadcast()
                || a.is_documentation()
                || a.is_unspecified()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 198 && (o[1] & 0xfe) == 18)
                || o[0] >= 240)
        }
        IpAddr::V6(a) => {
            let s = a.segments();
            !(a.is_loopback()
                || a.is_unspecified()
                || a.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                || (s[0] == 0x2001 && s[1] == 0x0db8)
                || a.to_ipv4_mapped().is_some())
        }
    }
}

pub fn short_org(org: &str) -> String {
    let mut s = org.trim().trim_end_matches(',').to_string();
    for suf in [", Inc.", " Inc.", " Inc", ", LLC", " LLC", " Ltd.", " Ltd", " Co., Ltd.", " Co.,Ltd.", " S.A. de C.V.", " Pte Ltd", " Corporation", " Corp.", " Corp"] {
        if let Some(x) = s.strip_suffix(suf) {
            s = x.trim().trim_end_matches(',').to_string();
        }
    }
    if s.is_empty() {
        org.to_string()
    } else {
        s
    }
}

fn city_key(c: &str) -> &str {
    c.split(" (").next().unwrap_or(c).trim()
}

fn zh_place(en: &str) -> String {
    ZH_PLACE.iter().find(|(k, _)| *k == en).map(|x| x.1.to_string()).unwrap_or_else(|| en.to_string())
}

const ZH_PLACE: &[(&str, &str)] = &[
    ("Texas", "得克萨斯州"), ("California", "加利福尼亚州"), ("Georgia", "佐治亚州"), ("Missouri", "密苏里州"), ("Illinois", "伊利诺伊州"),
    ("New York", "纽约州"), ("District of Columbia", "华盛顿特区"), ("Washington, D.C.", "华盛顿特区"), ("Florida", "佛罗里达州"),
    ("Virginia", "弗吉尼亚州"), ("New Jersey", "新泽西州"), ("Arizona", "亚利桑那州"), ("Washington", "华盛顿州"), ("Oregon", "俄勒冈州"),
    ("Nevada", "内华达州"), ("Nuevo León", "新莱昂州"), ("Guerrero", "格雷罗州"), ("Mexico City", "墨西哥城"),
    ("Irvine", "尔湾"), ("Fremont", "弗里蒙特"), ("Los Angeles", "洛杉矶"), ("Pacifica", "帕西菲卡"), ("San Jose", "圣何塞"),
    ("Dallas", "达拉斯"), ("Atlanta", "亚特兰大"), ("Chicago", "芝加哥"), ("Phoenix", "凤凰城"), ("Seattle", "西雅图"),
    ("Ciudad Apodaca", "阿波达卡"), ("Singapore", "新加坡"), ("Tokyo", "东京"), ("Hong Kong", "香港"), ("Central", "中环"),
    ("Mountain View", "山景城"), ("Ashburn", "阿什本"), ("Brisbane", "布里斯班"), ("Sydney", "悉尼"),
];

/// DB-IP 的中国省名（英文）→ ip2region 的省名前缀。
fn cn_province_from_en(en: &str) -> Option<&'static str> {
    Some(match en {
        "Beijing" => "北京", "Tianjin" => "天津", "Shanghai" => "上海", "Chongqing" => "重庆", "Hebei" => "河北", "Shanxi" => "山西",
        "Liaoning" => "辽宁", "Jilin" => "吉林", "Heilongjiang" => "黑龙江", "Jiangsu" => "江苏", "Zhejiang" => "浙江", "Anhui" => "安徽",
        "Fujian" => "福建", "Jiangxi" => "江西", "Shandong" => "山东", "Henan" => "河南", "Hubei" => "湖北", "Hunan" => "湖南",
        "Guangdong" => "广东", "Hainan" => "海南", "Sichuan" => "四川", "Guizhou" => "贵州", "Yunnan" => "云南", "Shaanxi" => "陕西",
        "Gansu" => "甘肃", "Qinghai" => "青海", "Inner Mongolia" => "内蒙古", "Guangxi" => "广西", "Tibet" => "西藏", "Ningxia" => "宁夏",
        "Xinjiang" => "新疆",
        _ => return None,
    })
}

fn same_province(a: &str, b: &str) -> bool {
    let n = |s: &str| s.chars().take(2).collect::<String>();
    n(a) == n(b)
}

/// 省会坐标（WGS84，城市中心点），按省名前两个字匹配。
fn cn_province_capital(p: &str) -> Option<(f64, f64)> {
    let k: String = p.chars().take(2).collect();
    Some(match k.as_str() {
        "北京" => (39.9042, 116.4074), "天津" => (39.1256, 117.1909), "上海" => (31.2304, 121.4737), "重庆" => (29.5630, 106.5516),
        "河北" => (38.0428, 114.5149), "山西" => (37.8706, 112.5489), "辽宁" => (41.8057, 123.4315), "吉林" => (43.8171, 125.3235),
        "黑龙" => (45.8038, 126.5350), "江苏" => (32.0603, 118.7969), "浙江" => (30.2741, 120.1551), "安徽" => (31.8206, 117.2272),
        "福建" => (26.0745, 119.2965), "江西" => (28.6820, 115.8579), "山东" => (36.6512, 117.1201), "河南" => (34.7466, 113.6254),
        "湖北" => (30.5928, 114.3055), "湖南" => (28.2282, 112.9388), "广东" => (23.1291, 113.2644), "海南" => (20.0444, 110.1999),
        "四川" => (30.5728, 104.0668), "贵州" => (26.6470, 106.6302), "云南" => (25.0389, 102.7183), "陕西" => (34.3416, 108.9398),
        "甘肃" => (36.0611, 103.8343), "青海" => (36.6171, 101.7782), "内蒙" => (40.8424, 111.7490), "广西" => (22.8170, 108.3665),
        "西藏" => (29.6520, 91.1721), "宁夏" => (38.4872, 106.2309), "新疆" => (43.8256, 87.6168),
        "香港" => (22.3193, 114.1694), "澳门" => (22.1987, 113.5439), "台湾" => (25.0330, 121.5654),
        _ => return None,
    })
}

/// 常见地级市中心点（本机队来源实际出现过的，加上各省会；不在表里的退回 DB-IP 或省会）。
fn cn_city_coord(c: &str) -> Option<(f64, f64)> {
    Some(match c {
        "郑州市" => (34.7466, 113.6254), "开封市" => (34.7972, 114.3076), "洛阳市" => (34.6197, 112.4540), "新乡市" => (35.3030, 113.9268),
        "武汉市" => (30.5928, 114.3055), "荆门市" => (31.0354, 112.1994), "鄂州市" => (30.3913, 114.8949), "黄冈市" => (30.4539, 114.8724),
        "宜昌市" => (30.6919, 111.2865), "襄阳市" => (32.0090, 112.1226), "孝感市" => (30.9245, 113.9169),
        "北京市" => (39.9042, 116.4074), "上海市" => (31.2304, 121.4737), "天津市" => (39.1256, 117.1909), "重庆市" => (29.5630, 106.5516),
        "广州市" => (23.1291, 113.2644), "深圳市" => (22.5431, 114.0579), "杭州市" => (30.2741, 120.1551), "南京市" => (32.0603, 118.7969),
        "苏州市" => (31.2990, 120.5853), "济南市" => (36.6512, 117.1201), "青岛市" => (36.0671, 120.3826), "成都市" => (30.5728, 104.0668),
        "长沙市" => (28.2282, 112.9388), "西安市" => (34.3416, 108.9398), "合肥市" => (31.8206, 117.2272), "福州市" => (26.0745, 119.2965),
        "厦门市" => (24.4798, 118.0894), "南昌市" => (28.6820, 115.8579), "石家庄市" => (38.0428, 114.5149), "太原市" => (37.8706, 112.5489),
        "沈阳市" => (41.8057, 123.4315), "长春市" => (43.8171, 125.3235), "哈尔滨市" => (45.8038, 126.5350), "昆明市" => (25.0389, 102.7183),
        "贵阳市" => (26.6470, 106.6302), "南宁市" => (22.8170, 108.3665), "海口市" => (20.0444, 110.1999), "兰州市" => (36.0611, 103.8343),
        _ => return None,
    })
}

/// 两点大圆距离（km）。
pub fn distance_km(a: (f64, f64), b: (f64, f64)) -> f64 {
    let r = 6371.0088f64;
    let (la1, lo1, la2, lo2) = (a.0.to_radians(), a.1.to_radians(), b.0.to_radians(), b.1.to_radians());
    let h = ((la2 - la1) / 2.0).sin().powi(2) + la1.cos() * la2.cos() * ((lo2 - lo1) / 2.0).sin().powi(2);
    2.0 * r * h.sqrt().asin()
}

/// 进程内的小缓存（查询本身是微秒级，这里主要省掉重复的 JSON 组装）。满了整体清空，简单可靠。
pub struct MemCache {
    map: HashMap<IpAddr, std::sync::Arc<Intel>>,
    cap: usize,
}

impl MemCache {
    pub fn new(cap: usize) -> MemCache {
        MemCache { map: HashMap::new(), cap }
    }
    pub fn get(&self, ip: &IpAddr) -> Option<std::sync::Arc<Intel>> {
        self.map.get(ip).cloned()
    }
    pub fn put(&mut self, ip: IpAddr, v: std::sync::Arc<Intel>) {
        if self.map.len() >= self.cap {
            self.map.clear();
        }
        self.map.insert(ip, v);
    }
    pub fn clear(&mut self) {
        self.map.clear();
    }
    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// 维度键：地点按“国家|省|市”拼，运营商直接用 isp。
pub fn city_dim(i: &Intel) -> Option<String> {
    if i.city.is_none() && i.region.is_none() {
        return None;
    }
    Some(format!("{}|{}|{}", i.country.clone().unwrap_or_default(), i.region.clone().unwrap_or_default(), i.city.clone().unwrap_or_default()))
}

pub fn dims(i: &Intel) -> BTreeMap<&'static str, String> {
    let mut m = BTreeMap::new();
    m.insert("isp", i.isp.clone());
    if let Some(c) = city_dim(i) {
        m.insert("city", c);
    }
    if let Some(r) = &i.region {
        m.insert("province", format!("{}|{}", i.country.clone().unwrap_or_default(), r));
    }
    if let Some(c) = &i.country {
        m.insert("country", c.clone());
    }
    if let Some(a) = i.asn {
        m.insert("asn", a.to_string());
    }
    m.insert("type", i.conn_type.as_str().to_string());
    m
}
