//! 经任意字节管道（直连 TCP 或 SS 隧道）做一次 HTTPS GET。
//!
//! 用 rustls 的 `ClientConnection` 手动驱动（write_tls/read_tls），所以隧道不需要实现 AsyncRead/AsyncWrite。
//! 根证书（webpki-roots）只加载一次，`ClientConfig` 全局复用，自带会话恢复缓存。
use std::io::{self, Read, Write};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::ss::SsStream;

/// 响应上限：trace 约 300 B，ipify 十几字节；超过即判异常
pub const MAX_RESPONSE: usize = 16 * 1024;

pub fn tls_config() -> Arc<ClientConfig> {
    static CFG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CFG.get_or_init(|| {
        let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut cfg = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("ring 支持默认协议版本")
            .with_root_certificates(roots)
            .with_no_client_auth();
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        Arc::new(cfg)
    })
    .clone()
}

/// 字节管道
pub enum Transport {
    Plain(TcpStream),
    Ss(SsStream),
}

impl Transport {
    async fn send(&mut self, b: &[u8]) -> io::Result<()> {
        match self {
            Transport::Plain(s) => s.write_all(b).await,
            Transport::Ss(s) => s.send(b).await,
        }
    }
    async fn recv(&mut self) -> io::Result<Vec<u8>> {
        match self {
            Transport::Plain(s) => {
                let mut buf = vec![0u8; 16 * 1024];
                let n = s.read(&mut buf).await?;
                buf.truncate(n);
                Ok(buf)
            }
            Transport::Ss(s) => s.recv().await,
        }
    }
}

/// 失败所处阶段
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// TLS 握手没完成（经 SS 时：隧道被断开，多为密码不符或节点连不上目标）
    Tls,
    /// 证书不可信
    Cert,
    /// 握手完成，但响应没读完 / 状态码不对
    Http,
}

#[derive(Debug)]
pub struct HttpError {
    pub phase: Phase,
    pub message: String,
}

pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub tls_ms: f64,
    pub first_byte_ms: f64,
}

fn ms(start: Instant) -> f64 {
    (start.elapsed().as_secs_f64() * 1000.0 * 10.0).round() / 10.0
}

fn err(phase: Phase, m: impl Into<String>) -> HttpError {
    HttpError { phase, message: m.into() }
}

fn tls_err(e: rustls::Error) -> HttpError {
    let phase = match e {
        rustls::Error::InvalidCertificate(_) => Phase::Cert,
        _ => Phase::Tls,
    };
    err(phase, format!("tls: {e}"))
}

/// 发 `GET path`，读到对端关闭或响应完整为止。`start` 是整个检查的起点（用于分段计时）。
pub async fn get(t: &mut Transport, host: &str, path: &str, start: Instant) -> Result<HttpResponse, HttpError> {
    let name = ServerName::try_from(host.to_string()).map_err(|_| err(Phase::Tls, "无效的主机名"))?;
    let mut conn = ClientConnection::new(tls_config(), name).map_err(tls_err)?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: fleet-probe/{}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        env!("CARGO_PKG_VERSION")
    );
    let mut sent = false;
    let mut tls_ms = None;
    let mut first_byte_ms = None;
    let mut resp: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        if !conn.is_handshaking() && !sent {
            tls_ms = Some(ms(start));
            conn.writer().write_all(req.as_bytes()).map_err(|e| err(Phase::Tls, e.to_string()))?;
            sent = true;
        }
        while conn.wants_write() {
            let mut out = Vec::new();
            conn.write_tls(&mut out).map_err(|e| err(Phase::Tls, e.to_string()))?;
            let phase = if sent { Phase::Http } else { Phase::Tls };
            t.send(&out).await.map_err(|e| err(phase, format!("send: {e}")))?;
        }
        let mut closed = false;
        loop {
            match conn.reader().read(&mut buf) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(n) => {
                    if first_byte_ms.is_none() {
                        first_byte_ms = Some(ms(start));
                    }
                    resp.extend_from_slice(&buf[..n]);
                    if resp.len() > MAX_RESPONSE {
                        return Err(err(Phase::Http, "响应过大"));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(err(Phase::Http, e.to_string())),
            }
        }
        if closed || (sent && complete(&resp)) {
            break;
        }
        let phase = if sent { Phase::Http } else { Phase::Tls };
        let data = t.recv().await.map_err(|e| err(phase, format!("recv: {e}")))?;
        if data.is_empty() {
            if sent && !resp.is_empty() {
                break; // 对端没发 close_notify 就关了：有数据就按已读到的解析
            }
            return Err(err(phase, "对端关闭连接"));
        }
        let mut rd = &data[..];
        while !rd.is_empty() {
            conn.read_tls(&mut rd).map_err(|e| err(phase, e.to_string()))?;
            conn.process_new_packets().map_err(tls_err)?;
        }
    }
    let (status, body) = parse(&resp).ok_or_else(|| err(Phase::Http, "HTTP 响应无法解析"))?;
    Ok(HttpResponse { status, body, tls_ms: tls_ms.unwrap_or(0.0), first_byte_ms: first_byte_ms.unwrap_or(0.0) })
}

fn header_end(b: &[u8]) -> Option<usize> {
    b.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

fn complete(b: &[u8]) -> bool {
    let Some(he) = header_end(b) else { return false };
    let head = String::from_utf8_lossy(&b[..he]);
    if let Some(cl) = header_value(&head, "content-length").and_then(|v| v.parse::<usize>().ok()) {
        return b.len() >= he + cl;
    }
    if header_value(&head, "transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        return b.ends_with(b"0\r\n\r\n");
    }
    false
}

/// 解析状态码与正文（支持 Content-Length 与 chunked）
pub fn parse(b: &[u8]) -> Option<(u16, Vec<u8>)> {
    let he = header_end(b)?;
    let head = String::from_utf8_lossy(&b[..he]);
    let status: u16 = head.split_whitespace().nth(1)?.parse().ok()?;
    let raw = &b[he..];
    let chunked = header_value(&head, "transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    if !chunked {
        let body = match header_value(&head, "content-length").and_then(|v| v.parse::<usize>().ok()) {
            Some(cl) => raw[..cl.min(raw.len())].to_vec(),
            None => raw.to_vec(),
        };
        return Some((status, body));
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let le = raw[i..].windows(2).position(|w| w == b"\r\n")? + i;
        let size_s = String::from_utf8_lossy(&raw[i..le]);
        let size = usize::from_str_radix(size_s.split(';').next()?.trim(), 16).ok()?;
        if size == 0 {
            break;
        }
        let s = le + 2;
        let e = (s + size).min(raw.len());
        out.extend_from_slice(&raw[s..e]);
        i = e + 2;
    }
    Some((status, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chunked_and_length() {
        let r = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nip=1.\r\n6\r\n2.3.4\n\r\n0\r\n\r\n";
        assert!(complete(r));
        assert_eq!(parse(r).unwrap(), (200, b"ip=1.2.3.4\n".to_vec()));
        let r = b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n1.2.3.4";
        assert!(complete(r));
        assert_eq!(parse(r).unwrap(), (200, b"1.2.3.4".to_vec()));
        assert!(!complete(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\n1.2.3.4"));
    }
}
