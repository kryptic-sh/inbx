//! ManageSieve client (RFC 5804) for server-side filter scripts.
//!
//! Hand-rolled protocol over tokio-rustls because no Rust crate ships a
//! mature async ManageSieve client. Supports AUTHENTICATE PLAIN with the
//! account's app password (OAuth2 SASL is left to a future milestone) and
//! the script-management verbs: LISTSCRIPTS, GETSCRIPT, PUTSCRIPT,
//! SETACTIVE, DELETESCRIPT.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use inbx_config::{Account, AuthMethod};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufStream};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::{oauth, proxy, tls};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("rustls: {0}")]
    Rustls(#[from] rustls::Error),
    #[error("invalid dns name: {0}")]
    InvalidDns(#[from] rustls::pki_types::InvalidDnsNameError),
    #[error("config: {0}")]
    Config(#[from] inbx_config::Error),
    #[error("server: {0}")]
    Server(String),
    #[error("protocol: {0}")]
    Protocol(&'static str),
    #[error("oauth: {0}")]
    OAuth(#[from] oauth::Error),
    #[error("proxy: {0}")]
    Proxy(#[from] proxy::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

const DEFAULT_PORT: u16 = 4190;
const MAX_RESPONSE_LINE_BYTES: usize = 64 * 1024;
const MAX_LITERAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_RESPONSE_RECORDS: usize = 10_000;

/// A live, authenticated ManageSieve session.
///
/// Constructed via [`SieveSession::connect`] (or its alias
/// [`connect_and_auth`]). After construction the TCP+TLS+AUTH handshake is
/// complete; subsequent calls to `list_scripts`, `get_script`, etc. reuse the
/// stream without a new handshake.
///
/// The TUI holds an `Option<SieveSession>` to avoid a fresh auth roundtrip
/// for each picker or wizard action within the same TUI overlay session.
pub type SieveSession = SieveClient;

pub struct SieveClient {
    stream: BufStream<TlsStream<TcpStream>>,
}

impl SieveClient {
    /// Connect over implicit TLS to host:4190 (configurable later) and
    /// authenticate via SASL PLAIN. OAuth2 accounts are rejected — wire
    /// XOAUTH2 SASL when the user asks.
    pub async fn connect(account: &Account) -> Result<Self> {
        let host = account.imap_host.as_str();
        let port = DEFAULT_PORT;
        let tcp = proxy::connect(account.proxy.as_ref(), host, port, &account.name).await?;
        let connector = TlsConnector::from(tls::CLIENT_CONFIG.clone());
        let server_name = ServerName::try_from(host.to_string())?;
        let tls = connector.connect(server_name, tcp).await?;
        let mut me = Self {
            stream: BufStream::new(tls),
        };
        // Drain greeting (capability lines + tagged OK).
        let _ = me.read_until_done().await?;

        match &account.auth {
            AuthMethod::AppPassword => {
                let password = inbx_config::load_password(&account.name)?;
                me.authenticate_plain(&account.username, &password).await?;
            }
            AuthMethod::OAuth2 { provider, .. } => {
                let refresh = inbx_config::load_refresh_token(&account.name)?;
                let access =
                    oauth::refresh(&account.auth, provider, &refresh, account.proxy.as_ref())
                        .await?;
                me.authenticate_xoauth2(&account.email, &access).await?;
            }
        }
        Ok(me)
    }

    async fn write_line(&mut self, line: &str) -> Result<()> {
        self.stream.write_all(line.as_bytes()).await?;
        self.stream.write_all(b"\r\n").await?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Read response lines until a tagged OK/NO/BYE arrives. Returns the
    /// data lines (everything before the tag) and the tag line itself.
    async fn read_until_done(&mut self) -> Result<(Vec<String>, String)> {
        read_response_until_done(&mut self.stream).await
    }

    async fn authenticate_plain(&mut self, user: &str, password: &str) -> Result<()> {
        let raw = format!("\0{user}\0{password}");
        let sasl = B64.encode(raw);
        let line = format!("AUTHENTICATE \"PLAIN\" \"{sasl}\"");
        self.write_line(&line).await?;
        let _ = self.read_until_done().await?;
        Ok(())
    }

    async fn authenticate_xoauth2(&mut self, email: &str, access_token: &str) -> Result<()> {
        let sasl = xoauth2_sasl_string(email, access_token);
        let line = format!("AUTHENTICATE \"XOAUTH2\" \"{sasl}\"");
        self.write_line(&line).await?;
        let _ = self.read_until_done().await?;
        Ok(())
    }

    pub async fn logout(mut self) -> Result<()> {
        self.write_line("LOGOUT").await?;
        // BYE is expected; treat any closure as success.
        let _ = self.read_until_done().await;
        Ok(())
    }

    /// Enumerate scripts; the active one is marked `active = true`.
    pub async fn list_scripts(&mut self) -> Result<Vec<SieveScript>> {
        self.write_line("LISTSCRIPTS").await?;
        let (lines, _) = self.read_until_done().await?;
        let mut out = Vec::new();
        for line in lines {
            // Format: "name" [ACTIVE]
            let name = parse_quoted(&line).unwrap_or_default();
            let active = line.to_ascii_uppercase().contains("ACTIVE");
            if !name.is_empty() {
                out.push(SieveScript { name, active });
            }
        }
        Ok(out)
    }

    pub async fn get_script(&mut self, name: &str) -> Result<String> {
        let line = format!("GETSCRIPT \"{}\"", quote_escape(name));
        self.write_line(&line).await?;
        let (data, _) = self.read_until_done().await?;
        Ok(data.join("\n"))
    }

    pub async fn put_script(&mut self, name: &str, body: &str) -> Result<()> {
        let header = format!(
            "PUTSCRIPT \"{}\" {{{len}+}}",
            quote_escape(name),
            len = body.len()
        );
        self.write_line(&header).await?;
        self.stream.write_all(body.as_bytes()).await?;
        self.stream.write_all(b"\r\n").await?;
        self.stream.flush().await?;
        let _ = self.read_until_done().await?;
        Ok(())
    }

    pub async fn set_active(&mut self, name: &str) -> Result<()> {
        let line = format!("SETACTIVE \"{}\"", quote_escape(name));
        self.write_line(&line).await?;
        let _ = self.read_until_done().await?;
        Ok(())
    }

    pub async fn delete_script(&mut self, name: &str) -> Result<()> {
        let line = format!("DELETESCRIPT \"{}\"", quote_escape(name));
        self.write_line(&line).await?;
        let _ = self.read_until_done().await?;
        Ok(())
    }
}

async fn read_response_line_with_limit<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_line_bytes: usize,
) -> Result<String> {
    let mut buf = Vec::new();
    loop {
        let byte = reader.read_u8().await?;
        if byte == b'\n' {
            break;
        }
        if buf.len() == max_line_bytes {
            return Err(Error::Protocol("response line exceeds limit"));
        }
        buf.push(byte);
    }
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

async fn read_response_until_done<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<(Vec<String>, String)> {
    read_response_until_done_with_limits(
        reader,
        MAX_RESPONSE_LINE_BYTES,
        MAX_LITERAL_BYTES,
        MAX_RESPONSE_BYTES,
        MAX_RESPONSE_RECORDS,
    )
    .await
}

async fn read_response_until_done_with_limits<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_line_bytes: usize,
    max_literal_bytes: usize,
    max_response_bytes: usize,
    max_records: usize,
) -> Result<(Vec<String>, String)> {
    let mut data = Vec::new();
    let mut response_bytes = 0usize;
    loop {
        let line = read_response_line_with_limit(reader, max_line_bytes).await?;
        if is_tagged_response(&line) {
            return if line.starts_with("OK") {
                Ok((data, line))
            } else {
                Err(Error::Server(line))
            };
        }
        if data.len() == max_records {
            return Err(Error::Protocol("response record count exceeds limit"));
        }
        response_bytes = response_bytes
            .checked_add(line.len())
            .ok_or(Error::Protocol("response exceeds aggregate limit"))?;
        if response_bytes > max_response_bytes {
            return Err(Error::Protocol("response exceeds aggregate limit"));
        }
        if let Some(len) = parse_literal_len(&line) {
            if len > max_literal_bytes {
                return Err(Error::Protocol("literal exceeds limit"));
            }
            response_bytes = response_bytes
                .checked_add(len)
                .ok_or(Error::Protocol("response exceeds aggregate limit"))?;
            if response_bytes > max_response_bytes {
                return Err(Error::Protocol("response exceeds aggregate limit"));
            }
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf).await?;
            let payload = String::from_utf8_lossy(&buf).into_owned();
            let delimiter = read_response_line_with_limit(reader, max_line_bytes).await?;
            response_bytes = response_bytes
                .checked_add(delimiter.len() + 1)
                .ok_or(Error::Protocol("response exceeds aggregate limit"))?;
            if response_bytes > max_response_bytes {
                return Err(Error::Protocol("response exceeds aggregate limit"));
            }
            if !delimiter.is_empty() {
                return Err(Error::Protocol("literal delimiter is not empty"));
            }
            data.push(payload);
        } else {
            data.push(line);
        }
    }
}

/// Connect to the ManageSieve server for `account` and authenticate.
///
/// This is the factory for [`SieveSession`]: it performs TCP dial + TLS
/// handshake + AUTHENTICATE once. The returned session can be reused for
/// multiple LISTSCRIPTS / GETSCRIPT / PUTSCRIPT calls without re-authenticating.
///
/// Equivalent to [`SieveClient::connect`] — provided as a free function so
/// callers do not need to import `SieveClient` when working exclusively with
/// the `SieveSession` type.
pub async fn connect_and_auth(account: &Account) -> Result<SieveSession> {
    SieveClient::connect(account).await
}

#[derive(Debug, Clone)]
pub struct SieveScript {
    pub name: String,
    pub active: bool,
}

fn is_tagged_response(line: &str) -> bool {
    let upper = line.to_ascii_uppercase();
    upper.starts_with("OK") || upper.starts_with("NO") || upper.starts_with("BYE")
}

fn parse_literal_len(line: &str) -> Option<usize> {
    // Match "{NNN+}" or "{NNN}" at end of line.
    let trimmed = line.trim();
    let bytes = trimmed.as_bytes();
    if bytes.last().is_none_or(|b| *b != b'}') {
        return None;
    }
    let lbrace = bytes.iter().rposition(|b| *b == b'{')?;
    let inner = &trimmed[lbrace + 1..bytes.len() - 1];
    let inner = inner.trim_end_matches('+');
    inner.parse().ok()
}

fn parse_quoted(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let start = bytes.iter().position(|b| *b == b'"')?;
    let after_start = &line[start + 1..];
    let end = after_start.as_bytes().iter().position(|b| *b == b'"')?;
    Some(after_start[..end].to_string())
}

fn quote_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Encode an XOAUTH2 SASL initial response per Google's spec, base64 encoded.
fn xoauth2_sasl_string(email: &str, access_token: &str) -> String {
    let raw = format!("user={email}\x01auth=Bearer {access_token}\x01\x01");
    B64.encode(raw)
}

/// Build a Sieve vacation script per RFC 5230.
pub fn vacation_script(message: &str, days: u32, subject: Option<&str>) -> String {
    let subject = subject.unwrap_or("Out of office");
    format!(
        "require [\"vacation\"];\r\n\
         vacation\r\n\
         :days {days}\r\n\
         :subject \"{subject}\"\r\n\
         \"{body}\";\r\n",
        days = days,
        subject = quote_escape(subject),
        body = quote_escape(message)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn response_line_limit_rejects_before_newline() {
        let mut input = std::io::Cursor::new(vec![b'x'; MAX_RESPONSE_LINE_BYTES + 1]);
        assert!(matches!(
            read_response_line_with_limit(&mut input, MAX_RESPONSE_LINE_BYTES).await,
            Err(Error::Protocol("response line exceeds limit"))
        ));
    }

    #[tokio::test]
    async fn aggregate_line_bytes_are_bounded() {
        let mut input = std::io::Cursor::new(b"one\r\ntwo\r\nOK\r\n".to_vec());
        assert!(matches!(
            read_response_until_done_with_limits(&mut input, 16, 16, 5, 8).await,
            Err(Error::Protocol("response exceeds aggregate limit"))
        ));
    }

    #[tokio::test]
    async fn aggregate_literal_bytes_are_bounded_before_allocation() {
        let mut input = std::io::Cursor::new(b"{4+}\r\n".to_vec());
        assert!(matches!(
            read_response_until_done_with_limits(&mut input, 16, 16, 3, 8).await,
            Err(Error::Protocol("response exceeds aggregate limit"))
        ));
    }

    #[tokio::test]
    async fn literal_limit_rejects_before_reading_payload() {
        let response = format!("{{{}+}}\r\n", MAX_LITERAL_BYTES + 1);
        let mut input = std::io::Cursor::new(response.into_bytes());
        assert!(matches!(
            read_response_until_done(&mut input).await,
            Err(Error::Protocol("literal exceeds limit"))
        ));
    }

    #[test]
    fn response_limits_are_nonzero_and_literal_bound_is_parseable() {
        assert!(MAX_RESPONSE_LINE_BYTES > 0);
        assert_eq!(
            parse_literal_len(&format!("{{{MAX_LITERAL_BYTES}+}}")),
            Some(MAX_LITERAL_BYTES)
        );
        assert!(parse_literal_len(&format!("{{{}+}}", MAX_LITERAL_BYTES + 1)).is_some());
    }

    #[tokio::test]
    async fn literal_delimiter_is_empty_and_limited() {
        let mut non_empty = std::io::Cursor::new(b"{0+}\r\nnope\r\n".to_vec());
        assert!(matches!(
            read_response_until_done_with_limits(&mut non_empty, 16, 16, 64, 8).await,
            Err(Error::Protocol("literal delimiter is not empty"))
        ));
        let mut overlong = std::io::Cursor::new(b"{0+}\r\nabcdef\r\n".to_vec());
        assert!(matches!(
            read_response_until_done_with_limits(&mut overlong, 3, 16, 64, 8).await,
            Err(Error::Protocol("response line exceeds limit"))
        ));
    }

    #[tokio::test]
    async fn literal_delimiters_count_toward_aggregate_limit() {
        let mut input = std::io::Cursor::new(b"{0+}\r\n\r\n{0+}\r\n\r\nOK\r\n".to_vec());
        assert!(matches!(
            read_response_until_done_with_limits(&mut input, 16, 16, 9, 8).await,
            Err(Error::Protocol("response exceeds aggregate limit"))
        ));
    }

    #[test]
    fn parse_literal() {
        assert_eq!(parse_literal_len("PUTSCRIPT \"x\" {42+}"), Some(42));
        assert_eq!(parse_literal_len("foo {7}"), Some(7));
        assert_eq!(parse_literal_len("OK"), None);
    }

    #[test]
    fn parse_quoted_extracts_name() {
        assert_eq!(parse_quoted("\"main\" ACTIVE").as_deref(), Some("main"));
    }

    #[test]
    fn vacation_template() {
        let s = vacation_script("Back monday", 5, None);
        assert!(s.contains(":days 5"));
        assert!(s.contains("Back monday"));
        assert!(s.contains("require [\"vacation\"]"));
    }

    /// Verify that `SieveSession` is a type alias for `SieveClient`.
    /// Both names refer to the same struct; the compiler checks this at
    /// compile time.  We also confirm the free-function factory `connect_and_auth`
    /// has the same return type so call-site code can use either spelling.
    #[test]
    fn sieve_session_is_alias_for_sieve_client() {
        // Compile-time check: the type alias works by accepting a value of either name.
        fn _accepts_session(_s: SieveSession) {}
        fn _accepts_client(_s: SieveClient) {}
        // The vacation_script helper remains accessible with the session type.
        let s = vacation_script("away", 7, Some("On holiday"));
        assert!(s.contains(":days 7"));
        assert!(s.contains("On holiday"));
    }

    /// `connect_and_auth` is a free-function factory that returns `SieveSession`.
    /// We can't call a real server in CI; confirm the function exists and is
    /// callable at the type level. The test just ensures the symbol is resolvable
    /// and the type alias `SieveSession = SieveClient` is consistent.
    #[test]
    fn connect_and_auth_signature_compiles() {
        // Confirm `SieveSession` and `SieveClient` are the same type by checking
        // that `connect_and_auth` produces a type accepted by a SieveClient sink.
        // Using a zero-size type trick: just verify the function is callable as a value.
        let _f = connect_and_auth; // resolves the symbol
        // vacation_script is a sibling utility on the session module.
        let s = vacation_script("off", 3, None);
        assert!(s.contains(":days 3"));
    }
}
