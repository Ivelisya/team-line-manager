//! ip2region xdb 读取器（格式 2.0 / 3.0，IPv4 与 IPv6），整个文件 mmap，不进堆。
//!
//! 文件布局（github.com/lionsoul2014/ip2region，Apache-2.0）：
//! - 0..256：头。`u16 structure`（2 或 3）、`u32 created_at`（偏移 4）；3.0 起偏移 16 是 `u16 ip_version`（4/6）。
//! - 256..256+512 KiB：向量索引，按 IP 前两个字节 `[b0][b1]` 分 65536 格，每格 8 字节 `(u32 s_ptr, u32 e_ptr)`（小端）。
//! - 段索引：每条 `start_ip | end_ip | u16 data_len | u32 data_ptr`。IPv4 条目 14 字节，IP 是小端 u32；
//!   IPv6 条目 38 字节，IP 是 16 字节大端（与原型 tools/ipdb.py 一致，已在 a395 上用真实库核对）。
//! - 数据区：UTF-8 区域串 `国家|省|市|运营商|国家代码`，“0” 表示空。
//!
//! 查询：向量索引定位到一小段（通常几十条），段内二分。
use memmap2::Mmap;
use std::fs::File;
use std::net::IpAddr;
use std::path::Path;

const HEADER: usize = 256;
const VEC_COLS: usize = 256;
const VEC_SIZE: usize = 8;

pub struct Xdb {
    map: Mmap,
    pub structure: u16,
    pub created_at: u32,
    pub v6: bool,
    isize: usize,
}

impl Xdb {
    pub fn open(path: &Path) -> Result<Xdb, String> {
        let f = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        // SAFETY: 库文件由 fetch-dbs.sh 以“写临时文件 + rename”方式替换，已映射的旧 inode 不会被截断或改写。
        let map = unsafe { Mmap::map(&f) }.map_err(|e| format!("{}: mmap: {e}", path.display()))?;
        if map.len() < HEADER + VEC_COLS * VEC_COLS * VEC_SIZE {
            return Err(format!("{}: 文件太短，不是 xdb", path.display()));
        }
        let structure = u16::from_le_bytes([map[0], map[1]]);
        let created_at = u32::from_le_bytes([map[4], map[5], map[6], map[7]]);
        let ipver = if structure >= 3 { u16::from_le_bytes([map[16], map[17]]) } else { 4 };
        let v6 = ipver == 6;
        #[cfg(unix)]
        {
            let _ = map.advise(memmap2::Advice::Random);
        }
        Ok(Xdb { map, structure, created_at, v6, isize: if v6 { 38 } else { 14 } })
    }

    fn u32le(&self, p: usize) -> u32 {
        let b = &self.map[p..p + 4];
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }

    /// 返回原始区域串；地址族不匹配、没有覆盖或数据为空时返回 None。
    pub fn search(&self, ip: IpAddr) -> Option<&str> {
        let (packed, val): (Vec<u8>, u128) = match (ip, self.v6) {
            (IpAddr::V4(a), false) => (a.octets().to_vec(), u32::from(a) as u128),
            (IpAddr::V6(a), true) => (a.octets().to_vec(), u128::from(a)),
            _ => return None,
        };
        let idx = packed[0] as usize * VEC_COLS * VEC_SIZE + packed[1] as usize * VEC_SIZE;
        let s_ptr = self.u32le(HEADER + idx) as usize;
        let e_ptr = self.u32le(HEADER + idx + 4) as usize;
        if s_ptr == 0 || e_ptr == 0 || e_ptr < s_ptr || e_ptr + self.isize > self.map.len() {
            return None;
        }
        let nb = if self.v6 { 16 } else { 4 };
        let (mut lo, mut hi) = (0i64, ((e_ptr - s_ptr) / self.isize) as i64);
        while lo <= hi {
            let m = (lo + hi) >> 1;
            let p = s_ptr + m as usize * self.isize;
            let (sip, eip) = if self.v6 {
                let s = u128::from_be_bytes(self.map[p..p + 16].try_into().ok()?);
                let e = u128::from_be_bytes(self.map[p + 16..p + 32].try_into().ok()?);
                (s, e)
            } else {
                (self.u32le(p) as u128, self.u32le(p + 4) as u128)
            };
            if val < sip {
                hi = m - 1;
            } else if val > eip {
                lo = m + 1;
            } else {
                let q = p + 2 * nb;
                let dlen = u16::from_le_bytes([self.map[q], self.map[q + 1]]) as usize;
                let dptr = self.u32le(q + 2) as usize;
                if dlen == 0 || dptr + dlen > self.map.len() {
                    return None;
                }
                return std::str::from_utf8(&self.map[dptr..dptr + dlen]).ok();
            }
        }
        None
    }
}

