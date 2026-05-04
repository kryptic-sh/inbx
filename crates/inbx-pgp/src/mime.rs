//! RFC 3156 PGP/MIME message assembly.
//!
//! Two public functions:
//!  - [`sign_pgp_mime`]    — multipart/signed (RFC 3156 §5)
//!  - [`encrypt_pgp_mime`] — multipart/encrypted (RFC 3156 §6.2 / §6.1)

use crate::{ArmoredKey, KeyId, KeySource, Result};

/// Headers placed on the outer RFC 5322 envelope that wraps PGP/MIME content.
#[derive(Debug, Clone)]
pub struct OuterHeaders {
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    /// RFC 2822 formatted timestamp. `None` → use "Thu, 01 Jan 1970 00:00:00 +0000".
    pub date: Option<String>,
    /// Pre-built Autocrypt header value (no leading "Autocrypt:" or trailing CRLF).
    /// When `Some`, both `sign_pgp_mime` and `encrypt_pgp_mime` emit the header.
    pub autocrypt: Option<String>,
}

/// Wrap a fully-formed RFC 5322 inner message in a multipart/signed envelope
/// per RFC 3156 §5.
///
/// The inner bytes are CRLF-normalised before signing.  The detached
/// signature is produced by `source.sign_detached` which uses SHA-256;
/// accordingly `micalg` is hard-coded to "pgp-sha256" in this slice.
pub async fn sign_pgp_mime(
    source: &dyn KeySource,
    signer: &KeyId,
    inner_rfc5322: &[u8],
    outer_headers: &OuterHeaders,
) -> Result<Vec<u8>> {
    let inner_crlf = normalize_crlf(inner_rfc5322);
    let sig = source.sign_detached(signer, &inner_crlf).await?;
    let sig_armored = String::from_utf8_lossy(&sig.0).into_owned();

    let boundary = make_boundary("pgp-signed");

    let mut out = Vec::new();
    write_outer_headers(&mut out, outer_headers);
    out.extend_from_slice(
        format!(
            "Content-Type: multipart/signed; protocol=\"application/pgp-signature\"; \
             micalg=\"pgp-sha256\"; boundary=\"{boundary}\"\r\n\
             MIME-Version: 1.0\r\n\
             \r\n\
             --{boundary}\r\n"
        )
        .as_bytes(),
    );
    out.extend_from_slice(&inner_crlf);
    out.extend_from_slice(
        format!(
            "\r\n--{boundary}\r\n\
             Content-Type: application/pgp-signature; name=\"signature.asc\"\r\n\
             Content-Description: OpenPGP digital signature\r\n\
             Content-Disposition: attachment; filename=\"signature.asc\"\r\n\
             \r\n\
             {sig_armored}\r\n\
             --{boundary}--\r\n"
        )
        .as_bytes(),
    );
    Ok(out)
}

