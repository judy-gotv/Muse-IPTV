//! 播放判定策略（§6.4 播放请求判定 6 步中的纯规则部分）。
//!
//! 顺序：Token 状态与有效期 → 频道鉴权（授权分组 / 黑名单 / 清晰度上限）
//! → 并发扣减（由 `iptv-concurrency` 在 Redis 内原子完成，不在这里）。
// 作者: Passwd Word

use chrono::{DateTime, Utc};
use iptv_dto::{Channel, ErrorCode, UserToken};

use crate::error::CoreError;

/// 播放判定结果：通过时给出允许的最高清晰度。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackDecision {
    /// 实际允许的清晰度（取 Token 上限与请求清晰度的较低者）。
    pub quality: String,
}

/// 清晰度排序（用于取“较低者”）。
pub fn quality_rank(q: &str) -> u32 {
    match q.to_ascii_lowercase().as_str() {
        "144p" => 1,
        "240p" => 2,
        "360p" => 3,
        "480p" => 4,
        "540p" => 5,
        "720p" => 6,
        "720p60" => 7,
        "1080p" => 8,
        "1080p60" => 9,
        "1440p" | "2k" => 10,
        "2160p" | "4k" => 11,
        _ => 0, // 未知清晰度视为最低，不直接放行高档
    }
}

/// §6.4 判定：Token → 频道授权 → 清晰度上限。
pub fn authorize_channel(
    token: &UserToken,
    channel: &Channel,
    requested_quality: Option<&str>,
    now: DateTime<Utc>,
) -> Result<PlaybackDecision, CoreError> {
    if !token.is_usable(now) {
        return Err(CoreError::coded(
            ErrorCode::TokenDisabledOrExpired,
            "token disabled or expired",
        ));
    }
    if token.denied_channels.contains(&channel.id) {
        return Err(CoreError::coded(
            ErrorCode::ChannelForbidden,
            "channel denied for this token",
        ));
    }
    if let Some(group) = channel.group_title.as_deref() {
        if !token.allowed_groups.is_empty() && !token.allowed_groups.iter().any(|g| g == group) {
            return Err(CoreError::coded(
                ErrorCode::ChannelForbidden,
                "channel group not allowed",
            ));
        }
    }
    if channel.status != iptv_dto::ChannelStatus::Active {
        return Err(CoreError::coded(
            ErrorCode::SourceUnavailable,
            "channel unavailable",
        ));
    }
    let max_q = token.features.max_quality.as_str();
    let want = requested_quality.unwrap_or(max_q);
    let quality = if quality_rank(want) <= quality_rank(max_q) {
        want.to_string()
    } else {
        max_q.to_string()
    };
    Ok(PlaybackDecision { quality })
}

#[cfg(test)]
mod tests {
    use super::*;
    use iptv_dto::{ChannelStatus, TokenFeatures, TokenStatus};

    fn token() -> UserToken {
        UserToken {
            id: 1,
            user_id: 1,
            token_prefix: "tk_live_".into(),
            name: "客厅".into(),
            device_type: Some("android_tv".into()),
            locale: "zh-Hant-TW".into(),
            max_concurrent_sessions: 2,
            max_bound_devices: 3,
            status: TokenStatus::Active,
            expires_at: None,
            allowed_groups: vec!["体育".into()],
            denied_channels: vec![99],
            features: TokenFeatures {
                catchup: true,
                recording: false,
                max_quality: "720p".into(),
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }
    fn channel(id: i64, group: &str) -> Channel {
        Channel {
            id,
            tvg_id: None,
            name: "CCTV-5".into(),
            display_name: None,
            logo_url: None,
            group_title: Some(group.into()),
            language: None,
            country: None,
            source_id: None,
            content_hash: None,
            status: ChannelStatus::Active,
            visibility: "public".into(),
            sort_weight: 0,
            last_checked_at: None,
            last_available: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn decision_ok_caps_quality() {
        let d =
            authorize_channel(&token(), &channel(5, "体育"), Some("1080p"), Utc::now()).unwrap();
        assert_eq!(d.quality, "720p");
        let d = authorize_channel(&token(), &channel(5, "体育"), Some("480p"), Utc::now()).unwrap();
        assert_eq!(d.quality, "480p");
    }

    #[test]
    fn decision_denied() {
        assert!(authorize_channel(&token(), &channel(99, "体育"), None, Utc::now()).is_err());
        assert!(authorize_channel(&token(), &channel(5, "电影"), None, Utc::now()).is_err());
    }
}
