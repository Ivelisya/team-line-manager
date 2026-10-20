//! Xray 访问日志 tail（取代旧 team-ops-bridge 的 activity 采集）。
//!
//! - 路径取自 Xray 配置 `log.access`（每 60 s 随配置重读）。
//! - 游标 `(inode, offset)`；轮转（logrotate `create` + `delaycompress`）时先读完旧 inode 的剩余部分
//!   （在同目录 `<文件名>.*` 里按 inode 找，不读 .gz），再从新文件开头读；找不到旧文件记一次 `rotation_gap`；
//!   文件被截断（offset > size）记一次 `truncation` 并从头读。
//! - 解析逐字移植旧 `bridge.parse_access`（见 `parse_line` 与测试）；唯一差别：出站是 `fleet-default-deny`（R1 加固后的默认
//!   blackhole）也记为 blocked，旧系统没有这个出站。
//! - 每批最多 `max_events` 条、读 `max_bytes` 字节；超过就停在行边界，剩下的下一批接着读（日志文件本身就是缓冲）。
//!   outbox 积压超过一半上限时暂停读取（背压），不丢行。
//! - 游标只在批次写进 outbox 之后才持久化（`commit`）；崩溃后从上次提交的游标重读，事件 id = `inode:offset`，中控按 id 去重。
//! - 计数都是精确的：`lines`（读到的完整行）、`events`、`loopback`（本机探测，旧系统同样不计）、`unparsed`、
//!   `dropped_lines`（超过 32 KiB 的畸形行，丢弃并计数）、`rotation_gaps`、`truncations`。
use crate::util::write_atomic;
use crate::{info, warn};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::net::IpAddr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const MAX_LINE: usize = 32768;

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub event_id: String,
    pub ts: f64,
    pub inbound: String,
    pub outbound: String,
    pub source_ip: String,
    pub source_port: Option<u16>,
    pub host: String,
    pub port: u16,
    pub protocol: String,
    pub kind: &'static str,
    pub action: &'static str,
    /// 日志行里的原词 accepted / rejected
    pub status: &'static str,
    pub client: String,
}

impl Event {
    /// fleet.access.v1 事件（API.md §11）；action 用 accepted/blocked（旧系统叫 routed/blocked）；rule 恒为 null（访问日志不含命中的规则）
    pub fn to_json(&self) -> Value {
        json!({"id": self.event_id, "ts": self.ts, "inbound": self.inbound, "client": self.client, "source_ip": self.source_ip,
               "source_port": self.source_port, "network": self.protocol, "host": self.host, "port": self.port, "kind": self.kind,
               "action": if self.action == "blocked" { "blocked" } else { "accepted" }, "status": self.status, "rule": null, "outbound": self.outbound})
    }
}

/// 本地时间（进程时区 = /etc/localtime，与旧 Python 的 naive `.timestamp()` 同为 mktime 语义）
fn local_ts(stamp: &str) -> Option<f64> {
    let b = stamp.as_bytes();
    if b.len() != 19 {
        return None;
    }
    let num = |a: usize, n: usize| -> Option<i32> { std::str::from_utf8(&b[a..a + n]).ok()?.parse().ok() };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = num(0, 4)? - 1900;
    tm.tm_mon = num(5, 2)? - 1;
    tm.tm_mday = num(8, 2)?;
    tm.tm_hour = num(11, 2)?;
    tm.tm_min = num(14, 2)?;
    tm.tm_sec = num(17, 2)?;
    tm.tm_isdst = -1;
    if !(0..12).contains(&tm.tm_mon) || !(1..=31).contains(&tm.tm_mday) || tm.tm_hour > 23 || tm.tm_min > 59 || tm.tm_sec > 61 {
        return None;
    }
    let t = unsafe { libc::mktime(&mut tm) };
    (t != -1).then_some(t as f64)
}

fn is_stamp(s: &[u8]) -> bool {
    // \d{4}/\d{2}/\d{2} \d{2}:\d{2}:\d{2}
    s.len() >= 19
        && s[..19].iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'/',
            10 => *c == b' ',
            13 | 16 => *c == b':',
            _ => c.is_ascii_digit(),
        })
}

