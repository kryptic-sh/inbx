pub mod autoconfig;
pub mod theme;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Parsed representation of a SOCKS5 proxy URL.
#[derive(Debug, Clone)]
pub struct ParsedProxy {
    pub host: String,
    pub port: u16,
    /// `true` when the scheme is `socks5h` (DNS resolved by the proxy).
    pub remote_dns: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// SOCKS5 proxy URL: e.g. `"socks5://127.0.0.1:9050"` (Tor) or
    /// `"socks5h://user:pass@host:1080"`.
    pub url: String,
    /// Optional username for SOCKS5 auth.  When present the password must be
    /// in the OS keyring under service `"inbx-proxy"`, username =
    /// `account.name + ".proxy"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

impl ProxyConfig {
    /// Parse `self.url` into host, port and scheme flag.  Returns `Err` on
    /// garbage input.
    pub fn parse(&self) -> Result<ParsedProxy> {
        let url = url::Url::parse(&self.url)
            .map_err(|_| Error::ProxyUrl(format!("invalid proxy URL: {}", self.url)))?;
        let scheme = url.scheme();
        let remote_dns = match scheme {
            "socks5" => false,
            "socks5h" => true,
            other => {
                return Err(Error::ProxyUrl(format!(
                    "unsupported proxy scheme `{other}`; expected socks5 or socks5h"
                )));
            }
        };
        let host = url
            .host_str()
            .ok_or_else(|| Error::ProxyUrl("proxy URL missing host".into()))?
            .to_string();
        let port = url
            .port()
            .ok_or_else(|| Error::ProxyUrl("proxy URL missing port".into()))?;
        Ok(ParsedProxy {
            host,
            port,
            remote_dns,
        })
    }
}

pub use inbx_pgp::config::PgpConfig;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("xdg dirs unavailable: {0}")]
    NoXdg(#[from] hjkl_config::ConfigError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml parse: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("toml encode: {0}")]
    TomlEncode(#[from] toml::ser::Error),
    #[error("keyring: {0}")]
    Keyring(#[from] keyring::Error),
    #[error("proxy url: {0}")]
    ProxyUrl(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub accounts: Vec<Account>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    /// Implicit TLS — encrypted from byte 0. IMAP 993, SMTP 465.
    #[default]
    Tls,
    /// Opportunistic upgrade. IMAP 143, SMTP 587. Hard-fails if upgrade fails.
    Starttls,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthMethod {
    /// Plain LOGIN / AUTH PLAIN with a password from the keyring.
    #[default]
    AppPassword,
    /// XOAUTH2 with a refresh token from the keyring.
    #[serde(rename = "oauth2")]
    OAuth2 {
        provider: OAuthProvider,
        /// OAuth client ID. Required when no built-in default exists.
        #[serde(default)]
        client_id: Option<String>,
        /// OAuth client secret (treat as public for desktop apps + PKCE).
        #[serde(default)]
        client_secret: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OAuthProvider {
    Gmail,
    Microsoft {
        #[serde(default = "default_ms_tenant")]
        tenant: String,
    },
}

/// How the account talks to its server. The default IMAP path uses the
/// existing imap_host / smtp_host fields. Graph and JMAP override the
/// transport so the top-level `fetch` / `send` / `watch` commands hit
/// the right protocol without per-call subcommand prefixes.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Transport {
    /// IMAP for fetch + IDLE; SMTP for send.
    #[default]
    Imap,
    /// Microsoft Graph (`/me/messages`, `/me/sendMail`).
    Graph,
    /// JMAP via the supplied session document.
    Jmap { session_url: String },
}

fn default_ms_tenant() -> String {
    "common".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub name: String,
    pub email: String,
    pub imap_host: String,
    #[serde(default = "default_imap_port")]
    pub imap_port: u16,
    #[serde(default)]
    pub imap_security: TlsMode,
    pub smtp_host: String,
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    #[serde(default)]
    pub smtp_security: TlsMode,
    pub username: String,
    #[serde(default)]
    pub auth: AuthMethod,
    #[serde(default)]
    pub transport: Transport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pgp: Option<PgpConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ProxyConfig>,
}

fn default_imap_port() -> u16 {
    993
}

fn default_smtp_port() -> u16 {
    465
}

/// `$XDG_CONFIG_HOME/inbx/` (or `~/.config/inbx/` fallback).
pub fn config_dir() -> Result<PathBuf> {
    Ok(hjkl_config::config_dir("inbx")?)
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

/// `$XDG_DATA_HOME/inbx/` (or `~/.local/share/inbx/` fallback).
pub fn data_dir() -> Result<PathBuf> {
    Ok(hjkl_config::data_dir("inbx")?)
}

pub fn load() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(Config::default());
    }
    let raw = std::fs::read_to_string(&path)?;
    Ok(toml::from_str(&raw)?)
}

pub fn save(cfg: &Config) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let raw = toml::to_string_pretty(cfg)?;
    std::fs::write(&path, raw)?;
    Ok(())
}

const KEYRING_SERVICE: &str = "inbx";
const KEYRING_SERVICE_REFRESH: &str = "inbx-refresh";

pub fn store_password(account: &str, password: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, account)?;
    entry.set_password(password)?;
    Ok(())
}

pub fn load_password(account: &str) -> Result<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, account)?;
    Ok(entry.get_password()?)
}

