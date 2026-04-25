use std::time::{SystemTime, UNIX_EPOCH};

use mail_parser::MessageParser;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

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

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Contact {
    pub email: String,
    pub name: Option<String>,
    pub frecency_count: i64,
    pub last_used_unix: i64,
}

pub struct ContactsStore {
    pool: SqlitePool,
}

impl ContactsStore {
    pub async fn open(account: &str) -> Result<Self> {
        let dir = inbx_config::data_dir()?.join(account);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("contacts.sqlite");
        let opts = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(opts)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn upsert(&self, email: &str, name: Option<&str>) -> Result<()> {
        let now = unix_now();
        sqlx::query(
            "INSERT INTO contacts (email, name, frecency_count, last_used_unix)
             VALUES (?1, ?2, 0, ?3)
             ON CONFLICT(email) DO UPDATE SET
                name = COALESCE(excluded.name, contacts.name),
                last_used_unix = excluded.last_used_unix",
        )
        .bind(email)
        .bind(name)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Bump frecency. Called once per appearance in a sent or received message.
    pub async fn bump(&self, email: &str, name: Option<&str>) -> Result<()> {
        let now = unix_now();
        sqlx::query(
            "INSERT INTO contacts (email, name, frecency_count, last_used_unix)
             VALUES (?1, ?2, 1, ?3)
             ON CONFLICT(email) DO UPDATE SET
                frecency_count = contacts.frecency_count + 1,
                name = COALESCE(excluded.name, contacts.name),
                last_used_unix = excluded.last_used_unix",
        )
        .bind(email)
        .bind(name)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Harvest all addresses (From/To/Cc) from a raw RFC 5322 message.
    /// Returns the number of contacts touched.
    pub async fn harvest(&self, raw: &[u8]) -> Result<usize> {
        let Some(parsed) = MessageParser::default().parse(raw) else {
            return Ok(0);
        };
        let mut touched = 0usize;
        for group in [parsed.from(), parsed.to(), parsed.cc()]
            .into_iter()
            .flatten()
        {
            for addr in group.iter() {
                if let Some(email) = addr.address() {
                    let name = addr.name().map(|s| s.to_string());
                    self.bump(email, name.as_deref()).await?;
                    touched += 1;
                }
            }
        }
        Ok(touched)
    }

    pub async fn list(&self, limit: u32) -> Result<Vec<Contact>> {
        let rows: Vec<Contact> = sqlx::query_as(
            "SELECT email, name, frecency_count, last_used_unix
             FROM contacts
             ORDER BY frecency_count DESC, last_used_unix DESC
             LIMIT ?1",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Prefix/substring match on email or name, frecency-ranked.
    pub async fn search(&self, query: &str, limit: u32) -> Result<Vec<Contact>> {
        let pattern = format!("%{}%", escape_like(query));
        let rows: Vec<Contact> = sqlx::query_as(
            "SELECT email, name, frecency_count, last_used_unix
             FROM contacts
             WHERE email LIKE ?1 ESCAPE '\\' OR name LIKE ?1 ESCAPE '\\'
             ORDER BY
                CASE WHEN email LIKE ?2 ESCAPE '\\' THEN 0 ELSE 1 END,
                frecency_count DESC,
                last_used_unix DESC
             LIMIT ?3",
        )
        .bind(&pattern)
        .bind(format!("{}%", escape_like(query)))
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn delete(&self, email: &str) -> Result<bool> {
        let res = sqlx::query("DELETE FROM contacts WHERE email = ?1")
            .bind(email)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }
}

fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
