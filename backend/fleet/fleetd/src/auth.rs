//! /v2/api/* 的鉴权：fleetd 自有会话之外的第二道——Komari 管理员会话（下线前的过渡，按 Cookie 哈希缓存 30 s），或测试服专用的 dev-allow-local。
//! `auth.mode = "fleetd"`（生产）只认 fleetd 自有会话。
use crate::config::AuthMode;
use axum::http::HeaderMap;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Mutex;

#[derive(Clone, Debug)]
pub struct Identity {
    pub username: String,
    /// Komari 会话令牌（按需读 Komari 时转发）；fleetd 会话、dev 模式为 None
    pub session: Option<String>,
    /// fleetd | komari | dev | local
    pub via: &'static str,
    /// fleetd 会话令牌（退出登录用）
    pub fleet_token: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum AuthError {
    Unauthorized,
    Unavailable(String),
}

pub struct Auth {
    pub mode: AuthMode,
    pub cookie_name: String,
    ttl: f64,
    cache: Mutex<HashMap<[u8; 32], (f64, Option<String>)>>,
    pub checks_upstream: std::sync::atomic::AtomicU64,
}

pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    for h in headers.get_all("cookie") {
        let Ok(s) = h.to_str() else { continue };
        for part in s.split(';') {
            if let Some((k, v)) = part.trim().split_once('=') {
                if k.trim() == name && !v.trim().is_empty() {
                    return Some(v.trim().to_string());
                }
            }
        }
    }
    None
}

/// 请求是否直接来自本机（不是经 Caddy 转发的）。Caddy 的 reverse_proxy 总会加 X-Forwarded-For。
pub fn direct_local(headers: &HeaderMap, peer: SocketAddr) -> bool {
    peer.ip().is_loopback() && !headers.contains_key("x-forwarded-for") && !headers.contains_key("forwarded") && !headers.contains_key("via")
}

impl Auth {
    pub fn new(mode: AuthMode, cookie_name: &str, ttl_s: u64) -> Auth {
        Auth {
            mode,
            cookie_name: cookie_name.to_string(),
            ttl: ttl_s as f64,
            cache: Mutex::new(HashMap::new()),
            checks_upstream: Default::default(),
        }
    }

    pub fn cached_sessions(&self) -> usize {
        self.cache.lock().unwrap().len()
    }

    /// `me(token)`：向 Komari 查询，Ok(Some(username)) = 管理员；Ok(None) = 未登录；Err = Komari 不可用（不缓存）。
    pub async fn check<F, Fut>(&self, headers: &HeaderMap, peer: SocketAddr, now: f64, me: F) -> Result<Identity, AuthError>
    where
        F: FnOnce(String) -> Fut,
        Fut: Future<Output = Result<Option<String>, String>>,
    {
        match self.mode {
            // 只认 fleetd 自有会话（调用方已先查过）
            AuthMode::Fleetd => Err(AuthError::Unauthorized),
            AuthMode::DevAllowLocal => {
                if direct_local(headers, peer) {
                    Ok(Identity { username: "dev-local".into(), session: None, via: "dev", fleet_token: None })
                } else {
                    Err(AuthError::Unauthorized)
                }
            }
            AuthMode::Komari => {
                let Some(token) = cookie_value(headers, &self.cookie_name) else {
                    return Err(AuthError::Unauthorized);
                };
                let h: [u8; 32] = Sha256::digest(token.as_bytes()).into();
                if let Some((exp, verdict)) = self.cache.lock().unwrap().get(&h).cloned() {
                    if now < exp {
                        return match verdict {
                            Some(u) => Ok(Identity { username: u, session: Some(token), via: "komari", fleet_token: None }),
                            None => Err(AuthError::Unauthorized),
                        };
                    }
                }
                self.checks_upstream.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let verdict = me(token.clone()).await.map_err(AuthError::Unavailable)?;
                {
                    let mut c = self.cache.lock().unwrap();
                    if c.len() > 1024 {
                        c.retain(|_, (exp, _)| *exp > now);
                        if c.len() > 1024 {
                            c.clear();
                        }
                    }
                    c.insert(h, (now + self.ttl, verdict.clone()));
                }
                match verdict {
                    Some(u) => Ok(Identity { username: u, session: Some(token), via: "komari", fleet_token: None }),
                    None => Err(AuthError::Unauthorized),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn hdr(cookie: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("cookie", HeaderValue::from_str(cookie).unwrap());
        h
    }

    fn peer() -> SocketAddr {
        "127.0.0.1:5000".parse().unwrap()
    }

    #[tokio::test]
    async fn caches_verdict_30s_per_cookie() {
        let a = Auth::new(AuthMode::Komari, "session_token", 30);
        let calls = Arc::new(AtomicUsize::new(0));
        let me = |c: Arc<AtomicUsize>| move |t: String| {
            c.fetch_add(1, Ordering::SeqCst);
            async move { Ok(if t == "good" { Some("admin".to_string()) } else { None }) }
        };
        let h = hdr("foo=1; session_token=good");
        assert_eq!(a.check(&h, peer(), 100.0, me(calls.clone())).await.unwrap().username, "admin");
        assert_eq!(a.check(&h, peer(), 129.0, me(calls.clone())).await.unwrap().username, "admin");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "30 s 内只查一次 Komari");
        a.check(&h, peer(), 131.0, me(calls.clone())).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2, "过期后重新校验");
        // 错误的 cookie：401，且同样缓存
        let bad = hdr("session_token=bad");
        assert_eq!(a.check(&bad, peer(), 100.0, me(calls.clone())).await.unwrap_err(), AuthError::Unauthorized);
        assert_eq!(a.check(&bad, peer(), 101.0, me(calls.clone())).await.unwrap_err(), AuthError::Unauthorized);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        // 没有 cookie：不打 Komari
        assert_eq!(a.check(&HeaderMap::new(), peer(), 100.0, me(calls.clone())).await.unwrap_err(), AuthError::Unauthorized);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn komari_down_is_not_cached() {
        let a = Auth::new(AuthMode::Komari, "session_token", 30);
        let h = hdr("session_token=x");
        let r = a.check(&h, peer(), 1.0, |_| async { Err("down".to_string()) }).await;
        assert!(matches!(r, Err(AuthError::Unavailable(_))));
        let r = a.check(&h, peer(), 2.0, |_| async { Ok(Some("admin".to_string())) }).await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn dev_mode_only_direct_loopback() {
        let a = Auth::new(AuthMode::DevAllowLocal, "session_token", 30);
        let never = |_: String| async { panic!("dev 模式不查 Komari") };
        assert!(a.check(&HeaderMap::new(), peer(), 1.0, never).await.is_ok());
        let remote: SocketAddr = "10.0.0.2:1".parse().unwrap();
        assert!(a.check(&HeaderMap::new(), remote, 1.0, |_: String| async { Ok(None) }).await.is_err());
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", HeaderValue::from_static("1.2.3.4"));
        assert!(a.check(&h, peer(), 1.0, |_: String| async { Ok(None) }).await.is_err(), "经反向代理的请求不放行");
    }
}
