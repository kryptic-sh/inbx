//! Smoke tests for `inbx_config::ProxyConfig::parse` and the
//! `inbx_net::proxy::connect` no-proxy path.

use inbx_config::ProxyConfig;
use inbx_net::proxy;

fn proxy_cfg(url: &str) -> ProxyConfig {
    ProxyConfig {
        url: url.to_string(),
        username: None,
    }
}

#[test]
fn proxy_config_parse_socks5() {
    let p = proxy_cfg("socks5://127.0.0.1:9050");
    let parsed = p.parse().expect("valid socks5 URL");
    assert_eq!(parsed.host, "127.0.0.1");
    assert_eq!(parsed.port, 9050);
    assert!(!parsed.remote_dns, "socks5 should not use remote DNS");
}

#[test]
fn proxy_config_parse_socks5h() {
    let p = proxy_cfg("socks5h://proxy.example.com:1080");
    let parsed = p.parse().expect("valid socks5h URL");
    assert_eq!(parsed.host, "proxy.example.com");
    assert_eq!(parsed.port, 1080);
    assert!(parsed.remote_dns, "socks5h should use remote DNS");
}

#[test]
fn proxy_config_parse_garbage() {
    let p = proxy_cfg("not a url at all!!!");
    assert!(p.parse().is_err(), "garbage URL must return Err");
}

#[test]
fn proxy_config_parse_wrong_scheme() {
    let p = proxy_cfg("http://proxy.example.com:3128");
    assert!(
        p.parse().is_err(),
        "http scheme is not supported, must return Err"
    );
}

/// Direct TCP connect with no proxy.  Requires outbound internet access and
/// `INBX_NETWORK_TESTS=1` to be set in the environment.
#[tokio::test]
async fn proxy_connect_with_no_proxy() {
    if std::env::var("INBX_NETWORK_TESTS").unwrap_or_default() != "1" {
        return; // skip when network is unavailable
    }
    let stream = proxy::connect(None, "example.com", 80, "").await;
    assert!(
        stream.is_ok(),
        "direct connect to example.com:80 failed: {stream:?}"
    );
}
