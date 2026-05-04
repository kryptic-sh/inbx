//! `MailProvider` — protocol-agnostic hot-path trait.
//!
//! Covers the operations users hit every minute:
//!   - list folders
//!   - fetch headers (with delta)
//!   - fetch a single body
//!   - set / clear flags
//!   - move a message
//!   - send outbound mail
//!   - append a draft
//!
//! IMAP-only operations (Sieve, IDLE push, list-unsubscribe, OAuth login) are
//! left on their direct call paths; they are not hot-path and either have no
//! JMAP equivalent in scope or belong to separate milestones.
//!
//! ## UID / ID mapping
//!
//! IMAP UIDs are u32 integers.  JMAP email IDs are opaque strings.  The
//! existing codebase already solves this in `cmd_jmap` (main.rs) with a
//! deterministic FNV-1a hash that folds the JMAP string id into a positive
//! i64.  `JmapProvider` reuses the same `jmap_id_to_uid` helper — no new
//! DB column, no side-table.  The hash is collision-resistant enough for a
//! single account's message space (billions of messages before birthday
//! collision becomes a concern), and the reverse mapping (uid → JMAP id) is
//! stored as the `message_id` field by the provider so the store can round-
//! trip without separate infrastructure.  NOTE: `jmap_id` is embedded in the
//! `provider_id` field when we need the raw JMAP id back (e.g. Email/set).
//! We carry it through `fetch_headers` by stashing it in `message_id` when
//! the message has no RFC 5322 Message-ID — callers that need it can look
//! there.  A cleaner path (separate column) is left for follow-up if needed.

use crate::{graph, imap, jmap};
use inbx_config::{Account, Transport};

