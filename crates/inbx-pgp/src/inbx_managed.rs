//! Pure-Rust PGP backend using the `pgp` (rpgp) crate.
//!
//! Keys are stored as ASCII-armored files in a per-account directory:
//!   `<managed_dir>/<fingerprint>.pub.asc`  — public key
//!   `<managed_dir>/<fingerprint>.sec.asc`  — secret key

use std::{
    io::BufReader,
    path::{Path, PathBuf},
};

use pgp::{
    composed::{ArmorOptions, Deserializable, DetachedSignature, Message, MessageBuilder},
    crypto::{hash::HashAlgorithm, sym::SymmetricKeyAlgorithm},
    types::{KeyDetails, Password},
};
use rand::thread_rng;

use crate::{
    ArmoredKey, Ciphertext, KeyId, KeySource, Plaintext, Signature, VerifyResult,
    error::{Error, Result},
};

/// Backend that stores keys in a local directory and uses rpgp for all crypto.
pub struct InbxManagedSource {
    pub managed_dir: PathBuf,
}

impl InbxManagedSource {
    pub fn new(managed_dir: PathBuf) -> Self {
        Self { managed_dir }
    }

    fn sec_path(&self, fingerprint: &str) -> PathBuf {
        self.managed_dir.join(format!("{fingerprint}.sec.asc"))
    }

    /// Load all secret keys found in managed_dir (*.sec.asc).
    fn all_secret_keys(&self) -> Result<Vec<(String, pgp::composed::SignedSecretKey)>> {
        let mut keys = Vec::new();
        let entries = match std::fs::read_dir(&self.managed_dir) {
            Ok(e) => e,
            Err(_) => return Ok(keys),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            if !name.ends_with(".sec.asc") {
                continue;
            }

            match pgp::composed::SignedSecretKey::from_armor_file(&path) {
                Ok((key, _)) => {
                    let fpr = key.primary_key.fingerprint().to_string();
                    keys.push((fpr, key));
                }
                Err(e) => {
                    tracing::warn!("failed to parse secret key {}: {e}", path.display());
                }
            }
        }
        Ok(keys)
    }

    fn load_secret_key(&self, key: &KeyId) -> Result<pgp::composed::SignedSecretKey> {
        let path = self.sec_path(&key.0);
        if path.exists() {
            let (k, _) = pgp::composed::SignedSecretKey::from_armor_file(&path)
                .map_err(|e| Error::Rpgp(e.to_string()))?;
            return Ok(k);
        }
        // Try searching by fingerprint substring.
        let all = self.all_secret_keys()?;
        for (fpr, k) in all {
            if fpr.to_lowercase().contains(&key.0.to_lowercase()) {
                return Ok(k);
            }
        }
        Err(Error::KeyNotFound(key.0.clone()))
    }

    fn load_passphrase(&self, fingerprint: &str) -> Result<Password> {
        let entry = keyring::Entry::new("inbx-pgp", fingerprint)?;
        match entry.get_password() {
            Ok(pw) => Ok(pw.into()),
            Err(keyring::Error::NoEntry) => Err(Error::PassphraseMissing {
                fingerprint: fingerprint.to_owned(),
            }),
            Err(e) => Err(Error::Keyring(e)),
        }
    }
}

#[async_trait::async_trait]
impl KeySource for InbxManagedSource {
    async fn list_keys(&self) -> Result<Vec<(KeyId, String)>> {
        let mut result = Vec::new();
        let all = self.all_secret_keys()?;
        for (fpr, key) in all {
            let uid = key
                .details
                .users
                .first()
                .and_then(|u| u.id.as_str())
                .map(|s| s.to_owned())
                .unwrap_or_default();
            result.push((KeyId(fpr), uid));
        }
        Ok(result)
    }

