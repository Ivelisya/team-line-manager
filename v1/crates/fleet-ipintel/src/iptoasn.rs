//! iptoasn.com `ip2asn-combined.tsv` 读取器（PDDL 1.0）。
//!
//! 每行：`range_start \t range_end \t AS_number \t country_code \t AS_description`，IPv4 与 IPv6 混在一个文件里，
//! AS_number 为 0 表示“未宣告”。文件解压后约 30–40 MB，整个 mmap；堆上只放每行起点偏移（u32，约 2–3 MB），
//! 按地址族分开、按起始地址排序，查询时二分并现场解析那一行。
use memmap2::Mmap;
use std::collections::HashSet;
use std::fs::File;
use std::net::IpAddr;
use std::path::Path;

pub struct IpToAsn {
    map: Mmap,
    v4: Vec<u32>,
    v6: Vec<u32>,
    pub rows: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AsnRow {
    pub asn: u32,
    pub country: Option<String>,
    pub desc: Option<String>,
}

fn key(ip: IpAddr) -> u128 {
    match ip {
        IpAddr::V4(a) => u32::from(a) as u128,
        IpAddr::V6(a) => u128::from(a),
    }
}

impl IpToAsn {
    pub fn open(path: &Path) -> Result<IpToAsn, String> {
        let f = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        // SAFETY: 同 xdb，替换走 rename，映射期间文件内容不变。
        let map = unsafe { Mmap::map(&f) }.map_err(|e| format!("{}: mmap: {e}", path.display()))?;
        if map.len() >= 2 && map[0] == 0x1f && map[1] == 0x8b {
            return Err(format!("{}: 是 gzip 压缩文件，请先解压（fetch-dbs.sh 会解压）", path.display()));
        }
        // 只存行偏移（u32）；iptoasn 本身按起始地址排好序，顺手核对，乱序时才按解析出的地址重排。
        let mut v4: Vec<u32> = Vec::with_capacity(600_000);
        let mut v6: Vec<u32> = Vec::with_capacity(300_000);
        let (mut last4, mut last6, mut sorted4, mut sorted6) = (0u128, 0u128, true, true);
        let mut off = 0usize;
        let b = &map[..];
        let mut rows = 0usize;
        while off < b.len() {
            let end = memchr_nl(b, off);
            if let Some((start, _)) = parse_start(&b[off..end]) {
                if off > u32::MAX as usize {
                    return Err("iptoasn 文件超过 4 GiB".into());
                }
                rows += 1;
                let k = key(start);
                match start {
                    IpAddr::V4(_) => {
                        sorted4 &= k >= last4;
                        last4 = k;
                        v4.push(off as u32)
                    }
                    IpAddr::V6(_) => {
                        sorted6 &= k >= last6;
                        last6 = k;
                        v6.push(off as u32)
                    }
                }
            }
            off = end + 1;
        }
        if rows == 0 {
            return Err(format!("{}: 没有可用的行", path.display()));
        }
        let skey = |off: &u32| {
            let o = *off as usize;
            parse_start(&b[o..memchr_nl(b, o)]).map(|(s, _)| key(s)).unwrap_or(0)
        };
        if !sorted4 {
            v4.sort_by_key(skey);
        }
        if !sorted6 {
            v6.sort_by_key(skey);
        }
        v4.shrink_to_fit();
        v6.shrink_to_fit();
        #[cfg(unix)]
        {
            let _ = map.advise(memmap2::Advice::Random);
        }
        Ok(IpToAsn { v4, v6, map, rows })
    }

    fn line(&self, off: u32) -> &[u8] {
        let o = off as usize;
        &self.map[o..memchr_nl(&self.map, o)]
    }

    pub fn lookup(&self, ip: IpAddr) -> Option<AsnRow> {
        let idx = if ip.is_ipv4() { &self.v4 } else { &self.v6 };
        let k = key(ip);
        // 最后一个 start <= k 的行
        let pos = idx.partition_point(|&off| parse_start(self.line(off)).map(|(s, _)| key(s) <= k).unwrap_or(true));
        if pos == 0 {
            return None;
        }
        let line = std::str::from_utf8(self.line(idx[pos - 1])).ok()?;
        // 迭代器按需取字段，命中后不再为切分结果分配 Vec；少于 3 列的行视为无效
        let mut f = line.splitn(5, '\t');
        let (_start, end, asn) = (f.next()?, f.next()?, f.next()?);
        let end: IpAddr = end.trim().parse().ok()?;
        if k > key(end) {
            return None;
        }
        let asn: u32 = asn.trim().parse().ok()?;
        if asn == 0 {
            return None;
        }
        let nz = |s: Option<&str>| s.map(|x| x.trim()).filter(|x| !x.is_empty() && *x != "None" && *x != "Not routed").map(String::from);
        let country = nz(f.next());
        let desc = nz(f.next());
        Some(AsnRow { asn, country, desc })
    }

