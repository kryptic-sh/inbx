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
                 date_unix, flags, maildir_path, headers_only, fetched_at_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(folder, uid, uidvalidity) DO UPDATE SET
                message_id = excluded.message_id,
                subject = excluded.subject,
                from_addr = excluded.from_addr,
                to_addrs = excluded.to_addrs,
                date_unix = excluded.date_unix,
                flags = excluded.flags,
                maildir_path = COALESCE(excluded.maildir_path, messages.maildir_path),
                headers_only = excluded.headers_only,
                fetched_at_unix = excluded.fetched_at_unix",
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
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_messages(&self, folder: &str, limit: u32) -> Result<Vec<MessageRow>> {
        let rows: Vec<MessageRow> = sqlx::query_as(
            "SELECT folder, uid, uidvalidity, message_id, subject, from_addr, to_addrs,
                    date_unix, flags, maildir_path, headers_only, fetched_at_unix
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
