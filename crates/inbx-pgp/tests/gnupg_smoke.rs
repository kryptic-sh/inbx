//! GnuPG smoke test — skipped automatically if `gpg` is not on PATH.
//!
//! Uses a throw-away `--homedir` so it never touches the user's real keyring.

#[cfg(unix)]
mod unix_smoke {
    use std::io::Write;

    use inbx_pgp::{
        KeySource,
        gnupg::{GnuPgSource, which_gpg},
    };

    fn gpg_available() -> bool {
        which_gpg().is_ok()
    }

    #[tokio::test]
    async fn sign_verify_round_trip() {
        if !gpg_available() {
            println!("gpg not found on PATH — skipped");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let homedir = tmp.path().to_path_buf();

        // Set homedir permissions so gpg doesn't complain.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&homedir, std::fs::Permissions::from_mode(0o700)).unwrap();

        let gpg = which_gpg().unwrap();

        // Generate a batch key in the temp homedir.
        // gpg 2.2+ batch format for Ed25519: Key-Type is "eddsa", Key-Curve is "ed25519"
        let batch_spec = r#"%no-protection
Key-Type: eddsa
Key-Curve: ed25519
Key-Usage: sign
Name-Real: Smoke Test
Name-Email: smoke@test.inbx
Expire-Date: 0
%commit
"#;

        let mut child = std::process::Command::new(&gpg)
            .args([
                "--homedir",
                &homedir.to_string_lossy(),
                "--batch",
                "--yes",
                "--generate-key",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn gpg --generate-key");

        child
            .stdin
            .take()
            .unwrap()
            .write_all(batch_spec.as_bytes())
            .unwrap();
        let status = child.wait().unwrap();
        assert!(status.success(), "gpg --generate-key failed: {}", status);

        let src = GnuPgSource::with_homedir(homedir.clone());

        // List keys — we should see the generated key.
        let keys = src.list_keys().await.expect("list_keys");
        assert!(!keys.is_empty(), "expected at least one key");

        let (key_id, _uid) = &keys[0];

        // Export public key.
        let pub_key = src.export_public(key_id).await.expect("export_public");
        assert!(!pub_key.0.is_empty());

        // Sign.
        let data = b"gpg smoke test data";
        let sig = src
            .sign_detached(key_id, data)
            .await
            .expect("sign_detached");
        assert!(!sig.0.is_empty(), "signature should not be empty");

        // Verify.
        let result = src
            .verify_detached(&pub_key, data, &sig)
            .await
            .expect("verify_detached");
        assert!(result.valid, "signature should verify as valid");
    }
}
