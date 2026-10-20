//! 最小 Shadowsocks 客户端（只做探测需要的部分）：
//! - TCP：AEAD（aes-128-gcm / aes-256-gcm / chacha20-ietf-poly1305，SIP004）与 2022（2022-blake3-aes-128/256-gcm，SIP022）
//! - UDP：AEAD（SIP004）单包往返（STUN 用）；2022 的 UDP 不支持，返回 Unsupported
//!
//! 为什么不用 `shadowsocks` crate：1.25 的加密后端固定是 aws-lc（C/cmake），在 a395 上交叉编 musl 静态二进制
//! 很麻烦，且依赖树 250+ 行；这里只需要客户端的“发一个请求、读一个响应”，ring（rustls 已经带着）+ md-5 + blake3
//! 约 300 行就够了。互通性用真实 ssserver 1.25 测过（见 WIRING.md）。
//!
//! 密码只存在 [`ServerCfg`] 里派生出的 key 中，不实现 Debug，不写日志。
use std::io;
use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use md5::{Digest, Md5};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_128_GCM, AES_256_GCM, CHACHA20_POLY1305};
use ring::hkdf;
use ring::rand::{SecureRandom, SystemRandom};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cipher {
    Aes128Gcm,
    Aes256Gcm,
    Chacha20Poly1305,
    Blake3Aes128Gcm,
    Blake3Aes256Gcm,
}

impl Cipher {
    pub fn parse(method: &str) -> Option<Cipher> {
        Some(match method.trim().to_ascii_lowercase().as_str() {
            "aes-128-gcm" => Cipher::Aes128Gcm,
            "aes-256-gcm" => Cipher::Aes256Gcm,
            "chacha20-ietf-poly1305" | "chacha20-poly1305" => Cipher::Chacha20Poly1305,
            "2022-blake3-aes-128-gcm" => Cipher::Blake3Aes128Gcm,
            "2022-blake3-aes-256-gcm" => Cipher::Blake3Aes256Gcm,
            _ => return None,
        })
    }
    pub fn key_len(self) -> usize {
        match self {
            Cipher::Aes128Gcm | Cipher::Blake3Aes128Gcm => 16,
            _ => 32,
        }
    }
    /// SIP004/SIP022 的 salt 长度都等于 key 长度
    pub fn salt_len(self) -> usize {
        self.key_len()
    }
    pub fn is_2022(self) -> bool {
        matches!(self, Cipher::Blake3Aes128Gcm | Cipher::Blake3Aes256Gcm)
    }
    fn alg(self) -> &'static ring::aead::Algorithm {
        match self {
            Cipher::Aes128Gcm | Cipher::Blake3Aes128Gcm => &AES_128_GCM,
            Cipher::Aes256Gcm | Cipher::Blake3Aes256Gcm => &AES_256_GCM,
            Cipher::Chacha20Poly1305 => &CHACHA20_POLY1305,
        }
    }
    fn max_chunk(self) -> usize {
        if self.is_2022() {
            0xFFFF
        } else {
            0x3FFF
        }
    }
}

/// 一台 SS 服务端的连接参数（key 已派生）
pub struct ServerCfg {
    pub addr: SocketAddr,
    pub cipher: Cipher,
    key: Vec<u8>,
}

impl ServerCfg {
    pub fn new(addr: SocketAddr, method: &str, password: &str) -> Result<ServerCfg, String> {
        let cipher = Cipher::parse(method).ok_or_else(|| format!("不支持的加密方式 {method}"))?;
        let key = if cipher.is_2022() {
            use base64::Engine;
            let k = base64::engine::general_purpose::STANDARD
                .decode(password.trim())
                .map_err(|_| "2022 密钥不是合法 base64".to_string())?;
            if k.len() != cipher.key_len() {
                return Err(format!("2022 密钥长度应为 {} 字节", cipher.key_len()));
            }
            k
        } else {
            evp_bytes_to_key(password.as_bytes(), cipher.key_len())
        };
        Ok(ServerCfg { addr, cipher, key })
    }
}

/// OpenSSL EVP_BytesToKey（MD5，无 salt，1 轮）—— SIP004 从密码派生主 key 的方式
pub fn evp_bytes_to_key(password: &[u8], n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n + 16);
    let mut prev: Vec<u8> = Vec::new();
    while out.len() < n {
        let mut h = Md5::new();
        h.update(&prev);
        h.update(password);
        prev = h.finalize().to_vec();
        out.extend_from_slice(&prev);
    }
    out.truncate(n);
    out
}

