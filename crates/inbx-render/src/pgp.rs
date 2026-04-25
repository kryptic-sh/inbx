//! Detect PGP / S/MIME signatures in incoming MIME mail.
//!
//! Verification against a key trust store is deferred — this milestone
//! surfaces presence so the UI can badge messages as "signed". Full
//! crypto verification lands when we add a key store under
//! ~/.local/share/inbx/<account>/pgp/.

use mail_parser::{MessageParser, MimeHeaders, PartType};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SecureKind {
    #[default]
    None,
    /// `multipart/signed; protocol="application/pgp-signature"` (RFC 3156).
    PgpMime,
    /// In-body `-----BEGIN PGP SIGNED MESSAGE-----` armor.
    PgpInline,
    /// `multipart/signed; protocol="application/pkcs7-signature"` (S/MIME).
    SMime,
    /// `multipart/encrypted; protocol="application/pgp-encrypted"`.
    PgpEncrypted,
    /// `application/pkcs7-mime; smime-type=enveloped-data` (S/MIME).
    SMimeEncrypted,
}

#[derive(Debug, Clone, Default)]
pub struct SecurityInfo {
    pub kind: SecureKind,
    /// Verification deferred — true only when a future verifier confirmed it.
    pub verified: bool,
    /// Optional summary surfaced to UI ("PGP signed", "S/MIME encrypted", …).
    pub label: Option<&'static str>,
}

pub fn detect(raw: &[u8]) -> SecurityInfo {
    let Some(parsed) = MessageParser::default().parse(raw) else {
        return SecurityInfo::default();
    };
    for part in parsed.parts.iter() {
        if let Some(ct) = part.content_type() {
            let ctype = ct.ctype().to_ascii_lowercase();
            let stype = ct
                .subtype()
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            let proto = ct
                .attribute("protocol")
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            let smime_type = ct
                .attribute("smime-type")
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();

            if ctype == "multipart" && stype == "signed" {
                if proto.contains("pgp-signature") {
                    return SecurityInfo {
                        kind: SecureKind::PgpMime,
                        verified: false,
                        label: Some("PGP/MIME signed"),
                    };
                }
                if proto.contains("pkcs7-signature") {
                    return SecurityInfo {
                        kind: SecureKind::SMime,
                        verified: false,
                        label: Some("S/MIME signed"),
                    };
                }
            }
            if ctype == "multipart" && stype == "encrypted" && proto.contains("pgp-encrypted") {
                return SecurityInfo {
                    kind: SecureKind::PgpEncrypted,
                    verified: false,
                    label: Some("PGP encrypted"),
                };
            }
            if ctype == "application" && stype == "pkcs7-mime" && smime_type.contains("enveloped") {
                return SecurityInfo {
                    kind: SecureKind::SMimeEncrypted,
                    verified: false,
                    label: Some("S/MIME encrypted"),
                };
            }
        }
        if let PartType::Text(t) = &part.body
            && t.contains("-----BEGIN PGP SIGNED MESSAGE-----")
        {
            return SecurityInfo {
                kind: SecureKind::PgpInline,
                verified: false,
                label: Some("PGP inline signed"),
            };
        }
    }
    SecurityInfo::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_pgp_mime() {
        let raw = b"From: a@x\r\n\
                    Content-Type: multipart/signed; boundary=\"--\"; protocol=\"application/pgp-signature\"\r\n\r\n\
                    ----\r\n\
                    Content-Type: text/plain\r\n\r\nhi\r\n\
                    ----\r\n\
                    Content-Type: application/pgp-signature\r\n\r\n-----BEGIN PGP SIGNATURE-----\r\n\
                    ----\r\n";
        let s = detect(raw);
        assert_eq!(s.kind, SecureKind::PgpMime);
    }

    #[test]
    fn detects_inline_pgp() {
        let raw = b"From: a@x\r\n\r\n-----BEGIN PGP SIGNED MESSAGE-----\r\n\
                    Hash: SHA256\r\n\r\nhello\r\n";
        let s = detect(raw);
        assert_eq!(s.kind, SecureKind::PgpInline);
    }

    #[test]
    fn unsigned_returns_none() {
        let raw = b"From: a@x\r\n\r\nplain body\r\n";
        let s = detect(raw);
        assert_eq!(s.kind, SecureKind::None);
    }
}
