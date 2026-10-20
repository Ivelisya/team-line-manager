//! 最小 STUN（RFC 5389）Binding：只构造请求、解析 XOR-MAPPED-ADDRESS / MAPPED-ADDRESS。
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

const MAGIC: u32 = 0x2112_A442;

pub fn binding_request(txid: [u8; 12]) -> Vec<u8> {
    let mut b = Vec::with_capacity(20);
    b.extend_from_slice(&0x0001u16.to_be_bytes());
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&MAGIC.to_be_bytes());
    b.extend_from_slice(&txid);
    b
}

/// 解析 Binding 成功响应，返回映射地址；事务 id 不符或格式不对返回 None
pub fn parse_response(b: &[u8], txid: [u8; 12]) -> Option<SocketAddr> {
    if b.len() < 20 || u16::from_be_bytes([b[0], b[1]]) != 0x0101 || b[4..8] != MAGIC.to_be_bytes() || b[8..20] != txid {
        return None;
    }
    let len = u16::from_be_bytes([b[2], b[3]]) as usize;
    let attrs = b.get(20..20 + len)?;
    let mut i = 0;
    let mut mapped = None;
    while i + 4 <= attrs.len() {
        let t = u16::from_be_bytes([attrs[i], attrs[i + 1]]);
        let l = u16::from_be_bytes([attrs[i + 2], attrs[i + 3]]) as usize;
        let v = attrs.get(i + 4..i + 4 + l)?;
        match t {
            0x0020 => return decode(v, true, txid),
            0x0001 => mapped = decode(v, false, txid),
            _ => {}
        }
        i += 4 + l.div_ceil(4) * 4;
    }
    mapped
}

fn decode(v: &[u8], xor: bool, txid: [u8; 12]) -> Option<SocketAddr> {
    if v.len() < 4 {
        return None;
    }
    let mut port = u16::from_be_bytes([v[2], v[3]]);
    if xor {
        port ^= (MAGIC >> 16) as u16;
    }
    match v[1] {
        0x01 if v.len() >= 8 => {
            let mut o = [v[4], v[5], v[6], v[7]];
            if xor {
                for (x, m) in o.iter_mut().zip(MAGIC.to_be_bytes()) {
                    *x ^= m;
                }
            }
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(o)), port))
        }
        0x02 if v.len() >= 20 => {
            let mut o = [0u8; 16];
            o.copy_from_slice(&v[4..20]);
            if xor {
                let mut k = [0u8; 16];
                k[..4].copy_from_slice(&MAGIC.to_be_bytes());
                k[4..].copy_from_slice(&txid);
                for (x, m) in o.iter_mut().zip(k) {
                    *x ^= m;
                }
            }
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(o)), port))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xor_mapped_v4() {
        // RFC 5769 2.2 样例响应里的 XOR-MAPPED-ADDRESS：192.0.2.1:32853
        let txid = [0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae];
        let mut b = vec![0x01, 0x01, 0x00, 0x0c];
        b.extend_from_slice(&MAGIC.to_be_bytes());
        b.extend_from_slice(&txid);
        b.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x01, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43]);
        assert_eq!(parse_response(&b, txid), Some("192.0.2.1:32853".parse().unwrap()));
        assert_eq!(binding_request(txid).len(), 20);
    }
}
