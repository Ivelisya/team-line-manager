//! 通知渠道：Telegram、企业微信群机器人、SMTP、签名 Webhook。
//! 没配渠道（或 notify.shadow = true）时为影子模式：每条本该发出的通知写进 notify_log（status = shadow）。
//! 同一渠道连续失败 3 次 → channel_failing 告警（经其他渠道发出；影子模式下同样只记日志）。
use crate::alerts::{Labels, Observation, Outgoing, Severity};
use crate::config::{read_secret, ChannelCfg, NotifyCfg};
use crate::store::NotifyRow;
use hmac::{Hmac, Mac};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct Channel {
    pub cfg: ChannelCfg,
    pub min: Severity,
    secret: Option<String>,
    url: Option<String>,
    password: Option<String>,
}

#[derive(Default, Clone, Serialize)]
pub struct ChannelState {
    pub sent: u64,
    pub failed: u64,
    pub consecutive_failures: u32,
    pub last_status: Option<String>,
    pub last_at: Option<f64>,
    pub last_error: Option<String>,
}

pub struct Notifier {
    pub channels: Vec<Channel>,
    pub shadow: bool,
    client: reqwest::Client,
    pub state: Mutex<HashMap<String, ChannelState>>,
}

fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…（已截断）", &s[..end])
}

impl Channel {
    pub fn load(cfg: &ChannelCfg) -> Result<Channel, String> {
        let min = Severity::parse(&cfg.min_severity).ok_or("min_severity 无效")?;
        let secret = match cfg.kind.as_str() {
            "telegram" => Some(read_secret(cfg.bot_token_file.as_ref().unwrap())?),
            "webhook" => Some(read_secret(cfg.secret_file.as_ref().unwrap())?),
            _ => None,
        };
        let url = match (&cfg.url, &cfg.url_file) {
            (Some(u), _) => Some(u.clone()),
            (None, Some(p)) => Some(read_secret(p)?),
            _ => None,
        };
        if let Some(u) = &url {
            if !u.starts_with("https://") && !u.starts_with("http://127.0.0.1") {
                return Err(format!("渠道 {}：只允许 https 地址", cfg.name));
            }
        }
        let password = match &cfg.password_file {
            Some(p) => Some(read_secret(p)?),
            None => None,
        };
        Ok(Channel { cfg: cfg.clone(), min, secret, url, password })
    }

    /// 给 /channels 用：不含任何机密。
    pub fn redacted(&self) -> Value {
        let target = match self.cfg.kind.as_str() {
            "telegram" => format!("chat {}", mask(self.cfg.chat_id.as_deref().unwrap_or(""))),
            "wecom" | "webhook" => self.url.as_deref().map(url_host).unwrap_or_default(),
            "smtp" => format!("{}:{} → {} 个收件人", self.cfg.host.as_deref().unwrap_or(""), self.cfg.port.unwrap_or(0), self.cfg.to.len()),
            _ => String::new(),
        };
        json!({"name": self.cfg.name, "type": self.cfg.kind, "min_severity": self.min, "target": target})
    }
}

fn mask(s: &str) -> String {
    let n = s.chars().count();
    if n <= 4 {
        "****".into()
    } else {
        format!("{}****{}", s.chars().take(2).collect::<String>(), s.chars().skip(n - 2).collect::<String>())
    }
}

fn url_host(u: &str) -> String {
    let rest = u.split("://").nth(1).unwrap_or(u);
    format!("{}/…", rest.split('/').next().unwrap_or(""))
}

