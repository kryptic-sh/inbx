//! OAuth2 + XOAUTH2 SASL helpers.
//!
//! Token storage strategy: persist only the refresh token in the OS keyring
//! and derive a fresh access token on every connection. Access tokens are
//! short-lived and never written to disk.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use inbx_config::{AuthMethod, OAuthProvider};
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, RefreshToken, Scope, TokenResponse, TokenUrl,
};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("url: {0}")]
    Url(#[from] url::ParseError),
    #[error("reqwest: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("oauth: missing client_id for provider")]
    MissingClient,
    #[error("oauth: not an OAuth2 account")]
    NotOAuth,
    #[error("oauth: csrf state mismatch")]
    CsrfMismatch,
    #[error("oauth: callback missing code")]
    NoCode,
    #[error("oauth: token endpoint returned no refresh token")]
    NoRefresh,
    #[error("oauth: token endpoint returned no access token")]
    NoAccess,
    #[error("oauth: configuration error: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Built-in OAuth client credentials. Desktop public clients ship their IDs in
/// open source; PKCE makes the secret non-secret. Override per account in
/// config when a tenant needs its own registration.
struct ClientDefaults {
    auth_url: &'static str,
    token_url: &'static str,
    scope: &'static str,
    /// Optional default client_id when none is provided in config.
    default_client_id: Option<&'static str>,
    default_client_secret: Option<&'static str>,
}

fn defaults_for(provider: &OAuthProvider) -> ClientDefaults {
    match provider {
        OAuthProvider::Gmail => ClientDefaults {
            auth_url: "https://accounts.google.com/o/oauth2/v2/auth",
            token_url: "https://oauth2.googleapis.com/token",
            scope: "https://mail.google.com/",
            default_client_id: None,
            default_client_secret: None,
        },
        OAuthProvider::Microsoft { .. } => ClientDefaults {
            auth_url: "", // filled in dynamically per-tenant below
            token_url: "",
            scope: "https://outlook.office.com/IMAP.AccessAsUser.All \
                    https://outlook.office.com/SMTP.Send \
                    offline_access",
            default_client_id: None,
            default_client_secret: None,
        },
    }
}

fn endpoints(provider: &OAuthProvider) -> (String, String, String) {
    let d = defaults_for(provider);
    match provider {
        OAuthProvider::Gmail => (d.auth_url.into(), d.token_url.into(), d.scope.into()),
        OAuthProvider::Microsoft { tenant } => (
            format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/authorize"),
            format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token"),
            d.scope.into(),
        ),
    }
}

fn pick_client_id(method: &AuthMethod, provider: &OAuthProvider) -> Result<ClientId> {
    let configured = match method {
        AuthMethod::OAuth2 { client_id, .. } => client_id.clone(),
        _ => return Err(Error::NotOAuth),
    };
    let id = configured
        .or_else(|| defaults_for(provider).default_client_id.map(String::from))
        .ok_or(Error::MissingClient)?;
    Ok(ClientId::new(id))
}

fn pick_client_secret(method: &AuthMethod, provider: &OAuthProvider) -> Option<ClientSecret> {
    let configured = match method {
        AuthMethod::OAuth2 { client_secret, .. } => client_secret.clone(),
        _ => None,
    };
    configured
        .or_else(|| {
            defaults_for(provider)
                .default_client_secret
                .map(String::from)
        })
        .map(ClientSecret::new)
}

fn http_client() -> Result<reqwest::Client> {
    let c = reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    Ok(c)
}

/// Run an interactive auth-code flow with a loopback redirect on a random
/// port. Returns (refresh_token, access_token, expires_in_secs).
pub async fn login(method: &AuthMethod, provider: &OAuthProvider) -> Result<TokenSet> {
    let client_id = pick_client_id(method, provider)?;
    let client_secret = pick_client_secret(method, provider);
    let (auth_url_s, token_url_s, scope_s) = endpoints(provider);
    let auth_url = AuthUrl::new(auth_url_s).map_err(|e| Error::Config(e.to_string()))?;
    let token_url = TokenUrl::new(token_url_s).map_err(|e| Error::Config(e.to_string()))?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let redirect = RedirectUrl::new(format!("http://127.0.0.1:{port}/callback"))
        .map_err(|e| Error::Config(e.to_string()))?;

    let mut client = BasicClient::new(client_id)
        .set_auth_uri(auth_url)
        .set_token_uri(token_url)
        .set_redirect_uri(redirect);
    if let Some(secret) = client_secret {
        client = client.set_client_secret(secret);
    }

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let mut auth_req = client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new(scope_s));
    // Gmail/MS need access_type=offline + prompt=consent to return refresh_token.
    if matches!(provider, OAuthProvider::Gmail) {
        auth_req = auth_req
            .add_extra_param("access_type", "offline")
            .add_extra_param("prompt", "consent");
    }
    let (authorize_url, csrf_state) = auth_req.set_pkce_challenge(pkce_challenge).url();

    println!("\nOpen this URL in your browser to authenticate:\n");
    println!("  {authorize_url}\n");
    println!("Waiting for redirect to http://127.0.0.1:{port}/callback ...");

    let (code, returned_state) = wait_for_callback(listener).await?;
    if returned_state.secret() != csrf_state.secret() {
        return Err(Error::CsrfMismatch);
    }

    let http = http_client()?;
    let token = client
        .exchange_code(code)
        .set_pkce_verifier(PkceCodeVerifier::new(pkce_verifier.secret().to_string()))
        .request_async(&http)
        .await
        .map_err(|e| Error::Config(e.to_string()))?;

    let refresh = token
        .refresh_token()
        .ok_or(Error::NoRefresh)?
        .secret()
        .clone();
    let access = token.access_token().secret().clone();
    let expires = token.expires_in().map(|d| d.as_secs()).unwrap_or(3600);

    Ok(TokenSet {
        refresh,
        access,
        expires_in: expires,
    })
}

