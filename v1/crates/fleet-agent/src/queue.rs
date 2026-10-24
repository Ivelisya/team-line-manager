//! 磁盘有界队列：每个待发送的 Report 一个文件 `<seq:020>.json`。
//! 先落盘再发送；服务端确认后删除；超过字节/时间上限从最旧的开始丢并计数（其中的入口流量交还给上报器结转）。
//! 永久拒绝（4xx）的报告移入 `dead/`，不静默删除；其中的入口流量同样交还结转（本身带着结转流量的除外）。

use crate::util::{now_ms, write_atomic};
use crate::xray::stats::{add_carry, Carry};
use anyhow::Result;
use fleet_proto::{EntrySample, Report};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const DEAD_KEEP: usize = 50;

pub struct Queue {
    dir: PathBuf,
    /// seq → (字节数, 入队时刻, 是否带着结转补报的入口流量)。最后一项只在本进程内记得：重启前入队的一律当没带，
    /// 被拒时最多多结转一次
    items: BTreeMap<u64, (u64, Enq, bool)>,
    bytes: u64,
    max_bytes: u64,
    max_age_ms: i64,
    pub dropped: u64,
    /// 本进程内移入 dead/ 的报告数
    pub dead: u64,
}

/// 入队时刻：本进程入队的记单调时钟（墙钟跳变——开机时钟错、NTP 一步校准几小时——不会让它们突然「过期」被整队丢掉），
/// 重启前留下的只能用文件修改时间（墙钟 ms）
#[derive(Clone, Copy)]
enum Enq {
    Mono(std::time::Instant),
    Wall(i64),
}

impl Enq {
    fn age_ms(self, now_ms: i64) -> i64 {
        match self {
            Enq::Mono(t) => t.elapsed().as_millis() as i64,
            Enq::Wall(ms) => now_ms - ms,
        }
    }
}

fn path_of(dir: &Path, seq: u64) -> PathBuf {
    dir.join(format!("{seq:020}.json"))
}

/// 只解析报告里的入口流量样本（访问事件等其余字段跳过）
fn read_entries(path: &Path) -> Result<Vec<EntrySample>> {
    #[derive(Deserialize)]
    struct OnlyEntries {
        #[serde(default)]
        entries: Vec<EntrySample>,
    }
    Ok(serde_json::from_slice::<OnlyEntries>(&std::fs::read(path)?)?.entries)
}

impl Queue {
    pub fn open(dir: PathBuf, max_bytes: u64, max_age_ms: i64) -> Result<Self> {
        std::fs::create_dir_all(dir.join("dead"))?;
        let mut q = Self { dir, items: BTreeMap::new(), bytes: 0, max_bytes, max_age_ms, dropped: 0, dead: 0 };
        let now = now_ms();
        for e in std::fs::read_dir(&q.dir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') && name.ends_with(".tmp") {
                let _ = std::fs::remove_file(e.path());
                continue;
            }
            let Some(seq) = name.strip_suffix(".json").and_then(|s| s.parse::<u64>().ok()) else { continue };
            let md = e.metadata()?;
            // 修改时间夹到不晚于现在：重启前时钟偏快留下的文件 mtime 在未来，年龄为负永不过期，evict 从最旧的开始遇到它就停，后面的也不再按时长淘汰
            let mt = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| (d.as_millis() as i64).min(now))
                .unwrap_or(now);
            q.bytes += md.len();
            q.items.insert(seq, (md.len(), Enq::Wall(mt), false));
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

    /// 入队；返回因此被淘汰的报告里的入口流量（见 `evict`）。`carried`：这份带着结转补报的入口流量（见 `dead_letter`）
    pub fn push(&mut self, r: &Report, carried: bool) -> Result<Carry> {
        let body = serde_json::to_vec(r)?;
        write_atomic(&path_of(&self.dir, r.seq), &body, true)?;
        if let Some((old, _, _)) = self.items.insert(r.seq, (body.len() as u64, Enq::Mono(std::time::Instant::now()), carried)) {
            self.bytes -= old;
        }
        self.bytes += body.len() as u64;
        Ok(self.evict())
    }