/// Wrap a fully-formed RFC 5322 inner message in a multipart/encrypted
/// envelope per RFC 3156 §6.2.
///
/// If `signer` is `Some`, the inner is first signed via [`sign_pgp_mime`]
/// (§6.1 — sign-then-encrypt).  The ciphertext is emitted in ASCII armor
/// for human-debuggability.
pub async fn encrypt_pgp_mime(
    source: &dyn KeySource,
    signer: Option<&KeyId>,
    recipients: &[ArmoredKey],
    inner_rfc5322: &[u8],
    outer_headers: &OuterHeaders,
) -> Result<Vec<u8>> {
    // Optionally sign first (RFC 3156 §6.1).
    let inner_bytes: Vec<u8> = if let Some(s) = signer {
        // Use a neutral outer-headers set for the signed inner blob —
        // headers visible to the recipient come from the outer envelope.
        let inner_headers = OuterHeaders {
            from: outer_headers.from.clone(),
            to: outer_headers.to.clone(),
            cc: outer_headers.cc.clone(),
            bcc: outer_headers.bcc.clone(),
            subject: outer_headers.subject.clone(),
            message_id: None,
            in_reply_to: None,
            references: Vec::new(),
            date: outer_headers.date.clone(),
            autocrypt: None,
        };
        sign_pgp_mime(source, s, inner_rfc5322, &inner_headers).await?
    } else {
        normalize_crlf(inner_rfc5322)
    };

    let ciphertext = source.encrypt_to(recipients, &inner_bytes).await?;

    // Base64-encode the binary ciphertext (76-char line-wrapped) so it survives
    // text-mode MIME transport. The render-side extractor base64-decodes it back.
    let ciphertext_b64 = b64_wrap(&ciphertext.0);

    let boundary = make_boundary("pgp-encrypted");

    let mut out = Vec::new();
    write_outer_headers(&mut out, outer_headers);
    out.extend_from_slice(
        format!(
            "Content-Type: multipart/encrypted; \
             protocol=\"application/pgp-encrypted\"; boundary=\"{boundary}\"\r\n\
             MIME-Version: 1.0\r\n\
             \r\n\
             --{boundary}\r\n\
             Content-Type: application/pgp-encrypted\r\n\
             Content-Description: PGP/MIME version identification\r\n\
             \r\n\
             Version: 1\r\n\
             \r\n\
             --{boundary}\r\n\
             Content-Type: application/octet-stream; name=\"encrypted.gpg\"\r\n\
             Content-Description: OpenPGP encrypted message\r\n\
             Content-Disposition: inline; filename=\"encrypted.gpg\"\r\n\
             Content-Transfer-Encoding: base64\r\n\
             \r\n\
             {ciphertext_b64}\r\n\
             --{boundary}--\r\n"
        )
        .as_bytes(),
    );
    Ok(out)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Normalise LF→CRLF and bare CR→CRLF without touching existing CRLF.
fn normalize_crlf(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 20);
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        if b == b'\r' {
            out.push(b'\r');
            out.push(b'\n');
            if data.get(i + 1) == Some(&b'\n') {
                i += 1; // skip the following LF — it will be emitted as part of CRLF already
            }
        } else if b == b'\n' {
            out.push(b'\r');
            out.push(b'\n');
        } else {
            out.push(b);
        }
        i += 1;
    }
    out
}

/// Produce a stable-enough MIME boundary string.
fn make_boundary(tag: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("inbx-{tag}-{ts:08x}")
}

/// Write the standard outer envelope headers.
fn write_outer_headers(buf: &mut Vec<u8>, h: &OuterHeaders) {
    let date = h
        .date
        .clone()
        .unwrap_or_else(|| "Thu, 01 Jan 1970 00:00:00 +0000".to_string());
    buf.extend_from_slice(format!("Date: {date}\r\n").as_bytes());
    buf.extend_from_slice(format!("From: {}\r\n", h.from).as_bytes());
    for t in &h.to {
        buf.extend_from_slice(format!("To: {t}\r\n").as_bytes());
    }
    for c in &h.cc {
        buf.extend_from_slice(format!("Cc: {c}\r\n").as_bytes());
    }
    // BCC intentionally omitted from wire message.
    buf.extend_from_slice(format!("Subject: {}\r\n", h.subject).as_bytes());
    if let Some(mid) = &h.message_id {
        buf.extend_from_slice(format!("Message-ID: {mid}\r\n").as_bytes());
    }
    if let Some(irt) = &h.in_reply_to {
        buf.extend_from_slice(format!("In-Reply-To: {irt}\r\n").as_bytes());
    }
    if !h.references.is_empty() {
        buf.extend_from_slice(format!("References: {}\r\n", h.references.join(" ")).as_bytes());
    }
    if let Some(ac) = &h.autocrypt {
        buf.extend_from_slice(format!("Autocrypt: {ac}\r\n").as_bytes());
    }
}

/// The `prefer-encrypt` attribute from an Autocrypt 1.1 header (§4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutocryptPreference {
    /// Attribute absent, or set to `nopreference` — default per spec.
    #[default]
    Nopreference,
    /// Sender advertises `prefer-encrypt=mutual`.
    Mutual,
}

/// Parsed representation of an `Autocrypt:` header.
#[derive(Debug, Clone)]
pub struct AutocryptHeader {
    /// The `addr=` value (sender email).
    pub addr: String,
    /// Re-armored ASCII public key (round-tripped through binary OpenPGP packets).
    pub keydata_armored: String,
    /// Lowercase hex fingerprint of the primary key.
    pub fingerprint: String,
    /// Autocrypt 1.1 §4 preference; `Nopreference` when the attribute is absent.
    pub prefer_encrypt: AutocryptPreference,
}

