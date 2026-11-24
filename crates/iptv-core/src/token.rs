//! Token 生成与哈希（§4.3 / §13）。
//!
//! - 明文格式 `tk_live_<32 hex>`（64 位熵以上，`rand` 密码学随机）。
//! - DB 只存 SHA-256（`hash_token`），`token_prefix` 存明文前 8 位用于索引。
// 作者: Passwd Word

use rand::RngCore;
use sha2::{Digest, Sha256};

pub const TOKEN_PREFIX_STR: &str = "tk_live_";

/// 生成一个新的长期 Token 明文。调用方必须当场展示一次，永不入库。
pub fn generate_token() -> String {
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("{}{}", TOKEN_PREFIX_STR, hex::encode(bytes))
}

/// SHA-256(明文)，入库用。
pub fn hash_token(plain: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(plain.as_bytes());
    h.finalize().into()
}

/// 明文前 8 位（`tk_live_` 之后的首 8 个 hex 字符取前 8 位整体，见 §6.2）。
pub fn token_prefix(plain: &str) -> String {
    plain.chars().take(8).collect()
}

/// 生成 256 bit 会话 Cookie 值（§4.3：`iptv_portal_sid`，DB 只存哈希）。
pub fn generate_session_value() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// 生成一次性登录票据（§4.3：60s 单次消费）。
pub fn generate_ticket() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_roundtrip() {
        let t = generate_token();
        assert!(t.starts_with(TOKEN_PREFIX_STR));
        assert_eq!(t.len(), TOKEN_PREFIX_STR.len() + 48);
        let h1 = hash_token(&t);
        let h2 = hash_token(&t);
        assert_eq!(h1, h2);
        assert_ne!(hash_token(&generate_token()), h1);
        assert_eq!(token_prefix(&t).len(), 8);
    }

    #[test]
    fn session_value_is_256_bit() {
        assert_eq!(generate_session_value().len(), 64);
        assert_eq!(generate_ticket().len(), 32);
    }
}
