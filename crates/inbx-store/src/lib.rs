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
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("migrate: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("config: {0}")]
    Config(#[from] inbx_config::Error),
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

    pub async fn upsert_folder(&self, f: &FolderRow) -> Result<()> {
        sqlx::query(
            "INSERT INTO folders (name, delim, special_use, attrs, uidvalidity, uidnext)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
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
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_folders(&self) -> Result<Vec<FolderRow>> {
        let rows: Vec<FolderRow> = sqlx::query_as(
            "SELECT name, delim, special_use, attrs, uidvalidity, uidnext
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
                headers_only = excluded.headers_only,
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
                    in_reply_to, refs, thread_id
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
                    m.fetched_at_unix, m.in_reply_to, m.refs, m.thread_id
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

    /// Drop all messages with `\Deleted` set (mirrors server EXPUNGE locally).
    pub async fn purge_deleted(&self, folder: &str) -> Result<u64> {
        let res = sqlx::query(
            "DELETE FROM messages
             WHERE folder = ?1 AND flags LIKE '%\\Deleted%' ESCAPE '\\\\'",
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
                    in_reply_to, refs, thread_id
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

    /// Missing uid returns None.
    #[tokio::test]
    async fn provider_id_missing_uid_returns_none() {
        let store = make_in_memory_store().await;
        let got = store.provider_id_for("Inbox", 9999).await.unwrap();
        assert_eq!(got, None);
    }
}
