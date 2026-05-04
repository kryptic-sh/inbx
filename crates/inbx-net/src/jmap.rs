//! Minimal JMAP (RFC 8620 / RFC 8621) client.
//!
//! Hand-rolled over reqwest because jmap-client crates churn fast. Targets
//! Fastmail / Stalwart. Auth is HTTP basic with the account's app password
//! (Bearer-token / OAuth wiring lives in the OAuth module and can attach
//! later). Implements the bare slice we need to fetch headers and send
//! mail; everything else (push, vacation, Sieve mgmt) lives in the
//! provider's own protocol path.

use std::pin::Pin;

use bytes::Bytes;
use futures_util::Stream;
use futures_util::StreamExt as _;
use inbx_config::{Account, AuthMethod};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{oauth, proxy};

/// Wrapper around a chunked SSE response. `next_event` strips the SSE
/// envelope (`event:` / `data:` / blank-line delimiter) and returns each
/// JSON state-change payload. Returns `Ok(None)` on stream close.
pub struct EventStream {
    inner: Pin<Box<dyn Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send>>,
    buf: Vec<u8>,
}

impl EventStream {
    pub async fn next_event(&mut self) -> Result<Option<String>> {
        loop {
            // Look for a complete SSE record (terminated by blank line `\n\n`).
            if let Some(end) = find_blank_line(&self.buf) {
                let raw = self.buf.drain(..end).collect::<Vec<u8>>();
                // Drop the blank-line terminator.
                let consume = if self.buf.starts_with(b"\r\n\r\n") {
                    4
                } else {
                    2
                };
                if self.buf.len() >= consume {
                    self.buf.drain(..consume);
                }
                let text = String::from_utf8_lossy(&raw);
                let mut data = String::new();
                for line in text.lines() {
                    if let Some(rest) = line.strip_prefix("data:") {
                        if !data.is_empty() {
                            data.push('\n');
                        }
                        data.push_str(rest.trim_start());
                    }
                }
                if !data.is_empty() {
                    return Ok(Some(data));
                }
                continue;
            }
            match self.inner.next().await {
                Some(Ok(chunk)) => self.buf.extend_from_slice(&chunk),
                Some(Err(e)) => return Err(Error::Reqwest(e)),
                None => return Ok(None),
            }
        }
    }
}

