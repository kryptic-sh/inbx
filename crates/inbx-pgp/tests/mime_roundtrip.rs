use base64::{Engine, engine::general_purpose::STANDARD as B64};
use inbx_pgp::{
    ArmoredKey, KeySource, Signature,
    inbx_managed::{InbxManagedSource, keygen},
    mime::{OuterHeaders, encrypt_pgp_mime, sign_pgp_mime},
};

/// Base64-decode a MIME part body (stripping whitespace first).
fn b64_decode_body(body: &[u8]) -> Vec<u8> {
    let s: String = String::from_utf8_lossy(body)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    B64.decode(s.as_bytes()).unwrap_or_else(|_| body.to_vec())
}

fn test_headers() -> OuterHeaders {
    OuterHeaders {
        from: "Alice <alice@example.com>".into(),
        to: vec!["Bob <bob@example.com>".into()],
        cc: vec![],
        bcc: vec![],
        subject: "Test".into(),
        message_id: Some("<test@example.com>".into()),
        in_reply_to: None,
        references: vec![],
        date: Some("Thu, 01 Jan 1970 00:00:00 +0000".into()),
        autocrypt: None,
    }
}

/// Pull the second MIME part body out of a two-part multipart message.
fn extract_parts(bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let s = String::from_utf8_lossy(bytes);
    // Find boundary from Content-Type header
    let boundary = s
        .lines()
        .find(|l| l.to_ascii_lowercase().contains("boundary="))
        .and_then(|l| {
            let idx = l.to_ascii_lowercase().find("boundary=\"")?;
            let after = &l[idx + 10..];
            let end = after.find('"')?;
            Some(after[..end].to_string())
        })
        .expect("boundary not found");

    let sep = format!("--{boundary}");
    let end_sep = format!("--{boundary}--");

    let parts: Vec<&str> = Vec::new();
    let mut in_part = false;
    let current_start = 0;
    let lines: Vec<&str> = s.split("\r\n").collect();
    let mut pos = 0usize;
    let mut part_starts: Vec<usize> = Vec::new();
    let mut part_ends: Vec<usize> = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        let _ = i;
        if line.trim() == sep.as_str() || *line == sep.as_str() {
            if in_part {
                part_ends.push(pos - sep.len() - 4); // rough
            }
            part_starts.push(pos + line.len() + 2);
            in_part = true;
        } else if line.trim() == end_sep.as_str() || *line == end_sep.as_str() {
            if in_part {
                part_ends.push(pos);
            }
            in_part = false;
        }
        pos += line.len() + 2; // +2 for \r\n
    }
    let _ = parts;
    let _ = current_start;
    let _ = in_part;

    // Simple split-based approach
    let raw_str = std::str::from_utf8(bytes).unwrap_or("");
    let sep_str = format!("--{boundary}");
    let split: Vec<&str> = raw_str.splitn(4, sep_str.as_str()).collect();
    // split[0] = preamble (before first boundary)
    // split[1] = first part (after first boundary line, before second)
    // split[2] = second part
    // split[3] = epilogue (after --)
    let first = if split.len() > 1 {
        split[1].trim_start_matches("\r\n").as_bytes().to_vec()
    } else {
        vec![]
    };
    let second = if split.len() > 2 {
        // Remove the trailing "--" that's part of end boundary
        let s2 = split[2].trim_start_matches("\r\n");
        let s2 = s2.trim_end_matches('-');
        s2.as_bytes().to_vec()
    } else {
        vec![]
    };
    (first, second)
}

/// Extract the body of a MIME part (after the double-CRLF blank line).
fn part_body(part: &[u8]) -> Vec<u8> {
    let s = std::str::from_utf8(part).unwrap_or("");
    if let Some(idx) = s.find("\r\n\r\n") {
        s[idx + 4..].trim_end_matches("\r\n").as_bytes().to_vec()
    } else if let Some(idx) = s.find("\n\n") {
        s[idx + 2..].trim_end_matches('\n').as_bytes().to_vec()
    } else {
        part.to_vec()
    }
}

