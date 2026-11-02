//! The commons database: SQLite through sqlx, schema in `migrations/`.

use std::path::Path;

use anyhow::Result;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

pub struct Store {
    pub pool: SqlitePool,
}

impl Store {
    /// Open (creating if absent) and migrate the database at `path`.
    pub async fn open(path: &Path) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    /// Leaves in the log.
    pub async fn tree_size(&self) -> Result<u64> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM log_leaves")
            .fetch_one(&self.pool)
            .await?;
        Ok(u64::try_from(count)?)
    }
}
