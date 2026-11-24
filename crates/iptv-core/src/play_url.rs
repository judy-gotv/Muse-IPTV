//! 短时效播放地址签名（§6.4 / §8）。
//!
//! `play_url` 携带 `exp`、`session`、`sig`（HMAC-SHA256），默认 300s，
//! 可绑 IP 段或 UA 前缀；上游真实地址不下发给客户端。
//! 格式：`{base}?exp={exp}&session={session_uid}&sig={hex(hmac)}`
//! 其中 `hmac = HMAC-SHA256(secret, "{session_uid}|{channel_id}|{exp}")`。
// 作者: Passwd Word

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// 签发播放地址。`base` 是平台自己的播放代理地址（如 `/p/{channel_id}.m3u8`），
/// 不是上游真实地址。
pub fn sign_play_url(
    secret: &[u8],
    base: &str,
    session_uid: &str,
    channel_id: i64,
    exp_epoch: i64,
) -> String {
    let msg = format!("{session_uid}|{channel_id}|{exp_epoch}");
    let mut mac = HmacSha256::new_from_slice(secret).expect("hmac key");
    mac.update(msg.as_bytes());
    let sig = hex::encode(mac.finalize().into_bytes());
    let sep = if base.contains('?') { "&" } else { "?" };
    format!("{base}{sep}exp={exp_epoch}&session={session_uid}&sig={sig}")
}

/// 校验播放地址签名与有效期。返回 `true` 当且仅当签名正确且 `now <= exp`。
pub fn verify_play_url(
    secret: &[u8],
    session_uid: &str,
    channel_id: i64,
    exp_epoch: i64,
    sig_hex: &str,
    now_epoch: i64,
) -> bool {
    if now_epoch > exp_epoch {
        return false;
    }
    let msg = format!("{session_uid}|{channel_id}|{exp_epoch}");
    let mut mac = HmacSha256::new_from_slice(secret).expect("hmac key");
    mac.update(msg.as_bytes());
    let expected = mac.finalize().into_bytes();
    let Ok(given) = hex::decode(sig_hex) else {
        return false;
    };
    // 等时比较，避免时序侧信道。
    expected.len() == given.len()
        && expected
            .iter()
            .zip(given.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify() {
        let secret = b"test-secret";
        let url = sign_play_url(secret, "/p/12.m3u8", "sess-1", 12, 1_700_000_300);
        assert!(url.contains("exp=1700000300"));
        let sig = url.split("sig=").nth(1).unwrap();
        assert!(verify_play_url(
            secret,
            "sess-1",
            12,
            1_700_000_300,
            sig,
            1_700_000_000
        ));
        assert!(!verify_play_url(
            secret,
            "sess-1",
            12,
            1_700_000_300,
            sig,
            1_700_000_301
        ));
        assert!(!verify_play_url(
            secret,
            "sess-2",
            12,
            1_700_000_300,
            sig,
            1_700_000_000
        ));
        assert!(!verify_play_url(
            b"wrong",
            "sess-1",
            12,
            1_700_000_300,
            sig,
            1_700_000_000
        ));
    }
}
