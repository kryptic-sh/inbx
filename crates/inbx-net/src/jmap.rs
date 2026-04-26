//! Minimal JMAP (RFC 8620 / RFC 8621) client.
//!
//! Hand-rolled over reqwest because jmap-client crates churn fast. Targets
//! Fastmail / Stalwart. Auth is HTTP basic with the account's app password
//! (Bearer-token / Oauth wiring lives in the Oauth module and can attach
//! later). Implements the bare slice we need to fetch headers and send
//! mail; everything else (push, vacation, Sieve mgmt) lives in the
//! provider's own protocol path.

use std::time::Duration;

use inbx_config::{Account, AuthMethod};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::oauth;

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
    Oauth(#[from] oauth::Error),
}

/// Either basic auth (app password) or Bearer (Oauth2 access token).
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
            AuthMethod::Oauth2 { provider, .. } => {
                let refresh = inbx_config::load_refresh_token(&account.name)?;
                let access = oauth::refresh(&account.auth, provider, &refresh).await?;
                JmapAuth::Bearer(access)
            }
        };
        let http = reqwest::ClientBuilder::new()
            .timeout(Duration::from_secs(30))
            .build()?;
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
