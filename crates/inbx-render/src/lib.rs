pub mod auth;
pub mod pgp;
pub mod phishing;

use std::collections::{HashMap, HashSet};

use mail_parser::{MessageParser, MimeHeaders, PartType};

pub use inbx_pgp::mime::AutocryptHeader;
pub use pgp::{SecureKind, SecurityInfo};

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

/// Result of a PGP verify or decrypt operation on the rendered message.
#[derive(Debug, Clone, Default)]
pub struct PgpVerifyResult {
    pub verified: bool,
    pub signer_fingerprint: Option<String>,
    pub signer_uid: Option<String>,
    pub created_unix: Option<i64>,
    /// Only `Some` for encrypted messages that were successfully decrypted.
    pub decrypted_body: Option<Vec<u8>>,
    /// Non-fatal error message if the PGP operation failed.
    pub error: Option<String>,
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
    /// Phishing heuristic warnings for this message.
    pub phishing: Vec<phishing::PhishingWarning>,
    /// PGP/S-MIME security kind detected in the raw message.
    pub security: SecurityInfo,
    /// Populated when the caller passes a `KeySource` and PGP content is present.
    pub pgp_verify: Option<PgpVerifyResult>,
    /// Parsed Autocrypt header from the incoming message, if present.
    /// Callers should pass this to `ContactsStore::store_autocrypt` for harvest.
    pub autocrypt: Option<AutocryptHeader>,
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
    render_message_inner(raw, policy)
}

/// Async entry-point that accepts an optional [`inbx_pgp::KeySource`] and an
/// optional [`inbx_pgp::PubkeyLookup`] for sender-key resolution.
///
/// * When `pgp` is `None` or the message contains no PGP content, behaves
///   identically to [`render_message`] with `pgp_verify: None`.
/// * `PgpMime` → extracts the signed inner part and `application/pgp-signature`
///   body. If `lookup` returns a key for the From address, verifies against that.
///   Otherwise falls back to the user's own key (slice-2 behaviour) and tags
///   `pgp_verify.error` with a fallback notice.
/// * `PgpEncrypted` → decrypts, re-renders the decrypted bytes through
///   `render_message_inner`, merges into the returned `Rendered`.
/// * Any `Autocrypt:` header is parsed and surfaced in `Rendered.autocrypt`.
///   The caller is responsible for calling `ContactsStore::store_autocrypt`.
///
/// Errors do NOT fail the render — they populate `pgp_verify.error`.
pub async fn render_message_with_pgp(
    raw: &[u8],
    policy: RemotePolicy,
    pgp: Option<&dyn inbx_pgp::KeySource>,
    lookup: Option<&dyn inbx_pgp::PubkeyLookup>,
) -> Result<Rendered> {
    let mut rendered = render_message_inner(raw, policy)?;
    rendered.security = pgp::detect(raw);

    // Harvest Autocrypt header regardless of PGP verification path.
    rendered.autocrypt = extract_autocrypt_header(raw);

    let Some(source) = pgp else {
        return Ok(rendered);
    };

    // Extract the From address for sender-key lookup.
    let from_email = extract_from_email(raw);

    match rendered.security.kind {
        SecureKind::PgpMime => {
            rendered.pgp_verify =
                Some(try_verify_pgp_mime(raw, source, lookup, from_email.as_deref()).await);
        }
        SecureKind::PgpEncrypted => {
            let (verify_result, decrypted_rendered) =
                try_decrypt_pgp_mime(raw, policy, source).await;
            if let Some(mut dr) = decrypted_rendered {
                // Merge: replace plain/html with decrypted content, keep outer security badge.
                rendered.plain = dr.plain;
                rendered.html = dr.html;
                rendered.blocked_remote += dr.blocked_remote;
                rendered.trackers.append(&mut dr.trackers);
            }
            rendered.pgp_verify = Some(verify_result);
        }
        _ => {}
    }

    Ok(rendered)
}

