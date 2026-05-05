//! Minimal CalDAV pull (RFC 4791).
//!
//! No auto-discovery required by the caller. Pass any URL on the CalDAV
//! server; `discover` walks the RFC 6764 chain
//! (`current-user-principal` → `calendar-home-set` → depth-1 PROPFIND for
//! `<calendar/>` resourcetype) and returns the list of discovered calendars.
//!
//! `sync` is etag-aware:
//! 1. Sends a `getetag` PROPFIND to get the current server state.
//! 2. Compares against `<store_dir>/_index.toml` (empty on first run).
//! 3. Issues a `calendar-multiget` REPORT only for new/changed hrefs.
//! 4. Deletes local files for hrefs the server no longer reports.
//!
//! On first run after upgrade the index is absent, so every server href is
//! treated as "new" — one full multiget, then incremental from there on.

use std::{collections::HashMap, path::Path, time::Duration};

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

// ── Index types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct Index {
    #[serde(default)]
    entries: Vec<IndexEntry>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct IndexEntry {
    href: String,
    uid: String,
    etag: String,
    filename: String,
}

fn load_index(store_dir: &Path) -> Result<Index> {
    let path = store_dir.join("_index.toml");
    if !path.exists() {
        return Ok(Index::default());
    }
    let raw = std::fs::read_to_string(&path)?;
    toml::from_str(&raw).map_err(|e| Error::Parse(e.to_string()))
}

fn save_index(store_dir: &Path, idx: &Index) -> Result<()> {
    let raw = toml::to_string_pretty(idx).map_err(|e| Error::Parse(e.to_string()))?;
    std::fs::write(store_dir.join("_index.toml"), raw)?;
    Ok(())
}

// ── XML request bodies ────────────────────────────────────────────────────────

const PROPFIND_ETAGS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop>
    <d:getetag/>
  </d:prop>
</d:propfind>"#;

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

/// Report from a `sync` call.
///
/// `events_stored` reflects only events written this run (new or changed).
/// Unchanged events already on disk are not counted. `events_seen` is the
/// total count of event hrefs the server reported via PROPFIND.
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

/// Etag-aware incremental sync.
///
/// 1. PROPFIND for current server etags.
/// 2. Diff against local `_index.toml`.
/// 3. `calendar-multiget` only for new/changed hrefs.
/// 4. Delete local files for hrefs the server no longer reports.
pub async fn sync(
    calendar_url: &str,
    user: &str,
    password: &str,
    store_dir: &Path,
) -> Result<SyncReport> {
    std::fs::create_dir_all(store_dir)?;

    // On first run after upgrade, the index is absent → every server href is
    // treated as "new", so one full multiget fires, then incremental from there.
    let mut index = load_index(store_dir)?;

    let http = reqwest::ClientBuilder::new()
        .timeout(Duration::from_secs(60))
        .build()?;

    // Step 1: get current server etags.
    let server_etags = list_etags(&http, calendar_url, user, password).await?;

    // Step 2: diff.
    let (changed, removed) = compute_diff(&server_etags, &index);

    // Step 3: delete local files for removed hrefs.
    for href in &removed {
        if let Some(entry) = index.entries.iter().find(|e| &e.href == href) {
            let path = store_dir.join(&entry.filename);
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::warn!("caldav: failed to remove {}: {e}", path.display());
            }
        }
    }
    // Drop removed entries from the index.
    index.entries.retain(|e| !removed.contains(&e.href));

    let events_seen = server_etags.len();

    // Fast-path: nothing to fetch.
    if changed.is_empty() {
        save_index(store_dir, &index)?;
        return Ok(SyncReport {
            events_seen,
            events_stored: 0,
        });
    }

    // Step 4: multiget for changed/new hrefs.
    let changed_refs: Vec<&str> = changed.iter().map(String::as_str).collect();
    let xml = multiget(&http, calendar_url, user, password, &changed_refs).await?;

    // Build a lookup of server etags for quick access.
    let etag_map: HashMap<String, String> = server_etags.into_iter().collect();

    let mut events_stored = 0usize;

    for resp in inbx_dav::split_responses(&xml) {
        let Some(href) = inbx_dav::extract_tag_text(&resp, "href") else {
            continue;
        };
        // Extract the calendar-data block from this individual response.
        let cals = extract_calendar_data_blocks(&resp);
        let Some(cal_data) = cals.into_iter().next() else {
            continue;
        };
        let uid = parse_uid_from_ical(&cal_data).unwrap_or_else(|| format!("inbx-{events_stored}"));
        let safe_uid = sanitize_uid(&uid);
        let filename = format!("{safe_uid}.ics");
        let path = store_dir.join(&filename);
        std::fs::write(&path, cal_data.as_bytes())?;

        let etag = etag_map.get(&href).cloned().unwrap_or_default();

        // Update or insert in the index.
        if let Some(entry) = index.entries.iter_mut().find(|e| e.href == href) {
            entry.uid = uid;
            entry.etag = etag;
            entry.filename = filename;
        } else {
            index.entries.push(IndexEntry {
                href,
                uid,
                etag,
                filename,
            });
        }

        events_stored += 1;
    }

    save_index(store_dir, &index)?;

    Ok(SyncReport {
        events_seen,
        events_stored,
    })
}

