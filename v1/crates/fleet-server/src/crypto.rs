//! 库里秘密字段的加解密（XChaCha20-Poly1305，密文 = 24 字节 nonce ‖ 密文）与随机令牌。

use anyhow::{bail, Context};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Clone)]
pub struct SecretBox {
    cipher: XChaCha20Poly1305,
}

impl SecretBox {
    pub fn from_key_file(path: &Path) -> anyhow::Result<Self> {
        let key = std::fs::read(path).with_context(|| format!("读取密钥 {}", path.display()))?;
        Self::from_key(&key)
    }

    pub fn from_key(key: &[u8]) -> anyhow::Result<Self> {
        if key.len() != 32 {
            bail!("密钥必须是 32 字节，实际 {}", key.len());
        }
        Ok(Self { cipher: XChaCha20Poly1305::new(key.into()) })
    }

    pub fn seal(&self, plain: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; 24];
        rand::thread_rng().fill_bytes(&mut nonce);
        let mut out = nonce.to_vec();
        out.extend(self.cipher.encrypt(XNonce::from_slice(&nonce), plain).expect("encrypt"));
        out
    }

    pub fn seal_str(&self, s: &str) -> Vec<u8> { self.seal(s.as_bytes()) }

    pub fn open(&self, data: &[u8]) -> anyhow::Result<Vec<u8>> {
        if data.len() < 24 {
            bail!("密文太短");
        }
        let (n, c) = data.split_at(24);
        self.cipher.decrypt(XNonce::from_slice(n), c).map_err(|_| anyhow::anyhow!("解密失败（密钥不对？）"))
    }

    pub fn open_str(&self, data: &[u8]) -> anyhow::Result<String> {
        Ok(String::from_utf8(self.open(data)?)?)
    }
}

/// URL 安全的随机串（用于令牌、会话、密码）
pub fn random_token(bytes: usize) -> String {
    use base64::Engine;
    let mut b = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut b);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

pub fn sha256(s: &[u8]) -> Vec<u8> { Sha256::digest(s).to_vec() }

/// Xray REALITY 用的 x25519 密钥对（base64url 无填充，与 `xray x25519` 输出一致）
pub fn reality_keypair() -> (String, String) {
    use base64::Engine;
    let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let mut sk = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut sk);
    let secret = x25519_dalek::StaticSecret::from(sk);
    let public = x25519_dalek::PublicKey::from(&secret);
    (e.encode(secret.to_bytes()), e.encode(public.as_bytes()))
}

/// REALITY short id：8 字节十六进制
pub fn short_id() -> String {
    let mut b = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip() {
        let b = SecretBox::from_key(&[7u8; 32]).unwrap();
        let c = b.seal_str("hello");
        assert_eq!(b.open_str(&c).unwrap(), "hello");
        assert!(SecretBox::from_key(&[8u8; 32]).unwrap().open(&c).is_err());
    }

    #[test]
    fn reality_keys_are_32_bytes() {
        use base64::Engine;
        let (sk, pk) = reality_keypair();
        let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        assert_eq!(e.decode(sk).unwrap().len(), 32);
        assert_eq!(e.decode(pk).unwrap().len(), 32);
    }
}
