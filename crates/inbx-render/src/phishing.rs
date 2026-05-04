//! Phishing heuristics: reply-to domain mismatch, lookalike domains, and
//! link-text / href domain mismatch. All checks are purely local — no DNS.

use mail_parser::MessageParser;

/// Extract the domain part of an email address (everything after the last `@`),
/// lowercased. Returns `None` if `addr` contains no `@` or the domain is empty.
fn domain_of(addr: &str) -> Option<String> {
    let domain = addr.rsplit_once('@')?.1;
    let domain = domain.trim_end_matches('.');
    if domain.is_empty() {
        None
    } else {
        Some(domain.to_ascii_lowercase())
    }
}

/// Hand-rolled Levenshtein distance, capped at 2 to stay O(n).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let n = a.len();
    let m = b.len();
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr = vec![0usize; m + 1];
    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[m]
}

/// Well-known domains against which we check for homoglyph / typo lookalikes.
const WELL_KNOWN: &[&str] = &[
    "gmail.com",
    "google.com",
    "microsoft.com",
    "outlook.com",
    "apple.com",
    "paypal.com",
    "amazon.com",
    "github.com",
    "kryptic.sh",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhishingWarning {
    /// Reply-To header points at a different domain than From.
    ReplyToDomainMismatch {
        from_domain: String,
        reply_to_domain: String,
    },
    /// From-address domain looks like a well-known domain (homoglyph / typo).
    LookalikeFromDomain {
        from_domain: String,
        looks_like: String,
    },
    /// An `<a>` tag's visible text and href point to different domains.
    LinkTextHrefMismatch {
        text_domain: String,
        href_domain: String,
        href: String,
    },
}

/// Run all heuristics against the parsed message + sanitized HTML.
pub fn analyze(raw: &[u8], html: Option<&str>) -> Vec<PhishingWarning> {
    let mut warnings = Vec::new();

    if let Some(parsed) = MessageParser::default().parse(raw) {
        let from_domain = parsed
            .from()
            .and_then(|a| a.first())
            .and_then(|a| a.address())
            .and_then(domain_of);

        // --- Reply-To domain mismatch ---
        if let Some(ref fd) = from_domain
            && let Some(reply_to_domain) = parsed
                .reply_to()
                .and_then(|a| a.first())
                .and_then(|a| a.address())
                .and_then(domain_of)
            && *fd != reply_to_domain
        {
            warnings.push(PhishingWarning::ReplyToDomainMismatch {
                from_domain: fd.clone(),
                reply_to_domain,
            });
        }

        // --- Lookalike / homoglyph from domain ---
        if let Some(ref fd) = from_domain {
            // Skip if already an exact well-known match.
            let is_exact = WELL_KNOWN.contains(&fd.as_str());
            if !is_exact {
                for &target in WELL_KNOWN {
                    if levenshtein(fd, target) <= 1 {
                        warnings.push(PhishingWarning::LookalikeFromDomain {
                            from_domain: fd.clone(),
                            looks_like: target.to_string(),
                        });
                        break; // one hit is enough
                    }
                }
            }
        }
    }

    // --- Link text ↔ href domain mismatch ---
    if let Some(html) = html {
        warnings.extend(check_link_mismatches(html));
    }

    warnings
}

/// Extract the domain from an absolute URL (`http://`, `https://`).
/// Returns `None` for relative URLs, `mailto:`, `tel:`, etc.
fn domain_of_url(url: &str) -> Option<String> {
    let url_lc = url.to_ascii_lowercase();
    let rest = url_lc
        .strip_prefix("https://")
        .or_else(|| url_lc.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#', ':']).next()?;
    let host = host.trim_end_matches('.');
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// Return true if `text` looks like it contains a domain name
/// (at least one dot with letters/digits on both sides).
fn text_contains_domain(text: &str) -> Option<String> {
    // Walk words and look for `word.tld` patterns.
    for word in text.split_ascii_whitespace() {
        // Strip leading/trailing punctuation.
        let word = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '.' && c != '-');
        if let Some(dot_pos) = word.find('.') {
            let lhs = &word[..dot_pos];
            let rhs = &word[dot_pos + 1..];
            // lhs: at least one alnum; rhs: 2+ letters (TLD-ish).
            let lhs_ok =
                lhs.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') && !lhs.is_empty();
            let rhs_ok = rhs.len() >= 2
                && rhs
                    .chars()
                    .all(|c| c.is_ascii_alphabetic() || c == '.' || c == '-');
            if lhs_ok && rhs_ok {
                // Return the leading part up to any path separator.
                let domain_part = word
                    .split(['/', '?', '#'])
                    .next()
                    .unwrap_or(word)
                    .to_ascii_lowercase();
                return Some(domain_part);
            }
        }
    }
    None
}

/// Strip HTML tags from a snippet to get visible text.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

/// Scan the sanitized HTML for `<a href="…">TEXT</a>` patterns and check
/// whether the visible TEXT contains a domain that differs from the href's domain.
fn check_link_mismatches(html: &str) -> Vec<PhishingWarning> {
    let mut warnings = Vec::new();
    let lower = html.to_ascii_lowercase();
    let mut search_from = 0;

    while let Some(rel) = lower[search_from..].find("<a ") {
        let tag_start = search_from + rel;

        // Find end of opening tag.
        let Some(tag_close) = lower[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + tag_close + 1;

        // Extract href from the opening tag (use original case for URL).
        let opening_tag = &html[tag_start..tag_end];
        let href_opt = extract_href(opening_tag);

        // Find closing </a>.
        let rest_start = tag_end;
        let close_tag = lower[rest_start..].find("</a");
        let (inner_html, next_search) = if let Some(close_rel) = close_tag {
            let close_start = rest_start + close_rel;
            let after_close = lower[close_start..].find('>').map(|r| close_start + r + 1);
            (
                &html[tag_end..close_start],
                after_close.unwrap_or(close_start),
            )
        } else {
            // No closing tag found, stop.
            break;
        };

        search_from = next_search;

        let Some(href) = href_opt else {
            continue;
        };
        let Some(href_domain) = domain_of_url(&href) else {
            continue; // skip relative, mailto:, tel:
        };

        let visible_text = strip_tags(inner_html);
        if let Some(text_domain) = text_contains_domain(&visible_text)
            && text_domain != href_domain
        {
            warnings.push(PhishingWarning::LinkTextHrefMismatch {
                text_domain,
                href_domain,
                href,
            });
        }
    }

    warnings
}

/// Extract the `href` attribute value from an `<a ...>` tag (original case).
fn extract_href(tag: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    // Look for href= with optional whitespace.
    let needle = "href=";
    let pos = lower.find(needle)?;
    let after = &tag[pos + needle.len()..];
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_raw(from: &str, reply_to: Option<&str>) -> Vec<u8> {
        let mut msg = format!("From: {from}\r\n");
        if let Some(rt) = reply_to {
            msg.push_str(&format!("Reply-To: {rt}\r\n"));
        }
        msg.push_str("\r\nbody\r\n");
        msg.into_bytes()
    }

    #[test]
    fn replyto_mismatch_flagged() {
        let raw = make_raw("alice@example.com", Some("alice@bad.tld"));
        let w = analyze(&raw, None);
        assert!(
            w.iter().any(|x| matches!(
                x,
                PhishingWarning::ReplyToDomainMismatch {
                    from_domain,
                    reply_to_domain
                } if from_domain == "example.com" && reply_to_domain == "bad.tld"
            )),
            "expected ReplyToDomainMismatch, got {:?}",
            w
        );
    }

    #[test]
    fn replyto_match_no_warning() {
        let raw = make_raw("alice@example.com", Some("alice@example.com"));
        let w = analyze(&raw, None);
        assert!(
            !w.iter()
                .any(|x| matches!(x, PhishingWarning::ReplyToDomainMismatch { .. })),
            "unexpected ReplyToDomainMismatch"
        );
    }

    #[test]
    fn lookalike_gmaii_flagged() {
        let raw = make_raw("alice@gmaii.com", None);
        let w = analyze(&raw, None);
        assert!(
            w.iter().any(|x| matches!(
                x,
                PhishingWarning::LookalikeFromDomain { from_domain, looks_like }
                if from_domain == "gmaii.com" && looks_like == "gmail.com"
            )),
            "expected LookalikeFromDomain for gmaii.com, got {:?}",
            w
        );
    }

    #[test]
    fn lookalike_real_gmail_no_warning() {
        let raw = make_raw("alice@gmail.com", None);
        let w = analyze(&raw, None);
        assert!(
            !w.iter()
                .any(|x| matches!(x, PhishingWarning::LookalikeFromDomain { .. })),
            "unexpected LookalikeFromDomain for real gmail.com"
        );
    }

    #[test]
    fn link_text_href_mismatch_flagged() {
        let html = r#"<p>Click <a href="https://evil.com">paypal.com login</a></p>"#;
        let raw = make_raw("sender@other.com", None);
        let w = analyze(&raw, Some(html));
        assert!(
            w.iter().any(|x| matches!(
                x,
                PhishingWarning::LinkTextHrefMismatch { text_domain, href_domain, .. }
                if text_domain == "paypal.com" && href_domain == "evil.com"
            )),
            "expected LinkTextHrefMismatch, got {:?}",
            w
        );
    }

    #[test]
    fn link_text_relative_no_warning() {
        let html = r#"<p><a href="/account">your account</a></p>"#;
        let raw = make_raw("sender@other.com", None);
        let w = analyze(&raw, Some(html));
        assert!(
            !w.iter()
                .any(|x| matches!(x, PhishingWarning::LinkTextHrefMismatch { .. })),
            "unexpected LinkTextHrefMismatch for relative href"
        );
    }

    #[test]
    fn levenshtein_distances() {
        assert_eq!(levenshtein("gmail.com", "gmail.com"), 0);
        assert_eq!(levenshtein("gmaii.com", "gmail.com"), 1);
        assert_eq!(levenshtein("gmaill.com", "gmail.com"), 1);
        assert_eq!(levenshtein("abc", "xyz"), 3);
    }
}
