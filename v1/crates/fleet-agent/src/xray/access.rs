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
use std::path::{Path, PathBuf};

const MAX_LINE: usize = 32 * 1024;
const MAX_WINDOW: u64 = 2 * 1024 * 1024;
/// 按每条事件这么多字节估算一次读多少（正常一行 200 字节左右，留出非入口流量行的余量）
const BYTES_PER_EVENT: u64 = 512;

/// 窗口下限：两倍行上限，超长行多半能整行装进一个窗口里丢掉
const MIN_WINDOW: u64 = 2 * MAX_LINE as u64;

/// 一次读取的窗口：按条数估算，夹在 MIN_WINDOW 与 MAX_WINDOW 之间
fn window_for(max_events: usize) -> u64 {
    (max_events as u64).saturating_mul(BYTES_PER_EVENT).clamp(MIN_WINDOW, MAX_WINDOW)
}

#[derive(Debug)]
pub enum Parsed {
    Event(AccessEvent),
    /// 合法但不是入口流量：探测入站等，以及 Xray 拒绝的连接（扫描器、协议不对的 `rejected` 行，没有目标可记）
    Other,
    Bad,
    /// 目标主机名含非法字符（控制字符、NUL、非 ASCII…）：整行丢弃，计入 dropped_lines，不让坏字节进上报
    BadHost,
}

