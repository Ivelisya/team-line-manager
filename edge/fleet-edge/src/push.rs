//! HTTPS 上报：ureq + rustls（ring）+ webpki 根证书，正常校验证书（包括 IP SAN），没有不安全模式。
//! 复用一条 keep-alive 连接（10 s 一次上报，远小于 Caddy 的空闲超时）；连接被关掉时这次失败走退避重发。
use std::time::Duration;

pub enum Outcome {
    /// 中控已接收（含重复：已经收过的批次同样视为确认）
    Accepted { duplicate: bool },
    /// 永久拒绝这一批（400/413/422）：丢弃这一条，避免堵住队列
    Rejected(String),
    /// 暂时失败：网络、5xx、401/403（令牌问题不丢数据）、429（带建议等待秒数）
    Retry { reason: String, after_s: Option<u64> },
}

pub struct Pusher {
    agent: ureq::Agent,
    url: String,
    auth: String,
}

impl Pusher {
    pub fn new(url: String, token: &str, timeout_s: u64) -> Pusher {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout(Duration::from_secs(timeout_s))
            // 10 s 一次上报：复用连接（省掉每次的 TLS 握手）。空闲连接被对端关掉时这次 POST 失败 → 进入退避（5 s 起）重发，数据在 outbox 里不丢
            .max_idle_connections(1)
            .max_idle_connections_per_host(1)
            .redirects(0)
            .user_agent(concat!("fleet-edge/", env!("CARGO_PKG_VERSION")))
            .build();
        Pusher { agent, url, auth: format!("Bearer {token}") }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn send(&self, body: &[u8]) -> Outcome {
        let r = self
            .agent
            .post(&self.url)
            .set("Authorization", &self.auth)
            .set("Content-Type", "application/json")
            .send_bytes(body);
        match r {
            Ok(resp) => {
                let s = resp.into_string().unwrap_or_default();
                // fleetd 的提示：有人在看（界面打开着）→ 接下来 90 s 按实时节奏
                if serde_json::from_str::<serde_json::Value>(&s).ok().and_then(|v| v["live"].as_bool()) == Some(true) {
                    crate::LIVE_UNTIL.store(crate::util::now_f64() as u64 + 90, std::sync::atomic::Ordering::Relaxed);
                }
                Outcome::Accepted { duplicate: s.contains("\"duplicate\"") }
            }
            Err(ureq::Error::Status(code, resp)) => {
                let after = resp.header("Retry-After").and_then(|v| v.trim().parse::<u64>().ok());
                let body: String = resp.into_string().unwrap_or_default().chars().take(200).collect();
                match code {
                    400 | 413 | 422 => Outcome::Rejected(format!("HTTP {code}: {body}")),
                    429 => Outcome::Retry { reason: "HTTP 429".into(), after_s: after.or(Some(5)) },
                    _ => Outcome::Retry { reason: format!("HTTP {code}: {body}"), after_s: after },
                }
            }
            Err(e) => Outcome::Retry { reason: e.to_string().chars().take(300).collect(), after_s: None },
        }
    }
}

/// 指数退避 + 抖动：5 s 起，每次翻倍，上限 60 s，乘以 [0.5, 1.5) 的随机因子（最长 90 s）。
/// 上限必须低于中控的离线判定（120 s），否则中控重启后所有主机都会被误判离线；每台主机每分钟一次握手的代价可以忽略。
pub struct Backoff {
    pub fails: u32,
}

impl Backoff {
    pub fn next_delay(&mut self, rng: &mut crate::util::Rng) -> Duration {
        let base = (5.0 * 2f64.powi(self.fails.min(6) as i32)).min(60.0);
        self.fails = self.fails.saturating_add(1);
        Duration::from_secs_f64(base * (0.5 + rng.unit()))
    }
    pub fn reset(&mut self) {
        self.fails = 0;
    }
}