/// ip2region 区域串拆成字段；“0”、空串一律为 None；Reserved（保留地址）整体为空。
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Region {
    pub country_name: Option<String>,
    pub province: Option<String>,
    pub city: Option<String>,
    pub isp: Option<String>,
    pub country: Option<String>,
    pub raw: Option<String>,
}

pub fn parse_region(raw: Option<&str>) -> Region {
    let Some(r) = raw else { return Region::default() };
    let p: Vec<&str> = r.split('|').collect();
    let z = |i: usize| p.get(i).map(|s| s.trim()).filter(|s| !s.is_empty() && *s != "0").map(String::from);
    if p.first().map(|s| s.eq_ignore_ascii_case("Reserved")).unwrap_or(false) {
        return Region { raw: Some(r.to_string()), ..Default::default() };
    }
    Region { country_name: z(0), province: z(1), city: z(2), isp: z(3), country: z(4), raw: Some(r.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::build_xdb;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn search_v4_and_v6_on_constructed_files() {
        let d = tempfile::tempdir().unwrap();
        let p4 = d.path().join("v4.xdb");
        std::fs::write(
            &p4,
            build_xdb(
                false,
                &[
                    (ip("202.102.224.0"), ip("202.102.224.127"), "中国|河南省|郑州市|联通|CN"),
                    (ip("202.102.224.128"), ip("202.102.224.255"), "中国|河南省|开封市|电信|CN"),
                    (ip("10.0.0.0"), ip("10.0.255.255"), "Reserved|Reserved|Reserved|0|0"),
                ],
            ),
        )
        .unwrap();
        let x = Xdb::open(&p4).unwrap();
        assert!(!x.v6);
        assert_eq!(x.structure, 3);
        assert_eq!(x.search(ip("202.102.224.68")), Some("中国|河南省|郑州市|联通|CN"));
        assert_eq!(x.search(ip("202.102.224.200")), Some("中国|河南省|开封市|电信|CN"));
        assert_eq!(x.search(ip("202.102.225.1")), None, "同一向量格里没有覆盖");
        assert_eq!(x.search(ip("8.8.8.8")), None, "空格子");
        assert_eq!(x.search(ip("2400:3200::1")), None, "地址族不匹配");
        let r = parse_region(x.search(ip("202.102.224.68")));
        assert_eq!((r.province.as_deref(), r.city.as_deref(), r.isp.as_deref(), r.country.as_deref()), (Some("河南省"), Some("郑州市"), Some("联通"), Some("CN")));
        let reserved = parse_region(x.search(ip("10.0.1.1")));
        assert!(reserved.country.is_none() && reserved.raw.is_some());

        let p6 = d.path().join("v6.xdb");
        std::fs::write(&p6, build_xdb(true, &[(ip("2400:3200::"), ip("2400:3200:ffff:ffff:ffff:ffff:ffff:ffff"), "中国|浙江省|杭州市|阿里|CN")])).unwrap();
        let x6 = Xdb::open(&p6).unwrap();
        assert!(x6.v6);
        assert_eq!(x6.search(ip("2400:3200::1")), Some("中国|浙江省|杭州市|阿里|CN"));
        assert_eq!(x6.search(ip("8.8.8.8")), None);
    }

    #[test]
    fn rejects_short_file_and_parse_region_zero() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("short.xdb");
        std::fs::write(&p, [0u8; 100]).unwrap();
        assert!(Xdb::open(&p).err().unwrap().contains("文件太短"));
        assert!(Xdb::open(&d.path().join("nope.xdb")).is_err());
        let r = parse_region(Some("0|0|0|0|0"));
        assert!(r.country.is_none() && r.province.is_none() && r.isp.is_none());
        assert_eq!(parse_region(None), Region::default());
    }
}
