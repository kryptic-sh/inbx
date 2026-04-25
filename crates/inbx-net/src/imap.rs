use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_imap::Authenticator;
use async_imap::Session;
use async_imap::imap_proto::types::NameAttribute;
use futures_util::StreamExt;
use inbx_config::{Account, AuthMethod, TlsMode};

use crate::oauth;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufStream};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

pub type ImapSession = Session<TlsStream<TcpStream>>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("imap: {0}")]
    Imap(#[from] async_imap::error::Error),
    #[error("rustls: {0}")]
    Rustls(#[from] rustls::Error),
    #[error("invalid dns name: {0}")]
    InvalidDns(#[from] rustls::pki_types::InvalidDnsNameError),
    #[error("server does not advertise STARTTLS")]
    StarttlsUnsupported,
    #[error("login failed: {0}")]
    Login(String),
    #[error("config: {0}")]
    Config(#[from] inbx_config::Error),
    #[error("oauth: {0}")]
    OAuth(#[from] oauth::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

fn tls_config() -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

async fn upgrade_tls(stream: TcpStream, host: &str) -> Result<TlsStream<TcpStream>> {
    let connector = TlsConnector::from(tls_config());
    let server_name = ServerName::try_from(host.to_string())?;
    Ok(connector.connect(server_name, stream).await?)
}

/// Drive the STARTTLS dance over a raw TCP socket: consume the greeting,
/// issue `A001 STARTTLS`, wait for tagged OK, then return the same socket
/// ready for TLS upgrade. Hard-fails on NO/BAD — never plaintext fallback.
async fn do_starttls(tcp: TcpStream) -> Result<TcpStream> {
    let mut buf = BufStream::new(tcp);
    let mut line = String::new();
    // Greeting: one untagged response starting with "* OK".
    line.clear();
    buf.read_line(&mut line).await?;
    if !line.starts_with("* OK") && !line.starts_with("* PREAUTH") {
        return Err(Error::Login(format!("unexpected greeting: {line:?}")));
    }
    buf.write_all(b"A001 STARTTLS\r\n").await?;
    buf.flush().await?;
    loop {
        line.clear();
        let n = buf.read_line(&mut line).await?;
        if n == 0 {
            return Err(Error::Login("connection closed during STARTTLS".into()));
        }
        if line.starts_with("A001 OK") {
            break;
        }
        if line.starts_with("A001 NO") || line.starts_with("A001 BAD") {
            return Err(Error::StarttlsUnsupported);
        }
        // Untagged response (* ...) — keep reading.
    }
    Ok(buf.into_inner())
}

/// Open an authenticated IMAP session honoring the account's TLS mode and
/// auth method. Resolves credentials from the OS keyring (app password) or
/// performs an OAuth2 refresh and authenticates via XOAUTH2.
pub async fn connect_imap(account: &Account) -> Result<ImapSession> {
    let addr = (account.imap_host.as_str(), account.imap_port);

    let tls_stream = match account.imap_security {
        TlsMode::Tls => {
            let tcp = TcpStream::connect(addr).await?;
            upgrade_tls(tcp, &account.imap_host).await?
        }
        TlsMode::Starttls => {
            let tcp = TcpStream::connect(addr).await?;
            let tcp = do_starttls(tcp).await?;
            upgrade_tls(tcp, &account.imap_host).await?
        }
    };

    let client = async_imap::Client::new(tls_stream);
    let session = match &account.auth {
        AuthMethod::AppPassword => {
            let password = inbx_config::load_password(&account.name)?;
            client
                .login(&account.username, &password)
                .await
                .map_err(|(e, _)| Error::Login(e.to_string()))?
        }
        AuthMethod::OAuth2 { provider, .. } => {
            let refresh = inbx_config::load_refresh_token(&account.name)?;
            let access = oauth::refresh(&account.auth, provider, &refresh).await?;
            let auth = Xoauth2Authenticator::new(&account.email, &access);
            client
                .authenticate("XOAUTH2", auth)
                .await
                .map_err(|(e, _)| Error::Login(e.to_string()))?
        }
    };
    Ok(session)
}

struct Xoauth2Authenticator {
    sasl: String,
    state: u8,
}

impl Xoauth2Authenticator {
    fn new(email: &str, access_token: &str) -> Self {
        let sasl = format!("user={email}\x01auth=Bearer {access_token}\x01\x01");
        Self { sasl, state: 0 }
    }
}

impl Authenticator for Xoauth2Authenticator {
    type Response = Vec<u8>;
    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        let out = if self.state == 0 {
            self.sasl.clone().into_bytes()
        } else {
            // Server returned an error JSON challenge; ack with empty.
            Vec::new()
        };
        self.state += 1;
        out
    }
}

#[derive(Debug, Clone)]
pub struct FolderInfo {
    pub name: String,
    pub delim: Option<String>,
    pub special_use: Option<String>,
    pub attrs: Vec<String>,
    pub selectable: bool,
}

/// Enumerate all mailboxes via `LIST "" "*"`.
pub async fn list_folders(session: &mut ImapSession) -> Result<Vec<FolderInfo>> {
    let stream = session.list(Some(""), Some("*")).await?;
    let names: Vec<async_imap::types::Name> =
        stream.filter_map(|r| async move { r.ok() }).collect().await;

    let mut out = Vec::with_capacity(names.len());
    for n in &names {
        let mut attrs = Vec::new();
        let mut special_use = None;
        let mut selectable = true;
        for a in n.attributes() {
            match a {
                NameAttribute::NoSelect => {
                    selectable = false;
                    attrs.push("\\Noselect".into());
                }
                NameAttribute::NoInferiors => attrs.push("\\Noinferiors".into()),
                NameAttribute::Marked => attrs.push("\\Marked".into()),
                NameAttribute::Unmarked => attrs.push("\\Unmarked".into()),
                NameAttribute::All => special_use = Some("\\All".into()),
                NameAttribute::Archive => special_use = Some("\\Archive".into()),
                NameAttribute::Drafts => special_use = Some("\\Drafts".into()),
                NameAttribute::Flagged => special_use = Some("\\Flagged".into()),
                NameAttribute::Junk => special_use = Some("\\Junk".into()),
                NameAttribute::Sent => special_use = Some("\\Sent".into()),
                NameAttribute::Trash => special_use = Some("\\Trash".into()),
                _ => {}
            }
        }
        out.push(FolderInfo {
            name: n.name().to_string(),
            delim: n.delimiter().map(|s| s.to_string()),
            special_use,
            attrs,
            selectable,
        });
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct HeaderRow {
    pub uid: u32,
    pub uidvalidity: u32,
    pub message_id: Option<String>,
    pub subject: Option<String>,
    pub from_addr: Option<String>,
    pub to_addrs: Option<String>,
    pub date_unix: Option<i64>,
    pub flags: String,
    pub fetched_at_unix: i64,
}

/// Select INBOX and fetch envelope + flags for all messages, UID-keyed.
/// Returns (uidvalidity, rows). M2 fetches headers only — body lazy.
pub async fn fetch_inbox_headers(session: &mut ImapSession) -> Result<(u32, Vec<HeaderRow>)> {
    let mailbox = session.select("INBOX").await?;
    let uidvalidity = mailbox.uid_validity.unwrap_or(0);
    if mailbox.exists == 0 {
        return Ok((uidvalidity, Vec::new()));
    }

    let stream = session
        .uid_fetch("1:*", "(UID FLAGS ENVELOPE INTERNALDATE)")
        .await?;
    let fetches: Vec<async_imap::types::Fetch> =
        stream.filter_map(|r| async move { r.ok() }).collect().await;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let mut out = Vec::with_capacity(fetches.len());
    for f in fetches {
        let Some(uid) = f.uid else { continue };
        let env = f.envelope();
        let subject = env
            .and_then(|e| e.subject.as_ref())
            .map(|b| String::from_utf8_lossy(b).into_owned());
        let message_id = env
            .and_then(|e| e.message_id.as_ref())
            .map(|b| String::from_utf8_lossy(b).into_owned());
        let from_addr = env.and_then(|e| e.from.as_ref()).map(|v| format_addrs(v));
        let to_addrs = env.and_then(|e| e.to.as_ref()).map(|v| format_addrs(v));
        let date_unix = f.internal_date().map(|d| d.timestamp());

        let flags: Vec<String> = f.flags().map(|fl| format!("{fl:?}")).collect();
        let flags = flags.join(" ");

        out.push(HeaderRow {
            uid,
            uidvalidity,
            message_id,
            subject,
            from_addr,
            to_addrs,
            date_unix,
            flags,
            fetched_at_unix: now,
        });
    }
    Ok((uidvalidity, out))
}

/// Fetch raw RFC 5322 bodies (`BODY.PEEK[]`) for the listed UIDs from the
/// currently-selected mailbox. Returns (uid, raw_bytes) pairs.
pub async fn fetch_bodies(
    session: &mut ImapSession,
    folder: &str,
    uids: &[u32],
) -> Result<Vec<(u32, Vec<u8>)>> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    session.select(folder).await?;
    let set = uid_set(uids);
    let stream = session.uid_fetch(set, "(UID BODY.PEEK[])").await?;
    let fetches: Vec<async_imap::types::Fetch> =
        stream.filter_map(|r| async move { r.ok() }).collect().await;
    let mut out = Vec::with_capacity(fetches.len());
    for f in fetches {
        let Some(uid) = f.uid else { continue };
        if let Some(body) = f.body() {
            out.push((uid, body.to_vec()));
        }
    }
    Ok(out)
}

fn uid_set(uids: &[u32]) -> String {
    uids.iter()
        .map(|u| u.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Locate the Sent folder by SPECIAL-USE flag, falling back to common names.
pub fn find_sent_folder(folders: &[FolderInfo]) -> Option<String> {
    if let Some(f) = folders
        .iter()
        .find(|f| f.special_use.as_deref() == Some("\\Sent"))
    {
        return Some(f.name.clone());
    }
    for guess in [
        "Sent",
        "INBOX/Sent",
        "Sent Items",
        "Sent Mail",
        "[Gmail]/Sent Mail",
    ] {
        if let Some(f) = folders.iter().find(|f| f.name.eq_ignore_ascii_case(guess)) {
            return Some(f.name.clone());
        }
    }
    None
}

/// APPEND raw RFC 5322 message into the named folder, marking it `\Seen`.
pub async fn append_message(session: &mut ImapSession, folder: &str, raw: &[u8]) -> Result<()> {
    session.append(folder, Some("(\\Seen)"), None, raw).await?;
    Ok(())
}

fn format_addrs(addrs: &[async_imap::imap_proto::types::Address<'_>]) -> String {
    addrs
        .iter()
        .map(|a| {
            let mailbox = a
                .mailbox
                .as_ref()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            let host = a
                .host
                .as_ref()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            let name = a
                .name
                .as_ref()
                .map(|b| String::from_utf8_lossy(b).into_owned());
            match name {
                Some(n) if !n.is_empty() => format!("{n} <{mailbox}@{host}>"),
                _ => format!("{mailbox}@{host}"),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}
