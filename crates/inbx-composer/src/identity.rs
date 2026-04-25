use serde::{Deserialize, Serialize};

/// One sender identity. An account may have several (aliases / send-as);
/// the composer takes one at construction time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub email: String,
    #[serde(default)]
    pub name: Option<String>,
    /// Plaintext signature appended to outgoing messages. The leading
    /// "-- \n" separator is added if missing.
    #[serde(default)]
    pub signature: Option<String>,
}

impl Identity {
    pub fn from_account(account: &inbx_config::Account) -> Self {
        Self {
            email: account.email.clone(),
            name: None,
            signature: None,
        }
    }

    /// Render the signature with a leading "-- \n" separator (RFC 3676
    /// recommends "-- " followed by LF). Returns None if no signature.
    pub fn signature_block(&self) -> Option<String> {
        let raw = self.signature.as_deref()?;
        if raw.starts_with("-- \n") || raw.starts_with("--\n") {
            Some(raw.to_string())
        } else {
            Some(format!("-- \n{raw}"))
        }
    }
}
