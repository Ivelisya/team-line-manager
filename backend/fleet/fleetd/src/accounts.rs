//! fleetd 自己的账户与会话（替代 Komari 登录）。
//!
//! - 密码：PBKDF2-HMAC-SHA256（310 000 轮，16 字节随机盐）；从 Komari 导入的旧哈希（base64(sha256(pw + 固定盐))）
//!   登录成功后自动升级成 PBKDF2。
//! - 会话：随机 32 字节令牌，库里只存 sha256；Cookie `fleet_session`（HttpOnly、Secure、SameSite=Strict、Path=/）。
//!   绝对有效期 7 天，闲置 24 小时失效。
//! - 两步验证：可选 TOTP（RFC 6238，SHA1、6 位、30 s，允许 ±1 步）。
//! - 登录限速：同一来源 IP 或同一用户名 15 分钟内失败 5 次 → 锁 15 分钟（内存，重启清零）。
use hmac::{Hmac, Mac};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;

pub const COOKIE: &str = "fleet_session";
pub const SESSION_ABS_S: f64 = 7.0 * 86400.0;
pub const SESSION_IDLE_S: f64 = 86400.0;
pub const PBKDF2_ROUNDS: u32 = 310_000;
const KOMARI_SALT: &str = "06Wm4Jv1Hkxx";

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users(id INTEGER PRIMARY KEY, username TEXT NOT NULL UNIQUE, pw_scheme TEXT NOT NULL, pw_hash TEXT NOT NULL,
  totp_secret TEXT, totp_pending TEXT, source TEXT NOT NULL, created_at REAL NOT NULL, updated_at REAL NOT NULL, disabled_at REAL) STRICT;
CREATE TABLE IF NOT EXISTS sessions(id INTEGER PRIMARY KEY, token_sha256 TEXT NOT NULL UNIQUE, user_id INTEGER NOT NULL,
  created_at REAL NOT NULL, last_seen REAL NOT NULL, expires_at REAL NOT NULL, ip TEXT, ua TEXT, revoked_at REAL) STRICT;
CREATE INDEX IF NOT EXISTS sessions_user ON sessions(user_id);
"#;

pub fn random_bytes(n: usize) -> Vec<u8> {
    use std::io::Read;
    let mut b = vec![0u8; n];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).expect("/dev/urandom");
    b
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

fn pbkdf2(pw: &str, salt: &[u8], rounds: u32) -> [u8; 32] {
    // PBKDF2-HMAC-SHA256，单块（32 字节输出）
    let mut mac = Hmac::<Sha256>::new_from_slice(pw.as_bytes()).unwrap();
    mac.update(salt);
    mac.update(&1u32.to_be_bytes());
    let mut u: [u8; 32] = mac.finalize().into_bytes().into();
    let mut out = u;
    for _ in 1..rounds {
        let mut m = Hmac::<Sha256>::new_from_slice(pw.as_bytes()).unwrap();
        m.update(&u);
        u = m.finalize().into_bytes().into();
        for i in 0..32 {
            out[i] ^= u[i];
        }
    }
    out
}