fn normalized_ip(s: &str) -> Option<IpAddr> {
    let ip: IpAddr = s.parse().ok()?;
    Some(match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    })
}

/// re.split(r"\s*(?:->|>>|=>)\s*", route)
fn split_route(route: &str) -> Vec<String> {
    let b = route.as_bytes();
    let mut parts = vec![];
    let mut cur = 0;
    let mut i = 0;
    while i + 1 < b.len() {
        let two = &b[i..i + 2];
        if two == b"->" || two == b">>" || two == b"=>" {
            let mut s = i;
            while s > cur && b[s - 1].is_ascii_whitespace() {
                s -= 1;
            }
            parts.push(route[cur..s].to_string());
            let mut e = i + 2;
            while e < b.len() && b[e].is_ascii_whitespace() {
                e += 1;
            }
            cur = e;
            i = e;
            continue;
        }
        i += 1;
    }
    parts.push(route[cur..].to_string());
    parts
}

fn trunc(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

pub enum Parsed {
    Event(Event),
    Loopback,
    Unparsed,
}

/// 旧 `parse_access` 的逐字移植
pub fn parse_line(line: &str, event_id: String, blackholes: &[&str]) -> Parsed {
    let lb = line.as_bytes();
    if !is_stamp(lb) {
        return Parsed::Unparsed;
    }
    let stamp = &line[..19];
    let mut i = 19;
    if lb.get(i) == Some(&b'.') {
        let s = i + 1;
        i += 1;
        while i < lb.len() && lb[i].is_ascii_digit() {
            i += 1;
        }
        if i == s {
            return Parsed::Unparsed;
        }
    }
    if lb.get(i) != Some(&b' ') {
        return Parsed::Unparsed;
    }
    i += 1;
    let mut rest = &line[i..];
    if let Some(r) = rest.strip_prefix("from ") {
        // (?:from )? 是可选的；若 "from " 之后不能匹配，正则会回退把 "from" 当作来源，那一定会在后面失败，这里等价
        rest = r;
    }
    let tok = |s: &str| -> Option<(String, usize)> {
        let e = s.find(|c: char| c.is_whitespace()).unwrap_or(s.len());
        (e > 0).then(|| (s[..e].to_string(), e))
    };
    let Some((source, n)) = tok(rest) else { return Parsed::Unparsed };
    rest = &rest[n..];
    let Some(r) = rest.strip_prefix(' ') else { return Parsed::Unparsed };
    let (status, r) = if let Some(r) = r.strip_prefix("accepted ") {
        ("accepted", r)
    } else if let Some(r) = r.strip_prefix("rejected ") {
        ("rejected", r)
    } else {
        return Parsed::Unparsed;
    };
    let Some((destination, n)) = tok(r) else { return Parsed::Unparsed };
    let after = &r[n..];
    // (?: .*?)?\[([^\]]+)\]
    let mut route = None;
    let mut tail = "";
    if after.starts_with(' ') {
        let mut from = 0;
        while let Some(p) = after[from..].find('[') {
            let s = from + p + 1;
            if let Some(q) = after[s..].find(']') {
                if q > 0 {
                    route = Some(&after[s..s + q]);
                    tail = &after[s + q + 1..];
                    break;
                }
            }
            from = s;
        }
    }
    let Some(route) = route else { return Parsed::Unparsed };
    let label = tail.strip_prefix(" email: ").and_then(|t| tok(t).map(|x| x.0)).unwrap_or_default();
    let ts = local_ts(stamp).unwrap_or_else(crate::util::now_f64);
    let path = split_route(route);
    if path.len() != 2 {
        return Parsed::Unparsed;
    }
    let (inbound, outbound) = (&path[0], &path[1]);
    // 旧系统只认 inbound-\d{2,5}；新系统的入口 tag 还有 in-*（API.md §10）
    if !crate::manage::is_entry_tag(inbound) {
        return Parsed::Unparsed;
    }
    let Some((protocol, address)) = destination.split_once(':') else { return Parsed::Unparsed };
    if protocol != "tcp" && protocol != "udp" {
        return Parsed::Unparsed;
    }
    let (host, port) = match address.rfind(':') {
        Some(p) => (&address[..p], &address[p + 1..]),
        None => ("", address),
    };
    let host = trunc(&host.trim_matches(|c| c == '[' || c == ']').to_lowercase(), 253);
    let Ok(port) = port.trim().parse::<i64>() else { return Parsed::Unparsed };
    let src_port = source.rsplit_once(':').and_then(|x| x.1.parse::<u16>().ok());
    let src = source.rsplit_once(':').map(|x| x.0).unwrap_or(&source).trim_matches(|c| c == '[' || c == ']');
    let Some(src_ip) = normalized_ip(src) else { return Parsed::Unparsed };
    if host.is_empty() || !(1..=65535).contains(&port) {
        return Parsed::Unparsed;
    }
    if src_ip.is_loopback() {
        return Parsed::Loopback;
    }
    let kind = if host.parse::<IpAddr>().is_ok() { "ip" } else { "domain" };
    let blocked = outbound == "blocked" || status == "rejected" || blackholes.contains(&outbound.as_str());
    Parsed::Event(Event {
        event_id,
        ts,
        inbound: inbound.clone(),
        outbound: trunc(outbound, 120),
        source_ip: src_ip.to_string(),
        source_port: src_port,
        host,
        port: port as u16,
        protocol: protocol.to_string(),
        kind,
        action: if blocked { "blocked" } else { "routed" },
        status,
        client: trunc(&label, 120),
    })
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Cursor {
    inode: u64,
    offset: u64,
}

#[derive(Default, Clone, Debug)]
pub struct Counters {
    pub lines: u64,
    pub events: u64,
    pub loopback: u64,
    pub unparsed: u64,
    pub dropped_lines: u64,
    pub rotation_gaps: u64,
    pub truncations: u64,
}

pub struct AccessTail {
    pub path: Option<PathBuf>,
    state_path: Option<PathBuf>,
    committed: Cursor,
    staged: Option<Cursor>,
    c: Counters,
    staged_c: Option<Counters>,
    pub max_events: usize,
    pub max_bytes: u64,
    last_err: Option<String>,
}

impl AccessTail {
    pub fn new(state_dir: Option<&Path>) -> AccessTail {
        let mut t = AccessTail {
            path: None,
            state_path: state_dir.map(|d| d.join("access.json")),
            committed: Cursor::default(),
            staged: None,
            c: Counters::default(),
            staged_c: None,
            max_events: 1000,
            max_bytes: 2 * 1024 * 1024,
            last_err: None,
        };
        if let Some(p) = &t.state_path {
            if let Ok(v) = std::fs::read_to_string(p).map(|s| serde_json::from_str::<Value>(&s).unwrap_or(Value::Null)) {
                t.committed = Cursor { inode: v["inode"].as_u64().unwrap_or(0), offset: v["offset"].as_u64().unwrap_or(0) };
                let g = |k: &str| v["counters"][k].as_u64().unwrap_or(0);
                t.c = Counters {
                    lines: g("lines"),
                    events: g("events"),
                    loopback: g("loopback"),
                    unparsed: g("unparsed"),
                    dropped_lines: g("dropped_lines"),
                    rotation_gaps: g("rotation_gaps"),
                    truncations: g("truncations"),
                };
                if v["path"].is_string() {
                    t.path = v["path"].as_str().map(PathBuf::from);
                }
            }
        }
        t
    }

    pub fn set_path(&mut self, p: Option<String>) {
        let np = p.filter(|s| !s.is_empty() && s != "none").map(PathBuf::from);
        if np != self.path {
            info!("access_log_path", "path={}", crate::log::q(&np.as_ref().map(|p| p.display().to_string()).unwrap_or_default()));
            if self.path.is_some() {
                // 换了文件：旧游标作废（新文件从当前末尾开始，避免把整份旧日志当新事件）
                self.committed = Cursor::default();
            }
            self.path = np;
        }
    }

    /// 读一批（不提交游标）。`paused` 为真时只返回状态（背压）。
    pub fn read(&mut self, blackholes: &[&str], paused: bool) -> Option<Value> {
        let path = self.path.clone()?;
        let md = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                let msg = format!("{}: {e}", path.display());
                if self.last_err.as_deref() != Some(&msg) {
                    warn!("access_log_unreadable", "err={}", crate::log::q(&msg));
                }
                self.last_err = Some(msg.clone());
                return Some(json!({"schema": "fleet.access.v1", "source": {"file": path}, "error": msg, "dropped": 0, "events": []}));
            }
        };
        self.last_err = None;
        let (inode, size) = (md.ino(), md.len());
        let mut c = self.c.clone();
        let mut cur = self.committed.clone();
        let mut events = vec![];
        if cur.inode == 0 {
            // 第一次：从当前末尾开始（旧日志已由旧采集器入库，不重放）
            cur = Cursor { inode, offset: size };
        }
        let offset_from = cur.clone();
        let mut segments: Vec<(PathBuf, u64, u64)> = vec![]; // (路径, 起点, inode)
        if cur.inode != inode {
            match find_rotated(&path, cur.inode) {
                Some(old) => segments.push((old, cur.offset, cur.inode)),
                None => {
                    c.rotation_gaps += 1;
                    warn!("access_log_rotation_gap", "old_inode={} note={}", cur.inode, crate::log::q("轮转后找不到旧文件（已压缩或删除），其剩余部分未读"));
                }
            }
            cur = Cursor { inode, offset: 0 };
        }
        if cur.offset > size {
            c.truncations += 1;
            warn!("access_log_truncated", "offset={} size={size}", cur.offset);
            cur.offset = 0;
        }
        segments.push((path.clone(), cur.offset, inode));
        let mut budget = self.max_bytes;
        let mut next = cur.clone();
        if !paused {
            'outer: for (p, start, ino) in segments {
                let Ok(mut f) = std::fs::File::open(&p) else { continue };
                if f.seek(SeekFrom::Start(start)).is_err() {
                    continue;
                }
                let mut r = BufReader::with_capacity(64 * 1024, f);
                let mut pos = start;
                let mut buf = Vec::with_capacity(512);
                loop {
                    if events.len() >= self.max_events || budget == 0 {
                        next = Cursor { inode: ino, offset: pos };
                        break 'outer;
                    }
                    buf.clear();
                    let n = match (&mut r).take(MAX_LINE as u64).read_until(b'\n', &mut buf) {
                        Ok(n) => n,
                        Err(_) => 0,
                    };
                    if n == 0 {
                        break;
                    }
                    if buf.last() != Some(&b'\n') {
                        if n >= MAX_LINE {
                            // 超长畸形行：跳到行尾，丢弃并计数
                            let mut skipped = n as u64;
                            loop {
                                buf.clear();
                                let m = (&mut r).take(MAX_LINE as u64).read_until(b'\n', &mut buf).unwrap_or(0);
                                skipped += m as u64;
                                if m == 0 || buf.last() == Some(&b'\n') {
                                    break;
                                }
                            }
                            pos += skipped;
                            budget = budget.saturating_sub(skipped);
                            c.dropped_lines += 1;
                            continue;
                        }
                        // 半行（Xray 还在写）：停在行首，下次再读（与旧 bridge 相同：旧文件末尾的半行在轮转后不再等）
                        break;
                    }
                    let line_start = pos;
                    pos += n as u64;
                    budget = budget.saturating_sub(n as u64);
                    c.lines += 1;
                    let s = String::from_utf8_lossy(&buf);
                    match parse_line(&s, format!("{ino}:{line_start}"), blackholes) {
                        Parsed::Event(e) => {
                            c.events += 1;
                            events.push(e.to_json());
                        }
                        Parsed::Loopback => c.loopback += 1,
                        Parsed::Unparsed => c.unparsed += 1,
                    }
                }
                next = Cursor { inode: ino, offset: pos };
            }
        }
        let backlog = if next.inode == inode { size.saturating_sub(next.offset) } else { size };
        let dropped_now = c.dropped_lines - self.c.dropped_lines;
        self.staged = Some(next.clone());
        self.staged_c = Some(c.clone());
        Some(json!({
            "schema": "fleet.access.v1",
            "source": {"file": path, "inode": next.inode, "offset_from": if offset_from.inode == next.inode { offset_from.offset } else { 0 }, "offset_to": next.offset},
            "dropped": dropped_now,
            "events": events,
            "backlog_bytes": backlog,
            "size_bytes": size,
            "paused": paused,
            "counters": {"lines": c.lines, "events": c.events, "loopback": c.loopback, "unparsed": c.unparsed,
                         "dropped_lines": c.dropped_lines, "rotation_gaps": c.rotation_gaps, "truncations": c.truncations},
        }))
    }

    /// 批次写进 outbox 之后：提交游标与计数
    pub fn commit(&mut self) {
        let (Some(cur), Some(c)) = (self.staged.take(), self.staged_c.take()) else { return };
        self.committed = cur;
        self.c = c;
        if let Some(p) = &self.state_path {
            let v = json!({"path": self.path, "inode": self.committed.inode, "offset": self.committed.offset,
                "counters": {"lines": self.c.lines, "events": self.c.events, "loopback": self.c.loopback, "unparsed": self.c.unparsed,
                             "dropped_lines": self.c.dropped_lines, "rotation_gaps": self.c.rotation_gaps, "truncations": self.c.truncations}});
            let _ = write_atomic(p, v.to_string().as_bytes());
        }
    }
}