    /// 扫一遍全部行，按 AS 描述里的机房关键词挑出“托管 / 云”ASN 集合（见 classify::HOSTING_KW）。
    pub fn scan_asns<F: Fn(u32, &str) -> bool>(&self, pred: F) -> HashSet<u32> {
        let mut out = HashSet::new();
        let mut last_asn = 0u32;
        for off in self.v4.iter().chain(self.v6.iter()) {
            let Ok(line) = std::str::from_utf8(self.line(*off)) else { continue };
            let p: Vec<&str> = line.splitn(5, '\t').collect();
            if p.len() < 5 {
                continue;
            }
            let Ok(asn) = p[2].trim().parse::<u32>() else { continue };
            if asn == 0 || asn == last_asn || out.contains(&asn) {
                continue;
            }
            last_asn = asn;
            if pred(asn, p[4]) {
                out.insert(asn);
            }
        }
        // 扫描把整个文件读进了页缓存；把本进程的映射页丢掉（RssFile 回落），之后查询按需缺页
        #[cfg(unix)]
        unsafe {
            let _ = self.map.unchecked_advise(memmap2::UncheckedAdvice::DontNeed);
        }
        out
    }
}

fn memchr_nl(b: &[u8], from: usize) -> usize {
    b[from..].iter().position(|&c| c == b'\n').map(|p| from + p).unwrap_or(b.len())
}

fn parse_start(line: &[u8]) -> Option<(IpAddr, usize)> {
    let tab = line.iter().position(|&c| c == b'\t')?;
    let s = std::str::from_utf8(&line[..tab]).ok()?;
    s.trim().parse::<IpAddr>().ok().map(|ip| (ip, tab))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TSV;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn lookup_v4_v6_and_gaps() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("ip2asn-combined.tsv");
        std::fs::write(&p, TSV).unwrap();
        let t = IpToAsn::open(&p).unwrap();
        assert_eq!(t.rows, 5);
        let g = t.lookup(ip("8.8.8.8")).unwrap();
        assert_eq!((g.asn, g.country.as_deref(), g.desc.as_deref()), (15169, Some("US"), Some("GOOGLE - Google LLC")));
        assert_eq!(t.lookup(ip("202.102.224.68")).unwrap().asn, 4837);
        assert_eq!(t.lookup(ip("2400:3200::1")).unwrap().asn, 37963);
        assert!(t.lookup(ip("8.8.9.1")).is_none(), "落在两段之间");
        assert!(t.lookup(ip("1.1.1.1")).is_none(), "比第一段还小");
        assert!(t.lookup(ip("203.0.114.9")).is_none(), "AS0 = 未宣告");
        assert!(t.lookup(ip("2600::1")).is_none());
    }

    #[test]
    fn scan_asns_and_bad_files() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t.tsv");
        std::fs::write(&p, TSV).unwrap();
        let t = IpToAsn::open(&p).unwrap();
        let hits = t.scan_asns(|_, desc| desc.contains("Alibaba"));
        assert_eq!(hits.len(), 1);
        assert!(hits.contains(&37963));
        // gzip 头
        let gz = d.path().join("gz.tsv");
        std::fs::write(&gz, [0x1f, 0x8b, 8, 0]).unwrap();
        assert!(IpToAsn::open(&gz).err().unwrap().contains("gzip"));
        let empty = d.path().join("empty.tsv");
        std::fs::write(&empty, "# comment only
").unwrap();
        assert!(IpToAsn::open(&empty).is_err());
    }

    #[test]
    fn unsorted_input_is_resorted() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("u.tsv");
        std::fs::write(&p, "9.0.0.0	9.0.0.255	2	US	B
1.0.0.0	1.0.0.255	1	US	A
").unwrap();
        let t = IpToAsn::open(&p).unwrap();
        assert_eq!(t.lookup(ip("1.0.0.7")).unwrap().asn, 1);
        assert_eq!(t.lookup(ip("9.0.0.7")).unwrap().asn, 2);
    }
}
