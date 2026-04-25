pub mod theme;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("xdg dirs unavailable")]
    NoXdg,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml parse: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("toml encode: {0}")]
    TomlEncode(#[from] toml::ser::Error),
    #[error("keyring: {0}")]
    Keyring(#[from] keyring::Error),
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
}

fn default_imap_port() -> u16 {
    993
}

fn default_smtp_port() -> u16 {
    465
}

pub fn project_dirs() -> Result<directories::ProjectDirs> {
    directories::ProjectDirs::from("sh", "kryptic", "inbx").ok_or(Error::NoXdg)
}

pub fn config_path() -> Result<PathBuf> {
    Ok(project_dirs()?.config_dir().join("config.toml"))
}

pub fn data_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.data_dir().to_path_buf())
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
}
