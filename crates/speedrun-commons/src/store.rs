//! The commons database: SQLite through sqlx, schema in `migrations/`.

use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, SecondsFormat, Utc};
use speedrun_core::commons::{Link, Person, PersonRecord, PublicationSummary};
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
    /// Hex public key that signed the bundle, so a person's publications can be
    /// found from the links without parsing every body.
    pub publisher_pubkey: &'a str,
    pub received_at: &'a str,
    pub judgment: &'a str,
    pub leaf_index: u64,
    pub leaf_hash: &'a str,
    pub tree_size: u64,
    pub tree_head: &'a str,
}

/// The state of a pending GitHub link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingState {
    Pending,
    Authorized,
    Refused,
}

/// A pending link as stored: the device flow's code and the person GitHub
/// named, once it has.
pub struct PendingLink {
    pub link_id: String,
    pub device_pubkey: String,
    pub device_code: String,
    pub expires_at: DateTime<Utc>,
    pub state: PendingState,
    pub person: Option<Person>,
}

/// The fields a new pending link is written with.
pub struct NewPendingLink<'a> {
    pub link_id: &'a str,
    pub device_pubkey: &'a str,
    pub device_code: &'a str,
    pub expires_at: DateTime<Utc>,
}

type PendingRow = (String, String, String, String, Option<i64>, Option<String>);
type PublicationRow = (String, String, i64, String, i64);

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
        sqlx::query(
            "INSERT INTO bundles (digest, run_id, tier, body, received_at, publisher_pubkey) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(publication.bundle_digest)
        .bind(publication.run_id)
        .bind(i64::from(publication.tier))
        .bind(publication.body)
        .bind(publication.received_at)
        .bind(publication.publisher_pubkey)
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

    /// Store a pending link before the person has authorized it.
    pub async fn insert_pending_link(&self, pending: &NewPendingLink<'_>) -> Result<()> {
        sqlx::query(
            "INSERT INTO pending_links (link_id, device_pubkey, device_code, expires_at, state) VALUES (?, ?, ?, ?, 'pending')",
        )
        .bind(pending.link_id)
        .bind(pending.device_pubkey)
        .bind(pending.device_code)
        .bind(stamp(pending.expires_at))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// A pending link by id, including the person GitHub named if it did.
    pub async fn pending_link(&self, link_id: &str) -> Result<Option<PendingLink>> {
        let row = sqlx::query_as::<_, PendingRow>(
            "SELECT device_pubkey, device_code, expires_at, state, github_id, login FROM pending_links WHERE link_id = ?",
        )
        .bind(link_id)
        .fetch_optional(&self.pool)
        .await?;
        let (device_pubkey, device_code, expires_at, state, github_id, login) = match row {
            Some(row) => row,
            None => return Ok(None),
        };
        let state = match state.as_str() {
            "pending" => PendingState::Pending,
            "authorized" => PendingState::Authorized,
            "refused" => PendingState::Refused,
            other => return Err(anyhow::anyhow!("a pending link's state {other} is unknown")),
        };
        let person = match (github_id, login) {
            (Some(github_id), Some(login)) => Some(Person {
                github_id: u64::try_from(github_id)?,
                login,
            }),
            (None, None) => None,
            _ => return Err(anyhow::anyhow!("a pending link names half a person")),
        };
        if state == PendingState::Authorized && person.is_none() {
            return Err(anyhow::anyhow!("an authorized pending link names no person"));
        }
        Ok(Some(PendingLink {
            link_id: link_id.to_string(),
            device_pubkey,
            device_code,
            expires_at: time(&expires_at, "a pending link's expiry")?,
            state,
            person,
        }))
    }

    /// Record that GitHub named `person` for this link.
    pub async fn authorize_pending_link(
        &self,
        link_id: &str,
        person: &Person,
        at: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE pending_links SET state = 'authorized', github_id = ?, login = ?, authorized_at = ? WHERE link_id = ?",
        )
        .bind(i64::try_from(person.github_id)?)
        .bind(&person.login)
        .bind(stamp(at))
        .bind(link_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record that the link can never complete.
    pub async fn refuse_pending_link(&self, link_id: &str) -> Result<()> {
        sqlx::query("UPDATE pending_links SET state = 'refused' WHERE link_id = ?")
            .bind(link_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The link already confirmed for this pending link, if any.
    pub async fn stored_link(&self, link_id: &str) -> Result<Option<Link>> {
        let body: Option<String> = sqlx::query_scalar("SELECT body FROM links WHERE link_id = ?")
            .bind(link_id)
            .fetch_optional(&self.pool)
            .await?;
        match body {
            Some(body) => {
                let link = serde_json::from_str(&body)
                    .map_err(|error| anyhow::anyhow!("a stored link is not JSON: {error}"))?;
                Ok(Some(link))
            }
            None => Ok(None),
        }
    }

    /// Store a confirmed link and make its person's login current.
    pub async fn insert_link(&self, link: &Link, body: &str, at: DateTime<Utc>) -> Result<()> {
        let github_id = i64::try_from(link.claim.github_id)?;
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO people (github_id, login) VALUES (?, ?) ON CONFLICT(github_id) DO UPDATE SET login = excluded.login",
        )
        .bind(github_id)
        .bind(&link.claim.login)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO links (link_id, github_id, login, device_pubkey, body, linked_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&link.claim.link_id)
        .bind(github_id)
        .bind(&link.claim.login)
        .bind(&link.claim.device_pubkey)
        .bind(body)
        .bind(stamp(at))
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// A person by their current login, with every link and every publication
    /// signed by one of their linked device keys.
    pub async fn person_record(&self, login: &str) -> Result<Option<PersonRecord>> {
        let person = sqlx::query_as::<_, (i64, String)>(
            "SELECT github_id, login FROM people WHERE lower(login) = lower(?) LIMIT 1",
        )
        .bind(login)
        .fetch_optional(&self.pool)
        .await?;
        let (github_id, stored_login) = match person {
            Some(person) => person,
            None => return Ok(None),
        };
        let link_bodies: Vec<String> =
            sqlx::query_scalar("SELECT body FROM links WHERE github_id = ? ORDER BY rowid")
                .bind(github_id)
                .fetch_all(&self.pool)
                .await?;
        let mut links: Vec<Link> = Vec::with_capacity(link_bodies.len());
        for body in link_bodies {
            links.push(
                serde_json::from_str(&body)
                    .map_err(|error| anyhow::anyhow!("a stored link is not JSON: {error}"))?,
            );
        }
        let rows = sqlx::query_as::<_, PublicationRow>(
            "SELECT bundles.run_id, bundles.digest, bundles.tier, bundles.received_at, log_leaves.leaf_index FROM bundles JOIN log_leaves ON log_leaves.bundle_digest = bundles.digest WHERE bundles.publisher_pubkey IN (SELECT device_pubkey FROM links WHERE github_id = ?) ORDER BY bundles.received_at DESC, bundles.rowid DESC",
        )
        .bind(github_id)
        .fetch_all(&self.pool)
        .await?;
        let mut publications: Vec<PublicationSummary> = Vec::with_capacity(rows.len());
        for (run_id, bundle_digest, tier, received_at, leaf_index) in rows {
            publications.push(PublicationSummary {
                run_id,
                bundle_digest,
                tier: u8::try_from(tier)?,
                received_at: time(&received_at, "a stored publication's time")?,
                leaf_index: u64::try_from(leaf_index)?,
            });
        }
        Ok(Some(PersonRecord {
            person: Person {
                github_id: u64::try_from(github_id)?,
                login: stored_login,
            },
            links,
            publications,
        }))
    }
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn time(text: &str, what: &str) -> Result<DateTime<Utc>> {
    let parsed = DateTime::parse_from_rfc3339(text)
        .map_err(|error| anyhow::anyhow!("{what} is not RFC 3339: {error}"))?;
    Ok(parsed.with_timezone(&Utc))
}