struct Len(usize);
impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn session_key(cipher: Cipher, key: &[u8], salt: &[u8]) -> Vec<u8> {
    if cipher.is_2022() {
        let mut m = Vec::with_capacity(key.len() + salt.len());
        m.extend_from_slice(key);
        m.extend_from_slice(salt);
        let full = blake3::derive_key("shadowsocks 2022 session subkey", &m);
        full[..cipher.key_len()].to_vec()
    } else {
        let mut out = vec![0u8; key.len()];
        hkdf::Salt::new(hkdf::HKDF_SHA1_FOR_LEGACY_USE_ONLY, salt)
            .extract(key)
            .expand(&[b"ss-subkey"], Len(key.len()))
            .and_then(|okm| okm.fill(&mut out))
            .expect("hkdf 长度固定，不会失败");
        out
    }
}

struct Aead {
    key: LessSafeKey,
    nonce: [u8; 12],
}

impl Aead {
    fn new(cipher: Cipher, subkey: &[u8]) -> Aead {
        let k = UnboundKey::new(cipher.alg(), subkey).expect("key 长度由 cipher 决定");
        Aead { key: LessSafeKey::new(k), nonce: [0; 12] }
    }
    fn next(&mut self) -> Nonce {
        let n = Nonce::assume_unique_for_key(self.nonce);
        for b in self.nonce.iter_mut() {
            *b = b.wrapping_add(1);
            if *b != 0 {
                break;
            }
        }
        n
    }
    fn seal(&mut self, plain: &[u8], out: &mut Vec<u8>) {
        let mut buf = plain.to_vec();
        let n = self.next();
        self.key.seal_in_place_append_tag(n, Aad::empty(), &mut buf).expect("seal 不会失败");
        out.extend_from_slice(&buf);
    }
    fn open(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.next();
        self.key
            .open_in_place(n, Aad::empty(), buf)
            .map(|p| p.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "aead 解密失败（密码或加密方式不符）"))
    }
}

fn random(buf: &mut [u8]) {
    SystemRandom::new().fill(buf).expect("系统随机数不可用");
}

/// SOCKS5 风格的目标地址（SS 头里用的格式）
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Domain(String, u16),
    Addr(SocketAddr),
}

impl Target {
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Target::Domain(h, p) => {
                let h = h.as_bytes();
                out.push(0x03);
                out.push(h.len().min(255) as u8);
                out.extend_from_slice(&h[..h.len().min(255)]);
                out.extend_from_slice(&p.to_be_bytes());
            }
            Target::Addr(SocketAddr::V4(a)) => {
                out.push(0x01);
                out.extend_from_slice(&a.ip().octets());
                out.extend_from_slice(&a.port().to_be_bytes());
            }
            Target::Addr(SocketAddr::V6(a)) => {
                out.push(0x04);
                out.extend_from_slice(&a.ip().octets());
                out.extend_from_slice(&a.port().to_be_bytes());
            }
        }
    }
    /// 解析地址头，返回（地址，占用字节数）
    pub fn decode(b: &[u8]) -> Option<(Target, usize)> {
        match *b.first()? {
            0x01 if b.len() >= 7 => {
                let ip = std::net::Ipv4Addr::new(b[1], b[2], b[3], b[4]);
                Some((Target::Addr(SocketAddr::new(ip.into(), u16::from_be_bytes([b[5], b[6]]))), 7))
            }
            0x04 if b.len() >= 19 => {
                let mut o = [0u8; 16];
                o.copy_from_slice(&b[1..17]);
                let ip = std::net::Ipv6Addr::from(o);
                Some((Target::Addr(SocketAddr::new(ip.into(), u16::from_be_bytes([b[17], b[18]]))), 19))
            }
            0x03 => {
                let n = *b.get(1)? as usize;
                if b.len() < 2 + n + 2 {
                    return None;
                }
                let h = String::from_utf8_lossy(&b[2..2 + n]).into_owned();
                Some((Target::Domain(h, u16::from_be_bytes([b[2 + n], b[3 + n]])), 4 + n))
            }
            _ => None,
        }
    }
}