/// Parse an `Autocrypt:` header value (everything AFTER `"Autocrypt: "`).
///
/// Per Autocrypt 1.1 §2:
///  - attributes are `key=value;` separated
///  - RFC 5322 line folds (`\r\n` + leading whitespace) inside `keydata=` are
///    collapsed before base64-decoding
///  - `addr=` is required; `keydata=` is required
///  - unknown attributes are silently ignored (forward compatibility)
pub fn parse_autocrypt_header(value: &str) -> Result<AutocryptHeader> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use pgp::composed::Deserializable;
    use pgp::types::KeyDetails;

    // Collapse RFC 5322 line folds: CRLF + leading whitespace → nothing.
    // Also handle bare LF folds (liberal parsing).
    let unfolded = {
        let mut s = value.to_string();
        // CRLF + whitespace
        while let Some(pos) = s.find("\r\n") {
            let after = pos + 2;
            if s[after..].starts_with([' ', '\t']) {
                s.replace_range(pos..after + 1, "");
            } else {
                break;
            }
        }
        // bare LF + whitespace
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\n'
                && chars
                    .peek()
                    .map(|p| *p == ' ' || *p == '\t')
                    .unwrap_or(false)
            {
                chars.next(); // drop the leading whitespace
                continue;
            }
            out.push(c);
        }
        out
    };

    let mut addr: Option<String> = None;
    let mut keydata_b64: Option<String> = None;
    let mut prefer_encrypt = AutocryptPreference::Nopreference;

    for part in unfolded.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some(eq) = part.find('=') {
            let key = part[..eq].trim().to_ascii_lowercase();
            let val = part[eq + 1..].trim();
            match key.as_str() {
                "addr" => addr = Some(val.to_string()),
                "keydata" => {
                    // Strip all whitespace from keydata value.
                    let clean: String = val.chars().filter(|c| !c.is_ascii_whitespace()).collect();
                    keydata_b64 = Some(clean);
                }
                "prefer-encrypt" if val.eq_ignore_ascii_case("mutual") => {
                    prefer_encrypt = AutocryptPreference::Mutual;
                }
                _ => {} // ignore other unknown attrs
            }
        }
    }

    let addr = addr.ok_or_else(|| crate::Error::Rpgp("Autocrypt: missing addr=".into()))?;
    let keydata_b64 =
        keydata_b64.ok_or_else(|| crate::Error::Rpgp("Autocrypt: missing keydata=".into()))?;

    let binary = STANDARD
        .decode(keydata_b64.as_bytes())
        .map_err(|e| crate::Error::Rpgp(format!("Autocrypt keydata base64: {e}")))?;

    let signed_key =
        pgp::composed::SignedPublicKey::from_bytes(std::io::BufReader::new(binary.as_slice()))
            .map_err(|e| crate::Error::Rpgp(format!("Autocrypt key parse: {e}")))?;

    let fingerprint = signed_key
        .primary_key
        .fingerprint()
        .to_string()
        .to_lowercase();

    let keydata_armored = signed_key
        .to_armored_string(None.into())
        .map_err(|e| crate::Error::Rpgp(format!("Autocrypt re-armor: {e}")))?;

    Ok(AutocryptHeader {
        addr,
        keydata_armored,
        fingerprint,
        prefer_encrypt,
    })
}

/// Build the value for an `Autocrypt:` header per Autocrypt 1.1.
///
/// Returns the header value (no leading "Autocrypt:" or trailing CRLF).
/// The `keydata` segment is folded at 78 cols (per RFC 5322 §2.2.3) by
/// inserting `"\r\n "` after every 76 base64 chars.
///
/// When `prefer_encrypt_mutual` is `true`, the header includes
/// `; prefer-encrypt=mutual` per Autocrypt 1.1 §4.
///
/// # Note
/// The gnupg key-source path is not supported here — see the
/// `// TODO: gnupg-source autocrypt` comment in `inbx-composer`.
pub fn autocrypt_header_value(
    addr: &str,
    armored_pubkey: &str,
    prefer_encrypt_mutual: bool,
) -> Result<String> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use pgp::composed::Deserializable;
    use std::io::BufReader;

    // Parse the armored public key.
    let (pubkey, _) = pgp::composed::SignedPublicKey::from_armor_single(BufReader::new(
        armored_pubkey.as_bytes(),
    ))
    .map_err(|e| crate::Error::Rpgp(e.to_string()))?;

    // Serialize as binary OpenPGP packets.
    let mut binary = Vec::new();
    pgp::ser::Serialize::to_writer(&pubkey, &mut binary)
        .map_err(|e| crate::Error::Rpgp(e.to_string()))?;

    // Base64-encode (no line breaks in the raw encode; we fold manually below).
    let b64 = STANDARD.encode(&binary);

    // Fold the keydata at 76 chars per line.  Per RFC 5322 §2.2.3, continuation
    // lines start with whitespace ("\r\n " here).  The keydata begins on a new
    // continuation line immediately after "keydata=" so that the first line
    // ("addr=...; keydata=") itself stays ≤ 78 chars regardless of addr length.
    let folded_keydata = b64
        .as_bytes()
        .chunks(76)
        .map(|chunk| std::str::from_utf8(chunk).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\r\n ");

    let pe = if prefer_encrypt_mutual {
        "; prefer-encrypt=mutual"
    } else {
        ""
    };
    Ok(format!("addr={addr}{pe}; keydata=\r\n {folded_keydata}"))
}

