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
}

/// Base64-encode binary data with 76-char line wrapping (RFC 2045 MIME style).
fn b64_wrap(data: &[u8]) -> String {
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