// ── Private helpers — network ─────────────────────────────────────────────────

/// Returns `Vec<(href, etag)>` for every `.ics` resource under the calendar.
async fn list_etags(
    http: &reqwest::Client,
    calendar_url: &str,
    user: &str,
    password: &str,
) -> Result<Vec<(String, String)>> {
    let body =
        inbx_dav::propfind_raw(http, calendar_url, user, password, PROPFIND_ETAGS, "1").await?;
    let mut out = Vec::new();
    for resp in inbx_dav::split_responses(&body) {
        let Some(href) = inbx_dav::extract_tag_text(&resp, "href") else {
            continue;
        };
        let Some(etag) = inbx_dav::extract_tag_text(&resp, "getetag") else {
            continue;
        };
        // Skip the collection itself — its href typically ends with '/'.
        if href.ends_with('/') {
            continue;
        }
        out.push((href, etag));
    }
    Ok(out)
}

/// RFC 4791 §7.9 calendar-multiget REPORT.
async fn multiget(
    http: &reqwest::Client,
    calendar_url: &str,
    user: &str,
    password: &str,
    hrefs: &[&str],
) -> Result<String> {
    let mut body = String::from(
        r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-multiget xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <d:getetag/>
    <c:calendar-data/>
  </d:prop>
"#,
    );
    for h in hrefs {
        let escaped = h
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        body.push_str(&format!("  <d:href>{escaped}</d:href>\n"));
    }
    body.push_str("</c:calendar-multiget>\n");

    let res = http
        .request(
            reqwest::Method::from_bytes(b"REPORT").unwrap(),
            calendar_url,
        )
        .basic_auth(user, Some(password))
        .header("Content-Type", "application/xml; charset=utf-8")
        .header("Depth", "1")
        .body(body)
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status().as_u16();
        let body = res.text().await.unwrap_or_default();
        return Err(Error::Server { status, body });
    }
    Ok(res.text().await?)
}

// ── Private helpers — diff ────────────────────────────────────────────────────

/// Pure diff: returns `(changed, removed)`.
///
/// `changed` — hrefs that are new or whose etag differs from the local index.
/// `removed` — hrefs in the local index no longer present on the server.
fn compute_diff(server: &[(String, String)], local: &Index) -> (Vec<String>, Vec<String>) {
    let server_map: HashMap<&str, &str> = server
        .iter()
        .map(|(h, e)| (h.as_str(), e.as_str()))
        .collect();
    let local_map: HashMap<&str, &str> = local
        .entries
        .iter()
        .map(|e| (e.href.as_str(), e.etag.as_str()))
        .collect();

    let changed: Vec<String> = server
        .iter()
        .filter(|(href, etag)| local_map.get(href.as_str()) != Some(&etag.as_str()))
        .map(|(href, _)| href.clone())
        .collect();

    let removed: Vec<String> = local
        .entries
        .iter()
        .filter(|e| !server_map.contains_key(e.href.as_str()))
        .map(|e| e.href.clone())
        .collect();

    (changed, removed)
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

    // ── compute_diff unit tests ───────────────────────────────────────────────

    fn make_index(entries: &[(&str, &str, &str, &str)]) -> Index {
        Index {
            entries: entries
                .iter()
                .map(|(href, uid, etag, filename)| IndexEntry {
                    href: href.to_string(),
                    uid: uid.to_string(),
                    etag: etag.to_string(),
                    filename: filename.to_string(),
                })
                .collect(),
        }
    }

    /// 1 changed (etag differs), 1 removed (absent from server), 1 unchanged.
    #[test]
    fn compute_diff_changed_removed_unchanged() {
        let server = vec![
            ("/cal/a.ics".to_string(), "\"etag-a-new\"".to_string()),
            ("/cal/c.ics".to_string(), "\"etag-c\"".to_string()),
        ];
        let local = make_index(&[
            ("/cal/a.ics", "a", "\"etag-a-old\"", "a.ics"),
            ("/cal/b.ics", "b", "\"etag-b\"", "b.ics"),
            ("/cal/c.ics", "c", "\"etag-c\"", "c.ics"),
        ]);
        let (changed, removed) = compute_diff(&server, &local);
        assert_eq!(changed, vec!["/cal/a.ics"]);
        assert_eq!(removed, vec!["/cal/b.ics"]);
    }

    /// Empty server with non-empty local → nothing changed, all removed.
    #[test]
    fn compute_diff_empty_server_non_empty_local() {
        let server: Vec<(String, String)> = vec![];
        let local = make_index(&[
            ("/cal/a.ics", "a", "\"etag-a\"", "a.ics"),
            ("/cal/b.ics", "b", "\"etag-b\"", "b.ics"),
        ]);
        let (changed, removed) = compute_diff(&server, &local);
        assert!(changed.is_empty());
        assert_eq!(removed.len(), 2);
        assert!(removed.contains(&"/cal/a.ics".to_string()));
        assert!(removed.contains(&"/cal/b.ics".to_string()));
    }
}
