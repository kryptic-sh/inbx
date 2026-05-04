/// PGP error type for inbx-pgp.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("gpg binary not found on PATH")]
    GpgMissing,

    #[error("gpg failed: {0}")]
    GpgFailed(String),

    #[error("rpgp: {0}")]
    Rpgp(String),

    #[error("keyring: {0}")]
    Keyring(#[from] keyring::Error),

    #[error("passphrase missing for key {fingerprint}")]
    PassphraseMissing { fingerprint: String },

    #[error("key not found: {0}")]
    KeyNotFound(String),

    #[error("verification failed")]
    VerifyFailed,
}

pub type Result<T> = std::result::Result<T, Error>;