/// Extract and parse the first `Autocrypt:` header from a raw message.
fn extract_autocrypt_header(raw: &[u8]) -> Option<AutocryptHeader> {
    let s = std::str::from_utf8(raw).ok()?;
    // Find "Autocrypt:" header (case-insensitive, at start of line).
    let lower = s.to_ascii_lowercase();
    let needle = "autocrypt:";
    let pos = lower.find(needle)?;
    let after = &s[pos + needle.len()..];
    // Collect the full (possibly folded) header value up to a non-folded newline.
    let mut value = String::new();
    let mut chars = after.chars().peekable();
    loop {
        let line: String = chars.by_ref().take_while(|&c| c != '\n').collect();
        // Strip trailing CR if any.
        let line = line.trim_end_matches('\r');
        value.push_str(line);
        // If next char is whitespace it's a fold continuation.
        if chars
            .peek()
            .map(|c| *c == ' ' || *c == '\t')
            .unwrap_or(false)
        {
            value.push('\n');
        } else {
            break;
        }
    }
    inbx_pgp::mime::parse_autocrypt_header(value.trim()).ok()
}

/// Extract the first From: address from a raw message.
fn extract_from_email(raw: &[u8]) -> Option<String> {
    let parsed = MessageParser::default().parse(raw)?;
    parsed
        .from()
        .and_then(|g| g.first())
        .and_then(|a| a.address())
        .map(|s| s.to_string())
}

async fn try_verify_pgp_mime(
    raw: &[u8],
    source: &dyn inbx_pgp::KeySource,
    lookup: Option<&dyn inbx_pgp::PubkeyLookup>,
    from_email: Option<&str>,
) -> PgpVerifyResult {
    // Extract signed inner bytes and detached signature from a multipart/signed message.
    let (signed_bytes, sig_bytes) = match extract_pgp_mime_parts(raw) {
        Some(p) => p,
        None => {
            return PgpVerifyResult {
                error: Some("could not extract PGP/MIME signed parts".into()),
                ..Default::default()
            };
        }
    };

    // Try to get sender's pubkey from contacts lookup first.
    let (pub_key, fallback_error) = if let (Some(lk), Some(email)) = (lookup, from_email) {
        match lk.lookup(email).await {
            Ok(Some(key)) => (key, None),
            Ok(None) => {
                // Fall back to own key — tag error as informational.
                let fallback_key = match own_key(source).await {
                    Ok(k) => k,
                    Err(e) => return e,
                };
                (
                    fallback_key,
                    Some(format!(
                        "no stored pubkey for sender ({email}) — verifying against own key (fallback)"
                    )),
                )
            }
            Err(e) => {
                let fallback_key = match own_key(source).await {
                    Ok(k) => k,
                    Err(fe) => return fe,
                };
                (
                    fallback_key,
                    Some(format!(
                        "pubkey lookup error: {e} — verifying against own key (fallback)"
                    )),
                )
            }
        }
    } else {
        // No lookup provided — slice-2 / no-contacts-store fallback. No error tag.
        let fallback_key = match own_key(source).await {
            Ok(k) => k,
            Err(e) => return e,
        };
        (fallback_key, None)
    };

    let sig = inbx_pgp::Signature(sig_bytes);
    match source.verify_detached(&pub_key, &signed_bytes, &sig).await {
        Ok(vr) => PgpVerifyResult {
            verified: vr.valid,
            signer_fingerprint: vr.signer_fingerprint,
            signer_uid: vr.signer_uid,
            created_unix: vr.created_unix,
            decrypted_body: None,
            error: fallback_error,
        },
        Err(e) => PgpVerifyResult {
            error: Some(format!("verify_detached: {e}")),
            ..Default::default()
        },
    }
}

