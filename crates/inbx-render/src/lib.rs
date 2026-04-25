pub mod auth;

use std::collections::{HashMap, HashSet};

use mail_parser::{MessageParser, MimeHeaders, PartType};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("parse: could not parse RFC 5322 input")]
    Parse,
    #[error("html2text: {0}")]
    Html2Text(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RemotePolicy {
    /// Default: rewrite all remote URLs in `<img>`/`<link>`/`<script>` etc.
    #[default]
    Block,
    /// Permit remote requests. Use only when sender is trusted.
    Allow,
}

#[derive(Debug, Clone)]
pub struct Rendered {
    /// Best-effort plaintext rendering for a TUI.
    pub plain: String,
    /// Sanitized HTML, if available, suitable for a sandboxed webview.
    pub html: Option<String>,
    /// Count of remote URLs blocked.
    pub blocked_remote: usize,
    /// Tracker URLs detected (1x1 imgs or known beacon hosts).
    pub trackers: Vec<String>,
    /// Inline cid: parts mapped to their content (for webview rewrite).
    pub inline_cids: HashMap<String, Vec<u8>>,
}

const TRACKER_HOSTS: &[&str] = &[
    "click.notifications.",
    "track.",
    "tracking.",
    "open.",
    "pixel.",
    "beacon.",
    "links.",
    "email.",
    "list-manage.com",
    "mailchimp.com",
    "sendgrid.net",
    "rs6.net",
    "mailgun.org",
    "sparkpostmail.com",
    "amazonses.com",
];

pub fn render_message(raw: &[u8], policy: RemotePolicy) -> Result<Rendered> {
    let parsed = MessageParser::default().parse(raw).ok_or(Error::Parse)?;

    let mut plain_parts: Vec<String> = Vec::new();
    let mut html_parts: Vec<String> = Vec::new();
    let mut inline_cids: HashMap<String, Vec<u8>> = HashMap::new();

    for part in parsed.parts.iter() {
        match &part.body {
            PartType::Text(t) => plain_parts.push(t.to_string()),
            PartType::Html(h) => html_parts.push(h.to_string()),
            PartType::Binary(b) | PartType::InlineBinary(b) => {
                if let Some(cid) = part.content_id() {
                    inline_cids.insert(cid.to_string(), b.to_vec());
                }
            }
            PartType::Message(_) | PartType::Multipart(_) => {}
        }
    }

    // If no text/plain, derive one from the HTML.
    let html_combined = html_parts.join("\n");

    let (sanitized_html, blocked_remote, trackers) = if html_combined.is_empty() {
        (None, 0, Vec::new())
    } else {
        let (out, blocked, trk) = sanitize_html(&html_combined, policy);
        (Some(out), blocked, trk)
    };

    let plain = if !plain_parts.is_empty() {
        plain_parts.join("\n\n")
    } else if let Some(html) = sanitized_html.as_deref() {
        html_to_text(html)
    } else {
        String::new()
    };

    Ok(Rendered {
        plain,
        html: sanitized_html,
        blocked_remote,
        trackers,
        inline_cids,
    })
}

fn sanitize_html(html: &str, policy: RemotePolicy) -> (String, usize, Vec<String>) {
    // First strip executables, event handlers, dangerous tags.
    let mut builder = ammonia::Builder::default();
    builder
        .add_generic_attributes(["style"])
        .url_relative(ammonia::UrlRelative::Deny);
    let cleaned = builder.clean(html).to_string();

    let mut blocked = 0usize;
    let mut trackers: HashSet<String> = HashSet::new();
    let out = match policy {
        RemotePolicy::Allow => cleaned,
        RemotePolicy::Block => block_remote_imgs(&cleaned, &mut blocked, &mut trackers),
    };
    let mut trackers: Vec<String> = trackers.into_iter().collect();
    trackers.sort();
    (out, blocked, trackers)
}

/// Crude scrubber: rewrite `src="http..."` inside `<img ...>` tags to a
/// `data:` placeholder and tally the count. Pure string ops — avoids
/// pulling in a full HTML parser.
fn block_remote_imgs(html: &str, blocked: &mut usize, trackers: &mut HashSet<String>) -> String {
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(rel) = lower[i..].find("<img") {
            let tag_start = i + rel;
            let tag_end = match lower[tag_start..].find('>') {
                Some(e) => tag_start + e + 1,
                None => {
                    out.push_str(&html[i..]);
                    break;
                }
            };
            let tag = &html[tag_start..tag_end];
            out.push_str(&html[i..tag_start]);

            if let Some(url) = extract_attr(tag, "src") {
                if is_remote(&url) {
                    *blocked += 1;
                    if is_tracker(&url) {
                        trackers.insert(url.clone());
                    }
                    let neutered = strip_attr(tag, "src");
                    out.push_str(&format!(
                        "{neutered_lhs} data-inbx-blocked=\"{u}\"{rhs}",
                        neutered_lhs = &neutered[..neutered.len() - 1],
                        u = ammonia::clean_text(&url),
                        rhs = ">"
                    ));
                } else {
                    out.push_str(tag);
                }
            } else {
                out.push_str(tag);
            }
            i = tag_end;
        } else {
            out.push_str(&html[i..]);
            break;
        }
    }
    out
}

fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!(" {name}=");
    let idx = lower.find(&needle)?;
    let after = &tag[idx + needle.len()..];
    let (q, rest) = after.split_at(1);
    if q == "\"" {
        rest.find('"').map(|e| rest[..e].to_string())
    } else if q == "'" {
        rest.find('\'').map(|e| rest[..e].to_string())
    } else {
        let end = after.find(|c: char| c.is_whitespace() || c == '>')?;
        Some(after[..end].to_string())
    }
}

fn strip_attr(tag: &str, name: &str) -> String {
    let lower = tag.to_ascii_lowercase();
    let needle = format!(" {name}=");
    let Some(idx) = lower.find(&needle) else {
        return tag.to_string();
    };
    let after_eq = idx + needle.len();
    let bytes = tag.as_bytes();
    let q = bytes.get(after_eq).copied();
    let val_end = match q {
        Some(b'"') => tag[after_eq + 1..]
            .find('"')
            .map(|e| after_eq + 1 + e + 1)
            .unwrap_or(tag.len()),
        Some(b'\'') => tag[after_eq + 1..]
            .find('\'')
            .map(|e| after_eq + 1 + e + 1)
            .unwrap_or(tag.len()),
        _ => tag[after_eq..]
            .find(|c: char| c.is_whitespace() || c == '>')
            .map(|e| after_eq + e)
            .unwrap_or(tag.len()),
    };
    let mut out = String::with_capacity(tag.len());
    out.push_str(&tag[..idx]);
    out.push_str(&tag[val_end..]);
    out
}

fn is_remote(url: &str) -> bool {
    let l = url.to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://") || l.starts_with("//")
}

fn is_tracker(url: &str) -> bool {
    let l = url.to_ascii_lowercase();
    TRACKER_HOSTS.iter().any(|h| l.contains(h))
        || l.contains("/open?")
        || l.contains("/pixel")
        || l.contains("track=")
}

fn html_to_text(html: &str) -> String {
    html2text::from_read(html.as_bytes(), 100).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_passthrough() {
        let raw = b"From: a@x\r\nTo: b@y\r\nSubject: hi\r\n\r\nhello world\r\n";
        let r = render_message(raw, RemotePolicy::Block).unwrap();
        assert_eq!(r.plain.trim(), "hello world");
        assert!(r.html.is_none());
        assert_eq!(r.blocked_remote, 0);
    }

    #[test]
    fn html_remote_blocked() {
        let raw = b"From: a@x\r\nTo: b@y\r\nSubject: hi\r\n\
                    Content-Type: text/html; charset=utf-8\r\n\r\n\
                    <p>Hello</p><img src=\"https://tracker.example.com/p.gif\" width=1 height=1>\r\n";
        let r = render_message(raw, RemotePolicy::Block).unwrap();
        let html = r.html.expect("html present");
        assert!(!html.contains("https://tracker.example.com"));
        assert!(html.contains("data-inbx-blocked"));
        assert_eq!(r.blocked_remote, 1);
    }

    #[test]
    fn script_stripped() {
        let raw = b"From: a@x\r\nTo: b@y\r\nSubject: hi\r\n\
                    Content-Type: text/html; charset=utf-8\r\n\r\n\
                    <p>Hi</p><script>alert(1)</script>\r\n";
        let r = render_message(raw, RemotePolicy::Allow).unwrap();
        let html = r.html.expect("html present");
        assert!(!html.contains("<script"));
    }

    #[test]
    fn html_to_text_fallback() {
        let raw = b"From: a@x\r\nTo: b@y\r\nSubject: hi\r\n\
                    Content-Type: text/html; charset=utf-8\r\n\r\n\
                    <p>Hello <b>world</b></p>\r\n";
        let r = render_message(raw, RemotePolicy::Block).unwrap();
        assert!(r.plain.to_lowercase().contains("hello"));
        assert!(r.plain.to_lowercase().contains("world"));
    }

    #[test]
    fn tracker_detected() {
        let raw = b"Content-Type: text/html\r\n\r\n\
                    <img src=\"https://list-manage.com/track/open?u=1\">\r\n";
        let r = render_message(raw, RemotePolicy::Block).unwrap();
        assert_eq!(r.blocked_remote, 1);
        assert!(!r.trackers.is_empty());
    }
}
