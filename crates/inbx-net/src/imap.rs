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
    Oauth(#[from] oauth::Error),
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
/// performs an Oauth2 refresh and authenticates via XOAUTH2.
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
        AuthMethod::Oauth2 { provider, .. } => {
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

/// UID SEARCH SINCE <date>. `days_ago` of 0 means no filter (all messages).
/// Date is formatted as IMAP-style `DD-Mon-YYYY` (e.g. `1-Jan-2026`).
pub async fn search_since(
    session: &mut ImapSession,
    folder: &str,
    days_ago: u32,
) -> Result<Vec<u32>> {
    session.select(folder).await?;
    if days_ago == 0 {
        // Return everything — caller will fetch_headers without a filter.
        return Ok(Vec::new());
    }
    let cutoff = days_ago_to_imap_date(days_ago);
    let uids = session.uid_search(format!("SINCE {cutoff}")).await?;
    Ok(uids.into_iter().collect())
}

fn days_ago_to_imap_date(days: u32) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let target = now - (days as i64) * 86_400;
    // civil_from_days
    let days_total = target / 86_400;
    let z = days_total + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!("{d}-{}-{year:04}", MONTHS[(m - 1) as usize])
}

/// Select a folder and fetch envelope + flags for the UID set. When
/// `uids_filter` is None, fetches the entire mailbox (1:*). Returns
/// (uidvalidity, rows). Body fetched lazily by `fetch_bodies`.
pub async fn fetch_headers_uids(
    session: &mut ImapSession,
    folder: &str,
    uids_filter: Option<&[u32]>,
) -> Result<(u32, Vec<HeaderRow>)> {
    let mailbox = session.select(folder).await?;
    let uidvalidity = mailbox.uid_validity.unwrap_or(0);
    if mailbox.exists == 0 {
        return Ok((uidvalidity, Vec::new()));
    }
    let set = match uids_filter {
        Some([]) => return Ok((uidvalidity, Vec::new())),
        Some(uids) => uids
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(","),
        None => "1:*".to_string(),
    };
    let stream = session
        .uid_fetch(set, "(UID FLAGS ENVELOPE INTERNALDATE)")
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

/// Select a folder and fetch envelope + flags for all messages, UID-keyed.
/// Returns (uidvalidity, rows). Body fetched lazily by `fetch_bodies`.
pub async fn fetch_headers(
    session: &mut ImapSession,
    folder: &str,
) -> Result<(u32, Vec<HeaderRow>)> {
    let mailbox = session.select(folder).await?;
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

/// Backwards-compat alias kept for existing CLI sites.
pub async fn fetch_inbox_headers(session: &mut ImapSession) -> Result<(u32, Vec<HeaderRow>)> {
    fetch_headers(session, "INBOX").await
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

/// Locate the Drafts folder by SPECIAL-USE flag with name fallbacks.
pub fn find_drafts_folder(folders: &[FolderInfo]) -> Option<String> {
    if let Some(f) = folders
        .iter()
        .find(|f| f.special_use.as_deref() == Some("\\Drafts"))
    {
        return Some(f.name.clone());
    }
    for guess in ["Drafts", "INBOX/Drafts", "Draft", "[Gmail]/Drafts"] {
        if let Some(f) = folders.iter().find(|f| f.name.eq_ignore_ascii_case(guess)) {
            return Some(f.name.clone());
        }
    }
    None
}

/// APPEND a draft into the named folder with `\Draft` (and `\Seen`).
pub async fn append_draft(session: &mut ImapSession, folder: &str, raw: &[u8]) -> Result<()> {
    session
        .append(folder, Some("(\\Seen \\Draft)"), None, raw)
        .await?;
    Ok(())
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

/// UID STORE flags on the selected folder. `op` is one of `+FLAGS`,
/// `-FLAGS`, or `FLAGS` (set). Flags should include the leading `\` for
/// system flags.
pub async fn store_flags(
    session: &mut ImapSession,
    folder: &str,
    uids: &[u32],
    op: &str,
    flags: &str,
) -> Result<()> {
    if uids.is_empty() {
        return Ok(());
    }
    session.select(folder).await?;
    let set = uids
        .iter()
        .map(|u| u.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let arg = format!("({flags})");
    let stream = session.uid_store(set, format!("{op} {arg}")).await?;
    let _: Vec<async_imap::types::Fetch> =
        stream.filter_map(|r| async move { r.ok() }).collect().await;
    Ok(())
}

/// Select a folder and EXPUNGE all `\Deleted` messages.
pub async fn expunge_folder(session: &mut ImapSession, folder: &str) -> Result<u32> {
    session.select(folder).await?;
    let stream = session.expunge().await?;
    let removed: Vec<u32> = stream.filter_map(|r| async move { r.ok() }).collect().await;
    Ok(removed.len() as u32)
}

/// UID COPY messages from current mailbox to `target`. Caller must have
/// SELECTed the source folder first.
pub async fn uid_copy(
    session: &mut ImapSession,
    source: &str,
    uids: &[u32],
    target: &str,
) -> Result<()> {
    if uids.is_empty() {
        return Ok(());
    }
    session.select(source).await?;
    let set = uids
        .iter()
        .map(|u| u.to_string())
        .collect::<Vec<_>>()
        .join(",");
    session.uid_copy(set, target).await?;
    Ok(())
}

/// UID MOVE (RFC 6851).
pub async fn uid_move(
    session: &mut ImapSession,
    source: &str,
    uids: &[u32],
    target: &str,
) -> Result<()> {
    if uids.is_empty() {
        return Ok(());
    }
    session.select(source).await?;
    let set = uids
        .iter()
        .map(|u| u.to_string())
        .collect::<Vec<_>>()
        .join(",");
    session.uid_mv(set, target).await?;
    Ok(())
}

/// IMAP CREATE.
pub async fn create_folder(session: &mut ImapSession, name: &str) -> Result<()> {
    session.create(name).await?;
    Ok(())
}

/// IMAP DELETE.
pub async fn delete_folder(session: &mut ImapSession, name: &str) -> Result<()> {
    session.delete(name).await?;
    Ok(())
}

/// IMAP RENAME.
pub async fn rename_folder(session: &mut ImapSession, from: &str, to: &str) -> Result<()> {
    session.rename(from, to).await?;
    Ok(())
}

/// IMAP SUBSCRIBE / UNSUBSCRIBE.
pub async fn subscribe_folder(session: &mut ImapSession, name: &str, on: bool) -> Result<()> {
    if on {
        session.subscribe(name).await?;
    } else {
        session.unsubscribe(name).await?;
    }
    Ok(())
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
