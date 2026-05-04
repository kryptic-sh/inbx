//! Web Key Directory (WKD) discovery per draft-koch-openpgp-webkey-service.
//!
//! Two URL forms are tried in order:
//!  1. **Advanced**: `https://openpgpkey.<domain>/.well-known/openpgpkey/<domain>/hu/<encoded>?l=<local>`
//!  2. **Direct**:   `https://<domain>/.well-known/openpgpkey/hu/<encoded>?l=<local>`
//!
//! The encoded part is the z-base-32 of the SHA-1 hash of the lowercased local part.

use std::time::Duration;

use pgp::composed::Deserializable as _;
use pgp::types::KeyDetails as _;
use sha1::{Digest, Sha1};

use crate::{ArmoredKey, Error, Result};

/// A key discovered via WKD.
#[derive(Debug, Clone)]
pub struct WkdKey {
    pub email: String,
    pub fingerprint: String,
    pub armored: ArmoredKey,
}

// ── URL derivation ─────────────────────────────────────────────────────────────

/// Encode `bytes` using the z-base-32 alphabet (RFC 6189).
fn zbase32_encode(bytes: &[u8]) -> String {
    zbase32::encode_full_bytes(bytes)
}

/// Derive the WKD hash string for a local-part.
///
/// Lowercases the local part, SHA-1s it, z-base-32 encodes the 20-byte digest.
pub fn wkd_hash(local: &str) -> String {
    let lowered = local.to_lowercase();
    let digest = Sha1::digest(lowered.as_bytes());
    zbase32_encode(&digest)
}

/// Build the advanced WKD URL.
///
/// `https://openpgpkey.<domain>/.well-known/openpgpkey/<domain>/hu/<hash>?l=<local>`
pub fn advanced_url(domain: &str, local: &str) -> String {
    let hash = wkd_hash(local);
    let encoded_local = percent_encode(local);
    format!(
        "https://openpgpkey.{domain}/.well-known/openpgpkey/{domain}/hu/{hash}?l={encoded_local}"
    )
}

/// Build the direct WKD URL.
///
/// `https://<domain>/.well-known/openpgpkey/hu/<hash>?l=<local>`
pub fn direct_url(domain: &str, local: &str) -> String {
    let hash = wkd_hash(local);
    let encoded_local = percent_encode(local);
    format!("https://{domain}/.well-known/openpgpkey/hu/{hash}?l={encoded_local}")
}

/// Percent-encode a string per RFC 3986 (unreserved chars pass through).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            // Unreserved: ALPHA / DIGIT / "-" / "." / "_" / "~"
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            other => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{:02X}", other);
            }
        }
    }
    out
}

// ── HTTP fetch + parse ─────────────────────────────────────────────────────────

/// Parse raw OpenPGP binary bytes into a `WkdKey`.
fn parse_key_bytes(email: &str, bytes: &[u8]) -> Result<WkdKey> {
    let key = pgp::composed::SignedPublicKey::from_bytes(std::io::BufReader::new(bytes))
        .map_err(|e| Error::Rpgp(format!("WKD key parse: {e}")))?;

    let fingerprint = key.primary_key.fingerprint().to_string().to_lowercase();

    let armored_str = key
        .to_armored_string(None.into())
        .map_err(|e| Error::Rpgp(format!("WKD re-armor: {e}")))?;

    Ok(WkdKey {
        email: email.to_string(),
        fingerprint,
        armored: ArmoredKey(armored_str),
    })
}

/// Try a single WKD URL.  Returns:
///  - `Ok(Some(key))` on HTTP 200 + valid key bytes.
///  - `Ok(None)` on any non-200, connection error, or timeout.
///  - `Err(_)` only on HTTP 200 with an unparseable body (misconfigured server).
async fn try_url(client: &reqwest::Client, url: &str, email: &str) -> Result<Option<WkdKey>> {
    let resp = match client
        .get(url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("WKD fetch {url}: {e}");
            return Ok(None);
        }
    };

    if resp.status() != reqwest::StatusCode::OK {
        tracing::debug!("WKD fetch {url}: HTTP {}", resp.status());
        return Ok(None);
    }

    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            tracing::debug!("WKD body {url}: {e}");
            return Ok(None);
        }
    };

    // Got 200 — body MUST be a valid key.
    parse_key_bytes(email, &bytes).map(Some)
}

// ── Public API ─────────────────────────────────────────────────────────────────

/// Look up `email` via WKD.
///
/// Tries the advanced URL first, falls back to direct on any error.
/// Returns `Ok(None)` if both fail — the caller treats that as "no key published".
pub async fn lookup(email: &str) -> Result<Option<WkdKey>> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| Error::Rpgp(format!("WKD client build: {e}")))?;
    lookup_with_client(&client, email).await
}

/// Look up via a specific [`reqwest::Client`] (for proxy threading).
pub async fn lookup_with_client(client: &reqwest::Client, email: &str) -> Result<Option<WkdKey>> {
    // Split local@domain.
    let (local, domain) = match email.split_once('@') {
        Some(parts) => parts,
        None => {
            tracing::debug!("WKD: no '@' in {email:?}, skipping");
            return Ok(None);
        }
    };

    // Advanced form first.
    let adv = advanced_url(domain, local);
    if let Some(key) = try_url(client, &adv, email).await? {
        return Ok(Some(key));
    }

    // Fall back to direct form.
    let dir = direct_url(domain, local);
    try_url(client, &dir, email).await
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Per WKD spec §3.1 example: `Joe.Doe@example.org`
    /// local lowercased = `joe.doe`
    /// SHA-1 → known bytes → z-base-32 = `iy9q119eutrkn8s1mk4r39qejnbu3n5q`
    #[test]
    fn derives_advanced_url_correctly() {
        let email = "Joe.Doe@example.org";
        let (local, domain) = email.split_once('@').unwrap();
        let hash = wkd_hash(local);
        assert_eq!(
            hash, "iy9q119eutrkn8s1mk4r39qejnbu3n5q",
            "z-base-32 hash mismatch for Joe.Doe@example.org"
        );
        let url = advanced_url(domain, local);
        assert_eq!(
            url,
            "https://openpgpkey.example.org/.well-known/openpgpkey/example.org/hu/iy9q119eutrkn8s1mk4r39qejnbu3n5q?l=Joe.Doe"
        );
    }

    #[test]
    fn derives_direct_url_correctly() {
        let email = "Joe.Doe@example.org";
        let (local, domain) = email.split_once('@').unwrap();
        let url = direct_url(domain, local);
        assert_eq!(
            url,
            "https://example.org/.well-known/openpgpkey/hu/iy9q119eutrkn8s1mk4r39qejnbu3n5q?l=Joe.Doe"
        );
    }

    #[tokio::test]
    async fn lookup_garbage_email_returns_none() {
        // No '@' — both URL builds are skipped; must return Ok(None).
        let result = lookup("notanemail").await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn lookup_unreachable_domain_returns_none() {
        if std::env::var("INBX_NETWORK_TESTS").is_err() {
            println!("skipped (set INBX_NETWORK_TESTS=1)");
            return;
        }
        let result = lookup("test@nonexistent.invalid").await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }
}