    /// 超过字节 / 时长上限就从最旧的开始丢（最新的一份总留着）。入口流量样本是增量，整份丢掉就永久少算：
    /// 删之前把其中各入口的字节数累加出来交给上报器，结转到下一份报告
    fn evict(&mut self) -> Carry {
        let now = now_ms();
        let mut carry = Carry::new();
        while let Some((&seq, &(size, ts, _))) = self.items.iter().next() {
            if self.items.len() > 1 && (self.bytes > self.max_bytes || ts.age_ms(now) > self.max_age_ms) {
                let path = path_of(&self.dir, seq);
                match read_entries(&path) {
                    Ok(es) => add_carry(&mut carry, &es),
                    Err(e) => tracing::warn!(seq, "读取要丢弃的报告失败，其中的入口流量无法结转：{e:#}"),
                }
                let _ = std::fs::remove_file(path);
                self.items.remove(&seq);
                self.bytes -= size;
                self.dropped += 1;
                tracing::warn!(seq, dropped_total = self.dropped, "离线队列超限，丢弃最旧的报告（入口流量结转到下一份报告）");
            } else {
                break;
            }
        }
        carry
    }

    /// 队列文件路径：调用方拿到路径后在阻塞线程里自己读（不在 async 线程上做磁盘 IO）
    pub fn file_path(&self, seq: u64) -> PathBuf {
        path_of(&self.dir, seq)
    }

    #[cfg(test)]
    pub fn read(&self, seq: u64) -> Result<Vec<u8>> {
        Ok(std::fs::read(self.file_path(seq))?)
    }

    /// 服务端已持久化到 seq（含）：删除其之前的全部
    pub fn ack_through(&mut self, seq: u64) -> usize {
        let done: Vec<u64> = self.items.range(..=seq).map(|(k, _)| *k).collect();
        for k in &done {
            let _ = std::fs::remove_file(path_of(&self.dir, *k));
            if let Some((sz, _, _)) = self.items.remove(k) {
                self.bytes -= sz;
            }
        }
        done.len()
    }

