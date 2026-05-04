//! SOCKS5 proxy helpers for inbx-net.
//!
//! Call [`connect`] from IMAP / Sieve connection sites to transparently
//! route through a SOCKS5 proxy when one is configured, or fall back to a
//! direct TCP connection when `proxy` is `None`.
//!
//! For reqwest-based paths (Graph, JMAP, OAuth) call
//! [`build_reqwest_client`] instead — reqwest handles the proxy tunnel
//! internally.

use std::io;

use inbx_config::ProxyConfig;
use tokio::net::TcpStream;
use tokio_socks::tcp::Socks5Stream;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("socks5: {0}")]
    Socks(#[from] tokio_socks::Error),
    #[error("keyring: {0}")]
    Keyring(#[from] keyring::Error),
    #[error("proxy url: {0}")]
    Url(String),
}

pub type Result<T> = std::result::Result<T, Error>;

const KEYRING_PROXY_SERVICE: &str = "inbx-proxy";

/// Connect to `(target_host, target_port)` through the given SOCKS5 proxy.
/// If `proxy.username` is set the password is read from the OS keyring under
/// service `"inbx-proxy"`, key `"{account_name}.proxy"`.
///
/// `account_name` is used only for the keyring lookup; pass `""` when no
/// username is configured.
pub async fn connect_via_socks(
    proxy: &ProxyConfig,
    target_host: &str,
    target_port: u16,
    account_name: &str,
) -> Result<TcpStream> {
    let parsed = proxy.parse().map_err(|e| Error::Url(e.to_string()))?;
    let proxy_addr = (parsed.host.as_str(), parsed.port);
    let target = (target_host, target_port);

    let inner = if let Some(ref user) = proxy.username {
        let key = format!("{account_name}.proxy");
        let entry = keyring::Entry::new(KEYRING_PROXY_SERVICE, &key)?;
        let password = entry.get_password()?;
        Socks5Stream::connect_with_password(proxy_addr, target, user, &password).await?
    } else {
        Socks5Stream::connect(proxy_addr, target).await?
    };

    Ok(inner.into_inner())
}

/// Connect to `(host, port)`, routing through the proxy if one is supplied,
/// or directly otherwise.
///
/// `account_name` is forwarded to [`connect_via_socks`] for keyring lookup.
pub async fn connect(
    proxy: Option<&ProxyConfig>,
    host: &str,
    port: u16,
    account_name: &str,
) -> Result<TcpStream> {
    match proxy {
        Some(p) => connect_via_socks(p, host, port, account_name).await,
        None => Ok(TcpStream::connect((host, port)).await?),
    }
}

/// Build a `reqwest::Client` wired to the given proxy (when set).
///
/// All three reqwest-based backends (Graph, JMAP, OAuth) call this so the
/// proxy is applied uniformly.
pub fn build_reqwest_client(
    proxy: Option<&ProxyConfig>,
    timeout_secs: u64,
) -> reqwest::Result<reqwest::Client> {
    let mut builder =
        reqwest::Client::builder().timeout(std::time::Duration::from_secs(timeout_secs));
    if let Some(p) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(&p.url)?);
    }
    builder.build()
}
