//! 存储层（开发文档 §1.1、§9、§10）。
//!
//! 实现方式：`sqlx::AnyPool`，同一套 SQL 跑 PostgreSQL / MySQL / SQLite，
//! 只改 `DATABASE_URL`，业务代码不改（§10.4）。
//!
//! # 可移植性约定（§10.2 的坑位，写代码前先读）
//! - 时间一律存 epoch 秒 `BIGINT`（docs/CONTRACTS.md）：`sqlx::Any` 只支持
//!   `bool/i16/i32/i64/f32/f64/String/Vec<u8>`，没有 `DateTime`/`Json` 类型，
//!   因此文档 §9 的 `TIMESTAMPTZ` / `DATETIME(6)` 在迁移文件里映射为 `BIGINT`
//!   epoch 秒，`JSONB` / `JSON` 映射为 `TEXT`（JSON 字符串），`INET` 映射为
//!   `TEXT`。主键与索引仍按方言区分（PG `BIGSERIAL` 与部分索引、MySQL
//!   `BIGINT AUTO_INCREMENT`、SQLite `INTEGER PRIMARY KEY AUTOINCREMENT`）。
//! - 占位符：SQL 文本里一律写 `?`，执行前由 `ph()` 按方言改写
//!   （PostgreSQL 只认 `$1..$n`；sqlx::Any **不会**替我们改写，原样发
//!   过去 PG 报 `syntax error at or near ","`，2026-11-25 CI 契约测试
//!   实测）。`sqlx::query` / `query_scalar` 调用点一律包 `ph()`；
//!   `QueryBuilder::push_bind` 同样写 `?`，故列表查询不用它，见 `DynSql`
//!   模式（手拼字符串 + `ph()` + 按顺序 `.bind()`）。
//! - 布尔存 `INTEGER` 0/1，避免三驱动的 bool 编解码差异。
//! - 新增行取 id：PG 用 `INSERT ... RETURNING id`，MySQL 用
//!   `SELECT LAST_INSERT_ID()`，SQLite 用 `SELECT last_insert_rowid()`
//!   （§10.2 坑 #5），见 `insert_id!` 宏。
//! - `system_setting` 表的列名用 `setting_key` / `setting_value`（`key` 在
//!   MySQL 是保留字，同一套 SQL 不能在三方言下都裸写 `key`）。
// 作者: Passwd Word

use chrono::{DateTime, Utc};
use iptv_dto::*;
use sqlx::any::AnyRow;
use sqlx::{AnyPool, Row};
use std::collections::HashMap;

mod admin;
mod channels;
mod epg;
mod plans;
mod playback;
mod portal;
mod settings;
mod sources;
mod tokens;
mod users;

/// 数据库方言（由 `DATABASE_URL` 的 scheme 判定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Postgres,
    MySql,
    Sqlite,
}

impl std::fmt::Display for Dialect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Dialect::Postgres => "postgres",
            Dialect::MySql => "mysql",
            Dialect::Sqlite => "sqlite",
        })
    }
}

/// 存储入口。`Arc<Store>` 由 `iptv-api::AppState` 持有。
pub struct Store {
    pool: AnyPool,
    dialect: Dialect,
    /// 最近一次 `upsert_channels` 的差异缓存，供 `get_sync_diff` 重建
    /// （CONTRACTS.md：“由 upsert_channels 的结果重建”；进程内缓存，
    /// 重启后为空，见 `get_sync_diff`）。
    last_diffs: std::sync::Mutex<HashMap<i64, SyncDiff>>,
    /// 系统设置缓存（M1）：整表一次读出，60 秒 TTL；`set_setting` 清空。
    /// 跨进程（API 与 Worker 各自）最多 stale 60 秒，设置类读操作可接受。
    settings_cache:
        std::sync::RwLock<Option<(std::time::Instant, HashMap<String, serde_json::Value>)>>,
}