/// Export the first available key from `source`, returning `Ok(ArmoredKey)` or
/// a ready-made `PgpVerifyResult` error.
async fn own_key(
    source: &dyn inbx_pgp::KeySource,
) -> std::result::Result<inbx_pgp::ArmoredKey, PgpVerifyResult> {
    let keys = match source.list_keys().await {
        Ok(k) => k,
        Err(e) => {
            return Err(PgpVerifyResult {
                error: Some(format!("list_keys: {e}")),
                ..Default::default()
            });
        }
    };
    let Some((first_key_id, _)) = keys.into_iter().next() else {
        return Err(PgpVerifyResult {
            error: Some("no keys available for verification".into()),
            ..Default::default()
        });
    };
    source
        .export_public(&first_key_id)
        .await
        .map_err(|e| PgpVerifyResult {
            error: Some(format!("export_public: {e}")),
            ..Default::default()
        })
}

async fn try_decrypt_pgp_mime(
    raw: &[u8],
    policy: RemotePolicy,
    source: &dyn inbx_pgp::KeySource,
) -> (PgpVerifyResult, Option<Rendered>) {
    let ct_bytes = match extract_pgp_encrypted_ciphertext(raw) {
        Some(b) => b,
        None => {
            return (
                PgpVerifyResult {
                    error: Some("could not extract PGP encrypted ciphertext part".into()),
                    ..Default::default()
                },
                None,
            );
        }
    };

    let ct = inbx_pgp::Ciphertext(ct_bytes);
    match source.decrypt(&ct).await {
        Ok((plain, _vr)) => {
            let decrypted_body = plain.0.clone();
            // Re-render the decrypted payload.
            let inner_rendered = render_message_inner(&plain.0, policy).ok();
            (
                PgpVerifyResult {
                    verified: false, // decrypt-only; signing is asserted via inner multipart/signed
                    decrypted_body: Some(decrypted_body),
                    ..Default::default()
                },
                inner_rendered,
            )
        }
        Err(e) => (
            PgpVerifyResult {
                error: Some(format!("decrypt: {e}")),
                ..Default::default()
            },
            None,
        ),
    }
}

/// For a `multipart/signed` raw message, extract `(signed_bytes, signature_bytes)`
/// where `signed_bytes` is the verbatim first MIME part bytes (CRLF normalised)
/// and `signature_bytes` is the body of the `application/pgp-signature` part.
fn extract_pgp_mime_parts(raw: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let s = std::str::from_utf8(raw).ok()?;
    let lower = s.to_ascii_lowercase();

    // Find boundary.
    let bnd_start = lower.find("boundary=\"")? + 10;
    let bnd_end = s[bnd_start..].find('"')? + bnd_start;
    let boundary = s[bnd_start..bnd_end].to_string();

    let sep = format!("--{boundary}");
    let end_sep = format!("--{boundary}--");

    let parts: Vec<&str> = s
        .split(sep.as_str())
        .filter(|p| !p.trim_start_matches("\r\n").starts_with('-'))
        .filter(|p| p != &"" && !p.starts_with("--"))
        .collect();

    // parts[0] = preamble, parts[1] = first body part, parts[2] = sig part
    // Collect properly by finding boundaries
    let mut boundaries: Vec<usize> = Vec::new();
    let mut search_from = 0;
    while let Some(pos) = s[search_from..].find(sep.as_str()) {
        let abs = search_from + pos;
        boundaries.push(abs);
        search_from = abs + sep.len();
    }

    if boundaries.len() < 2 {
        return None;
    }

    // Signed content: from end of first boundary line to start of second boundary.
    let first_sep_end = s[boundaries[0]..]
        .find("\r\n")
        .map(|o| boundaries[0] + o + 2)
        .or_else(|| s[boundaries[0]..].find('\n').map(|o| boundaries[0] + o + 1))?;

    let second_sep_start = boundaries[1];
    // Per RFC 3156: the CRLF immediately before the boundary delimiter is NOT
    // part of the signed data. Trim it.
    let signed_end = if second_sep_start >= 2
        && s.as_bytes().get(second_sep_start - 2) == Some(&b'\r')
        && s.as_bytes().get(second_sep_start - 1) == Some(&b'\n')
    {
        second_sep_start - 2
    } else if second_sep_start >= 1 && s.as_bytes().get(second_sep_start - 1) == Some(&b'\n') {
        second_sep_start - 1
    } else {
        second_sep_start
    };

    let signed_bytes = s.as_bytes()[first_sep_end..signed_end].to_vec();

    // Signature part: after third boundary line to end_sep or next boundary.
    let sig_sep_end = s[boundaries[1]..]
        .find("\r\n")
        .map(|o| boundaries[1] + o + 2)
        .or_else(|| s[boundaries[1]..].find('\n').map(|o| boundaries[1] + o + 1))?;

    let sig_part_end = if let Some(next_end) = s[sig_sep_end..].find(end_sep.as_str()) {
        sig_sep_end + next_end
    } else {
        s.len()
    };
    let sig_part = &s[sig_sep_end..sig_part_end];

    // Strip headers from sig_part to get the body.
    let sig_body = if let Some(idx) = sig_part.find("\r\n\r\n") {
        &sig_part[idx + 4..]
    } else if let Some(idx) = sig_part.find("\n\n") {
        &sig_part[idx + 2..]
    } else {
        sig_part
    };

    let _ = parts;
    Some((signed_bytes, sig_body.trim_end().as_bytes().to_vec()))
}

