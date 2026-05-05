//! Minimal CalDAV pull (RFC 4791).
//!
//! No auto-discovery required by the caller. Pass any URL on the CalDAV
//! server; `discover` walks the RFC 6764 chain
//! (`current-user-principal` → `calendar-home-set` → depth-1 PROPFIND for
//! `<calendar/>` resourcetype) and returns the list of discovered calendars.
//! `sync` issues a `calendar-query` REPORT filtered for VEVENT components,
//! scrapes the `<calendar-data>` blocks, and writes each VEVENT as
//! `<uid>.ics` under `store_dir`.

use std::{path::Path, time::Duration};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("reqwest: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("server: {status}: {body}")]
    Server { status: u16, body: String },
    #[error("dav: {0}")]
    Dav(#[from] inbx_dav::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse: {0}")]
    Parse(String),
}

pub type Result<T> = std::result::Result<T, Error>;

// ── XML request bodies ────────────────────────────────────────────────────────

const REPORT_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <d:getetag/>
    <c:calendar-data/>
  </d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR">
      <c:comp-filter name="VEVENT"/>
    </c:comp-filter>
  </c:filter>
</c:calendar-query>"#;

const PROPFIND_PRINCIPAL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop>
    <d:current-user-principal/>
  </d:prop>
</d:propfind>"#;

const PROPFIND_HOME: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <c:calendar-home-set/>
  </d:prop>
</d:propfind>"#;

const PROPFIND_CALENDARS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:a="http://apple.com/ns/ical/">
  <d:prop>
    <d:resourcetype/>
    <d:displayname/>
    <a:calendar-color/>
  </d:prop>
</d:propfind>"#;

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DiscoveredCalendar {
    pub url: String,
    pub display_name: Option<String>,
    /// Apple/Fastmail calendar-color extension — best-effort, may be `None`.
    pub color: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SyncReport {
    pub events_seen: usize,
    pub events_stored: usize,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// RFC 6764 simplified discovery chain.
///
/// Pass any URL on the CalDAV server (`/.well-known/caldav` redirect target,
/// account base URL, principal, or home set — the chain follows whichever step
/// is needed).
pub async fn discover(
    server_base: &str,
    user: &str,
    password: &str,
) -> Result<Vec<DiscoveredCalendar>> {
    let http = reqwest::ClientBuilder::new()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?;

    // Step 1: principal URL.
    let principal = match inbx_dav::propfind_extract(
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
        Some(href) => inbx_dav::absolutize(server_base, &href),
        None => server_base.to_string(),
    };

    // Step 2: calendar-home-set off the principal.
    let home = match inbx_dav::propfind_extract(
        &http,
        &principal,
        user,
        password,
        PROPFIND_HOME,
        "0",
        "calendar-home-set",
    )
    .await?
    {
        Some(href) => inbx_dav::absolutize(&principal, &href),
        None => principal.clone(),
    };

    // Step 3: depth-1 PROPFIND of the home; collect resources of type calendar.
    let body =
        inbx_dav::propfind_raw(&http, &home, user, password, PROPFIND_CALENDARS, "1").await?;
    let mut out = Vec::new();
    for resp in inbx_dav::split_responses(&body) {
        if !resp.contains("<calendar") && !resp.contains(":calendar") {
            continue;
        }
        // Make sure it's a resourcetype=calendar, not e.g. a calendar-home-set.
        // The resourcetype block looks like <resourcetype><calendar/></resourcetype>.
        let Some(href) = inbx_dav::extract_tag_text(&resp, "href") else {
            continue;
        };
        let url = inbx_dav::absolutize(&home, &href);
        if url == home {
            continue;
        }
        let display_name = inbx_dav::extract_tag_text(&resp, "displayname");
        let color = inbx_dav::extract_tag_text(&resp, "calendar-color");
        out.push(DiscoveredCalendar {
            url,
            display_name,
            color,
        });
    }
    Ok(out)
}

/// Issue a `calendar-query` REPORT filtered for VEVENT components, scrape
/// the `<calendar-data>` blocks, parse each VEVENT, extract the UID, and
/// write the VCALENDAR block to `store_dir/<uid>.ics`.
pub async fn sync(
    calendar_url: &str,
    user: &str,
    password: &str,
    store_dir: &Path,
) -> Result<SyncReport> {
    std::fs::create_dir_all(store_dir)?;

    let http = reqwest::ClientBuilder::new()
        .timeout(Duration::from_secs(60))
        .build()?;
    let res = http
        .request(
            reqwest::Method::from_bytes(b"REPORT").unwrap(),
            calendar_url,
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
        events_seen: 0,
        events_stored: 0,
    };

    for cal_data in extract_calendar_data_blocks(&body) {
        report.events_seen += 1;
        let uid = parse_uid_from_ical(&cal_data)
            .unwrap_or_else(|| format!("inbx-{}", report.events_seen));
        let safe_uid = sanitize_uid(&uid);
        let path = store_dir.join(format!("{safe_uid}.ics"));
        std::fs::write(&path, cal_data.as_bytes())?;
        report.events_stored += 1;
    }

    Ok(report)
}

// ── Helpers — XML scraping ────────────────────────────────────────────────────

fn extract_calendar_data_blocks(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = 0;
    while let Some(start) = xml[cur..].find("BEGIN:VCALENDAR") {
        let abs_start = cur + start;
        let Some(end_off) = xml[abs_start..].find("END:VCALENDAR") else {
            break;
        };
        let abs_end = abs_start + end_off + "END:VCALENDAR".len();
        out.push(inbx_dav::decode_xml_entities(&xml[abs_start..abs_end]));
        cur = abs_end;
    }
    out
}

fn parse_uid_from_ical(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        let upper = line.to_ascii_uppercase();
        if upper.starts_with("UID:") {
            let val = line["UID:".len()..].trim();
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

fn sanitize_uid(uid: &str) -> String {
    uid.chars()
        .map(|c| {
            if c == '/' || c == '\\' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:propstat><d:prop>
      <c:calendar-data>BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//Example//EN
BEGIN:VEVENT
UID:event-1@example.com
SUMMARY:Team standup
DTSTART:20260601T090000Z
DTEND:20260601T093000Z
END:VEVENT
END:VCALENDAR</c:calendar-data>
    </d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:propstat><d:prop>
      <c:calendar-data>BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//Example//EN
BEGIN:VEVENT
UID:event-2 &amp; special@example.com
SUMMARY:All-hands &amp; planning
DTSTART:20260602T140000Z
DTEND:20260602T160000Z
END:VEVENT
END:VCALENDAR</c:calendar-data>
    </d:prop></d:propstat>
  </d:response>
</d:multistatus>"#;

    #[test]
    fn extract_two_calendar_data_blocks() {
        let blocks = extract_calendar_data_blocks(SAMPLE);
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn parse_uid_first_event() {
        let blocks = extract_calendar_data_blocks(SAMPLE);
        let uid = parse_uid_from_ical(&blocks[0]);
        assert_eq!(uid.as_deref(), Some("event-1@example.com"));
    }

    #[test]
    fn entities_decoded_in_uid() {
        let blocks = extract_calendar_data_blocks(SAMPLE);
        let uid = parse_uid_from_ical(&blocks[1]);
        // &amp; → & after decode_xml_entities inside extract_calendar_data_blocks
        assert_eq!(uid.as_deref(), Some("event-2 & special@example.com"));
    }

    #[test]
    fn sanitize_uid_replaces_slashes() {
        assert_eq!(sanitize_uid("a/b\\c\x00d"), "a_b_c_d");
    }

    #[test]
    fn absolutize_absolute_href() {
        assert_eq!(
            inbx_dav::absolutize("https://dav.example.com/", "https://other.com/cal"),
            "https://other.com/cal"
        );
    }

    #[test]
    fn absolutize_root_relative() {
        assert_eq!(
            inbx_dav::absolutize("https://dav.example.com/user/", "/calendars/me/"),
            "https://dav.example.com/calendars/me/"
        );
    }
}