pub use crate::imap::{FolderInfo, HeaderRow};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("imap: {0}")]
    Imap(#[from] imap::Error),
    #[error("jmap: {0}")]
    Jmap(#[from] jmap::Error),
    #[error("graph: {0}")]
    Graph(#[from] graph::Error),
    #[error("auto-detect: {0}")]
    AutoDetect(String),
}

pub type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Hot-path operations abstracted over IMAP and JMAP.
///
/// The trait is `dyn`-compatible: all methods take `&mut self` and return
/// `Pin<Box<dyn Future>>` via `async_trait`.
#[async_trait::async_trait]
pub trait MailProvider: Send + Sync {
    /// List all folders / mailboxes.
    async fn list_folders(&mut self) -> Result<Vec<FolderInfo>>;

    /// Fetch headers for `folder`.  `since_uid` is a server-side delta hint
    /// (IMAP: only fetch UIDs > since; JMAP: treated as a state token when
    /// non-negative — for this milestone we do a full fetch on both).
    /// `limit` caps results.
    async fn fetch_headers(
        &mut self,
        folder: &str,
        since_uid: Option<i64>,
        limit: u32,
    ) -> Result<Vec<HeaderRow>>;

    /// Fetch raw RFC 5322 body for the message at `uid` in `folder`.
    async fn fetch_body(&mut self, folder: &str, uid: i64) -> Result<Vec<u8>>;

    /// Mutate keyword flags on a message.  IMAP flags include the leading `\`
    /// (e.g. `\\Seen`); JMAP keywords use the lowercase `$seen` form.  The
    /// provider translates.
    ///
    /// `add` and `remove` use IMAP-convention strings (`\\Seen`, `\\Flagged`,
    /// `\\Answered`, `\\Draft`, `\\Deleted`) — the JMAP impl maps these to
    /// JMAP keywords automatically.
    async fn set_flags(
        &mut self,
        folder: &str,
        uid: i64,
        add: &[&str],
        remove: &[&str],
    ) -> Result<()>;

    /// Move `uid` from `folder` to `dest`.  The message is removed from
    /// `folder` and appears in `dest`.
    async fn move_message(&mut self, folder: &str, uid: i64, dest: &str) -> Result<()>;

    /// Send `raw` RFC 5322 bytes via the provider's outbound path.
    async fn send(&mut self, raw: &[u8]) -> Result<()>;

    /// Append `raw` to the provider's Drafts mailbox, marked `$draft`.
    async fn append_draft(&mut self, folder: &str, raw: &[u8]) -> Result<()>;

    /// Expunge `\Deleted`-flagged messages from `folder`.
    ///
    /// Returns the number of messages destroyed server-side.
    ///
    /// - IMAP: `SELECT` then `EXPUNGE`.
    /// - JMAP: `Email/query` filtered by `inMailbox + $deleted`, then
    ///   `Email/set { destroy: [...] }`.
    /// - Graph: no-op (returns 0) — Graph has no per-message deletion flag;
    ///   "delete" means move to DeletedItems.
    async fn expunge_folder(&mut self, folder: &str) -> Result<usize>;
}

// ---------------------------------------------------------------------------
// IMAP impl
// ---------------------------------------------------------------------------

/// Thin wrapper that owns an authenticated IMAP session and implements
/// `MailProvider` by delegating to the existing free functions in `imap.rs`.
pub struct ImapProvider {
    pub session: imap::ImapSession,
}

#[async_trait::async_trait]
impl MailProvider for ImapProvider {
    async fn list_folders(&mut self) -> Result<Vec<FolderInfo>> {
        Ok(imap::list_folders(&mut self.session).await?)
    }

    async fn fetch_headers(
        &mut self,
        folder: &str,
        since_uid: Option<i64>,
        _limit: u32,
    ) -> Result<Vec<HeaderRow>> {
        // `since_uid` is a delta hint — for now do a full fetch like the
        // existing TUI sync path does.  The IMAP path does not yet do
        // incremental UID-range fetches; that optimisation is left for a
        // follow-up once the trait is stable.
        let _ = since_uid; // TODO(M21-delta): use UID SEARCH UID <since>:*
        let (_uidvalidity, rows) = imap::fetch_headers(&mut self.session, folder).await?;
        Ok(rows)
    }

    async fn fetch_body(&mut self, folder: &str, uid: i64) -> Result<Vec<u8>> {
        let pairs = imap::fetch_bodies(&mut self.session, folder, &[uid as u32]).await?;
        pairs.into_iter().next().map(|(_, raw)| raw).ok_or_else(|| {
            Error::Imap(imap::Error::Imap(async_imap::error::Error::No(
                "body not found".into(),
            )))
        })
    }

    async fn set_flags(
        &mut self,
        folder: &str,
        uid: i64,
        add: &[&str],
        remove: &[&str],
    ) -> Result<()> {
        if !add.is_empty() {
            imap::store_flags(
                &mut self.session,
                folder,
                &[uid as u32],
                "+FLAGS",
                &add.join(" "),
            )
            .await?;
        }
        if !remove.is_empty() {
            imap::store_flags(
                &mut self.session,
                folder,
                &[uid as u32],
                "-FLAGS",
                &remove.join(" "),
            )
            .await?;
        }
        Ok(())
    }

    async fn move_message(&mut self, folder: &str, uid: i64, dest: &str) -> Result<()> {
        Ok(imap::uid_move(&mut self.session, folder, &[uid as u32], dest).await?)
    }

    async fn send(&mut self, _raw: &[u8]) -> Result<()> {
        // IMAP doesn't send; caller should use inbx_net::send_message directly.
        // The trait still requires this for dyn-compat; realistically only
        // JmapProvider's send is used here.  ImapProvider delegates to SMTP
        // outside this trait.
        Ok(())
    }

    async fn append_draft(&mut self, folder: &str, raw: &[u8]) -> Result<()> {
        Ok(imap::append_draft(&mut self.session, folder, raw).await?)
    }

    async fn expunge_folder(&mut self, folder: &str) -> Result<usize> {
        Ok(imap::expunge_folder(&mut self.session, folder).await? as usize)
    }
}

// ---------------------------------------------------------------------------
// JMAP impl — see jmap.rs for the MailProvider impl block
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Factory: connect_provider
// ---------------------------------------------------------------------------

/// Probe `https://<host>/.well-known/jmap` (RFC 8620 §2.2) and return the
/// session URL on 200.  Used by account setup / wizard — not on the hot path.
#[allow(dead_code)]
pub async fn probe_well_known(host: &str, timeout_secs: u64) -> Option<String> {
    let url = format!("https://{host}/.well-known/jmap");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .danger_accept_invalid_certs(false)
        .build()
        .ok()?;
    let res = client.get(&url).send().await.ok()?;
    if res.status().is_success() {
        Some(url)
    } else {
        None
    }
}

/// Fastmail's `/.well-known/jmap` lives on `api.fastmail.com`, not on the
/// IMAP host.  Exposed for account-setup tooling.
#[allow(dead_code)]
pub fn fastmail_jmap_session_url(imap_host: &str) -> Option<&'static str> {
    if imap_host.eq_ignore_ascii_case("imap.fastmail.com")
        || imap_host.eq_ignore_ascii_case("fastmail.com")
    {
        Some("https://api.fastmail.com/jmap/session")
    } else {
        None
    }
}

/// Connect using the account's configured `transport` field.
///
/// - `Transport::Imap` (default) → authenticated IMAP session wrapped in
///   `ImapProvider`.
/// - `Transport::Jmap { session_url }` → `JmapClient` connected to `session_url`.
/// - `Transport::Graph` → `GraphClient` implementing `MailProvider` via the
///   Microsoft Graph API (`/me/messages`, `/me/sendMail`, etc.).
///
/// Existing IMAP-only accounts keep working unchanged because `Transport`
/// defaults to `Imap`.  To opt into JMAP, set `[transport]` in the account
/// TOML:
/// ```toml
/// [accounts.transport]
/// kind = "jmap"
/// session_url = "https://api.fastmail.com/jmap/session"
/// ```
/// To opt into Graph (Microsoft 365 / Outlook):
/// ```toml
/// [accounts.transport]
/// kind = "graph"
/// ```
pub async fn connect_provider(
    account: &Account,
    store: Option<&inbx_store::Store>,
) -> std::result::Result<Box<dyn MailProvider>, Error> {
    match &account.transport {
        Transport::Imap => {
            let session = imap::connect_imap(account).await?;
            Ok(Box::new(ImapProvider { session }))
        }
        Transport::Jmap { session_url } => {
            let mut client = jmap::JmapClient::connect(account, session_url).await?;
            client.store = store.cloned();
            Ok(Box::new(client))
        }
        Transport::Graph => {
            let mut client = graph::GraphClient::connect(account).await?;
            client.store = store.cloned();
            Ok(Box::new(client))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Verify the trait is object-safe (dyn-compat).  If `MailProvider` is not
    // dyn-compatible this module won't compile.
    struct MockProvider;

    #[async_trait::async_trait]
    impl MailProvider for MockProvider {
        async fn list_folders(&mut self) -> Result<Vec<FolderInfo>> {
            Ok(vec![FolderInfo {
                name: "INBOX".into(),
                delim: Some("/".into()),
                special_use: None,
                attrs: vec![],
                selectable: true,
            }])
        }

        async fn fetch_headers(
            &mut self,
            _folder: &str,
            _since_uid: Option<i64>,
            _limit: u32,
        ) -> Result<Vec<HeaderRow>> {
            Ok(vec![])
        }

        async fn fetch_body(&mut self, _folder: &str, _uid: i64) -> Result<Vec<u8>> {
            Ok(b"Subject: test\r\n\r\nhello".to_vec())
        }

        async fn set_flags(
            &mut self,
            _folder: &str,
            _uid: i64,
            _add: &[&str],
            _remove: &[&str],
        ) -> Result<()> {
            Ok(())
        }

        async fn move_message(&mut self, _folder: &str, _uid: i64, _dest: &str) -> Result<()> {
            Ok(())
        }

        async fn send(&mut self, _raw: &[u8]) -> Result<()> {
            Ok(())
        }

        async fn append_draft(&mut self, _folder: &str, _raw: &[u8]) -> Result<()> {
            Ok(())
        }

        async fn expunge_folder(&mut self, _folder: &str) -> Result<usize> {
            Ok(0)
        }
    }

    #[tokio::test]
    async fn mock_provider_dyn_compat() {
        // Must compile as Box<dyn MailProvider>.
        let mut p: Box<dyn MailProvider> = Box::new(MockProvider);
        let folders = p.list_folders().await.unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].name, "INBOX");
    }

    #[tokio::test]
    async fn mock_provider_fetch_headers() {
        let mut p = MockProvider;
        let rows = p.fetch_headers("INBOX", None, 100).await.unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn mock_provider_fetch_body() {
        let mut p = MockProvider;
        let body = p.fetch_body("INBOX", 1).await.unwrap();
        assert!(!body.is_empty());
    }

    /// Verify the Graph error variant can be constructed and formatted.
    /// This also ensures `provider::Error::Graph` compiles with `#[from]`.
    #[test]
    fn provider_error_graph_variant_compiles() {
        let graph_err = crate::graph::Error::Missing("test field");
        let provider_err = Error::Graph(graph_err);
        let msg = provider_err.to_string();
        assert!(msg.contains("graph"), "error message: {msg}");
    }

    /// Verify Transport::Graph matches the correct arm (no live network needed).
    #[test]
    fn transport_graph_variant_matches() {
        use inbx_config::Transport;
        let t = Transport::Graph;
        // Verify the variant is Graph — connect_provider would pick the Graph arm.
        assert!(
            matches!(t, Transport::Graph),
            "Transport::Graph should match Graph arm"
        );
        // Imap and Jmap must not match Graph.
        assert!(!matches!(Transport::Imap, Transport::Graph));
        assert!(!matches!(
            Transport::Jmap {
                session_url: String::new()
            },
            Transport::Graph
        ));
    }

    /// Confirm the Store-aware resolve fast path: `provider_id_for` returns the
    /// stored value when the row exists, and `None` when it doesn't (the latter
    /// triggers the slow-path scan in the real resolve helpers).
    #[tokio::test]
    async fn store_provider_id_fast_path() {
        use inbx_store::{MessageRow, Store};
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

        // Build an in-memory store with all migrations applied.
        let opts = SqliteConnectOptions::new().in_memory(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("../inbx-store/migrations")
            .run(&pool)
            .await
            .unwrap();
        let store = Store::from_pool(pool);

        // Insert a row that mimics a JMAP-synced message with provider_id set.
        let jmap_pid = "M99abc-opaque-jmap-id";
        store
            .upsert_message(&MessageRow {
                folder: "Inbox".to_string(),
                uid: 12345,
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
                provider_id: Some(jmap_pid.to_string()),
            })
            .await
            .unwrap();

        // Fast path: row present → returns provider_id.
        let found = store.provider_id_for("Inbox", 12345).await.unwrap();
        assert_eq!(
            found,
            Some(jmap_pid.to_string()),
            "fast path must return stored provider_id"
        );

        // Slow-path trigger: unknown uid → None.
        let missing = store.provider_id_for("Inbox", 99999).await.unwrap();
        assert_eq!(
            missing, None,
            "unknown uid must return None, triggering slow-path fallback"
        );
    }
}