pub fn webhook_signature(secret: &str, ts: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac");
    mac.update(ts.as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

impl Notifier {
    pub fn from_config(cfg: &NotifyCfg) -> Result<Notifier, String> {
        let mut channels = vec![];
        for c in &cfg.channels {
            channels.push(Channel::load(c)?);
        }
        let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).build().map_err(|e| e.to_string())?;
        Ok(Notifier { channels, shadow: cfg.shadow || cfg.channels.is_empty(), client, state: Mutex::new(HashMap::new()) })
    }

    pub fn is_shadow(&self) -> bool {
        self.shadow
    }

    /// 投递一条通知；返回写 notify_log 的行。
    pub async fn deliver(&self, msg: &Outgoing, now: f64) -> Vec<NotifyRow> {
        let base = |channel: &str, status: &str, error: Option<String>, ms: Option<i64>| NotifyRow {
            ts: now,
            channel: channel.into(),
            status: status.into(),
            kind: msg.kind.clone(),
            severity: msg.severity.as_str().into(),
            group_key: msg.group_key.clone(),
            title: msg.title.clone(),
            body: msg.body.clone(),
            alert_ids: msg.alert_ids.clone(),
            error,
            latency_ms: ms,
        };
        if self.shadow {
            let targets: Vec<&str> = self.channels.iter().filter(|c| msg.kind == "digest" || msg.severity >= c.min).map(|c| c.cfg.name.as_str()).collect();
            let note = if self.channels.is_empty() { None } else { Some(format!("影子模式：本应发往 {}", targets.join("、"))) };
            return vec![base("shadow", "shadow", note, None)];
        }
        let mut rows = vec![];
        for ch in &self.channels {
            if msg.kind != "digest" && msg.severity < ch.min {
                continue;
            }
            let t0 = Instant::now();
            let r = self.send(ch, msg).await;
            let ms = t0.elapsed().as_millis() as i64;
            let mut st = self.state.lock().unwrap();
            let s = st.entry(ch.cfg.name.clone()).or_default();
            s.last_at = Some(now);
            match r {
                Ok(()) => {
                    s.sent += 1;
                    s.consecutive_failures = 0;
                    s.last_status = Some("sent".into());
                    s.last_error = None;
                    rows.push(base(&ch.cfg.name, "sent", None, Some(ms)));
                }
                Err(e) => {
                    tracing::warn!(channel = %ch.cfg.name, error = %e, "notification failed");
                    s.failed += 1;
                    s.consecutive_failures += 1;
                    s.last_status = Some("failed".into());
                    s.last_error = Some(e.clone());
                    rows.push(base(&ch.cfg.name, "failed", Some(e), Some(ms)));
                }
            }
        }
        rows
    }

    pub async fn send(&self, ch: &Channel, msg: &Outgoing) -> Result<(), String> {
        match ch.cfg.kind.as_str() {
            "telegram" => {
                let token = ch.secret.as_deref().unwrap();
                let text = truncate_bytes(&msg.body, 3800);
                let body = json!({"chat_id": ch.cfg.chat_id, "text": text, "disable_web_page_preview": true,
                    "reply_markup": {"inline_keyboard": [[{"text": "打开", "url": msg.link}]]}});
                let r = self.client.post(format!("https://api.telegram.org/bot{token}/sendMessage")).json(&body).send().await
                    .map_err(|e| format!("Telegram 连接失败：{}", crate::legacy::short_err(&e)))?;
                let st = r.status();
                let v: Value = r.json().await.unwrap_or(Value::Null);
                if st.is_success() && v.get("ok").and_then(|x| x.as_bool()) == Some(true) {
                    Ok(())
                } else {
                    Err(format!("Telegram HTTP {}：{}", st.as_u16(), v.get("description").and_then(|x| x.as_str()).unwrap_or("")))
                }
            }
            "wecom" => {
                let url = ch.url.as_deref().unwrap();
                let body = json!({"msgtype": "text", "text": {"content": truncate_bytes(&msg.body, 1900)}});
                let r = self.client.post(url).json(&body).send().await.map_err(|e| format!("企业微信连接失败：{}", crate::legacy::short_err(&e)))?;
                let v: Value = r.json().await.unwrap_or(Value::Null);
                if v.get("errcode").and_then(|x| x.as_i64()) == Some(0) {
                    Ok(())
                } else {
                    Err(format!("企业微信返回 {}", v))
                }
            }
            "webhook" => {
                let url = ch.url.as_deref().unwrap();
                let payload = serde_json::to_vec(&json!({"kind": msg.kind, "severity": msg.severity, "group_key": msg.group_key,
                    "title": msg.title, "text": msg.body, "link": msg.link, "alerts": msg.alerts, "ts": msg.ts})).unwrap();
                let ts = format!("{}", msg.ts as i64);
                let sig = webhook_signature(ch.secret.as_deref().unwrap(), &ts, &payload);
                let r = self.client.post(url).header("Content-Type", "application/json").header("X-Fleet-Timestamp", &ts)
                    .header("X-Fleet-Signature", sig).body(payload).send().await
                    .map_err(|e| format!("Webhook 连接失败：{}", crate::legacy::short_err(&e)))?;
                if r.status().is_success() {
                    Ok(())
                } else {
                    Err(format!("Webhook HTTP {}", r.status().as_u16()))
                }
            }
            "smtp" => self.send_smtp(ch, msg).await,
            other => Err(format!("未知渠道类型 {other}")),
        }
    }

    async fn send_smtp(&self, ch: &Channel, msg: &Outgoing) -> Result<(), String> {
        use lettre::message::header::ContentType;
        use lettre::transport::smtp::authentication::Credentials;
        use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
        let c = &ch.cfg;
        let mut b = Message::builder()
            .from(c.from.as_deref().unwrap().parse().map_err(|e| format!("from 无效：{e}"))?)
            .subject(&msg.title)
            .header(ContentType::TEXT_PLAIN);
        for to in &c.to {
            b = b.to(to.parse().map_err(|e| format!("收件人 {to} 无效：{e}"))?);
        }
        let email = b.body(msg.body.clone()).map_err(|e| e.to_string())?;
        let host = c.host.as_deref().unwrap();
        let builder = match c.security.as_deref().unwrap_or("starttls") {
            "tls" => AsyncSmtpTransport::<Tokio1Executor>::relay(host).map_err(|e| e.to_string())?,
            "none" => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host),
            _ => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host).map_err(|e| e.to_string())?,
        };
        let mut builder = builder.timeout(Some(Duration::from_secs(15)));
        if let Some(p) = c.port {
            builder = builder.port(p);
        }
        if let (Some(u), Some(p)) = (&c.username, &ch.password) {
            builder = builder.credentials(Credentials::new(u.clone(), p.clone()));
        }
        builder.build().send(email).await.map(|_| ()).map_err(|e| format!("SMTP 发送失败：{e}"))
    }

    /// 连续失败 ≥ 3 次的渠道 → channel_failing 观察值（规则作用域 "channel_failing"）。
    pub fn failing_observations(&self) -> Vec<Observation> {
        let st = self.state.lock().unwrap();
        let mut out = vec![];
        for (name, s) in st.iter() {
            if s.consecutive_failures >= 3 {
                let mut l = Labels::new();
                l.insert("kind".into(), "system".into());
                l.insert("cause".into(), "channel_failing".into());
                l.insert("channel".into(), name.clone());
                out.push(Observation {
                    rule: "channel_failing".into(),
                    labels: l,
                    severity: Severity::Critical,
                    summary: format!("通知渠道 {name} 连续 {} 次投递失败：{}", s.consecutive_failures, s.last_error.clone().unwrap_or_default()),
                    evidence: json!({"consecutive_failures": s.consecutive_failures, "last_error": s.last_error, "last_at": s.last_at}),
                    active_since: None,
                    for_s: 0.0,
                    notify_resolved: true,
                });
            }
        }
        out
    }

    pub fn channels_json(&self) -> Vec<Value> {
        let st = self.state.lock().unwrap();
        self.channels
            .iter()
            .map(|c| {
                let mut v = c.redacted();
                v["state"] = serde_json::to_value(st.get(&c.cfg.name).cloned().unwrap_or_default()).unwrap();
                v
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(sev: Severity) -> Outgoing {
        Outgoing { kind: "group".into(), severity: sev, group_key: Some("relay=线路机2|cause=auth".into()), title: "t".into(),
            body: "b".into(), link: "l".into(), alert_ids: vec![1], alerts: vec![], ts: 1.0 }
    }

    #[tokio::test]
    async fn shadow_mode_logs_only() {
        let n = Notifier::from_config(&NotifyCfg::default()).unwrap();
        assert!(n.is_shadow());
        let rows = n.deliver(&msg(Severity::Critical), 5.0).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "shadow");
        assert_eq!(rows[0].channel, "shadow");
    }

    #[tokio::test]
    async fn failing_channel_raises_after_3() {
        let d = tempfile::tempdir().unwrap();
        let sp = d.path().join("s");
        std::fs::write(&sp, "secret").unwrap();
        let cfg: NotifyCfg = toml::from_str(&format!(
            "shadow = false\n[[channels]]\nname = \"hook\"\ntype = \"webhook\"\nurl = \"http://127.0.0.1:1/x\"\nsecret_file = \"{}\"\nmin_severity = \"warning\"",
            sp.display()
        )).unwrap();
        let n = Notifier::from_config(&cfg).unwrap();
        assert!(!n.is_shadow());
        for i in 0..3 {
            assert!(n.failing_observations().is_empty(), "第 {i} 次前不应告警");
            let rows = n.deliver(&msg(Severity::Critical), i as f64).await;
            assert_eq!(rows[0].status, "failed");
        }
        let obs = n.failing_observations();
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].labels["channel"], "hook");
        // info 不发到 min_severity=warning 的渠道
        assert!(n.deliver(&msg(Severity::Info), 9.0).await.is_empty());
    }

    #[test]
    fn signature_is_hmac_sha256() {
        // echo -n '1700000000.{}' | openssl dgst -sha256 -hmac k
        let s = webhook_signature("k", "1700000000", b"{}");
        assert!(s.starts_with("sha256=") && s.len() == 7 + 64);
        assert_eq!(s, webhook_signature("k", "1700000000", b"{}"));
        assert_ne!(s, webhook_signature("k2", "1700000000", b"{}"));
    }

    #[test]
    fn truncation_on_char_boundary() {
        let s = "告警".repeat(2000);
        let t = truncate_bytes(&s, 1900);
        assert!(t.len() < 1950);
    }
}