/// Base64-encode binary data with 76-char line wrapping (RFC 2045 MIME style).
pub(crate) fn b64_wrap(data: &[u8]) -> String {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let encoded = STANDARD.encode(data);
    let mut out = String::new();
    for chunk in encoded.as_bytes().chunks(76) {
        // SAFETY: base64 output is always ASCII.
        out.push_str(std::str::from_utf8(chunk).unwrap_or(""));
        out.push_str("\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbx_managed::keygen;

    /// Generate a key, build the Autocrypt header value, parse the base64 back,
    /// and assert the fingerprint matches the original key.
    #[tokio::test]
    async fn autocrypt_value_round_trip() {
        use base64::{Engine, engine::general_purpose::STANDARD};
        use pgp::composed::Deserializable;
        use pgp::types::KeyDetails;

        let tmp = tempfile::tempdir().unwrap();
        let (key_id, _) = keygen(tmp.path(), "Bob", "bob@example.com", "")
            .await
            .unwrap();

        // Export the public key as armor.
        let src = crate::inbx_managed::InbxManagedSource::new(tmp.path().to_path_buf());
        let armored = src.export_public(&key_id).await.unwrap();

        let value = autocrypt_header_value("bob@example.com", &armored.0, false).unwrap();

        // Extract the keydata= portion (after "keydata="), strip fold whitespace.
        let keydata_raw = value.split("keydata=").nth(1).expect("keydata= present");
        let b64_clean: String = keydata_raw.split("\r\n ").collect::<Vec<_>>().join("");
        let binary = STANDARD.decode(&b64_clean).expect("valid base64");

        let parsed =
            pgp::composed::SignedPublicKey::from_bytes(std::io::BufReader::new(binary.as_slice()))
                .unwrap();
        let parsed_fpr = parsed.primary_key.fingerprint().to_string();
        assert_eq!(parsed_fpr.to_lowercase(), key_id.0.to_lowercase());
    }

    /// Assert no line in the Autocrypt header value exceeds 78 chars.
    #[tokio::test]
    async fn autocrypt_value_folds_long_keydata() {
        let tmp = tempfile::tempdir().unwrap();
        let (key_id, _) = keygen(tmp.path(), "Carol", "carol@example.com", "")
            .await
            .unwrap();

        let src = crate::inbx_managed::InbxManagedSource::new(tmp.path().to_path_buf());
        let armored = src.export_public(&key_id).await.unwrap();

        let value = autocrypt_header_value("carol@example.com", &armored.0, false).unwrap();

        for line in value.split("\r\n") {
            assert!(
                line.len() <= 78,
                "line too long ({} chars): {:?}",
                line.len(),
                line
            );
        }
    }

    /// Generate a key, build an Autocrypt header value, parse it back via
    /// `parse_autocrypt_header`, assert the fingerprint round-trips correctly.
    #[tokio::test]
    async fn parse_autocrypt_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let (key_id, _) = keygen(tmp.path(), "Dave", "dave@example.com", "")
            .await
            .unwrap();
        let src = crate::inbx_managed::InbxManagedSource::new(tmp.path().to_path_buf());
        let armored = src.export_public(&key_id).await.unwrap();

        let header_value = autocrypt_header_value("dave@example.com", &armored.0, false).unwrap();
        let parsed = parse_autocrypt_header(&header_value).unwrap();

        assert_eq!(parsed.addr, "dave@example.com");
        assert_eq!(parsed.fingerprint, key_id.0.to_lowercase());
        assert!(
            parsed
                .keydata_armored
                .contains("BEGIN PGP PUBLIC KEY BLOCK")
        );
    }

    /// Manually fold a header value at 76 chars, verify parse still succeeds.
    #[tokio::test]
    async fn parse_autocrypt_handles_folds() {
        let tmp = tempfile::tempdir().unwrap();
        let (key_id, _) = keygen(tmp.path(), "Eve", "eve@example.com", "")
            .await
            .unwrap();
        let src = crate::inbx_managed::InbxManagedSource::new(tmp.path().to_path_buf());
        let armored = src.export_public(&key_id).await.unwrap();

        // Build unfolded value, then manually re-fold at 76 chars with CRLF + space.
        let value = autocrypt_header_value("eve@example.com", &armored.0, false).unwrap();
        // Replace existing folds with a different fold length to test robustness.
        let unfolded: String = value.replace("\r\n ", "");
        let refolded = unfolded
            .as_bytes()
            .chunks(76)
            .map(|c| std::str::from_utf8(c).unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\r\n ");

        let parsed = parse_autocrypt_header(&refolded).unwrap();
        assert_eq!(parsed.fingerprint, key_id.0.to_lowercase());
    }

    /// `parse_autocrypt_header` must error when `addr=` is absent.
    #[test]
    fn parse_autocrypt_missing_addr_errors() {
        // Build a minimal header value with keydata but no addr.
        use base64::{Engine, engine::general_purpose::STANDARD};
        let fake_keydata = STANDARD.encode(b"not-real-but-just-testing-attr-parse");
        let value = format!("keydata={fake_keydata}");
        let result = parse_autocrypt_header(&value);
        assert!(
            result.is_err(),
            "expected error when addr= is missing, got: {:?}",
            result
        );
    }

    /// Parsing `prefer-encrypt=mutual` yields `AutocryptPreference::Mutual`.
    #[tokio::test]
    async fn parse_prefer_encrypt_mutual() {
        let tmp = tempfile::tempdir().unwrap();
        let (key_id, _) = keygen(tmp.path(), "Frank", "frank@example.com", "")
            .await
            .unwrap();
        let src = crate::inbx_managed::InbxManagedSource::new(tmp.path().to_path_buf());
        let armored = src.export_public(&key_id).await.unwrap();

        // Build header with prefer-encrypt=mutual.
        let value = autocrypt_header_value("frank@example.com", &armored.0, true).unwrap();
        assert!(
            value.contains("prefer-encrypt=mutual"),
            "emitted header should contain prefer-encrypt=mutual: {value}"
        );
        let parsed = parse_autocrypt_header(&value).unwrap();
        assert_eq!(
            parsed.prefer_encrypt,
            AutocryptPreference::Mutual,
            "parsed prefer_encrypt should be Mutual"
        );
    }

    /// Parsing a header without `prefer-encrypt` yields `AutocryptPreference::Nopreference`.
    #[tokio::test]
    async fn parse_prefer_encrypt_absent_is_nopreference() {
        let tmp = tempfile::tempdir().unwrap();
        let (key_id, _) = keygen(tmp.path(), "Grace", "grace@example.com", "")
            .await
            .unwrap();
        let src = crate::inbx_managed::InbxManagedSource::new(tmp.path().to_path_buf());
        let armored = src.export_public(&key_id).await.unwrap();

        // Build header without prefer-encrypt.
        let value = autocrypt_header_value("grace@example.com", &armored.0, false).unwrap();
        assert!(
            !value.contains("prefer-encrypt"),
            "header should not contain prefer-encrypt when disabled"
        );
        let parsed = parse_autocrypt_header(&value).unwrap();
        assert_eq!(
            parsed.prefer_encrypt,
            AutocryptPreference::Nopreference,
            "parsed prefer_encrypt should be Nopreference when attribute absent"
        );
    }

    /// Round-trip: emit with mutual, parse back, verify the value is preserved.
    #[tokio::test]
    async fn prefer_encrypt_mutual_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let (key_id, _) = keygen(tmp.path(), "Heidi", "heidi@example.com", "")
            .await
            .unwrap();
        let src = crate::inbx_managed::InbxManagedSource::new(tmp.path().to_path_buf());
        let armored = src.export_public(&key_id).await.unwrap();

        let value = autocrypt_header_value("heidi@example.com", &armored.0, true).unwrap();
        let parsed = parse_autocrypt_header(&value).unwrap();

        // fingerprint matches
        assert_eq!(parsed.fingerprint, key_id.0.to_lowercase());
        // preference preserved
        assert_eq!(parsed.prefer_encrypt, AutocryptPreference::Mutual);
    }
}