fn find_rotated(path: &Path, inode: u64) -> Option<PathBuf> {
    let dir = path.parent()?;
    let base = path.file_name()?.to_string_lossy().to_string() + ".";
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if n.starts_with(&base) && !n.ends_with(".gz") && e.metadata().map(|m| m.ino() == inode).unwrap_or(false) {
            return Some(e.path());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(line: &str) -> Parsed {
        parse_line(line, "1:0".into(), &["fleet-default-deny"])
    }

    #[test]
    fn parses_like_legacy() {
        let Parsed::Event(e) = ev("2026/09/23 15:41:03.330897 from 203.0.113.9:29518 accepted tcp:27-courier2.push.apple.com:5223 [inbound-20005 -> direct] email: main\n") else {
            panic!()
        };
        assert_eq!((e.inbound.as_str(), e.outbound.as_str(), e.source_ip.as_str(), e.host.as_str(), e.port, e.protocol.as_str(), e.kind, e.action, e.client.as_str()),
            ("inbound-20005", "direct", "203.0.113.9", "27-courier2.push.apple.com", 5223, "tcp", "domain", "routed", "main"));
        // IPv6 映射来源、IP 目标、blocked、无 email、>> 分隔
        let Parsed::Event(e) = ev("2026/09/23 15:41:03 [::ffff:198.51.100.7]:1 accepted udp:[2001:DB8::1]:443 [inbound-20001 >> blocked]\n") else { panic!() };
        assert_eq!((e.source_ip.as_str(), e.host.as_str(), e.kind, e.action, e.client.as_str(), e.protocol.as_str()), ("198.51.100.7", "2001:db8::1", "ip", "blocked", "", "udp"));
        // R1 默认拒绝
        let Parsed::Event(e) = ev("2026/09/23 15:41:03 from 198.51.100.7:1 accepted tcp:x.com:80 [inbound-20001 -> fleet-default-deny]\n") else { panic!() };
        assert_eq!(e.action, "blocked");
        // rejected
        let Parsed::Event(e) = ev("2026/09/23 15:41:03 from 198.51.100.7:1 rejected tcp:x.com:80 reason [inbound-20001 -> ss-1]\n") else { panic!() };
        assert_eq!(e.action, "blocked");
    }

    #[test]
    fn rejects_like_legacy() {
        assert!(matches!(ev("2026/09/23 15:40:51.2 from 127.0.0.1:43168 accepted tcp:www.cloudflare.com:443 [inbound-20012 -> ss-7] email: node-36\n"), Parsed::Loopback));
        for l in [
            "2026/09/23 15:41:03 from 1.2.3.4:1 accepted tcp:x.com:80 [api -> api]\n",
            "2026/09/23 15:41:03 from 1.2.3.4:1 accepted tcp:x.com:80 [inbound-1 -> a]\n",
            "2026/09/23 15:41:03 from 1.2.3.4:1 accepted tcp:x.com:0 [inbound-20001 -> a]\n",
            "2026/09/23 15:41:03 from 1.2.3.4:1 accepted icmp:x.com:1 [inbound-20001 -> a]\n",
            "2026/09/23 15:41:03 from 1.2.3.4:1 accepted tcp:x.com:80 [inbound-20001 -> a -> b]\n",
            "2026/09/23 15:41:03 from nothost:1 accepted tcp:x.com:80 [inbound-20001 -> a]\n",
            "garbage\n",
        ] {
            assert!(matches!(ev(l), Parsed::Unparsed), "{l}");
        }
    }

    #[test]
    fn tail_rotation_truncation_oversize() {
        use std::io::Write;
        let d = std::env::temp_dir().join(format!("fe-al-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let log = d.join("xray-access.log");
        let line = |n: u32| format!("2026/09/23 15:41:03 from 198.51.100.7:{n} accepted tcp:h{n}.com:443 [inbound-20001 -> direct] email: c\n");
        std::fs::write(&log, "").unwrap();
        let mut t = AccessTail::new(Some(&d));
        t.set_path(Some(log.display().to_string()));
        t.max_events = 3;
        let v = t.read(&[], false).unwrap();
        assert_eq!(v["events"].as_array().unwrap().len(), 0);
        t.commit();
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        for n in 1..=5 {
            f.write_all(line(n).as_bytes()).unwrap();
        }
        f.write_all(&vec![b'x'; MAX_LINE + 10]).unwrap();
        f.write_all(b"\n").unwrap();
        f.write_all(line(6).as_bytes()).unwrap();
        f.write_all(b"2026/09/23 15:41:03 from 198.51.100.7:7 acc").unwrap(); // 半行
        drop(f);
        let v = t.read(&[], false).unwrap();
        assert_eq!(v["events"].as_array().unwrap().len(), 3);
        // 不提交 → 重读同样的 3 条（崩溃重放）
        let v2 = t.read(&[], false).unwrap();
        assert_eq!(v["events"], v2["events"]);
        t.commit();
        let v = t.read(&[], false).unwrap();
        let ids: Vec<String> = v["events"].as_array().unwrap().iter().map(|e| e["host"].as_str().unwrap().to_string()).collect();
        assert_eq!(ids, vec!["h4.com", "h5.com", "h6.com"]);
        assert_eq!(v["counters"]["dropped_lines"], 1);
        t.commit();
        // 轮转：rename + create；旧文件的半行补全后要能读到
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(b"epted tcp:h7.com:443 [inbound-20001 -> direct]\n").unwrap();
        drop(f);
        std::fs::rename(&log, d.join("xray-access.log.1")).unwrap();
        std::fs::write(&log, line(8)).unwrap();
        let v = t.read(&[], false).unwrap();
        let hosts: Vec<&str> = v["events"].as_array().unwrap().iter().map(|e| e["host"].as_str().unwrap()).collect();
        assert_eq!(hosts, vec!["h7.com", "h8.com"]);
        assert_eq!(v["counters"]["rotation_gaps"], 0);
        t.commit();
        // 截断
        std::fs::write(&log, "").unwrap();
        let v = t.read(&[], false).unwrap();
        assert_eq!(v["counters"]["truncations"], 1);
        t.commit();
        // 轮转且旧文件已不在
        std::fs::remove_file(d.join("xray-access.log.1")).unwrap();
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(line(9).as_bytes()).unwrap();
        drop(f);
        t.commit();
        std::fs::rename(&log, d.join("gone")).unwrap();
        std::fs::remove_file(d.join("gone")).unwrap();
        std::fs::write(&log, line(10)).unwrap();
        let v = t.read(&[], false).unwrap();
        assert_eq!(v["counters"]["rotation_gaps"], 1);
        assert_eq!(v["events"].as_array().unwrap().len(), 1);
        // 背压：暂停时不读
        t.commit();
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(line(11).as_bytes()).unwrap();
        drop(f);
        let v = t.read(&[], true).unwrap();
        assert_eq!(v["events"].as_array().unwrap().len(), 0);
        assert!(v["backlog_bytes"].as_u64().unwrap() > 0);
    }
}
