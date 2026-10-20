//! /v2/api/stream：资源变化通知。每帧 `{"resource","version","ts","path"}`，客户端据此重新取数（浏览器不再轮询）。
//! 事件带单调 id，服务端保留最近 512 条；带 Last-Event-ID 重连时补发，过旧则发 reset 让客户端全量重取。
use serde_json::json;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub struct SseEvent {
    pub id: u64,
    pub resource: String,
    pub version: u64,
    pub ts: f64,
    pub path: Option<String>,
}

impl SseEvent {
    pub fn encode(&self) -> String {
        let data = json!({"resource": self.resource, "version": self.version, "ts": self.ts, "path": self.path});
        format!("id: {}\nevent: change\ndata: {}\n\n", self.id, data)
    }
}

struct Inner {
    seq: u64,
    versions: BTreeMap<String, u64>,
    ring: VecDeque<Arc<SseEvent>>,
}

pub struct Hub {
    inner: Mutex<Inner>,
    tx: broadcast::Sender<Arc<SseEvent>>,
    clients: AtomicUsize,
    pub epoch: u64,
}

pub struct Subscription {
    pub rx: broadcast::Receiver<Arc<SseEvent>>,
    pub replay: Vec<Arc<SseEvent>>,
    pub reset: bool,
    pub versions: BTreeMap<String, u64>,
    pub last_id: u64,
}

pub const RING: usize = 512;

impl Hub {
    pub fn new(epoch: u64) -> Hub {
        let (tx, _) = broadcast::channel(256);
        Hub { inner: Mutex::new(Inner { seq: 0, versions: BTreeMap::new(), ring: VecDeque::new() }), tx, clients: AtomicUsize::new(0), epoch }
    }

    pub fn clients(&self) -> usize {
        self.clients.load(Ordering::SeqCst)
    }

    pub fn client_guard(self: &Arc<Self>) -> ClientGuard {
        self.clients.fetch_add(1, Ordering::SeqCst);
        ClientGuard(self.clone())
    }

    pub fn ring_len(&self) -> usize {
        self.inner.lock().unwrap().ring.len()
    }

    pub fn version(&self, resource: &str) -> u64 {
        self.inner.lock().unwrap().versions.get(resource).copied().unwrap_or(0)
    }

    pub fn versions(&self) -> BTreeMap<String, u64> {
        self.inner.lock().unwrap().versions.clone()
    }

    /// 预先登记一个资源（版本 0），让它从第一条连接起就出现在 hello 的版本表里（还没变化过的资源也能被客户端识别）
    pub fn declare(&self, resource: &str) {
        self.inner.lock().unwrap().versions.entry(resource.to_string()).or_insert(0);
    }

    /// 资源变了：版本 +1，广播一帧。
    pub fn publish(&self, resource: &str, path: Option<String>, ts: f64) -> Arc<SseEvent> {
        let ev = {
            let mut i = self.inner.lock().unwrap();
            i.seq += 1;
            let seq = i.seq;
            let v = i.versions.entry(resource.to_string()).or_insert(0);
            *v += 1;
            let version = *v;
            let ev = Arc::new(SseEvent { id: seq, resource: resource.to_string(), version, ts, path });
            i.ring.push_back(ev.clone());
            while i.ring.len() > RING {
                i.ring.pop_front();
            }
            ev
        };
        let _ = self.tx.send(ev.clone());
        ev
    }

    /// 先订阅再取快照，保证不漏帧。
    pub fn subscribe(&self, last_event_id: Option<u64>) -> Subscription {
        let rx = self.tx.subscribe();
        let i = self.inner.lock().unwrap();
        let mut replay = vec![];
        let mut reset = false;
        if let Some(last) = last_event_id {
            if last > i.seq {
                // 服务重启过（id 从 0 重新计数）：让客户端全量重取
                reset = true;
            } else if last < i.seq {
                let oldest = i.ring.front().map(|e| e.id).unwrap_or(i.seq + 1);
                if last + 1 < oldest {
                    reset = true;
                } else {
                    replay = i.ring.iter().filter(|e| e.id > last).cloned().collect();
                }
            }
        }
        Subscription { rx, replay, reset, versions: i.versions.clone(), last_id: i.seq }
    }
}

pub struct ClientGuard(Arc<Hub>);

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.clients.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Last-Event-ID 形如 "<epoch>-<id>"：epoch 不同（fleetd 重启过）即视为过期。
pub fn parse_last_event_id(s: &str, epoch: u64) -> Option<u64> {
    let (e, id) = s.trim().split_once('-')?;
    let e: u64 = e.parse().ok()?;
    let id: u64 = id.parse().ok()?;
    if e != epoch {
        return Some(u64::MAX);
    }
    Some(id)
}

pub fn encode_event(ev: &SseEvent, epoch: u64) -> String {
    let data = json!({"resource": ev.resource, "version": ev.version, "ts": ev.ts, "path": ev.path});
    format!("id: {}-{}\nevent: change\ndata: {}\n\n", epoch, ev.id, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_and_replay() {
        let h = Hub::new(7);
        h.publish("overview", Some("/v2/api/legacy/overview".into()), 1.0);
        h.publish("health", None, 1.0);
        let e3 = h.publish("overview", None, 2.0);
        assert_eq!(e3.version, 2);
        assert_eq!(h.version("health"), 1);
        let s = h.subscribe(Some(1));
        assert!(!s.reset);
        assert_eq!(s.replay.iter().map(|e| e.id).collect::<Vec<_>>(), vec![2, 3]);
        let s = h.subscribe(Some(3));
        assert!(s.replay.is_empty() && !s.reset);
        let s = h.subscribe(None);
        assert!(s.replay.is_empty() && !s.reset);
        assert_eq!(s.versions["overview"], 2);
        h.declare("policies");
        h.declare("overview");
        let s = h.subscribe(None);
        assert_eq!(s.versions["policies"], 0);
        assert_eq!(s.versions["overview"], 2, "登记不改已有版本");
        assert_eq!(h.publish("policies", None, 3.0).version, 1);
    }

    #[test]
    fn too_old_or_restarted_resets() {
        let h = Hub::new(1);
        for i in 0..(RING as u64 + 10) {
            h.publish("overview", None, i as f64);
        }
        assert!(h.subscribe(Some(2)).reset);
        assert!(!h.subscribe(Some(20)).reset);
        assert!(h.subscribe(Some(u64::MAX)).reset);
        assert_eq!(parse_last_event_id("1-20", 1), Some(20));
        assert_eq!(parse_last_event_id("9-20", 1), Some(u64::MAX));
        assert_eq!(parse_last_event_id("junk", 1), None);
    }

    #[tokio::test]
    async fn live_frames_and_client_count() {
        let h = Arc::new(Hub::new(1));
        let g = h.client_guard();
        assert_eq!(h.clients(), 1);
        let mut s = h.subscribe(None);
        h.publish("alerts", None, 5.0);
        let ev = s.rx.recv().await.unwrap();
        assert_eq!(ev.resource, "alerts");
        assert!(encode_event(&ev, 1).starts_with("id: 1-1\nevent: change\ndata: {\"resource\":\"alerts\",\"version\":1"));
        drop(g);
        assert_eq!(h.clients(), 0);
    }
}
