use inbx_pgp::config::{KeySourceKind, PgpConfig};

#[test]
fn gnupg_roundtrip() {
    let cfg = PgpConfig {
        key_source: KeySourceKind::Gnupg,
        key_fingerprint: Some("AABBCCDDEEFF00112233445566778899AABBCCDD".into()),
        managed_dir: None,
        prefer_encrypt_mutual: true,
    };
    let raw = toml::to_string_pretty(&cfg).unwrap();
    let parsed: PgpConfig = toml::from_str(&raw).unwrap();
    assert_eq!(parsed.key_source, KeySourceKind::Gnupg);
    assert_eq!(
        parsed.key_fingerprint.as_deref(),
        Some("AABBCCDDEEFF00112233445566778899AABBCCDD")
    );
    assert!(parsed.managed_dir.is_none());
    assert!(parsed.prefer_encrypt_mutual);
}

#[test]
fn inbx_managed_roundtrip() {
    let cfg = PgpConfig {
        key_source: KeySourceKind::InbxManaged,
        key_fingerprint: None,
        managed_dir: Some("/home/user/.local/share/inbx/pgp".into()),
        prefer_encrypt_mutual: false,
    };
    let raw = toml::to_string_pretty(&cfg).unwrap();
    let parsed: PgpConfig = toml::from_str(&raw).unwrap();
    assert_eq!(parsed.key_source, KeySourceKind::InbxManaged);
    assert!(parsed.key_fingerprint.is_none());
    assert_eq!(
        parsed.managed_dir.as_deref(),
        Some(std::path::Path::new("/home/user/.local/share/inbx/pgp"))
    );
    assert!(!parsed.prefer_encrypt_mutual);
}

#[test]
fn default_is_gnupg() {
    let cfg = PgpConfig::default();
    assert_eq!(cfg.key_source, KeySourceKind::Gnupg);
    assert!(cfg.key_fingerprint.is_none());
    assert!(cfg.managed_dir.is_none());
}

#[test]
fn minimal_toml_parses() {
    let raw = r#"key_source = "inbx-managed""#;
    let cfg: PgpConfig = toml::from_str(raw).unwrap();
    assert_eq!(cfg.key_source, KeySourceKind::InbxManaged);
}
