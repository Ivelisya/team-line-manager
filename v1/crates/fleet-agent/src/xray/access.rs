//! Xray 访问日志：行解析 + 带游标的尾读（日志文件本身就是缓冲，不会因积压而丢行）。
//!
//! 行格式：`2026/10/20 13:50:56.612764 from 1.2.3.4:5678 accepted tcp:example.com:443 [in-… -> out-…] email: e-…`
//! 旧版也可能是 `from tcp:1.2.3.4:5678`。时间戳是进程本地时区的墙钟，这里用本机当前 UTC 偏移换算（Agent 与 Xray 同机）。

use anyhow::{Context, Result};
use fleet_proto::{tags, AccessEvent};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom};
use std::net::IpAddr;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use uuid::Uuid;

const MAX_LINE: usize = 32 * 1024;
const MAX_WINDOW: u64 = 2 * 1024 * 1024;

#[derive(Debug)]
pub enum Parsed {
    Event(AccessEvent),
    /// 合法但不是入口流量（探测入站等）
    Other,
    Bad,
}

/// 本机当前 UTC 偏移（秒）
pub fn local_utc_offset_s() -> i64 {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff as i64
    }
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// "2026/10/20" + "13:50:56.612764" + 本地 UTC 偏移 → unix ms
pub fn parse_ts(date: &str, time: &str, utc_offset_s: i64) -> Option<i64> {
    let mut d = date.split('/');
    let (y, mo, da): (i64, i64, i64) = (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.split(':');
    let (h, mi, s): (i64, i64, i64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?, t.next()?.parse().ok()?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&da) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    let ms: i64 = if frac.is_empty() { 0 } else { format!("{:0<3}", &frac[..frac.len().min(3)]).parse().ok()? };
    let secs = days_from_civil(y, mo, da) * 86400 + h * 3600 + mi * 60 + s - utc_offset_s;
    Some(secs * 1000 + ms)
}

fn norm_ip(s: &str) -> Option<IpAddr> {
    let ip: IpAddr = s.trim_matches(|c| c == '[' || c == ']').parse().ok()?;
    Some(match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    })
}

pub fn parse_line(line: &str, utc_offset_s: i64) -> Parsed {
    let line = line.trim();
    let mut p = line.splitn(3, char::is_whitespace);
    let (Some(date), Some(time), Some(rest)) = (p.next(), p.next(), p.next()) else { return Parsed::Bad };
    let Some(ts_ms) = parse_ts(date, time, utc_offset_s) else { return Parsed::Bad };
    let tok: Vec<&str> = rest.split_whitespace().collect();
    let Some(i) = tok.iter().position(|t| *t == "accepted" || *t == "rejected") else { return Parsed::Bad };
    if tok[i] == "rejected" {
        return Parsed::Bad;
    }
    // 来源：accepted 之前最后一个 token（跳过 "from"）
    let src = tok[..i].iter().rev().find(|t| **t != "from").copied();
    let src_ip = src.and_then(|s| {
        let s = s.strip_prefix("tcp:").or_else(|| s.strip_prefix("udp:")).unwrap_or(s);
        norm_ip(s.rsplit_once(':')?.0)
    });
    // 目标：network:host:port
    let Some(dest) = tok.get(i + 1) else { return Parsed::Bad };
    let (network, hostport) = match dest.split_once(':') {
        Some((n @ ("tcp" | "udp"), r)) => (n, r),
        _ => ("tcp", *dest),
    };
    let Some((host, port)) = hostport.rsplit_once(':') else { return Parsed::Bad };
    let host = host.trim_matches(|c| c == '[' || c == ']').to_ascii_lowercase();
    let Ok(port) = port.parse::<u16>() else { return Parsed::Bad };
    if host.is_empty() || host.len() > 253 {
        return Parsed::Bad;
    }
    // 路由 [in -> out]
    let tail = &tok[i + 2..];
    let route: Option<String> = tail.iter().position(|t| t.starts_with('[')).and_then(|s| {
        let mut buf = String::new();
        for t in &tail[s..] {
            buf.push_str(t);
            buf.push(' ');
            if t.ends_with(']') {
                return Some(buf.trim().trim_start_matches('[').trim_end_matches(']').to_string());
            }
        }
        None
    });
    let (inbound, outbound) = route
        .as_deref()
        .and_then(|r| {
            let r = r.replace(">>", "->").replace("=>", "->");
            let (a, b) = r.split_once("->")?;
            Some((a.trim().to_string(), b.trim().to_string()))
        })
        .unwrap_or_default();
    if !inbound.starts_with("in-") {
        return Parsed::Other;
    }
    let email = tail.iter().position(|t| *t == "email:").and_then(|k| tail.get(k + 1)).copied();
    let entry_id: Option<Uuid> = email.and_then(tags::parse_entry_id).or_else(|| tags::parse_entry_id(&inbound));
    let Some(entry_id) = entry_id else { return Parsed::Bad };
    Parsed::Event(AccessEvent {
        entry_id,
        ts_ms,
        src_ip,
        dest_host: host,
        dest_port: Some(port),
        network: network.to_string(),
        blocked: outbound == "blocked" || outbound == "deny",
    })
}

