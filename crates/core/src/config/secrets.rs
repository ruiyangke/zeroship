//! Shared security policy: secret-strength validation and loopback checks.

use std::net::IpAddr;

use base64::Engine;

// These values used to be shipped development defaults. They are public and
// therefore compromised even when they happen to meet a length or decoding
// requirement. Keep them only as denylist entries; no runtime path supplies
// them as defaults.
const KNOWN_WEAK_STASH_KEYS: &[&str] = &[
    "dev-stash-key-please-rotate",
    "dev-stash-signing-key-not-for-production",
    "dev-only-stash-signing-key-not-for-production-use!!",
];
const KNOWN_WEAK_PAIRWISE_SALTS: &[&str] = &["dev-pairwise-salt-never-rotate-in-prod"];
const KNOWN_WEAK_WORKER_KEYS: &[&str] = &["dev-worker-key-not-for-production-use"];
const KNOWN_WEAK_MASTER_KEYS: &[&str] =
    &["00000000000000000000000000000000000000000000000000000000000000ff"];

fn reject_known_weak(label: &str, value: &str, denylist: &[&str]) -> Result<(), String> {
    if denylist.contains(&value) {
        Err(format!(
            "{label} is a known public development value; generate a unique secret"
        ))
    } else {
        Ok(())
    }
}

/// Require `value` to be non-empty.
///
/// # Errors
///
/// Returns a startup-facing message naming `label` when the value is missing.
pub fn require_nonempty(label: &str, value: &str) -> Result<(), String> {
    if !value.is_empty() {
        Ok(())
    } else {
        Err(format!("{label} is required"))
    }
}

/// Validate the shared requirement for a stash signing key.
///
/// The single stash validator across every binary: non-empty, raw UTF-8 length
/// of at least 32 bytes.
///
/// # Errors
///
/// Returns an explanatory error when `value` is a known public development
/// value, empty, or shorter than 32 bytes.
pub fn validate_stash_key(value: &str) -> Result<(), String> {
    reject_known_weak("STASH_SIGNING_KEY", value, KNOWN_WEAK_STASH_KEYS)?;
    if value.is_empty() {
        return Err("STASH_SIGNING_KEY is required; set a strong (>=32 byte) value".to_owned());
    }

    if value.len() < 32 {
        return Err(format!(
            "STASH_SIGNING_KEY is too short ({} bytes); minimum 32 bytes",
            value.len()
        ));
    }

    Ok(())
}

/// Validate the requirement for the dedicated pairwise-salt secret
/// (auth-sdk section 6.2). Same shape as [`validate_stash_key`]: non-empty,
/// raw UTF-8 length of at least 32 bytes. The error text
/// stresses the never-rotate contract so an operator does not treat it like a
/// rotatable operational key.
///
/// # Errors
///
/// Returns an explanatory error when `value` is a known public development
/// value, empty, or shorter than 32 bytes.
pub fn validate_pairwise_salt(value: &str) -> Result<(), String> {
    reject_known_weak("PAIRWISE_SALT", value, KNOWN_WEAK_PAIRWISE_SALTS)?;
    if value.is_empty() {
        return Err(
            "PAIRWISE_SALT is required; set a strong (>=32 byte) value \
             (identical on gateway + control, never rotated without a migration)"
                .to_owned(),
        );
    }

    if value.len() < 32 {
        return Err(format!(
            "PAIRWISE_SALT is too short ({} bytes); minimum 32 bytes",
            value.len()
        ));
    }

    Ok(())
}

