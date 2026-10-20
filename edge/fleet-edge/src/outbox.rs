//! 有界磁盘队列（单文件环）：`outbox.dat`
//!
//! 文件头 32 字节：magic "FEOB" | ver u32 | head u64（第一条未确认记录的偏移）| dropped u64 | reserved u64
//! 记录：len u32 LE | crc32 u32 LE | payload（一批 JSON）
//!
//! - `push`：追加 + fdatasync。未确认字节数 + 新记录超过上限时，从头丢最旧的记录（`dropped` 累加）。
//! - `ack`：只 pwrite 头里的 head（不 fsync：崩溃后最多重发一批，中控按 (boot_id, seq) 去重）。
//! - 全部确认后截断为空；文件将超过 上限+32 B 时压缩（写新文件 + rename，原子），所以磁盘占用 ≤ 上限 + 32 B。
//! - 打开时从 head 起逐条校验 crc，遇到撕裂的尾部就截掉。
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 4] = b"FEOB";
const HDR: u64 = 32;
const REC_HDR: u64 = 8;

pub struct Outbox {
    path: PathBuf,
    f: File,
    head: u64,
    len: u64,
    dropped: u64,
    max: u64,
    /// 从 head 开始每条记录的长度（含记录头），用于计数和丢最旧
    recs: std::collections::VecDeque<u64>,
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut c: u32 = 0xFFFF_FFFF;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
        }
    }
    !c
}

fn write_header(f: &File, head: u64, dropped: u64) -> std::io::Result<()> {
    let mut h = [0u8; HDR as usize];
    h[0..4].copy_from_slice(MAGIC);
    h[4..8].copy_from_slice(&1u32.to_le_bytes());
    h[8..16].copy_from_slice(&head.to_le_bytes());
    h[16..24].copy_from_slice(&dropped.to_le_bytes());
    f.write_all_at(&h, 0)
}

impl Outbox {
    pub fn open(dir: &Path, max: u64) -> std::io::Result<Outbox> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("outbox.dat");
        let _ = std::fs::remove_file(dir.join("outbox.dat.tmp"));
        let mut f = OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(&path)?;
        let mut len = f.metadata()?.len();
        let (mut head, mut dropped) = (HDR, 0u64);
        let mut hdr = [0u8; HDR as usize];
        if len >= HDR && f.read_exact_at(&mut hdr, 0).is_ok() && &hdr[0..4] == MAGIC {
            head = u64::from_le_bytes(hdr[8..16].try_into().unwrap()).clamp(HDR, len);
            dropped = u64::from_le_bytes(hdr[16..24].try_into().unwrap());
        } else {
            f.set_len(0)?;
            write_header(&f, HDR, 0)?;
            f.sync_all()?;
            len = HDR;
        }
        // 校验 head 之后的记录
        let mut recs = std::collections::VecDeque::new();
        let mut off = head;
        f.seek(SeekFrom::Start(off))?;
        let mut rh = [0u8; REC_HDR as usize];
        let mut buf = vec![];
        while off + REC_HDR <= len {
            if f.read_exact(&mut rh).is_err() {
                break;
            }
            let n = u32::from_le_bytes(rh[0..4].try_into().unwrap()) as u64;
            let crc = u32::from_le_bytes(rh[4..8].try_into().unwrap());
            if n == 0 || n > max || off + REC_HDR + n > len {
                break;
            }
            buf.resize(n as usize, 0);
            if f.read_exact(&mut buf).is_err() || crc32(&buf) != crc {
                break;
            }
            recs.push_back(REC_HDR + n);
            off += REC_HDR + n;
        }
        if off < len {
            crate::warn!("outbox_truncated", "at={off} len={len}");
            f.set_len(off)?;
            len = off;
        }
        let mut ob = Outbox { path, f, head, len, dropped, max, recs };
        if ob.recs.is_empty() && ob.len > HDR {
            ob.reset()?;
        }
        Ok(ob)
    }

    pub fn pending(&self) -> usize {
        self.recs.len()
    }
    pub fn pending_bytes(&self) -> u64 {
        self.len - self.head
    }
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    fn reset(&mut self) -> std::io::Result<()> {
        self.f.set_len(HDR)?;
        self.head = HDR;
        self.len = HDR;
        write_header(&self.f, HDR, self.dropped)
    }

    pub fn push(&mut self, payload: &[u8]) -> std::io::Result<()> {
        let n = payload.len() as u64;
        if n == 0 || REC_HDR + n > self.max {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "记录过大"));
        }
        let mut dropped_now = 0;
        while self.pending_bytes() + REC_HDR + n > self.max {
            let Some(r) = self.recs.pop_front() else { break };
            self.head += r;
            self.dropped += 1;
            dropped_now += 1;
        }
        if dropped_now > 0 {
            crate::warn!("outbox_drop_oldest", "dropped_now={dropped_now} dropped_total={}", self.dropped);
            write_header(&self.f, self.head, self.dropped)?;
        }
        if self.recs.is_empty() {
            self.reset()?;
        } else if self.len + REC_HDR + n > self.max + HDR {
            self.compact()?;
        }
        let mut rec = Vec::with_capacity((REC_HDR + n) as usize);
        rec.extend_from_slice(&(n as u32).to_le_bytes());
        rec.extend_from_slice(&crc32(payload).to_le_bytes());
        rec.extend_from_slice(payload);
        self.f.write_all_at(&rec, self.len)?;
        self.f.sync_data()?;
        self.len += REC_HDR + n;
        self.recs.push_back(REC_HDR + n);
        Ok(())
    }

    /// 最旧的一条未确认记录
    pub fn peek(&self) -> std::io::Result<Option<Vec<u8>>> {
        let Some(&r) = self.recs.front() else { return Ok(None) };
        let mut buf = vec![0u8; (r - REC_HDR) as usize];
        self.f.read_exact_at(&mut buf, self.head + REC_HDR)?;
        Ok(Some(buf))
    }

    pub fn ack(&mut self) -> std::io::Result<()> {
        let Some(r) = self.recs.pop_front() else { return Ok(()) };
        self.head += r;
        if self.recs.is_empty() {
            self.reset()
        } else {
            write_header(&self.f, self.head, self.dropped)
        }
    }

    fn compact(&mut self) -> std::io::Result<()> {
        let tmp = self.path.with_extension("dat.tmp");
        let mut nf = OpenOptions::new().read(true).write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        write_header(&nf, HDR, self.dropped)?;
        nf.seek(SeekFrom::Start(HDR))?;
        let mut src = &self.f;
        src.seek(SeekFrom::Start(self.head))?;
        let mut take = src.take(self.len - self.head);
        std::io::copy(&mut take, &mut nf)?;
        nf.sync_all()?;
        std::fs::rename(&tmp, &self.path)?;
        let newlen = HDR + (self.len - self.head);
        self.f = nf;
        self.head = HDR;
        self.len = newlen;
        Ok(())
    }

    /// 仅测试用：文件实际大小
    pub fn file_len(&self) -> u64 {
        self.len
    }
}

