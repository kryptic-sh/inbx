//! List-Unsubscribe (RFC 2369) and one-click unsubscribe (RFC 8058).

use std::time::Duration;

use mail_parser::MessageParser;

use crate::smtp;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("parse: could not parse RFC 5322")]
    Parse,
    #[error("no List-Unsubscribe header")]
    NotPresent,
    #[error("reqwest: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("smtp: {0}")]
    Smtp(#[from] smtp::Error),
    #[error("server returned {0}")]
    BadStatus(u16),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone)]
pub struct Targets {
    /// List-Unsubscribe https URL (RFC 8058 one-click capable).
    pub https: Option<String>,
    /// List-Unsubscribe mailto: URL.
    pub mailto: Option<String>,
    /// True when List-Unsubscribe-Post: List-Unsubscribe=One-Click is set.
    pub one_click: bool,
}

pub fn extract_targets(raw: &[u8]) -> Result<Targets> {
    let parsed = MessageParser::default().parse(raw).ok_or(Error::Parse)?;
    let header = parsed
        .header_values("List-Unsubscribe")
        .next()
        .ok_or(Error::NotPresent)?
        .as_text()
        .ok_or(Error::NotPresent)?;
    let one_click = parsed
        .header_values("List-Unsubscribe-Post")
        .next()
        .and_then(|v| v.as_text())
        .map(|s| {
            s.to_ascii_lowercase()
                .contains("list-unsubscribe=one-click")
        })
        .unwrap_or(false);

    let mut https = None;
    let mut mailto = None;
    for raw_url in header.split(',') {
        let url = raw_url.trim().trim_matches(|c| c == '<' || c == '>');
        let lower = url.to_ascii_lowercase();
        if lower.starts_with("https://") && https.is_none() {
            https = Some(url.to_string());
        } else if lower.starts_with("mailto:") && mailto.is_none() {
            mailto = Some(url.to_string());
        }
    }
    Ok(Targets {
        https,
        mailto,
        one_click,
    })
}

/// Perform RFC 8058 one-click unsubscribe via HTTPS POST.
pub async fn one_click(url: &str) -> Result<()> {
    let http = reqwest::ClientBuilder::new()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?;
    let res = http
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("List-Unsubscribe=One-Click")
        .send()
        .await?;
    if !res.status().is_success() {
        return Err(Error::BadStatus(res.status().as_u16()));
    }
    Ok(())
}

/// Send a one-line empty unsubscribe email to the mailto: target. The body is
/// per common-practice: "unsubscribe" subject and minimal body.
pub async fn via_mailto(account: &inbx_config::Account, mailto_url: &str) -> Result<()> {
    let stripped = mailto_url.strip_prefix("mailto:").unwrap_or(mailto_url);
    // Ignore any ?subject=... params; build our own minimal message.
    let to = stripped.split('?').next().unwrap_or(stripped);
    let raw = format!(
        "From: {from}\r\n\
         To: {to}\r\n\
         Subject: unsubscribe\r\n\
         Auto-Submitted: auto-generated\r\n\
         \r\n\
         unsubscribe\r\n",
        from = account.email,
        to = to,
    );
    smtp::send_message(account, raw.as_bytes()).await?;
    Ok(())
}
