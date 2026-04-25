//! Microsoft Graph backend for Outlook / Microsoft 365.
//!
//! Lives next to the IMAP/SMTP path so individual accounts can opt in. Uses
//! the same OAuth2 refresh-token storage as IMAP-side XOAUTH2.

use std::time::Duration;

use inbx_config::{Account, AuthMethod, OAuthProvider};
use serde::Deserialize;

use crate::oauth;

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
}

pub type Result<T> = std::result::Result<T, Error>;

/// Authenticated Graph client for one account. Fresh access token per session.
pub struct GraphClient {
    http: reqwest::Client,
    token: String,
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
        let token = oauth::refresh(&account.auth, &provider, &refresh).await?;
        let http = reqwest::ClientBuilder::new()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { http, token })
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
        loop {
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

    /// List messages in a folder, newest first. Headers only — body fetched lazy.
    pub async fn list_messages(&self, folder_id: &str, limit: u32) -> Result<Vec<GraphMessage>> {
        let url = format!(
            "https://graph.microsoft.com/v1.0/me/mailFolders/{folder_id}/messages\
             ?$top={limit}&$orderby=receivedDateTime desc\
             &$select=id,subject,from,toRecipients,receivedDateTime,internetMessageId,isRead,flag"
        );
        let res = self.get(&url).await?;
        let page: Page<GraphMessage> = res.json().await?;
        Ok(page.value)
    }

    /// Walk Graph's delta endpoint for a folder. Pass `None` for the first
    /// run; pass the previously-stored deltaLink on subsequent runs to fetch
    /// only changes. Returns `(messages, new_delta_link)`.
    pub async fn delta_messages(
        &self,
        folder_id: &str,
        delta_link: Option<&str>,
    ) -> Result<(Vec<GraphMessage>, Option<String>)> {
        let mut url = match delta_link {
            Some(link) => link.to_string(),
            None => format!(
                "https://graph.microsoft.com/v1.0/me/mailFolders/{folder_id}/messages/delta\
                 ?$select=id,subject,from,toRecipients,receivedDateTime,internetMessageId,isRead"
            ),
        };
        let mut messages = Vec::new();
        let mut next_delta: Option<String> = None;
        loop {
            let res = self.get(&url).await?;
            let page: DeltaPage<GraphMessage> = res.json().await?;
            messages.extend(page.value);
            if let Some(d) = page.delta_link {
                next_delta = Some(d);
                break;
            }
            match page.next_link {
                Some(n) => url = n,
                None => break,
            }
        }
        Ok((messages, next_delta))
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
        if !res.status().is_success() {
            let status = res.status().as_u16();
            let body = res.text().await.unwrap_or_default();
            return Err(Error::Api { status, body });
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct Page<T> {
    value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeltaPage<T> {
    value: Vec<T>,
    #[serde(rename = "@odata.nextLink", default)]
    next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink", default)]
    delta_link: Option<String>,
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
