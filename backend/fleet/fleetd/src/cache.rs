//! 共享的短时缓存：TTL + 同一 key 的并发请求合并（只发一次上游请求）。
use bytes::Bytes;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::sync::OnceCell;

#[derive(Debug)]
pub struct Entry {
    pub status: u16,
    pub body: Bytes,
    pub fetched_at: f64,
    pub hash: [u8; 32],
    pub latency_ms: f64,
}

impl Entry {
    pub fn new(status: u16, body: Bytes, fetched_at: f64, latency_ms: f64) -> Entry {
        let hash: [u8; 32] = Sha256::digest(&body).into();
        Entry { status, body, fetched_at, hash, latency_ms }
    }
}

type Slot = Arc<OnceCell<Result<Arc<Entry>, String>>>;

#[derive(Default)]
pub struct Cache {
    entries: Mutex<HashMap<String, Arc<Entry>>>,
    inflight: Mutex<HashMap<String, Slot>>,
    max_entries: usize,
}

impl Cache {
    pub fn new(max_entries: usize) -> Cache {
        Cache { max_entries, ..Default::default() }
    }

    pub fn peek(&self, key: &str) -> Option<Arc<Entry>> {
        self.entries.lock().unwrap().get(key).cloned()
    }

    pub fn put(&self, key: &str, e: Arc<Entry>) {
        let mut m = self.entries.lock().unwrap();
        if m.len() >= self.max_entries && !m.contains_key(key) {
            // 淘汰最旧的一条
            if let Some(oldest) = m.iter().min_by(|a, b| a.1.fetched_at.partial_cmp(&b.1.fetched_at).unwrap()).map(|(k, _)| k.clone()) {
                m.remove(&oldest);
            }
        }
        m.insert(key.to_string(), e);
    }

    pub fn purge_key(&self, key: &str) {
        self.entries.lock().unwrap().remove(key);
    }

    /// 原生模式下写入之后：旧形状的读缓存全部作废
    pub fn purge_all(&self) {
        self.entries.lock().unwrap().clear();
    }

    pub fn purge_older_than(&self, t: f64) {
        self.entries.lock().unwrap().retain(|_, e| e.fetched_at >= t);
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    pub fn bytes(&self) -> usize {
        self.entries.lock().unwrap().values().map(|e| e.body.len()).sum()
    }

    /// 缓存够新就直接返回；否则合并并发请求，只调用一次 `fetch`。只缓存 2xx/4xx（5xx 与连接错误不缓存）。
    pub async fn get_or_fetch<F, Fut>(&self, key: &str, now: f64, max_age: f64, fetch: F) -> (Result<Arc<Entry>, String>, bool)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Entry, String>>,
    {
        if let Some(e) = self.peek(key) {
            if now - e.fetched_at <= max_age {
                return (Ok(e), true);
            }
        }
        let slot: Slot = {
            let mut m = self.inflight.lock().unwrap();
            m.entry(key.to_string()).or_insert_with(|| Arc::new(OnceCell::new())).clone()
        };
        let mut leader = false;
        let res = slot
            .get_or_init(|| {
                leader = true;
                let fut = fetch();
                async move {
                    match fut.await {
                        Ok(e) => {
                            let e = Arc::new(e);
                            if e.status < 500 {
                                self.put(key, e.clone());
                            }
                            Ok(e)
                        }
                        Err(err) => Err(err),
                    }
                }
            })
            .await
            .clone();
        if leader {
            let mut m = self.inflight.lock().unwrap();
            if let Some(s) = m.get(key) {
                if Arc::ptr_eq(s, &slot) {
                    m.remove(key);
                }
            }
        }
        (res, !leader)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn coalesces_and_caches() {
        let c = Arc::new(Cache::new(8));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut hs = vec![];
        for _ in 0..10 {
            let c = c.clone();
            let calls = calls.clone();
            hs.push(tokio::spawn(async move {
                c.get_or_fetch("/overview", 100.0, 10.0, || async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    Ok(Entry::new(200, Bytes::from_static(b"{\"data\":1}"), 100.0, 1.0))
                })
                .await
                .0
                .unwrap()
            }));
        }
        for h in hs {
            h.await.unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "10 个并发请求只打一次上游");
        // TTL 内命中缓存
        let (_, hit) = c.get_or_fetch("/overview", 105.0, 10.0, || async { panic!("不应请求") }).await;
        assert!(hit);
        // 过期后重新请求
        let (r, _) = c.get_or_fetch("/overview", 111.0, 10.0, || async { Ok(Entry::new(200, Bytes::from_static(b"2"), 111.0, 1.0)) }).await;
        assert_eq!(&r.unwrap().body[..], b"2");
    }

    #[tokio::test]
    async fn errors_not_cached() {
        let c = Cache::new(8);
        let (r, _) = c.get_or_fetch("/x", 1.0, 10.0, || async { Err("down".to_string()) }).await;
        assert!(r.is_err());
        let (r, _) = c.get_or_fetch("/x", 1.0, 10.0, || async { Ok(Entry::new(500, Bytes::new(), 1.0, 1.0)) }).await;
        assert_eq!(r.unwrap().status, 500);
        assert!(c.peek("/x").is_none());
    }

    #[test]
    fn eviction() {
        let c = Cache::new(2);
        c.put("a", Arc::new(Entry::new(200, Bytes::new(), 1.0, 0.0)));
        c.put("b", Arc::new(Entry::new(200, Bytes::new(), 2.0, 0.0)));
        c.put("c", Arc::new(Entry::new(200, Bytes::new(), 3.0, 0.0)));
        assert!(c.peek("a").is_none());
        assert_eq!(c.len(), 2);
    }
}