/// 一条 SS TCP 隧道。请求头随第一次 [`SsStream::send`] 一起发出（与常见客户端一致，不多一个 RTT）。
pub struct SsStream {
    tcp: TcpStream,
    cipher: Cipher,
    key: Vec<u8>,
    salt: Vec<u8>,
    enc: Aead,
    dec: Option<Aead>,
    pending: Option<Target>,
    eof: bool,
}

impl SsStream {
    /// 只做 TCP 连接（返回后即可计 connect 耗时）；SS 头在第一次 send 时发出
    pub async fn connect(cfg: &ServerCfg, target: Target) -> io::Result<SsStream> {
        let tcp = TcpStream::connect(cfg.addr).await?;
        let _ = tcp.set_nodelay(true);
        let mut salt = vec![0u8; cfg.cipher.salt_len()];
        random(&mut salt);
        let enc = Aead::new(cfg.cipher, &session_key(cfg.cipher, &cfg.key, &salt));
        Ok(SsStream { tcp, cipher: cfg.cipher, key: cfg.key.clone(), salt, enc, dec: None, pending: Some(target), eof: false })
    }

    fn seal_chunk(&mut self, c: &[u8], out: &mut Vec<u8>) {
        self.enc.seal(&(c.len() as u16).to_be_bytes(), out);
        self.enc.seal(c, out);
    }

    pub async fn send(&mut self, data: &[u8]) -> io::Result<()> {
        let mut out = Vec::with_capacity(data.len() + 160);
        let mut rest = data;
        if let Some(t) = self.pending.take() {
            out.extend_from_slice(&self.salt);
            if self.cipher.is_2022() {
                let mut var = Vec::with_capacity(rest.len().min(8192) + 300);
                t.encode(&mut var);
                let take = rest.len().min(8192);
                // SIP022：首包没有载荷时必须带填充
                let pad = if take == 0 {
                    let mut r = [0u8; 1];
                    random(&mut r);
                    1 + (r[0] as usize % 64)
                } else {
                    0
                };
                var.extend_from_slice(&(pad as u16).to_be_bytes());
                var.resize(var.len() + pad, 0);
                var.extend_from_slice(&rest[..take]);
                rest = &rest[take..];
                let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                let mut fixed = Vec::with_capacity(11);
                fixed.push(0u8); // 0 = client stream
                fixed.extend_from_slice(&ts.to_be_bytes());
                fixed.extend_from_slice(&(var.len() as u16).to_be_bytes());
                self.enc.seal(&fixed, &mut out);
                self.enc.seal(&var, &mut out);
            } else {
                let mut first = Vec::with_capacity(rest.len().min(0x3FFF) + 260);
                t.encode(&mut first);
                let take = rest.len().min(0x3FFF - first.len());
                first.extend_from_slice(&rest[..take]);
                rest = &rest[take..];
                self.seal_chunk(&first, &mut out);
            }
        }
        let max = self.cipher.max_chunk();
        for c in rest.chunks(max) {
            self.seal_chunk(c, &mut out);
        }
        self.tcp.write_all(&out).await
    }

    /// 读下一个解密后的数据块；空 Vec 表示对端在块边界上正常关闭
    pub async fn recv(&mut self) -> io::Result<Vec<u8>> {
        if self.eof {
            return Ok(Vec::new());
        }
        if self.dec.is_none() {
            let sl = self.cipher.salt_len();
            let mut salt = vec![0u8; sl];
            if !read_exact_or_eof(&mut self.tcp, &mut salt).await? {
                self.eof = true;
                return Ok(Vec::new());
            }
            let mut dec = Aead::new(self.cipher, &session_key(self.cipher, &self.key, &salt));
            if self.cipher.is_2022() {
                // 响应固定头：type(1)=1 | timestamp(8) | 请求 salt | 长度(2)
                let n = 1 + 8 + sl + 2;
                let mut h = vec![0u8; n + 16];
                self.tcp.read_exact(&mut h).await?;
                dec.open(&mut h)?;
                if h[0] != 1 || h[9..9 + sl] != self.salt[..] {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "2022 响应头无效（请求 salt 不符）"));
                }
                let len = u16::from_be_bytes([h[9 + sl], h[10 + sl]]) as usize;
                let mut p = vec![0u8; len + 16];
                self.tcp.read_exact(&mut p).await?;
                let l = dec.open(&mut p)?;
                p.truncate(l);
                self.dec = Some(dec);
                return Ok(p);
            }
            self.dec = Some(dec);
        }
        let mut lb = [0u8; 18];
        if !read_exact_or_eof(&mut self.tcp, &mut lb).await? {
            self.eof = true;
            return Ok(Vec::new());
        }
        let dec = self.dec.as_mut().expect("上面已初始化");
        dec.open(&mut lb)?;
        let mut len = u16::from_be_bytes([lb[0], lb[1]]) as usize;
        if !self.cipher.is_2022() {
            len &= 0x3FFF;
        }
        let mut p = vec![0u8; len + 16];
        self.tcp.read_exact(&mut p).await?;
        let l = dec.open(&mut p)?;
        p.truncate(l);
        Ok(p)
    }
}

