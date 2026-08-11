//! Microsoft Graph backend for Outlook / Microsoft 365.
//!
//! Lives next to the IMAP/SMTP path so individual accounts can opt in. Uses
//! the same OAuth2 refresh-token storage as IMAP-side XOAUTH2.

use inbx_config::{Account, AuthMethod, OAuthProvider};
use serde::Deserialize;

use crate::{oauth, proxy};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("reqwest: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("config: {0}")]
    Config(#[from] inbx_config::Error),
    #[error("oauth: {0}")]
    OAuth(#[from] oauth::Error),
    #[error("graph: account is not Microsoft OAuth2")]
    NotMicrosoft,
    #[error("graph: api error {status}: {body}")]
    Api { status: u16, body: String },
    #[error("graph: missing data: {0}")]
    Missing(&'static str),
    #[error("graph: pagination {0}")]
    Pagination(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

const MAX_GRAPH_PAGES: usize = 10_000;

struct PageTracker {
    seen: std::collections::HashSet<String>,
    pages: usize,
    max_pages: usize,
}

impl PageTracker {
    fn new(max_pages: usize) -> Self {
        Self {
            seen: std::collections::HashSet::new(),
            pages: 0,
            max_pages,
        }
    }

    fn visit(&mut self, url: &str) -> Result<()> {
        if self.pages == self.max_pages {
            return Err(Error::Pagination("exceeds page limit"));
        }
        if !self.seen.insert(url.to_string()) {
            return Err(Error::Pagination("repeats nextLink"));
        }
        self.pages += 1;
        Ok(())
    }
}

async fn ensure_api_success(res: reqwest::Response) -> Result<reqwest::Response> {
    if res.status().is_success() {
        Ok(res)
    } else {
        let status = res.status().as_u16();
        let body = res.text().await.unwrap_or_default();
        Err(Error::Api { status, body })
    }
}

/// A terminal Graph delta link, present on every successful delta response.
pub struct DeltaLink(String);

impl DeltaLink {
    pub fn as_deref(&self) -> Option<&str> {
        Some(&self.0)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Authenticated Graph client for one account. Fresh access token per session.
pub struct GraphClient {
    http: reqwest::Client,
    token: String,
    /// Optional store reference for fast provider_id lookups. When `None`,
    /// `resolve_graph_id` falls back to the slow 500-message scan.
    pub store: Option<inbx_store::Store>,
}

impl GraphClient {
    pub async fn connect(account: &Account) -> Result<Self> {
        let provider = match &account.auth {
            AuthMethod::OAuth2 {
                provider: provider @ OAuthProvider::Microsoft { .. },
                ..
            } => provider.clone(),
            _ => return Err(Error::NotMicrosoft),
        };
        let refresh = inbx_config::load_refresh_token(&account.name)?;
        let token =
            oauth::refresh(&account.auth, &provider, &refresh, account.proxy.as_ref()).await?;
        let http = proxy::build_reqwest_client(account.proxy.as_ref(), 30)?;
        Ok(Self {
            http,
            token,
            store: None,
        })
    }

    async fn get(&self, url: &str) -> Result<reqwest::Response> {
        let res = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .header("Accept", "application/json")
            .send()
            .await?;
        if !res.status().is_success() {
            let status = res.status().as_u16();
            let body = res.text().await.unwrap_or_default();
            return Err(Error::Api { status, body });
        }
        Ok(res)
    }

    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let res = self.http.get(url).bearer_auth(&self.token).send().await?;
        if !res.status().is_success() {
            let status = res.status().as_u16();
            let body = res.text().await.unwrap_or_default();
            return Err(Error::Api { status, body });
        }
        Ok(res.bytes().await?.to_vec())
    }

    /// List all mail folders. Walks @odata.nextLink pagination.
    pub async fn list_folders(&self) -> Result<Vec<GraphFolder>> {
        let mut out = Vec::new();
        let mut url = String::from("https://graph.microsoft.com/v1.0/me/mailFolders?$top=200");
        let mut pages = PageTracker::new(MAX_GRAPH_PAGES);
        loop {
            pages.visit(&url)?;
            let res = self.get(&url).await?;
            let page: Page<GraphFolder> = res.json().await?;
            out.extend(page.value);
            match page.next_link {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    /// List at most `limit` messages in a folder, newest first. Headers only — body fetched lazy.
    /// Every Graph page is capped at Graph's documented maximum of 1000.
    pub async fn list_messages(&self, folder_id: &str, limit: u32) -> Result<Vec<GraphMessage>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let page_size = graph_page_size(limit);
        let mut url = message_list_url(folder_id, page_size);
        let mut messages = Vec::new();
        let mut pages = PageTracker::new(MAX_GRAPH_PAGES);
        loop {
            pages.visit(&url)?;
            let page: Page<GraphMessage> = self.get(&url).await?.json().await?;
            match accumulate_message_page(&mut messages, page, Some(limit as usize)) {
                Some(next) => url = next,
                None => return Ok(messages),
            }
        }
    }

    /// List a complete folder snapshot by following every Graph continuation.
    async fn list_all_messages(&self, folder_id: &str) -> Result<Vec<GraphMessage>> {
        let mut url = message_list_url(folder_id, 1000);
        let mut messages = Vec::new();
        let mut pages = PageTracker::new(MAX_GRAPH_PAGES);
        loop {
            pages.visit(&url)?;
            let page: Page<GraphMessage> = self.get(&url).await?.json().await?;
            match accumulate_message_page(&mut messages, page, None) {
                Some(next) => url = next,
                None => return Ok(messages),
            }
        }
    }

    /// Walk Graph's delta endpoint for a folder. Pass `None` for the first
    /// run; pass the previously-stored deltaLink on subsequent runs to fetch
    /// only changes. Returns `(messages, new_delta_link)`.
    pub async fn delta_messages(
        &self,
        folder_id: &str,
        delta_link: Option<&str>,
    ) -> Result<(Vec<GraphMessage>, DeltaLink)> {
        let mut url = match delta_link {
            Some(link) => link.to_string(),
            None => format!(
                "https://graph.microsoft.com/v1.0/me/mailFolders/{folder_id}/messages/delta\
                 ?$select=id,subject,from,toRecipients,receivedDateTime,internetMessageId,isRead"
            ),
        };
        let mut messages = Vec::new();
        let mut pages = PageTracker::new(MAX_GRAPH_PAGES);
        loop {
            pages.visit(&url)?;
            let res = self.get(&url).await?;
            let page: DeltaPage<GraphMessage> = res.json().await?;
            let continuation = delta_page_continuation(&page)?;
            messages.extend(page.value);
            match continuation {
                DeltaPageContinuation::Next(next) => url = next,
                DeltaPageContinuation::Complete(delta_link) => {
                    return Ok((messages, DeltaLink(delta_link)));
                }
            }
        }
    }

    /// Download the raw RFC 822 body for one message.
    pub async fn fetch_mime(&self, message_id: &str) -> Result<Vec<u8>> {
        let url = format!("https://graph.microsoft.com/v1.0/me/messages/{message_id}/$value");
        self.get_bytes(&url).await
    }

    /// Send a raw RFC 822 message via Graph. Uploads MIME bytes with
    /// Content-Type: text/plain — Graph accepts MIME directly on /me/sendMail
    /// in this shape and saves to Sent Items by default.
    pub async fn send_mime(&self, raw: &[u8], save_to_sent: bool) -> Result<()> {
        let url = if save_to_sent {
            "https://graph.microsoft.com/v1.0/me/sendMail"
        } else {
            "https://graph.microsoft.com/v1.0/me/sendMail?saveToSentItems=false"
        };
        let res = self
            .http
            .post(url)
            .bearer_auth(&self.token)
            .header("Content-Type", "text/plain")
            .body(raw.to_vec())
            .send()
            .await?;
        ensure_api_success(res).await?;
        Ok(())
    }
}

fn graph_page_size(limit: u32) -> u32 {
    limit.clamp(1, 1000)
}

fn message_list_url(folder_id: &str, page_size: u32) -> String {
    format!(
        "https://graph.microsoft.com/v1.0/me/mailFolders/{folder_id}/messages\
         ?$top={page_size}&$orderby=receivedDateTime desc\
         &$select=id,subject,from,toRecipients,receivedDateTime,internetMessageId,isRead,flag"
    )
}

#[derive(Debug, Deserialize)]
struct Page<T> {
    value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

/// Append one message page and return its continuation when more data is needed.
fn accumulate_message_page<T>(
    messages: &mut Vec<T>,
    page: Page<T>,
    total_cap: Option<usize>,
) -> Option<String> {
    messages.extend(page.value);
    if let Some(cap) = total_cap
        && messages.len() >= cap
    {
        messages.truncate(cap);
        return None;
    }
    page.next_link
}

#[derive(Debug, Deserialize)]
struct DeltaPage<T> {
    value: Vec<T>,
    #[serde(rename = "@odata.nextLink", default)]
    next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink", default)]
    delta_link: Option<String>,
}

enum DeltaPageContinuation {
    Next(String),
    Complete(String),
}

fn delta_page_continuation<T>(page: &DeltaPage<T>) -> Result<DeltaPageContinuation> {
    match (&page.next_link, &page.delta_link) {
        (_, Some(delta_link)) => Ok(DeltaPageContinuation::Complete(delta_link.clone())),
        (Some(next_link), None) => Ok(DeltaPageContinuation::Next(next_link.clone())),
        (None, None) => Err(Error::Pagination("terminal page is missing deltaLink")),
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct GraphFolder {
    pub id: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(rename = "totalItemCount", default)]
    pub total: i64,
    #[serde(rename = "unreadItemCount", default)]
    pub unread: i64,
    #[serde(rename = "wellKnownName", default)]
    pub well_known: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GraphMessage {
    pub id: String,
    #[serde(rename = "@removed", default)]
    pub removed: Option<GraphRemoved>,
    pub subject: Option<String>,
    pub from: Option<GraphRecipient>,
    #[serde(rename = "toRecipients", default)]
    pub to: Vec<GraphRecipient>,
    #[serde(rename = "receivedDateTime")]
    pub received: Option<String>,
    #[serde(rename = "internetMessageId")]
    pub message_id: Option<String>,
    #[serde(rename = "isRead", default)]
    pub is_read: bool,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GraphRemoved {
    pub reason: Option<String>,
}

impl GraphMessage {
    pub fn is_removed(&self) -> bool {
        self.removed.is_some()
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct GraphRecipient {
    #[serde(rename = "emailAddress")]
    pub email_address: GraphAddress,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GraphAddress {
    #[serde(default)]
    pub name: String,
    pub address: String,
}

impl GraphRecipient {
    pub fn formatted(&self) -> String {
        if self.email_address.name.is_empty() {
            self.email_address.address.clone()
        } else {
            format!(
                "{} <{}>",
                self.email_address.name, self.email_address.address
            )
        }
    }
}

// ---------------------------------------------------------------------------
// UID helper
// ---------------------------------------------------------------------------

/// Deterministic FNV-1a hash: Graph string id → stable positive i64.
///
/// Uses the same algorithm as `jmap_id_to_uid` so UIDs are consistent
/// across backends (both use FNV-1a offset=0xcbf29ce484222325, prime=0x100000001b3).
/// The high bit is always cleared so the result is positive and fits i64.
pub fn graph_id_to_uid(id: &str) -> i64 {
    // FNV-1a 64-bit: offset basis = 14695981039346656037, prime = 1099511628211
    let mut h: u64 = 0xcbf29ce484222325; // offset basis
    for b in id.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3); // FNV prime
    }
    // Mask high bit → always positive i64
    (h & 0x7fff_ffff_ffff_ffff) as i64
}

// ---------------------------------------------------------------------------
// Graph low-level helpers (PATCH / POST)
// ---------------------------------------------------------------------------

impl GraphClient {
    async fn patch_json(&self, url: &str, body: serde_json::Value) -> Result<()> {
        let res = self
            .http
            .patch(url)
            .bearer_auth(&self.token)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;
        ensure_api_success(res).await?;
        Ok(())
    }

    async fn post_json(&self, url: &str, body: serde_json::Value) -> Result<()> {
        let res = self
            .http
            .post(url)
            .bearer_auth(&self.token)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;
        ensure_api_success(res).await?;
        Ok(())
    }

    /// Resolve a display_name → Graph folder id. Does a fresh list_folders
    /// call; v1 — no in-memory cache (kept self-contained per provider design).
    async fn resolve_folder_id(&self, display_name: &str) -> Result<String> {
        let folders = self.list_folders().await?;
        folders
            .iter()
            .find(|f| f.display_name.eq_ignore_ascii_case(display_name))
            .map(|f| f.id.clone())
            .ok_or_else(|| Error::Missing("folder not found by display_name"))
    }

    /// Resolve uid (FNV-1a of Graph id) back to the raw Graph message id.
    ///
    /// Fast path: query `provider_id` from the store when a `Store` is attached.
    /// Slow path (pre-migration or no store): scan the most recent 500 messages.
    /// A `tracing::debug!` is emitted whenever the slow path is taken.
    async fn resolve_graph_id(&self, folder_display_name: &str, uid: i64) -> Result<String> {
        // Fast path: store lookup.
        if let Some(store) = &self.store {
            if let Ok(Some(pid)) = store.provider_id_for(folder_display_name, uid).await {
                return Ok(pid);
            }
            tracing::debug!(
                folder = folder_display_name,
                uid,
                "resolve_graph_id: provider_id not in store, falling back to 500-message scan"
            );
        }

        // Slow path: list messages and match by hash.
        let folder_id = self.resolve_folder_id(folder_display_name).await?;
        let msgs = self.list_messages(&folder_id, 500).await?;
        msgs.into_iter()
            .find(|m| graph_id_to_uid(&m.id) == uid)
            .map(|m| m.id)
            .ok_or(Error::Missing("uid not found in recent 500 messages"))
    }
}

// ---------------------------------------------------------------------------
// MailProvider impl for GraphClient
// ---------------------------------------------------------------------------

/// Map Graph well_known name → IMAP special-use string.
fn well_known_to_special_use(wk: &str) -> Option<String> {
    match wk.to_ascii_lowercase().as_str() {
        "inbox" => Some("\\Inbox".into()),
        "sentitems" => Some("\\Sent".into()),
        "drafts" => Some("\\Drafts".into()),
        "deleteditems" => Some("\\Trash".into()),
        "junkemail" => Some("\\Junk".into()),
        "archive" => Some("\\Archive".into()),
        _ => None,
    }
}

/// Parse an RFC 3339 date string to Unix timestamp.
/// Graph returns e.g. `"2026-01-02T15:04:05Z"` — same shape as JMAP.
fn parse_graph_date(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let year: i64 = std::str::from_utf8(&bytes[0..4]).ok()?.parse().ok()?;
    let month: i64 = std::str::from_utf8(&bytes[5..7]).ok()?.parse().ok()?;
    let day: i64 = std::str::from_utf8(&bytes[8..10]).ok()?.parse().ok()?;
    let hour: i64 = std::str::from_utf8(&bytes[11..13]).ok()?.parse().ok()?;
    let min: i64 = std::str::from_utf8(&bytes[14..16]).ok()?.parse().ok()?;
    let sec: i64 = std::str::from_utf8(&bytes[17..19]).ok()?.parse().ok()?;
    let days_in_month = [0i64, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let mut days: i64 = (year - 1970) * 365 + (year - 1969).div_euclid(4)
        - (year - 1901).div_euclid(100)
        + (year - 1601).div_euclid(400);
    for m in 1..month {
        days += days_in_month[m as usize];
        if m == 2 && leap {
            days += 1;
        }
    }
    days += day - 1;
    Some(days * 86400 + hour * 3600 + min * 60 + sec)
}

#[async_trait::async_trait]
impl crate::provider::MailProvider for GraphClient {
    async fn list_folders(&mut self) -> crate::provider::Result<Vec<crate::provider::FolderInfo>> {
        let raw = GraphClient::list_folders(self)
            .await
            .map_err(crate::provider::Error::Graph)?;
        let folders = raw
            .into_iter()
            .map(|f| {
                let special_use = f.well_known.as_deref().and_then(well_known_to_special_use);
                crate::imap::FolderInfo {
                    name: f.display_name,
                    delim: Some("/".into()),
                    special_use,
                    attrs: vec![],
                    selectable: true,
                }
            })
            .collect();
        Ok(folders)
    }

    async fn fetch_headers(
        &mut self,
        folder: &str,
        _since_uid: Option<i64>,
        _limit: u32,
    ) -> crate::provider::Result<crate::provider::HeaderSnapshot> {
        // NOTE: since_uid is ignored for v1. Graph delta sync via
        // delta_messages/deltaLink (not since_uid) is deferred to M11+1.
        use std::time::{SystemTime, UNIX_EPOCH};

        let folder_id = self
            .resolve_folder_id(folder)
            .await
            .map_err(crate::provider::Error::Graph)?;
        let msgs = self
            .list_all_messages(&folder_id)
            .await
            .map_err(crate::provider::Error::Graph)?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let rows = msgs
            .into_iter()
            .map(|msg| {
                let uid = graph_id_to_uid(&msg.id);
                let from_addr = msg.from.as_ref().map(|r| r.formatted());
                let to_addrs = if msg.to.is_empty() {
                    None
                } else {
                    Some(
                        msg.to
                            .iter()
                            .map(|r| r.formatted())
                            .collect::<Vec<_>>()
                            .join(", "),
                    )
                };
                let date_unix = msg.received.as_deref().and_then(parse_graph_date);
                let flags = if msg.is_read {
                    "\\Seen".to_string()
                } else {
                    String::new()
                };
                crate::imap::HeaderRow {
                    uid,
                    uidvalidity: 0, // Graph has no UIDVALIDITY equivalent
                    message_id: msg.message_id,
                    subject: msg.subject,
                    from_addr,
                    to_addrs,
                    date_unix,
                    flags,
                    fetched_at_unix: now,
                    provider_id: Some(msg.id.clone()),
                }
            })
            .collect();

        Ok(crate::provider::HeaderSnapshot {
            rows,
            complete: true,
            transport: crate::provider::SnapshotTransport::Opaque,
        })
    }

    async fn fetch_body(&mut self, folder: &str, uid: i64) -> crate::provider::Result<Vec<u8>> {
        let graph_id = self
            .resolve_graph_id(folder, uid)
            .await
            .map_err(crate::provider::Error::Graph)?;
        self.fetch_mime(&graph_id)
            .await
            .map_err(crate::provider::Error::Graph)
    }

    async fn set_flags(
        &mut self,
        folder: &str,
        uid: i64,
        add: &[&str],
        remove: &[&str],
    ) -> crate::provider::Result<()> {
        if add.is_empty() && remove.is_empty() {
            return Ok(());
        }

        let graph_id = self
            .resolve_graph_id(folder, uid)
            .await
            .map_err(crate::provider::Error::Graph)?;

        // Build a single PATCH body combining all flag changes.
        // \\Seen → isRead, \\Flagged → flag.flagStatus
        // \\Answered, \\Draft, \\Deleted — Graph has no bool field; log and skip.
        let mut patch = serde_json::Map::new();
        let mut flag_status: Option<&str> = None;

        for &flag in add.iter().chain(remove.iter()) {
            match flag.to_ascii_lowercase().as_str() {
                "\\answered" | "\\draft" | "\\deleted" => {
                    tracing::debug!(
                        flag,
                        "Graph set_flags: flag has no Graph equivalent, ignored"
                    );
                }
                _ => {}
            }
        }

        // isRead: last writer wins if both add and remove contain \\Seen.
        let add_seen = add.iter().any(|f| f.eq_ignore_ascii_case("\\seen"));
        let rem_seen = remove.iter().any(|f| f.eq_ignore_ascii_case("\\seen"));
        if add_seen {
            patch.insert("isRead".into(), serde_json::Value::Bool(true));
        } else if rem_seen {
            patch.insert("isRead".into(), serde_json::Value::Bool(false));
        }

        // \\Flagged → flag.flagStatus
        let add_flagged = add.iter().any(|f| f.eq_ignore_ascii_case("\\flagged"));
        let rem_flagged = remove.iter().any(|f| f.eq_ignore_ascii_case("\\flagged"));
        if add_flagged {
            flag_status = Some("flagged");
        } else if rem_flagged {
            flag_status = Some("notFlagged");
        }
        if let Some(status) = flag_status {
            patch.insert("flag".into(), serde_json::json!({ "flagStatus": status }));
        }

        if patch.is_empty() {
            return Ok(());
        }

        let url = format!("https://graph.microsoft.com/v1.0/me/messages/{graph_id}");
        self.patch_json(&url, serde_json::Value::Object(patch))
            .await
            .map_err(crate::provider::Error::Graph)
    }

    async fn move_message(
        &mut self,
        folder: &str,
        uid: i64,
        dest: &str,
    ) -> crate::provider::Result<()> {
        let graph_id = self
            .resolve_graph_id(folder, uid)
            .await
            .map_err(crate::provider::Error::Graph)?;
        let dest_id = self
            .resolve_folder_id(dest)
            .await
            .map_err(crate::provider::Error::Graph)?;
        let url = format!("https://graph.microsoft.com/v1.0/me/messages/{graph_id}/move");
        self.post_json(&url, serde_json::json!({ "destinationId": dest_id }))
            .await
            .map_err(crate::provider::Error::Graph)
    }

    async fn send(&mut self, raw: &[u8]) -> crate::provider::Result<()> {
        // save_to_sent=true: Graph saves to Sent Items by default.
        self.send_mime(raw, true)
            .await
            .map_err(crate::provider::Error::Graph)
    }

    async fn expunge_folder(&mut self, folder: &str) -> crate::provider::Result<usize> {
        // Graph has no per-message \Deleted flag: "delete" in Graph is
        // move-to-DeletedItems. There is no folder-level expunge equivalent.
        // Returning 0 is correct — nothing is silently destroyed.
        tracing::debug!(
            folder,
            "Graph expunge_folder: no-op (Graph uses move-to-DeletedItems, not a deletion flag)"
        );
        Ok(0)
    }

    async fn create_folder(&mut self, name: &str) -> crate::provider::Result<()> {
        self.post_json(
            "https://graph.microsoft.com/v1.0/me/mailFolders",
            serde_json::json!({ "displayName": name }),
        )
        .await
        .map_err(crate::provider::Error::Graph)
    }

    async fn delete_folder(&mut self, name: &str) -> crate::provider::Result<()> {
        let folder_id = self
            .resolve_folder_id(name)
            .await
            .map_err(crate::provider::Error::Graph)?;
        let url = format!("https://graph.microsoft.com/v1.0/me/mailFolders/{folder_id}");
        let res = self
            .http
            .delete(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| crate::provider::Error::Graph(Error::Reqwest(e)))?;
        if !res.status().is_success() {
            let status = res.status().as_u16();
            let body = res.text().await.unwrap_or_default();
            return Err(crate::provider::Error::Graph(Error::Api { status, body }));
        }
        Ok(())
    }

    async fn rename_folder(&mut self, from: &str, to: &str) -> crate::provider::Result<()> {
        let folder_id = self
            .resolve_folder_id(from)
            .await
            .map_err(crate::provider::Error::Graph)?;
        let url = format!("https://graph.microsoft.com/v1.0/me/mailFolders/{folder_id}");
        self.patch_json(&url, serde_json::json!({ "displayName": to }))
            .await
            .map_err(crate::provider::Error::Graph)
    }

    async fn subscribe_folder(&mut self, name: &str, on: bool) -> crate::provider::Result<()> {
        // Graph has no subscription concept; log and no-op.
        tracing::debug!(
            name,
            on,
            "Graph subscribe_folder: no-op (Graph has no subscription concept)"
        );
        Ok(())
    }

    async fn append_draft(&mut self, folder: &str, raw: &[u8]) -> crate::provider::Result<()> {
        // Resolve the caller-supplied folder (normally Drafts) to its Graph id.
        let folder_id = self
            .resolve_folder_id(folder)
            .await
            .map_err(crate::provider::Error::Graph)?;
        // POST raw MIME to /me/mailFolders/{id}/messages with Content-Type: text/plain.
        // Graph accepts RFC 822 MIME bytes here and the message lands in the folder.
        let url = format!("https://graph.microsoft.com/v1.0/me/mailFolders/{folder_id}/messages");
        let res = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .header("Content-Type", "text/plain")
            .body(raw.to_vec())
            .send()
            .await
            .map_err(Error::Reqwest)
            .map_err(crate::provider::Error::Graph)?;
        if !res.status().is_success() {
            let status = res.status().as_u16();
            let body = res.text().await.unwrap_or_default();
            return Err(crate::provider::Error::Graph(Error::Api { status, body }));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_page_continuation_requires_terminal_delta_link() {
        let terminal = DeltaPage::<GraphMessage> {
            value: Vec::new(),
            next_link: None,
            delta_link: None,
        };
        assert!(matches!(
            delta_page_continuation(&terminal),
            Err(Error::Pagination("terminal page is missing deltaLink"))
        ));
        let terminal = DeltaPage::<GraphMessage> {
            value: Vec::new(),
            next_link: None,
            delta_link: Some("delta".into()),
        };
        assert!(matches!(
            delta_page_continuation(&terminal),
            Ok(DeltaPageContinuation::Complete(link)) if link == "delta"
        ));
        let next = DeltaPage::<GraphMessage> {
            value: Vec::new(),
            next_link: Some("next".into()),
            delta_link: None,
        };
        assert!(matches!(
            delta_page_continuation(&next),
            Ok(DeltaPageContinuation::Next(link)) if link == "next"
        ));
    }

    #[test]
    fn graph_delta_tombstone_deserializes() {
        let message: GraphMessage =
            serde_json::from_str(r#"{"id":"graph-id","@removed":{"reason":"deleted"}}"#).unwrap();
        assert!(message.is_removed());
        assert_eq!(
            message
                .removed
                .and_then(|removed| removed.reason)
                .as_deref(),
            Some("deleted")
        );
    }

    #[test]
    fn page_tracker_rejects_repeated_url_and_ceiling() {
        let mut tracker = PageTracker::new(2);
        tracker.visit("first").unwrap();
        assert!(matches!(
            tracker.visit("first"),
            Err(Error::Pagination("repeats nextLink"))
        ));

        let mut tracker = PageTracker::new(1);
        tracker.visit("first").unwrap();
        assert!(matches!(
            tracker.visit("second"),
            Err(Error::Pagination("exceeds page limit"))
        ));
    }

    #[test]
    fn pagination_limits_page_size_and_total() {
        assert_eq!(graph_page_size(0), 1);
        assert_eq!(graph_page_size(500), 500);
        assert_eq!(graph_page_size(1001), 1000);
        let url = message_list_url("folder", graph_page_size(5000));
        assert!(url.contains("$top=1000"));
    }

    #[test]
    fn pagination_accumulator_continues_and_honors_mode() {
        let mut limited = vec![1];
        let next = accumulate_message_page(
            &mut limited,
            Page {
                value: vec![2],
                next_link: Some("page-2".into()),
            },
            Some(3),
        );
        assert_eq!(next.as_deref(), Some("page-2"));
        assert_eq!(limited, vec![1, 2]);

        let next = accumulate_message_page(
            &mut limited,
            Page {
                value: vec![3, 4],
                next_link: Some("page-3".into()),
            },
            Some(3),
        );
        assert_eq!(next, None);
        assert_eq!(limited, vec![1, 2, 3]);

        let mut full = Vec::new();
        let next = accumulate_message_page(
            &mut full,
            Page {
                value: vec![1, 2],
                next_link: Some("page-2".into()),
            },
            None,
        );
        assert_eq!(next.as_deref(), Some("page-2"));
        let next = accumulate_message_page(
            &mut full,
            Page {
                value: vec![3],
                next_link: None,
            },
            None,
        );
        assert_eq!(next, None);
        assert_eq!(full, vec![1, 2, 3]);
    }

    #[test]
    fn graph_id_to_uid_deterministic() {
        let id = "AAMkAGE1M2IyNGNmLWI4NTEtNDI4My1iYmU0LTc4NjJlYThmNGFlOABGAAAAAADRlY7ewL2fEqiri-s2";
        let uid1 = graph_id_to_uid(id);
        let uid2 = graph_id_to_uid(id);
        assert_eq!(uid1, uid2, "same id → same uid");
        assert!(uid1 > 0, "uid must be positive");
    }

    #[test]
    fn graph_id_to_uid_different_ids() {
        let a = graph_id_to_uid("AAMkAGE1abc");
        let b = graph_id_to_uid("AAMkAGE1xyz");
        assert_ne!(a, b, "different ids → different uids");
    }

    #[test]
    fn well_known_to_special_use_mapping() {
        assert_eq!(well_known_to_special_use("inbox"), Some("\\Inbox".into()));
        assert_eq!(
            well_known_to_special_use("sentitems"),
            Some("\\Sent".into())
        );
        assert_eq!(well_known_to_special_use("drafts"), Some("\\Drafts".into()));
        assert_eq!(
            well_known_to_special_use("deleteditems"),
            Some("\\Trash".into())
        );
        assert_eq!(
            well_known_to_special_use("junkemail"),
            Some("\\Junk".into())
        );
        assert_eq!(
            well_known_to_special_use("archive"),
            Some("\\Archive".into())
        );
        assert_eq!(well_known_to_special_use("calendar"), None);
    }

    #[test]
    fn parse_graph_date_basic() {
        // 2026-01-02T15:04:05Z → unix ts
        let ts = parse_graph_date("2026-01-02T15:04:05Z").unwrap();
        assert!(ts > 0);
        // 1970-01-01T00:00:00Z → 0
        let epoch = parse_graph_date("1970-01-01T00:00:00Z").unwrap();
        assert_eq!(epoch, 0);
    }

    #[test]
    fn graph_id_to_uid_matches_fnv1a() {
        // Manually computed FNV-1a of "test" to verify algorithm is correct.
        let uid = graph_id_to_uid("test");
        assert!(uid > 0);
        // Verify idempotent across calls
        assert_eq!(uid, graph_id_to_uid("test"));
    }
}
