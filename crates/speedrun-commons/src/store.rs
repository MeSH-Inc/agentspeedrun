//! The commons database: SQLite through sqlx, schema in `migrations/`.

use std::path::Path;

use anyhow::Result;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

pub struct Store {
    pub pool: SqlitePool,
}

/// A bundle stored with its judgment and its leaf in the log.
pub struct StoredPublication {
    pub digest: String,
    pub body: String,
    pub tier: u8,
    pub judgment: String,
    pub leaf_index: u64,
}

/// Everything one append writes, gathered before the transaction begins.
pub struct Publication<'a> {
    pub bundle_digest: &'a str,
    pub run_id: &'a str,
    pub tier: u8,
    pub body: &'a str,
    pub received_at: &'a str,
    pub judgment: &'a str,
    pub leaf_index: u64,
    pub leaf_hash: &'a str,
    pub tree_size: u64,
    pub tree_head: &'a str,
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

    /// Every leaf hash, in log order.
    pub async fn leaf_hashes(&self) -> Result<Vec<String>> {
        let leaves: Vec<String> = sqlx::query_scalar("SELECT leaf_hash FROM log_leaves ORDER BY leaf_index")
            .fetch_all(&self.pool)
            .await?;
        Ok(leaves)
    }

    /// The most recently issued tree head, if the log has one.
    pub async fn latest_tree_head(&self) -> Result<Option<String>> {
        let body: Option<String> =
            sqlx::query_scalar("SELECT body FROM tree_heads ORDER BY tree_size DESC LIMIT 1")
                .fetch_optional(&self.pool)
                .await?;
        Ok(body)
    }

    /// A bundle with its judgment and leaf, by bundle digest.
    pub async fn publication(&self, digest: &str) -> Result<Option<StoredPublication>> {
        let row = sqlx::query_as::<_, (String, i64, String, i64)>(
            "SELECT bundles.body, bundles.tier, judgments.body, log_leaves.leaf_index \
             FROM bundles \
             JOIN judgments ON judgments.bundle_digest = bundles.digest \
             JOIN log_leaves ON log_leaves.bundle_digest = bundles.digest \
             WHERE bundles.digest = ?",
        )
        .bind(digest)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some((body, tier, judgment, leaf_index)) => Ok(Some(StoredPublication {
                digest: digest.to_string(),
                body,
                tier: u8::try_from(tier)?,
                judgment,
                leaf_index: u64::try_from(leaf_index)?,
            })),
            None => Ok(None),
        }
    }

    /// The run's latest publication, by arrival order.
    pub async fn latest_publication_for_run(&self, run_id: &str) -> Result<Option<StoredPublication>> {
        let digest: Option<String> = sqlx::query_scalar(
            "SELECT digest FROM bundles WHERE run_id = ? ORDER BY received_at DESC, rowid DESC LIMIT 1",
        )
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await?;
        match digest {
            Some(digest) => self.publication(&digest).await,
            None => Ok(None),
        }
    }

    /// Write one publication and its log entry in one transaction.
    pub async fn append(&self, publication: &Publication<'_>) -> Result<()> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("INSERT INTO bundles (digest, run_id, tier, body, received_at) VALUES (?, ?, ?, ?, ?)")
            .bind(publication.bundle_digest)
            .bind(publication.run_id)
            .bind(i64::from(publication.tier))
            .bind(publication.body)
            .bind(publication.received_at)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("INSERT INTO judgments (bundle_digest, body) VALUES (?, ?)")
            .bind(publication.bundle_digest)
            .bind(publication.judgment)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("INSERT INTO log_leaves (leaf_index, bundle_digest, leaf_hash) VALUES (?, ?, ?)")
            .bind(i64::try_from(publication.leaf_index)?)
            .bind(publication.bundle_digest)
            .bind(publication.leaf_hash)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("INSERT INTO tree_heads (tree_size, body) VALUES (?, ?)")
            .bind(i64::try_from(publication.tree_size)?)
            .bind(publication.tree_head)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }
}
