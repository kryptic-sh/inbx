//! GnuPG shell-out backend.
//!
//! Each method invokes the `gpg` binary, relying on gpg-agent for key
//! material, passphrase prompting, and smartcard access.

use std::{io::Write, path::PathBuf};

use crate::{
    ArmoredKey, Ciphertext, KeyId, KeySource, Plaintext, Signature, VerifyResult,
    error::{Error, Result},
};

/// Backend that shells out to the system `gpg` binary.
pub struct GnuPgSource {
    /// Optional explicit homedir (`--homedir`). `None` = use gpg's default.
    pub homedir: Option<PathBuf>,
}

impl GnuPgSource {
    pub fn new() -> Self {
        Self { homedir: None }
    }

    pub fn with_homedir(dir: PathBuf) -> Self {
        Self { homedir: Some(dir) }
    }

    fn gpg_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(ref dir) = self.homedir {
            args.push("--homedir".into());
            args.push(dir.to_string_lossy().into_owned());
        }
        args.push("--batch".into());
        args.push("--yes".into());
        args
    }

    fn run_gpg_sync(&self, extra_args: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>> {
        let gpg = which_gpg()?;

        let mut base = self.gpg_args();
        base.extend(extra_args.iter().map(|s| s.to_string()));

        let mut cmd = std::process::Command::new(&gpg);
        cmd.args(&base);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        if stdin.is_some() {
            cmd.stdin(std::process::Stdio::piped());
        }

        let mut child = cmd.spawn()?;

        if let Some(data) = stdin
            && let Some(mut si) = child.stdin.take()
        {
            si.write_all(data)?;
        }

        let output = child.wait_with_output()?;

        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(Error::GpgFailed(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }
}

impl Default for GnuPgSource {
    fn default() -> Self {
        Self::new()
    }
}

/// Check gpg is on PATH and return its path.
pub fn which_gpg() -> Result<PathBuf> {
    which::which("gpg").map_err(|_| Error::GpgMissing)
}

#[async_trait::async_trait]
impl KeySource for GnuPgSource {
    async fn list_keys(&self) -> Result<Vec<(KeyId, String)>> {
        let out = self.run_gpg_sync(&["--list-keys", "--with-colons"], None)?;
        let text = String::from_utf8_lossy(&out);

        let mut keys: Vec<(KeyId, String)> = Vec::new();
        let mut current_fpr: Option<String> = None;

        for line in text.lines() {
            let fields: Vec<&str> = line.split(':').collect();
            match fields.first().copied() {
                Some("fpr") if fields.len() > 9 => {
                    // field index 9 is the fingerprint
                    current_fpr = Some(fields[9].to_owned());
                }
                Some("uid") => {
                    if let Some(fpr) = &current_fpr {
                        // field index 9 is the uid string
                        let uid = if fields.len() > 9 {
                            fields[9].to_owned()
                        } else {
                            String::new()
                        };
                        keys.push((KeyId(fpr.clone()), uid));
                    }
                }
                _ => {}
            }
        }

        Ok(keys)
    }

    async fn export_public(&self, key: &KeyId) -> Result<ArmoredKey> {
        let out = self.run_gpg_sync(&["--armor", "--export", &key.0], None)?;
        Ok(ArmoredKey(String::from_utf8_lossy(&out).into_owned()))
    }

    async fn sign_detached(&self, key: &KeyId, data: &[u8]) -> Result<Signature> {
        let out = self.run_gpg_sync(
            &[
                "--armor",
                "--detach-sign",
                "--local-user",
                &key.0,
                "--output",
                "-",
            ],
            Some(data),
        )?;
        Ok(Signature(out))
    }

    async fn verify_detached(
        &self,
        signer_pubkey: &ArmoredKey,
        data: &[u8],
        sig: &Signature,
    ) -> Result<VerifyResult> {
        // Import pubkey into a throw-away homedir, then verify.
        let tmp = tempfile::tempdir()?;
        let tmp_path = tmp.path().to_path_buf();

        // Make the tmp homedir readable only by the current user (gpg requirement).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o700))?;
        }

        // Write pubkey to a temp file and import it.
        let pubkey_file = tmp_path.join("pubkey.asc");
        std::fs::write(&pubkey_file, signer_pubkey.0.as_bytes())?;

        let gpg = which_gpg()?;

        // Import key into tmp homedir.
        let import_status = std::process::Command::new(&gpg)
            .args([
                "--homedir",
                &tmp_path.to_string_lossy(),
                "--batch",
                "--yes",
                "--import",
                &pubkey_file.to_string_lossy(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;

        if !import_status.success() {
            return Err(Error::GpgFailed(
                "failed to import pubkey for verify".into(),
            ));
        }

        // Write sig and data to temp files.
        let sig_file = tmp_path.join("sig.asc");
        let data_file = tmp_path.join("data.bin");
        std::fs::write(&sig_file, &sig.0)?;
        std::fs::write(&data_file, data)?;

        let verify_output = std::process::Command::new(&gpg)
            .args([
                "--homedir",
                &tmp_path.to_string_lossy(),
                "--batch",
                "--yes",
                "--status-fd",
                "1",
                "--verify",
                &sig_file.to_string_lossy(),
                &data_file.to_string_lossy(),
            ])
            .output()?;

        let status_text = String::from_utf8_lossy(&verify_output.stdout).into_owned();
        let valid = verify_output.status.success();

        // Parse GOODSIG / VALIDSIG from status output.
        let mut signer_fingerprint: Option<String> = None;
        let mut signer_uid: Option<String> = None;
        let mut created_unix: Option<i64> = None;

        for line in status_text.lines() {
            // [GNUPG:] GOODSIG <keyid> <uid>
            if let Some(rest) = line.strip_prefix("[GNUPG:] GOODSIG ") {
                let mut parts = rest.splitn(2, ' ');
                let _ = parts.next(); // keyid
                signer_uid = parts.next().map(|s| s.to_owned());
            }
            // [GNUPG:] VALIDSIG <fpr> <date> <timestamp> ...
            if let Some(rest) = line.strip_prefix("[GNUPG:] VALIDSIG ") {
                let mut parts = rest.split(' ');
                signer_fingerprint = parts.next().map(|s| s.to_owned());
                let _ = parts.next(); // date string
                created_unix = parts.next().and_then(|s| s.parse().ok());
            }
        }

        if valid {
            Ok(VerifyResult {
                valid: true,
                signer_fingerprint,
                signer_uid,
                created_unix,
            })
        } else {
            Ok(VerifyResult {
                valid: false,
                signer_fingerprint: None,
                signer_uid: None,
                created_unix: None,
            })
        }
    }

    async fn encrypt_to(
        &self,
        recipient_pubkeys: &[ArmoredKey],
        plaintext: &[u8],
    ) -> Result<Ciphertext> {
        let tmp = tempfile::tempdir()?;
        let tmp_path = tmp.path().to_path_buf();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o700))?;
        }

        let gpg = which_gpg()?;

        // Import all recipient pubkeys, collect fingerprints.
        let mut recipient_fprs: Vec<String> = Vec::new();
        for (i, key) in recipient_pubkeys.iter().enumerate() {
            let pubkey_file = tmp_path.join(format!("recipient_{i}.asc"));
            std::fs::write(&pubkey_file, key.0.as_bytes())?;

            let out = std::process::Command::new(&gpg)
                .args([
                    "--homedir",
                    &tmp_path.to_string_lossy(),
                    "--batch",
                    "--yes",
                    "--import",
                    &pubkey_file.to_string_lossy(),
                ])
                .output()?;

            // Parse imported fingerprints from stderr/stdout (best effort).
            let stderr_text = String::from_utf8_lossy(&out.stderr).into_owned();
            // gpg outputs lines like: "gpg: key XXXX: public key "..." imported"
            // We'll just trust that we import and then list.
            let _ = stderr_text;
        }

        // List all keys in tmp homedir to get fingerprints.
        let list_out = std::process::Command::new(&gpg)
            .args([
                "--homedir",
                &tmp_path.to_string_lossy(),
                "--batch",
                "--yes",
                "--list-keys",
                "--with-colons",
            ])
            .output()?;

        let list_text = String::from_utf8_lossy(&list_out.stdout).into_owned();
        for line in list_text.lines() {
            let fields: Vec<&str> = line.split(':').collect();
            if fields.first().copied() == Some("fpr") && fields.len() > 9 {
                recipient_fprs.push(fields[9].to_owned());
            }
        }

        // Trust all imported keys (gpg won't encrypt to untrusted keys without --trust-model).
        let mut encrypt_args = vec![
            "--homedir".to_owned(),
            tmp_path.to_string_lossy().into_owned(),
            "--batch".into(),
            "--yes".into(),
            "--trust-model".into(),
            "always".into(),
            "--armor".into(),
            "--encrypt".into(),
        ];

        for fpr in &recipient_fprs {
            encrypt_args.push("--recipient".into());
            encrypt_args.push(fpr.clone());
        }

        let mut child = std::process::Command::new(&gpg)
            .args(&encrypt_args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        if let Some(mut si) = child.stdin.take() {
            si.write_all(plaintext)?;
        }

        let output = child.wait_with_output()?;
        if output.status.success() {
            Ok(Ciphertext(output.stdout))
        } else {
            Err(Error::GpgFailed(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }

    async fn decrypt(&self, ciphertext: &Ciphertext) -> Result<(Plaintext, VerifyResult)> {
        let out = self.run_gpg_sync(&["--decrypt", "--output", "-"], Some(&ciphertext.0))?;

        // GnuPG decrypt with just `--decrypt` doesn't give us an easy verify result
        // from stdout; a full implementation would parse --status-fd output.
        // For slice 1, return the plaintext with a stub VerifyResult (valid=false, no sig info).
        Ok((
            Plaintext(out),
            VerifyResult {
                valid: false,
                signer_fingerprint: None,
                signer_uid: None,
                created_unix: None,
            },
        ))
    }
}