/// For a `multipart/encrypted` raw message, extract the ciphertext bytes
/// from the second part (`application/octet-stream`).
fn extract_pgp_encrypted_ciphertext(raw: &[u8]) -> Option<Vec<u8>> {
    let s = std::str::from_utf8(raw).ok()?;
    let lower = s.to_ascii_lowercase();

    let bnd_start = lower.find("boundary=\"")? + 10;
    let bnd_end = s[bnd_start..].find('"')? + bnd_start;
    let boundary = s[bnd_start..bnd_end].to_string();

    let sep = format!("--{boundary}");
    let end_sep = format!("--{boundary}--");

    // Find boundaries.
    let mut boundaries: Vec<usize> = Vec::new();
    let mut search_from = 0;
    while let Some(pos) = s[search_from..].find(sep.as_str()) {
        let abs = search_from + pos;
        boundaries.push(abs);
        search_from = abs + sep.len();
    }

    if boundaries.len() < 2 {
        return None;
    }

    // Second part (index 1).
    let second_start = s[boundaries[1]..]
        .find("\r\n")
        .map(|o| boundaries[1] + o + 2)
        .or_else(|| s[boundaries[1]..].find('\n').map(|o| boundaries[1] + o + 1))?;

    let second_end = if boundaries.len() > 2 {
        boundaries[2]
    } else if let Some(end_pos) = s[second_start..].find(end_sep.as_str()) {
        second_start + end_pos
    } else {
        s.len()
    };

    let second_part = &s[second_start..second_end];

    // Check if this part has Content-Transfer-Encoding: base64
    let headers_end = second_part
        .find("\r\n\r\n")
        .map(|i| (i, i + 4))
        .or_else(|| second_part.find("\n\n").map(|i| (i, i + 2)));
    let (_, body_start) = headers_end.unwrap_or((0, 0));
    let headers_str = &second_part[..body_start];
    let is_base64 = headers_str
        .to_ascii_lowercase()
        .contains("content-transfer-encoding: base64");

    let body = second_part[body_start..].trim_end();

    if is_base64 {
        // Strip whitespace from base64 body and decode.
        let b64_clean: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        use base64::{Engine, engine::general_purpose::STANDARD};
        STANDARD.decode(b64_clean.as_bytes()).ok()
    } else {
        Some(body.as_bytes().to_vec())
    }
}

fn render_message_inner(raw: &[u8], policy: RemotePolicy) -> Result<Rendered> {
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

    let phishing_warnings = phishing::analyze(raw, sanitized_html.as_deref());
    let security = pgp::detect(raw);

    Ok(Rendered {
        plain,
        html: sanitized_html,
        blocked_remote,
        trackers,
        inline_cids,
        phishing: phishing_warnings,
        security,
        pgp_verify: None,
        autocrypt: None,
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
