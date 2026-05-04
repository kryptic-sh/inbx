use inbx_pgp::{
    KeySource,
    inbx_managed::{InbxManagedSource, keygen},
    mime::{OuterHeaders, encrypt_pgp_mime, sign_pgp_mime},
};
use inbx_render::{RemotePolicy, render_message, render_message_with_pgp};

fn test_headers() -> OuterHeaders {
    OuterHeaders {
        from: "Alice <alice@example.com>".into(),
        to: vec!["Bob <bob@example.com>".into()],
        cc: vec![],
        bcc: vec![],
        subject: "Test".into(),
        message_id: None,
        in_reply_to: None,
        references: vec![],
        date: Some("Thu, 01 Jan 1970 00:00:00 +0000".into()),
        autocrypt: None,
    }
}

/// Plain message with no KeySource behaves like the legacy render_message.
#[test]
fn render_passthrough_no_pgp() {
    let raw = b"From: a@x\r\nTo: b@y\r\nSubject: hi\r\n\r\nhello world\r\n";
    let r = render_message(raw, RemotePolicy::Block).unwrap();
    assert_eq!(r.plain.trim(), "hello world");
    assert!(r.html.is_none());
    assert!(r.pgp_verify.is_none());
}

/// Build a multipart/signed message, render with a KeySource, assert verified.
#[tokio::test]
async fn render_signed_message_verifies() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let (key_id, _) = keygen(&dir, "Alice", "alice@example.com", "")
        .await
        .unwrap();
    let src = InbxManagedSource::new(dir.clone());

    let inner = b"From: alice@example.com\r\nTo: bob@example.com\r\nSubject: signed\r\n\r\nHello signed world\r\n";
    let signed_msg = sign_pgp_mime(&src, &key_id, inner, &test_headers())
        .await
        .unwrap();

    let r = render_message_with_pgp(&signed_msg, RemotePolicy::Block, Some(&src))
        .await
        .unwrap();

    let pgp = r.pgp_verify.expect("pgp_verify should be populated");
    assert!(
        pgp.verified,
        "signature should verify: error={:?}",
        pgp.error
    );
    assert!(pgp.signer_fingerprint.is_some());
    assert!(pgp.error.is_none());
}

/// Build a multipart/encrypted message, render with a KeySource, assert decrypted body.
#[tokio::test]
async fn render_encrypted_message_decrypts() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let (key_id, _) = keygen(&dir, "Bob", "bob@example.com", "").await.unwrap();
    let src = InbxManagedSource::new(dir.clone());
    let pub_key = src.export_public(&key_id).await.unwrap();

    let inner = b"From: alice@example.com\r\nSubject: secret\r\n\r\nSecret decrypted body here\r\n";
    let enc_msg = encrypt_pgp_mime(&src, None, &[pub_key], inner, &test_headers())
        .await
        .unwrap();

    let r = render_message_with_pgp(&enc_msg, RemotePolicy::Block, Some(&src))
        .await
        .unwrap();

    let pgp = r.pgp_verify.expect("pgp_verify should be populated");
    assert!(
        pgp.error.is_none(),
        "decrypt should succeed, error={:?}",
        pgp.error
    );
    assert!(pgp.decrypted_body.is_some(), "decrypted_body should be set");

    // Rendered plain should contain the decrypted content.
    assert!(
        r.plain.contains("Secret decrypted body here"),
        "rendered plain should contain decrypted text, got: {:?}",
        r.plain
    );
}
