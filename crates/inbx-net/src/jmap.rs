//! Minimal JMAP (RFC 8620 / RFC 8621) client.
//!
//! Hand-rolled over reqwest because jmap-client crates churn fast. Targets
//! Fastmail / Stalwart. Auth is HTTP basic with the account's app password
//! (Bearer-token / OAuth wiring lives in the OAuth module and can attach
//! later). Implements the bare slice we need to fetch headers and send
//! mail; everything else (push, vacation, Sieve mgmt) lives in the
//! provider's own protocol path.

use std::{
    collections::{HashMap, HashSet},
    pin::Pin,
};

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
    scan_from: usize,
}

impl EventStream {
    pub async fn next_event(&mut self) -> Result<Option<String>> {
        loop {
            if let Some((end, delimiter_len)) = find_blank_line(&self.buf, self.scan_from) {
                if end > MAX_SSE_RECORD_BYTES {
                    return Err(Error::SseRecordTooLarge(MAX_SSE_RECORD_BYTES));
                }
                let raw = self.buf.drain(..end).collect::<Vec<u8>>();
                self.buf.drain(..delimiter_len);
                self.scan_from = 0;
                let text = String::from_utf8_lossy(&raw)
                    .replace("\r\n", "\n")
                    .replace('\r', "\n");
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
                Some(Ok(chunk)) => {
                    self.scan_from = self.buf.len().saturating_sub(3);
                    self.buf.extend_from_slice(&chunk);
                    // A chunk may contain several records.  Enforce the limit
                    // only on the unfinished current record after draining all
                    // complete records above.
                    if find_blank_line(&self.buf, self.scan_from).is_none()
                        && self.buf.len() > MAX_SSE_RECORD_BYTES
                    {
                        return Err(Error::SseRecordTooLarge(MAX_SSE_RECORD_BYTES));
                    }
                }
                Some(Err(e)) => return Err(Error::Reqwest(e)),
                None => return Ok(None),
            }
        }
    }
}

const MAX_SSE_RECORD_BYTES: usize = 1024 * 1024;

fn find_blank_line(buf: &[u8], start: usize) -> Option<(usize, usize)> {
    let mut index = start.saturating_sub(3);
    while index < buf.len() {
        if let Some(first_len) = line_ending_len(buf, index) {
            let second = index + first_len;
            if let Some(second_len) = line_ending_len(buf, second) {
                return Some((index, first_len + second_len));
            }
        }
        index += 1;
    }
    None
}

/// Returns one legal SSE line terminator length at `index`.
fn line_ending_len(buf: &[u8], index: usize) -> Option<usize> {
    match buf.get(index) {
        Some(b'\r') if buf.get(index + 1) == Some(&b'\n') => Some(2),
        Some(b'\r' | b'\n') => Some(1),
        _ => None,
    }
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
    #[error("SSE record exceeds {0} bytes")]
    SseRecordTooLarge(usize),
    #[error("JMAP Email/query stopped before its advertised total")]
    IncompleteQuery,
    #[error("JMAP method error {kind}: {description}")]
    Method { kind: String, description: String },
    #[error("malformed JMAP mailbox hierarchy: {0}")]
    MailboxHierarchy(String),
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
    #[serde(rename = "downloadUrl", default)]
    pub download_url: Option<String>,
    #[serde(rename = "eventSourceUrl", default)]
    pub event_source_url: Option<String>,
    #[serde(default)]
    pub capabilities: serde_json::Map<String, Value>,
}

impl Session {
    pub fn account_id_for(&self, capability: &str) -> Option<&str> {
        self.primary_accounts
            .get(capability)
            .and_then(|v| v.as_str())
    }

    fn max_objects_in_get(&self) -> usize {
        self.core_capability_limit("maxObjectsInGet", 500)
    }

    fn max_objects_in_set(&self) -> usize {
        self.core_capability_limit("maxObjectsInSet", 500)
    }