/// Validate the requirement for the worker dispatch key.
///
/// The `worker_key` authenticates the gateway→worker dispatch bearer AND keys
/// the per-request `ZeroShip-User` HMAC (the only authoritative identity channel
/// into app code). An empty or weak key therefore disables auth or makes the
/// HMAC forgeable, so it carries the same strength posture as the stash key and
/// pairwise salt: non-empty, raw UTF-8 length of at least 32 bytes.
///
/// # Errors
///
/// Returns an explanatory error when `value` is a known public development
/// value, empty, or shorter than 32 bytes.
pub fn validate_worker_key(value: &str) -> Result<(), String> {
    reject_known_weak("WORKER_KEY", value, KNOWN_WEAK_WORKER_KEYS)?;
    if value.is_empty() {
        return Err("WORKER_KEY is required; set a strong (>=32 byte) value".to_owned());
    }

    if value.len() < 32 {
        return Err(format!(
            "WORKER_KEY is too short ({} bytes); minimum 32 bytes",
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
/// Returns an explanatory error when `value` is a known public development
/// value or does not decode to at least 32 bytes.
pub fn validate_master_key_material(label: &str, value: &str) -> Result<(), String> {
    reject_known_weak(label, value, KNOWN_WEAK_MASTER_KEYS)?;
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

/// A secret input: a literal value, or a file reference.
///
/// Detection is by RESERVED PREFIX: a value starting with `urn:` or `arn:` is a
/// reference; anything else is a [`SecretRef::Literal`].
///
/// The type admits exactly two things, and that is the whole point. A secret is
/// either the material itself or a path to the file holding it; there is no
/// third form. The env-to-env alias (`urn:zeroship:env:<VAR>`) is gone because
/// an environment source that points at another environment name is the
/// deployment alias hop under a different spelling, and the parsed-but-
/// unresolvable Vault / AWS Secrets Manager forms are gone because a syntax the
/// runtime always refuses is not a deployment source. `arn:` stays a RESERVED
/// prefix so an AWS ARN is refused loudly rather than silently taken as a
/// literal secret.
#[derive(Debug, PartialEq, Eq)]
pub enum SecretRef<'a> {
    /// A literal secret value, used verbatim.
    Literal(&'a str),
    /// `urn:zeroship:file:<path>` — resolves to the (newline-trimmed) file contents.
    File(&'a str),
}

/// Failure modes for resolving a [`SecretRef`].
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// `urn:zeroship:file:<path>` could not be read.
    #[error("read secret file '{path}': {source}")]
    FileIo {
        /// The path that failed to read.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A value with a reserved `urn:`/`arn:` prefix that is not a recognized reference.
    #[error("malformed secret reference '{0}' (a value starting with urn:/arn: must be a recognized reference: the only one is urn:zeroship:file:<path>)")]
    Malformed(String),
}

const URN_FILE: &str = "urn:zeroship:file:";

/// Parse a raw secret input into a [`SecretRef`].
///
/// A value NOT starting with `urn:` or `arn:` is always [`SecretRef::Literal`].
/// A `urn:`/`arn:` value MUST be `urn:zeroship:file:<path>`, else this returns
/// [`SecretError::Malformed`] — reserved prefixes never silently become literals.
///
/// # Errors
///
/// Returns [`SecretError::Malformed`] when a `urn:`/`arn:` value is not a file
/// reference, or names the file scheme with an empty body.
pub fn parse_secret_ref(raw: &str) -> Result<SecretRef<'_>, SecretError> {
    if !raw.starts_with("urn:") && !raw.starts_with("arn:") {
        return Ok(SecretRef::Literal(raw));
    }

    if let Some(rest) = raw.strip_prefix(URN_FILE) {
        return non_empty(rest, raw).map(SecretRef::File);
    }

    Err(SecretError::Malformed(raw.to_owned()))
}

/// A recognized scheme with an empty body is [`SecretError::Malformed`].
fn non_empty<'a>(rest: &'a str, raw: &str) -> Result<&'a str, SecretError> {
    if rest.is_empty() {
        Err(SecretError::Malformed(raw.to_owned()))
    } else {
        Ok(rest)
    }
}

/// Resolve a secret input to its literal value. Literals pass through unchanged.
///
/// PERFORMS A FILE READ for a `urn:zeroship:file:` reference — do NOT call
/// during `--check-config`; use [`validate_secret_ref`] there instead.
///
/// Reserved-prefix tradeoff: because any value starting with `urn:` or `arn:` is
/// treated as a reference, a *literal* secret whose own text begins with `urn:`
/// or `arn:` cannot be expressed as a [`SecretRef::Literal`] — it will parse as a
/// reference (and error as malformed, since the file scheme is the only one).
/// Operators who genuinely need such a literal must put it in a file and point
/// `urn:zeroship:file:/path` at it; a bare literal beginning with these reserved
/// prefixes is not representable.
///
/// File contents are the literal secret and are NOT recursively parsed as
/// another reference.
///
/// # Errors
///
/// Returns [`SecretError::FileIo`] for an unreadable file and
/// [`SecretError::Malformed`] for an unrecognized `urn:`/`arn:` value.
pub fn resolve_secret(raw: &str) -> Result<String, SecretError> {
    match parse_secret_ref(raw)? {
        SecretRef::Literal(s) => Ok(s.to_owned()),
        SecretRef::File(path) => read_secret_file(path),
    }
}

/// Read a secret file, stripping exactly one trailing line ending.
///
/// Shared by the `urn:zeroship:file:` reference arm and the generated `-file`
/// path flag, so the two spellings of "the secret is in this file" cannot
/// disagree about trailing whitespace.
///
/// # Errors
///
/// Returns [`SecretError::FileIo`] when the file cannot be read.
pub fn read_secret_file(path: &str) -> Result<String, SecretError> {
    match std::fs::read_to_string(path) {
        Ok(mut contents) => {
            // Strip a single trailing '\n' (and a preceding '\r' if present),
            // matching how `printf 'secret' > file` vs an editor's trailing
            // newline differ.
            if contents.ends_with('\n') {
                contents.pop();
                if contents.ends_with('\r') {
                    contents.pop();
                }
            }
            Ok(contents)
        }
        Err(source) => Err(SecretError::FileIo {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Validate a secret input's SOURCE POLICY and FORMAT only — NO file access.
///
/// For `--check-config` dry runs. A literal is `Ok`; `urn:zeroship:file:<path>`
/// is `Ok` whether or not the path exists (existence is NOT checked here); every
/// other `urn:`/`arn:` value is an error, which is what makes an env-to-env
/// alias, a Vault URN and an AWS ARN all fail the dry run rather than fail at
/// boot.
///
/// # Errors
///
/// Returns [`SecretError::Malformed`] when a `urn:`/`arn:` value is not a file
/// reference. Performs no side effects.
pub fn validate_secret_ref(raw: &str) -> Result<(), SecretError> {
    parse_secret_ref(raw).map(|_| ())
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use base64::Engine as _;

    use super::{
        decoded_master_key_len, is_loopback_url, parse_secret_ref, require_nonempty,
        resolve_secret, validate_master_key_material, validate_pairwise_salt, validate_secret_ref,
        validate_stash_key, validate_worker_key, SecretError, SecretRef, KNOWN_WEAK_MASTER_KEYS,
        KNOWN_WEAK_PAIRWISE_SALTS, KNOWN_WEAK_STASH_KEYS, KNOWN_WEAK_WORKER_KEYS,
    };

    #[test]
    fn require_nonempty_rejects_missing_values() {
        let err = require_nonempty("CONTROL_KEY / --control-key", "")
            .expect_err("missing secret");
        assert_eq!(err, "CONTROL_KEY / --control-key is required");
        require_nonempty("CONTROL_KEY / --control-key", "secret").expect("present");
    }

    // S4: one unified stash validator. Empty and short keys are rejected and
    // values of at least 32 bytes are accepted.
    #[test]
    fn validate_stash_key_unified() {
        for weak in KNOWN_WEAK_STASH_KEYS {
            assert!(
                validate_stash_key(weak)
                    .expect_err("known public stash key")
                    .contains("known public")
            );
        }

        // empty rejected
        assert!(validate_stash_key("")
            .expect_err("empty key")
            .contains("required"));

        // short (<32) rejected
        assert!(validate_stash_key("short")
            .expect_err("short key")
            .contains("too short"));

        // exactly 32 ok
        validate_stash_key("0123456789abcdef0123456789abcdef").expect("strong key");
    }

    /// MAJOR fix (pairwise_salt secret lifecycle) — the dedicated pairwise-salt
    /// secret has its OWN validator with the SAME strength posture as the stash
    /// key while remaining a distinct input with its own lifecycle.
    #[test]
    fn validate_pairwise_salt_enforces_strength() {
        for weak in KNOWN_WEAK_PAIRWISE_SALTS {
            assert!(
                validate_pairwise_salt(weak)
                    .expect_err("known public pairwise salt")
                    .contains("known public")
            );
        }

        // empty rejected, short rejected, exactly 32 ok
        assert!(validate_pairwise_salt("")
            .expect_err("empty")
            .contains("required"));
        assert!(validate_pairwise_salt("short")
            .expect_err("short")
            .contains("too short"));
        validate_pairwise_salt("0123456789abcdef0123456789abcdef").expect("strong salt");
    }

    // L6: the worker_key gates BOTH the dispatch bearer check and the
    // ZeroShip-User HMAC. Presence-only validation let a weak short key bind
    // any interface and be brute-forced for header forgery. It now carries the
    // SAME ≥32-byte strength floor as the stash key / pairwise salt: empty
    // rejected, short rejected, and values of at least 32 bytes accepted.
    #[test]
    fn validate_worker_key_enforces_min_len() {
        for weak in KNOWN_WEAK_WORKER_KEYS {
            assert!(
                validate_worker_key(weak)
                    .expect_err("known public worker key")
                    .contains("known public")
            );
        }

        // empty rejected (an empty key would HMAC-verify against a zero-length
        // key any party can compute)
        assert!(validate_worker_key("")
            .expect_err("empty key")
            .contains("required"));

        // short (<32) rejected
        assert!(validate_worker_key("short")
            .expect_err("short key")
            .contains("too short"));

        // exactly 32 ok
        validate_worker_key("0123456789abcdef0123456789abcdef").expect("strong key");
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
        for weak in KNOWN_WEAK_MASTER_KEYS {
            assert!(
                validate_master_key_material("MASTER_KEY", weak)
                    .expect_err("known public master key")
                    .contains("known public")
            );
        }

        // 32 decoded bytes ok
        let key = "00".repeat(32);
        validate_master_key_material("MASTER_KEY", &key).expect("32 bytes ok");

        // short decode rejected; error text says "bytes" not "random bytes" (O4)
        let err = validate_master_key_material("MASTER_KEY", "YWJj").unwrap_err();
        assert!(err.contains("bytes"));
        assert!(!err.contains("random"));

        // undecodable rejected
        assert!(validate_master_key_material("MASTER_KEY", "not!base64!").is_err());
    }

    // S8: literal-only loopback, no DNS resolution.
    #[test]
    fn is_loopback_url_literal_only() {
        assert!(is_loopback_url("http://127.0.0.1:4445"));
        assert!(is_loopback_url("http://localhost:4445"));
        assert!(is_loopback_url("http://[::1]:4445"));

        // non-loopback hostnames rejected
        assert!(!is_loopback_url("http://auth-internal:4445"));

        // a hostname that resolves to loopback must NOT be accepted (no DNS).
        assert!(!is_loopback_url("http://localhost.localdomain:4445"));
        assert!(!is_loopback_url("http://127.0.0.1.nip.io:4445"));

        // garbage / no host
        assert!(!is_loopback_url("not a url"));
    }


    #[test]
    fn literal_passthrough_including_colons() {
        let literal = "hunter2hunter2hunter2hunter2hunter2";
        assert_eq!(
            parse_secret_ref(literal).expect("literal parses"),
            SecretRef::Literal(literal)
        );
        assert_eq!(resolve_secret(literal).expect("literal resolves"), literal);

        // A DSN-like literal full of colons must NOT be mistaken for a reference.
        let dsn = "postgres://u:p@h/db";
        assert_eq!(
            parse_secret_ref(dsn).expect("dsn parses"),
            SecretRef::Literal(dsn)
        );
        assert_eq!(resolve_secret(dsn).expect("dsn resolves"), dsn);

        // Does NOT cover a literal that itself begins `urn:`/`arn:`; that shape
        // is deliberately unrepresentable and the case below pins it.
    }

    #[test]
    fn file_reference_trims_trailing_newline() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("zeroship_secret_{}.txt", std::process::id()));
        // RAII cleanup: remove the temp file even if an assertion below panics.
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _cleanup = Cleanup(path.clone());
        {
            let mut f = std::fs::File::create(&path).expect("create temp secret");
            // Trailing newline (and a CR) must be stripped on resolve.
            f.write_all(b"file-secret-value\r\n").expect("write secret");
        }
        let path_str = path.to_str().expect("utf8 path");
        let reference = format!("urn:zeroship:file:{path_str}");

        assert_eq!(
            parse_secret_ref(&reference).expect("file ref parses"),
            SecretRef::File(path_str)
        );
        assert_eq!(
            resolve_secret(&reference).expect("file resolves"),
            "file-secret-value"
        );

        // Nonexistent path => FileIo on resolve.
        let missing = "urn:zeroship:file:/no/such/zeroship/secret/path";
        match resolve_secret(missing) {
            Err(SecretError::FileIo { path, .. }) => {
                assert_eq!(path, "/no/such/zeroship/secret/path");
            }
            other => panic!("expected FileIo, got {other:?}"),
        }

        // But validate (format-only) accepts it without touching the filesystem.
        validate_secret_ref(missing).expect("missing file path is format-valid");

        // Does NOT cover file PERMISSIONS: a world-readable secret file still
        // resolves here. That check belongs to the overlay permission gate.
    }

    // File contents are the literal secret. A file whose text happens to spell a
    // reference must NOT be dereferenced again - otherwise a secret store could
    // be made to redirect a read at a path the operator never named.
    #[test]
    fn file_contents_are_not_recursively_parsed_as_another_reference() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("zeroship_secret_nested_{}.txt", std::process::id()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _cleanup = Cleanup(path.clone());
        std::fs::write(&path, b"urn:zeroship:file:/etc/shadow\n").expect("write nested ref");
        let reference = format!("urn:zeroship:file:{}", path.to_str().expect("utf8 path"));

        assert_eq!(
            resolve_secret(&reference).expect("file resolves"),
            "urn:zeroship:file:/etc/shadow"
        );

        // Does NOT cover the CLI `-file` flag arm; that path shares
        // `read_secret_file` but is exercised through the generated resolver.
    }

    // THE DELETED ARMS. Each of these used to PARSE: `urn:zeroship:env:` became
    // SecretRef::Env and resolved a second environment variable, and the vault /
    // awssm / arn forms became SecretRef::Vault / ::AwsSecretsManager and
    // returned BackendUnavailable at boot while passing --check-config. All four
    // are now malformed at parse, which is what makes --check-config reject them.
    #[test]
    fn the_deleted_reference_schemes_are_malformed_not_recognized() {
        for deleted in [
            "urn:zeroship:env:SOME_OTHER_VAR",
            "urn:zeroship:vault:secret/data/app",
            "urn:zeroship:awssm:prod/db",
            "arn:aws:secretsmanager:us-east-1:123:secret:x",
        ] {
            match parse_secret_ref(deleted) {
                Err(SecretError::Malformed(raw)) => assert_eq!(raw, deleted),
                other => panic!("expected Malformed for {deleted:?}, got {other:?}"),
            }
            assert!(
                validate_secret_ref(deleted).is_err(),
                "{deleted:?} must fail the no-side-effect dry run too"
            );
            assert!(resolve_secret(deleted).is_err());
        }

        // The one-variable control: the SAME shape with the one surviving scheme
        // parses. Without this, "everything is malformed" would also pass.
        assert_eq!(
            parse_secret_ref("urn:zeroship:file:/tmp/x").expect("file scheme survives"),
            SecretRef::File("/tmp/x")
        );

        // Does NOT cover: a `urn:zeroship:env:` spelling surviving anywhere else
        // in the tree. That is a grep, not a unit test.
    }

    #[test]
    fn malformed_references_are_rejected() {
        for bad in [
            "urn:bogus:x",
            "urn:zeroship:nope:x",
            "urn:zeroship:file:", // recognized scheme, empty body
            "arn:aws:s3:::bucket",
        ] {
            match parse_secret_ref(bad) {
                Err(SecretError::Malformed(r)) => assert_eq!(r, bad),
                other => panic!("expected Malformed for {bad:?}, got {other:?}"),
            }
            match resolve_secret(bad) {
                Err(SecretError::Malformed(r)) => assert_eq!(r, bad),
                other => panic!("expected Malformed (resolve) for {bad:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn validate_secret_ref_performs_no_side_effects() {
        // A file reference at a path that does not exist is FORMAT-valid: the
        // dry run must never open it. This is the half of the pair that would
        // fail if validation quietly started stat-ing the path.
        validate_secret_ref("urn:zeroship:file:/definitely/not/a/real/path")
            .expect("missing file ref is format-valid");

        // The one-variable partner: same field, same shape, only the scheme
        // differs - and a scheme outside the supply set fails.
        assert!(matches!(
            validate_secret_ref("urn:zeroship:env:DEFINITELY_UNSET_VAR"),
            Err(SecretError::Malformed(_))
        ));
        assert!(matches!(
            validate_secret_ref("urn:zeroship:nope:x"),
            Err(SecretError::Malformed(_))
        ));
    }
}