/// Trade a stored refresh token for a fresh access token.
pub async fn refresh(
    method: &AuthMethod,
    provider: &OAuthProvider,
    refresh_token: &str,
) -> Result<String> {
    let client_id = pick_client_id(method, provider)?;
    let client_secret = pick_client_secret(method, provider);
    let (_, token_url_s, _) = endpoints(provider);
    let auth_url = AuthUrl::new("https://example.invalid/".into())
        .map_err(|e| Error::Config(e.to_string()))?; // unused; required by builder
    let token_url = TokenUrl::new(token_url_s).map_err(|e| Error::Config(e.to_string()))?;

    let mut client = BasicClient::new(client_id)
        .set_auth_uri(auth_url)
        .set_token_uri(token_url);
    if let Some(secret) = client_secret {
        client = client.set_client_secret(secret);
    }

    let http = http_client()?;
    let token = client
        .exchange_refresh_token(&RefreshToken::new(refresh_token.to_string()))
        .request_async(&http)
        .await
        .map_err(|e| Error::Config(e.to_string()))?;
    Ok(token.access_token().secret().clone())
}

#[derive(Debug, Clone)]
pub struct TokenSet {
    pub refresh: String,
    pub access: String,
    pub expires_in: u64,
}

async fn wait_for_callback(listener: TcpListener) -> Result<(AuthorizationCode, CsrfToken)> {
    let (mut stream, _) = listener.accept().await?;
    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let path = line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| Error::Config("no request line".into()))?;
    let url = Url::parse(&format!("http://127.0.0.1{path}"))?;
    let mut code: Option<AuthorizationCode> = None;
    let mut state: Option<CsrfToken> = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(AuthorizationCode::new(v.into_owned())),
            "state" => state = Some(CsrfToken::new(v.into_owned())),
            _ => {}
        }
    }
    let body = "<!doctype html><meta charset=utf-8><title>inbx</title>\
                <body style=\"font-family:system-ui;padding:2rem\">\
                <h1>Authenticated</h1><p>You can close this window and return to your terminal.</p>\
                </body>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
    Ok((code.ok_or(Error::NoCode)?, state.ok_or(Error::NoCode)?))
}

/// Encode an XOAUTH2 SASL initial-response.
///
/// Format: `user=<email>\x01auth=Bearer <token>\x01\x01` then base64.
pub fn xoauth2_sasl(user: &str, access_token: &str) -> String {
    let raw = format!("user={user}\x01auth=Bearer {access_token}\x01\x01");
    B64.encode(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xoauth2_format() {
        let s = xoauth2_sasl("me@example.com", "ya29.tok");
        let decoded = B64.decode(&s).unwrap();
        let text = std::str::from_utf8(&decoded).unwrap();
        assert!(text.contains("user=me@example.com"));
        assert!(text.contains("auth=Bearer ya29.tok"));
        assert!(text.ends_with("\x01\x01"));
    }
}
