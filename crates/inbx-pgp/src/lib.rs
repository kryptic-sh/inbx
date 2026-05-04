//! PGP/OpenPGP support for inbx.
//!
//! Two key sources, picked per account:
//!  - `gnupg`: shells out to `gpg` to preserve gpg-agent + smartcard
//!  - `inbx-managed`: pure-Rust crypto via the `pgp` (rpgp) crate

pub mod config;
pub mod error;
pub mod gnupg;
pub mod inbx_managed;
pub mod mime;
pub mod wkd;

pub use config::{KeySourceKind, PgpConfig};
pub use error::{Error, Result};

/// Hex fingerprint or short key id.
#[derive(Debug, Clone)]
pub struct KeyId(pub String);

/// ASCII-armored public OR secret key.
#[derive(Debug, Clone)]
pub struct ArmoredKey(pub String);

/// Detached signature bytes (armored or binary).
#[derive(Debug, Clone)]
pub struct Signature(pub Vec<u8>);

/// OpenPGP encrypted blob (binary or armor).
#[derive(Debug, Clone)]
pub struct Ciphertext(pub Vec<u8>);

/// Decrypted plaintext bytes.
#[derive(Debug, Clone)]
pub struct Plaintext(pub Vec<u8>);

/// Result of a signature verification.
#[derive(Debug, Clone)]
pub struct VerifyResult {
    pub valid: bool,
    pub signer_fingerprint: Option<String>,
    /// "Alice <alice@example.com>"
    pub signer_uid: Option<String>,
    pub created_unix: Option<i64>,
}

/// Trait for looking up a sender's stored public key by email address.
/// Implemented by `inbx-contacts::ContactsStore`; `inbx-render` depends only on
/// this trait so it does not need a hard dep on `inbx-contacts` (sqlite).
#[async_trait::async_trait]
pub trait PubkeyLookup: Send + Sync {
    /// Return the ASCII-armored public key stored for `email`, or `None`.
    async fn lookup(&self, email: &str) -> Result<Option<ArmoredKey>>;
}

/// Operations that any PGP key source must support.
#[async_trait::async_trait]
pub trait KeySource: Send + Sync {
    async fn list_keys(&self) -> Result<Vec<(KeyId, String /* uid */)>>;
    async fn export_public(&self, key: &KeyId) -> Result<ArmoredKey>;
    async fn sign_detached(&self, key: &KeyId, data: &[u8]) -> Result<Signature>;
    async fn verify_detached(
        &self,
        signer_pubkey: &ArmoredKey,
        data: &[u8],
        sig: &Signature,
    ) -> Result<VerifyResult>;
    async fn encrypt_to(
        &self,
        recipient_pubkeys: &[ArmoredKey],
        plaintext: &[u8],
    ) -> Result<Ciphertext>;
    async fn decrypt(&self, ciphertext: &Ciphertext) -> Result<(Plaintext, VerifyResult)>;
}

/// Construct the right [`KeySource`] impl for an account's PGP config.
pub fn key_source_for(cfg: &PgpConfig) -> Result<Box<dyn KeySource>> {
    match cfg.key_source {
        KeySourceKind::Gnupg => {
            // Fail fast if gpg is not available.
            gnupg::which_gpg()?;
            Ok(Box::new(gnupg::GnuPgSource::new()))
        }
        KeySourceKind::InbxManaged => {
            let dir = cfg.managed_dir.clone().unwrap_or_else(|| {
                // Default: ~/.local/share/inbx/pgp/
                dirs_data_dir()
            });
            Ok(Box::new(inbx_managed::InbxManagedSource::new(dir)))
        }
    }
}

fn dirs_data_dir() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(|h| {
            std::path::PathBuf::from(h)
                .join(".local")
                .join("share")
                .join("inbx")
                .join("pgp")
        })
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/inbx-pgp"))
}
