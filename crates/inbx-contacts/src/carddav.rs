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

const PROPFIND_PRINCIPAL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop>
    <d:current-user-principal/>
  </d:prop>
</d:propfind>"#;

const PROPFIND_HOME: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav">
  <d:prop>
    <c:addressbook-home-set/>
  </d:prop>
</d:propfind>"#;

const PROPFIND_BOOKS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav">
  <d:prop>
    <d:resourcetype/>
    <d:displayname/>
  </d:prop>
</d:propfind>"#;

#[derive(Debug, Clone)]
pub struct DiscoveredBook {
    pub url: String,
    pub display_name: Option<String>,
}

/// RFC 6764 simplified discovery chain. Pass any URL on the CardDAV server
/// (`/.well-known/carddav` redirect target, account base URL, principal,
/// or home set — the chain follows whichever step is needed).
pub async fn discover(
    server_base: &str,
    user: &str,
    password: &str,
) -> Result<Vec<DiscoveredBook>> {
    let http = reqwest::ClientBuilder::new()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?;

    // Step 1: principal-URL.
    let principal = match propfind_extract(
        &http,
        server_base,
        user,
        password,
        PROPFIND_PRINCIPAL,
        "0",
        "current-user-principal",
    )
    .await?
    {
        Some(href) => absolutize(server_base, &href),
        None => server_base.to_string(),
    };

    // Step 2: addressbook-home-set off the principal.
    let home = match propfind_extract(
        &http,
        &principal,
        user,
        password,
        PROPFIND_HOME,
        "0",
        "addressbook-home-set",
    )
    .await?
    {
        Some(href) => absolutize(&principal, &href),
        None => principal.clone(),
    };

    // Step 3: depth-1 PROPFIND of the home, find resources of type addressbook.
    let body = propfind_raw(&http, &home, user, password, PROPFIND_BOOKS, "1").await?;
    let mut out = Vec::new();
    for resp in split_responses(&body) {
        if !resp.contains("<addressbook") && !resp.contains(":addressbook") {
            continue;
        }
        let Some(href) = extract_tag_text(&resp, "href") else {
            continue;
        };
        // Skip the home-set placeholder itself (no resourcetype addressbook).
        let url = absolutize(&home, &href);
        if url == home {
            continue;
        }
        let name = extract_tag_text(&resp, "displayname");
        out.push(DiscoveredBook {
            url,
            display_name: name,
        });
    }
    Ok(out)
}

async fn propfind_raw(
    http: &reqwest::Client,
    url: &str,
    user: &str,
    password: &str,
    body: &str,
    depth: &str,
) -> Result<String> {
    let res = http
        .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), url)
        .basic_auth(user, Some(password))
        .header("Content-Type", "application/xml; charset=utf-8")
        .header("Depth", depth)
        .body(body.to_string())
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status().as_u16();
        let body = res.text().await.unwrap_or_default();
        return Err(Error::Server { status, body });
    }
    Ok(res.text().await?)
}

async fn propfind_extract(
    http: &reqwest::Client,
    url: &str,
    user: &str,
    password: &str,
    body: &str,
    depth: &str,
    parent_tag: &str,
) -> Result<Option<String>> {
    let xml = propfind_raw(http, url, user, password, body, depth).await?;
    Ok(find_href_under(&xml, parent_tag))
}

fn split_responses(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = 0;
    while let Some(start) = xml[cur..].find("<") {
        let abs_start = cur + start;
        let after = &xml[abs_start..];
        // Match `<response` or `<*:response`.
        let header_end = match after.find('>') {
            Some(e) => abs_start + e + 1,
            None => break,
        };
        let header = &xml[abs_start..header_end];
        if header.contains("response") && !header.contains("/>") && !header.contains("</") {
            let close_needle = match header
                .trim_start_matches('<')
                .split(|c: char| c.is_whitespace() || c == '>')
                .next()
            {
                Some(name) => format!("</{name}>"),
                None => break,
            };
            let Some(close_off) = xml[header_end..].find(&close_needle) else {
                break;
            };
            let abs_close = header_end + close_off + close_needle.len();
            out.push(decode_xml_entities(&xml[abs_start..abs_close]));
            cur = abs_close;
        } else {
            cur = header_end;
        }
    }
    out
}

fn find_href_under(xml: &str, parent_tag: &str) -> Option<String> {
    // Locate the parent tag (matching `<*:tag` or `<tag`), then the first
    // <href> inside it.
    let lower = xml.to_ascii_lowercase();
    let needle = parent_tag.to_ascii_lowercase();
    let start = lower.find(&needle)?;
    let end = lower[start..]
        .find(&format!("/{needle}"))
        .map(|e| start + e)
        .unwrap_or(xml.len());
    let slice = &xml[start..end];
    extract_tag_text(slice, "href")
}

fn extract_tag_text(xml: &str, tag: &str) -> Option<String> {
    // Match `<tag>text</tag>` or `<*:tag>text</*:tag>` case-insensitively.
    let lower = xml.to_ascii_lowercase();
    let mut cur = 0;
    let needle = format!("{tag}>");
    while let Some(off) = lower[cur..].find(&needle) {
        let open_end = cur + off + needle.len();
        // confirm preceding char is `<` or `:`
        let bytes = lower.as_bytes();
        if open_end < needle.len() + 1 {
            return None;
        }
        let preceding = bytes[cur + off - 1];
        if preceding != b'<' && preceding != b':' {
            cur = open_end;
            continue;
        }
        // Find closing tag.
        let close = lower[open_end..].find("</")?;
        let text = &xml[open_end..open_end + close];
        let text = decode_xml_entities(text.trim());
        return Some(text);
    }
    None
}

fn absolutize(base: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    // base looks like "https://host/path"; replace path with href if href starts with /.
    if let Some(scheme_end) = base.find("://") {
        let after = &base[scheme_end + 3..];
        let host_end = after
            .find('/')
            .map(|i| scheme_end + 3 + i)
            .unwrap_or(base.len());
        let host_part = &base[..host_end];
        if href.starts_with('/') {
            return format!("{host_part}{href}");
        }
        // relative href — append with `/`
        let trimmed = base.trim_end_matches('/');
        return format!("{trimmed}/{href}");
    }
    href.to_string()
}

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
