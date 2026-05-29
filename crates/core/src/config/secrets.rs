//! Shared security policy: secret-strength validation and loopback checks.

use std::net::IpAddr;

use base64::Engine;

/// Development-only stash signing key used by web binaries when insecure dev is explicit.
pub const DEV_STASH_SIGNING_KEY: &str = "dev-stash-key-please-rotate";

/// Require `value` to be non-empty unless insecure development mode is explicit.
///
/// # Errors
///
/// Returns a startup-facing message naming `label` when the value is missing
/// outside `--dev-insecure`.
pub fn require_unless_dev(label: &str, value: &str, insecure_dev: bool) -> Result<(), String> {
    if insecure_dev || !value.is_empty() {
        Ok(())
    } else {
        Err(format!("{label} is required outside --dev-insecure"))
    }
}

/// Validate the shared production requirement for a stash signing key.
///
/// The single stash validator across every binary: exact-match dev sentinel,
/// non-empty, raw UTF-8 length ≥ 32 bytes.
///
/// # Errors
///
/// Returns an explanatory error when `value` is the development sentinel, empty,
/// or shorter than 32 bytes, unless `insecure_dev` is enabled.
pub fn validate_stash_key(value: &str, insecure_dev: bool) -> Result<(), String> {
    if insecure_dev {
        return Ok(());
    }

    if value == DEV_STASH_SIGNING_KEY {
        return Err(
            "STASH_SIGNING_KEY is the dev default; refusing to boot without --dev-insecure"
                .to_owned(),
        );
    }

    if value.is_empty() {
        return Err(
            "STASH_SIGNING_KEY is required outside --dev-insecure; set a strong (>=32 byte) value"
                .to_owned(),
        );
    }

    if value.len() < 32 {
        return Err(format!(
            "STASH_SIGNING_KEY is too short ({} bytes); minimum 32 bytes",
            value.len()
        ));
    }

    Ok(())
}

/// Decode a master-key candidate and return its byte length, if decodable.
///
/// Accepts hex (even length, all hex digits, ≥32 decoded bytes) or
/// base64url (padded or unpadded). Returns `None` when the value is empty or
/// cannot be decoded.
#[must_use]
pub fn decoded_master_key_len(value: &str) -> Option<usize> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed.len().is_multiple_of(2) && trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        if let Ok(bytes) = hex::decode(trimmed) {
            if bytes.len() >= 32 {
                return Some(bytes.len());
            }
        }
    }

    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(trimmed)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(trimmed))
        .ok()
        .map(|bytes| bytes.len())
}

/// Validate master-key material: must decode (hex or base64url) to ≥ 32 bytes.
///
/// # Errors
///
/// Returns an explanatory error when `value` does not decode to at least 32
/// bytes, unless `insecure_dev` is enabled.
pub fn validate_master_key_material(
    label: &str,
    value: &str,
    insecure_dev: bool,
) -> Result<(), String> {
    if insecure_dev {
        return Ok(());
    }
    match decoded_master_key_len(value) {
        Some(n) if n >= 32 => Ok(()),
        Some(n) => Err(format!("{label} decodes to {n} bytes; minimum is 32 bytes")),
        None => Err(format!(
            "{label} must be hex or base64url encoded and decode to at least 32 bytes"
        )),
    }
}

/// Return true when `url` points at a literal loopback host.
///
/// Accepts ONLY `host == "localhost"` (case-insensitive) or a literal IP whose
/// [`IpAddr::is_loopback`] is true. Performs NO DNS resolution — a hostname that
/// happens to resolve to loopback is rejected, closing the rebind/TOCTOU window
/// the previous `to_socket_addrs` check left open (S8).
#[must_use]
pub fn is_loopback_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };

    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }

    // `url` keeps IPv6 hosts bracketed in `host_str`; strip the brackets so the
    // IpAddr parser sees a bare address.
    let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);

    host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;

    use super::{
        decoded_master_key_len, is_loopback_url, require_unless_dev, validate_master_key_material,
        validate_stash_key, DEV_STASH_SIGNING_KEY,
    };

    #[test]
    fn require_unless_dev_rejects_missing_only_outside_dev() {
        let err = require_unless_dev("CONTROL_KEY / --control-key", "", false)
            .expect_err("missing secret");
        assert_eq!(
            err,
            "CONTROL_KEY / --control-key is required outside --dev-insecure"
        );

        require_unless_dev("CONTROL_KEY / --control-key", "", true).expect("dev bypass");
        require_unless_dev("CONTROL_KEY / --control-key", "secret", false).expect("present");
    }

    // S4: one unified stash validator. The dev sentinel is rejected by
    // exact-match (not prefix), short keys are rejected, ≥32 ok, dev bypass.
    #[test]
    fn validate_stash_key_unified() {
        // dev sentinel rejected non-dev
        let err = validate_stash_key(DEV_STASH_SIGNING_KEY, false).expect_err("dev default");
        assert!(err.contains("dev default"));

        // empty rejected
        assert!(validate_stash_key("", false)
            .expect_err("empty key")
            .contains("required"));

        // short (<32) rejected
        assert!(validate_stash_key("short", false)
            .expect_err("short key")
            .contains("too short"));

        // exactly 32 ok
        validate_stash_key("0123456789abcdef0123456789abcdef", false).expect("strong key");

        // insecure_dev bypasses every check
        validate_stash_key("", true).expect("insecure dev bypass");
        validate_stash_key(DEV_STASH_SIGNING_KEY, true).expect("insecure dev bypass sentinel");
    }

    #[test]
    fn decoded_master_key_len_handles_hex_and_base64() {
        // 64 hex chars -> 32 bytes
        let hex = "00".repeat(32);
        assert_eq!(decoded_master_key_len(&hex), Some(32));

        // base64url of 32 bytes
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0u8; 32]);
        assert_eq!(decoded_master_key_len(&b64), Some(32));

        // empty -> None
        assert_eq!(decoded_master_key_len(""), None);
    }

    #[test]
    fn validate_master_key_material_enforces_min_len() {
        // 32 decoded bytes ok
        let key = "00".repeat(32);
        validate_master_key_material("MASTER_KEY", &key, false).expect("32 bytes ok");

        // short decode rejected; error text says "bytes" not "random bytes" (O4)
        let err = validate_master_key_material("MASTER_KEY", "YWJj", false).unwrap_err();
        assert!(err.contains("bytes"));
        assert!(!err.contains("random"));

        // undecodable rejected
        assert!(validate_master_key_material("MASTER_KEY", "not!base64!", false).is_err());

        // insecure_dev bypass
        validate_master_key_material("MASTER_KEY", "password", true).expect("dev bypass");
    }

    // S8: literal-only loopback, no DNS resolution.
    #[test]
    fn is_loopback_url_literal_only() {
        assert!(is_loopback_url("http://127.0.0.1:4445"));
        assert!(is_loopback_url("http://localhost:4445"));
        assert!(is_loopback_url("http://[::1]:4445"));

        // non-loopback hostnames rejected
        assert!(!is_loopback_url("http://hydra:4445"));

        // a hostname that resolves to loopback must NOT be accepted (no DNS).
        assert!(!is_loopback_url("http://localhost.localdomain:4445"));
        assert!(!is_loopback_url("http://127.0.0.1.nip.io:4445"));

        // garbage / no host
        assert!(!is_loopback_url("not a url"));
    }
}
