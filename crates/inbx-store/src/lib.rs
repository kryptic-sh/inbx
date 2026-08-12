use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{FromRow, SqlitePool};

mod threading;
pub use threading::normalize_subject;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("thread root not found for {0}")]
    ThreadRoot(String),
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("migrate: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("config: {0}")]
    Config(#[from] inbx_config::Error),
    #[error("invalid snapshot: {0}")]
    InvalidSnapshot(&'static str),
    #[error("snapshot generation {generation} was not reserved for folder {folder}")]
    UnreservedSnapshotGeneration { folder: String, generation: i64 },
    #[error("folder not found: {0}")]
    FolderNotFound(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, FromRow)]
pub struct FolderRow {
    pub name: String,
    pub delim: Option<String>,
    pub special_use: Option<String>,
    pub attrs: Option<String>,
    pub uidvalidity: Option<i64>,
    pub uidnext: Option<i64>,
    #[sqlx(default)]
    pub delta_link: Option<String>,
    pub last_sync_unix: Option<i64>,
}

#[derive(Debug, Clone, FromRow)]
pub struct OutboxRow {
    pub id: i64,
    pub enqueued_unix: i64,
    pub raw: Vec<u8>,
    pub attempts: i64,
    pub next_retry_unix: i64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct MessageRow {
    pub folder: String,
    pub uid: i64,
    pub uidvalidity: i64,
    pub message_id: Option<String>,
    pub subject: Option<String>,
    pub from_addr: Option<String>,
    pub to_addrs: Option<String>,
    pub date_unix: Option<i64>,
    pub flags: String,
    pub maildir_path: Option<String>,
    pub headers_only: i64,
    pub fetched_at_unix: i64,
    #[sqlx(default)]
    pub in_reply_to: Option<String>,
    #[sqlx(default)]
    pub refs: Option<String>,
    #[sqlx(default)]
    pub thread_id: Option<String>,
    #[sqlx(default)]
    pub provider_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotTransport {
    Imap { uidvalidity: i64 },
    Opaque,
}

impl SnapshotTransport {
    pub fn uidvalidity(self) -> i64 {
        match self {
            Self::Imap { uidvalidity } => uidvalidity,
            Self::Opaque => 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotHeader {
    pub uid: i64,
    pub uidvalidity: i64,
    pub message_id: Option<String>,
    pub subject: Option<String>,
    pub from_addr: Option<String>,
    pub to_addrs: Option<String>,
    pub date_unix: Option<i64>,
    pub flags: String,
    pub fetched_at_unix: i64,
    pub provider_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SnapshotInput {
    pub folder: String,
    pub generation: i64,
    pub complete: bool,
    pub transport: SnapshotTransport,
    pub rows: Vec<SnapshotHeader>,
}

#[derive(Debug, Clone)]
pub struct SnapshotOutput {
    pub applied: bool,
    pub new_rows: Vec<SnapshotHeader>,
}

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
    root: PathBuf,
}

static MAILDIR_COUNTER: AtomicU64 = AtomicU64::new(0);

impl Store {
    pub async fn open(account: &str) -> Result<Self> {
        let root = inbx_config::data_dir()?.join(account);
        std::fs::create_dir_all(&root)?;
        let db_path = root.join("index.sqlite");
        let opts = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool, root })
    }

    /// Construct a Store from a pre-existing pool (used in tests).
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self {
            pool,
            root: PathBuf::new(),
        }
    }

    /// Expose the pool (used in tests).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Reserve a database-issued generation before starting a folder fetch.
    /// A later reservation makes an older fetched snapshot ineligible to apply.
    pub async fn reserve_snapshot_generation(&self, folder: &str) -> Result<i64> {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO folders (name, snapshot_generation, latest_reserved_generation)
             VALUES (?1, 0, 1)
             ON CONFLICT(name) DO UPDATE SET
               latest_reserved_generation = folders.latest_reserved_generation + 1
             RETURNING latest_reserved_generation",
        )
        .bind(folder)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Apply a validated provider snapshot in one SQLite transaction.
    pub async fn apply_snapshot(&self, input: SnapshotInput) -> Result<SnapshotOutput> {
        use std::collections::{HashMap, HashSet};

        let mut provider_ids = HashSet::new();
        let mut message_id_counts = HashMap::new();
        let mut uids = HashSet::new();
        if input.transport == SnapshotTransport::Opaque {
            for row in &input.rows {
                let Some(provider_id) = row.provider_id.as_ref() else {
                    return Err(Error::InvalidSnapshot(
                        "opaque snapshot is missing a provider id",
                    ));
                };
                if !provider_ids.insert(provider_id) {
                    return Err(Error::InvalidSnapshot(
                        "opaque snapshot has duplicate provider ids",
                    ));
                }
                if !uids.insert(row.uid) {
                    return Err(Error::InvalidSnapshot(
                        "opaque snapshot has duplicate canonical uids",
                    ));
                }
                if let Some(message_id) = &row.message_id {
                    *message_id_counts
                        .entry(message_id.as_str())
                        .or_insert(0usize) += 1;
                }
            }
        }

        let mut tx = self.pool.begin().await?;
        let reserved: Option<(i64, i64)> = sqlx::query_as(
            "SELECT snapshot_generation, latest_reserved_generation FROM folders WHERE name = ?1",
        )
        .bind(&input.folder)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((snapshot_generation, latest_reserved_generation)) = reserved else {
            tx.rollback().await?;
            return Err(Error::UnreservedSnapshotGeneration {
                folder: input.folder.clone(),
                generation: input.generation,
            });
        };
        if input.generation <= 0 {
            tx.rollback().await?;
            return Err(Error::UnreservedSnapshotGeneration {
                folder: input.folder.clone(),
                generation: input.generation,
            });
        }
        if input.generation < latest_reserved_generation || input.generation <= snapshot_generation
        {
            tx.rollback().await?;
            return Ok(SnapshotOutput {
                applied: false,
                new_rows: Vec::new(),
            });
        }
        if input.generation > latest_reserved_generation {
            tx.rollback().await?;
            return Err(Error::UnreservedSnapshotGeneration {
                folder: input.folder.clone(),
                generation: input.generation,
            });
        }

        let previous_uidvalidity: Option<(Option<i64>,)> =
            sqlx::query_as("SELECT uidvalidity FROM folders WHERE name = ?1")
                .bind(&input.folder)
                .fetch_optional(&mut *tx)
                .await?;
        let uidvalidity = input.transport.uidvalidity();
        let old_provider_ids: HashSet<String> = sqlx::query_scalar(
            "SELECT provider_id FROM messages WHERE folder = ?1 AND provider_id IS NOT NULL",
        )
        .bind(&input.folder)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect();
        let old_max: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(uid), 0) FROM messages WHERE folder = ?1 AND uidvalidity = ?2",
        )
        .bind(&input.folder)
        .bind(uidvalidity)
        .fetch_one(&mut *tx)
        .await?;
        let mut reconciled_provider_ids: HashSet<String> = HashSet::new();

        if matches!(input.transport, SnapshotTransport::Imap { .. })
            && previous_uidvalidity
                .and_then(|(uidvalidity,)| uidvalidity)
                .is_some_and(|old| old != uidvalidity)
        {
            sqlx::query("DELETE FROM messages WHERE folder = ?1")
                .bind(&input.folder)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query(
            "INSERT INTO folders (name, uidvalidity, snapshot_generation, latest_reserved_generation)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(name) DO UPDATE SET
                uidvalidity = excluded.uidvalidity,
                snapshot_generation = excluded.snapshot_generation",
        ).bind(&input.folder).bind(uidvalidity).bind(input.generation).execute(&mut *tx).await?;

        if input.transport == SnapshotTransport::Opaque {
            for (index, row) in input.rows.iter().enumerate() {
                let provider_id = row.provider_id.as_deref().expect("validated above");
                // A legacy null-provider row is only rekeyed when one stable identity agrees.
                let low_uid = (row.uid as u64 & u32::MAX as u64) as i64;
                let xor_uid = row.uid ^ index as i64;
                let candidates: Vec<(i64, i64)> = if row
                    .message_id
                    .as_deref()
                    .is_some_and(|message_id| message_id_counts[message_id] == 1)
                {
                    sqlx::query_as(
                        "SELECT uid, uidvalidity FROM messages WHERE folder = ?1 AND provider_id IS NULL
                         AND (message_id = ?2 OR uid = ?3 OR uid = ?4 OR uid = ?5)",
                    )
                    .bind(&input.folder)
                    .bind(&row.message_id)
                    .bind(row.uid)
                    .bind(low_uid)
                    .bind(xor_uid)
                    .fetch_all(&mut *tx)
                    .await?
                } else {
                    sqlx::query_as(
                        "SELECT uid, uidvalidity FROM messages WHERE folder = ?1 AND provider_id IS NULL
                         AND (uid = ?2 OR uid = ?3 OR uid = ?4)",
                    )
                    .bind(&input.folder)
                    .bind(row.uid)
                    .bind(low_uid)
                    .bind(xor_uid)
                    .fetch_all(&mut *tx)
                    .await?
                };
                if candidates.len() == 1 {
                    let (old_uid, old_uv) = candidates[0];
                    sqlx::query("DELETE FROM messages WHERE folder = ?1 AND provider_id = ?2")
                        .bind(&input.folder)
                        .bind(provider_id)
                        .execute(&mut *tx)
                        .await?;
                    sqlx::query("UPDATE messages SET uid = ?3, uidvalidity = ?4, provider_id = ?5 WHERE folder = ?1 AND uid = ?2 AND uidvalidity = ?6")
                        .bind(&input.folder).bind(old_uid).bind(row.uid).bind(row.uidvalidity).bind(provider_id).bind(old_uv).execute(&mut *tx).await?;
                    reconciled_provider_ids.insert(provider_id.to_owned());
                } else if candidates.len() > 1 {
                    tracing::warn!(folder = %input.folder, provider_id, candidates = candidates.len(), "ambiguous legacy opaque rows left unresolved");
                }
                let keeper: Option<(i64, i64)> = sqlx::query_as(
                    "SELECT uid, uidvalidity FROM messages WHERE folder = ?1 AND provider_id = ?2
                     ORDER BY (maildir_path IS NOT NULL) DESC, headers_only ASC, fetched_at_unix DESC, uid DESC LIMIT 1",
                ).bind(&input.folder).bind(provider_id).fetch_optional(&mut *tx).await?;
                if let Some((old_uid, old_uv)) = keeper {
                    sqlx::query("DELETE FROM messages WHERE folder = ?1 AND provider_id = ?2 AND NOT (uid = ?3 AND uidvalidity = ?4)")
                        .bind(&input.folder).bind(provider_id).bind(old_uid).bind(old_uv).execute(&mut *tx).await?;
                    if old_uid != row.uid || old_uv != row.uidvalidity {
                        sqlx::query("UPDATE messages SET uid = ?3, uidvalidity = ?4 WHERE folder = ?1 AND uid = ?2 AND uidvalidity = ?5")
                            .bind(&input.folder).bind(old_uid).bind(row.uid).bind(row.uidvalidity).bind(old_uv).execute(&mut *tx).await?;
                    }
                }
            }
        }
        for row in &input.rows {
            sqlx::query("INSERT INTO messages (folder, uid, uidvalidity, message_id, subject, from_addr, to_addrs, date_unix, flags, maildir_path, headers_only, fetched_at_unix, provider_id)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, 1, ?10, ?11)
                         ON CONFLICT(folder, uid, uidvalidity) DO UPDATE SET message_id=excluded.message_id, subject=excluded.subject, from_addr=excluded.from_addr, to_addrs=excluded.to_addrs, date_unix=excluded.date_unix, flags=excluded.flags, headers_only=MIN(excluded.headers_only,messages.headers_only), fetched_at_unix=excluded.fetched_at_unix, provider_id=COALESCE(excluded.provider_id,messages.provider_id)")
                .bind(&input.folder).bind(row.uid).bind(row.uidvalidity).bind(&row.message_id).bind(&row.subject).bind(&row.from_addr).bind(&row.to_addrs).bind(row.date_unix).bind(&row.flags).bind(row.fetched_at_unix).bind(&row.provider_id).execute(&mut *tx).await?;
        }
        if input.complete {
            match input.transport {
                SnapshotTransport::Imap { .. } => {
                    let uids: HashSet<i64> = input.rows.iter().map(|row| row.uid).collect();
                    let existing: Vec<i64> = sqlx::query_scalar(
                        "SELECT uid FROM messages WHERE folder = ?1 AND uidvalidity = ?2",
                    )
                    .bind(&input.folder)
                    .bind(uidvalidity)
                    .fetch_all(&mut *tx)
                    .await?;
                    for uid in existing.into_iter().filter(|uid| !uids.contains(uid)) {
                        sqlx::query("DELETE FROM messages WHERE folder = ?1 AND uid = ?2 AND uidvalidity = ?3").bind(&input.folder).bind(uid).bind(uidvalidity).execute(&mut *tx).await?;
                    }
                }
                SnapshotTransport::Opaque => {
                    for id in old_provider_ids
                        .iter()
                        .filter(|id| !provider_ids.contains(*id))
                    {
                        sqlx::query("DELETE FROM messages WHERE folder = ?1 AND provider_id = ?2")
                            .bind(&input.folder)
                            .bind(id)
                            .execute(&mut *tx)
                            .await?;
                    }
                    // Null-provider rows are retained unless safely merged above.
                }
            }
        }
        let new_rows = input
            .rows
            .iter()
            .filter(|row| match input.transport {
                SnapshotTransport::Opaque => row.provider_id.as_ref().is_some_and(|id| {
                    !old_provider_ids.contains(id) && !reconciled_provider_ids.contains(id.as_str())
                }),
                SnapshotTransport::Imap { .. } => row.uid > old_max,
            })
            .cloned()
            .collect();
        sqlx::query("UPDATE folders SET last_sync_unix = ?2 WHERE name = ?1")
            .bind(&input.folder)
            .bind(unix_now())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(SnapshotOutput {
            applied: true,
            new_rows,
        })
    }

    /// Reconcile folders with a complete, authoritative provider listing.
    ///
    /// Upserts every listed folder and removes any stored folder absent from the
    /// listing in one transaction. Messages are deleted before their folder
    /// metadata; the schema's message-delete trigger clears the matching FTS
    /// entries. There is no foreign key from messages to folders — callers must
    /// treat this as authoritative and never pass a partial listing.
    pub async fn reconcile_folders(&self, folders: &[FolderRow]) -> Result<()> {
        use std::collections::HashSet;

        let names: HashSet<&str> = folders.iter().map(|folder| folder.name.as_str()).collect();
        let mut tx = self.pool.begin().await?;
        for folder in folders {
            sqlx::query(
                "INSERT INTO folders (name, delim, special_use, attrs, uidvalidity, uidnext, last_sync_unix)
                  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(name) DO UPDATE SET
                    delim = excluded.delim,
                    special_use = excluded.special_use,
                    attrs = excluded.attrs,
                    uidvalidity = COALESCE(excluded.uidvalidity, folders.uidvalidity),
                    uidnext = COALESCE(excluded.uidnext, folders.uidnext)",
            )
            .bind(&folder.name)
            .bind(&folder.delim)
            .bind(&folder.special_use)
            .bind(&folder.attrs)
            .bind(folder.uidvalidity)
            .bind(folder.uidnext)
            .bind(folder.last_sync_unix)
            .execute(&mut *tx)
            .await?;
        }

        let existing: Vec<(String,)> = sqlx::query_as("SELECT name FROM folders")
            .fetch_all(&mut *tx)
            .await?;
        for (name,) in existing {
            if !names.contains(name.as_str()) {
                // The schema's message-delete trigger removes FTS rows; delete
                // messages before their now-orphaned folder metadata.
                sqlx::query("DELETE FROM messages WHERE folder = ?1")
                    .bind(&name)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("DELETE FROM folders WHERE name = ?1")
                    .bind(name)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn upsert_folder(&self, f: &FolderRow) -> Result<()> {
        sqlx::query(
            "INSERT INTO folders (name, delim, special_use, attrs, uidvalidity, uidnext, last_sync_unix)
              VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(name) DO UPDATE SET
                delim = excluded.delim,
                special_use = excluded.special_use,
                attrs = excluded.attrs,
                uidvalidity = COALESCE(excluded.uidvalidity, folders.uidvalidity),
                uidnext = COALESCE(excluded.uidnext, folders.uidnext)",
        )
        .bind(&f.name)
        .bind(&f.delim)
        .bind(&f.special_use)
        .bind(&f.attrs)
        .bind(f.uidvalidity)
        .bind(f.uidnext)
        .bind(f.last_sync_unix)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record that an existing folder completed a successful sync.
    pub async fn mark_folder_synced(&self, folder: &str) -> Result<i64> {
        let timestamp = unix_now();
        let result = sqlx::query("UPDATE folders SET last_sync_unix = ?2 WHERE name = ?1")
            .bind(folder)
            .bind(timestamp)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(Error::FolderNotFound(folder.to_owned()));
        }
        Ok(timestamp)
    }

    pub async fn list_folders(&self) -> Result<Vec<FolderRow>> {
        let rows: Vec<FolderRow> = sqlx::query_as(
            "SELECT name, delim, special_use, attrs, uidvalidity, uidnext, last_sync_unix
              FROM folders ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn get_delta_link(&self, folder: &str) -> Result<Option<String>> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT delta_link FROM folders WHERE name = ?1")
                .bind(folder)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(v,)| v))
    }

    pub async fn set_delta_link(&self, folder: &str, link: Option<&str>) -> Result<()> {
        sqlx::query("UPDATE folders SET delta_link = ?2 WHERE name = ?1")
            .bind(folder)
            .bind(link)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn folder_max_uid(&self, folder: &str, uidvalidity: i64) -> Result<Option<i64>> {
        let row: Option<(Option<i64>,)> =
            sqlx::query_as("SELECT MAX(uid) FROM messages WHERE folder = ?1 AND uidvalidity = ?2")
                .bind(folder)
                .bind(uidvalidity)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(v,)| v))
    }

    pub async fn folder_uidvalidity(&self, name: &str) -> Result<Option<i64>> {
        let row: Option<(Option<i64>,)> =
            sqlx::query_as("SELECT uidvalidity FROM folders WHERE name = ?1")
                .bind(name)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(v,)| v))
    }

    /// Drop all messages in a folder. Use when UIDVALIDITY changes.
    pub async fn wipe_folder_messages(&self, folder: &str) -> Result<()> {
        sqlx::query("DELETE FROM messages WHERE folder = ?1")
            .bind(folder)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn upsert_message(&self, m: &MessageRow) -> Result<()> {
        sqlx::query(
            "INSERT INTO messages
                (folder, uid, uidvalidity, message_id, subject, from_addr, to_addrs,
                 date_unix, flags, maildir_path, headers_only, fetched_at_unix,
                 in_reply_to, refs, thread_id, provider_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
             ON CONFLICT(folder, uid, uidvalidity) DO UPDATE SET
                message_id = excluded.message_id,
                subject = excluded.subject,
                from_addr = excluded.from_addr,
                to_addrs = excluded.to_addrs,
                date_unix = excluded.date_unix,
                flags = excluded.flags,
                maildir_path = COALESCE(excluded.maildir_path, messages.maildir_path),
                headers_only = MIN(excluded.headers_only, messages.headers_only),
                fetched_at_unix = excluded.fetched_at_unix,
                in_reply_to = COALESCE(excluded.in_reply_to, messages.in_reply_to),
                refs = COALESCE(excluded.refs, messages.refs),
                thread_id = COALESCE(excluded.thread_id, messages.thread_id),
                provider_id = COALESCE(excluded.provider_id, messages.provider_id)",
        )
        .bind(&m.folder)
        .bind(m.uid)
        .bind(m.uidvalidity)
        .bind(&m.message_id)
        .bind(&m.subject)
        .bind(&m.from_addr)
        .bind(&m.to_addrs)
        .bind(m.date_unix)
        .bind(&m.flags)
        .bind(&m.maildir_path)
        .bind(m.headers_only)
        .bind(m.fetched_at_unix)
        .bind(&m.in_reply_to)
        .bind(&m.refs)
        .bind(&m.thread_id)
        .bind(&m.provider_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn folder_provider_ids(&self, folder: &str) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT provider_id FROM messages WHERE folder = ?1 AND provider_id IS NOT NULL",
        )
        .bind(folder)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// Re-key a legacy opaque row to the canonical UID derived from its provider ID.
    ///
    /// Older releases used truncated/index-XOR UIDs.  A provider ID identifies the
    /// logical message, so collapse every duplicate to the richest row before the
    /// current snapshot upsert refreshes its headers.
    pub async fn rekey_opaque_message(
        &self,
        folder: &str,
        provider_id: &str,
        uid: i64,
        uidvalidity: i64,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let keeper: Option<(i64, i64)> = sqlx::query_as(
            "SELECT uid, uidvalidity FROM messages
             WHERE folder = ?1 AND provider_id = ?2
             ORDER BY (maildir_path IS NOT NULL) DESC, headers_only ASC,
                      fetched_at_unix DESC, uid DESC
             LIMIT 1",
        )
        .bind(folder)
        .bind(provider_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((old_uid, old_uidvalidity)) = keeper else {
            tx.commit().await?;
            return Ok(());
        };

        sqlx::query(
            "DELETE FROM messages WHERE folder = ?1 AND provider_id = ?2
             AND NOT (uid = ?3 AND uidvalidity = ?4)",
        )
        .bind(folder)
        .bind(provider_id)
        .bind(old_uid)
        .bind(old_uidvalidity)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE messages SET uid = ?3, uidvalidity = ?4
             WHERE folder = ?1 AND uid = ?2 AND uidvalidity = ?5",
        )
        .bind(folder)
        .bind(old_uid)
        .bind(uid)
        .bind(uidvalidity)
        .bind(old_uidvalidity)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Delete opaque rows absent from a complete provider snapshot.
    pub async fn delete_provider_ids_not_in(
        &self,
        folder: &str,
        provider_ids: &std::collections::HashSet<String>,
    ) -> Result<()> {
        let existing = self.folder_provider_ids(folder).await?;
        for provider_id in existing.into_iter().filter(|id| !provider_ids.contains(id)) {
            sqlx::query("DELETE FROM messages WHERE folder = ?1 AND provider_id = ?2")
                .bind(folder)
                .bind(provider_id)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    /// Look up the provider's opaque string id for a message row.
    /// Returns `None` when the row was synced before migration 0006 (IMAP rows
    /// always stay `None`).
    pub async fn provider_id_for(&self, folder: &str, uid: i64) -> Result<Option<String>> {
        let row: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT provider_id FROM messages
             WHERE folder = ?1 AND uid = ?2 AND provider_id IS NOT NULL
             LIMIT 1",
        )
        .bind(folder)
        .bind(uid)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(v,)| v))
    }

    /// Update threading columns and resolve thread_id via the JWZ algorithm.
    /// The public signature is unchanged; the implementation now uses
    /// `Threader::ingest` from the `threading` module.
    pub async fn set_threading(
        &self,
        folder: &str,
        uid: i64,
        uidvalidity: i64,
        message_id: Option<&str>,
        in_reply_to: Option<&str>,
        refs: &[String],
    ) -> Result<()> {
        // Fetch the subject for loose Subject grouping.
        let subject_row: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT subject FROM messages WHERE folder = ?1 AND uid = ?2 AND uidvalidity = ?3",
        )
        .bind(folder)
        .bind(uid)
        .bind(uidvalidity)
        .fetch_optional(&self.pool)
        .await?;
        let subject = subject_row.and_then(|(s,)| s);

        // Synthesise a stable message_id when the message has none.
        let synthetic = format!("{folder}/{uid}/{uidvalidity}");
        let mid = message_id.unwrap_or(synthetic.as_str());

        let thread_id = threading::Threader::new(&self.pool)
            .ingest(mid, in_reply_to, refs, subject.as_deref())
            .await?;

        // Persist refs column update.
        let refs_joined = if refs.is_empty() {
            None
        } else {
            Some(refs.join("\n"))
        };
        sqlx::query(
            "UPDATE messages
             SET in_reply_to = ?4, refs = ?5, thread_id = ?6
             WHERE folder = ?1 AND uid = ?2 AND uidvalidity = ?3",
        )
        .bind(folder)
        .bind(uid)
        .bind(uidvalidity)
        .bind(in_reply_to)
        .bind(refs_joined)
        .bind(&thread_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_thread(&self, thread_id: &str) -> Result<Vec<MessageRow>> {
        let rows: Vec<MessageRow> = sqlx::query_as(
            "SELECT folder, uid, uidvalidity, message_id, subject, from_addr, to_addrs,
                    date_unix, flags, maildir_path, headers_only, fetched_at_unix,
                    in_reply_to, refs, thread_id, provider_id
             FROM messages
             WHERE thread_id = ?1
             ORDER BY date_unix ASC NULLS LAST",
        )
        .bind(thread_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Insert/replace a message in the FTS index. `body` may be empty.
    #[allow(clippy::too_many_arguments)]
    pub async fn index_for_search(
        &self,
        folder: &str,
        uid: i64,
        uidvalidity: i64,
        subject: &str,
        from_addr: &str,
        to_addrs: &str,
        body: &str,
    ) -> Result<()> {
        // Find the rowid via the messages PK, since FTS5 keys by rowid.
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT id FROM messages WHERE folder = ?1 AND uid = ?2 AND uidvalidity = ?3",
        )
        .bind(folder)
        .bind(uid)
        .bind(uidvalidity)
        .fetch_optional(&self.pool)
        .await?;
        let Some((id,)) = row else {
            return Ok(());
        };
        // Replace prior entry to avoid duplicates.
        sqlx::query("DELETE FROM messages_fts WHERE rowid = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "INSERT INTO messages_fts(rowid, subject, from_addr, to_addrs, body)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(id)
        .bind(subject)
        .bind(from_addr)
        .bind(to_addrs)
        .bind(body)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // -- outbox --

    pub async fn outbox_enqueue(&self, raw: &[u8]) -> Result<i64> {
        let now = unix_now();
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO outbox (enqueued_unix, raw, attempts, next_retry_unix)
             VALUES (?1, ?2, 0, ?1)
             RETURNING id",
        )
        .bind(now)
        .bind(raw)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    pub async fn outbox_list(&self) -> Result<Vec<OutboxRow>> {
        let rows: Vec<OutboxRow> = sqlx::query_as(
            "SELECT id, enqueued_unix, raw, attempts, next_retry_unix, last_error
             FROM outbox ORDER BY enqueued_unix ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn outbox_due(&self) -> Result<Vec<OutboxRow>> {
        let now = unix_now();
        let rows: Vec<OutboxRow> = sqlx::query_as(
            "SELECT id, enqueued_unix, raw, attempts, next_retry_unix, last_error
             FROM outbox WHERE next_retry_unix <= ?1
             ORDER BY enqueued_unix ASC",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn outbox_delete(&self, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM outbox WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn outbox_record_failure(&self, id: i64, error: &str) -> Result<()> {
        // Exponential backoff: 30, 60, 120, 240, … capped at 1h.
        let row: Option<(i64,)> = sqlx::query_as("SELECT attempts FROM outbox WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        let attempts = row.map(|(a,)| a).unwrap_or(0) + 1;
        let delay = 30i64.saturating_mul(1 << (attempts - 1).min(7)).min(3600);
        let next = unix_now() + delay;
        sqlx::query(
            "UPDATE outbox SET attempts = ?2, next_retry_unix = ?3, last_error = ?4
             WHERE id = ?1",
        )
        .bind(id)
        .bind(attempts)
        .bind(next)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn search(&self, query: &str, limit: u32) -> Result<Vec<MessageRow>> {
        let rows: Vec<MessageRow> = sqlx::query_as(
            "SELECT m.folder, m.uid, m.uidvalidity, m.message_id, m.subject, m.from_addr,
                    m.to_addrs, m.date_unix, m.flags, m.maildir_path, m.headers_only,
                    m.fetched_at_unix, m.in_reply_to, m.refs, m.thread_id, m.provider_id
             FROM messages_fts f
             JOIN messages m ON m.id = f.rowid
             WHERE f.messages_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )
        .bind(query)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Add or remove flag tokens from the local cached flags column.
    /// `add` and `remove` are sets of system flags (`\\Seen`, `\\Flagged`,
    /// etc.). Idempotent: adding an existing flag is a no-op.
    pub async fn mutate_flags(
        &self,
        folder: &str,
        uids: &[i64],
        add: &[&str],
        remove: &[&str],
    ) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        let placeholders = (1..=uids.len())
            .map(|i| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT folder, uid, uidvalidity, flags FROM messages
             WHERE folder = ?1 AND uid IN ({placeholders})"
        );
        let mut q = sqlx::query_as::<_, (String, i64, i64, String)>(&sql).bind(folder);
        for u in uids {
            q = q.bind(u);
        }
        let rows = q.fetch_all(&self.pool).await?;
        for (_, uid, uidvalidity, flags) in rows {
            let mut tokens: Vec<String> = flags.split_whitespace().map(|s| s.to_string()).collect();
            for r in remove {
                tokens.retain(|t| !t.eq_ignore_ascii_case(r));
            }
            for a in add {
                if !tokens.iter().any(|t| t.eq_ignore_ascii_case(a)) {
                    tokens.push((*a).to_string());
                }
            }
            let new_flags = tokens.join(" ");
            sqlx::query(
                "UPDATE messages SET flags = ?4
                 WHERE folder = ?1 AND uid = ?2 AND uidvalidity = ?3",
            )
            .bind(folder)
            .bind(uid)
            .bind(uidvalidity)
            .bind(&new_flags)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    /// Return every UID currently stored for `folder` at the given uidvalidity.
    /// Use to compare against the server's authoritative UID set when pruning.
    pub async fn folder_uids(&self, folder: &str, uidvalidity: i64) -> Result<Vec<i64>> {
        let uids: Vec<i64> =
            sqlx::query_scalar("SELECT uid FROM messages WHERE folder = ?1 AND uidvalidity = ?2")
                .bind(folder)
                .bind(uidvalidity)
                .fetch_all(&self.pool)
                .await?;
        Ok(uids)
    }

    /// Return `(folder, unread_count)` pairs for every folder with at least one
    /// unread message. "Unread" = flags do not contain "seen"; deleted rows are
    /// excluded.
    pub async fn folder_unread_counts(&self) -> Result<Vec<(String, i64)>> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT folder, COUNT(*) FROM messages
             WHERE LOWER(flags) NOT LIKE '%seen%'
               AND LOWER(flags) NOT LIKE '%deleted%'
             GROUP BY folder",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Drop messages from the local index (e.g. after EXPUNGE or UID MOVE).
    pub async fn delete_messages(&self, folder: &str, uids: &[i64]) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        let placeholders = (1..=uids.len())
            .map(|i| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("DELETE FROM messages WHERE folder = ?1 AND uid IN ({placeholders})");
        let mut q = sqlx::query(&sql).bind(folder);
        for u in uids {
            q = q.bind(u);
        }
        q.execute(&self.pool).await?;
        Ok(())
    }

    /// Drop opaque-provider messages by their exact provider IDs.
    pub async fn delete_messages_by_provider_ids(
        &self,
        folder: &str,
        ids: &[String],
    ) -> Result<()> {
        const SQLITE_BIND_LIMIT: usize = 999;
        const PROVIDER_ID_BATCH_SIZE: usize = SQLITE_BIND_LIMIT - 1;

        for ids in ids.chunks(PROVIDER_ID_BATCH_SIZE) {
            let placeholders = (1..=ids.len())
                .map(|i| format!("?{}", i + 1))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "DELETE FROM messages WHERE folder = ?1 AND provider_id IN ({placeholders})"
            );
            let mut query = sqlx::query(&sql).bind(folder);
            for id in ids {
                query = query.bind(id);
            }
            query.execute(&self.pool).await?;
        }
        Ok(())
    }

    /// Drop all messages with `\Deleted` set (mirrors server EXPUNGE locally).
    pub async fn purge_deleted(&self, folder: &str) -> Result<u64> {
        let res = sqlx::query(
            "DELETE FROM messages
             WHERE folder = ?1
               AND (instr(' ' || flags || ' ', ' \\Deleted ') > 0
                    OR instr(' ' || flags || ' ', ' Deleted ') > 0)",
        )
        .bind(folder)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    pub async fn list_unfetched(&self, folder: &str, limit: u32) -> Result<Vec<i64>> {
        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT uid FROM messages
             WHERE folder = ?1 AND maildir_path IS NULL
             ORDER BY date_unix DESC NULLS LAST
             LIMIT ?2",
        )
        .bind(folder)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(u,)| u).collect())
    }

    pub async fn set_maildir_path(
        &self,
        folder: &str,
        uid: i64,
        uidvalidity: i64,
        path: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE messages
             SET maildir_path = ?4, headers_only = 0
             WHERE folder = ?1 AND uid = ?2 AND uidvalidity = ?3",
        )
        .bind(folder)
        .bind(uid)
        .bind(uidvalidity)
        .bind(path)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_messages(&self, folder: &str, limit: u32) -> Result<Vec<MessageRow>> {
        let rows: Vec<MessageRow> = sqlx::query_as(
            "SELECT folder, uid, uidvalidity, message_id, subject, from_addr, to_addrs,
                    date_unix, flags, maildir_path, headers_only, fetched_at_unix,
                    in_reply_to, refs, thread_id, provider_id
             FROM messages
             WHERE folder = ?1
             ORDER BY date_unix DESC NULLS LAST
             LIMIT ?2",
        )
        .bind(folder)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Maildir layout: `<root>/<folder-as-maildir++>/{cur,new,tmp}/`.
    /// Folder hierarchy `INBOX/Work` becomes `INBOX.Work` per Maildir++.
    pub fn maildir_for(&self, folder: &str) -> PathBuf {
        let safe = folder.replace('/', ".");
        self.root.join(&safe)
    }

    pub fn ensure_maildir(&self, folder: &str) -> Result<PathBuf> {
        let dir = self.maildir_for(folder);
        for sub in ["cur", "new", "tmp"] {
            std::fs::create_dir_all(dir.join(sub))?;
        }
        Ok(dir)
    }

    /// Write raw RFC 5322 bytes into Maildir `cur/` with flag-encoded info section.
    /// Filename: `<ts>.<pid>_<counter>.<host>:2,<flags>`.
    pub fn write_maildir(&self, folder: &str, raw: &[u8], flags: &str) -> Result<PathBuf> {
        let dir = self.ensure_maildir(folder)?;
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let pid = std::process::id();
        let n = MAILDIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let host = gethostname::gethostname()
            .to_string_lossy()
            .replace([':', '/'], "_");
        let info = maildir_info(flags);
        let name = format!("{ts}.{pid}_{n}.{host}:2,{info}");
        let path = dir.join("cur").join(&name);
        std::fs::write(&path, raw)?;
        Ok(path)
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Translate IMAP flag string ("\\Seen \\Flagged") into Maildir info chars.
/// Maildir info: P=passed, R=replied, S=seen, T=trashed, D=draft, F=flagged.
/// Letters MUST be ASCII-sorted in info section.
fn maildir_info(flags: &str) -> String {
    let mut out = Vec::new();
    let lower = flags.to_ascii_lowercase();
    if lower.contains("\\seen") {
        out.push('S');
    }
    if lower.contains("\\answered") {
        out.push('R');
    }
    if lower.contains("\\flagged") {
        out.push('F');
    }
    if lower.contains("\\draft") {
        out.push('D');
    }
    if lower.contains("\\deleted") {
        out.push('T');
    }
    out.sort_unstable();
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maildir_info_sorted() {
        assert_eq!(maildir_info("\\Seen \\Flagged"), "FS");
        assert_eq!(maildir_info("\\Answered"), "R");
        assert_eq!(maildir_info(""), "");
    }

    async fn make_in_memory_store() -> Store {
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        let opts = SqliteConnectOptions::new().in_memory(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .expect("pool");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrate");
        Store::from_pool(pool)
    }

    fn make_row(folder: &str, uid: i64, provider_id: Option<&str>) -> MessageRow {
        MessageRow {
            folder: folder.to_string(),
            uid,
            uidvalidity: 0,
            message_id: None,
            subject: None,
            from_addr: None,
            to_addrs: None,
            date_unix: None,
            flags: String::new(),
            maildir_path: None,
            headers_only: 1,
            fetched_at_unix: 0,
            in_reply_to: None,
            refs: None,
            thread_id: None,
            provider_id: provider_id.map(|s| s.to_string()),
        }
    }

    #[tokio::test]
    async fn reconcile_folders_removes_missing_folder_messages_and_fts_rows() {
        let store = make_in_memory_store().await;
        let folder = |name: &str| FolderRow {
            name: name.to_owned(),
            delim: Some("/".to_owned()),
            special_use: None,
            attrs: None,
            uidvalidity: None,
            uidnext: None,
            delta_link: None,
            last_sync_unix: None,
        };
        store
            .reconcile_folders(&[folder("Inbox"), folder("Archive")])
            .await
            .unwrap();
        let mut archived = make_row("Archive", 1, None);
        archived.subject = Some("obsolete needle".to_owned());
        store.upsert_message(&archived).await.unwrap();
        store
            .index_for_search("Archive", 1, 0, "obsolete needle", "", "", "")
            .await
            .unwrap();

        store.reconcile_folders(&[folder("Inbox")]).await.unwrap();

        assert_eq!(
            store
                .list_folders()
                .await
                .unwrap()
                .into_iter()
                .map(|folder| folder.name)
                .collect::<Vec<_>>(),
            vec!["Inbox"]
        );
        assert!(store.list_messages("Archive", 10).await.unwrap().is_empty());
        assert!(store.search("obsolete", 10).await.unwrap().is_empty());
    }

    /// Migration 0006 runs cleanly on a fresh store (implicit in make_in_memory_store).
    /// Verify provider_id round-trips through upsert_message.
    #[tokio::test]
    async fn provider_id_round_trips() {
        let store = make_in_memory_store().await;
        let pid = "AAMkAGE1M2IyNGNm-test-graph-id";
        let row = make_row("Inbox", 42, Some(pid));
        store.upsert_message(&row).await.unwrap();

        // provider_id_for returns the stored value.
        let got = store.provider_id_for("Inbox", 42).await.unwrap();
        assert_eq!(got, Some(pid.to_string()));
    }

    /// IMAP rows (provider_id = None) return None from provider_id_for.
    #[tokio::test]
    async fn provider_id_none_for_imap_rows() {
        let store = make_in_memory_store().await;
        let row = make_row("INBOX", 1, None);
        store.upsert_message(&row).await.unwrap();

        let got = store.provider_id_for("INBOX", 1).await.unwrap();
        assert_eq!(got, None);
    }

    /// provider_id is preserved (not overwritten to NULL) by a subsequent upsert
    /// that doesn't set it, thanks to COALESCE in the ON CONFLICT clause.
    #[tokio::test]
    async fn provider_id_preserved_on_flag_update() {
        let store = make_in_memory_store().await;
        let pid = "jmap-id-xyz";
        // First upsert with provider_id.
        store
            .upsert_message(&make_row("Inbox", 7, Some(pid)))
            .await
            .unwrap();
        // Second upsert (e.g. flag refresh) without provider_id.
        let mut refresh = make_row("Inbox", 7, None);
        refresh.flags = "\\Seen".to_string();
        store.upsert_message(&refresh).await.unwrap();

        let got = store.provider_id_for("Inbox", 7).await.unwrap();
        assert_eq!(
            got,
            Some(pid.to_string()),
            "provider_id must survive flag refresh"
        );
    }

    #[tokio::test]
    async fn opaque_rekey_preserves_downloaded_legacy_state() {
        let store = make_in_memory_store().await;
        let old_uid = 427_567_909;
        let full_uid = 8_783_962_037_831_871_269;
        let mut legacy = make_row("Inbox", old_uid, Some("test"));
        legacy.maildir_path = Some("cur/downloaded:2,S".to_owned());
        legacy.headers_only = 0;
        legacy.thread_id = Some("thread".to_owned());
        store.upsert_message(&legacy).await.unwrap();

        store
            .rekey_opaque_message("Inbox", "test", full_uid, 0)
            .await
            .unwrap();
        let rows = store.list_messages("Inbox", 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].uid, full_uid);
        assert_eq!(rows[0].maildir_path.as_deref(), Some("cur/downloaded:2,S"));
        assert_eq!(rows[0].headers_only, 0);
        assert_eq!(rows[0].thread_id.as_deref(), Some("thread"));
    }

    #[tokio::test]
    async fn snapshot_generation_rejects_unreserved_future_apply_and_keeps_unused_reservations() {
        let store = make_in_memory_store().await;
        let first = store.reserve_snapshot_generation("Inbox").await.unwrap();
        assert_eq!(first, 1);
        let generations: (i64, i64) = sqlx::query_as(
            "SELECT snapshot_generation, latest_reserved_generation FROM folders WHERE name = ?1",
        )
        .bind("Inbox")
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(generations, (0, 1));

        let input = |generation| SnapshotInput {
            folder: "Inbox".into(),
            generation,
            complete: true,
            transport: SnapshotTransport::Imap { uidvalidity: 1 },
            rows: Vec::new(),
        };
        assert!(matches!(
            store.apply_snapshot(input(first + 1)).await,
            Err(Error::UnreservedSnapshotGeneration { .. })
        ));
        let unused = store.reserve_snapshot_generation("Inbox").await.unwrap();
        assert_eq!(unused, 2);
        let generations: (i64, i64) = sqlx::query_as(
            "SELECT snapshot_generation, latest_reserved_generation FROM folders WHERE name = ?1",
        )
        .bind("Inbox")
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(generations, (0, 2));
    }

    #[tokio::test]
    async fn snapshot_generation_rejects_zero_for_unreserved_upserted_folder() {
        let store = make_in_memory_store().await;
        store
            .upsert_folder(&FolderRow {
                name: "Inbox".into(),
                delim: None,
                special_use: None,
                attrs: None,
                uidvalidity: None,
                uidnext: None,
                delta_link: None,
                last_sync_unix: None,
            })
            .await
            .unwrap();

        assert!(matches!(
            store
                .apply_snapshot(SnapshotInput {
                    folder: "Inbox".into(),
                    generation: 0,
                    complete: true,
                    transport: SnapshotTransport::Imap { uidvalidity: 1 },
                    rows: vec![SnapshotHeader {
                        uid: 1,
                        uidvalidity: 1,
                        message_id: None,
                        subject: None,
                        from_addr: None,
                        to_addrs: None,
                        date_unix: None,
                        flags: String::new(),
                        fetched_at_unix: 0,
                        provider_id: None,
                    }],
                })
                .await,
            Err(Error::UnreservedSnapshotGeneration { generation: 0, .. })
        ));
        assert!(store.list_messages("Inbox", 10).await.unwrap().is_empty());
        let generations: (i64, i64) = sqlx::query_as(
            "SELECT snapshot_generation, latest_reserved_generation FROM folders WHERE name = ?1",
        )
        .bind("Inbox")
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(generations, (0, 0));
    }

    #[tokio::test]
    async fn mark_folder_synced_records_current_time_and_requires_existing_folder() {
        let store = make_in_memory_store().await;
        assert!(matches!(
            store.mark_folder_synced("Missing").await,
            Err(Error::FolderNotFound(folder)) if folder == "Missing"
        ));
        store
            .upsert_folder(&FolderRow {
                name: "Inbox".into(),
                delim: None,
                special_use: None,
                attrs: None,
                uidvalidity: None,
                uidnext: None,
                delta_link: None,
                last_sync_unix: None,
            })
            .await
            .unwrap();

        let before = unix_now();
        let timestamp = store.mark_folder_synced("Inbox").await.unwrap();
        let after = unix_now();
        assert!((before..=after).contains(&timestamp));
        assert_eq!(
            store.list_folders().await.unwrap()[0].last_sync_unix,
            Some(timestamp)
        );
    }

    #[tokio::test]
    async fn applied_snapshot_sets_folder_last_sync_and_replays_do_not_advance_it() {
        let store = make_in_memory_store().await;
        let generation = store.reserve_snapshot_generation("Inbox").await.unwrap();
        assert_eq!(store.list_folders().await.unwrap()[0].last_sync_unix, None);

        let before = unix_now();
        assert!(
            store
                .apply_snapshot(SnapshotInput {
                    folder: "Inbox".into(),
                    generation,
                    complete: true,
                    transport: SnapshotTransport::Imap { uidvalidity: 1 },
                    rows: Vec::new(),
                })
                .await
                .unwrap()
                .applied
        );
        let after = unix_now();
        let last_sync_unix = store.list_folders().await.unwrap()[0]
            .last_sync_unix
            .expect("applied snapshot updates last sync");
        assert!((before..=after).contains(&last_sync_unix));

        sqlx::query("UPDATE folders SET last_sync_unix = ?2 WHERE name = ?1")
            .bind("Inbox")
            .bind(123)
            .execute(store.pool())
            .await
            .unwrap();
        let last_sync_unix = 123;

        assert!(
            !store
                .apply_snapshot(SnapshotInput {
                    folder: "Inbox".into(),
                    generation,
                    complete: true,
                    transport: SnapshotTransport::Imap { uidvalidity: 1 },
                    rows: Vec::new(),
                })
                .await
                .unwrap()
                .applied
        );
        assert_eq!(
            store.list_folders().await.unwrap()[0].last_sync_unix,
            Some(last_sync_unix)
        );
    }

    #[tokio::test]
    async fn folder_upsert_without_last_sync_preserves_existing_timestamp() {
        let store = make_in_memory_store().await;
        store
            .upsert_folder(&FolderRow {
                name: "Inbox".into(),
                delim: None,
                special_use: None,
                attrs: None,
                uidvalidity: None,
                uidnext: None,
                delta_link: None,
                last_sync_unix: Some(123),
            })
            .await
            .unwrap();
        store
            .upsert_folder(&FolderRow {
                name: "Inbox".into(),
                delim: Some("/".into()),
                special_use: None,
                attrs: None,
                uidvalidity: None,
                uidnext: None,
                delta_link: None,
                last_sync_unix: None,
            })
            .await
            .unwrap();

        assert_eq!(
            store.list_folders().await.unwrap()[0].last_sync_unix,
            Some(123)
        );
    }

    #[tokio::test]
    async fn folder_upsert_stale_timestamp_does_not_replace_store_owned_timestamp() {
        let store = make_in_memory_store().await;
        for last_sync_unix in [Some(200), Some(100)] {
            store
                .upsert_folder(&FolderRow {
                    name: "Inbox".into(),
                    delim: None,
                    special_use: None,
                    attrs: None,
                    uidvalidity: None,
                    uidnext: None,
                    delta_link: None,
                    last_sync_unix,
                })
                .await
                .unwrap();
        }

        assert_eq!(
            store.list_folders().await.unwrap()[0].last_sync_unix,
            Some(200)
        );
    }

    #[tokio::test]
    async fn snapshot_generation_replay_does_not_prune_applied_snapshot() {
        let store = make_in_memory_store().await;
        let generation = store.reserve_snapshot_generation("Inbox").await.unwrap();
        let initial = store
            .apply_snapshot(SnapshotInput {
                folder: "Inbox".into(),
                generation,
                complete: true,
                transport: SnapshotTransport::Imap { uidvalidity: 1 },
                rows: vec![SnapshotHeader {
                    uid: 1,
                    uidvalidity: 1,
                    message_id: None,
                    subject: None,
                    from_addr: None,
                    to_addrs: None,
                    date_unix: None,
                    flags: String::new(),
                    fetched_at_unix: 0,
                    provider_id: None,
                }],
            })
            .await
            .unwrap();
        assert!(initial.applied);
        let generations: (i64, i64) = sqlx::query_as(
            "SELECT snapshot_generation, latest_reserved_generation FROM folders WHERE name = ?1",
        )
        .bind("Inbox")
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(generations, (generation, generation));

        let replay = store
            .apply_snapshot(SnapshotInput {
                folder: "Inbox".into(),
                generation,
                complete: true,
                transport: SnapshotTransport::Imap { uidvalidity: 1 },
                rows: Vec::new(),
            })
            .await
            .unwrap();
        assert!(!replay.applied);
        assert!(replay.new_rows.is_empty());
        let rows = store.list_messages("Inbox", 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].uid, 1);
    }

    #[tokio::test]
    async fn snapshot_generation_rejects_stale_apply() {
        let store = make_in_memory_store().await;
        let older = store.reserve_snapshot_generation("Inbox").await.unwrap();
        let newer = store.reserve_snapshot_generation("Inbox").await.unwrap();
        let input = |generation, uid| SnapshotInput {
            folder: "Inbox".into(),
            generation,
            complete: true,
            transport: SnapshotTransport::Imap { uidvalidity: 1 },
            rows: vec![SnapshotHeader {
                uid,
                uidvalidity: 1,
                message_id: None,
                subject: None,
                from_addr: None,
                to_addrs: None,
                date_unix: None,
                flags: String::new(),
                fetched_at_unix: 0,
                provider_id: None,
            }],
        };
        assert!(store.apply_snapshot(input(newer, 2)).await.unwrap().applied);
        sqlx::query("UPDATE folders SET last_sync_unix = ?2 WHERE name = ?1")
            .bind("Inbox")
            .bind(456)
            .execute(store.pool())
            .await
            .unwrap();
        let last_sync_unix = Some(456);
        assert!(!store.apply_snapshot(input(older, 1)).await.unwrap().applied);
        assert_eq!(
            store.list_folders().await.unwrap()[0].last_sync_unix,
            last_sync_unix
        );
        assert_eq!(store.list_messages("Inbox", 10).await.unwrap()[0].uid, 2);
    }

    #[tokio::test]
    async fn snapshot_trigger_failure_rolls_back_every_mutation() {
        let store = make_in_memory_store().await;
        store
            .upsert_folder(&FolderRow {
                name: "Inbox".into(),
                delim: None,
                special_use: None,
                attrs: None,
                uidvalidity: Some(1),
                uidnext: Some(7),
                delta_link: None,
                last_sync_unix: None,
            })
            .await
            .unwrap();
        let mut old = make_row("Inbox", 1, None);
        old.uidvalidity = 1;
        old.maildir_path = Some("cur/body".into());
        old.headers_only = 0;
        old.thread_id = Some("thread".into());
        store.upsert_message(&old).await.unwrap();
        store
            .index_for_search("Inbox", 1, 1, "oldsecret", "", "", "oldsecret")
            .await
            .unwrap();
        sqlx::query("CREATE TRIGGER reject_snapshot BEFORE INSERT ON messages WHEN NEW.uid = 2 BEGIN SELECT RAISE(ABORT, 'reject'); END").execute(store.pool()).await.unwrap();
        let generation = store.reserve_snapshot_generation("Inbox").await.unwrap();
        let result = store
            .apply_snapshot(SnapshotInput {
                folder: "Inbox".into(),
                generation,
                complete: true,
                transport: SnapshotTransport::Imap { uidvalidity: 2 },
                rows: vec![SnapshotHeader {
                    uid: 2,
                    uidvalidity: 2,
                    message_id: None,
                    subject: Some("new".into()),
                    from_addr: None,
                    to_addrs: None,
                    date_unix: None,
                    flags: String::new(),
                    fetched_at_unix: 1,
                    provider_id: None,
                }],
            })
            .await;
        assert!(result.is_err());
        assert_eq!(store.list_folders().await.unwrap()[0].last_sync_unix, None);
        assert_eq!(store.folder_uidvalidity("Inbox").await.unwrap(), Some(1));
        let rows = store.list_messages("Inbox", 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].maildir_path.as_deref(), Some("cur/body"));
        assert_eq!(rows[0].thread_id.as_deref(), Some("thread"));
        assert_eq!(store.search("oldsecret", 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn delete_provider_ids_removes_exact_rows_and_fts() {
        let store = make_in_memory_store().await;
        let mut deleted = make_row("Inbox", 1, Some("graph-deleted"));
        deleted.uidvalidity = 1;
        store.upsert_message(&deleted).await.unwrap();
        store
            .index_for_search("Inbox", 1, 1, "removedsecret", "", "", "removedsecret")
            .await
            .unwrap();
        let mut kept = make_row("Inbox", 2, Some("graph-kept"));
        kept.uidvalidity = 1;
        store.upsert_message(&kept).await.unwrap();

        store
            .delete_messages_by_provider_ids("Inbox", &["graph-deleted".into()])
            .await
            .unwrap();
        assert_eq!(store.list_messages("Inbox", 10).await.unwrap().len(), 1);
        assert_eq!(
            store.provider_id_for("Inbox", 2).await.unwrap().as_deref(),
            Some("graph-kept")
        );
        assert!(store.search("removedsecret", 10).await.unwrap().is_empty());
        store
            .delete_messages_by_provider_ids("Inbox", &[])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn opaque_snapshot_new_rows_are_selected_by_provider_id() {
        let store = make_in_memory_store().await;
        let existing = SnapshotHeader {
            uid: 1,
            uidvalidity: 0,
            message_id: None,
            subject: None,
            from_addr: None,
            to_addrs: None,
            date_unix: None,
            flags: String::new(),
            fetched_at_unix: 0,
            provider_id: Some("opaque-existing".into()),
        };
        let generation = store.reserve_snapshot_generation("Inbox").await.unwrap();
        store
            .apply_snapshot(SnapshotInput {
                folder: "Inbox".into(),
                generation,
                complete: true,
                transport: SnapshotTransport::Opaque,
                rows: vec![existing.clone()],
            })
            .await
            .unwrap();
        let mut new = existing.clone();
        new.uid = 2;
        new.provider_id = Some("opaque-new".into());
        let output = store
            .apply_snapshot(SnapshotInput {
                folder: "Inbox".into(),
                generation: store.reserve_snapshot_generation("Inbox").await.unwrap(),
                complete: true,
                transport: SnapshotTransport::Opaque,
                rows: vec![existing, new],
            })
            .await
            .unwrap();
        assert_eq!(
            output
                .new_rows
                .iter()
                .map(|row| row.provider_id.as_deref())
                .collect::<Vec<_>>(),
            [Some("opaque-new")]
        );
    }

    #[tokio::test]
    async fn purge_deleted_matches_complete_flag_tokens() {
        let store = make_in_memory_store().await;
        for (uid, flags) in [
            (1, "\\Deleted"),
            (2, "\\Seen \\Deleted \\Flagged"),
            (3, "Deleted"),
            (4, "\\Seen Deleted \\Flagged"),
            (5, "DeletedFoo"),
            (6, "\\DeletedFoo"),
        ] {
            let mut row = make_row("Inbox", uid, None);
            row.flags = flags.into();
            store.upsert_message(&row).await.unwrap();
        }

        assert_eq!(store.purge_deleted("Inbox").await.unwrap(), 4);
        let rows = store.list_messages("Inbox", 10).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.iter().map(|row| row.uid).collect::<Vec<_>>(), [5, 6]);
    }

    #[tokio::test]
    async fn delete_trigger_removes_fts_before_rowid_reuse() {
        let store = make_in_memory_store().await;
        let mut old = make_row("Inbox", 1, None);
        old.uidvalidity = 1;
        store.upsert_message(&old).await.unwrap();
        store
            .index_for_search("Inbox", 1, 1, "oldsecret", "", "", "oldsecret")
            .await
            .unwrap();
        store.delete_messages("Inbox", &[1]).await.unwrap();
        let mut replacement = make_row("Inbox", 2, None);
        replacement.uidvalidity = 1;
        store.upsert_message(&replacement).await.unwrap();
        assert!(store.search("oldsecret", 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn opaque_snapshot_preserves_unambiguous_legacy_message_id() {
        let store = make_in_memory_store().await;
        let mut legacy = make_row("Inbox", 99, None);
        legacy.message_id = Some("stable@example.test".into());
        legacy.maildir_path = Some("cur/downloaded".into());
        legacy.headers_only = 0;
        store.upsert_message(&legacy).await.unwrap();
        let generation = store.reserve_snapshot_generation("Inbox").await.unwrap();
        let output = store
            .apply_snapshot(SnapshotInput {
                folder: "Inbox".into(),
                generation,
                complete: true,
                transport: SnapshotTransport::Opaque,
                rows: vec![SnapshotHeader {
                    uid: 500,
                    uidvalidity: 0,
                    message_id: Some("stable@example.test".into()),
                    subject: None,
                    from_addr: None,
                    to_addrs: None,
                    date_unix: None,
                    flags: String::new(),
                    fetched_at_unix: 1,
                    provider_id: Some("provider-500".into()),
                }],
            })
            .await
            .unwrap();
        assert!(output.new_rows.is_empty());
        let rows = store.list_messages("Inbox", 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].uid, 500);
        assert_eq!(rows[0].provider_id.as_deref(), Some("provider-500"));
        assert_eq!(rows[0].maildir_path.as_deref(), Some("cur/downloaded"));
    }

    #[tokio::test]
    async fn opaque_snapshot_does_not_rekey_duplicate_message_ids() {
        let store = make_in_memory_store().await;
        let mut legacy = make_row("Inbox", 99, None);
        legacy.message_id = Some("stable@example.test".into());
        legacy.maildir_path = Some("cur/downloaded".into());
        legacy.headers_only = 0;
        store.upsert_message(&legacy).await.unwrap();
        let generation = store.reserve_snapshot_generation("Inbox").await.unwrap();
        let header = SnapshotHeader {
            uid: 500,
            uidvalidity: 0,
            message_id: Some("stable@example.test".into()),
            subject: None,
            from_addr: None,
            to_addrs: None,
            date_unix: None,
            flags: String::new(),
            fetched_at_unix: 1,
            provider_id: Some("provider-500".into()),
        };
        let mut duplicate = header.clone();
        duplicate.uid = 501;
        duplicate.provider_id = Some("provider-501".into());
        store
            .apply_snapshot(SnapshotInput {
                folder: "Inbox".into(),
                generation,
                complete: true,
                transport: SnapshotTransport::Opaque,
                rows: vec![header, duplicate],
            })
            .await
            .unwrap();
        let rows = store.list_messages("Inbox", 10).await.unwrap();
        let legacy = rows.iter().find(|row| row.uid == 99).unwrap();
        assert_eq!(legacy.provider_id, None);
        assert_eq!(legacy.maildir_path.as_deref(), Some("cur/downloaded"));
    }

    #[tokio::test]
    async fn opaque_snapshot_rejects_invalid_identity_before_mutation() {
        let store = make_in_memory_store().await;
        let generation = store.reserve_snapshot_generation("Inbox").await.unwrap();
        let row = SnapshotHeader {
            uid: 1,
            uidvalidity: 0,
            message_id: None,
            subject: None,
            from_addr: None,
            to_addrs: None,
            date_unix: None,
            flags: String::new(),
            fetched_at_unix: 0,
            provider_id: None,
        };
        assert!(matches!(
            store
                .apply_snapshot(SnapshotInput {
                    folder: "Inbox".into(),
                    generation,
                    complete: true,
                    transport: SnapshotTransport::Opaque,
                    rows: vec![row]
                })
                .await,
            Err(Error::InvalidSnapshot(_))
        ));
        assert!(store.list_messages("Inbox", 10).await.unwrap().is_empty());
    }

    /// Missing uid returns None.
    #[tokio::test]
    async fn provider_id_missing_uid_returns_none() {
        let store = make_in_memory_store().await;
        let got = store.provider_id_for("Inbox", 9999).await.unwrap();
        assert_eq!(got, None);
    }
}
