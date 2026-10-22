//! 两步验证：TOTP（RFC 6238：HMAC-SHA1、6 位、30 秒一步，前后各容忍一步）与恢复码。只有纯计算，存取在 api/account.rs。

use hmac::{Hmac, Mac};
use rand::Rng;
use sha1::Sha1;

const STEP_SECS: i64 = 30;
const DIGITS: u32 = 6;
pub const RECOVERY_COUNT: usize = 10;

/// RFC 4226 HOTP：`digits` 位
pub fn hotp(secret: &[u8], counter: u64, digits: u32) -> u32 {
    let mut m = Hmac::<Sha1>::new_from_slice(secret).expect("HMAC 接受任意长度的密钥");
    m.update(&counter.to_be_bytes());
    let h = m.finalize().into_bytes();
    let o = (h[19] & 0x0f) as usize;
    let v = u32::from_be_bytes([h[o] & 0x7f, h[o + 1], h[o + 2], h[o + 3]]);
    v % 10u32.pow(digits)
}

/// 当前时间步
pub fn now_step() -> i64 { time::OffsetDateTime::now_utc().unix_timestamp() / STEP_SECS }

/// 6 位数字（中间可以有空格）才算 TOTP 验证码，其它的按恢复码处理
pub fn parse_code(s: &str) -> Option<u32> {
    let d: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (d.len() == DIGITS as usize && d.bytes().all(|b| b.is_ascii_digit())).then(|| d.parse().ok()).flatten()
}

/// 在 `now` 前后各一步里找与 `code` 吻合、且晚于 `last`（上次接受的那一步）的时间步
pub fn matching_step(secret: &[u8], code: u32, now: i64, last: Option<i64>) -> Option<i64> {
    (now - 1..=now + 1).filter(|s| last.is_none_or(|l| *s > l)).find(|s| hotp(secret, *s as u64, DIGITS) == code)
}

pub fn new_secret() -> [u8; 20] {
    let mut b = [0u8; 20];
    rand::thread_rng().fill(&mut b);
    b
}

pub fn base32(b: &[u8]) -> String { data_encoding::BASE32_NOPAD.encode(b) }

/// 认证器 App 扫的 otpauth:// 链接
pub fn otpauth_url(issuer: &str, user: &str, secret_b32: &str) -> String {
    let (i, u) = (pct(issuer), pct(user));
    format!("otpauth://totp/{i}:{u}?secret={secret_b32}&issuer={i}&algorithm=SHA1&digits={DIGITS}&period={STEP_SECS}")
}

fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// 二维码 SVG（黑白、带静区）
pub fn qr_svg(data: &str) -> anyhow::Result<String> {
    use qrcode::render::svg;
    let q = qrcode::QrCode::new(data.as_bytes())?;
    Ok(q.render::<svg::Color>().min_dimensions(200, 200).dark_color(svg::Color("#000000")).light_color(svg::Color("#ffffff")).build())
}

/// 恢复码字母表：去掉了容易看混的 0/o、1/l/i
const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";

/// 一组新恢复码，形如 `xxxx-xxxx-xxxx`（约 59 位熵）
pub fn new_recovery_codes() -> Vec<String> {
    let mut r = rand::thread_rng();
    (0..RECOVERY_COUNT)
        .map(|_| {
            let c: Vec<char> = (0..12).map(|_| ALPHABET[r.gen_range(0..ALPHABET.len())] as char).collect();
            format!("{}-{}-{}", c[..4].iter().collect::<String>(), c[4..8].iter().collect::<String>(), c[8..].iter().collect::<String>())
        })
        .collect()
}

/// 恢复码入库前的规整：小写、去掉连字符与空白（用户手抄时大小写、分隔可能不同）
pub fn recovery_hash(s: &str) -> Vec<u8> {
    let n: String = s.chars().filter(|c| !c.is_whitespace() && *c != '-').flat_map(char::to_lowercase).collect();
    crate::crypto::sha256(n.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238 附录 B 的 SHA1 测试向量（8 位）；6 位就是取后 6 位
    #[test]
    fn rfc6238_vectors() {
        let k = b"12345678901234567890";
        for (t, want) in [(59, 94287082), (1111111109, 7081804), (1111111111, 14050471), (1234567890, 89005924), (2000000000, 69279037), (20000000000u64, 65353130)] {
            assert_eq!(hotp(k, t / 30, 8), want, "T={t}");
            assert_eq!(hotp(k, t / 30, 6), want % 1_000_000, "T={t}");
        }
    }

    #[test]
    fn window_and_replay() {
        let k = b"12345678901234567890";
        let now = 1111111111 / 30;
        let prev = hotp(k, (now - 1) as u64, 6);
        assert_eq!(matching_step(k, prev, now, None), Some(now - 1), "容忍前一步");
        assert_eq!(matching_step(k, hotp(k, (now + 1) as u64, 6), now, None), Some(now + 1), "容忍后一步");
        assert_eq!(matching_step(k, hotp(k, (now - 2) as u64, 6), now, None), None, "两步之前的不收");
        assert_eq!(matching_step(k, prev, now, Some(now - 1)), None, "用过的那一步不再收");
    }

    #[test]
    fn code_parsing_and_recovery() {
        assert_eq!(parse_code(" 123 456 "), Some(123456));
        assert_eq!(parse_code("012345"), Some(12345));
        assert_eq!(parse_code("12345"), None);
        assert_eq!(parse_code("abcd-efgh-jkmn"), None);
        let codes = new_recovery_codes();
        assert_eq!(codes.len(), RECOVERY_COUNT);
        assert!(codes.iter().all(|c| c.len() == 14 && c.matches('-').count() == 2));
        assert_eq!(recovery_hash(&codes[0]), recovery_hash(&codes[0].to_uppercase().replace('-', " ")));
        assert_eq!(base32(b"12345678901234567890"), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        assert!(otpauth_url("Fleet", "a b", "ABC").starts_with("otpauth://totp/Fleet:a%20b?secret=ABC&issuer=Fleet"));
        assert!(qr_svg("otpauth://totp/x").unwrap().contains("<svg"));
    }
}
