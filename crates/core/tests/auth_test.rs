use appbase_core::auth::{extract_bearer, hash_api_key, validate_api_key, validate_control_key};

#[test]
fn control_key_valid() {
    assert!(validate_control_key("my-secret-key", "my-secret-key"));
}

#[test]
fn control_key_invalid() {
    assert!(!validate_control_key("wrong-key", "my-secret-key"));
}

#[test]
fn control_key_empty() {
    // Both empty — identical, so valid.
    assert!(validate_control_key("", ""));
    // Length mismatch — invalid.
    assert!(!validate_control_key("", "nonempty"));
    assert!(!validate_control_key("nonempty", ""));
}

#[test]
fn api_key_roundtrip() {
    let key = "super-secret-api-key";
    let stored = hash_api_key(key);
    // Hash must be a 64-char hex SHA-256.
    assert_eq!(stored.len(), 64);
    assert!(stored.chars().all(|c| c.is_ascii_hexdigit()));
    // Validation against the correct hash must succeed.
    assert!(validate_api_key(key, &stored));
}

#[test]
fn api_key_wrong() {
    let key = "correct-key";
    let stored = hash_api_key(key);
    assert!(!validate_api_key("wrong-key", &stored));
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
