//! Pure (no-network) provider autoconfig.
//!
//! Given an email address, suggest IMAP/SMTP host/port/security and an OAuth
//! provider hint based on a built-in table of common providers. Unknown
//! domains fall back to a `imap.<domain>` / `smtp.<domain>` heuristic guess.

use crate::{OauthProvider, TlsMode};

/// Where the suggestion came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuggestionSource {
    /// Matched a built-in provider entry (e.g. "gmail", "fastmail").
    BuiltIn { name: &'static str },
    /// Domain not recognized; values are a best-effort guess from the domain.
    DomainGuess,
}

/// Suggested account fields for a given email address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: TlsMode,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: TlsMode,
    pub oauth_provider: Option<OauthProvider>,
    pub source: SuggestionSource,
}

/// Suggest configuration for the given email address.
///
/// Returns `None` when:
/// - the input has no `@` (not an email), or
/// - the domain is on the unsupported list (e.g. Tutanota, which has no IMAP).
pub fn suggest(email: &str) -> Option<Suggestion> {
    let domain = email.split_once('@')?.1.trim().to_ascii_lowercase();
    if domain.is_empty() {
        return None;
    }

    if is_unsupported(&domain) {
        return None;
    }

    if let Some(s) = builtin(&domain) {
        return Some(s);
    }

    // Fallback: pure guess from the domain. This is unlikely to be correct for
    // arbitrary domains; callers should let the user confirm/edit.
    Some(Suggestion {
        imap_host: format!("imap.{domain}"),
        imap_port: 993,
        imap_security: TlsMode::Tls,
        smtp_host: format!("smtp.{domain}"),
        smtp_port: 465,
        smtp_security: TlsMode::Tls,
        oauth_provider: None,
        source: SuggestionSource::DomainGuess,
    })
}

fn is_unsupported(domain: &str) -> bool {
    matches!(domain, "tutanota.com" | "tuta.io")
}