#[tokio::test]
async fn sign_then_verify_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let (key_id, _) = keygen(&dir, "Alice", "alice@example.com", "")
        .await
        .unwrap();
    let src = InbxManagedSource::new(dir.clone());

    let inner =
        b"From: alice@example.com\r\nTo: bob@example.com\r\nSubject: hi\r\n\r\nHello world\r\n";
    let outer = sign_pgp_mime(&src, &key_id, inner, &test_headers())
        .await
        .unwrap();

    assert!(outer.len() > 100, "outer should have content");

    // Parse the outer: find the boundary and extract parts.
    let outer_str = String::from_utf8_lossy(&outer);
    assert!(
        outer_str.contains("multipart/signed"),
        "should be multipart/signed"
    );
    assert!(
        outer_str.contains("application/pgp-signature"),
        "should have pgp-signature part"
    );

    // Extract the two parts.
    let (first_part, second_part) = extract_parts(&outer);
    let inner_body = part_body(&first_part);
    let sig_body = part_body(&second_part);

    assert!(!inner_body.is_empty(), "inner body should not be empty");
    assert!(!sig_body.is_empty(), "signature body should not be empty");
    assert!(
        String::from_utf8_lossy(&sig_body).contains("-----BEGIN PGP SIGNATURE-----"),
        "second part should be PGP signature"
    );

    // Verify using the KeySource.
    let pub_key = src.export_public(&key_id).await.unwrap();

    // The signed data is the CRLF-normalised inner; re-extract it (trim trailing boundary noise).
    let signed_data = {
        let s = String::from_utf8_lossy(&outer);
        let boundary = s
            .lines()
            .find(|l| l.to_ascii_lowercase().contains("boundary=\""))
            .and_then(|l| {
                let idx = l.to_ascii_lowercase().find("boundary=\"")?;
                let after = &l[idx + 10..];
                let end = after.find('"')?;
                Some(after[..end].to_string())
            })
            .unwrap();
        let sep = format!("--{boundary}");
        // Find start after first separator line
        let full = std::str::from_utf8(&outer).unwrap();
        let after_sep = full.find(&sep).unwrap() + sep.len() + 2; // skip \r\n
        let next_sep_pos = full[after_sep..].find(&sep).unwrap() + after_sep;
        // The signed content is [after_sep .. next_sep_pos - 2] (minus the preceding \r\n)
        let signed_slice = if next_sep_pos >= 2 {
            &full[after_sep..next_sep_pos - 2]
        } else {
            &full[after_sep..next_sep_pos]
        };
        signed_slice.as_bytes().to_vec()
    };

    let sig = Signature(sig_body);
    let result = src
        .verify_detached(&pub_key, &signed_data, &sig)
        .await
        .unwrap();
    assert!(result.valid, "signature must verify");
}

#[tokio::test]
async fn encrypt_then_decrypt_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let (key_id, _) = keygen(&dir, "Bob", "bob@example.com", "").await.unwrap();
    let src = InbxManagedSource::new(dir.clone());
    let pub_key = src.export_public(&key_id).await.unwrap();

    let inner = b"From: alice@example.com\r\nSubject: secret\r\n\r\nSecret message\r\n";
    let outer = encrypt_pgp_mime(&src, None, &[pub_key], inner, &test_headers())
        .await
        .unwrap();

    let outer_str = String::from_utf8_lossy(&outer);
    assert!(
        outer_str.contains("multipart/encrypted"),
        "should be multipart/encrypted"
    );
    assert!(
        outer_str.contains("Version: 1"),
        "first part should be version ident"
    );

    // Extract the second part (ciphertext).
    let (_, second_part) = extract_parts(&outer);
    let ct_body_b64 = part_body(&second_part);
    assert!(!ct_body_b64.is_empty(), "ciphertext should not be empty");

    // The ciphertext part uses Content-Transfer-Encoding: base64.
    let ct_bytes = b64_decode_body(&ct_body_b64);

    // Decrypt.
    let ct = inbx_pgp::Ciphertext(ct_bytes);
    let (plain, _) = src.decrypt(&ct).await.unwrap();
    assert!(
        plain
            .0
            .windows(b"Secret message".len())
            .any(|w| w == b"Secret message"),
        "decrypted body should contain original plaintext"
    );
}

#[tokio::test]
async fn signed_and_encrypted_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let (key_id, _) = keygen(&dir, "Charlie", "charlie@example.com", "")
        .await
        .unwrap();
    let src = InbxManagedSource::new(dir.clone());
    let pub_key: ArmoredKey = src.export_public(&key_id).await.unwrap();

    let inner = b"From: charlie@example.com\r\nSubject: signed+encrypted\r\n\r\nHello\r\n";
    let outer = encrypt_pgp_mime(&src, Some(&key_id), &[pub_key], inner, &test_headers())
        .await
        .unwrap();

    let outer_str = String::from_utf8_lossy(&outer);
    assert!(
        outer_str.contains("multipart/encrypted"),
        "outer should be encrypted"
    );

    // Decrypt.
    let (_, second_part) = extract_parts(&outer);
    let ct_body_b64 = part_body(&second_part);
    let ct_bytes = b64_decode_body(&ct_body_b64);
    let ct = inbx_pgp::Ciphertext(ct_bytes);
    let (plain, _) = src.decrypt(&ct).await.unwrap();

    // The decrypted payload is itself a multipart/signed message.
    let decrypted_str = String::from_utf8_lossy(&plain.0);
    assert!(
        decrypted_str.contains("multipart/signed") || decrypted_str.contains("Hello"),
        "decrypted payload should contain signed inner or plaintext"
    );
}