/// 主机名 / IP 只允许这些字符：字母数字与 `-._:`（IPv6 去掉方括号后含 `:`）
fn host_ok(h: &str) -> bool {
    h.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b':'))
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
    let ms: i64 = if frac.is_empty() {
        0
    } else {
        // 只认 ASCII 数字：按字节切多字节字符会 panic，游标落盘前崩溃会导致重启后无限崩溃
        let head = frac.get(..frac.len().min(3))?;
        if !head.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        format!("{head:0<3}").parse().ok()?
    };
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
        return Parsed::Other;
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
    if !host_ok(&host) {
        return Parsed::BadHost;
    }
    // 路由 [in -> out]。目标（客户给的域名）里可以带空格，伪造出 `[in-别人 -> …] email: e-别人`：伪造的只能在真路由之前，
    // 所以取最后一组；它前面还有别的 token（目标被空格断开了）就整行丢弃
    let tail = &tok[i + 2..];
    let rs = tail.iter().rposition(|t| t.starts_with('['));
    if rs.is_some_and(|s| s > 0) {
        return Parsed::BadHost;
    }
    let mut route_end = 0;
    let route: Option<String> = rs.and_then(|s| {
        let mut buf = String::new();
        for (k, t) in tail.iter().enumerate().skip(s) {
            buf.push_str(t);
            buf.push(' ');
            if t.ends_with(']') {
                route_end = k + 1;
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
    // 入口按入站 tag 认；有 email（路由之后、最后一个）就必须是同一个入口
    let Some(entry_id) = tags::parse_entry_id(&inbound) else { return Parsed::Bad };
    let email = tail[route_end..].iter().rposition(|t| *t == "email:").map(|k| tail.get(route_end + k + 1).copied());
    if email.is_some_and(|e| e.and_then(tags::parse_entry_id) != Some(entry_id)) {
        return Parsed::Bad;
    }
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

/// 指纹最多取文件开头多少字节
const FP_MAX: usize = 256;

/// 文件开头的指纹：前 `len` 字节的 FNV-1a 哈希。访问日志只追加，开头不会变；
/// 开头变了 = 轮转（rename 或 copytruncate）或被截断重写，光靠 inode/偏移分不清 copytruncate。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Fp {
    pub len: u32,
    pub hash: u64,
}

fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for x in b {
        h ^= *x as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn read_head(path: &Path, n: usize) -> Option<Vec<u8>> {
    let f = std::fs::File::open(path).ok()?;
    let mut v = Vec::with_capacity(n);
    f.take(n as u64).read_to_end(&mut v).ok()?;
    Some(v)
}

/// 取文件开头的指纹；空文件没有
fn fingerprint(path: &Path) -> Option<Fp> {
    let h = read_head(path, FP_MAX)?;
    (!h.is_empty()).then(|| Fp { len: h.len() as u32, hash: fnv1a(&h) })
}

/// 文件开头是否仍与指纹一致（文件比指纹短也算不一致）
fn fp_matches(path: &Path, fp: Fp) -> bool {
    read_head(path, fp.len as usize).is_some_and(|h| h.len() == fp.len as usize && fnv1a(&h) == fp.hash)
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Cursor {
    pub inode: u64,
    pub offset: u64,
    /// 游标所指文件的开头指纹；None = 还没取到（文件刚出现 / 刚切到新文件），读过一次后补上
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fp: Option<Fp>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Counters {
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
            None => Cursor { inode, offset: if self.seen_missing { 0 } else { size }, fp: None },
        };
        // 当前路径上的文件还是游标指向的那个吗？以指纹为准（能识别 copytruncate：inode 不变、
        // 新文件长过旧偏移时偏移判断会漏）；还没有指纹时只看偏移是否超出文件大小。
        let rotated = cur.offset > size || cur.fp.is_some_and(|fp| !fp_matches(&self.path, fp));
        let (read_path, mut cur) = if !rotated {
            (self.path.clone(), cur)
        } else {
            // 找轮转出去的旧内容 access.log.1（rename 轮转是同一个文件；copytruncate 是拷贝，开头相同）
            let old = self.old_path();
            let old_ok = cur.fp.is_some_and(|fp| std::fs::metadata(&old).is_ok_and(|m| m.len() >= cur.offset && fp_matches(&old, fp)));
            if old_ok {
                tracing::info!("访问日志已轮转，先读完 {} 里剩余部分，再从新文件开头读", old.display());
                (old, cur)
            } else {
                if cur.inode == inode {
                    self.counters.truncations += 1;
                    tracing::warn!("访问日志被截断/重写且找不到匹配的 .1，从新文件开头读，可能漏掉一段");
                } else {
                    self.counters.rotation_gaps += 1;
                    tracing::warn!("访问日志轮转后找不到旧文件，可能漏掉一段");
                }
                (self.path.clone(), Cursor { inode, offset: 0, fp: None })
            }
        };
        let fsize = std::fs::metadata(&read_path).map(|m| m.len()).unwrap_or(0);
        if cur.offset > fsize {
            // 两次 stat 之间又被截断了
            self.counters.truncations += 1;
            cur.offset = 0;
            cur.fp = None;
        }
        let window = window_for(max_events);
        let want = (fsize - cur.offset).min(window) as usize;
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
            if raw.len() > MAX_LINE {
                self.counters.dropped_lines += 1;
                continue;
            }
            match parse_line(&String::from_utf8_lossy(raw), utc_offset_s) {
                Parsed::Event(e) => events.push(e),
                Parsed::Other => {}
                Parsed::Bad => self.counters.unparsed += 1,
                Parsed::BadHost => self.counters.dropped_lines += 1,
            }
        }
        let reading_old = read_path != self.path;
        // 旧文件末尾那截没有换行的残行（拷贝时恰好写到一半）永远不会被补全，不丢掉游标就卡在旧文件上
        if reading_old && !hit_cap && cur.offset + want as u64 >= fsize && consumed < buf.len() {
            self.counters.dropped_lines += 1;
            consumed = buf.len();
        }
        let mut next = Cursor { inode: cur.inode, offset: cur.offset + consumed as u64, fp: cur.fp };
        // 指纹：还没有，或文件原先太短、现在能取更长的（开头不变，所以升级是安全的）
        if next.fp.is_none_or(|f| (f.len as usize) < FP_MAX) {
            if let Some(f) = fingerprint(&read_path) {
                next.fp = Some(f);
            }
        }
        let at_eof = next.offset >= fsize;
        // 旧文件读完了 → 切到新文件开头
        if reading_old && at_eof {
            next = Cursor { inode, offset: 0, fp: None };
        }
        let more = hit_cap || (want as u64 == window && !at_eof) || (reading_old && !at_eof);
        Ok(Batch { events, next: Some(next), more })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use uuid::Uuid;

    #[test]
    fn timestamps() {
        // 2026-10-20 00:00:00 UTC = 1792454400
        assert_eq!(parse_ts("2026/10/20", "00:00:00", 0), Some(1_792_454_400_000));
        // UTC+8 的 08:00:00.5 就是同一时刻
        assert_eq!(parse_ts("2026/10/20", "08:00:00.5", 8 * 3600), Some(1_792_454_400_500));
        assert_eq!(parse_ts("1970/01/01", "00:00:01.123456", 0), Some(1123));
        assert_eq!(parse_ts("2026/13/20", "00:00:00", 0), None);
        // 多字节字符落在第 3 个字节上：按字节切会 panic
        assert_eq!(parse_ts("2026/10/20", "00:00:00.12é", 0), None);
        assert_eq!(parse_ts("2026/10/20", "00:00:00.é", 0), None);
        assert_eq!(parse_ts("2026/10/20", "00:00:00.1x3", 0), None);
        assert!(matches!(parse_line("2026/10/20 00:00:00.12é from 1.2.3.4:5 accepted tcp:a.com:443", 0), Parsed::Bad));
    }

    fn id() -> Uuid {
        Uuid::from_u128(0xabc)
    }

    #[test]
    fn invalid_host_chars_drop_the_line() {
        let line = |h: &str| format!("2026/10/20 13:50:56 from 1.2.3.4:5678 accepted tcp:{h}:443 [in-{} -> out-x] email: e-{}", id().simple(), id().simple());
        for h in ["bad\u{0}host", "a\u{7}b.com", "例子.com", "a\u{fffd}b"] {
            assert!(matches!(parse_line(&line(h), 0), Parsed::BadHost), "{h:?}");
        }
        for h in ["a_b-c.example.com", "[2001:db8::1]", "10.0.0.1"] {
            assert!(matches!(parse_line(&line(h), 0), Parsed::Event(_)), "{h:?}");
        }
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

    /// 客户在目标域名里塞进 `[in-别人 -> out-别人] email: e-别人`：不能记到别的入口头上
    #[test]
    fn forged_route_in_destination_is_not_attributed() {
        let (me, other) = (id().simple(), Uuid::from_u128(0xdef).simple());
        let forged = format!("a.com:443 [in-{other} -> out-{other}] email: e-{other} b.com");
        let l = format!("2026/10/20 13:50:56 from 1.2.3.4:5 accepted tcp:{forged}:443 [in-{me} -> out-{me}] email: e-{me}");
        assert!(matches!(parse_line(&l, 0), Parsed::BadHost), "{l}");
        // 伪造的在真路由之前、没有空格隔开以外的东西也一样
        let l = format!("2026/10/20 13:50:56 from 1.2.3.4:5 accepted tcp:a.com:443 [in-{other} -> out-{other}] email: e-{other} [in-{me} -> out-{me}] email: e-{me}");
        assert!(matches!(parse_line(&l, 0), Parsed::BadHost), "{l}");
        // email 与入站不是同一个入口：坏行
        let l = format!("2026/10/20 13:50:56 from 1.2.3.4:5 accepted tcp:a.com:443 [in-{me} -> out-{me}] email: e-{other}");
        assert!(matches!(parse_line(&l, 0), Parsed::Bad), "{l}");
        let l = format!("2026/10/20 13:50:56 from 1.2.3.4:5 accepted tcp:a.com:443 [in-{me} -> out-{me}] email:");
        assert!(matches!(parse_line(&l, 0), Parsed::Bad), "{l}");
        // 取最后一个 email
        let l = format!("2026/10/20 13:50:56 from 1.2.3.4:5 accepted tcp:a.com:443 [in-{me} -> out-{me}] email: e-{other} email: e-{me}");
        let Parsed::Event(e) = parse_line(&l, 0) else { panic!("{l}") };
        assert_eq!(e.entry_id, id());
    }

    #[test]
    fn other_and_bad() {
        let probe = format!("2026/10/20 13:50:56 from 127.0.0.1:5 accepted tcp:a.com:80 [probe-{} -> out-x]", id().simple());
        assert!(matches!(parse_line(&probe, 0), Parsed::Other));
        assert!(matches!(parse_line("garbage", 0), Parsed::Bad));
        // 扫描器被 Xray 拒绝：不算解析失败，否则 access_unparsed 永远不为 0
        assert!(matches!(parse_line("2026/10/20 13:50:56 from 1.2.3.4:5 rejected proxy/vless/inbound: invalid request", 0), Parsed::Other));
        assert!(matches!(parse_line("2026/10/20 13:50:56 from 1.2.3.4:5 accepted tcp:a.com:99999 [in-x -> y]", 0), Parsed::Bad));
    }

    fn tmp(n: &str) -> crate::util::TmpDir { crate::util::TmpDir::new(&format!("acc-{n}")) }

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
        writeln!(f, "0:56 from 1.2.3.4:5 accepted tcp:d.com:80 [in-{} -> out-x]", id().simple()).unwrap();
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
    }

    #[test]
    fn overlong_line_is_dropped_not_stuck() {
        let d = tmp("long");
        let p = d.join("access.log");
        let mut t = Tailer::new(p.clone(), Some(Cursor { inode: std::fs::metadata(&d).unwrap().ino(), offset: 0, fp: None }));
        std::fs::write(&p, vec![b'x'; 40 * 1024]).unwrap();
        t.cursor = Some(Cursor { inode: std::fs::metadata(&p).unwrap().ino(), offset: 0, fp: None });
        let b = t.read_batch(10, 0).unwrap();
        assert_eq!(t.counters.dropped_lines, 1);
        assert_eq!(b.next.unwrap().offset, 40 * 1024);
    }

    fn hosts(b: &Batch) -> Vec<&str> {
        b.events.iter().map(|e| e.dest_host.as_str()).collect()
    }

    /// 与 line() 等长的主机名，保证各行字节数相同，便于构造"新文件比旧偏移还长"
    fn h(c: char) -> String {
        format!("{c}{c}{c}.com")
    }

    #[test]
    fn copytruncate_reads_rest_of_copy_then_new_file() {
        let d = tmp("ct");
        let p = d.join("access.log");
        std::fs::write(&p, line(&h('0'))).unwrap();
        let mut t = Tailer::new(p.clone(), None);
        let b = t.read_batch(10, 0).unwrap();
        t.commit(b.next);
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        write!(f, "{}{}", line(&h('a')), line(&h('b'))).unwrap();
        let b = t.read_batch(10, 0).unwrap();
        assert_eq!(hosts(&b), ["aaa.com", "bbb.com"]);
        t.commit(b.next);
        // 还有一行没读，logrotate copytruncate：拷贝到 .1，再原地截断；新文件很快长过旧偏移
        write!(f, "{}", line(&h('c'))).unwrap();
        drop(f);
        std::fs::copy(&p, d.join("access.log.1")).unwrap();
        std::fs::OpenOptions::new().write(true).truncate(true).open(&p).unwrap();
        std::fs::write(&p, format!("{}{}{}{}", line(&h('d')), line(&h('e')), line(&h('f')), line(&h('g')))).unwrap();
        assert!(std::fs::metadata(&p).unwrap().len() > t.cursor.unwrap().offset, "新文件要比旧偏移长，旧实现在这里会读错位置");
        let b = t.read_batch(10, 0).unwrap();
        assert_eq!(hosts(&b), ["ccc.com"]); // 先读完 .1 里没读的
        t.commit(b.next);
        let b = t.read_batch(10, 0).unwrap();
        assert_eq!(hosts(&b), ["ddd.com", "eee.com", "fff.com", "ggg.com"]); // 再从新文件 0 开始
        t.commit(b.next);
        assert_eq!((t.counters.rotation_gaps, t.counters.truncations), (0, 0));
        // 之后继续正常追加
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        write!(f, "{}", line(&h('h'))).unwrap();
        assert_eq!(hosts(&t.read_batch(10, 0).unwrap()), ["hhh.com"]);
    }

    #[test]
    fn copytruncate_without_copy_restarts_from_zero_and_counts() {
        let d = tmp("ct-nocopy");
        let p = d.join("access.log");
        std::fs::write(&p, format!("{}{}", line(&h('0')), line(&h('1')))).unwrap();
        let mut t = Tailer::new(p.clone(), None);
        let b = t.read_batch(10, 0).unwrap();
        t.commit(b.next);
        // 被截断后又长过旧偏移，没有 .1：只能从头读，记一次截断
        std::fs::write(&p, format!("{}{}{}", line(&h('a')), line(&h('b')), line(&h('c')))).unwrap();
        let b = t.read_batch(10, 0).unwrap();
        assert_eq!(hosts(&b), ["aaa.com", "bbb.com", "ccc.com"]);
        assert_eq!(t.counters.truncations, 1);
    }

    #[test]
    fn rename_rotation_with_partial_tail_in_old_file_does_not_stick() {
        let d = tmp("rot-partial");
        let p = d.join("access.log");
        std::fs::write(&p, line(&h('0'))).unwrap();
        let mut t = Tailer::new(p.clone(), None);
        let b = t.read_batch(10, 0).unwrap();
        t.commit(b.next);
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        write!(f, "{}2026/10/20 13:5", line(&h('a'))).unwrap(); // 末尾是半行
        drop(f);
        std::fs::rename(&p, d.join("access.log.1")).unwrap();
        std::fs::write(&p, line(&h('n'))).unwrap();
        let b = t.read_batch(10, 0).unwrap();
        assert_eq!(hosts(&b), ["aaa.com"]);
        t.commit(b.next);
        let b = t.read_batch(10, 0).unwrap();
        assert_eq!(hosts(&b), ["nnn.com"]);
        assert_eq!(t.counters.dropped_lines, 1);
    }

    #[test]
    fn window_follows_max_events_and_reports_more() {
        assert_eq!(window_for(2000), 2000 * BYTES_PER_EVENT);
        assert_eq!(window_for(1), MIN_WINDOW);
        assert_eq!(window_for(usize::MAX), MAX_WINDOW);
        // 窗口装满了非入口流量的行（不计条数）：读一窗、报还有积压，下次接着读
        let d = tmp("win");
        let p = d.join("access.log");
        let probe = format!("2026/10/20 13:50:56 from 127.0.0.1:5 accepted tcp:a.com:80 [probe-{} -> out-x]\n", id().simple());
        let n = MIN_WINDOW as usize / probe.len() + 10;
        std::fs::write(&p, format!("{}{}", probe.repeat(n), line(&h('z')))).unwrap();
        let mut t = Tailer::new(p.clone(), Some(Cursor { inode: std::fs::metadata(&p).unwrap().ino(), offset: 0, fp: None }));
        let b = t.read_batch(1, 0).unwrap();
        assert!(b.events.is_empty() && b.more);
        assert!(b.next.unwrap().offset <= MIN_WINDOW);
        t.commit(b.next);
        let b = t.read_batch(1, 0).unwrap();
        assert_eq!(hosts(&b), ["zzz.com"]);
        assert!(!b.more);
    }

    #[test]
    fn cursor_serde_without_fp() {
        // 没有 fp 字段的游标文件照常读入（fp = None），序列化时也不带 fp
        let c: Cursor = serde_json::from_str(r#"{"inode":5,"offset":10}"#).unwrap();
        assert_eq!(c, Cursor { inode: 5, offset: 10, fp: None });
        assert_eq!(serde_json::to_string(&c).unwrap(), r#"{"inode":5,"offset":10}"#);
        let c2 = Cursor { inode: 1, offset: 2, fp: Some(Fp { len: 3, hash: 4 }) };
        assert_eq!(serde_json::from_str::<Cursor>(&serde_json::to_string(&c2).unwrap()).unwrap(), c2);
    }

    #[test]
    fn cursor_without_fp_reads_and_gets_fp() {
        let d = tmp("nofp");
        let p = d.join("access.log");
        std::fs::write(&p, line(&h('0'))).unwrap();
        let ino = std::fs::metadata(&p).unwrap().ino();
        let off = std::fs::metadata(&p).unwrap().len();
        let mut t = Tailer::new(p.clone(), Some(Cursor { inode: ino, offset: off, fp: None }));
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        write!(f, "{}", line(&h('a'))).unwrap();
        drop(f);
        let b = t.read_batch(10, 0).unwrap();
        assert_eq!(hosts(&b), ["aaa.com"]);
        assert!(b.next.unwrap().fp.is_some()); // 读过一次就补上了指纹
    }
}
