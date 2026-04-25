//! Minimal CardDAV pull (RFC 6352).
//!
//! No auto-discovery. The caller passes the addressbook URL; we issue a
//! REPORT with an addressbook-query body, scrape the response for VCARD
//! blocks, and merge each `EMAIL` into the local contacts store, keyed
//! by the address with the `FN` as the display name.

use std::time::Duration;

use crate::ContactsStore;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("reqwest: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("server: {status}: {body}")]
    Server { status: u16, body: String },
    #[error("contacts: {0}")]
    Contacts(#[from] crate::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

const REPORT_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<c:addressbook-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav">
  <d:prop>
    <d:getetag/>
    <c:address-data/>
  </d:prop>
</c:addressbook-query>"#;

#[derive(Debug, Clone)]
pub struct SyncReport {
    pub vcards_seen: usize,
    pub addresses_imported: usize,
}

pub async fn sync(
    addressbook_url: &str,
    user: &str,
    password: &str,
    store: &ContactsStore,
) -> Result<SyncReport> {
    let http = reqwest::ClientBuilder::new()
        .timeout(Duration::from_secs(60))
        .build()?;
    let res = http
        .request(
            reqwest::Method::from_bytes(b"REPORT").unwrap(),
            addressbook_url,
        )
        .basic_auth(user, Some(password))
        .header("Content-Type", "application/xml; charset=utf-8")
        .header("Depth", "1")
        .body(REPORT_BODY)
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status().as_u16();
        let body = res.text().await.unwrap_or_default();
        return Err(Error::Server { status, body });
    }
    let body = res.text().await?;
    let mut report = SyncReport {
        vcards_seen: 0,
        addresses_imported: 0,
    };
    for vcard in extract_vcards(&body) {
        report.vcards_seen += 1;
        let (fn_, emails) = parse_vcard(&vcard);
        for email in emails {
            store.upsert(&email, fn_.as_deref()).await?;
            report.addresses_imported += 1;
        }
    }
    Ok(report)
}

fn extract_vcards(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = 0;
    while let Some(start) = xml[cur..].find("BEGIN:VCARD") {
        let abs_start = cur + start;
        let Some(end_off) = xml[abs_start..].find("END:VCARD") else {
            break;
        };
        let abs_end = abs_start + end_off + "END:VCARD".len();
        out.push(decode_xml_entities(&xml[abs_start..abs_end]));
        cur = abs_end;
    }
    out
}

fn parse_vcard(text: &str) -> (Option<String>, Vec<String>) {
    let mut fn_ = None;
    let mut emails = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        let upper = line.to_ascii_uppercase();
        if let Some(rest) = strip_prefix(line, &upper, "FN:") {
            fn_ = Some(rest.to_string());
        } else if upper.starts_with("EMAIL")
            && let Some(idx) = line.find(':')
        {
            let value = line[idx + 1..].trim();
            if !value.is_empty() {
                emails.push(value.to_string());
            }
        }
    }
    (fn_, emails)
}

fn strip_prefix<'a>(line: &'a str, upper: &str, prefix: &str) -> Option<&'a str> {
    if upper.starts_with(prefix) {
        Some(&line[prefix.len()..])
    } else {
        None
    }
}

fn decode_xml_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav">
  <d:response>
    <d:propstat><d:prop>
      <c:address-data>BEGIN:VCARD
VERSION:3.0
FN:Alice Example
EMAIL;TYPE=INTERNET:alice@example.com
EMAIL:alice.alt@example.com
END:VCARD</c:address-data>
    </d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:propstat><d:prop>
      <c:address-data>BEGIN:VCARD
VERSION:3.0
FN:Bob &amp; Co
EMAIL:bob@example.com
END:VCARD</c:address-data>
    </d:prop></d:propstat>
  </d:response>
</d:multistatus>"#;

    #[test]
    fn extract_two_vcards() {
        let cards = extract_vcards(SAMPLE);
        assert_eq!(cards.len(), 2);
    }

    #[test]
    fn parse_emails() {
        let cards = extract_vcards(SAMPLE);
        let (fn_, emails) = parse_vcard(&cards[0]);
        assert_eq!(fn_.as_deref(), Some("Alice Example"));
        assert_eq!(emails, vec!["alice@example.com", "alice.alt@example.com"]);
    }

    #[test]
    fn entities_decoded() {
        let cards = extract_vcards(SAMPLE);
        let (fn_, _) = parse_vcard(&cards[1]);
        assert_eq!(fn_.as_deref(), Some("Bob & Co"));
    }
}
