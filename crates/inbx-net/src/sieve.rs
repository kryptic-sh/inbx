//! ManageSieve client (RFC 5804) for server-side filter scripts.
//!
//! Hand-rolled protocol over tokio-rustls because no Rust crate ships a
//! mature async ManageSieve client. Supports AUTHENTICATE PLAIN with the
//! account's app password (OAuth2 SASL is left to a future milestone) and
//! the script-management verbs: LISTSCRIPTS, GETSCRIPT, PUTSCRIPT,
//! SETACTIVE, DELETESCRIPT.

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use inbx_config::{Account, AuthMethod};
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufStream};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::oauth;

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
}

pub type Result<T> = std::result::Result<T, Error>;

const DEFAULT_PORT: u16 = 4190;

pub struct SieveClient {
    stream: BufStream<TlsStream<TcpStream>>,
}

fn tls_config() -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

impl SieveClient {
    /// Connect over implicit TLS to host:4190 (configurable later) and
    /// authenticate via SASL PLAIN. OAuth2 accounts are rejected — wire
    /// XOAUTH2 SASL when the user asks.
    pub async fn connect(account: &Account) -> Result<Self> {
        let host = account.imap_host.as_str();
        let port = DEFAULT_PORT;
        let tcp = TcpStream::connect((host, port)).await?;
        let connector = TlsConnector::from(tls_config());
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
                let access = oauth::refresh(&account.auth, provider, &refresh).await?;
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

    /// Read one line, including its trailing CRLF stripped.
    async fn read_line(&mut self) -> Result<String> {
        let mut buf = String::new();
        let n = self.stream.read_line(&mut buf).await?;
        if n == 0 {
            return Err(Error::Protocol("connection closed"));
        }
        if buf.ends_with('\n') {
            buf.pop();
            if buf.ends_with('\r') {
                buf.pop();
            }
        }
        Ok(buf)
    }

    /// Read response lines until a tagged OK/NO/BYE arrives. Returns the
    /// data lines (everything before the tag) and the tag line itself.
    async fn read_until_done(&mut self) -> Result<(Vec<String>, String)> {
        let mut data = Vec::new();
        loop {
            let line = self.read_line().await?;
            if is_tagged_response(&line) {
                if line.starts_with("OK") {
                    return Ok((data, line));
                }
                return Err(Error::Server(line));
            }
            // {literal-len+}: read that many bytes then the rest of line.
            if let Some(len) = parse_literal_len(&line) {
                let mut buf = vec![0u8; len];
                self.stream.read_exact(&mut buf).await?;
                let payload = String::from_utf8_lossy(&buf).into_owned();
                // ManageSieve emits a CRLF after the literal; consume it.
                let _ = self.read_line().await;
                data.push(payload);
            } else {
                data.push(line);
            }
        }
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
}
