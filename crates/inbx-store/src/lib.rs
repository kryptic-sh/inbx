use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{FromRow, SqlitePool};

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
}

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
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool, root })
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
                 in_reply_to, refs, thread_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
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
                thread_id = COALESCE(excluded.thread_id, messages.thread_id)",
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
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update threading columns and resolve thread_id by walking In-Reply-To
    /// up through known parents. If no parent is in the store, the message's
    /// own message_id (or self-uid placeholder) becomes the thread root.
    pub async fn set_threading(
        &self,
        folder: &str,
        uid: i64,
        uidvalidity: i64,
        message_id: Option<&str>,
        in_reply_to: Option<&str>,
        refs: &[String],
    ) -> Result<()> {
        let parent_ids: Vec<&str> = refs.iter().map(|s| s.as_str()).chain(in_reply_to).collect();

        // Look up the parent's thread_id, walking from most-recent ref backward.
        let mut thread_id: Option<String> = None;
        for parent in parent_ids.iter().rev() {
            let row: Option<(Option<String>,)> =
                sqlx::query_as("SELECT thread_id FROM messages WHERE message_id = ?1 LIMIT 1")
                    .bind(parent)
                    .fetch_optional(&self.pool)
                    .await?;
            if let Some((Some(t),)) = row {
                thread_id = Some(t);
                break;
            }
        }
        let resolved = thread_id
            .or_else(|| message_id.map(|s| s.to_string()))
            .unwrap_or_else(|| format!("{folder}/{uid}/{uidvalidity}"));
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
        .bind(&resolved)
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
}
