use inbx_pgp::{
    KeySource,
    inbx_managed::{InbxManagedSource, keygen},
    mime::{OuterHeaders, autocrypt_header_value},
};
use inbx_render::{RemotePolicy, render_message_with_pgp};

/// Build a message with an Autocrypt: header (using mail-builder + autocrypt_header_value),
/// render it, assert Rendered.autocrypt is Some and addr matches.
#[tokio::test]
async fn render_surfaces_autocrypt_header() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let (key_id, _) = keygen(&dir, "Alice", "alice@example.com", "")
        .await
        .unwrap();
    let src = InbxManagedSource::new(dir.clone());
    let armored = src.export_public(&key_id).await.unwrap();

    // Build the Autocrypt header value.
    let ac_value = autocrypt_header_value("alice@example.com", &armored.0).unwrap();

    // Assemble a minimal RFC 5322 message with the Autocrypt header.
    // We embed the (possibly folded) value; use OuterHeaders to get a real message.
    let inner = b"From: alice@example.com\r\nTo: bob@example.com\r\nSubject: autocrypt-test\r\n\r\nHello autocrypt world\r\n";

    let outer = OuterHeaders {
        from: "alice@example.com".into(),
        to: vec!["bob@example.com".into()],
        cc: vec![],
        bcc: vec![],
        subject: "autocrypt-test".into(),
        message_id: None,
        in_reply_to: None,
        references: vec![],
        date: Some("Thu, 01 Jan 1970 00:00:00 +0000".into()),
        autocrypt: Some(ac_value),
    };

    // Build a signed message (which emits Autocrypt header via OuterHeaders).
    let signed_msg = inbx_pgp::mime::sign_pgp_mime(&src, &key_id, inner, &outer)
        .await
        .unwrap();

    let r = render_message_with_pgp(&signed_msg, RemotePolicy::Block, None, None)
        .await
        .unwrap();

    let ac = r
        .autocrypt
        .expect("Rendered.autocrypt should be Some for a message with Autocrypt: header");
    assert_eq!(
        ac.addr, "alice@example.com",
        "addr should match From address"
    );
    assert_eq!(
        ac.fingerprint,
        key_id.0.to_lowercase(),
        "fingerprint should round-trip"
    );
}
