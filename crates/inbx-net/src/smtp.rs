use inbx_config::{Account, AuthMethod, TlsMode};
use lettre::address::Address as LettreAddress;
use lettre::address::Envelope;
use lettre::transport::smtp::AsyncSmtpTransport;
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{AsyncTransport, Tokio1Executor};
use mail_parser::MessageParser;

use crate::oauth;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("smtp: {0}")]
    Smtp(#[from] lettre::transport::smtp::Error),
    #[error("address: {0}")]
    Address(#[from] lettre::address::AddressError),
    #[error("lettre: {0}")]
    Lettre(#[from] lettre::error::Error),
    #[error("envelope: missing From or no recipients")]
    EmptyEnvelope,
    #[error("parse: could not parse RFC 5322 input")]
    Parse,
    #[error("invalid address: {0}")]
    InvalidAddress(String),
    #[error("config: {0}")]
    Config(#[from] inbx_config::Error),
    #[error("oauth: {0}")]
    OAuth(#[from] oauth::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Send a raw RFC 5322 message via the account's SMTP server.
/// Envelope is derived from From/To/Cc/Bcc headers in the message itself.
/// Honors `account.auth` (app password vs OAuth2 XOAUTH2).
pub async fn send_message(account: &Account, raw: &[u8]) -> Result<()> {
    let envelope = envelope_from_raw(raw)?;
    let builder = match account.smtp_security {
        TlsMode::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&account.smtp_host)?,
        TlsMode::Starttls => {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&account.smtp_host)?
        }
    };
    let mut builder = builder.port(account.smtp_port);
    builder = match &account.auth {
        AuthMethod::AppPassword => {
            let password = inbx_config::load_password(&account.name)?;
            builder.credentials(Credentials::new(account.username.clone(), password))
        }
        AuthMethod::OAuth2 { provider, .. } => {
            let refresh = inbx_config::load_refresh_token(&account.name)?;
            let access = oauth::refresh(&account.auth, provider, &refresh).await?;
            builder
                .credentials(Credentials::new(account.email.clone(), access))
                .authentication(vec![Mechanism::Xoauth2])
        }
    };
    let transport = builder.build();
    transport.send_raw(&envelope, raw).await?;
    Ok(())
}

fn envelope_from_raw(raw: &[u8]) -> Result<Envelope> {
    let parsed = MessageParser::default().parse(raw).ok_or(Error::Parse)?;

    let from = parsed
        .from()
        .and_then(|a| a.first())
        .and_then(|a| a.address())
        .ok_or(Error::EmptyEnvelope)?;
    let from: LettreAddress = from
        .parse()
        .map_err(|_| Error::InvalidAddress(from.to_string()))?;

    let mut recipients: Vec<LettreAddress> = Vec::new();
    for group in [parsed.to(), parsed.cc(), parsed.bcc()]
        .into_iter()
        .flatten()
    {
        for addr in group.iter() {
            if let Some(s) = addr.address() {
                let parsed: LettreAddress = s
                    .parse()
                    .map_err(|_| Error::InvalidAddress(s.to_string()))?;
                if !recipients.iter().any(|r| r == &parsed) {
                    recipients.push(parsed);
                }
            }
        }
    }
    if recipients.is_empty() {
        return Err(Error::EmptyEnvelope);
    }
    Ok(Envelope::new(Some(from), recipients)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_extraction() {
        let raw = b"From: alice@example.com\r\n\
                    To: bob@example.com, carol@example.com\r\n\
                    Cc: dave@example.com\r\n\
                    Subject: hi\r\n\
                    \r\n\
                    body\r\n";
        let env = envelope_from_raw(raw).unwrap();
        assert_eq!(env.from().unwrap().to_string(), "alice@example.com");
        assert_eq!(env.to().len(), 3);
    }

    #[test]
    fn missing_from() {
        let raw = b"To: bob@example.com\r\n\r\nbody\r\n";
        assert!(matches!(envelope_from_raw(raw), Err(Error::EmptyEnvelope)));
    }
}
