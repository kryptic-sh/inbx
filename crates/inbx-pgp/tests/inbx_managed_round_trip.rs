use inbx_pgp::{
    KeySource,
    inbx_managed::{InbxManagedSource, keygen},
};

#[tokio::test]
async fn sign_verify_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();

    // Generate a key (empty passphrase so keyring isn't needed).
    let (key_id, _sec_path) = keygen(&dir, "Test User", "test@example.com", "")
        .await
        .expect("keygen failed");

    let src = InbxManagedSource::new(dir.clone());

    // Export the public key.
    let pub_key = src.export_public(&key_id).await.expect("export_public");
    assert!(!pub_key.0.is_empty());

    // Sign some data.
    let data = b"Hello, inbx PGP!";
    let sig = src
        .sign_detached(&key_id, data)
        .await
        .expect("sign_detached");
    assert!(!sig.0.is_empty());

    // Verify with the exported public key.
    let result = src
        .verify_detached(&pub_key, data, &sig)
        .await
        .expect("verify_detached");
    assert!(result.valid, "signature should be valid");
    assert!(result.signer_fingerprint.is_some());

    // Tampered data should NOT verify.
    let result_bad = src
        .verify_detached(&pub_key, b"tampered data", &sig)
        .await
        .expect("verify_detached bad data");
    assert!(
        !result_bad.valid,
        "signature over tampered data should be invalid"
    );
}

#[tokio::test]
async fn encrypt_decrypt_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();

    let (key_id, _) = keygen(&dir, "Alice", "alice@example.com", "")
        .await
        .expect("keygen");

    let src = InbxManagedSource::new(dir.clone());
    let pub_key = src.export_public(&key_id).await.expect("export_public");

    let plaintext = b"Secret message for Alice";
    let ciphertext = src
        .encrypt_to(&[pub_key], plaintext)
        .await
        .expect("encrypt_to");

    assert!(!ciphertext.0.is_empty());

    let (decrypted, _verify) = src.decrypt(&ciphertext).await.expect("decrypt");
    assert_eq!(
        decrypted.0, plaintext,
        "decrypted content should match original"
    );
}

#[tokio::test]
async fn list_keys_returns_generated() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();

    let (key_id, _) = keygen(&dir, "Bob", "bob@example.com", "")
        .await
        .expect("keygen");

    let src = InbxManagedSource::new(dir.clone());
    let keys = src.list_keys().await.expect("list_keys");

    assert!(!keys.is_empty(), "should find at least one key");
    assert!(
        keys.iter().any(|(k, _)| k.0 == key_id.0),
        "generated key should appear in list"
    );
}