    /// 被服务端永久拒绝：移入 dead/（保留最近 DEAD_KEEP 份供排查）。返回其中的入口流量样本，由上报器结转到下一份报告
    /// （同 `evict`）；这份本身带着结转补报的流量时返回空、不再结转：多半正是这些流量入不了库，再结转会一份接一份地被拒
    pub fn dead_letter(&mut self, seq: u64) -> Vec<EntrySample> {
        self.dead += 1;
        let from = path_of(&self.dir, seq);
        let entries = if self.items.get(&seq).is_some_and(|i| i.2) {
            tracing::warn!(seq, "被拒收的报告带着结转补报的入口流量，不再结转，这份里的入口流量丢弃");
            vec![]
        } else {
            read_entries(&from).unwrap_or_else(|e| {
                tracing::warn!(seq, "读取被拒收的报告失败，其中的入口流量无法结转：{e:#}");
                vec![]
            })
        };
        let to = self.dir.join("dead").join(format!("{seq:020}.json"));
        if let Err(e) = std::fs::rename(&from, &to) {
            tracing::error!("移入 dead/ 失败：{e}");
            let _ = std::fs::remove_file(&from);
        }
        if let Some((sz, _, _)) = self.items.remove(&seq) {
            self.bytes -= sz;
        }
        if let Ok(rd) = std::fs::read_dir(self.dir.join("dead")) {
            let mut names: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            names.sort();
            while names.len() > DEAD_KEEP {
                let _ = std::fs::remove_file(names.remove(0));
            }
        }
        entries
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
            ..Default::default()
        }
    }

    fn tmpdir(n: &str) -> crate::util::TmpDir { crate::util::TmpDir::new(&format!("q-{n}")) }

    #[test]
    fn push_ack_reopen() {
        let d = tmpdir("a");
        let mut q = Queue::open(d.to_path_buf(), 1 << 20, i64::MAX / 2).unwrap();
        for s in 1..=3 {
            q.push(&rep(s, 10), false).unwrap();
        }
        assert_eq!(q.oldest(), Some(1));
        let q2 = Queue::open(d.to_path_buf(), 1 << 20, i64::MAX / 2).unwrap();
        assert_eq!(q2.len(), 3);
        assert_eq!(q.ack_through(2), 2);
        assert_eq!(q.oldest(), Some(3));
        assert_eq!(serde_json::from_slice::<Report>(&q.read(3).unwrap()).unwrap().seq, 3);
        q.dead_letter(3);
        assert_eq!(q.dead, 1);
        assert!(q.is_empty());
        assert_eq!(q.bytes, 0);
    }

    #[test]
    fn evicts_oldest_when_full() {
        let d = tmpdir("b");
        let mut q = Queue::open(d.to_path_buf(), 2000, i64::MAX / 2).unwrap();
        // 每份报告给同一个入口带 s 字节上行：被丢掉的那些报告的流量要全部交回来结转
        let id = uuid::Uuid::from_u128(1);
        let mut carried = Carry::new();
        for s in 1..=10 {
            let mut r = rep(s, 300);
            r.entries = vec![EntrySample { entry_id: id, ts_ms: 0, up_bytes: s, down_bytes: 1, conns: 2 }];
            for (k, (up, down)) in q.push(&r, false).unwrap() {
                let c = carried.entry(k).or_default();
                (c.0, c.1) = (c.0 + up, c.1 + down);
            }
        }
        assert!(q.dropped > 0);
        assert!(q.bytes <= 2000);
        assert_eq!(*q.items.keys().last().unwrap(), 10);
        let left: u64 = q.items.keys().sum();
        assert_eq!(carried, Carry::from([(id, ((1..=10).sum::<u64>() - left, q.dropped))]));
    }

    #[test]
    fn evicts_by_age_on_monotonic_clock() {
        // 本进程入队的按单调时钟算年龄：超过时长上限的照丢（最新的一份总留着）
        let d = tmpdir("m");
        let mut q = Queue::open(d.to_path_buf(), 1 << 20, 50).unwrap();
        q.push(&rep(1, 10), false).unwrap();
        q.push(&rep(2, 10), false).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(120));
        q.push(&rep(3, 10), false).unwrap();
        assert_eq!((q.dropped, q.oldest()), (2, Some(3)));
    }

    #[test]
    fn reopen_clamps_future_mtime_so_it_still_expires() {
        // 重启前时钟偏快留下的文件 mtime 在未来：当作刚入队，不然年龄为负永不过期，还挡着后面的报告按时长淘汰
        let d = tmpdir("f");
        let mut q = Queue::open(d.to_path_buf(), 1 << 20, 50).unwrap();
        q.push(&rep(1, 10), false).unwrap();
        q.push(&rep(2, 10), false).unwrap();
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(86400);
        std::fs::File::options().write(true).open(q.file_path(1)).unwrap().set_modified(future).unwrap();
        let mut q = Queue::open(d.to_path_buf(), 1 << 20, 50).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(120));
        q.push(&rep(3, 10), false).unwrap();
        assert_eq!((q.dropped, q.oldest()), (2, Some(3)));
    }

    #[test]
    fn dead_letter_hands_back_traffic_unless_already_carried() {
        let d = tmpdir("c");
        let mut q = Queue::open(d.to_path_buf(), 1 << 20, i64::MAX / 2).unwrap();
        let id = uuid::Uuid::from_u128(1);
        let mut r = rep(1, 10);
        r.entries = vec![EntrySample { entry_id: id, ts_ms: 0, up_bytes: 5, down_bytes: 7, conns: 1 }];
        q.push(&r, false).unwrap();
        r.seq = 2;
        q.push(&r, true).unwrap();
        // 被拒的报告里的流量交回去结转；报告本身照样留在 dead/
        assert_eq!(q.dead_letter(1).iter().map(|e| (e.entry_id, e.up_bytes, e.down_bytes)).collect::<Vec<_>>(), [(id, 5, 7)]);
        assert!(d.join("dead").join(format!("{:020}.json", 1)).exists());
        // 带着结转流量的那份再被拒：不再结转，免得一份接一份地被拒下去
        assert!(q.dead_letter(2).is_empty());
        assert_eq!((q.dead, q.len(), q.bytes), (2, 0, 0));
        // 重启后不记得哪份带过结转：当没带，照常交回
        r.seq = 3;
        q.push(&r, true).unwrap();
        let mut q = Queue::open(d.to_path_buf(), 1 << 20, i64::MAX / 2).unwrap();
        assert_eq!(q.dead_letter(3).len(), 1);
    }
}