pub fn delete_password(account: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, account)?;
    entry.delete_credential()?;
    Ok(())
}

pub fn store_refresh_token(account: &str, token: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE_REFRESH, account)?;
    entry.set_password(token)?;
    Ok(())
}

pub fn load_refresh_token(account: &str) -> Result<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE_REFRESH, account)?;
    Ok(entry.get_password()?)
}

pub fn delete_refresh_token(account: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE_REFRESH, account)?;
    entry.delete_credential()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trip() {
        let cfg = Config {
            accounts: vec![Account {
                name: "personal".into(),
                email: "me@example.com".into(),
                imap_host: "imap.example.com".into(),
                imap_port: 993,
                imap_security: TlsMode::Tls,
                smtp_host: "smtp.example.com".into(),
                smtp_port: 465,
                smtp_security: TlsMode::Tls,
                username: "me".into(),
                auth: AuthMethod::AppPassword,
                transport: Transport::Imap,
                pgp: None,
                proxy: None,
            }],
        };
        let raw = toml::to_string_pretty(&cfg).unwrap();
        let parsed: Config = toml::from_str(&raw).unwrap();
        assert_eq!(parsed.accounts.len(), 1);
        assert_eq!(parsed.accounts[0].name, "personal");
        assert_eq!(parsed.accounts[0].imap_security, TlsMode::Tls);
    }

    #[test]
    fn starttls_round_trip() {
        let raw = r#"
[[accounts]]
name = "corp"
email = "me@corp.com"
imap_host = "mail.corp.com"
imap_port = 143
imap_security = "starttls"
smtp_host = "mail.corp.com"
smtp_port = 587
smtp_security = "starttls"
username = "me"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.accounts[0].imap_security, TlsMode::Starttls);
        assert_eq!(cfg.accounts[0].smtp_security, TlsMode::Starttls);
    }

    #[test]
    fn defaults_to_tls() {
        let raw = r#"
[[accounts]]
name = "x"
email = "x@x.com"
imap_host = "imap.x.com"
smtp_host = "smtp.x.com"
username = "x"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.accounts[0].imap_security, TlsMode::Tls);
        assert_eq!(cfg.accounts[0].imap_port, 993);
        assert_eq!(cfg.accounts[0].smtp_port, 465);
    }

    /// Verify that accounts without a `[transport]` section parse as
    /// `Transport::Imap` (the default), so existing config files keep working.
    #[test]
    fn transport_defaults_to_imap() {
        let raw = r#"
[[accounts]]
name = "y"
email = "y@y.com"
imap_host = "imap.y.com"
smtp_host = "smtp.y.com"
username = "y"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(
            matches!(cfg.accounts[0].transport, Transport::Imap),
            "transport should default to Imap, got {:?}",
            cfg.accounts[0].transport
        );
    }

    /// Verify Graph transport round-trips through TOML.
    #[test]
    fn transport_graph_round_trip() {
        let raw = r#"
[[accounts]]
name = "outlook"
email = "me@outlook.com"
imap_host = "outlook.office365.com"
smtp_host = "smtp.office365.com"
username = "me@outlook.com"

[accounts.transport]
kind = "graph"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(
            matches!(cfg.accounts[0].transport, Transport::Graph),
            "expected Transport::Graph, got {:?}",
            cfg.accounts[0].transport
        );
        // Serialise → re-parse round-trip.
        let serialised = toml::to_string_pretty(&cfg).unwrap();
        let reparsed: Config = toml::from_str(&serialised).unwrap();
        assert!(
            matches!(reparsed.accounts[0].transport, Transport::Graph),
            "round-trip failed: {:?}",
            reparsed.accounts[0].transport
        );
    }

    /// Verify JMAP transport round-trips through TOML.
    #[test]
    fn transport_jmap_round_trip() {
        let raw = r#"
[[accounts]]
name = "fastmail"
email = "me@fastmail.com"
imap_host = "imap.fastmail.com"
smtp_host = "smtp.fastmail.com"
username = "me@fastmail.com"

[accounts.transport]
kind = "jmap"
session_url = "https://api.fastmail.com/jmap/session"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        match &cfg.accounts[0].transport {
            Transport::Jmap { session_url } => {
                assert_eq!(session_url, "https://api.fastmail.com/jmap/session");
            }
            other => panic!("expected Transport::Jmap, got {other:?}"),
        }
        // Also verify serialise → parse round-trip.
        let serialised = toml::to_string_pretty(&cfg).unwrap();
        let reparsed: Config = toml::from_str(&serialised).unwrap();
        assert!(matches!(
            reparsed.accounts[0].transport,
            Transport::Jmap { .. }
        ));
    }
}
