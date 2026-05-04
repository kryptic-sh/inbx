use serde::{Deserialize, Serialize};

/// Which backend provides PGP key material for an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum KeySourceKind {
    /// Shell out to the system `gpg` binary.  Preserves gpg-agent + smartcard.
    #[default]
    Gnupg,
    /// Pure-Rust crypto via the `pgp` (rpgp) crate; keys stored in inbx data dir.
    InbxManaged,
}

/// Per-account PGP configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PgpConfig {
    /// Which backend to use for this account.
    #[serde(default)]
    pub key_source: KeySourceKind,

    /// Hex fingerprint of the chosen key (40 chars, no spaces).
    /// When `None`, the source picks the first matching key for the account email.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_fingerprint: Option<String>,

    /// For `InbxManaged`: explicit override of where the keypair lives.
    /// When `None`, defaults to `~/.local/share/inbx/<account>/pgp/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_dir: Option<std::path::PathBuf>,

    /// Emit `prefer-encrypt=mutual` in outbound Autocrypt headers per
    /// Autocrypt 1.1 §4.  When both peers advertise `mutual`, the composer
    /// auto-enables encryption on replies.  Defaults to `true`.
    #[serde(default = "default_prefer_encrypt_mutual")]
    pub prefer_encrypt_mutual: bool,
}

fn default_prefer_encrypt_mutual() -> bool {
    true
}

impl Default for PgpConfig {
    fn default() -> Self {
        Self {
            key_source: KeySourceKind::default(),
            key_fingerprint: None,
            managed_dir: None,
            prefer_encrypt_mutual: true,
        }
    }
}

/// Detect a sensible default key source for first-time setup.
/// Returns `Gnupg` if `~/.gnupg/` exists, else `InbxManaged`.
pub fn detect_default() -> KeySourceKind {
    let exists = std::env::var("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".gnupg").exists())
        .unwrap_or(false);
    if exists {
        KeySourceKind::Gnupg
    } else {
        KeySourceKind::InbxManaged
    }
}
