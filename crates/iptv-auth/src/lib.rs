//! Token 哈希、会话、TOTP、限流（开发文档 §1.1、§2、§13）。
//!
//! - 管理员密码：Argon2id（PHC 串入库，永不明文）。
//! - TOTP：RFC 6238 自研实现（HMAC-SHA1 + Base32），不依赖 `totp-rs` 的
//!   版本 API；含 RFC 6238 测试向量单测。
//! - IP 脱敏：`192.168.1.23` → `192.168.***.**`（§4.1 门户展示）。
// 作者: Passwd Word

use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::Argon2;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type HmacSha1 = Hmac<Sha1>;

/// 管理员密码哈希（Argon2id，默认参数）。
pub fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)?
        .to_string())
}

/// 校验管理员密码。哈希损坏时返回 false（不抛错，避免登录口信息泄露）。
pub fn verify_password(hash: &str, password: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// SHA-256 hex（Token / 会话哈希入库用，§4.3 / §13）。
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(data))
}

/// IP 脱敏：保留前两段（IPv4）或前 4 组（IPv6），其余打码。
pub fn mask_ip(ip: &str) -> String {
    if let Ok(addr) = ip.parse::<std::net::IpAddr>() {
        match addr {
            std::net::IpAddr::V4(v4) => {
                let o = v4.octets();
                format!("{}.{}.***.**", o[0], o[1])
            }
            std::net::IpAddr::V6(v6) => {
                let s = v6.segments();
                format!("{:x}:{:x}:{:x}:{:x}::****", s[0], s[1], s[2], s[3])
            }
        }
    } else {
        "***".to_string()
    }
}

// ---------------------------------------------------------------------------
// TOTP（RFC 6238）
// ---------------------------------------------------------------------------

const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::new();
    let mut buf: u32 = 0;
    let mut bits = 0;
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32_ALPHABET[((buf >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(BASE32_ALPHABET[((buf << (5 - bits)) & 31) as usize] as char);
    }
    while out.len() % 8 != 0 {
        out.push('=');
    }
    out
}

pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf: u32 = 0;
    let mut bits = 0;
    for c in s.trim_end_matches('=').chars() {
        let v = BASE32_ALPHABET
            .iter()
            .position(|&x| x as char == c.to_ascii_uppercase())? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// 生成 TOTP 密钥（160 bit，Base32）。
pub fn generate_totp_secret() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut bytes);
    base32_encode(&bytes)
}

pub struct Totp {
    secret: Vec<u8>,
    digits: u32,
    step: u64,
}

impl Totp {
    pub fn new(secret_base32: &str) -> Option<Self> {
        Some(Self {
            secret: base32_decode(secret_base32)?,
            digits: 6,
            step: 30,
        })
    }

    pub fn code_at(&self, unix_secs: u64) -> String {
        let counter = unix_secs / self.step;
        let mut mac = HmacSha1::new_from_slice(&self.secret).expect("hmac key");
        mac.update(&counter.to_be_bytes());
        let hash = mac.finalize().into_bytes();
        let offset = (hash[hash.len() - 1] & 0x0f) as usize;
        let code = (((hash[offset] & 0x7f) as u32) << 24)
            | ((hash[offset + 1] as u32) << 16)
            | ((hash[offset + 2] as u32) << 8)
            | (hash[offset + 3] as u32);
        let modulus = 10u32.pow(self.digits);
        format!("{:0width$}", code % modulus, width = self.digits as usize)
    }

    /// 校验当前码，允许前后 1 步漂移（`window = 1`）。
    pub fn verify(&self, code: &str, now_unix: u64) -> bool {
        if code.len() != self.digits as usize || !code.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        for delta in [0i64, -1, 1] {
            let t = (now_unix as i64 + delta * self.step as i64).max(0) as u64;
            if self.code_at(t) == code {
                return true;
            }
        }
        false
    }

    pub fn provisioning_uri(&self, account: &str, issuer: &str) -> String {
        format!(
            "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits=6&period=30",
            url_encode(issuer),
            url_encode(account),
            base32_encode(&self.secret),
            url_encode(issuer)
        )
    }
}

fn url_encode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 内存限流（§13：按 IP / Token / 管理员账号三维限流）。
// Redis 限流走 `iptv-concurrency` 的同款结构；这里是登录口的轻量实现。
// ---------------------------------------------------------------------------

pub struct RateLimiter {
    limit: u32,
    window: Duration,
    buckets: Mutex<HashMap<String, (u32, Instant)>>,
}

impl RateLimiter {
    pub fn new(limit: u32, window_secs: u64) -> Self {
        Self {
            limit,
            window: Duration::from_secs(window_secs),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// 命中一次；超限返回 false。
    pub fn check(&self, key: &str) -> bool {
        let mut buckets = self.buckets.lock().unwrap();
        let now = Instant::now();
        let entry = buckets.entry(key.to_string()).or_insert((0, now));
        if now.duration_since(entry.1) > self.window {
            *entry = (1, now);
            return true;
        }
        if entry.0 >= self.limit {
            return false;
        }
        entry.0 += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_roundtrip() {
        let h = hash_password("correct horse battery staple").unwrap();
        assert!(verify_password(&h, "correct horse battery staple"));
        assert!(!verify_password(&h, "wrong"));
        assert!(!verify_password("not-a-hash", "x"));
    }

    #[test]
    fn totp_rfc6238_vectors() {
        // RFC 6238 附录 B：secret = "12345678901234567890" 的 ASCII，
        // Base32 为 GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ。
        let totp = Totp::new("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ").unwrap();
        assert_eq!(totp.code_at(59), "287082");
        assert_eq!(totp.code_at(1_111_111_109), "081804");
        assert_eq!(totp.code_at(1_111_111_111), "050471");
        assert!(totp.verify("287082", 59));
        assert!(!totp.verify("287083", 59));
    }

    #[test]
    fn mask_ip_cases() {
        assert_eq!(mask_ip("192.168.1.23"), "192.168.***.**");
        assert_eq!(mask_ip("10.0.0.1"), "10.0.***.**");
        assert_eq!(mask_ip("garbage"), "***");
    }

    #[test]
    fn rate_limiter_blocks() {
        let rl = RateLimiter::new(2, 60);
        assert!(rl.check("ip-1"));
        assert!(rl.check("ip-1"));
        assert!(!rl.check("ip-1"));
        assert!(rl.check("ip-2"));
    }
}
