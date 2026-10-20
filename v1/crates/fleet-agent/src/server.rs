//! 与中控的 HTTP 交互：上报 / 长轮询期望状态 / 回报应用结果

use anyhow::{Context, Result};
use fleet_proto::{ApplyReport, DesiredState, ReportAck};
use std::path::PathBuf;
use std::time::Duration;

pub struct ServerClient {
    http: reqwest::Client,
    base: String,
    token_file: PathBuf,
}

/// 一次上报请求的分类结果
#[derive(Debug, PartialEq)]
pub enum Outcome<T> {
    Ok(T),
    /// 可重试（网络、5xx、429、403…），带建议等待
    Retry { wait: Option<Duration>, why: String },
    /// 令牌无效/被撤销：停止发送，长睡
    Revoked,
    /// 永久拒绝（400/413/422 等其它 4xx）
    Rejected(String),
}

/// HTTP 状态码 → 分类。纯函数，便于测试。
pub fn classify(status: u16, retry_after_s: Option<u64>, body: &str) -> Outcome<()> {
    let short = || format!("HTTP {status}: {}", body.chars().take(200).collect::<String>());
    match status {
        200..=299 => Outcome::Ok(()),
        401 => Outcome::Revoked,
        429 => Outcome::Retry { wait: Some(Duration::from_secs(retry_after_s.unwrap_or(5).clamp(1, 600))), why: short() },
        // 403：令牌与机器不匹配等，不丢数据，响亮地重试
        403 | 404 | 408 => Outcome::Retry { wait: None, why: short() },
        400..=499 => Outcome::Rejected(short()),
        _ => Outcome::Retry { wait: None, why: short() },
    }
}

impl ServerClient {
    pub fn new(base: &str, token_file: PathBuf, ca_file: Option<&PathBuf>) -> Result<Self> {
        let mut b = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(format!("fleet-agent/{}", env!("CARGO_PKG_VERSION")));
        if let Some(ca) = ca_file {
            let pem = std::fs::read(ca).with_context(|| format!("读取 CA {}", ca.display()))?;
            b = b.add_root_certificate(reqwest::Certificate::from_pem(&pem)?);
        }
        Ok(Self { http: b.build()?, base: base.trim_end_matches('/').to_string(), token_file })
    }

    /// 每次请求重读令牌，换令牌无需重启
    fn token(&self) -> Result<String> {
        let t = std::fs::read_to_string(&self.token_file).with_context(|| format!("读取令牌 {}", self.token_file.display()))?;
        let t = t.trim().to_string();
        anyhow::ensure!(!t.is_empty(), "令牌文件为空");
        Ok(t)
    }

    fn retry_after(r: &reqwest::Response) -> Option<u64> {
        r.headers().get("retry-after")?.to_str().ok()?.trim().parse().ok()
    }

    pub async fn post_report(&self, body: Vec<u8>) -> Outcome<ReportAck> {
        let token = match self.token() {
            Ok(t) => t,
            Err(e) => return Outcome::Retry { wait: Some(Duration::from_secs(30)), why: format!("{e:#}") },
        };
        let res = self
            .http
            .post(format!("{}/agent/v1/report", self.base))
            .bearer_auth(token)
            .header("content-type", "application/json")
            .timeout(Duration::from_secs(15))
            .body(body)
            .send()
            .await;
        let r = match res {
            Ok(r) => r,
            Err(e) => return Outcome::Retry { wait: None, why: format!("网络错误：{e}") },
        };
        let (status, ra) = (r.status().as_u16(), Self::retry_after(&r));
        let text = r.text().await.unwrap_or_default();
        match classify(status, ra, &text) {
            Outcome::Ok(()) => match serde_json::from_str::<ReportAck>(&text) {
                Ok(a) => Outcome::Ok(a),
                Err(e) => Outcome::Retry { wait: None, why: format!("ReportAck 解析失败：{e}") },
            },
            Outcome::Retry { wait, why } => Outcome::Retry { wait, why },
            Outcome::Revoked => Outcome::Revoked,
            Outcome::Rejected(s) => Outcome::Rejected(s),
        }
    }

    /// 长轮询；`Ok(None)` = 204（无变化）
    pub async fn get_desired(&self, after: u64) -> Outcome<Option<DesiredState>> {
        let token = match self.token() {
            Ok(t) => t,
            Err(e) => return Outcome::Retry { wait: Some(Duration::from_secs(30)), why: format!("{e:#}") },
        };
        let res = self
            .http
            .get(format!("{}/agent/v1/desired", self.base))
            .query(&[("after", after)])
            .bearer_auth(token)
            .timeout(Duration::from_secs(50))
            .send()
            .await;
        let r = match res {
            Ok(r) => r,
            Err(e) if e.is_timeout() => return Outcome::Ok(None),
            Err(e) => return Outcome::Retry { wait: None, why: format!("网络错误：{e}") },
        };
        let (status, ra) = (r.status().as_u16(), Self::retry_after(&r));
        if status == 204 {
            return Outcome::Ok(None);
        }
        let text = r.text().await.unwrap_or_default();
        match classify(status, ra, &text) {
            Outcome::Ok(()) => match serde_json::from_str::<DesiredState>(&text) {
                Ok(d) => Outcome::Ok(Some(d)),
                Err(e) => Outcome::Retry { wait: None, why: format!("DesiredState 解析失败：{e}") },
            },
            Outcome::Retry { wait, why } => Outcome::Retry { wait, why },
            Outcome::Revoked => Outcome::Revoked,
            Outcome::Rejected(s) => Outcome::Retry { wait: None, why: s },
        }
    }

    pub async fn post_applied(&self, rep: &ApplyReport) -> Outcome<()> {
        let token = match self.token() {
            Ok(t) => t,
            Err(e) => return Outcome::Retry { wait: Some(Duration::from_secs(30)), why: format!("{e:#}") },
        };
        let res = self
            .http
            .post(format!("{}/agent/v1/applied", self.base))
            .bearer_auth(token)
            .timeout(Duration::from_secs(15))
            .json(rep)
            .send()
            .await;
        let r = match res {
            Ok(r) => r,
            Err(e) => return Outcome::Retry { wait: None, why: format!("网络错误：{e}") },
        };
        let (status, ra) = (r.status().as_u16(), Self::retry_after(&r));
        let text = r.text().await.unwrap_or_default();
        classify(status, ra, &text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_classification() {
        assert_eq!(classify(200, None, ""), Outcome::Ok(()));
        assert_eq!(classify(401, None, ""), Outcome::Revoked);
        assert!(matches!(classify(429, Some(7), ""), Outcome::Retry { wait: Some(d), .. } if d.as_secs() == 7));
        assert!(matches!(classify(429, Some(9999), ""), Outcome::Retry { wait: Some(d), .. } if d.as_secs() == 600));
        assert!(matches!(classify(403, None, "x"), Outcome::Retry { .. }));
        assert!(matches!(classify(503, None, "x"), Outcome::Retry { .. }));
        assert!(matches!(classify(400, None, "bad"), Outcome::Rejected(_)));
        assert!(matches!(classify(413, None, "big"), Outcome::Rejected(_)));
    }
}
