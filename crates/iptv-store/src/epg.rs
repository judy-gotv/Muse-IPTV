//! EPG（`epg_source` / `epg_program`，开发文档 §9 / §14.1，XMLTV）。
// 作者: Passwd Word

use super::*;

pub(crate) fn row_to_epg_source(row: &AnyRow) -> anyhow::Result<EpgSource> {
    Ok(EpgSource {
        id: r_i64(row, "id")?,
        name: r_string(row, "name")?,
        url: r_string(row, "url")?,
        last_sync_at: r_opt_ts(row, "last_sync_at")?,
        last_status: r_opt_string(row, "last_status")?,
    })
}

pub(crate) fn row_to_program(row: &AnyRow) -> anyhow::Result<EpgProgram> {
    Ok(EpgProgram {
        id: r_i64(row, "id")?,
        channel_id: r_i64(row, "channel_id")?,
        tvg_id: r_opt_string(row, "tvg_id")?,
        title: r_string(row, "title")?,
        description: r_opt_string(row, "description")?,
        category: r_opt_string(row, "category")?,
        start_at: r_ts(row, "start_at")?,
        stop_at: r_ts(row, "stop_at")?,
    })
}

const EPG_SOURCE_COLS: &str = "id, name, url, last_sync_at, last_status";
const PROGRAM_COLS: &str =
    "id, channel_id, tvg_id, title, description, category, start_at, stop_at";

impl Store {
    pub async fn create_epg_source(&self, name: &str, url: &str) -> anyhow::Result<EpgSource> {
        let now = Utc::now();
        let id = insert_id!(
            self,
            "INSERT INTO epg_source (name, url, last_sync_at, last_status, created_at) VALUES (?, ?, ?, ?, ?)",
            name,
            url,
            None::<i64>,
            None::<String>,
            epoch(now)
        )?;
        let row = sqlx::query(&ph(
            &format!("SELECT {EPG_SOURCE_COLS} FROM epg_source WHERE id = ?"),
            self.dialect,
        ))
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        row_to_epg_source(&row)
    }

    pub async fn list_epg_sources(&self) -> anyhow::Result<Vec<EpgSource>> {
        let rows = sqlx::query(&ph(
            &format!("SELECT {EPG_SOURCE_COLS} FROM epg_source ORDER BY id ASC"),
            self.dialect,
        ))
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            out.push(row_to_epg_source(r)?);
        }
        Ok(out)
    }

    /// 按 (channel_id, start_at) 去重后写入：先删同键旧节目再插新节目。
    /// 返回实际写入条数。
    pub async fn upsert_epg_programs(&self, items: Vec<EpgProgramInput>) -> anyhow::Result<u64> {
        let now = Utc::now();
        let mut n: u64 = 0;
        for item in &items {
            sqlx::query(&ph(
                "DELETE FROM epg_program WHERE channel_id = ? AND start_at = ?",
                self.dialect,
            ))
            .bind(item.channel_id)
            .bind(epoch(item.start_at))
            .execute(&self.pool)
            .await?;
            sqlx::query(
                &ph("INSERT INTO epg_program (channel_id, tvg_id, title, description, category, start_at, stop_at, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)", self.dialect),
            )
            .bind(item.channel_id)
            .bind(item.tvg_id.clone())
            .bind(&item.title)
            .bind(item.description.clone())
            .bind(item.category.clone())
            .bind(epoch(item.start_at))
            .bind(epoch(item.stop_at))
            .bind(epoch(now))
            .execute(&self.pool)
            .await?;
            n += 1;
        }
        Ok(n)
    }

    pub async fn programs_for_channel(
        &self,
        channel_id: i64,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> anyhow::Result<Vec<EpgProgram>> {
        let rows = sqlx::query(&ph(&format!(
            "SELECT {PROGRAM_COLS} FROM epg_program WHERE channel_id = ? AND start_at >= ? AND start_at < ? ORDER BY start_at ASC"
        ), self.dialect))
        .bind(channel_id)
        .bind(epoch(from))
        .bind(epoch(to))
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            out.push(row_to_program(r)?);
        }
        Ok(out)
    }

    pub async fn current_program(
        &self,
        channel_id: i64,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<EpgProgram>> {
        let row = sqlx::query(&ph(&format!(
            "SELECT {PROGRAM_COLS} FROM epg_program WHERE channel_id = ? AND start_at <= ? AND stop_at > ? ORDER BY start_at DESC LIMIT 1"
        ), self.dialect))
        .bind(channel_id)
        .bind(epoch(now))
        .bind(epoch(now))
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| row_to_program(&r)).transpose()
    }
}