    async fn export_public(&self, key: &KeyId) -> Result<ArmoredKey> {
        // Try loading from a pre-exported .pub.asc file first.
        let pub_path = self.managed_dir.join(format!("{}.pub.asc", key.0));
        if pub_path.exists() {
            let raw = std::fs::read_to_string(&pub_path)?;
            return Ok(ArmoredKey(raw));
        }
        // Derive from secret key.
        let skey = self.load_secret_key(key)?;
        let pubkey = skey.to_public_key();
        let armor = pubkey
            .to_armored_string(ArmorOptions::default())
            .map_err(|e| Error::Rpgp(e.to_string()))?;
        Ok(ArmoredKey(armor))
    }

    async fn sign_detached(&self, key: &KeyId, data: &[u8]) -> Result<Signature> {
        let skey = self.load_secret_key(key)?;
        let fpr = skey.primary_key.fingerprint().to_string();
        let pw = self.load_passphrase(&fpr).unwrap_or(Password::empty());

        let mut rng = thread_rng();
        let sig = DetachedSignature::sign_binary_data(
            &mut rng,
            &skey.primary_key,
            &pw,
            HashAlgorithm::Sha256,
            data,
        )
        .map_err(|e| Error::Rpgp(e.to_string()))?;

        let sig_bytes = sig
            .to_armored_bytes(ArmorOptions::default())
            .map_err(|e| Error::Rpgp(e.to_string()))?;

        Ok(Signature(sig_bytes))
    }

    async fn verify_detached(
        &self,
        signer_pubkey: &ArmoredKey,
        data: &[u8],
        sig: &Signature,
    ) -> Result<VerifyResult> {
        let (pubkey, _) = pgp::composed::SignedPublicKey::from_armor_single(BufReader::new(
            signer_pubkey.0.as_bytes(),
        ))
        .map_err(|e| Error::Rpgp(e.to_string()))?;

        let (det_sig, _) = DetachedSignature::from_armor_single(BufReader::new(sig.0.as_slice()))
            .map_err(|e| Error::Rpgp(e.to_string()))?;

        match det_sig.verify(&pubkey.primary_key, data) {
            Ok(()) => {
                let fpr = pubkey.primary_key.fingerprint().to_string();
                let uid = pubkey
                    .details
                    .users
                    .first()
                    .and_then(|u| u.id.as_str())
                    .map(|s| s.to_owned());
                // Extract creation time from the signature's subpackets.
                let created = det_sig.signature.config().and_then(|c| {
                    c.hashed_subpackets.iter().find_map(|sp| {
                        if let pgp::packet::SubpacketData::SignatureCreationTime(t) = &sp.data {
                            Some(t.as_secs() as i64)
                        } else {
                            None
                        }
                    })
                });
                Ok(VerifyResult {
                    valid: true,
                    signer_fingerprint: Some(fpr),
                    signer_uid: uid,
                    created_unix: created,
                })
            }
            Err(_) => Ok(VerifyResult {
                valid: false,
                signer_fingerprint: None,
                signer_uid: None,
                created_unix: None,
            }),
        }
    }

    async fn encrypt_to(
        &self,
        recipient_pubkeys: &[ArmoredKey],
        plaintext: &[u8],
    ) -> Result<Ciphertext> {
        let mut rng = thread_rng();
        // `from_bytes` requires owned data (Into<Bytes>).
        let plaintext_owned: Vec<u8> = plaintext.to_vec();

        let mut builder = MessageBuilder::from_bytes("", plaintext_owned)
            .seipd_v1(&mut rng, SymmetricKeyAlgorithm::AES256);

        for armored in recipient_pubkeys {
            let (pubkey, _) = pgp::composed::SignedPublicKey::from_armor_single(BufReader::new(
                armored.0.as_bytes(),
            ))
            .map_err(|e| Error::Rpgp(e.to_string()))?;

            // Look for an encryption-capable subkey first; fall back to the primary key.
            let enc_subkey = pubkey.public_subkeys.iter().find(|sk| {
                sk.signatures.iter().any(|sig| {
                    let kf = sig.key_flags();
                    kf.encrypt_comms() || kf.encrypt_storage()
                })
            });

            if let Some(subkey) = enc_subkey {
                builder
                    .encrypt_to_key(&mut rng, &subkey.key)
                    .map_err(|e| Error::Rpgp(e.to_string()))?;
            } else {
                builder
                    .encrypt_to_key(&mut rng, &pubkey)
                    .map_err(|e| Error::Rpgp(e.to_string()))?;
            }
        }

        let ciphertext = builder
            .to_vec(&mut rng)
            .map_err(|e| Error::Rpgp(e.to_string()))?;

        Ok(Ciphertext(ciphertext))
    }