fn find_blank_line(buf: &[u8]) -> Option<usize> {
    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
        return Some(p);
    }
    buf.windows(2).position(|w| w == b"\n\n")
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("reqwest: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("config: {0}")]
    Config(#[from] inbx_config::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("server {status}: {body}")]
    Server { status: u16, body: String },
    #[error("missing account id in JMAP session")]
    NoAccountId,
    #[error("only AppPassword auth supported by this JMAP client")]
    UnsupportedAuth,
    #[error("oauth: {0}")]
    OAuth(#[from] oauth::Error),
}

/// Either basic auth (app password) or Bearer (OAuth2 access token).
#[derive(Debug, Clone)]
enum JmapAuth {
    Basic { user: String, password: String },
    Bearer(String),
}

pub type Result<T> = std::result::Result<T, Error>;

const MAIL_CAPABILITY: &str = "urn:ietf:params:jmap:mail";
const SUBMISSION_CAPABILITY: &str = "urn:ietf:params:jmap:submission";
const CORE_CAPABILITY: &str = "urn:ietf:params:jmap:core";

/// JMAP session document — only the fields we use are kept typed.
#[derive(Debug, Clone, Deserialize)]
pub struct Session {
    #[serde(rename = "apiUrl")]
    pub api_url: String,
    #[serde(rename = "primaryAccounts", default)]
    pub primary_accounts: serde_json::Map<String, Value>,
    #[serde(rename = "uploadUrl", default)]
    pub upload_url: Option<String>,
    #[serde(rename = "eventSourceUrl", default)]
    pub event_source_url: Option<String>,
}

impl Session {
    pub fn account_id_for(&self, capability: &str) -> Option<&str> {
        self.primary_accounts
            .get(capability)
            .and_then(|v| v.as_str())
    }
}

pub struct JmapClient {
    http: reqwest::Client,
    auth: JmapAuth,
    pub session: Session,
    pub account_id: String,
    /// Optional store reference for fast provider_id lookups. When `None`,
    /// `resolve_jmap_id` falls back to the slow 500-message scan.
    pub store: Option<inbx_store::Store>,
}

impl JmapClient {
    /// `session_url` is typically the JMAP host's `/.well-known/jmap`
    /// (Fastmail: `https://api.fastmail.com/jmap/session`).
    pub async fn connect(account: &Account, session_url: &str) -> Result<Self> {
        let auth = match &account.auth {
            AuthMethod::AppPassword => JmapAuth::Basic {
                user: account.username.clone(),
                password: inbx_config::load_password(&account.name)?,
            },
            AuthMethod::OAuth2 { provider, .. } => {
                let refresh = inbx_config::load_refresh_token(&account.name)?;
                let access =
                    oauth::refresh(&account.auth, provider, &refresh, account.proxy.as_ref())
                        .await?;
                JmapAuth::Bearer(access)
            }
        };
        let http = proxy::build_reqwest_client(account.proxy.as_ref(), 30)?;
        let res = apply_auth(http.get(session_url), &auth).send().await?;
        if !res.status().is_success() {
            let status = res.status().as_u16();
            let body = res.text().await.unwrap_or_default();
            return Err(Error::Server { status, body });
        }
        let session: Session = res.json().await?;
        let account_id = session
            .account_id_for(MAIL_CAPABILITY)
            .ok_or(Error::NoAccountId)?
            .to_string();
        Ok(Self {
            http,
            auth,
            session,
            account_id,
            store: None,
        })
    }

    async fn invoke(&self, methods: Vec<Value>, using: Vec<&str>) -> Result<Value> {
        let body = json!({ "using": using, "methodCalls": methods });
        let req = apply_auth(self.http.post(&self.session.api_url), &self.auth).json(&body);
        let res = req.send().await?;
        if !res.status().is_success() {
            let status = res.status().as_u16();
            let body = res.text().await.unwrap_or_default();
            return Err(Error::Server { status, body });
        }
        Ok(res.json().await?)
    }

    pub async fn list_mailboxes(&self) -> Result<Vec<Mailbox>> {
        let v = self
            .invoke(
                vec![json!([
                    "Mailbox/get",
                    {"accountId": self.account_id},
                    "0"
                ])],
                vec![CORE_CAPABILITY, MAIL_CAPABILITY],
            )
            .await?;
        let list: Vec<Mailbox> =
            serde_json::from_value(v["methodResponses"][0][1]["list"].clone())?;
        Ok(list)
    }

    pub async fn fetch_inbox_headers(&self, limit: u32) -> Result<Vec<EmailHeader>> {
        let mailboxes = self.list_mailboxes().await?;
        let inbox = mailboxes
            .iter()
            .find(|m| m.role.as_deref() == Some("inbox"))
            .or_else(|| {
                mailboxes
                    .iter()
                    .find(|m| m.name.eq_ignore_ascii_case("Inbox"))
            })
            .ok_or(Error::Server {
                status: 0,
                body: "no Inbox mailbox".into(),
            })?;
        let v = self
            .invoke(
                vec![
                    json!([
                        "Email/query",
                        {
                            "accountId": self.account_id,
                            "filter": { "inMailbox": inbox.id },
                            "sort": [ {"property": "receivedAt", "isAscending": false} ],
                            "limit": limit,
                        },
                        "q"
                    ]),
                    json!([
                        "Email/get",
                        {
                            "accountId": self.account_id,
                            "#ids": {
                                "resultOf": "q",
                                "name": "Email/query",
                                "path": "/ids"
                            },
                            "properties": [
                                "id","subject","from","to","receivedAt","messageId","keywords"
                            ]
                        },
                        "g"
                    ]),
                ],
                vec![CORE_CAPABILITY, MAIL_CAPABILITY],
            )
            .await?;
        let list = v["methodResponses"][1][1]["list"].clone();
        let emails: Vec<EmailHeader> = serde_json::from_value(list)?;
        Ok(emails)
    }

    /// Open the JMAP EventSource (RFC 8620 §7.3) stream and yield one
    /// notification per state-change line. The stream stays open until
    /// the server closes it or the caller drops the future.
    pub async fn open_event_source(&self) -> Result<EventStream> {
        let raw = self
            .session
            .event_source_url
            .as_deref()
            .ok_or(Error::Server {
                status: 0,
                body: "session has no eventSourceUrl".into(),
            })?;
        // Some implementations template `{types}`/`{closeafter}`/`{ping}`.
        let url = raw
            .replace("{types}", "Email")
            .replace("{closeafter}", "no")
            .replace("{ping}", "30");
        let res = apply_auth(self.http.get(&url), &self.auth)
            .header("Accept", "text/event-stream")
            .send()
            .await?;
        if !res.status().is_success() {
            let status = res.status().as_u16();
            let body = res.text().await.unwrap_or_default();
            return Err(Error::Server { status, body });
        }
        Ok(EventStream {
            inner: Box::pin(res.bytes_stream()),
            buf: Vec::new(),
        })
    }

    /// Email/changes — pass the previously-stored state. Returns the new
    /// state plus created/updated/destroyed Email ids since.
    pub async fn changes(&self, since_state: &str) -> Result<EmailChanges> {
        let v = self
            .invoke(
                vec![json!([
                    "Email/changes",
                    {"accountId": self.account_id, "sinceState": since_state},
                    "c"
                ])],
                vec![CORE_CAPABILITY, MAIL_CAPABILITY],
            )
            .await?;
        let resp = &v["methodResponses"][0][1];
        Ok(EmailChanges {
            new_state: resp["newState"].as_str().unwrap_or_default().to_string(),
            created: as_id_vec(&resp["created"]),
            updated: as_id_vec(&resp["updated"]),
            destroyed: as_id_vec(&resp["destroyed"]),
            has_more_changes: resp["hasMoreChanges"].as_bool().unwrap_or(false),
        })
    }

    /// First-time state probe — Email/get on no ids just to grab `state`.
    pub async fn current_state(&self) -> Result<String> {
        let v = self
            .invoke(
                vec![json!([
                    "Email/get",
                    {"accountId": self.account_id, "ids": []},
                    "s"
                ])],
                vec![CORE_CAPABILITY, MAIL_CAPABILITY],
            )
            .await?;
        Ok(v["methodResponses"][0][1]["state"]
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    /// Hydrate Email headers for the listed ids.
    pub async fn fetch_by_ids(&self, ids: &[String]) -> Result<Vec<EmailHeader>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let v = self
            .invoke(
                vec![json!([
                    "Email/get",
                    {
                        "accountId": self.account_id,
                        "ids": ids,
                        "properties": [
                            "id","subject","from","to","receivedAt","messageId","keywords"
                        ]
                    },
                    "g"
                ])],
                vec![CORE_CAPABILITY, MAIL_CAPABILITY],
            )
            .await?;
        let list = v["methodResponses"][0][1]["list"].clone();
        Ok(serde_json::from_value(list)?)
    }

    /// Upload a raw RFC 5322 blob and submit it via Email/import +
    /// EmailSubmission/set. Stalwart and Fastmail both accept this.
    pub async fn send_mime(&self, raw: &[u8]) -> Result<()> {
        let upload_url = self
            .session
            .upload_url
            .as_deref()
            .ok_or(Error::Server {
                status: 0,
                body: "session has no uploadUrl".into(),
            })?
            .replace("{accountId}", &self.account_id);
        let upload: Value = apply_auth(self.http.post(&upload_url), &self.auth)
            .header("Content-Type", "message/rfc822")
            .body(raw.to_vec())
            .send()
            .await?
            .json()
            .await?;
        let blob_id = upload["blobId"].as_str().ok_or(Error::Server {
            status: 0,
            body: "upload missing blobId".into(),
        })?;

        let mailboxes = self.list_mailboxes().await?;
        let drafts_id = mailboxes
            .iter()
            .find(|m| m.role.as_deref() == Some("drafts"))
            .map(|m| m.id.clone())
            .or_else(|| {
                mailboxes
                    .iter()
                    .find(|m| m.name.eq_ignore_ascii_case("Drafts"))
                    .map(|m| m.id.clone())
            })
            .unwrap_or_default();

        let _ = self
            .invoke(
                vec![
                    json!([
                        "Email/import",
                        {
                            "accountId": self.account_id,
                            "emails": {
                                "ev": {
                                    "blobId": blob_id,
                                    "mailboxIds": { drafts_id: true },
                                    "keywords": { "$draft": true }
                                }
                            }
                        },
                        "i"
                    ]),
                    json!([
                        "EmailSubmission/set",
                        {
                            "accountId": self.account_id,
                            "create": {
                                "s": {
                                    "emailId": "#ev",
                                    "envelope": null
                                }
                            },
                            "onSuccessDestroyEmail": ["#s"]
                        },
                        "s"
                    ]),
                ],
                vec![CORE_CAPABILITY, MAIL_CAPABILITY, SUBMISSION_CAPABILITY],
            )
            .await?;
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Mailbox {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(rename = "totalEmails", default)]
    pub total: i64,
    #[serde(rename = "unreadEmails", default)]
    pub unread: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmailHeader {
    pub id: String,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub from: Option<Vec<EmailAddress>>,
    #[serde(default)]
    pub to: Option<Vec<EmailAddress>>,
    #[serde(rename = "receivedAt", default)]
    pub received_at: Option<String>,
    #[serde(rename = "messageId", default)]
    pub message_id: Option<Vec<String>>,
    #[serde(default)]
    pub keywords: Option<serde_json::Map<String, Value>>,
}

impl EmailHeader {
    pub fn is_seen(&self) -> bool {
        self.keywords
            .as_ref()
            .map(|m| m.get("$seen").is_some_and(|v| v.as_bool().unwrap_or(false)))
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmailAddress {
    #[serde(default)]
    pub name: Option<String>,
    pub email: String,
}

fn as_id_vec(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone)]
pub struct EmailChanges {
    pub new_state: String,
    pub created: Vec<String>,
    pub updated: Vec<String>,
    pub destroyed: Vec<String>,
    pub has_more_changes: bool,
}

fn apply_auth(builder: reqwest::RequestBuilder, auth: &JmapAuth) -> reqwest::RequestBuilder {
    match auth {
        JmapAuth::Basic { user, password } => builder.basic_auth(user, Some(password)),
        JmapAuth::Bearer(token) => builder.bearer_auth(token),
    }
}

impl EmailAddress {
    pub fn formatted(&self) -> String {
        match self.name.as_deref() {
            Some(n) if !n.is_empty() => format!("{n} <{}>", self.email),
            _ => self.email.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// MailProvider impl for JmapClient
// ---------------------------------------------------------------------------

/// Deterministic FNV-1a hash: JMAP string id → stable positive i64.
///
/// Uses the same algorithm as `jmap_uid` in `apps/inbx/src/main.rs` so that
/// UIDs produced by the provider and by the CLI subcommand are identical.
pub fn jmap_id_to_uid(id: &str) -> i64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in id.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (h & 0x7fff_ffff_ffff_ffff) as i64
}

/// Map IMAP flag string (`\\Seen`) to JMAP keyword (`$seen`).
fn imap_flag_to_jmap(flag: &str) -> Option<&'static str> {
    match flag.to_ascii_lowercase().as_str() {
        "\\seen" => Some("$seen"),
        "\\flagged" => Some("$flagged"),
        "\\answered" => Some("$answered"),
        "\\draft" => Some("$draft"),
        "\\deleted" => Some("$deleted"),
        _ => None,
    }
}

/// JMAP `Email/get` — fetch raw RFC 5322 via Blob/download (lossless).
impl JmapClient {
    /// Resolve a JMAP email id to its blobId via `Email/get`, then download
    /// the raw RFC 5322 blob via the `downloadUrl` template.
    pub async fn fetch_raw_blob(&self, jmap_id: &str) -> Result<Vec<u8>> {
        // Step 1: get the blobId.
        let v = self
            .invoke(
                vec![json!([
                    "Email/get",
                    {
                        "accountId": self.account_id,
                        "ids": [jmap_id],
                        "properties": ["blobId"]
                    },
                    "b"
                ])],
                vec![CORE_CAPABILITY, MAIL_CAPABILITY],
            )
            .await?;
        let blob_id = v["methodResponses"][0][1]["list"][0]["blobId"]
            .as_str()
            .ok_or(Error::Server {
                status: 0,
                body: format!("no blobId for JMAP id {jmap_id}"),
            })?
            .to_string();

        // Step 2: download via downloadUrl template.
        // The Session struct doesn't expose downloadUrl yet — derive it
        // from the apiUrl.  Fastmail: api.fastmail.com/jmap/download/…
        // Stalwart: same pattern.  We fall back to an RFC 8620 §6.2 standard path.
        let res = apply_auth(
            self.http.get(format!(
                "{}/download/{}/{}/{}",
                self.session
                    .api_url
                    .trim_end_matches("/jmap/api")
                    .trim_end_matches("/api"),
                self.account_id,
                blob_id,
                "message.eml",
            )),
            &self.auth,
        )
        .send()
        .await;

        // If the constructed URL fails, fall back to Email/get with full body
        // properties.  This is lossy but always works.
        match res {
            Ok(r) if r.status().is_success() => Ok(r.bytes().await?.to_vec()),
            _ => {
                // Fallback: Email/get with bodyValues + fetchAllBodyValues.
                self.fetch_body_via_email_get(jmap_id).await
            }
        }
    }

    /// Download via the session's `downloadUrl` template (preferred, lossless).
    pub async fn fetch_raw_blob_via_template(
        &self,
        blob_id: &str,
        download_url_tmpl: &str,
    ) -> Result<Vec<u8>> {
        let url = download_url_tmpl
            .replace("{accountId}", &self.account_id)
            .replace("{blobId}", blob_id)
            .replace("{type}", "message%2Frfc822")
            .replace("{name}", "message.eml");
        let res = apply_auth(self.http.get(&url), &self.auth).send().await?;
        if !res.status().is_success() {
            let status = res.status().as_u16();
            let body = res.text().await.unwrap_or_default();
            return Err(Error::Server { status, body });
        }
        Ok(res.bytes().await?.to_vec())
    }

    /// Fallback body fetch using `Email/get` with `fetchAllBodyValues: true`.
    /// Reconstructs a minimal RFC 5322 message from JMAP body parts.  Less
    /// faithful than the blob path but always available.
    async fn fetch_body_via_email_get(&self, jmap_id: &str) -> Result<Vec<u8>> {
        let v = self
            .invoke(
                vec![json!([
                    "Email/get",
                    {
                        "accountId": self.account_id,
                        "ids": [jmap_id],
                        "properties": [
                            "subject", "from", "to", "cc", "date",
                            "messageId", "bodyStructure", "bodyValues",
                            "htmlBody", "textBody"
                        ],
                        "fetchAllBodyValues": true,
                        "maxBodyValueBytes": 1_048_576_u32
                    },
                    "e"
                ])],
                vec![CORE_CAPABILITY, MAIL_CAPABILITY],
            )
            .await?;
        let email = &v["methodResponses"][0][1]["list"][0];
        // Re-assemble a minimal RFC 5322 representation.
        let mut out = String::new();
        if let Some(s) = email["subject"].as_str() {
            out.push_str(&format!("Subject: {s}\r\n"));
        }
        if let Some(arr) = email["from"].as_array() {
            let addrs: Vec<String> = arr
                .iter()
                .filter_map(|a| {
                    let email_str = a["email"].as_str()?;
                    if let Some(n) = a["name"].as_str().filter(|n| !n.is_empty()) {
                        Some(format!("{n} <{email_str}>"))
                    } else {
                        Some(email_str.to_string())
                    }
                })
                .collect();
            out.push_str(&format!("From: {}\r\n", addrs.join(", ")));
        }
        if let Some(arr) = email["to"].as_array() {
            let addrs: Vec<String> = arr
                .iter()
                .filter_map(|a| a["email"].as_str().map(|s| s.to_string()))
                .collect();
            out.push_str(&format!("To: {}\r\n", addrs.join(", ")));
        }
        if let Some(d) = email["date"].as_str() {
            out.push_str(&format!("Date: {d}\r\n"));
        }
        if let Some(ids) = email["messageId"].as_array() {
            let id_strs: Vec<&str> = ids.iter().filter_map(|v| v.as_str()).collect();
            if !id_strs.is_empty() {
                out.push_str(&format!("Message-ID: <{}>\r\n", id_strs[0]));
            }
        }
        out.push_str("Content-Type: text/plain; charset=utf-8\r\n\r\n");
        // Pick text body value.
        if let Some(body_values) = email["bodyValues"].as_object()
            && let Some(part_id) = email["textBody"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|p| p["partId"].as_str())
            && let Some(val) = body_values.get(part_id).and_then(|v| v["value"].as_str())
        {
            out.push_str(val);
        }
        Ok(out.into_bytes())
    }

    /// `Email/set` — update keywords on one message.
    ///
    /// `add_imap` and `remove_imap` use IMAP convention (`\\Seen` etc.);
    /// this method translates to JMAP keywords.
    pub async fn set_email_flags(
        &self,
        jmap_id: &str,
        add_imap: &[&str],
        remove_imap: &[&str],
    ) -> Result<()> {
        let mut patch = serde_json::Map::new();
        for f in add_imap {
            if let Some(kw) = imap_flag_to_jmap(f) {
                patch.insert(format!("keywords/{kw}"), serde_json::Value::Bool(true));
            }
        }
        for f in remove_imap {
            if let Some(kw) = imap_flag_to_jmap(f) {
                patch.insert(format!("keywords/{kw}"), serde_json::Value::Null);
            }
        }
        if patch.is_empty() {
            return Ok(());
        }
        self.invoke(
            vec![json!([
                "Email/set",
                {
                    "accountId": self.account_id,
                    "update": {
                        jmap_id: patch
                    }
                },
                "u"
            ])],
            vec![CORE_CAPABILITY, MAIL_CAPABILITY],
        )
        .await?;
        Ok(())
    }

    /// `Email/set` — move a message to a new mailbox using JMAP patch syntax.
    pub async fn move_email(
        &self,
        jmap_id: &str,
        from_mailbox_id: &str,
        to_mailbox_id: &str,
    ) -> Result<()> {
        let mut patch = serde_json::Map::new();
        patch.insert(
            format!("mailboxIds/{to_mailbox_id}"),
            serde_json::Value::Bool(true),
        );
        patch.insert(
            format!("mailboxIds/{from_mailbox_id}"),
            serde_json::Value::Null,
        );
        self.invoke(
            vec![json!([
                "Email/set",
                {
                    "accountId": self.account_id,
                    "update": { jmap_id: patch }
                },
                "m"
            ])],
            vec![CORE_CAPABILITY, MAIL_CAPABILITY],
        )
        .await?;
        Ok(())
    }

    /// `Email/import` — append a draft to the specified mailbox.
    pub async fn import_draft(&self, mailbox_id: &str, raw: &[u8]) -> Result<()> {
        let upload_url = self
            .session
            .upload_url
            .as_deref()
            .ok_or(Error::Server {
                status: 0,
                body: "session has no uploadUrl".into(),
            })?
            .replace("{accountId}", &self.account_id);
        let upload: serde_json::Value = apply_auth(self.http.post(&upload_url), &self.auth)
            .header("Content-Type", "message/rfc822")
            .body(raw.to_vec())
            .send()
            .await?
            .json()
            .await?;
        let blob_id = upload["blobId"].as_str().ok_or(Error::Server {
            status: 0,
            body: "upload missing blobId".into(),
        })?;
        self.invoke(
            vec![json!([
                "Email/import",
                {
                    "accountId": self.account_id,
                    "emails": {
                        "d": {
                            "blobId": blob_id,
                            "mailboxIds": { mailbox_id: true },
                            "keywords": { "$draft": true, "$seen": true }
                        }
                    }
                },
                "i"
            ])],
            vec![CORE_CAPABILITY, MAIL_CAPABILITY],
        )
        .await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl crate::provider::MailProvider for JmapClient {
    async fn list_folders(&mut self) -> crate::provider::Result<Vec<crate::provider::FolderInfo>> {
        let mailboxes = self
            .list_mailboxes()
            .await
            .map_err(crate::provider::Error::Jmap)?;
        let folders = mailboxes
            .into_iter()
            .map(|m| {
                let special_use = m.role.as_deref().map(|r| match r {
                    "inbox" => "\\Inbox".to_string(),
                    "archive" => "\\Archive".to_string(),
                    "drafts" => "\\Drafts".to_string(),
                    "sent" => "\\Sent".to_string(),
                    "junk" | "spam" => "\\Junk".to_string(),
                    "trash" => "\\Trash".to_string(),
                    "all" => "\\All".to_string(),
                    _ => format!("\\{}", r),
                });
                crate::imap::FolderInfo {
                    name: m.name,
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
        limit: u32,
    ) -> crate::provider::Result<Vec<crate::imap::HeaderRow>> {
        use std::time::{SystemTime, UNIX_EPOCH};

        // Resolve the folder name to a JMAP mailbox id.
        let mailboxes = self
            .list_mailboxes()
            .await
            .map_err(crate::provider::Error::Jmap)?;
        let mailbox = mailboxes
            .iter()
            .find(|m| {
                m.name.eq_ignore_ascii_case(folder)
                    || m.role
                        .as_deref()
                        .map(|r| r.eq_ignore_ascii_case(folder))
                        .unwrap_or(false)
            })
            .ok_or_else(|| {
                crate::provider::Error::Jmap(Error::Server {
                    status: 0,
                    body: format!("JMAP: no mailbox matching '{folder}'"),
                })
            })?;
        let mailbox_id = mailbox.id.clone();

        // Email/query + Email/get in one batch.
        let v = self
            .invoke(
                vec![
                    json!([
                        "Email/query",
                        {
                            "accountId": self.account_id,
                            "filter": { "inMailbox": mailbox_id },
                            "sort": [{"property": "receivedAt", "isAscending": false}],
                            "limit": limit,
                        },
                        "q"
                    ]),
                    json!([
                        "Email/get",
                        {
                            "accountId": self.account_id,
                            "#ids": {
                                "resultOf": "q",
                                "name": "Email/query",
                                "path": "/ids"
                            },
                            "properties": [
                                "id","subject","from","to",
                                "receivedAt","messageId","keywords"
                            ]
                        },
                        "g"
                    ]),
                ],
                vec![CORE_CAPABILITY, MAIL_CAPABILITY],
            )
            .await
            .map_err(crate::provider::Error::Jmap)?;

        let emails: Vec<EmailHeader> =
            serde_json::from_value(v["methodResponses"][1][1]["list"].clone())
                .map_err(|e| crate::provider::Error::Jmap(Error::Json(e)))?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let rows = emails
            .into_iter()
            .map(|e| {
                let uid = jmap_id_to_uid(&e.id);
                let from_addr = e
                    .from
                    .as_ref()
                    .and_then(|v| v.first())
                    .map(|a| a.formatted());
                let to_addrs = e.to.as_ref().map(|v| {
                    v.iter()
                        .map(|a| a.formatted())
                        .collect::<Vec<_>>()
                        .join(", ")
                });
                let date_unix = e.received_at.as_deref().and_then(parse_jmap_date);
                let flags = build_flags_from_keywords(e.keywords.as_ref());
                crate::imap::HeaderRow {
                    uid: uid as u32,
                    uidvalidity: 0,
                    message_id: e.message_id.as_ref().and_then(|v| v.first()).cloned(),
                    subject: e.subject,
                    from_addr,
                    to_addrs,
                    date_unix,
                    flags,
                    fetched_at_unix: now,
                    provider_id: Some(e.id.clone()),
                }
            })
            .collect();

        Ok(rows)
    }

    async fn fetch_body(&mut self, folder: &str, uid: i64) -> crate::provider::Result<Vec<u8>> {
        // Resolve uid → JMAP id via store (fast) or 500-message scan (slow).
        let jmap_id = self
            .resolve_jmap_id(folder, uid)
            .await
            .map_err(crate::provider::Error::Jmap)?;

        // Fetch blobId for the resolved id.
        let v = self
            .invoke(
                vec![json!([
                    "Email/get",
                    {
                        "accountId": self.account_id,
                        "ids": [&jmap_id],
                        "properties": ["id", "blobId"]
                    },
                    "b"
                ])],
                vec![CORE_CAPABILITY, MAIL_CAPABILITY],
            )
            .await
            .map_err(crate::provider::Error::Jmap)?;

        let blob_id = v["methodResponses"][0][1]["list"][0]["blobId"]
            .as_str()
            .unwrap_or("")
            .to_string();

        // Prefer the downloadUrl template from the session for a lossless fetch.
        // The current `Session` struct only stores apiUrl, so we derive the
        // download URL from the apiUrl base.
        let api_base = self
            .session
            .api_url
            .trim_end_matches("/jmap/api")
            .trim_end_matches("/api");
        let download_url = format!(
            "{api_base}/jmap/download/{}/{}/message.eml",
            self.account_id, blob_id
        );

        let res = apply_auth(self.http.get(&download_url), &self.auth)
            .send()
            .await;

        match res {
            Ok(r) if r.status().is_success() => Ok(r
                .bytes()
                .await
                .map_err(|e| crate::provider::Error::Jmap(Error::Reqwest(e)))?
                .to_vec()),
            _ => {
                // Blob URL failed; fall back to Email/get body values.
                self.fetch_body_via_email_get(&jmap_id)
                    .await
                    .map_err(crate::provider::Error::Jmap)
            }
        }
    }

    async fn set_flags(
        &mut self,
        folder: &str,
        uid: i64,
        add: &[&str],
        remove: &[&str],
    ) -> crate::provider::Result<()> {
        // Resolve uid → JMAP id via store (fast) or scan (slow).
        let jmap_id = self
            .resolve_jmap_id(folder, uid)
            .await
            .map_err(crate::provider::Error::Jmap)?;
        self.set_email_flags(&jmap_id, add, remove)
            .await
            .map_err(crate::provider::Error::Jmap)
    }

    async fn move_message(
        &mut self,
        folder: &str,
        uid: i64,
        dest: &str,
    ) -> crate::provider::Result<()> {
        let jmap_id = self
            .resolve_jmap_id(folder, uid)
            .await
            .map_err(crate::provider::Error::Jmap)?;
        let mailboxes = self
            .list_mailboxes()
            .await
            .map_err(crate::provider::Error::Jmap)?;
        let from_id = mailboxes
            .iter()
            .find(|m| {
                m.name.eq_ignore_ascii_case(folder)
                    || m.role
                        .as_deref()
                        .map(|r| r.eq_ignore_ascii_case(folder))
                        .unwrap_or(false)
            })
            .map(|m| m.id.clone())
            .unwrap_or_default();
        let to_id = mailboxes
            .iter()
            .find(|m| {
                m.name.eq_ignore_ascii_case(dest)
                    || m.role
                        .as_deref()
                        .map(|r| r.eq_ignore_ascii_case(dest))
                        .unwrap_or(false)
            })
            .map(|m| m.id.clone())
            .ok_or_else(|| {
                crate::provider::Error::Jmap(Error::Server {
                    status: 0,
                    body: format!("JMAP: no mailbox matching destination '{dest}'"),
                })
            })?;
        self.move_email(&jmap_id, &from_id, &to_id)
            .await
            .map_err(crate::provider::Error::Jmap)
    }

    async fn send(&mut self, raw: &[u8]) -> crate::provider::Result<()> {
        self.send_mime(raw)
            .await
            .map_err(crate::provider::Error::Jmap)
    }

    async fn append_draft(&mut self, _folder: &str, raw: &[u8]) -> crate::provider::Result<()> {
        // Resolve the Drafts mailbox id.
        let mailboxes = self
            .list_mailboxes()
            .await
            .map_err(crate::provider::Error::Jmap)?;
        let drafts_id = mailboxes
            .iter()
            .find(|m| m.role.as_deref() == Some("drafts"))
            .or_else(|| {
                mailboxes
                    .iter()
                    .find(|m| m.name.eq_ignore_ascii_case("Drafts"))
            })
            .map(|m| m.id.clone())
            .ok_or_else(|| {
                crate::provider::Error::Jmap(Error::Server {
                    status: 0,
                    body: "JMAP: no Drafts mailbox".into(),
                })
            })?;
        self.import_draft(&drafts_id, raw)
            .await
            .map_err(crate::provider::Error::Jmap)
    }

    async fn expunge_folder(&mut self, folder: &str) -> crate::provider::Result<usize> {
        // Resolve the folder name to its JMAP mailbox id.
        let mailboxes = self
            .list_mailboxes()
            .await
            .map_err(crate::provider::Error::Jmap)?;
        let mailbox_id = mailboxes
            .iter()
            .find(|m| {
                m.name.eq_ignore_ascii_case(folder)
                    || m.role
                        .as_deref()
                        .map(|r| r.eq_ignore_ascii_case(folder))
                        .unwrap_or(false)
            })
            .map(|m| m.id.clone())
            .ok_or_else(|| {
                crate::provider::Error::Jmap(Error::Server {
                    status: 0,
                    body: format!("JMAP: no mailbox matching '{folder}'"),
                })
            })?;

        // Email/query filtered by inMailbox + $deleted keyword.
        // This mirrors the IMAP \Deleted flag: messages marked $deleted in this
        // mailbox are the ones EXPUNGE would remove on IMAP.
        let v = self
            .invoke(
                vec![json!([
                    "Email/query",
                    {
                        "accountId": self.account_id,
                        "filter": {
                            "inMailbox": mailbox_id,
                            "hasKeyword": "$deleted"
                        },
                        "limit": 1000_u32,
                    },
                    "q"
                ])],
                vec![CORE_CAPABILITY, MAIL_CAPABILITY],
            )
            .await
            .map_err(crate::provider::Error::Jmap)?;

        let ids: Vec<String> = v["methodResponses"][0][1]["ids"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        if ids.is_empty() {
            return Ok(0);
        }

        // TODO: paginate Email/query if a folder ever holds >1000 $deleted msgs.
        // Email/set { destroy: [...] } — permanently removes the messages.
        let resp = self
            .invoke(
                vec![json!([
                    "Email/set",
                    {
                        "accountId": self.account_id,
                        "destroy": ids
                    },
                    "d"
                ])],
                vec![CORE_CAPABILITY, MAIL_CAPABILITY],
            )
            .await
            .map_err(crate::provider::Error::Jmap)?;

        // Server reports actual destroyed ids; notDestroyed entries are skipped.
        let destroyed = resp["methodResponses"][0][1]["destroyed"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        Ok(destroyed)
    }

    async fn create_folder(&mut self, name: &str) -> crate::provider::Result<()> {
        // TODO(hierarchical): split on '/' and create with parentId for nested folders.
        // For now pass the literal name — Fastmail accepts it as a top-level mailbox.
        self.invoke(
            vec![json!([
                "Mailbox/set",
                {
                    "accountId": self.account_id,
                    "create": {
                        "new": {
                            "name": name,
                            "parentId": null
                        }
                    }
                },
                "c"
            ])],
            vec![CORE_CAPABILITY, MAIL_CAPABILITY],
        )
        .await
        .map_err(crate::provider::Error::Jmap)?;
        Ok(())
    }

    async fn delete_folder(&mut self, name: &str) -> crate::provider::Result<()> {
        let mailboxes = self
            .list_mailboxes()
            .await
            .map_err(crate::provider::Error::Jmap)?;
        let mailbox_id = mailboxes
            .iter()
            .find(|m| {
                m.name.eq_ignore_ascii_case(name)
                    || m.role
                        .as_deref()
                        .map(|r| r.eq_ignore_ascii_case(name))
                        .unwrap_or(false)
            })
            .map(|m| m.id.clone())
            .ok_or_else(|| {
                crate::provider::Error::Jmap(Error::Server {
                    status: 0,
                    body: format!("JMAP: no mailbox matching '{name}'"),
                })
            })?;
        self.invoke(
            vec![json!([
                "Mailbox/set",
                {
                    "accountId": self.account_id,
                    "destroy": [mailbox_id]
                },
                "d"
            ])],
            vec![CORE_CAPABILITY, MAIL_CAPABILITY],
        )
        .await
        .map_err(crate::provider::Error::Jmap)?;
        Ok(())
    }

    async fn rename_folder(&mut self, from: &str, to: &str) -> crate::provider::Result<()> {
        let mailboxes = self
            .list_mailboxes()
            .await
            .map_err(crate::provider::Error::Jmap)?;
        let mailbox_id = mailboxes
            .iter()
            .find(|m| {
                m.name.eq_ignore_ascii_case(from)
                    || m.role
                        .as_deref()
                        .map(|r| r.eq_ignore_ascii_case(from))
                        .unwrap_or(false)
            })
            .map(|m| m.id.clone())
            .ok_or_else(|| {
                crate::provider::Error::Jmap(Error::Server {
                    status: 0,
                    body: format!("JMAP: no mailbox matching '{from}'"),
                })
            })?;
        self.invoke(
            vec![json!([
                "Mailbox/set",
                {
                    "accountId": self.account_id,
                    "update": {
                        mailbox_id: { "name": to }
                    }
                },
                "u"
            ])],
            vec![CORE_CAPABILITY, MAIL_CAPABILITY],
        )
        .await
        .map_err(crate::provider::Error::Jmap)?;
        Ok(())
    }

    async fn subscribe_folder(&mut self, name: &str, on: bool) -> crate::provider::Result<()> {
        let mailboxes = self
            .list_mailboxes()
            .await
            .map_err(crate::provider::Error::Jmap)?;
        let mailbox_id = mailboxes
            .iter()
            .find(|m| {
                m.name.eq_ignore_ascii_case(name)
                    || m.role
                        .as_deref()
                        .map(|r| r.eq_ignore_ascii_case(name))
                        .unwrap_or(false)
            })
            .map(|m| m.id.clone())
            .ok_or_else(|| {
                crate::provider::Error::Jmap(Error::Server {
                    status: 0,
                    body: format!("JMAP: no mailbox matching '{name}'"),
                })
            })?;
        self.invoke(
            vec![json!([
                "Mailbox/set",
                {
                    "accountId": self.account_id,
                    "update": {
                        mailbox_id: { "isSubscribed": on }
                    }
                },
                "s"
            ])],
            vec![CORE_CAPABILITY, MAIL_CAPABILITY],
        )
        .await
        .map_err(crate::provider::Error::Jmap)?;
        Ok(())
    }
}

impl JmapClient {
    /// Resolve a local uid (FNV-1a hash of JMAP id) back to the JMAP string id.
    ///
    /// Fast path: query `provider_id` from the store when a `Store` is attached.
    /// Slow path (pre-migration or no store): scan the most recent 500 messages
    /// and re-hash each id. A `tracing::debug!` is emitted whenever the slow
    /// path is taken so production instances can verify the fast path is hit.
    async fn resolve_jmap_id(&self, folder: &str, uid: i64) -> Result<String> {
        // Fast path: store lookup.
        if let Some(store) = &self.store {
            if let Ok(Some(pid)) = store.provider_id_for(folder, uid).await {
                return Ok(pid);
            }
            tracing::debug!(
                folder,
                uid,
                "resolve_jmap_id: provider_id not in store, falling back to 500-message scan"
            );
        }

        // Slow path: scan recent messages and find by hash.
        let v = self
            .invoke(
                vec![
                    json!([
                        "Email/query",
                        {
                            "accountId": self.account_id,
                            "sort": [{"property": "receivedAt", "isAscending": false}],
                            "limit": 500_u32,
                        },
                        "q"
                    ]),
                    json!([
                        "Email/get",
                        {
                            "accountId": self.account_id,
                            "#ids": {
                                "resultOf": "q",
                                "name": "Email/query",
                                "path": "/ids"
                            },
                            "properties": ["id"]
                        },
                        "g"
                    ]),
                ],
                vec![CORE_CAPABILITY, MAIL_CAPABILITY],
            )
            .await?;

        let list = v["methodResponses"][1][1]["list"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        list.iter()
            .find(|e| {
                e["id"]
                    .as_str()
                    .map(|id| jmap_id_to_uid(id) == uid)
                    .unwrap_or(false)
            })
            .and_then(|e| e["id"].as_str().map(|s| s.to_string()))
            .ok_or(Error::Server {
                status: 0,
                body: format!("JMAP: uid {uid} not found in recent 500 messages"),
            })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a JMAP ISO 8601 date string (e.g. `2026-01-02T15:04:05Z`) to Unix ts.
fn parse_jmap_date(s: &str) -> Option<i64> {
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
    // Days from 1970-01-01 using civil_from_days algorithm.
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

/// Build an IMAP-style flags string from JMAP keywords map.
fn build_flags_from_keywords(
    keywords: Option<&serde_json::Map<String, serde_json::Value>>,
) -> String {
    let Some(kw) = keywords else {
        return String::new();
    };
    let mut flags = Vec::new();
    if kw.get("$seen").and_then(|v| v.as_bool()).unwrap_or(false) {
        flags.push("\\Seen");
    }
    if kw
        .get("$flagged")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        flags.push("\\Flagged");
    }
    if kw
        .get("$answered")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        flags.push("\\Answered");
    }
    if kw.get("$draft").and_then(|v| v.as_bool()).unwrap_or(false) {
        flags.push("\\Draft");
    }
    if kw
        .get("$deleted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        flags.push("\\Deleted");
    }
    flags.join(" ")
}