fn builtin(domain: &str) -> Option<Suggestion> {
    match domain {
        "gmail.com" | "googlemail.com" => Some(Suggestion {
            imap_host: "imap.gmail.com".into(),
            imap_port: 993,
            imap_security: TlsMode::Tls,
            smtp_host: "smtp.gmail.com".into(),
            smtp_port: 465,
            smtp_security: TlsMode::Tls,
            oauth_provider: Some(OauthProvider::Gmail),
            source: SuggestionSource::BuiltIn { name: "gmail" },
        }),
        "outlook.com" | "hotmail.com" | "live.com" | "msn.com" | "office365.com" => {
            Some(Suggestion {
                imap_host: "outlook.office365.com".into(),
                imap_port: 993,
                imap_security: TlsMode::Tls,
                smtp_host: "smtp.office365.com".into(),
                smtp_port: 587,
                smtp_security: TlsMode::Starttls,
                oauth_provider: Some(OauthProvider::Microsoft {
                    tenant: "common".into(),
                }),
                source: SuggestionSource::BuiltIn { name: "microsoft" },
            })
        }
        "fastmail.com" | "fastmail.fm" | "messagingengine.com" => Some(Suggestion {
            imap_host: "imap.fastmail.com".into(),
            imap_port: 993,
            imap_security: TlsMode::Tls,
            smtp_host: "smtp.fastmail.com".into(),
            smtp_port: 465,
            smtp_security: TlsMode::Tls,
            oauth_provider: None,
            source: SuggestionSource::BuiltIn { name: "fastmail" },
        }),
        "icloud.com" | "me.com" | "mac.com" => Some(Suggestion {
            imap_host: "imap.mail.me.com".into(),
            imap_port: 993,
            imap_security: TlsMode::Tls,
            smtp_host: "smtp.mail.me.com".into(),
            smtp_port: 587,
            smtp_security: TlsMode::Starttls,
            oauth_provider: None,
            source: SuggestionSource::BuiltIn { name: "icloud" },
        }),
        "yahoo.com" | "ymail.com" => Some(Suggestion {
            imap_host: "imap.mail.yahoo.com".into(),
            imap_port: 993,
            imap_security: TlsMode::Tls,
            smtp_host: "smtp.mail.yahoo.com".into(),
            smtp_port: 465,
            smtp_security: TlsMode::Tls,
            oauth_provider: None,
            source: SuggestionSource::BuiltIn { name: "yahoo" },
        }),
        "aol.com" => Some(Suggestion {
            imap_host: "imap.aol.com".into(),
            imap_port: 993,
            imap_security: TlsMode::Tls,
            smtp_host: "smtp.aol.com".into(),
            smtp_port: 465,
            smtp_security: TlsMode::Tls,
            oauth_provider: None,
            source: SuggestionSource::BuiltIn { name: "aol" },
        }),
        // Proton requires the local Bridge, which exposes plaintext+STARTTLS
        // on loopback ports.
        "protonmail.com" | "proton.me" | "pm.me" => Some(Suggestion {
            imap_host: "127.0.0.1".into(),
            imap_port: 1143,
            imap_security: TlsMode::Starttls,
            smtp_host: "127.0.0.1".into(),
            smtp_port: 1025,
            smtp_security: TlsMode::Starttls,
            oauth_provider: None,
            source: SuggestionSource::BuiltIn {
                name: "proton-bridge",
            },
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gmail() {
        let s = suggest("user@gmail.com").unwrap();
        assert_eq!(s.imap_host, "imap.gmail.com");
        assert_eq!(s.imap_port, 993);
        assert_eq!(s.imap_security, TlsMode::Tls);
        assert_eq!(s.smtp_host, "smtp.gmail.com");
        assert_eq!(s.smtp_port, 465);
        assert_eq!(s.smtp_security, TlsMode::Tls);
        assert_eq!(s.oauth_provider, Some(OauthProvider::Gmail));
        assert_eq!(s.source, SuggestionSource::BuiltIn { name: "gmail" });
    }

    #[test]
    fn outlook() {
        let s = suggest("user@outlook.com").unwrap();
        assert_eq!(s.imap_host, "outlook.office365.com");
        assert_eq!(s.imap_port, 993);
        assert_eq!(s.imap_security, TlsMode::Tls);
        assert_eq!(s.smtp_host, "smtp.office365.com");
        assert_eq!(s.smtp_port, 587);
        assert_eq!(s.smtp_security, TlsMode::Starttls);
        assert!(matches!(
            s.oauth_provider,
            Some(OauthProvider::Microsoft { .. })
        ));
        assert_eq!(s.source, SuggestionSource::BuiltIn { name: "microsoft" });
    }

    #[test]
    fn fastmail() {
        let s = suggest("user@fastmail.com").unwrap();
        assert_eq!(s.imap_host, "imap.fastmail.com");
        assert_eq!(s.smtp_host, "smtp.fastmail.com");
        assert_eq!(s.oauth_provider, None);
        assert_eq!(s.source, SuggestionSource::BuiltIn { name: "fastmail" });
    }

    #[test]
    fn unknown_domain_falls_back_to_guess() {
        let s = suggest("user@example.org").unwrap();
        assert_eq!(s.imap_host, "imap.example.org");
        assert_eq!(s.imap_port, 993);
        assert_eq!(s.imap_security, TlsMode::Tls);
        assert_eq!(s.smtp_host, "smtp.example.org");
        assert_eq!(s.smtp_port, 465);
        assert_eq!(s.smtp_security, TlsMode::Tls);
        assert_eq!(s.oauth_provider, None);
        assert_eq!(s.source, SuggestionSource::DomainGuess);
    }

    #[test]
    fn missing_at_returns_none() {
        assert!(suggest("not-an-email").is_none());
    }

    #[test]
    fn tutanota_unsupported() {
        assert!(suggest("user@tutanota.com").is_none());
        assert!(suggest("user@tuta.io").is_none());
    }

    #[test]
    fn case_insensitive_domain() {
        let s = suggest("User@GMail.COM").unwrap();
        assert_eq!(s.source, SuggestionSource::BuiltIn { name: "gmail" });
    }
}