impl Drop for Outbox {
    fn drop(&mut self) {
        let _ = self.f.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fe-ob-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn crc_known() {
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
    }

    #[test]
    fn push_peek_ack_reopen() {
        let d = tmpdir("a");
        {
            let mut ob = Outbox::open(&d, 4096).unwrap();
            for i in 0..5 {
                ob.push(format!("rec{i}").as_bytes()).unwrap();
            }
            assert_eq!(ob.peek().unwrap().unwrap(), b"rec0");
            ob.ack().unwrap();
            ob.ack().unwrap();
        }
        let mut ob = Outbox::open(&d, 4096).unwrap();
        assert_eq!(ob.pending(), 3);
        assert_eq!(ob.peek().unwrap().unwrap(), b"rec2");
        ob.ack().unwrap();
        ob.ack().unwrap();
        ob.ack().unwrap();
        assert_eq!(ob.pending(), 0);
        assert_eq!(ob.file_len(), HDR);
    }

    #[test]
    fn bounded_drop_oldest() {
        let d = tmpdir("b");
        let mut ob = Outbox::open(&d, 1024).unwrap();
        let payload = vec![b'x'; 100];
        for i in 0..50u8 {
            let mut p = payload.clone();
            p[0] = i;
            ob.push(&p).unwrap();
            assert!(ob.pending_bytes() <= 1024);
            assert!(ob.file_len() <= 1024 + HDR, "file {}", ob.file_len());
        }
        assert!(ob.dropped() > 0);
        let first = ob.peek().unwrap().unwrap()[0];
        assert_eq!(first as u64, ob.dropped());
        drop(ob);
        let ob = Outbox::open(&d, 1024).unwrap();
        assert_eq!(ob.peek().unwrap().unwrap()[0] as u64, ob.dropped());
    }

    #[test]
    fn torn_tail_truncated() {
        let d = tmpdir("c");
        {
            let mut ob = Outbox::open(&d, 4096).unwrap();
            ob.push(b"good").unwrap();
            ob.push(b"good2").unwrap();
        }
        // 模拟撕裂写：追加半条记录
        let mut f = OpenOptions::new().append(true).open(d.join("outbox.dat")).unwrap();
        f.write_all(&[50, 0, 0, 0, 1, 2]).unwrap();
        drop(f);
        let ob = Outbox::open(&d, 4096).unwrap();
        assert_eq!(ob.pending(), 2);
    }
}
