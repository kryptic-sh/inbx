use inbx_pgp::{
    ArmoredKey, KeySource, PubkeyLookup, Result as PgpResult,
    inbx_managed::{InbxManagedSource, keygen},
    mime::{OuterHeaders, encrypt_pgp_mime, sign_pgp_mime},
};
use inbx_render::{RemotePolicy, render_message, render_message_with_pgp};

/// In-test mock that returns a fixed key for one email, None for everything else.
struct MockLookup {
    email: String,
    key: ArmoredKey,
}

#[async_trait::async_trait]
impl PubkeyLookup for MockLookup {
    async fn lookup(&self, email: &str) -> PgpResult<Option<ArmoredKey>> {
        if email.eq_ignore_ascii_case(&self.email) {
            Ok(Some(self.key.clone()))
        } else {
            Ok(None)
        }
    }
}

/// Mock that returns None for every email (simulates missing stored key).
struct EmptyLookup;

#[async_trait::async_trait]
impl PubkeyLookup for EmptyLookup {
    async fn lookup(&self, _email: &str) -> PgpResult<Option<ArmoredKey>> {
        Ok(None)
    }
}

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

    let r = render_message_with_pgp(&signed_msg, RemotePolicy::Block, Some(&src), None)
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

    let r = render_message_with_pgp(&enc_msg, RemotePolicy::Block, Some(&src), None)
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

/// Build a signed message from key A; render with a MockLookup returning A's pubkey
/// for the From address; assert verified == true and error is None.
#[tokio::test]
async fn verify_uses_sender_key_from_lookup() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let (key_id, _) = keygen(&dir, "Alice", "alice@example.com", "")
        .await
        .unwrap();
    let src = InbxManagedSource::new(dir.clone());
    let pub_key = src.export_public(&key_id).await.unwrap();

    let inner = b"From: alice@example.com\r\nTo: bob@example.com\r\nSubject: sender-key-test\r\n\r\nHello from Alice\r\n";
    let signed_msg = sign_pgp_mime(&src, &key_id, inner, &test_headers())
        .await
        .unwrap();

    let mock = MockLookup {
        email: "alice@example.com".into(),
        key: pub_key,
    };

    let r = render_message_with_pgp(&signed_msg, RemotePolicy::Block, Some(&src), Some(&mock))
        .await
        .unwrap();

    let pgp = r.pgp_verify.expect("pgp_verify populated");
    assert!(
        pgp.verified,
        "should verify with sender key; error={:?}",
        pgp.error
    );
    assert!(
        pgp.error.is_none(),
        "no fallback error expected; got: {:?}",
        pgp.error
    );
}

/// Render with a lookup that returns None; assert pgp_verify.error mentions fallback.
#[tokio::test]
async fn verify_falls_back_when_no_stored_pubkey() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let (key_id, _) = keygen(&dir, "Alice", "alice@example.com", "")
        .await
        .unwrap();
    let src = InbxManagedSource::new(dir.clone());

    let inner = b"From: alice@example.com\r\nTo: bob@example.com\r\nSubject: fallback-test\r\n\r\nHello fallback\r\n";
    let signed_msg = sign_pgp_mime(&src, &key_id, inner, &test_headers())
        .await
        .unwrap();

    let r = render_message_with_pgp(
        &signed_msg,
        RemotePolicy::Block,
        Some(&src),
        Some(&EmptyLookup),
    )
    .await
    .unwrap();

    let pgp = r.pgp_verify.expect("pgp_verify populated");
    let err = pgp.error.expect("should have a fallback error message");
    assert!(
        err.contains("fallback"),
        "error should mention fallback; got: {err:?}"
    );
}
