pub mod carddav;

use std::time::{SystemTime, UNIX_EPOCH};

use mail_parser::MessageParser;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

// Re-export so callers can impl the trait without depending on inbx-pgp directly.
pub use inbx_pgp::PubkeyLookup;

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
    pub pgp_pubkey: Option<String>,
    pub pgp_fingerprint: Option<String>,
    pub pgp_seen_unix: Option<i64>,
}

pub struct ContactsStore {
    pool: SqlitePool,
    carddav: Option<CardDavCreds>,
}

#[derive(Clone)]
struct CardDavCreds {
    url: String,
    username: String,
    password: String,
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
        Ok(Self {
            pool,
            carddav: None,
        })
    }

    /// Attach CardDAV credentials so `upsert` auto-pushes on each write.
    pub fn with_carddav(
        mut self,
        cfg: &inbx_config::CardDavConfig,
        account_username: &str,
        password: String,
    ) -> Self {
        let url = if cfg.addressbook_url.ends_with('/') {
            cfg.addressbook_url.clone()
        } else {
            format!("{}/", cfg.addressbook_url)
        };
        let username = cfg
            .username
            .clone()
            .unwrap_or_else(|| account_username.to_string());
        self.carddav = Some(CardDavCreds {
            url,
            username,
            password,
        });
        self
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

        if let Some(creds) = self.carddav.clone() {
            let email = email.to_string();
            let name = name.map(|s| s.to_string());
            tokio::spawn(async move {
                if let Err(e) = push_to_carddav(&creds, &email, name.as_deref()).await {
                    tracing::warn!(email = %email, error = %e, "carddav auto-push failed");
                }
            });
        }
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
            "SELECT email, name, frecency_count, last_used_unix, pgp_pubkey, pgp_fingerprint, pgp_seen_unix
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
            "SELECT email, name, frecency_count, last_used_unix, pgp_pubkey, pgp_fingerprint, pgp_seen_unix
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

    /// Store / update an Autocrypt-harvested pubkey for an email.
    /// Bumps `pgp_seen_unix` to NOW. Creates the contact row if missing.
    pub async fn store_autocrypt(
        &self,
        email: &str,
        armored_pubkey: &str,
        fingerprint: &str,
    ) -> Result<()> {
        let now = unix_now();
        sqlx::query(
            "INSERT INTO contacts (email, name, frecency_count, last_used_unix, pgp_pubkey, pgp_fingerprint, pgp_seen_unix)
             VALUES (?1, NULL, 0, ?2, ?3, ?4, ?2)
             ON CONFLICT(email) DO UPDATE SET
                pgp_pubkey      = excluded.pgp_pubkey,
                pgp_fingerprint = excluded.pgp_fingerprint,
                pgp_seen_unix   = excluded.pgp_seen_unix",
        )
        .bind(email)
        .bind(now)
        .bind(armored_pubkey)
        .bind(fingerprint)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Look up the stored ASCII-armored pubkey for `email`.
    /// Returns `None` if the contact has no stored key.
    pub async fn lookup_pubkey(&self, email: &str) -> Result<Option<String>> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT pgp_pubkey FROM contacts WHERE email = ?1 COLLATE NOCASE")
                .bind(email)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(key,)| key))
    }
}

/// Trait-based decoupling: `inbx-render` depends on `inbx-pgp::PubkeyLookup`,
/// not on `inbx-contacts`. Implemented here so the store satisfies the trait.
#[async_trait::async_trait]
impl inbx_pgp::PubkeyLookup for ContactsStore {
    async fn lookup(&self, email: &str) -> inbx_pgp::Result<Option<inbx_pgp::ArmoredKey>> {
        let key = self
            .lookup_pubkey(email)
            .await
            .map_err(|e| inbx_pgp::Error::Rpgp(e.to_string()))?;
        Ok(key.map(inbx_pgp::ArmoredKey))
    }
}

/// Fire-and-forget CardDAV PUT for a single contact.
/// Uses a deterministic UID so re-PUTs hit the same resource path (idempotent overwrite).
async fn push_to_carddav(
    creds: &CardDavCreds,
    email: &str,
    name: Option<&str>,
) -> carddav::Result<()> {
    let uid = stable_uid_for(email);
    let vcard = carddav::build_vcard(email, name, Some(&uid));
    let resource_url = format!("{}{}.vcf", creds.url, uid);
    carddav::put_vcard(
        &resource_url,
        &creds.username,
        &creds.password,
        &vcard,
        carddav::PutMode::Overwrite,
    )
    .await
}

/// Derive a stable UID from an email address.
/// Stable UID → idempotent re-PUTs hit the same resource path on the server.
fn stable_uid_for(email: &str) -> String {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(email.to_ascii_lowercase().as_bytes());
    let hex = h.finalize();
    format!("inbx-{hex:x}")
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

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    #[test]
    fn stable_uid_deterministic_and_lowercase_normalized() {
        let a = stable_uid_for("Alice@Example.COM");
        let b = stable_uid_for("alice@example.com");
        assert_eq!(a, b, "UID must be case-insensitive");
        assert!(a.starts_with("inbx-"), "UID must have inbx- prefix");
        // Same input always produces same output.
        assert_eq!(a, stable_uid_for("ALICE@EXAMPLE.COM"));
    }

    /// Open an in-memory SQLite ContactsStore (runs all migrations).
    async fn in_memory_store() -> ContactsStore {
        let opts = SqliteConnectOptions::new()
            .filename(":memory:")
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        ContactsStore {
            pool,
            carddav: None,
        }
    }

    #[tokio::test]
    async fn store_autocrypt_creates_row() {
        let store = in_memory_store().await;
        store
            .store_autocrypt("new@example.com", "armored-key", "deadbeef")
            .await
            .unwrap();
        let key = store.lookup_pubkey("new@example.com").await.unwrap();
        assert_eq!(key.as_deref(), Some("armored-key"));
    }

    #[tokio::test]
    async fn store_autocrypt_updates_existing() {
        let store = in_memory_store().await;
        // Create contact first via bump.
        store
            .bump("existing@example.com", Some("Existing"))
            .await
            .unwrap();
        // Now store autocrypt key.
        store
            .store_autocrypt("existing@example.com", "first-key", "aabbccdd")
            .await
            .unwrap();
        // Update with a newer key.
        store
            .store_autocrypt("existing@example.com", "second-key", "11223344")
            .await
            .unwrap();
        let key = store.lookup_pubkey("existing@example.com").await.unwrap();
        assert_eq!(
            key.as_deref(),
            Some("second-key"),
            "key should be updated to the newest value"
        );
    }

    #[tokio::test]
    async fn lookup_pubkey_missing_returns_none() {
        let store = in_memory_store().await;
        let key = store.lookup_pubkey("nobody@example.com").await.unwrap();
        assert!(key.is_none(), "absent contact should return None");
    }
}