/// 新密码哈希：`pbkdf2-sha256$<rounds>$<salt hex>$<hash hex>`
pub fn hash_password(pw: &str) -> String {
    let salt = random_bytes(16);
    format!("{}${}${}", PBKDF2_ROUNDS, hex::encode(&salt), hex::encode(pbkdf2(pw, &salt, PBKDF2_ROUNDS)))
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn komari_hash(pw: &str) -> String {
    use base64_lite::encode;
    encode(&Sha256::digest(format!("{pw}{KOMARI_SALT}").as_bytes()))
}

pub fn verify_password(scheme: &str, stored: &str, pw: &str) -> bool {
    match scheme {
        "pbkdf2-sha256" => {
            let parts: Vec<&str> = stored.split('$').collect();
            if parts.len() != 3 {
                return false;
            }
            let (Ok(rounds), Ok(salt), Ok(want)) = (parts[0].parse::<u32>(), hex::decode(parts[1]), hex::decode(parts[2])) else { return false };
            ct_eq(&pbkdf2(pw, &salt, rounds), &want)
        }
        "komari-sha256" => ct_eq(komari_hash(pw).as_bytes(), stored.as_bytes()),
        _ => false,
    }
}

/// 最小 base64（标准字母表、带填充），只用来复现 Komari 的哈希格式
mod base64_lite {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    pub fn encode(b: &[u8]) -> String {
        let mut s = String::new();
        for c in b.chunks(3) {
            let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
            s.push(T[(n >> 18 & 63) as usize] as char);
            s.push(T[(n >> 12 & 63) as usize] as char);
            s.push(if c.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
            s.push(if c.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
        }
        s
    }
}

// ─── TOTP（RFC 6238） ──────────────────────────────────────────────────────────────────────────────

const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub fn base32_encode(b: &[u8]) -> String {
    let mut out = String::new();
    let mut buf: u64 = 0;
    let mut bits = 0;
    for &x in b {
        buf = (buf << 8) | x as u64;
        bits += 8;
        while bits >= 5 {
            out.push(B32[((buf >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(B32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = vec![];
    let mut buf: u64 = 0;
    let mut bits = 0;
    for ch in s.chars().filter(|c| *c != '=' && !c.is_whitespace()) {
        let v = B32.iter().position(|&x| x as char == ch.to_ascii_uppercase())? as u64;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            out.push(((buf >> (bits - 8)) & 0xff) as u8);
            bits -= 8;
        }
    }
    Some(out)
}

pub fn totp_at(secret: &[u8], step: u64) -> u32 {
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(secret).unwrap();
    mac.update(&step.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let o = (h[19] & 0xf) as usize;
    ((u32::from(h[o]) & 0x7f) << 24 | u32::from(h[o + 1]) << 16 | u32::from(h[o + 2]) << 8 | u32::from(h[o + 3])) % 1_000_000
}

pub fn totp_check(secret_b32: &str, code: &str, now: f64) -> bool {
    let Some(secret) = base32_decode(secret_b32) else { return false };
    let Ok(c) = code.trim().parse::<u32>() else { return false };
    let step = (now as u64) / 30;
    [step.saturating_sub(1), step, step + 1].iter().any(|s| totp_at(&secret, *s) == c)
}

// ─── 库操作 ────────────────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub pw_scheme: String,
    pub pw_hash: String,
    pub totp_secret: Option<String>,
}

pub fn user_by_name(c: &Connection, name: &str) -> Option<User> {
    c.query_row(
        "SELECT id, username, pw_scheme, pw_hash, totp_secret FROM users WHERE username=? AND disabled_at IS NULL",
        [name],
        |r| Ok(User { id: r.get(0)?, username: r.get(1)?, pw_scheme: r.get(2)?, pw_hash: r.get(3)?, totp_secret: r.get(4)? }),
    )
    .optional()
    .ok()
    .flatten()
}

pub fn upsert_user(c: &Connection, name: &str, scheme: &str, hash: &str, totp: Option<&str>, source: &str, now: f64) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO users(username, pw_scheme, pw_hash, totp_secret, source, created_at, updated_at) VALUES(?1,?2,?3,?4,?5,?6,?6)
         ON CONFLICT(username) DO UPDATE SET pw_scheme=excluded.pw_scheme, pw_hash=excluded.pw_hash,
           totp_secret=coalesce(excluded.totp_secret, users.totp_secret), updated_at=excluded.updated_at, disabled_at=NULL",
        params![name, scheme, hash, totp, source, now],
    )?;
    Ok(())
}

pub fn create_session(c: &Connection, user_id: i64, ip: &str, ua: &str, now: f64) -> rusqlite::Result<String> {
    let token = hex::encode(random_bytes(32));
    c.execute(
        "INSERT INTO sessions(token_sha256, user_id, created_at, last_seen, expires_at, ip, ua) VALUES(?,?,?,?,?,?,?)",
        params![sha256_hex(&token), user_id, now, now, now + SESSION_ABS_S, ip, ua.chars().take(200).collect::<String>()],
    )?;
    Ok(token)
}

/// 校验会话；有效则刷新 last_seen（最多每分钟写一次）并返回 (session id, username)
pub fn check_session(c: &Connection, token: &str, now: f64) -> Option<(i64, String)> {
    let h = sha256_hex(token);
    let row: Option<(i64, String, f64, f64)> = c
        .query_row(
            "SELECT s.id, u.username, s.last_seen, s.expires_at FROM sessions s JOIN users u ON u.id=s.user_id
             WHERE s.token_sha256=? AND s.revoked_at IS NULL AND u.disabled_at IS NULL",
            [&h],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .ok()
        .flatten();
    let (id, name, last, exp) = row?;
    if now > exp || now - last > SESSION_IDLE_S {
        return None;
    }
    if now - last > 60.0 {
        let _ = c.execute("UPDATE sessions SET last_seen=? WHERE id=?", params![now, id]);
    }
    Some((id, name))
}

pub fn revoke_session_token(c: &Connection, token: &str, now: f64) -> rusqlite::Result<usize> {
    c.execute("UPDATE sessions SET revoked_at=? WHERE token_sha256=? AND revoked_at IS NULL", params![now, sha256_hex(token)])
}

pub fn list_sessions(c: &Connection, username: &str, now: f64) -> Vec<Value> {
    let mut st = c
        .prepare(
            "SELECT s.id, s.created_at, s.last_seen, s.expires_at, s.ip, s.ua FROM sessions s JOIN users u ON u.id=s.user_id
             WHERE u.username=? AND s.revoked_at IS NULL AND s.expires_at>? AND s.last_seen>? ORDER BY s.last_seen DESC",
        )
        .unwrap();
    st.query_map(params![username, now, now - SESSION_IDLE_S], |r| {
        Ok(json!({"id": r.get::<_, i64>(0)?, "created_at": r.get::<_, f64>(1)?, "last_seen": r.get::<_, f64>(2)?,
            "expires_at": r.get::<_, f64>(3)?, "ip": r.get::<_, Option<String>>(4)?, "ua": r.get::<_, Option<String>>(5)?}))
    })
    .unwrap()
    .filter_map(Result::ok)
    .collect()
}

// ─── 登录限速 ──────────────────────────────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct RateLimit {
    fails: Mutex<HashMap<String, Vec<f64>>>,
}

pub const FAIL_WINDOW_S: f64 = 900.0;
pub const FAIL_MAX: usize = 5;

impl RateLimit {
    /// 被锁时返回还要等几秒
    pub fn blocked(&self, keys: &[String], now: f64) -> Option<f64> {
        let m = self.fails.lock().unwrap();
        keys.iter()
            .filter_map(|k| {
                let v = m.get(k)?;
                let recent: Vec<f64> = v.iter().copied().filter(|t| now - t < FAIL_WINDOW_S).collect();
                if recent.len() >= FAIL_MAX {
                    Some(FAIL_WINDOW_S - (now - recent[recent.len() - FAIL_MAX]))
                } else {
                    None
                }
            })
            .fold(None, |a: Option<f64>, b| Some(a.map_or(b, |a| a.max(b))))
    }
    pub fn fail(&self, keys: &[String], now: f64) {
        let mut m = self.fails.lock().unwrap();
        if m.len() > 10_000 {
            m.clear();
        }
        for k in keys {
            let v = m.entry(k.clone()).or_default();
            v.retain(|t| now - t < FAIL_WINDOW_S);
            v.push(now);
        }
    }
    pub fn success(&self, keys: &[String]) {
        let mut m = self.fails.lock().unwrap();
        for k in keys {
            m.remove(k);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hashes() {
        let h = hash_password("S3cret!pw");
        assert!(verify_password("pbkdf2-sha256", &h, "S3cret!pw"));
        assert!(!verify_password("pbkdf2-sha256", &h, "wrong"));
        // Komari：base64(sha256(pw + "06Wm4Jv1Hkxx"))
        let k = komari_hash("admin123");
        assert_eq!(k.len(), 44);
        assert!(verify_password("komari-sha256", &k, "admin123"));
        assert!(!verify_password("komari-sha256", &k, "admin124"));
        assert_eq!(base64_lite::encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_lite::encode(b"fo"), "Zm8=");
    }

    #[test]
    fn totp_rfc6238_vector() {
        // RFC 6238 附录 B：SHA1，密钥 "12345678901234567890"，T=59 → 94287082（取后 6 位 287082）
        let secret = b"12345678901234567890";
        assert_eq!(totp_at(secret, 59 / 30), 287082);
        assert_eq!(totp_at(secret, 1111111109 / 30), 81804);
        let b = base32_encode(secret);
        assert_eq!(base32_decode(&b).unwrap(), secret);
        assert!(totp_check(&b, "287082", 59.0));
        assert!(!totp_check(&b, "000000", 59.0));
    }

    #[test]
    fn sessions_and_expiry() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        upsert_user(&c, "admin", "pbkdf2-sha256", &hash_password("x"), None, "cli", 100.0).unwrap();
        let u = user_by_name(&c, "admin").unwrap();
        let t = create_session(&c, u.id, "1.2.3.4", "ua", 100.0).unwrap();
        assert_eq!(check_session(&c, &t, 200.0).unwrap().1, "admin");
        assert!(check_session(&c, &t, 200.0 + SESSION_IDLE_S + 1.0).is_none(), "闲置 24 h 失效");
        let t2 = create_session(&c, u.id, "", "", 100.0).unwrap();
        revoke_session_token(&c, &t2, 150.0).unwrap();
        assert!(check_session(&c, &t2, 160.0).is_none());
        assert!(check_session(&c, "nope", 160.0).is_none());
    }

    #[test]
    fn rate_limit() {
        let r = RateLimit::default();
        let k = vec!["ip:1.2.3.4".to_string()];
        for i in 0..5 {
            assert!(r.blocked(&k, i as f64).is_none());
            r.fail(&k, i as f64);
        }
        assert!(r.blocked(&k, 10.0).is_some());
        assert!(r.blocked(&k, 10.0 + FAIL_WINDOW_S).is_none());
    }
}