    async fn decrypt(&self, ciphertext: &Ciphertext) -> Result<(Plaintext, VerifyResult)> {
        let all_keys = self.all_secret_keys()?;
        if all_keys.is_empty() {
            return Err(Error::KeyNotFound("no managed keys found".into()));
        }

        // Try each secret key until one decrypts successfully.
        for (fpr, skey) in &all_keys {
            let pw = self.load_passphrase(fpr).unwrap_or(Password::empty());
            // Re-parse from bytes each attempt (Message is not Clone).
            let message = Message::from_bytes(ciphertext.0.as_slice())
                .map_err(|e| Error::Rpgp(e.to_string()))?;
            match message.decrypt(&pw, skey) {
                Ok(mut decrypted) => {
                    let plain = decrypted.as_data_vec().map_err(Error::Io)?;
                    return Ok((
                        Plaintext(plain),
                        VerifyResult {
                            valid: false,
                            signer_fingerprint: None,
                            signer_uid: None,
                            created_unix: None,
                        },
                    ));
                }
                Err(_) => continue,
            }
        }

        Err(Error::GpgFailed(
            "no matching key found for decryption".into(),
        ))
    }
}

/// Generate an Ed25519 primary key + X25519 encryption subkey in `managed_dir`,
/// store the passphrase in the keyring (if non-empty), and return
/// `(KeyId(fingerprint), path_to_sec_key)`.
pub async fn keygen(
    managed_dir: &Path,
    name: &str,
    email: &str,
    passphrase: &str,
) -> Result<(KeyId, PathBuf)> {
    use pgp::composed::{EncryptionCaps, KeyType, SecretKeyParamsBuilder, SubkeyParamsBuilder};
    use smallvec::smallvec;

    std::fs::create_dir_all(managed_dir)?;

    let uid = format!("{name} <{email}>");
    let mut rng = thread_rng();

    let key_params = SecretKeyParamsBuilder::default()
        .key_type(KeyType::Ed25519)
        .can_sign(true)
        .can_certify(true)
        .primary_user_id(uid)
        .preferred_symmetric_algorithms(smallvec![SymmetricKeyAlgorithm::AES256])
        .preferred_hash_algorithms(smallvec![HashAlgorithm::Sha256])
        .subkey(
            SubkeyParamsBuilder::default()
                .key_type(KeyType::X25519)
                .can_encrypt(EncryptionCaps::All)
                .build()
                .map_err(|e| Error::Rpgp(e.to_string()))?,
        )
        .build()
        .map_err(|e| Error::Rpgp(e.to_string()))?;

    let secret_key = key_params
        .generate(&mut rng)
        .map_err(|e| Error::Rpgp(e.to_string()))?;

    let fingerprint = secret_key.primary_key.fingerprint().to_string();

    // Write public key.
    let pub_path = managed_dir.join(format!("{fingerprint}.pub.asc"));
    let pub_key = secret_key.to_public_key();
    let pub_armor = pub_key
        .to_armored_string(ArmorOptions::default())
        .map_err(|e| Error::Rpgp(e.to_string()))?;
    std::fs::write(&pub_path, &pub_armor)?;

    // Write secret key (unencrypted for now; passphrase stored in keyring separately).
    let sec_path = managed_dir.join(format!("{fingerprint}.sec.asc"));
    let sec_armor = secret_key
        .to_armored_string(ArmorOptions::default())
        .map_err(|e| Error::Rpgp(e.to_string()))?;
    std::fs::write(&sec_path, &sec_armor)?;

    // Store passphrase in keyring if provided.
    if !passphrase.is_empty() {
        let entry = keyring::Entry::new("inbx-pgp", &fingerprint)?;
        entry.set_password(passphrase)?;
    }

    Ok((KeyId(fingerprint), sec_path))
}
