//! Display-time authentication signals: read the receiving MTA's
//! Authentication-Results header (RFC 8601) plus a few cheap phishing
//! heuristics. We don't run DKIM crypto here — that requires a verifier
//! talking DNS, which lives outside the render path. When the receiving
//! MTA already evaluated DKIM/SPF/DMARC the result is in the header it
//! stamped, and we surface that to the UI as a badge.

use mail_parser::{HeaderValue, MessageParser};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthVerdict {
    #[default]
    None,
    Pass,
    Fail,
    SoftFail,
    Neutral,
    Policy,
    PermError,
    TempError,
}

impl AuthVerdict {
    fn from_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "pass" => Self::Pass,
            "fail" => Self::Fail,
            "softfail" => Self::SoftFail,
            "neutral" => Self::Neutral,
            "policy" => Self::Policy,
            "permerror" => Self::PermError,
            "temperror" => Self::TempError,
            _ => Self::None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AuthResults {
    pub spf: AuthVerdict,
    pub dkim: AuthVerdict,
    pub dmarc: AuthVerdict,
}

#[derive(Debug, Clone, Default)]
pub struct Phishing {
    /// Reply-To address domain differs from the From address domain.
    pub reply_to_mismatch: bool,
    /// From display name contains an @ (classic spoofing pattern).
    pub display_name_email: bool,
    /// From domain looks like a homoglyph variant of a common host.
    pub lookalike_from: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AuthSignals {
    pub auth: AuthResults,
    pub phishing: Phishing,
}

pub fn evaluate(raw: &[u8]) -> AuthSignals {
    let parsed = match MessageParser::default().parse(raw) {
        Some(p) => p,
        None => return AuthSignals::default(),
    };
    let auth = parse_auth_results(&parsed);
    let phishing = phishing_signals(&parsed);
    AuthSignals { auth, phishing }
}

fn parse_auth_results(parsed: &mail_parser::Message<'_>) -> AuthResults {
    let mut out = AuthResults::default();
    for header in parsed.headers().iter() {
        if !header.name().eq_ignore_ascii_case("Authentication-Results") {
            continue;
        }
        let HeaderValue::Text(text) = header.value() else {
            continue;
        };
        for token in text.split(';') {
            let token = token.trim();
            for (prefix, slot) in [
                ("spf=", &mut out.spf),
                ("dkim=", &mut out.dkim),
                ("dmarc=", &mut out.dmarc),
            ] {
                if let Some(rest) = strip_prefix_ci(token, prefix) {
                    let value = rest
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .trim_matches(|c: char| c == '"' || c == ',');
                    *slot = AuthVerdict::from_str(value);
                }
            }
        }
    }
    out
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() < prefix.len() {
        return None;
    }
    let head = &s[..prefix.len()];
    if head.eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

fn phishing_signals(parsed: &mail_parser::Message<'_>) -> Phishing {
    let mut out = Phishing::default();
    let from = parsed
        .from()
        .and_then(|a| a.first())
        .and_then(|a| a.address())
        .map(|s| s.to_string());
    let from_name = parsed
        .from()
        .and_then(|a| a.first())
        .and_then(|a| a.name())
        .map(|s| s.to_string());
    let reply_to = parsed
        .reply_to()
        .and_then(|a| a.first())
        .and_then(|a| a.address())
        .map(|s| s.to_string());

    if let (Some(f), Some(r)) = (from.as_deref(), reply_to.as_deref())
        && domain_of(f) != domain_of(r)
    {
        out.reply_to_mismatch = true;
    }
    if let Some(name) = from_name
        && name.contains('@')
    {
        out.display_name_email = true;
    }
    if let Some(f) = from.as_deref()
        && is_lookalike(domain_of(f))
    {
        out.lookalike_from = true;
    }
    out
}

fn domain_of(addr: &str) -> &str {
    addr.rsplit_once('@').map(|(_, d)| d).unwrap_or("")
}

const LOOKALIKE_HOSTS: &[&str] = &[
    "g00gle.com",
    "gooogle.com",
    "paypa1.com",
    "amaz0n.com",
    "rnicrosoft.com",
    "microsofft.com",
    "app1e.com",
    "faceb00k.com",
    "1inkedin.com",
];

fn is_lookalike(host: &str) -> bool {
    LOOKALIKE_HOSTS.iter().any(|h| h.eq_ignore_ascii_case(host))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_auth_header() {
        let raw = b"Authentication-Results: mx.example.com;\r\n\
                    \tspf=pass smtp.mailfrom=alice@example.com;\r\n\
                    \tdkim=pass header.d=example.com header.s=k1;\r\n\
                    \tdmarc=fail (p=NONE) header.from=example.com\r\n\
                    From: alice@example.com\r\n\r\nbody\r\n";
        let s = evaluate(raw);
        assert_eq!(s.auth.spf, AuthVerdict::Pass);
        assert_eq!(s.auth.dkim, AuthVerdict::Pass);
        assert_eq!(s.auth.dmarc, AuthVerdict::Fail);
    }

    #[test]
    fn detects_reply_to_mismatch() {
        let raw = b"From: alice@example.com\r\n\
                    Reply-To: alice@phishing.example\r\n\r\nbody\r\n";
        let s = evaluate(raw);
        assert!(s.phishing.reply_to_mismatch);
    }

    #[test]
    fn detects_lookalike() {
        let raw = b"From: support@paypa1.com\r\n\r\nbody\r\n";
        let s = evaluate(raw);
        assert!(s.phishing.lookalike_from);
    }
}