/// 读满 buf；在读到第一个字节之前对端关闭则返回 Ok(false)
async fn read_exact_or_eof(s: &mut TcpStream, buf: &mut [u8]) -> io::Result<bool> {
    let mut got = 0;
    while got < buf.len() {
        let n = s.read(&mut buf[got..]).await?;
        if n == 0 {
            if got == 0 {
                return Ok(false);
            }
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "隧道在数据块中间断开"));
        }
        got += n;
    }
    Ok(true)
}

/// UDP relay 单包往返（SIP004 AEAD）：发一个包，等第一个能解开的回包，返回（来源地址，载荷）。
/// 超时返回 Ok(None)。
pub async fn udp_exchange(cfg: &ServerCfg, target: &Target, payload: &[u8], wait: Duration) -> io::Result<Option<(Target, Vec<u8>)>> {
    if cfg.cipher.is_2022() {
        return Err(io::Error::new(io::ErrorKind::Unsupported, "2022 UDP 未实现"));
    }
    let bind: SocketAddr = if cfg.addr.is_ipv4() { "0.0.0.0:0".parse().unwrap() } else { "[::]:0".parse().unwrap() };
    let sock = UdpSocket::bind(bind).await?;
    sock.connect(cfg.addr).await?;
    let sl = cfg.cipher.salt_len();
    let mut salt = vec![0u8; sl];
    random(&mut salt);
    let mut plain = Vec::with_capacity(payload.len() + 64);
    target.encode(&mut plain);
    plain.extend_from_slice(payload);
    let mut pkt = salt.clone();
    Aead::new(cfg.cipher, &session_key(cfg.cipher, &cfg.key, &salt)).seal(&plain, &mut pkt);
    sock.send(&pkt).await?;
    let deadline = tokio::time::Instant::now() + wait;
    let mut buf = vec![0u8; 2048];
    loop {
        let n = match tokio::time::timeout_at(deadline, sock.recv(&mut buf)).await {
            Ok(r) => r?,
            Err(_) => return Ok(None),
        };
        if n < sl + 16 {
            continue;
        }
        let rsalt = buf[..sl].to_vec();
        let mut body = buf[sl..n].to_vec();
        let Ok(l) = Aead::new(cfg.cipher, &session_key(cfg.cipher, &cfg.key, &rsalt)).open(&mut body) else { continue };
        body.truncate(l);
        if let Some((addr, used)) = Target::decode(&body) {
            return Ok(Some((addr, body[used..].to_vec())));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evp_matches_openssl() {
        // `openssl enc -aes-256-cbc -k foobar -P -md md5 -nosalt` → key=3858F62230AC3C915F300C664312C63F56BA...
        let k = evp_bytes_to_key(b"foobar", 32);
        assert_eq!(hex(&k[..16]), "3858f62230ac3c915f300c664312c63f");
    }

    #[test]
    fn target_roundtrip() {
        for t in [Target::Domain("www.cloudflare.com".into(), 443), Target::Addr("1.2.3.4:3478".parse().unwrap()), Target::Addr("[2001:db8::1]:53".parse().unwrap())] {
            let mut b = Vec::new();
            t.encode(&mut b);
            b.extend_from_slice(b"xyz");
            let (d, n) = Target::decode(&b).unwrap();
            assert_eq!(d, t);
            assert_eq!(&b[n..], b"xyz");
        }
    }

    #[test]
    fn nonce_increments_little_endian() {
        let mut a = Aead::new(Cipher::Aes256Gcm, &[7u8; 32]);
        a.nonce = [255, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let _ = a.next();
        assert_eq!(a.nonce[..3], [0, 0, 1]);
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
