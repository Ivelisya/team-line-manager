//! 每个入口端口的已建立连接数：/proc/net/tcp{,6}，状态 01 = ESTABLISHED，不计回环来源（探测、自检）。

use std::collections::HashMap;

fn is_loopback(hex_addr: &str) -> bool {
    match hex_addr.len() {
        // IPv4 小端：127.x.x.x → 末字节 "7F"
        8 => hex_addr.ends_with("7F"),
        32 => {
            hex_addr == "00000000000000000000000001000000"
                || (hex_addr.starts_with("0000000000000000FFFF0000") && hex_addr.ends_with("7F"))
        }
        _ => false,
    }
}

/// 统计 `ports` 里每个端口的连接数
pub fn count(proc_net_tcp: &[&str], ports: &[u16]) -> HashMap<u16, u32> {
    let mut m: HashMap<u16, u32> = ports.iter().map(|p| (*p, 0)).collect();
    for content in proc_net_tcp {
        // 每 30 秒扫一遍全部连接（可能上万行）：跳过 sl 列后逐列迭代取本地/远端/状态，不为每行分配 Vec，非 ESTABLISHED 直接跳过
        for l in content.lines().skip(1) {
            let mut f = l.split_whitespace().skip(1);
            let (Some(local), Some(remote), Some("01")) = (f.next(), f.next(), f.next()) else { continue };
            let Some((_, lp)) = local.rsplit_once(':') else { continue };
            let Ok(port) = u16::from_str_radix(lp, 16) else { continue };
            let Some(c) = m.get_mut(&port) else { continue };
            let Some((ra, _)) = remote.rsplit_once(':') else { continue };
            if !is_loopback(ra) {
                *c += 1;
            }
        }
    }
    m
}

pub fn count_now(ports: &[u16]) -> HashMap<u16, u32> {
    let a = std::fs::read_to_string("/proc/net/tcp").unwrap_or_default();
    let b = std::fs::read_to_string("/proc/net/tcp6").unwrap_or_default();
    count(&[&a, &b], ports)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_established_non_loopback() {
        let t4 = "  sl  local_address rem_address   st\n\
   0: 00000000:4E21 0100A8C0:C350 01 00000000:00000000 00:00000000 00000000     0        0 1\n\
   1: 00000000:4E21 0100007F:C351 01 00000000:00000000 00:00000000 00000000     0        0 2\n\
   2: 00000000:4E21 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 3\n\
   3: 00000000:4E22 0200A8C0:C352 01 00000000:00000000 00:00000000 00000000     0        0 4\n";
        let t6 = "  sl  local_address rem_address   st\n\
   0: 00000000000000000000000000000000:4E21 B80D0120000000000000000001000000:C353 01 0 0 0 0 0\n\
   1: 00000000000000000000000000000000:4E21 00000000000000000000000001000000:C354 01 0 0 0 0 0\n\
   2: 00000000000000000000000000000000:4E21 0000000000000000FFFF00000100007F:C355 01 0 0 0 0 0\n";
        let m = count(&[t4, t6], &[0x4E21, 0x4E22, 0x4E23]);
        // 0x4E21=20001：v4 一条 + v6 一条；回环三条不计；监听(0A)不计
        assert_eq!(m[&0x4E21], 2);
        assert_eq!(m[&0x4E22], 1);
        assert_eq!(m[&0x4E23], 0);
    }
}