    fn core_capability_limit(&self, key: &str, default: usize) -> usize {
        self.capabilities
            .get(CORE_CAPABILITY)
            .and_then(|v| v.get(key))
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|&n| n > 0)
            .unwrap_or(default)
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
        let res = ensure_success(apply_auth(http.get(session_url), &auth).send().await?).await?;
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
        let res = ensure_success(req.send().await?).await?;
        let value = res.json().await?;
        validate_method_responses(&value)?;
        Ok(value)
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
        let inbox = resolve_mailbox(&mailboxes, "Inbox")?;
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
        let res = ensure_success(
            apply_auth(self.http.get(&url), &self.auth)
                .header("Accept", "text/event-stream")
                .send()
                .await?,
        )
        .await?;
        Ok(EventStream {
            inner: Box::pin(res.bytes_stream()),
            buf: Vec::new(),
            scan_from: 0,
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
        let drafts_id = resolve_mailbox(&mailboxes, "Drafts")?.id.clone();

        let response = self
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
        validate_email_mutation(&response, "i", "created", "notCreated", &["ev"])?;
        validate_email_mutation(&response, "s", "created", "notCreated", &["s"])?;
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Mailbox {
    pub id: String,
    pub name: String,
    #[serde(rename = "parentId", default)]
    pub parent_id: Option<String>,
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

fn validate_email_mutation(
    value: &Value,
    call_id: &str,
    succeeded: &str,
    failed: &str,
    keys: &[&str],
) -> Result<()> {
    let response = value
        .get("methodResponses")
        .and_then(Value::as_array)
        .and_then(|responses| {
            responses.iter().find_map(|response| {
                let parts = response.as_array()?;
                (parts.get(2).and_then(Value::as_str) == Some(call_id)).then(|| parts.get(1))?
            })
        })
        .ok_or_else(|| Error::Method {
            kind: "invalidResponse".into(),
            description: format!("missing response for {call_id}"),
        })?;
    for key in keys {
        if let Some(error) = response.get(failed).and_then(|v| v.get(*key)) {
            return Err(Error::Method {
                kind: error
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or(failed)
                    .to_string(),
                description: error
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("operation failed")
                    .to_string(),
            });
        }
        let present = match response.get(succeeded) {
            Some(Value::Array(ids)) => ids.iter().any(|id| id.as_str() == Some(*key)),
            Some(Value::Object(entries)) => entries.contains_key(*key),
            _ => false,
        };
        if !present {
            return Err(Error::Method {
                kind: "invalidResponse".into(),
                description: format!("{call_id} did not report {key} in {succeeded}"),
            });
        }
    }
    Ok(())
}

fn validate_method_responses(value: &Value) -> Result<()> {
    let Some(responses) = value.get("methodResponses").and_then(Value::as_array) else {
        return Ok(());
    };
    for response in responses {
        let Some(parts) = response.as_array() else {
            continue;
        };
        if parts.first().and_then(Value::as_str) == Some("error") {
            let payload = parts.get(1).unwrap_or(&Value::Null);
            return Err(Error::Method {
                kind: payload
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                description: payload
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        }
    }
    Ok(())
}

async fn ensure_success(res: reqwest::Response) -> Result<reqwest::Response> {
    if res.status().is_success() {
        Ok(res)
    } else {
        let status = res.status().as_u16();
        let body = res.text().await.unwrap_or_default();
        Err(Error::Server { status, body })
    }
}

fn download_url_template(session: &Session) -> Result<&str> {
    session.download_url.as_deref().ok_or(Error::Server {
        status: 0,
        body: "session has no downloadUrl".into(),
    })
}

fn email_blob_id<'a>(response: &'a Value, jmap_id: &str) -> Result<&'a str> {
    response["methodResponses"][0][1]["list"][0]["blobId"]
        .as_str()
        .ok_or_else(|| Error::Server {
            status: 0,
            body: format!("no blobId for JMAP id {jmap_id}"),
        })
}

fn expand_download_url_template(
    template: &str,
    account_id: &str,
    blob_id: &str,
    content_type: &str,
    name: &str,
) -> String {
    fn encode(value: &str) -> String {
        url::form_urlencoded::byte_serialize(value.as_bytes())
            .collect::<String>()
            .replace('+', "%20")
    }

    template
        .replace("{accountId}", &encode(account_id))
        .replace("{blobId}", &encode(blob_id))
        .replace("{type}", &encode(content_type))
        .replace("{name}", &encode(name))
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
    /// the raw RFC 5322 blob through the server-provided `downloadUrl` template.
    pub async fn fetch_raw_blob(&self, jmap_id: &str) -> Result<Vec<u8>> {
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
        let blob_id = email_blob_id(&v, jmap_id)?;
        self.fetch_raw_blob_via_template(blob_id, download_url_template(&self.session)?)
            .await
    }

    /// Download raw RFC 5322 bytes using an RFC 8620 `downloadUrl` template.
    pub async fn fetch_raw_blob_via_template(
        &self,
        blob_id: &str,
        download_url_tmpl: &str,
    ) -> Result<Vec<u8>> {
        let url = expand_download_url_template(
            download_url_tmpl,
            &self.account_id,
            blob_id,
            "message/rfc822",
            "message.eml",
        );
        let res = ensure_success(apply_auth(self.http.get(url), &self.auth).send().await?).await?;
        Ok(res.bytes().await?.to_vec())
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
        let response = self
            .invoke(
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
        validate_email_mutation(&response, "u", "updated", "notUpdated", &[jmap_id])?;
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
        let response = self
            .invoke(
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
        validate_email_mutation(&response, "m", "updated", "notUpdated", &[jmap_id])?;
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
        let response = self
            .invoke(
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
        validate_email_mutation(&response, "i", "created", "notCreated", &["d"])?;
        Ok(())
    }

    async fn query_all_email_ids(&self, mailbox_id: &str) -> Result<Vec<String>> {
        const PAGE_SIZE: u32 = 500;
        let mut ids = Vec::new();
        let mut position = 0_u32;
        let mut query_state: Option<String> = None;
        let mut expected_total: Option<usize> = None;
        loop {
            let v = self
                .invoke(
                    vec![json!(["Email/query", {
                "accountId": self.account_id, "filter": { "inMailbox": mailbox_id },
                "sort": [{"property": "receivedAt", "isAscending": false}], "position": position,
                "limit": PAGE_SIZE
            }, "q"])],
                    vec![CORE_CAPABILITY, MAIL_CAPABILITY],
                )
                .await?;
            let response = &v["methodResponses"][0][1];
            let page = as_id_vec(&response["ids"]);
            let (state, total) = validate_query_page(
                position,
                query_state.as_deref().zip(expected_total),
                &ids,
                &page,
                (
                    response["position"].as_u64(),
                    response["queryState"].as_str(),
                    response["total"].as_u64(),
                ),
            )?;
            query_state = Some(state);
            expected_total = Some(total);
            let page_len = page.len();
            ids.extend(page);
            let Some(next_position) = next_query_position(ids.len(), total, page_len)? else {
                return Ok(ids);
            };
            position = next_position;
        }
    }

    async fn get_email_headers(&self, ids: &[String]) -> Result<Vec<EmailHeader>> {
        let mut emails = Vec::with_capacity(ids.len());
        for ids in email_get_batches(ids, self.session.max_objects_in_get()) {
            let v = self
                .invoke(
                    vec![json!(["Email/get", {
                "accountId": self.account_id, "ids": ids,
                "properties": ["id", "subject", "from", "to", "receivedAt", "messageId", "keywords"]
            }, "g"])],
                    vec![CORE_CAPABILITY, MAIL_CAPABILITY],
                )
                .await?;
            let page: Vec<EmailHeader> =
                serde_json::from_value(v["methodResponses"][0][1]["list"].clone())?;
            if page.len() != ids.len()
                || page
                    .iter()
                    .map(|email| &email.id)
                    .collect::<std::collections::HashSet<_>>()
                    != ids.iter().collect()
            {
                return Err(Error::IncompleteQuery);
            }
            emails.extend(page);
        }
        Ok(emails)
    }
}

#[async_trait::async_trait]
impl crate::provider::MailProvider for JmapClient {
    async fn list_folders(&mut self) -> crate::provider::Result<Vec<crate::provider::FolderInfo>> {
        let mailboxes = self
            .list_mailboxes()
            .await
            .map_err(crate::provider::Error::Jmap)?;
        let paths = mailbox_paths(&mailboxes).map_err(crate::provider::Error::Jmap)?;
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
                    name: paths[&m.id].clone(),
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
        use std::time::{SystemTime, UNIX_EPOCH};

        // Resolve the folder name to a JMAP mailbox id.
        let mailboxes = self
            .list_mailboxes()
            .await
            .map_err(crate::provider::Error::Jmap)?;
        let mailbox = resolve_mailbox(&mailboxes, folder).map_err(crate::provider::Error::Jmap)?;
        let mailbox_id = mailbox.id.clone();

        // A complete snapshot walks every query page, then respects the server's
        // advertised Email/get batch limit.
        let ids = self
            .query_all_email_ids(&mailbox_id)
            .await
            .map_err(crate::provider::Error::Jmap)?;
        let emails = self
            .get_email_headers(&ids)
            .await
            .map_err(crate::provider::Error::Jmap)?;

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
                    uid,
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

        Ok(crate::provider::HeaderSnapshot {
            rows,
            complete: true,
            transport: crate::provider::SnapshotTransport::Opaque,
        })
    }

    async fn fetch_body(&mut self, folder: &str, uid: i64) -> crate::provider::Result<Vec<u8>> {
        // Resolve uid → JMAP id via store (fast) or 500-message scan (slow).
        let jmap_id = self
            .resolve_jmap_id(folder, uid)
            .await
            .map_err(crate::provider::Error::Jmap)?;

        self.fetch_raw_blob(&jmap_id)
            .await
            .map_err(crate::provider::Error::Jmap)
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
        let from_id = resolve_mailbox(&mailboxes, folder)
            .map_err(crate::provider::Error::Jmap)?
            .id
            .clone();
        let to_id = resolve_mailbox(&mailboxes, dest)
            .map_err(crate::provider::Error::Jmap)?
            .id
            .clone();
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
        let drafts_id = resolve_mailbox(&mailboxes, "Drafts")
            .map_err(crate::provider::Error::Jmap)?
            .id
            .clone();
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
        let mailbox_id = resolve_mailbox(&mailboxes, folder)
            .map_err(crate::provider::Error::Jmap)?
            .id
            .clone();
        let mut removed = 0;
        let mut previous_ids = None;
        loop {
            let v = self
                .invoke(
                    vec![json!([
                        "Email/query",
                        {
                            "accountId": self.account_id,
                            "filter": { "inMailbox": mailbox_id, "hasKeyword": "$deleted" },
                            "limit": self.session.max_objects_in_set(),
                        },
                        "q"
                    ])],
                    vec![CORE_CAPABILITY, MAIL_CAPABILITY],
                )
                .await
                .map_err(crate::provider::Error::Jmap)?;
            let ids = as_id_vec(&v["methodResponses"][0][1]["ids"]);
            if ids.is_empty() {
                return Ok(removed);
            }
            expunge_page_progresses(previous_ids.as_deref(), &ids)
                .map_err(crate::provider::Error::Jmap)?;
            previous_ids = Some(ids.clone());
            for batch in email_set_batches(&ids, self.session.max_objects_in_set()) {
                let response = self
                    .invoke(
                        vec![json!([
                            "Email/set",
                            { "accountId": self.account_id, "destroy": batch },
                            "d"
                        ])],
                        vec![CORE_CAPABILITY, MAIL_CAPABILITY],
                    )
                    .await
                    .map_err(crate::provider::Error::Jmap)?;
                let requested: Vec<&str> = batch.iter().map(String::as_str).collect();
                validate_email_mutation(&response, "d", "destroyed", "notDestroyed", &requested)
                    .map_err(crate::provider::Error::Jmap)?;
                removed += batch.len();
            }
        }
    }

    async fn create_folder(&mut self, name: &str) -> crate::provider::Result<()> {
        let segments = split_mailbox_path(name).map_err(crate::provider::Error::Jmap)?;

        if segments.len() == 1 {
            // Single segment — create top-level mailbox directly.
            let response = self
                .invoke(
                    vec![json!([
                        "Mailbox/set",
                        {
                            "accountId": self.account_id,
                            "create": {
                                "new": {
                                    "name": segments[0],
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
            validate_email_mutation(&response, "c", "created", "notCreated", &["new"])
                .map_err(crate::provider::Error::Jmap)?;
            return Ok(());
        }

        // Multi-segment path: walk existing mailboxes, creating any missing segments.
        let mut mailboxes = self
            .list_mailboxes()
            .await
            .map_err(crate::provider::Error::Jmap)?;

        let mut parent_id: Option<String> = None;

        for segment in &segments {
            // Find an existing mailbox at this level with the same name.
            let existing = mailboxes
                .iter()
                .find(|m| m.parent_id == parent_id && m.name.eq_ignore_ascii_case(segment));

            if let Some(m) = existing {
                parent_id = Some(m.id.clone());
                continue;
            }

            // Segment not found — create it under parent_id.
            let parent_value = match &parent_id {
                Some(id) => Value::String(id.clone()),
                None => Value::Null,
            };
            let v = self
                .invoke(
                    vec![json!([
                        "Mailbox/set",
                        {
                            "accountId": self.account_id,
                            "create": {
                                "new": {
                                    "name": segment,
                                    "parentId": parent_value
                                }
                            }
                        },
                        "c"
                    ])],
                    vec![CORE_CAPABILITY, MAIL_CAPABILITY],
                )
                .await
                .map_err(crate::provider::Error::Jmap)?;

            validate_email_mutation(&v, "c", "created", "notCreated", &["new"])
                .map_err(crate::provider::Error::Jmap)?;

            let new_id = v["methodResponses"][0][1]["created"]["new"]["id"]
                .as_str()
                .ok_or_else(|| {
                    crate::provider::Error::Jmap(Error::Server {
                        status: 0,
                        body: format!("JMAP: no id returned for created mailbox '{segment}'"),
                    })
                })?
                .to_owned();

            // Avoid round-trip re-list — server-assigned id is enough for subsequent segments.
            mailboxes.push(Mailbox {
                id: new_id.clone(),
                name: segment.to_string(),
                parent_id: parent_id.clone(),
                role: None,
                total: 0,
                unread: 0,
            });

            parent_id = Some(new_id);
        }

        Ok(())
    }

    async fn delete_folder(&mut self, name: &str) -> crate::provider::Result<()> {
        let mailboxes = self
            .list_mailboxes()
            .await
            .map_err(crate::provider::Error::Jmap)?;
        let mailbox_id = resolve_mailbox(&mailboxes, name)
            .map_err(crate::provider::Error::Jmap)?
            .id
            .clone();
        let response = self
            .invoke(
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
        validate_email_mutation(&response, "d", "destroyed", "notDestroyed", &[&mailbox_id])
            .map_err(crate::provider::Error::Jmap)?;
        Ok(())
    }

    async fn rename_folder(&mut self, from: &str, to: &str) -> crate::provider::Result<()> {
        let mailboxes = self
            .list_mailboxes()
            .await
            .map_err(crate::provider::Error::Jmap)?;
        let mailbox_id = resolve_mailbox(&mailboxes, from)
            .map_err(crate::provider::Error::Jmap)?
            .id
            .clone();
        let response = self
            .invoke(
                vec![json!([
                    "Mailbox/set",
                    {
                        "accountId": self.account_id,
                        "update": {
                            mailbox_id.clone(): { "name": to }
                        }
                    },
                    "u"
                ])],
                vec![CORE_CAPABILITY, MAIL_CAPABILITY],
            )
            .await
            .map_err(crate::provider::Error::Jmap)?;
        validate_email_mutation(&response, "u", "updated", "notUpdated", &[&mailbox_id])
            .map_err(crate::provider::Error::Jmap)?;
        Ok(())
    }

    async fn subscribe_folder(&mut self, name: &str, on: bool) -> crate::provider::Result<()> {
        let mailboxes = self
            .list_mailboxes()
            .await
            .map_err(crate::provider::Error::Jmap)?;
        let mailbox_id = resolve_mailbox(&mailboxes, name)
            .map_err(crate::provider::Error::Jmap)?
            .id
            .clone();
        let response = self
            .invoke(
                vec![json!([
                    "Mailbox/set",
                    {
                        "accountId": self.account_id,
                        "update": {
                            mailbox_id.clone(): { "isSubscribed": on }
                        }
                    },
                    "s"
                ])],
                vec![CORE_CAPABILITY, MAIL_CAPABILITY],
            )
            .await
            .map_err(crate::provider::Error::Jmap)?;
        validate_email_mutation(&response, "s", "updated", "notUpdated", &[&mailbox_id])
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

fn mailbox_paths(mailboxes: &[Mailbox]) -> Result<HashMap<String, String>> {
    let by_id: HashMap<&str, &Mailbox> = mailboxes
        .iter()
        .map(|mailbox| (mailbox.id.as_str(), mailbox))
        .collect();
    fn path_for<'a>(
        mailbox: &'a Mailbox,
        by_id: &HashMap<&'a str, &'a Mailbox>,
        paths: &mut HashMap<String, String>,
        visiting: &mut HashSet<String>,
    ) -> Result<String> {
        if let Some(path) = paths.get(&mailbox.id) {
            return Ok(path.clone());
        }
        if !visiting.insert(mailbox.id.clone()) {
            return Err(Error::MailboxHierarchy(format!(
                "parent cycle at {}",
                mailbox.id
            )));
        }
        let path = match mailbox.parent_id.as_deref() {
            Some(parent_id) => {
                let parent = by_id.get(parent_id).ok_or_else(|| {
                    Error::MailboxHierarchy(format!(
                        "{} has missing parent {parent_id}",
                        mailbox.id
                    ))
                })?;
                format!(
                    "{}/{}",
                    path_for(parent, by_id, paths, visiting)?,
                    escape_mailbox_segment(&mailbox.name)
                )
            }
            None => escape_mailbox_segment(&mailbox.name),
        };
        visiting.remove(&mailbox.id);
        paths.insert(mailbox.id.clone(), path.clone());
        Ok(path)
    }
    let mut paths = HashMap::new();
    for mailbox in mailboxes {
        path_for(mailbox, &by_id, &mut paths, &mut HashSet::new())?;
    }
    Ok(paths)
}

fn resolve_mailbox<'a>(mailboxes: &'a [Mailbox], folder: &str) -> Result<&'a Mailbox> {
    let paths = mailbox_paths(mailboxes)?;
    let path_matches: Vec<_> = mailboxes
        .iter()
        .filter(|mailbox| {
            paths
                .get(&mailbox.id)
                .is_some_and(|path| path.eq_ignore_ascii_case(folder))
        })
        .collect();
    match path_matches.as_slice() {
        [mailbox] => return Ok(mailbox),
        [] => {}
        _ => {
            return Err(Error::MailboxHierarchy(format!(
                "ambiguous mailbox path '{folder}'"
            )));
        }
    }
    let role = folder.trim_start_matches('\\');
    let matches: Vec<_> = mailboxes
        .iter()
        .filter(|mailbox| {
            mailbox
                .role
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case(role))
        })
        .collect();
    match matches.as_slice() {
        [mailbox] => Ok(mailbox),
        [] => Err(Error::Server {
            status: 0,
            body: format!("JMAP: no mailbox matching '{folder}'"),
        }),
        _ => Err(Error::MailboxHierarchy(format!(
            "ambiguous role alias '{folder}'"
        ))),
    }
}

fn email_set_batches(ids: &[String], max_objects_in_set: usize) -> Vec<&[String]> {
    ids.chunks(max_objects_in_set.max(1)).collect()
}

fn expunge_page_progresses(previous_ids: Option<&[String]>, ids: &[String]) -> Result<()> {
    if !ids.is_empty() && previous_ids == Some(ids) {
        return Err(Error::IncompleteQuery);
    }
    Ok(())
}

fn email_get_batches(ids: &[String], max_objects_in_get: usize) -> Vec<&[String]> {
    ids.chunks(max_objects_in_get.max(1)).collect()
}

fn validate_query_page(
    position: u32,
    previous: Option<(&str, usize)>,
    collected: &[String],
    page: &[String],
    returned: (Option<u64>, Option<&str>, Option<u64>),
) -> Result<(String, usize)> {
    let (returned_position, returned_state, total) = returned;
    let total = total
        .and_then(|total| usize::try_from(total).ok())
        .ok_or(Error::IncompleteQuery)?;
    if returned_position != Some(u64::from(position))
        || returned_state.is_none()
        || previous
            .is_some_and(|(state, expected)| Some(state) != returned_state || expected != total)
        || collected.len().saturating_add(page.len()) > total
        || page.iter().any(|id| collected.contains(id))
        || page.iter().collect::<std::collections::HashSet<_>>().len() != page.len()
    {
        return Err(Error::IncompleteQuery);
    }
    Ok((returned_state.unwrap().to_string(), total))
}

fn next_query_position(collected: usize, total: usize, page_len: usize) -> Result<Option<u32>> {
    if collected >= total {
        return Ok(None);
    }
    if page_len == 0 {
        return Err(Error::IncompleteQuery);
    }
    Ok(Some(
        u32::try_from(collected).map_err(|_| Error::IncompleteQuery)?,
    ))
}

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

fn escape_mailbox_segment(segment: &str) -> String {
    segment.replace('\\', "\\\\").replace('/', "\\/")
}

/// Split a canonical `/`-delimited mailbox path into non-empty decoded segments.
///
/// A literal slash is written as `\\/` and a literal backslash as `\\\\`.
fn split_mailbox_path(path: &str) -> Result<Vec<String>> {
    if path.is_empty() {
        return Err(Error::Server {
            status: 0,
            body: "JMAP: mailbox path must not be empty".into(),
        });
    }

    let mut segments = Vec::new();
    let mut segment = String::new();
    let mut escaped = false;
    for character in path.chars() {
        if escaped {
            match character {
                '/' | '\\' => segment.push(character),
                _ => {
                    return Err(Error::Server {
                        status: 0,
                        body: format!("JMAP: mailbox path '{path}' has an invalid escape"),
                    });
                }
            }
            escaped = false;
        } else {
            match character {
                '\\' => escaped = true,
                '/' => {
                    if segment.is_empty() {
                        return Err(Error::Server {
                            status: 0,
                            body: format!("JMAP: mailbox path '{path}' contains an empty segment"),
                        });
                    }
                    segments.push(std::mem::take(&mut segment));
                }
                _ => segment.push(character),
            }
        }
    }
    if escaped {
        return Err(Error::Server {
            status: 0,
            body: format!("JMAP: mailbox path '{path}' has a dangling escape"),
        });
    }
    if segment.is_empty() {
        return Err(Error::Server {
            status: 0,
            body: format!("JMAP: mailbox path '{path}' contains an empty segment"),
        });
    }
    segments.push(segment);
    Ok(segments)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures_util::stream;

    use serde_json::json;

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    use super::{
        Error, EventStream, JmapAuth, JmapClient, MAX_SSE_RECORD_BYTES, Mailbox, Session,
        download_url_template, email_blob_id, email_get_batches, email_set_batches,
        expand_download_url_template, expunge_page_progresses, mailbox_paths, next_query_position,
        resolve_mailbox, split_mailbox_path, validate_email_mutation, validate_method_responses,
        validate_query_page,
    };

    fn event_stream(chunks: &[&[u8]]) -> EventStream {
        let chunks: Vec<std::result::Result<Bytes, reqwest::Error>> = chunks
            .iter()
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect();
        EventStream {
            inner: Box::pin(stream::iter(chunks)),
            buf: Vec::new(),
            scan_from: 0,
        }
    }

    #[test]
    fn email_mutations_reject_partial_success_and_item_errors() {
        let updated_error = json!({"methodResponses":[["Email/set", {"notUpdated":{"a":{"type":"forbidden","description":"no"}}}, "u"]]});
        assert!(
            validate_email_mutation(&updated_error, "u", "updated", "notUpdated", &["a"]).is_err()
        );

        let missing_update = json!({"methodResponses":[["Email/set", {"updated":[]}, "u"]]});
        assert!(
            validate_email_mutation(&missing_update, "u", "updated", "notUpdated", &["a"]).is_err()
        );

        let destroyed_error = json!({"methodResponses":[["Email/set", {"notDestroyed":{"a":{"type":"forbidden"}}}, "d"]]});
        assert!(
            validate_email_mutation(&destroyed_error, "d", "destroyed", "notDestroyed", &["a"])
                .is_err()
        );

        let created_error = json!({"methodResponses":[["Email/import", {"notCreated":{"new":{"type":"invalidProperties"}}}, "i"]]});
        assert!(
            validate_email_mutation(&created_error, "i", "created", "notCreated", &["new"])
                .is_err()
        );

        let partial_update = json!({"methodResponses":[["Email/set", {
            "updated":["a"],
            "notUpdated":{"a":{"type":"forbidden"}}
        }, "u"]]});
        assert!(
            validate_email_mutation(&partial_update, "u", "updated", "notUpdated", &["a"]).is_err()
        );

        let success = json!({"methodResponses":[
            ["Email/set", {"updated":["a"], "destroyed":["b"]}, "u"],
            ["Email/import", {"created":{"new":{"id":"e"}}}, "i"]
        ]});
        assert!(validate_email_mutation(&success, "u", "updated", "notUpdated", &["a"]).is_ok());
        assert!(
            validate_email_mutation(&success, "u", "destroyed", "notDestroyed", &["b"]).is_ok()
        );
        assert!(validate_email_mutation(&success, "i", "created", "notCreated", &["new"]).is_ok());
    }

    #[test]
    fn mailbox_paths_preserve_same_named_leaves_and_reject_bad_graphs() {
        let mailboxes = vec![
            Mailbox {
                id: "p".into(),
                name: "Projects".into(),
                parent_id: None,
                role: None,
                total: 0,
                unread: 0,
            },
            Mailbox {
                id: "literal".into(),
                name: "Projects/2026".into(),
                parent_id: None,
                role: None,
                total: 0,
                unread: 0,
            },
            Mailbox {
                id: "a".into(),
                name: "Archive".into(),
                parent_id: None,
                role: None,
                total: 0,
                unread: 0,
            },
            Mailbox {
                id: "p26".into(),
                name: "2026".into(),
                parent_id: Some("p".into()),
                role: None,
                total: 0,
                unread: 0,
            },
            Mailbox {
                id: "a26".into(),
                name: "2026".into(),
                parent_id: Some("a".into()),
                role: None,
                total: 0,
                unread: 0,
            },
        ];
        let paths = mailbox_paths(&mailboxes).unwrap();
        assert_eq!(paths["p26"], "Projects/2026");
        assert_eq!(paths["literal"], "Projects\\/2026");
        assert_eq!(
            resolve_mailbox(&mailboxes, "Projects\\/2026").unwrap().id,
            "literal"
        );
        assert_eq!(
            resolve_mailbox(&mailboxes, "Projects/2026").unwrap().id,
            "p26"
        );
        assert_eq!(
            resolve_mailbox(&mailboxes, "Archive/2026").unwrap().id,
            "a26"
        );
        assert!(resolve_mailbox(&mailboxes, "2026").is_err());
        assert!(
            mailbox_paths(&[Mailbox {
                id: "x".into(),
                name: "X".into(),
                parent_id: Some("missing".into()),
                role: None,
                total: 0,
                unread: 0
            }])
            .is_err()
        );
        assert!(
            mailbox_paths(&[
                Mailbox {
                    id: "x".into(),
                    name: "X".into(),
                    parent_id: Some("y".into()),
                    role: None,
                    total: 0,
                    unread: 0
                },
                Mailbox {
                    id: "y".into(),
                    name: "Y".into(),
                    parent_id: Some("x".into()),
                    role: None,
                    total: 0,
                    unread: 0
                },
            ])
            .is_err()
        );
    }

    #[test]
    fn download_template_encodes_dynamic_values_and_rejects_missing_inputs() {
        assert_eq!(
            expand_download_url_template(
                "https://example.test/{accountId}/{blobId}?type={type}&name={name}",
                "acct/?#%",
                "blob &=",
                "message/rfc822; x",
                "a b?.eml",
            ),
            "https://example.test/acct%2F%3F%23%25/blob%20%26%3D?type=message%2Frfc822%3B%20x&name=a%20b%3F.eml"
        );
        let session: Session =
            serde_json::from_value(json!({"apiUrl": "https://example.test"})).unwrap();
        assert!(download_url_template(&session).is_err());
        assert!(
            email_blob_id(
                &json!({"methodResponses":[["Email/get", {"list":[{}]}, "b"]]}),
                "email"
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn raw_blob_download_rejects_non_success_status() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 7\r\n\r\nmissing")
                .await
                .unwrap();
        });
        let client = JmapClient {
            http: reqwest::Client::new(),
            auth: JmapAuth::Bearer("token".into()),
            session: serde_json::from_value(json!({"apiUrl": "https://example.test"})).unwrap(),
            account_id: "account".into(),
            store: None,
        };
        assert!(matches!(
            client
                .fetch_raw_blob_via_template("blob", &format!("http://{address}/{{blobId}}"))
                .await,
            Err(Error::Server { status: 404, body }) if body == "missing"
        ));
    }

    #[test]
    fn expunge_batches_and_repeated_page_checks_are_used() {
        let ids = (0..7).map(|id| format!("id{id}")).collect::<Vec<_>>();
        assert_eq!(
            email_set_batches(&ids, 3)
                .iter()
                .map(|batch| batch.iter().map(String::as_str).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            vec![
                vec!["id0", "id1", "id2"],
                vec!["id3", "id4", "id5"],
                vec!["id6"]
            ]
        );
        assert!(expunge_page_progresses(None, &ids).is_ok());
        assert!(matches!(
            expunge_page_progresses(Some(&ids), &ids),
            Err(Error::IncompleteQuery)
        ));
    }

    #[test]
    fn download_template_and_set_limit_are_read_from_session() {
        let session: Session = serde_json::from_value(json!({
            "apiUrl": "https://example.test/api",
            "downloadUrl": "https://example.test/download/{accountId}/{blobId}/{type}/{name}",
            "capabilities": {"urn:ietf:params:jmap:core": {"maxObjectsInGet": 2, "maxObjectsInSet": 3}}
        })).unwrap();
        assert_eq!(
            session.download_url.as_deref(),
            Some("https://example.test/download/{accountId}/{blobId}/{type}/{name}")
        );
        assert_eq!(session.max_objects_in_set(), 3);
    }
    #[test]
    fn query_page_rejects_delete_between_pages_and_missing_get_coverage() {
        let collected = vec!["a".to_string(), "b".to_string()];
        assert!(matches!(
            validate_query_page(
                2,
                Some(("state-a", 3)),
                &collected,
                &["c".into()],
                (Some(2), Some("state-b"), Some(2)),
            ),
            Err(Error::IncompleteQuery)
        ));
        assert!(matches!(
            validate_query_page(
                2,
                Some(("state-a", 3)),
                &collected,
                &["b".into()],
                (Some(2), Some("state-a"), Some(3)),
            ),
            Err(Error::IncompleteQuery)
        ));
        assert!(matches!(
            validate_query_page(
                0,
                None,
                &[],
                &["a".into(), "a".into()],
                (Some(0), Some("state-a"), Some(2)),
            ),
            Err(Error::IncompleteQuery)
        ));
    }

    #[test]
    fn method_error_is_not_an_empty_success() {
        let response = json!({
            "methodResponses": [["error", {
                "type": "serverFail",
                "description": "snapshot unavailable"
            }, "q"]]
        });
        assert!(matches!(
            validate_method_responses(&response),
            Err(Error::Method { kind, description })
                if kind == "serverFail" && description == "snapshot unavailable"
        ));
    }

    #[tokio::test]
    async fn event_stream_handles_fragmented_lf_and_crlf_delimiters() {
        let mut stream = event_stream(&[
            b"event: state\r\ndata: {\"a\":",
            b"1}\r\n\r",
            b"\n",
            b"data: {\"b\":2}\n",
            b"\n",
        ]);
        assert_eq!(
            stream.next_event().await.unwrap().as_deref(),
            Some("{\"a\":1}")
        );
        assert_eq!(
            stream.next_event().await.unwrap().as_deref(),
            Some("{\"b\":2}")
        );
    }

    #[tokio::test]
    async fn event_stream_accepts_lone_cr_and_multiple_records_per_chunk() {
        let mut stream = event_stream(&[b"data: one\r\rdata: two\n\r"]);
        assert_eq!(stream.next_event().await.unwrap().as_deref(), Some("one"));
        assert_eq!(stream.next_event().await.unwrap().as_deref(), Some("two"));
    }

    #[tokio::test]
    async fn event_stream_rejects_oversize_record() {
        let chunk = vec![b'x'; MAX_SSE_RECORD_BYTES + 1];
        let mut stream = event_stream(&[&chunk]);
        assert!(matches!(
            stream.next_event().await,
            Err(Error::SseRecordTooLarge(_))
        ));
    }

    #[tokio::test]
    async fn event_stream_rejects_delimiter_terminated_oversize_record() {
        let mut chunk = vec![b'x'; MAX_SSE_RECORD_BYTES + 1];
        chunk.extend_from_slice(b"\n\n");
        let mut stream = event_stream(&[&chunk]);
        assert!(matches!(
            stream.next_event().await,
            Err(Error::SseRecordTooLarge(_))
        ));
    }

    #[tokio::test]
    async fn event_stream_supports_mixed_and_split_lone_cr_boundaries() {
        let mut stream = event_stream(&[b"data: one\n\r", b"\ndata: two\r", b"\r"]);
        assert_eq!(stream.next_event().await.unwrap().as_deref(), Some("one"));
        assert_eq!(stream.next_event().await.unwrap().as_deref(), Some("two"));
    }

    #[test]
    fn email_get_batches_and_query_progression() {
        let ids = ["a", "b", "c", "d", "e"].map(str::to_owned);
        let batches = email_get_batches(&ids, 2);
        assert_eq!(
            batches.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
            vec![2, 2, 1]
        );
        assert_eq!(next_query_position(2, 5, 2).unwrap(), Some(2));
        assert_eq!(next_query_position(4, 5, 2).unwrap(), Some(4));
        assert_eq!(next_query_position(5, 5, 1).unwrap(), None);
        assert!(matches!(
            next_query_position(2, 5, 0),
            Err(Error::IncompleteQuery)
        ));
    }

    #[test]
    fn session_reads_core_batch_cap() {
        let session: Session = serde_json::from_value(serde_json::json!({
            "apiUrl": "https://example.test/api",
            "capabilities": {"urn:ietf:params:jmap:core": {"maxObjectsInGet": 2}}
        }))
        .unwrap();
        assert_eq!(session.max_objects_in_get(), 2);
    }

    #[test]
    fn split_single() {
        assert_eq!(split_mailbox_path("INBOX").unwrap(), vec!["INBOX"]);
    }

    #[test]
    fn split_nested() {
        assert_eq!(
            split_mailbox_path("INBOX/Foo/Bar").unwrap(),
            vec!["INBOX", "Foo", "Bar"]
        );
    }

    #[test]
    fn split_mailbox_path_round_trips_escaped_names() {
        assert_eq!(
            split_mailbox_path("Projects\\/2026/Back\\\\slash").unwrap(),
            ["Projects/2026", "Back\\slash"]
        );
        assert_eq!(
            split_mailbox_path("Projects/2026\\/archive").unwrap(),
            ["Projects", "2026/archive"]
        );
        assert!(split_mailbox_path("dangling\\").is_err());
    }

    #[test]
    fn split_empty_path() {
        assert!(split_mailbox_path("").is_err());
    }

    #[test]
    fn split_empty_segment() {
        assert!(split_mailbox_path("a//b").is_err());
        assert!(split_mailbox_path("/leading").is_err());
        assert!(split_mailbox_path("trailing/").is_err());
    }
}