// ── 尾读 ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Cursor {
    pub inode: u64,
    pub offset: u64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Counters {
    pub lines: u64,
    pub events: u64,
    pub other: u64,
    pub unparsed: u64,
    pub dropped_lines: u64,
    pub rotation_gaps: u64,
    pub truncations: u64,
}

pub struct Tailer {
    path: PathBuf,
    pub cursor: Option<Cursor>,
    seen_missing: bool,
    pub counters: Counters,
}

pub struct Batch {
    pub events: Vec<AccessEvent>,
    /// 提交后的新游标（报告落盘成功后才 commit）
    pub next: Option<Cursor>,
    /// 还有没读完的内容
    pub more: bool,
}

impl Tailer {
    pub fn new(path: PathBuf, cursor: Option<Cursor>) -> Self {
        Self { path, cursor, seen_missing: false, counters: Counters::default() }
    }

    pub fn commit(&mut self, next: Option<Cursor>) {
        if next.is_some() {
            self.cursor = next;
        }
    }

    fn old_path(&self) -> PathBuf {
        let mut s = self.path.clone().into_os_string();
        s.push(".1");
        PathBuf::from(s)
    }

    pub fn read_batch(&mut self, max_events: usize, utc_offset_s: i64) -> Result<Batch> {
        let empty = Batch { events: vec![], next: None, more: false };
        let md = match std::fs::metadata(&self.path) {
            Ok(m) => m,
            Err(_) => {
                self.seen_missing = true;
                return Ok(empty);
            }
        };
        let (inode, size) = (md.ino(), md.len());
        let cur = match self.cursor {
            Some(c) => c,
            // 首次：从当前末尾开始，不重放历史；但文件是在我们启动后才出现的则从头读
            None => Cursor { inode, offset: if self.seen_missing { 0 } else { size } },
        };
        let (read_path, mut cur) = if cur.inode == inode {
            (self.path.clone(), cur)
        } else {
            let old = self.old_path();
            match std::fs::metadata(&old) {
                Ok(m) if m.ino() == cur.inode => (old, cur),
                _ => {
                    self.counters.rotation_gaps += 1;
                    tracing::warn!("访问日志轮转后找不到旧文件，可能漏掉一段");
                    (self.path.clone(), Cursor { inode, offset: 0 })
                }
            }
        };
        let fsize = std::fs::metadata(&read_path).map(|m| m.len()).unwrap_or(0);
        if cur.offset > fsize {
            self.counters.truncations += 1;
            cur.offset = 0;
        }
        let want = (fsize - cur.offset).min(MAX_WINDOW) as usize;
        let mut buf = vec![0u8; want];
        if want > 0 {
            let mut f = std::fs::File::open(&read_path).with_context(|| format!("打开 {}", read_path.display()))?;
            f.seek(SeekFrom::Start(cur.offset))?;
            f.read_exact(&mut buf)?;
        }
        let mut events = Vec::new();
        let mut consumed = 0usize;
        let mut hit_cap = false;
        let end = buf.iter().rposition(|b| *b == b'\n').map(|p| p + 1).unwrap_or(0);
        if end == 0 && buf.len() >= MAX_LINE {
            // 一行没有换行且超长：丢掉这一窗口，避免卡死
            consumed = buf.len();
            self.counters.dropped_lines += 1;
        }
        for raw in buf[..end].split_inclusive(|b| *b == b'\n') {
            if events.len() >= max_events {
                hit_cap = true;
                break;
            }
            consumed += raw.len();
            self.counters.lines += 1;
            if raw.len() > MAX_LINE {
                self.counters.dropped_lines += 1;
                continue;
            }
            match parse_line(&String::from_utf8_lossy(raw), utc_offset_s) {
                Parsed::Event(e) => {
                    self.counters.events += 1;
                    events.push(e);
                }
                Parsed::Other => self.counters.other += 1,
                Parsed::Bad => self.counters.unparsed += 1,
            }
        }
        let mut next = Cursor { inode: cur.inode, offset: cur.offset + consumed as u64 };
        let at_eof = next.offset >= fsize;
        // 旧文件读完了 → 切到新文件开头
        if read_path != self.path && at_eof {
            next = Cursor { inode, offset: 0 };
        }
        let more = hit_cap || (want as u64 == MAX_WINDOW && !at_eof) || (read_path != self.path && !at_eof);
        Ok(Batch { events, next: Some(next), more })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn timestamps() {
        // 2026-10-20 00:00:00 UTC = 1792454400
        assert_eq!(parse_ts("2026/10/20", "00:00:00", 0), Some(1_792_454_400_000));
        // UTC+8 的 08:00:00.5 就是同一时刻
        assert_eq!(parse_ts("2026/10/20", "08:00:00.5", 8 * 3600), Some(1_792_454_400_500));
        assert_eq!(parse_ts("1970/01/01", "00:00:01.123456", 0), Some(1123));
        assert_eq!(parse_ts("2026/13/20", "00:00:00", 0), None);
    }

    fn id() -> Uuid {
        Uuid::from_u128(0xabc)
    }

    #[test]
    fn parses_standard_line() {
        let l = format!(
            "2026/10/20 13:50:56.612764 from 1.2.3.4:5678 accepted tcp:Example.COM:443 [in-{} -> out-{}] email: e-{}",
            id().simple(), id().simple(), id().simple()
        );
        let Parsed::Event(e) = parse_line(&l, 0) else { panic!("{l}") };
        assert_eq!(e.entry_id, id());
        assert_eq!(e.dest_host, "example.com");
        assert_eq!(e.dest_port, Some(443));
        assert_eq!(e.network, "tcp");
        assert_eq!(e.src_ip, Some("1.2.3.4".parse().unwrap()));
        assert!(!e.blocked);
    }

    #[test]
    fn parses_variants() {
        // 新格式带 network 前缀；IPv6 源；出站 blocked / deny；无 email 时用入站 tag
        let l = format!("2026/10/20 13:50:56 from tcp:[2001:db8::1]:5678 accepted udp:[2606:4700::1111]:53 [in-{} -> blocked]", id().simple());
        let Parsed::Event(e) = parse_line(&l, 0) else { panic!() };
        assert_eq!(e.src_ip, Some("2001:db8::1".parse().unwrap()));
        assert_eq!(e.network, "udp");
        assert_eq!(e.dest_host, "2606:4700::1111");
        assert!(e.blocked);
        // IPv4 映射的 IPv6 来源归一成 IPv4
        let l = format!("2026/10/20 13:50:56 from [::ffff:1.2.3.4]:99 accepted tcp:1.1.1.1:80 [in-{} -> deny] email: e-{}", id().simple(), id().simple());
        let Parsed::Event(e) = parse_line(&l, 0) else { panic!() };
        assert_eq!(e.src_ip, Some("1.2.3.4".parse().unwrap()));
        assert!(e.blocked);
        // 分隔符 >>
        let l = format!("2026/10/20 13:50:56 from 1.2.3.4:5 accepted tcp:a.com:80 [in-{} >> out-x] email: e-{}", id().simple(), id().simple());
        assert!(matches!(parse_line(&l, 0), Parsed::Event(_)));
    }

    #[test]
    fn other_and_bad() {
        let probe = format!("2026/10/20 13:50:56 from 127.0.0.1:5 accepted tcp:a.com:80 [probe-{} -> out-x]", id().simple());
        assert!(matches!(parse_line(&probe, 0), Parsed::Other));
        assert!(matches!(parse_line("garbage", 0), Parsed::Bad));
        assert!(matches!(parse_line("2026/10/20 13:50:56 from 1.2.3.4:5 rejected proxy/vless/inbound: invalid request", 0), Parsed::Bad));
        assert!(matches!(parse_line("2026/10/20 13:50:56 from 1.2.3.4:5 accepted tcp:a.com:99999 [in-x -> y]", 0), Parsed::Bad));
    }

    fn tmp(n: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fa-acc-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn line(h: &str) -> String {
        format!("2026/10/20 13:50:56 from 1.2.3.4:5 accepted tcp:{h}:80 [in-{} -> out-x] email: e-{}\n", id().simple(), id().simple())
    }

    #[test]
    fn tail_start_cap_partial_rotate_truncate() {
        let d = tmp("t");
        let p = d.join("access.log");
        std::fs::write(&p, line("old.com")).unwrap();
        let mut t = Tailer::new(p.clone(), None);
        // 首次：从末尾开始，不重放历史
        let b = t.read_batch(10, 0).unwrap();
        assert!(b.events.is_empty());
        t.commit(b.next);
        // 追加 3 行 + 半行；上限 2 条
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        write!(f, "{}{}{}2026/10/20 13:5", line("a.com"), line("b.com"), line("c.com")).unwrap();
        let b = t.read_batch(2, 0).unwrap();
        assert_eq!(b.events.iter().map(|e| e.dest_host.as_str()).collect::<Vec<_>>(), ["a.com", "b.com"]);
        assert!(b.more);
        t.commit(b.next);
        let b = t.read_batch(2, 0).unwrap();
        assert_eq!(b.events.len(), 1); // 半行不读
        t.commit(b.next);
        // 补全半行
        write!(f, "0:56 from 1.2.3.4:5 accepted tcp:d.com:80 [in-{} -> out-x]\n", id().simple()).unwrap();
        let b = t.read_batch(10, 0).unwrap();
        assert_eq!(b.events[0].dest_host, "d.com");
        t.commit(b.next);
        // 轮转：旧文件改名 .1，新文件里有内容；旧文件里还有没读的一行
        write!(f, "{}", line("tail-of-old.com")).unwrap();
        drop(f);
        std::fs::rename(&p, d.join("access.log.1")).unwrap();
        std::fs::write(&p, line("new.com")).unwrap();
        let b = t.read_batch(10, 0).unwrap();
        assert_eq!(b.events.iter().map(|e| e.dest_host.as_str()).collect::<Vec<_>>(), ["tail-of-old.com"]);
        t.commit(b.next);
        let b = t.read_batch(10, 0).unwrap();
        assert_eq!(b.events[0].dest_host, "new.com");
        t.commit(b.next);
        assert_eq!(t.counters.rotation_gaps, 0);
        // 截断（copytruncate）：偏移大于文件大小 → 从头读
        std::fs::write(&p, line("x.com")).unwrap();
        std::fs::OpenOptions::new().write(true).truncate(true).open(&p).unwrap();
        std::fs::write(&p, line("y.com")).unwrap();
        let b = t.read_batch(10, 0).unwrap();
        assert_eq!(b.events.len(), 1);
        assert_eq!(t.counters.truncations, 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn file_appears_after_start_is_read_from_zero() {
        let d = tmp("late");
        let p = d.join("access.log");
        let mut t = Tailer::new(p.clone(), None);
        assert!(t.read_batch(10, 0).unwrap().events.is_empty());
        std::fs::write(&p, line("first.com")).unwrap();
        let b = t.read_batch(10, 0).unwrap();
        assert_eq!(b.events.len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn overlong_line_is_dropped_not_stuck() {
        let d = tmp("long");
        let p = d.join("access.log");
        let mut t = Tailer::new(p.clone(), Some(Cursor { inode: std::fs::metadata(&d).unwrap().ino(), offset: 0 }));
        std::fs::write(&p, vec![b'x'; 40 * 1024]).unwrap();
        t.cursor = Some(Cursor { inode: std::fs::metadata(&p).unwrap().ino(), offset: 0 });
        let b = t.read_batch(10, 0).unwrap();
        assert_eq!(t.counters.dropped_lines, 1);
        assert_eq!(b.next.unwrap().offset, 40 * 1024);
        let _ = std::fs::remove_dir_all(&d);
    }
}