impl Store {
    /// 按 URL scheme 判定方言并建池。
    /// 支持 `postgres://` / `postgresql://`、`mysql://` / `mariadb://`、`sqlite://`。
    pub async fn connect(database_url: &str) -> anyhow::Result<Self> {
        // sqlx::Any 的驱动必须先注册（否则 "No drivers installed"），
        // 该函数内部用 Once 保证只注册一次，多次调用安全。
        sqlx::any::install_default_drivers();
        let lower = database_url.trim().to_ascii_lowercase();
        let dialect = if lower.starts_with("postgres") {
            Dialect::Postgres
        } else if lower.starts_with("mysql") || lower.starts_with("mariadb") {
            Dialect::MySql
        } else if lower.starts_with("sqlite") {
            Dialect::Sqlite
        } else {
            anyhow::bail!(
                "不支持的 DATABASE_URL（仅支持 postgres://、mysql://、sqlite://）：{database_url}"
            );
        };
        // SQLite 首次启动要建库文件：sqlx 默认 create_if_missing=false，
        // 不带 mode 参数的 sqlite URL 一律补 ?mode=rwc（显式带 mode 的不动）。
        let url_owned;
        let url = if lower.starts_with("sqlite") && !database_url.contains("mode=") {
            url_owned = if database_url.contains('?') {
                format!("{database_url}&mode=rwc")
            } else {
                format!("{database_url}?mode=rwc")
            };
            url_owned.as_str()
        } else {
            database_url
        };
        let pool = sqlx::any::AnyPoolOptions::new()
            .max_connections(5)
            .connect(url)
            .await?;
        Ok(Self {
            pool,
            dialect,
            last_diffs: std::sync::Mutex::new(HashMap::new()),
            settings_cache: std::sync::RwLock::new(None),
        })
    }

    /// 按方言依次执行内嵌的 V1–V4 迁移（`migrations/{dialect}/`）。
    /// 幂等：V1 用 `CREATE TABLE IF NOT EXISTS` + `CREATE INDEX IF NOT EXISTS`
    /// （MySQL 不支持索引的 `IF NOT EXISTS` 语法，见 `exec_script` 的
    /// 重复索引容忍），V2 种子用各方言的忽略冲突写法，重复执行不会产生
    /// 重复数据。API 与 Worker 启动都调本函数，先后启动必须都成功。
    pub async fn migrate(&self) -> anyhow::Result<()> {
        let files: &[&str] = match self.dialect {
            Dialect::Postgres => &[
                include_str!("../../../migrations/postgres/V1__init.sql"),
                include_str!("../../../migrations/postgres/V2__seed.sql"),
                include_str!("../../../migrations/postgres/V3__i18n_tables.sql"),
                include_str!("../../../migrations/postgres/V4__drop_max_upload_mb.sql"),
            ],
            Dialect::MySql => &[
                include_str!("../../../migrations/mysql/V1__init.sql"),
                include_str!("../../../migrations/mysql/V2__seed.sql"),
                include_str!("../../../migrations/mysql/V3__i18n_tables.sql"),
                include_str!("../../../migrations/mysql/V4__drop_max_upload_mb.sql"),
            ],
            Dialect::Sqlite => &[
                include_str!("../../../migrations/sqlite/V1__init.sql"),
                include_str!("../../../migrations/sqlite/V2__seed.sql"),
                include_str!("../../../migrations/sqlite/V3__i18n_tables.sql"),
                include_str!("../../../migrations/sqlite/V4__drop_max_upload_mb.sql"),
            ],
        };
        if self.dialect == Dialect::Sqlite {
            sqlx::query("PRAGMA foreign_keys = ON")
                .execute(&self.pool)
                .await?;
        }
        for sql in files {
            self.exec_script(sql).await?;
        }
        Ok(())
    }

