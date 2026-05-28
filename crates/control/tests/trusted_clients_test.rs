use zeroship_control::trusted_clients::{is_trusted, TRUSTED_OAUTH_CLIENTS};

#[test]
fn builder_is_trusted() {
    assert!(is_trusted("zeroship-builder"));
}

#[test]
fn arbitrary_client_is_not_trusted() {
    assert!(!is_trusted("acme-ci"));
    assert!(!is_trusted(""));
    assert!(!is_trusted("zeroship-builder-malicious-suffix"));
}

#[test]
fn whitelist_is_non_empty() {
    assert!(!TRUSTED_OAUTH_CLIENTS.is_empty());
}
