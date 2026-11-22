//! 每个入口端口的已建立连接数：/proc/net/tcp{,6}，状态 01 = ESTABLISHED，不计回环来源（探测、自检）。

use std::collections::HashMap;
use std::io::{BufRead, BufReader};

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

/// 统计 `ports` 里每个端口的连接数。`tables` 是 /proc/net/tcp、tcp6 的内容（首行是表头）
pub fn count<R: BufRead>(tables: impl IntoIterator<Item = R>, ports: &[u16]) -> HashMap<u16, u32> {
    let mut m: HashMap<u16, u32> = ports.iter().map(|p| (*p, 0)).collect();
    // 每 30 秒扫一遍全部连接：逐行读进同一个缓冲（整表读成字符串在 5 万连接时每次要分配约 30 MB）；
    // 跳过 sl 列后逐列迭代取本地/远端/状态，不为每行分配 Vec，非 ESTABLISHED 直接跳过
    let mut l = String::new();
    for mut table in tables {
        let _ = table.read_line(&mut l); // 表头
        loop {
            l.clear();
            if matches!(table.read_line(&mut l), Ok(0) | Err(_)) {
                break;
            }
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
    let tables = ["/proc/net/tcp", "/proc/net/tcp6"].into_iter().filter_map(|p| std::fs::File::open(p).ok()).map(BufReader::new);
    count(tables, ports)
}

/// `ports` 里有哪些正在被监听（状态 0A = LISTEN，任意本地地址，IPv4 / IPv6 都算；同 `ss -tlnH '( sport = :P … )'`）。
/// 内核先列监听的套接字再列连接，要找的端口都找到了就不再往下读（端口都在听时只读表头那一小段）；有没找到的才读完整张表
pub fn listening<R: BufRead>(tables: impl IntoIterator<Item = R>, ports: &[u16]) -> Vec<u16> {
    let mut found: Vec<u16> = vec![];
    let all = |f: &Vec<u16>| ports.iter().all(|p| f.contains(p));
    if all(&found) {
        return found;
    }
    let mut l = String::new();
    for mut table in tables {
        let _ = table.read_line(&mut l); // 表头
        loop {
            l.clear();
            if matches!(table.read_line(&mut l), Ok(0) | Err(_)) {
                break;
            }
            let mut f = l.split_whitespace().skip(1);
            let (Some(local), Some(_), Some("0A")) = (f.next(), f.next(), f.next()) else { continue };
            let Some(port) = local.rsplit_once(':').and_then(|(_, p)| u16::from_str_radix(p, 16).ok()) else { continue };
            if ports.contains(&port) && !found.contains(&port) {
                found.push(port);
                if all(&found) {
                    return found;
                }
            }
        }
    }
    found
}

/// 读本机的 /proc/net/tcp、tcp6 看 `ports` 里有哪些在监听。两张表都打不开返回 None（调用方当「不知道」）
pub fn listening_now(ports: &[u16]) -> Option<Vec<u16>> {
    let tables: Vec<_> = ["/proc/net/tcp", "/proc/net/tcp6"].into_iter().filter_map(|p| std::fs::File::open(p).ok()).map(BufReader::new).collect();
    if tables.is_empty() {
        return None;
    }
    Some(listening(tables, ports))
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
        let m = count([t4.as_bytes(), t6.as_bytes()], &[0x4E21, 0x4E22, 0x4E23]);
        // 0x4E21=20001：v4 一条 + v6 一条；回环三条不计；监听(0A)不计
        assert_eq!(m[&0x4E21], 2);
        assert_eq!(m[&0x4E22], 1);
        assert_eq!(m[&0x4E23], 0);
    }

    /// 监听：只认 0A，任意本地地址、v4 / v6 都算；同一端口多个监听（SO_REUSEPORT、v4 + v6）只记一次；连接、TIME-WAIT 不算
    #[test]
    fn listening_ports_from_proc() {
        // 0x9C40=40000 v4 听 0.0.0.0；0x9C41=40001 只在 v6 听 [::]；0x9C42=40002 只有连接和 TIME-WAIT；0x9C43=40003 听 127.0.0.1
        let t4 = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 00000000:9C40 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 11 1 0000000000000000 100 0 0 10 0\n\
   1: 00000000:9C40 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12 1 0000000000000000 100 0 0 10 0\n\
   2: 0100007F:9C43 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 13 1 0000000000000000 100 0 0 10 0\n\
   3: 0500000A:9C42 04030201:01BB 01 00000000:00000000 00:00000000 00000000     0        0 14 1 0000000000000000 20 4 30 10 -1\n\
   4: 0500000A:9C42 04030201:01BB 06 00000000:00000000 03:00000F9A 00000000     0        0 0 3 0000000000000000\n";
        let t6 = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 00000000000000000000000000000000:9C41 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 21 1 0000000000000000 100 0 0 10 0\n\
   1: 0000000000000000FFFF00000500000A:9C40 0000000000000000FFFF000004030201:C350 01 00000000:00000000 00:00000000 00000000     0        0 22 1 0000000000000000 20 4 30 10 -1\n";
        let tables = || [t4.as_bytes(), t6.as_bytes()];
        let mut v = listening(tables(), &[40000, 40001, 40002, 40003, 40004]);
        v.sort_unstable();
        assert_eq!(v, vec![40000, 40001, 40003]);
        // 都在听：结果就是全部（读到即止）
        let mut v = listening(tables(), &[40001, 40000]);
        v.sort_unstable();
        assert_eq!(v, vec![40000, 40001]);
        // 只有连接没有监听（sing-box 没起来、端口被出站连接占着）
        assert!(listening(tables(), &[40002]).is_empty());
        assert!(listening(tables(), &[]).is_empty());
        // 只有 v4 表（关了 IPv6）
        assert_eq!(listening([t4.as_bytes()], &[40001, 40000]), vec![40000]);
    }

    /// 本机实测：自己起一个监听，读真的 /proc 能看到；关掉之后看不到
    #[test]
    fn listening_now_sees_real_listener() {
        let l = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let p = l.local_addr().unwrap().port();
        assert_eq!(listening_now(&[p]), Some(vec![p]));
        drop(l);
        assert_eq!(listening_now(&[p]), Some(vec![]));
    }
}
