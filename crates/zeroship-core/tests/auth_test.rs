use zeroship_core::auth::{
    constant_time_eq, extract_bearer, hash_client_secret, validate_client_secret,
};

#[test]
fn control_key_valid() {
    assert!(constant_time_eq("my-secret-key", "my-secret-key"));
}

#[test]
fn control_key_invalid() {
    assert!(!constant_time_eq("wrong-key", "my-secret-key"));
}

#[test]
fn control_key_empty() {
    // Both empty — identical, so valid.
    assert!(constant_time_eq("", ""));
    // Length mismatch — invalid.
    assert!(!constant_time_eq("", "nonempty"));
    assert!(!constant_time_eq("nonempty", ""));
}

#[test]
fn client_secret_roundtrip() {
    let secret = "super-secret-client-secret";
    let stored = hash_client_secret(secret);
    // Hash must be a 64-char hex SHA-256.
    assert_eq!(stored.len(), 64);
    assert!(stored.chars().all(|c| c.is_ascii_hexdigit()));
    // Validation against the correct hash must succeed.
    assert!(validate_client_secret(secret, &stored));
}

#[test]
fn client_secret_wrong() {
    let secret = "correct-secret";
    let stored = hash_client_secret(secret);
    assert!(!validate_client_secret("wrong-secret", &stored));
}

#[test]
fn extract_bearer_token() {
    // Valid Bearer header.
    assert_eq!(extract_bearer("Bearer tok123"), Some("tok123"));
    // Missing "Bearer " prefix.
    assert_eq!(extract_bearer("Basic tok123"), None);
    assert_eq!(extract_bearer("bearer tok123"), None); // case-sensitive
    // Empty string.
    assert_eq!(extract_bearer(""), None);
    // "Bearer " with empty token — still a valid strip result.
    assert_eq!(extract_bearer("Bearer "), Some(""));
}
