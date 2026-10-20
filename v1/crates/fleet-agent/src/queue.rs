//! 磁盘有界队列：每个待发送的 Report 一个文件 `<seq:020>.json`。
//! 先落盘再发送；服务端确认后删除；超过字节/时间上限从最旧的开始丢并计数。
//! 永久拒绝（4xx）的报告移入 `dead/`，不静默删除。

use crate::util::{now_ms, write_atomic};
use anyhow::Result;
use fleet_proto::Report;
use std::collections::BTreeMap;
use std::path::PathBuf;

const DEAD_KEEP: usize = 50;

pub struct Queue {
    dir: PathBuf,
    /// seq → (字节数, 入队时间 ms)
    items: BTreeMap<u64, (u64, i64)>,
    bytes: u64,
    max_bytes: u64,
    max_age_ms: i64,
    pub dropped: u64,
    /// 本进程内移入 dead/ 的报告数
    pub dead: u64,
}

fn path_of(dir: &std::path::Path, seq: u64) -> PathBuf {
    dir.join(format!("{seq:020}.json"))
}

impl Queue {
    pub fn open(dir: PathBuf, max_bytes: u64, max_age_ms: i64) -> Result<Self> {
        std::fs::create_dir_all(dir.join("dead"))?;
        let mut q = Self { dir, items: BTreeMap::new(), bytes: 0, max_bytes, max_age_ms, dropped: 0, dead: 0 };
        for e in std::fs::read_dir(&q.dir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') && name.ends_with(".tmp") {
                let _ = std::fs::remove_file(e.path());
                continue;
            }
            let Some(seq) = name.strip_suffix(".json").and_then(|s| s.parse::<u64>().ok()) else { continue };
            let md = e.metadata()?;
            let mt = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or_else(now_ms);
            q.bytes += md.len();
            q.items.insert(seq, (md.len(), mt));
        }
        Ok(q)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    pub fn oldest(&self) -> Option<u64> {
        self.items.keys().next().copied()
    }
    pub fn newest(&self) -> Option<u64> {
        self.items.keys().next_back().copied()
    }
    /// 已用字节 / 上限，供访问日志背压
    pub fn pressure(&self) -> f32 {
        self.bytes as f32 / self.max_bytes as f32
    }

    pub fn push(&mut self, r: &Report) -> Result<()> {
        let body = serde_json::to_vec(r)?;
        write_atomic(&path_of(&self.dir, r.seq), &body, true)?;
        if let Some((old, _)) = self.items.insert(r.seq, (body.len() as u64, now_ms())) {
            self.bytes -= old;
        }
        self.bytes += body.len() as u64;
        self.evict();
        Ok(())
    }

    fn evict(&mut self) {
        let now = now_ms();
        while let Some((&seq, &(size, ts))) = self.items.iter().next() {
            if self.items.len() > 1 && (self.bytes > self.max_bytes || now - ts > self.max_age_ms) {
                let _ = std::fs::remove_file(path_of(&self.dir, seq));
                self.items.remove(&seq);
                self.bytes -= size;
                self.dropped += 1;
                tracing::warn!(seq, dropped_total = self.dropped, "离线队列超限，丢弃最旧的报告");
            } else {
                break;
            }
        }
    }

    pub fn read(&self, seq: u64) -> Result<Vec<u8>> {
        Ok(std::fs::read(path_of(&self.dir, seq))?)
    }

    /// 服务端已持久化到 seq（含）：删除其之前的全部
    pub fn ack_through(&mut self, seq: u64) -> usize {
        let done: Vec<u64> = self.items.range(..=seq).map(|(k, _)| *k).collect();
        for k in &done {
            let _ = std::fs::remove_file(path_of(&self.dir, *k));
            if let Some((sz, _)) = self.items.remove(k) {
                self.bytes -= sz;
            }
        }
        done.len()
    }

    /// 被服务端永久拒绝：移入 dead/（保留最近 DEAD_KEEP 份供排查）
    pub fn dead_letter(&mut self, seq: u64) {
        self.dead += 1;
        let from = path_of(&self.dir, seq);
        let to = self.dir.join("dead").join(format!("{seq:020}.json"));
        if let Err(e) = std::fs::rename(&from, &to) {
            tracing::error!("移入 dead/ 失败：{e}");
            let _ = std::fs::remove_file(&from);
        }
        if let Some((sz, _)) = self.items.remove(&seq) {
            self.bytes -= sz;
        }
        if let Ok(rd) = std::fs::read_dir(self.dir.join("dead")) {
            let mut names: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            names.sort();
            while names.len() > DEAD_KEEP {
                let _ = std::fs::remove_file(names.remove(0));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rep(seq: u64, pad: usize) -> Report {
        Report {
            seq,
            boot_id: "b".into(),
            agent_version: "x".into(),
            sent_at_ms: 0,
            info: Some(fleet_proto::SystemInfo { hostname: "h".repeat(pad), ..Default::default() }),
            host: vec![],
            entries: vec![],
            probes: vec![],
            access: vec![],
            xray: None,
            agent_health: None,
        }
    }

    fn tmpdir(n: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fa-q-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn push_ack_reopen() {
        let d = tmpdir("a");
        let mut q = Queue::open(d.clone(), 1 << 20, i64::MAX / 2).unwrap();
        for s in 1..=3 {
            q.push(&rep(s, 10)).unwrap();
        }
        assert_eq!(q.oldest(), Some(1));
        let q2 = Queue::open(d.clone(), 1 << 20, i64::MAX / 2).unwrap();
        assert_eq!(q2.len(), 3);
        assert_eq!(q.ack_through(2), 2);
        assert_eq!(q.oldest(), Some(3));
        assert_eq!(serde_json::from_slice::<Report>(&q.read(3).unwrap()).unwrap().seq, 3);
        q.dead_letter(3);
        assert_eq!(q.dead, 1);
        assert!(q.is_empty());
        assert_eq!(q.bytes, 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn evicts_oldest_when_full() {
        let d = tmpdir("b");
        let mut q = Queue::open(d.clone(), 2000, i64::MAX / 2).unwrap();
        for s in 1..=10 {
            q.push(&rep(s, 300)).unwrap();
        }
        assert!(q.dropped > 0);
        assert!(q.bytes <= 2000);
        assert_eq!(*q.items.keys().last().unwrap(), 10);
        let _ = std::fs::remove_dir_all(&d);
    }
}