    /// 逐条执行迁移脚本（按 `;` 切分；脚本内不得在字符串字面量里写 `;`）。
    ///
    /// 幂等容忍：MySQL 没有 `CREATE INDEX IF NOT EXISTS` 语法（MariaDB
    /// 才有），同一条 `CREATE [UNIQUE] INDEX` 第二次执行会报
    /// `Duplicate key name`；SQLite/PG 虽已用 `IF NOT EXISTS`，但索引
    /// 若已存在且定义不同也会报错。因此仅对 `CREATE INDEX` /
    /// `CREATE UNIQUE INDEX` 语句，把“已存在”类错误当作已应用跳过，
    /// 其他语句的任何错误仍原样返回。
    async fn exec_script(&self, sql: &str) -> anyhow::Result<()> {
        for chunk in sql.split(';') {
            let cleaned: String = chunk
                .lines()
                .filter(|l| !l.trim_start().starts_with("--"))
                .collect::<Vec<_>>()
                .join("\n");
            let stmt = cleaned.trim();
            if stmt.is_empty() {
                continue;
            }
            if let Err(e) = sqlx::query(stmt).execute(&self.pool).await {
                let upper = stmt.trim_start().to_ascii_uppercase();
                let is_create_index =
                    upper.starts_with("CREATE INDEX") || upper.starts_with("CREATE UNIQUE INDEX");
                let msg = e.to_string().to_ascii_lowercase();
                let duplicate = msg.contains("already exists")
                    || msg.contains("duplicate key name")
                    || msg.contains("duplicate key");
                if is_create_index && duplicate {
                    continue;
                }
                return Err(e.into());
            }
        }
        Ok(())
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    pub fn pool(&self) -> &AnyPool {
        &self.pool
    }

    /// 非 PG 方言取 id：必须在**同一连接**上执行 INSERT 与取 id
    /// （`last_insert_rowid()` / `LAST_INSERT_ID()` 均为连接级函数，
    /// 换连接会取错 id）。`insert_id!` 宏内用 `pool.acquire()` 保证同连接。
    async fn last_insert_id_on(
        conn: &mut sqlx::pool::PoolConnection<sqlx::Any>,
        dialect: Dialect,
    ) -> anyhow::Result<i64> {
        match dialect {
            Dialect::Postgres => anyhow::bail!("Postgres 插入必须走 RETURNING id"),
            Dialect::MySql => Ok(sqlx::query_scalar("SELECT LAST_INSERT_ID()")
                .fetch_one(&mut **conn)
                .await?),
            Dialect::Sqlite => Ok(sqlx::query_scalar("SELECT last_insert_rowid()")
                .fetch_one(&mut **conn)
                .await?),
        }
    }
}

/// 把 SQL 文本里的 `?` 占位符按方言改写：PostgreSQL 要 `$1..$n`，
/// MySQL/SQLite 保持 `?`。
///
/// sqlx 0.8 的 `Any` 驱动不改写占位符（源码核实：`AnyConnection` 把 SQL
/// 原样交给后端驱动，`AnyArguments` 也没有改写 `format_placeholder` 的
/// 实现），所以带 `?` 的 SQL 发到 PG 会报 `syntax error at or near ","`。
/// 改写时跳过单引号字符串字面量里的 `?`（如 `'[]'`、`'active'`）；
/// SQL 文本里不得在字符串字面量外写裸 `?`（JSON `?` 运算符等三方言
/// 也不通用，应用层不要用）。
pub(crate) fn ph(sql: &str, dialect: Dialect) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if dialect != Dialect::Postgres || !sql.contains('?') {
        return Cow::Borrowed(sql);
    }
    let mut out = String::with_capacity(sql.len() + 8);
    let mut n = 0usize;
    let mut chars = sql.chars();
    while let Some(c) = chars.next() {
        if c == '\'' {
            // 单引号字符串：原样抄到闭合引号，'' 是转义的单引号。
            out.push(c);
            while let Some(c2) = chars.next() {
                out.push(c2);
                if c2 == '\'' {
                    if chars.clone().next() == Some('\'') {
                        out.push(chars.next().unwrap());
                    } else {
                        break;
                    }
                }
            }
        } else if c == '?' {
            n += 1;
            out.push('$');
            out.push_str(&n.to_string());
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// 往动态 SQL 里追加 `... IN (?, ?, ...)` 片段（n 个占位符）。
pub(crate) fn push_in_list(sql: &mut String, prefix: &str, n: usize) {
    sql.push_str(prefix);
    for i in 0..n {
        if i > 0 {
            sql.push_str(", ");
        }
        sql.push('?');
    }
    sql.push(')');
}

/// 新增行取 id（§10.2 坑 #5）。
/// PG 用 `INSERT ... RETURNING id`；MySQL/SQLite 在同一连接上执行
/// INSERT 后取 `LAST_INSERT_ID()` / `last_insert_rowid()`（连接级函数，
/// 换连接会取错 id，因此这里用 `pool.acquire()` 显式拿同一连接）。
macro_rules! insert_id {
    ($store:expr, $sql:literal, $($arg:expr),* $(,)?) => {{
        match $store.dialect {
            Dialect::Postgres => {
                sqlx::query_scalar::<_, i64>(&ph(
                    concat!($sql, " RETURNING id"),
                    $store.dialect,
                ))
                    $(.bind($arg))*
                    .fetch_one(&$store.pool)
                    .await
                    .map_err(anyhow::Error::from)
            }
            _ => {
                let mut conn = $store.pool.acquire().await?;
                sqlx::query($sql)
                    $(.bind($arg))*
                    .execute(&mut *conn)
                    .await?;
                Self::last_insert_id_on(&mut conn, $store.dialect).await
            }
        }
    }};
}
pub(crate) use insert_id;

// ---------------------------------------------------------------------------
// 行读取 / 时间 / JSON 辅助
// ---------------------------------------------------------------------------

pub(crate) fn epoch(dt: DateTime<Utc>) -> i64 {
    dt.timestamp()
}

pub(crate) fn from_epoch(secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(secs, 0).unwrap_or(DateTime::UNIX_EPOCH)
}

pub(crate) fn opt_epoch(v: Option<i64>) -> Option<DateTime<Utc>> {
    v.map(from_epoch)
}

pub(crate) fn r_i64(row: &AnyRow, col: &str) -> anyhow::Result<i64> {
    Ok(row.try_get(col)?)
}

pub(crate) fn r_opt_i64(row: &AnyRow, col: &str) -> anyhow::Result<Option<i64>> {
    Ok(row.try_get(col)?)
}

pub(crate) fn r_string(row: &AnyRow, col: &str) -> anyhow::Result<String> {
    if let Ok(v) = row.try_get::<String, _>(col) {
        return Ok(v);
    }
    // MySQL 的 TEXT 列在 sqlx 下按 BLOB 上报，String 解码报
    // "Rust type String is not compatible with SQL type BLOB"；
    // 兜底按字节读再转 UTF-8（非法字节有损替换）。
    let bytes = row.try_get::<Vec<u8>, _>(col)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub(crate) fn r_opt_string(row: &AnyRow, col: &str) -> anyhow::Result<Option<String>> {
    if let Ok(v) = row.try_get::<Option<String>, _>(col) {
        return Ok(v);
    }
    let bytes = row.try_get::<Option<Vec<u8>>, _>(col)?;
    Ok(bytes.map(|b| String::from_utf8_lossy(&b).into_owned()))
}

pub(crate) fn r_bool(row: &AnyRow, col: &str) -> anyhow::Result<bool> {
    Ok(row.try_get::<i64, _>(col)? != 0)
}

pub(crate) fn r_opt_bool(row: &AnyRow, col: &str) -> anyhow::Result<Option<bool>> {
    Ok(row.try_get::<Option<i64>, _>(col)?.map(|v| v != 0))
}

pub(crate) fn r_ts(row: &AnyRow, col: &str) -> anyhow::Result<DateTime<Utc>> {
    Ok(from_epoch(row.try_get::<i64, _>(col)?))
}

pub(crate) fn r_opt_ts(row: &AnyRow, col: &str) -> anyhow::Result<Option<DateTime<Utc>>> {
    Ok(opt_epoch(row.try_get::<Option<i64>, _>(col)?))
}

pub(crate) fn r_json(row: &AnyRow, col: &str) -> serde_json::Value {
    // 与 r_string 同理：MySQL TEXT 列按 BLOB 上报，String 解码失败时
    // 兜底按字节读（否则这里会静默返回 Null，把设置/审计 JSON 读丢）。
    let text: Option<String> = match row.try_get::<String, _>(col) {
        Ok(s) => Some(s),
        Err(_) => row
            .try_get::<Vec<u8>, _>(col)
            .ok()
            .and_then(|b| String::from_utf8(b).ok()),
    };
    text.and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null)
}

pub(crate) fn r_opt_json(row: &AnyRow, col: &str) -> Option<serde_json::Value> {
    let text: Option<String> = match row.try_get::<Option<String>, _>(col) {
        Ok(v) => v,
        Err(_) => row
            .try_get::<Option<Vec<u8>>, _>(col)
            .ok()
            .flatten()
            .and_then(|b| String::from_utf8(b).ok()),
    };
    text.and_then(|s| serde_json::from_str(&s).ok())
}

pub(crate) fn json_text(v: &serde_json::Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".to_string())
}

pub(crate) fn str_list(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn i64_list(v: &serde_json::Value) -> Vec<i64> {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
        .unwrap_or_default()
}

pub(crate) fn page_bounds(q: &PageQuery) -> (i64, i64) {
    let size = i64::from(q.page_size.clamp(1, 100));
    let page = i64::from(q.page.max(1));
    (size, (page - 1) * size)
}

pub(crate) fn like_pattern(q: &Option<String>) -> Option<String> {
    q.as_ref()
        .map(|s| format!("%{}%", s.replace('%', "\\%").replace('_', "\\_")))
}

// ---------------------------------------------------------------------------
// CONTRACTS.md 定义的跨方法结构
// ---------------------------------------------------------------------------

/// `upsert_channels` 的同步结果（§7.3 四态）。
#[derive(Debug, Clone, Default)]
pub struct SyncOutcome {
    pub added: i64,
    pub updated: i64,
    pub removed: i64,
    pub added_channels: Vec<Channel>,
    pub metadata_changed: Vec<Channel>,
    pub url_changed: Vec<Channel>,
    pub missing: Vec<Channel>,
}

#[derive(Debug, Clone)]
pub struct NewAuditLog {
    pub actor: String,
    pub actor_role: Option<String>,
    pub action: String,
    pub object_type: String,
    pub object_id: String,
    pub before_json: Option<serde_json::Value>,
    pub after_json: Option<serde_json::Value>,
    pub ip_address: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewPlaybackEvent {
    pub session_uid: String,
    pub token_id: i64,
    pub channel_id: i64,
    pub event_type: String,
    pub detail: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct EpgProgramInput {
    pub channel_id: i64,
    pub tvg_id: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub category: Option<String>,
    pub start_at: DateTime<Utc>,
    pub stop_at: DateTime<Utc>,
}

#[cfg(test)]
mod ph_tests {
    use super::*;

    #[test]
    fn ph_rewrites_postgres_placeholders() {
        assert_eq!(
            ph("SELECT * FROM t WHERE a = ? AND b = ?", Dialect::Postgres).as_ref(),
            "SELECT * FROM t WHERE a = $1 AND b = $2"
        );
        // 字符串字面量里的 ? 不动。
        assert_eq!(
            ph("SELECT * FROM t WHERE a = '?' AND b = ?", Dialect::Postgres).as_ref(),
            "SELECT * FROM t WHERE a = '?' AND b = $1"
        );
        // '' 转义的单引号不提前闭合字符串。
        assert_eq!(
            ph(
                "SELECT * FROM t WHERE a = 'it''s ?' AND b = ?",
                Dialect::Postgres
            )
            .as_ref(),
            "SELECT * FROM t WHERE a = 'it''s ?' AND b = $1"
        );
        // MySQL / SQLite 保持原样。
        assert_eq!(
            ph("SELECT * FROM t WHERE a = ?", Dialect::MySql).as_ref(),
            "SELECT * FROM t WHERE a = ?"
        );
        assert_eq!(
            ph("SELECT * FROM t WHERE a = ?", Dialect::Sqlite).as_ref(),
            "SELECT * FROM t WHERE a = ?"
        );
        // 无占位符时原样返回。
        assert_eq!(ph("SELECT 1", Dialect::Postgres).as_ref(), "SELECT 1");
    }
}
