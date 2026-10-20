//! 单元测试用的小型构造库：不依赖真实的离线库文件。mmdb 没法手工构造，涉及 DB-IP 的断言只在有真实库时跑（tests/real_dbs.rs）。
use std::collections::BTreeMap;
use std::net::IpAddr;

const HEADER: usize = 256;
const VEC_BYTES: usize = 256 * 256 * 8;

/// 按 xdb 3.0 布局造一个文件：头 + 向量索引 + 数据区 + 段索引。每条 (起, 止, 区域串)，起止必须在同一个 /16 内（v6 看前两个字节）。
pub fn build_xdb(v6: bool, entries: &[(IpAddr, IpAddr, &str)]) -> Vec<u8> {
    let mut buf = vec![0u8; HEADER + VEC_BYTES];
    buf[0..2].copy_from_slice(&3u16.to_le_bytes());
    buf[4..8].copy_from_slice(&1_790_000_000u32.to_le_bytes());
    buf[16..18].copy_from_slice(&(if v6 { 6u16 } else { 4 }).to_le_bytes());
    // 数据区
    let mut dptr = Vec::new();
    for (_, _, region) in entries {
        dptr.push(buf.len() as u32);
        buf.extend_from_slice(region.as_bytes());
    }
    // 段索引，按向量格分组、组内按起点排序
    let mut cells: BTreeMap<(u8, u8), Vec<usize>> = BTreeMap::new();
    for (i, (s, e, _)) in entries.iter().enumerate() {
        let (a, b) = match (s, e) {
            (IpAddr::V4(a), IpAddr::V4(b)) => (a.octets().to_vec(), b.octets().to_vec()),
            (IpAddr::V6(a), IpAddr::V6(b)) => (a.octets().to_vec(), b.octets().to_vec()),
            _ => panic!("起止地址族不一致"),
        };
        assert_eq!(a[..2], b[..2], "起止要在同一个 /16 内");
        cells.entry((a[0], a[1])).or_default().push(i);
    }
    let key = |ip: &IpAddr| match ip {
        IpAddr::V4(a) => u32::from(*a) as u128,
        IpAddr::V6(a) => u128::from(*a),
    };
    for ((b0, b1), mut idx) in cells {
        idx.sort_by_key(|&i| key(&entries[i].0));
        let s_ptr = buf.len() as u32;
        let mut last = s_ptr;
        for &i in &idx {
            last = buf.len() as u32;
            let (s, e, region) = &entries[i];
            match (s, e) {
                (IpAddr::V4(s), IpAddr::V4(e)) => {
                    buf.extend_from_slice(&u32::from(*s).to_le_bytes());
                    buf.extend_from_slice(&u32::from(*e).to_le_bytes());
                }
                (IpAddr::V6(s), IpAddr::V6(e)) => {
                    buf.extend_from_slice(&s.octets());
                    buf.extend_from_slice(&e.octets());
                }
                _ => unreachable!(),
            }
            buf.extend_from_slice(&(region.len() as u16).to_le_bytes());
            buf.extend_from_slice(&dptr[i].to_le_bytes());
        }
        let cell = HEADER + b0 as usize * 256 * 8 + b1 as usize * 8;
        buf[cell..cell + 4].copy_from_slice(&s_ptr.to_le_bytes());
        buf[cell + 4..cell + 8].copy_from_slice(&last.to_le_bytes());
    }
    buf
}

/// 一份能覆盖 lookup 测试用例的 iptoasn TSV
pub const TSV: &str = "\
8.8.8.0\t8.8.8.255\t15169\tUS\tGOOGLE - Google LLC
45.87.212.0\t45.87.212.255\t9009\tRO\tM247 Europe SRL
202.102.224.0\t202.102.224.255\t4837\tCN\tCHINA UNICOM China169 Backbone
203.0.114.0\t203.0.114.255\t0\tNone\tNot routed
2400:3200::\t2400:3200:ffff:ffff:ffff:ffff:ffff:ffff\t37963\tCN\tHangzhou Alibaba Advertising Co.,Ltd.
";
