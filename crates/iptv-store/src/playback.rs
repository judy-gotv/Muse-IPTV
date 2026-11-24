//! 播放会话与事件（`playback_session` / `playback_event`，开发文档 §6.2/§9）。
// 作者: Passwd Word

use super::*;

pub(crate) fn row_to_playback(row: &AnyRow) -> anyhow::Result<PlaybackSession> {
    Ok(PlaybackSession {
        id: r_i64(row, "id")?,
        session_uid: r_string(row, "session_uid")?,
        token_id: r_i64(row, "token_id")?,
        channel_id: r_i64(row, "channel_id")?,
        device_id: r_string(row, "device_id")?,
        ip_address: r_opt_string(row, "ip_address")?,
        quality: r_opt_string(row, "quality")?,
        started_at: r_ts(row, "started_at")?,
        last_heartbeat_at: r_ts(row, "last_heartbeat_at")?,
        ended_at: r_opt_ts(row, "ended_at")?,
        status: r_string(row, "status")?,
    })
}

pub(crate) fn row_to_event(row: &AnyRow) -> anyhow::Result<PlaybackEvent> {
    Ok(PlaybackEvent {
        id: r_i64(row, "id")?,
        session_uid: r_string(row, "session_uid")?,
        token_id: r_i64(row, "token_id")?,
        channel_id: r_i64(row, "channel_id")?,
        event_type: r_string(row, "event_type")?,
        detail: r_json(row, "detail"),
        created_at: r_ts(row, "created_at")?,
    })
}

const PLAYBACK_COLS: &str = "id, session_uid, token_id, channel_id, device_id, ip_address, quality, started_at, last_heartbeat_at, ended_at, status";
const EVENT_COLS: &str = "id, session_uid, token_id, channel_id, event_type, detail, created_at";

impl Store {
    pub async fn create_playback_session(
        &self,
        token_id: i64,
        channel_id: i64,
        device_id: &str,
        session_uid: &str,
        quality: Option<&str>,
        ip: Option<&str>,
    ) -> anyhow::Result<PlaybackSession> {
        let now = Utc::now();
        let id = insert_id!(
            self,
            "INSERT INTO playback_session (session_uid, token_id, channel_id, device_id, ip_address, user_agent, quality, started_at, last_heartbeat_at, ended_at, status) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            session_uid,
            token_id,
            channel_id,
            device_id,
            ip,
            None::<String>,
            quality,
            epoch(now),
            epoch(now),
            None::<i64>,
            "active"
        )?;
        self.get_playback_session(session_uid)
            .await?
            .ok_or_else(|| anyhow::anyhow!("新建播放会话后回查失败 id={id}"))
    }

    /// 补充契约：按 `session_uid` 查单条播放会话。
    pub async fn get_playback_session(
        &self,
        session_uid: &str,
    ) -> anyhow::Result<Option<PlaybackSession>> {
        let row = sqlx::query(&format!(
            "SELECT {PLAYBACK_COLS} FROM playback_session WHERE session_uid = ?"
        ))
        .bind(session_uid)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| row_to_playback(&r)).transpose()
    }

    /// 补充契约：全局 `status='active'` 的播放会话，按 `started_at` 倒序分页。
    pub async fn list_active_playback_sessions(
        &self,
        q: &PageQuery,
    ) -> anyhow::Result<Paged<PlaybackSession>> {
        let (limit, offset) = page_bounds(q);
        let total: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM playback_session WHERE status = 'active'")
                .fetch_one(&self.pool)
                .await?;
        let rows = sqlx::query(&format!(
            "SELECT {PLAYBACK_COLS} FROM playback_session WHERE status = 'active' ORDER BY started_at DESC, id DESC LIMIT ? OFFSET ?"
        ))
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let mut items = Vec::with_capacity(rows.len());
        for r in &rows {
            items.push(row_to_playback(r)?);
        }
        Ok(Paged {
            items,
            total: total as u64,
            page: q.page.max(1),
            page_size: q.page_size.clamp(1, 100),
        })
    }

    pub async fn heartbeat_session(
        &self,
        session_uid: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        let n = sqlx::query(
            "UPDATE playback_session SET last_heartbeat_at = ? WHERE session_uid = ? AND status = 'active'",
        )
        .bind(epoch(now))
        .bind(session_uid)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    pub async fn end_session(
        &self,
        session_uid: &str,
        now: DateTime<Utc>,
        status: &str,
    ) -> anyhow::Result<bool> {
        let n = sqlx::query(
            "UPDATE playback_session SET status = ?, ended_at = ? WHERE session_uid = ? AND status = 'active'",
        )
        .bind(status)
        .bind(epoch(now))
        .bind(session_uid)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    pub async fn active_sessions_for_token(
        &self,
        token_id: i64,
    ) -> anyhow::Result<Vec<PlaybackSession>> {
        let rows = sqlx::query(&format!(
            "SELECT {PLAYBACK_COLS} FROM playback_session WHERE token_id = ? AND status = 'active' ORDER BY started_at ASC"
        ))
        .bind(token_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            out.push(row_to_playback(r)?);
        }
        Ok(out)
    }

    /// Worker 补偿（§6.3）：90s 未心跳的会话过期，DB 状态同步为 `expired`。
    /// 返回过期条数；`ended_at` 写为 cutoff（即最后一次心跳的超时点）。
    pub async fn expire_stale_sessions(&self, cutoff: DateTime<Utc>) -> anyhow::Result<u64> {
        let n = sqlx::query(
            "UPDATE playback_session SET status = 'expired', ended_at = ? WHERE status = 'active' AND last_heartbeat_at < ?",
        )
        .bind(epoch(cutoff))
        .bind(epoch(cutoff))
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n)
    }

    pub async fn playback_history(
        &self,
        token_id: i64,
        q: &PageQuery,
    ) -> anyhow::Result<Paged<PlaybackEvent>> {
        let (limit, offset) = page_bounds(q);
        let total: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM playback_event WHERE token_id = ?")
                .bind(token_id)
                .fetch_one(&self.pool)
                .await?;
        let rows = sqlx::query(&format!(
            "SELECT {EVENT_COLS} FROM playback_event WHERE token_id = ? ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?"
        ))
        .bind(token_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let mut items = Vec::with_capacity(rows.len());
        for r in &rows {
            items.push(row_to_event(r)?);
        }
        Ok(Paged {
            items,
            total: total as u64,
            page: q.page.max(1),
            page_size: q.page_size.clamp(1, 100),
        })
    }

    pub async fn record_event(&self, ev: &NewPlaybackEvent) -> anyhow::Result<()> {
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO playback_event (session_uid, token_id, channel_id, event_type, detail, created_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&ev.session_uid)
        .bind(ev.token_id)
        .bind(ev.channel_id)
        .bind(&ev.event_type)
        .bind(json_text(&ev.detail))
        .bind(epoch(now))
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
