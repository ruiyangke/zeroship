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

/// A secret input: a literal value, or a URN/ARN reference to an external source.
///
/// Detection is by RESERVED PREFIX: a value starting with `urn:` or `arn:` is a
/// reference; anything else is a [`SecretRef::Literal`] (so existing literal
/// secrets are untouched).
#[derive(Debug, PartialEq, Eq)]
pub enum SecretRef<'a> {
    /// A literal secret value, used verbatim.
    Literal(&'a str),
    /// `urn:zeroship:env:<VARNAME>` — resolves to the named environment variable.
    Env(&'a str),
    /// `urn:zeroship:file:<path>` — resolves to the (newline-trimmed) file contents.
    File(&'a str),
    /// `urn:zeroship:vault:<nss>` — `HashiCorp` Vault (resolution not yet implemented).
    Vault(&'a str),
    /// `urn:zeroship:awssm:<nss>` or `arn:aws:secretsmanager:<...>` — AWS Secrets
    /// Manager (resolution not yet implemented). The ARN form keeps the whole ARN.
    AwsSecretsManager(&'a str),
}

/// Failure modes for resolving a [`SecretRef`].
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// `urn:zeroship:env:<VAR>` named a variable that is not set in the environment.
    #[error("secret reference env var '{0}' is not set")]
    EnvUnset(String),
    /// `urn:zeroship:file:<path>` could not be read.
    #[error("read secret file '{path}': {source}")]
    FileIo {
        /// The path that failed to read.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A well-formed reference to a backend whose resolver is not implemented yet.
    #[error("secret backend '{backend}' is not yet implemented (reference '{reference}'); use a literal value, urn:zeroship:env:<VAR>, or urn:zeroship:file:<path>")]
    BackendUnavailable {
        /// Short backend identifier (`"vault"`, `"awssm"`).
        backend: &'static str,
        /// The original reference, echoed for operator diagnosis.
        reference: String,
    },
    /// A value with a reserved `urn:`/`arn:` prefix that is not a recognized reference.
    #[error("malformed secret reference '{0}' (a value starting with urn:/arn: must be a recognized reference: urn:zeroship:{{env|file|vault|awssm}}:<...> or arn:aws:secretsmanager:<...>)")]
    Malformed(String),
}

const URN_ENV: &str = "urn:zeroship:env:";
const URN_FILE: &str = "urn:zeroship:file:";
const URN_VAULT: &str = "urn:zeroship:vault:";
const URN_AWSSM: &str = "urn:zeroship:awssm:";
const ARN_AWSSM: &str = "arn:aws:secretsmanager:";

/// Parse a raw secret input into a [`SecretRef`].
///
/// A value NOT starting with `urn:` or `arn:` is always [`SecretRef::Literal`].
/// A `urn:`/`arn:` value MUST be a recognized reference, else this returns
/// [`SecretError::Malformed`] — reserved prefixes never silently become literals.
///
/// # Errors
///
/// Returns [`SecretError::Malformed`] when a `urn:`/`arn:` value is not a
/// recognized reference, or names a recognized scheme with an empty body.
pub fn parse_secret_ref(raw: &str) -> Result<SecretRef<'_>, SecretError> {
    if !raw.starts_with("urn:") && !raw.starts_with("arn:") {
        return Ok(SecretRef::Literal(raw));
    }

    // ARN form keeps the whole ARN (it has internal structure callers may need),
    // but still requires a non-empty body after the prefix — symmetric with the
    // urn: schemes, so a bare `arn:aws:secretsmanager:` is Malformed, not a ref.
    if let Some(rest) = raw.strip_prefix(ARN_AWSSM) {
        non_empty(rest, raw)?;
        return Ok(SecretRef::AwsSecretsManager(raw));
    }

    // urn:zeroship:<scheme>:<rest>; rest must be non-empty for every scheme.
    if let Some(rest) = raw.strip_prefix(URN_ENV) {
        return non_empty(rest, raw).map(SecretRef::Env);
    }
    if let Some(rest) = raw.strip_prefix(URN_FILE) {
        return non_empty(rest, raw).map(SecretRef::File);
    }
    if let Some(rest) = raw.strip_prefix(URN_VAULT) {
        return non_empty(rest, raw).map(SecretRef::Vault);
    }
    if let Some(rest) = raw.strip_prefix(URN_AWSSM) {
        return non_empty(rest, raw).map(SecretRef::AwsSecretsManager);
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
/// PERFORMS SIDE EFFECTS (env read, file read, future network fetch) — do NOT
/// call during `--check-config`; use [`validate_secret_ref`] there instead.
///
/// Reserved-prefix tradeoff: because any value starting with `urn:` or `arn:` is
/// treated as a reference, a *literal* secret whose own text begins with `urn:`
/// or `arn:` cannot be expressed as a [`SecretRef::Literal`] — it will parse as a
/// reference (and error as malformed if it is not a recognized one). Operators
/// who genuinely need such a literal must front it with one of the indirection
/// schemes (e.g. `urn:zeroship:env:MY_SECRET` or `urn:zeroship:file:/path`); a
/// bare literal beginning with these reserved prefixes is not representable.
///
/// # Errors
///
/// Returns [`SecretError::EnvUnset`] for an unset env var, [`SecretError::FileIo`]
/// for an unreadable file, [`SecretError::BackendUnavailable`] for vault/awssm
/// references (resolution not yet implemented), and [`SecretError::Malformed`]
/// for an unrecognized `urn:`/`arn:` value.
pub fn resolve_secret(raw: &str) -> Result<String, SecretError> {
    match parse_secret_ref(raw)? {
        SecretRef::Literal(s) => Ok(s.to_owned()),
        SecretRef::Env(name) => {
            std::env::var(name).map_err(|_| SecretError::EnvUnset(name.to_owned()))
        }
        SecretRef::File(path) => match std::fs::read_to_string(path) {
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
        },
        SecretRef::Vault(_) => Err(SecretError::BackendUnavailable {
            backend: "vault",
            reference: raw.to_owned(),
        }),
        SecretRef::AwsSecretsManager(_) => Err(SecretError::BackendUnavailable {
            backend: "awssm",
            reference: raw.to_owned(),
        }),
    }
}

/// Validate a secret reference's FORMAT only — NO env/file/network access.
///
/// For `--check-config` dry runs. Literals and well-formed refs are `Ok`; a
/// malformed `urn:`/`arn:` is an error. A well-formed `urn:zeroship:file:/missing`
/// path is `Ok` (format is valid; existence is NOT checked here). Vault and AWS
/// Secrets Manager references are also `Ok` (their format is valid even though
/// resolution is not implemented yet).
///
/// # Errors
///
/// Returns [`SecretError::Malformed`] when a `urn:`/`arn:` value is not a
/// recognized reference. Performs no side effects.
pub fn validate_secret_ref(raw: &str) -> Result<(), SecretError> {
    parse_secret_ref(raw).map(|_| ())
}

/// True when `raw` is a (non-literal) secret REFERENCE.
///
/// A malformed reference still counts as a reference: it starts with a reserved
/// `urn:`/`arn:` prefix, so [`parse_secret_ref`] returns an error rather than a
/// [`SecretRef::Literal`]. Only a value that parses cleanly as a literal is "not
/// a reference".
#[must_use]
pub fn is_secret_ref(raw: &str) -> bool {
    !matches!(parse_secret_ref(raw), Ok(SecretRef::Literal(_)))
}

/// Resolve a secret input for real startup, exiting(1) with a uniform message on
/// failure.
///
/// The single binary-boundary exit point for secret resolution (mirrors
/// `bootstrap_or_exit`). Use on the REAL boot path, NOT during `--check-config`
/// — it performs side effects (env/file read, future network fetch). For a dry
/// run use [`validate_secret_ref_or_exit`].
#[must_use]
pub fn resolve_secret_or_exit(label: &str, raw: &str) -> String {
    match resolve_secret(raw) {
        Ok(v) => v,
        Err(e) => {
            let m = format!("config: {label}: {e}");
            tracing::error!("{m}");
            eprintln!("{m}");
            std::process::exit(1);
        }
    }
}

/// Validate a secret reference's FORMAT for a `--check-config` dry run (no fetch),
/// exiting(1) on a malformed reference.
///
/// Mirrors [`resolve_secret_or_exit`] for the check-config path: it never reads
/// env/file/network, so the caller keeps using `raw` as the (unresolved)
/// check-config value.
pub fn validate_secret_ref_or_exit(label: &str, raw: &str) {
    if let Err(e) = validate_secret_ref(raw) {
        let m = format!("config: {label}: {e}");
        tracing::error!("{m}");
        eprintln!("{m}");
        std::process::exit(1);
    }
}

/// Obtain a secret with precedence CLI/env > `[secrets]` file (reference-only) > default.
///
/// `cli` is the clap-merged CLI/env value (`""` when unset). `file` is the optional
/// `[secrets]` overlay value, which MUST be a `urn:`/`arn:` reference — a literal there
/// is rejected (the config file must never carry a plaintext secret). With
/// `check_config` true, validates the resulting reference FORMAT only (no env/file/
/// network read) and returns the raw value; otherwise resolves it. Exits(1) on any
/// error (fail-closed).
#[must_use]
pub fn obtain_secret(label: &str, cli: &str, file: Option<&str>, check_config: bool) -> String {
    let raw = if !cli.is_empty() {
        cli.to_string()
    } else if let Some(f) = file.filter(|f| !f.trim().is_empty()) {
        // An empty/whitespace `[secrets]` entry means "no override" (fall through to
        // the compiled default), NOT a rejected literal.
        if !is_secret_ref(f) {
            let m = format!("config: {label}: a secret in the [secrets] config section must be a urn:/arn: reference, not a literal value");
            tracing::error!("{m}");
            eprintln!("{m}");
            std::process::exit(1);
        }
        f.to_string()
    } else {
        String::new()
    };
    if check_config {
        validate_secret_ref_or_exit(label, &raw);
        raw
    } else {
        resolve_secret_or_exit(label, &raw)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::Mutex;

    use base64::Engine as _;

    use super::{
        decoded_master_key_len, is_loopback_url, is_secret_ref, obtain_secret, parse_secret_ref,
        require_unless_dev, resolve_secret, validate_master_key_material, validate_secret_ref,
        validate_stash_key, SecretError, SecretRef, DEV_STASH_SIGNING_KEY,
    };

    // `std::env::set_var` mutates process-global state; serialize the env-touching
    // tests behind a mutex so parallel test threads don't race each other.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

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
    }

    #[test]
    fn env_reference_resolves_and_reports_unset() {
        let _guard = ENV_GUARD.lock().expect("env guard");
        let key = format!("ZEROSHIP_TEST_SECRET_ENV_{}", std::process::id());
        let reference = format!("urn:zeroship:env:{key}");

        // edition 2021: set_var is safe (no unsafe block; workspace denies unsafe_code).
        std::env::set_var(&key, "s3cr3t-from-env");
        assert_eq!(
            parse_secret_ref(&reference).expect("env ref parses"),
            SecretRef::Env(&key)
        );
        assert_eq!(
            resolve_secret(&reference).expect("env var resolves"),
            "s3cr3t-from-env"
        );

        std::env::remove_var(&key);
        match resolve_secret(&reference) {
            Err(SecretError::EnvUnset(name)) => assert_eq!(name, key),
            other => panic!("expected EnvUnset, got {other:?}"),
        }
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
    }

    #[test]
    fn vault_and_awssm_parse_but_are_unavailable() {
        // vault urn
        assert_eq!(
            parse_secret_ref("urn:zeroship:vault:secret/data/app#token").expect("vault parses"),
            SecretRef::Vault("secret/data/app#token")
        );
        match resolve_secret("urn:zeroship:vault:secret/data/app") {
            Err(SecretError::BackendUnavailable { backend, reference }) => {
                assert_eq!(backend, "vault");
                assert_eq!(reference, "urn:zeroship:vault:secret/data/app");
            }
            other => panic!("expected BackendUnavailable(vault), got {other:?}"),
        }

        // awssm urn form
        assert_eq!(
            parse_secret_ref("urn:zeroship:awssm:prod/db").expect("awssm parses"),
            SecretRef::AwsSecretsManager("prod/db")
        );
        match resolve_secret("urn:zeroship:awssm:prod/db") {
            Err(SecretError::BackendUnavailable { backend, .. }) => assert_eq!(backend, "awssm"),
            other => panic!("expected BackendUnavailable(awssm), got {other:?}"),
        }

        // arn form keeps the whole ARN.
        let arn = "arn:aws:secretsmanager:us-east-1:123:secret:x";
        assert_eq!(
            parse_secret_ref(arn).expect("arn parses"),
            SecretRef::AwsSecretsManager(arn)
        );
        match resolve_secret(arn) {
            Err(SecretError::BackendUnavailable { backend, reference }) => {
                assert_eq!(backend, "awssm");
                assert_eq!(reference, arn);
            }
            other => panic!("expected BackendUnavailable(awssm), got {other:?}"),
        }
    }

    #[test]
    fn malformed_references_are_rejected() {
        for bad in [
            "urn:bogus:x",
            "urn:zeroship:nope:x",
            "urn:zeroship:env:", // recognized scheme, empty body
            "arn:aws:secretsmanager:", // recognized ARN prefix, empty body
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
    fn is_secret_ref_classifies_by_reserved_prefix() {
        // Recognized references are references.
        assert!(is_secret_ref("urn:zeroship:env:MY_VAR"));
        assert!(is_secret_ref("urn:zeroship:file:/etc/secret"));
        assert!(is_secret_ref("urn:zeroship:vault:secret/x"));
        assert!(is_secret_ref("urn:zeroship:awssm:prod/db"));
        assert!(is_secret_ref("arn:aws:secretsmanager:us-east-1:123:secret:x"));

        // Malformed urn:/arn: values still count as references (they don't parse
        // as a literal).
        assert!(is_secret_ref("urn:bogus:x"));
        assert!(is_secret_ref("urn:zeroship:nope:x"));
        assert!(is_secret_ref("urn:zeroship:env:")); // recognized scheme, empty body
        assert!(is_secret_ref("arn:aws:s3:::bucket"));

        // Literals (including a colon-laden DSN) are NOT references.
        assert!(!is_secret_ref("hunter2hunter2hunter2hunter2hunter2"));
        assert!(!is_secret_ref("postgres://u:p@h/db"));
    }

    #[test]
    fn validate_secret_ref_performs_no_side_effects() {
        // Validating an env reference must NOT read the env: an unset var is still
        // format-Ok (format only, never resolution).
        validate_secret_ref("urn:zeroship:env:DEFINITELY_UNSET_VAR")
            .expect("unset env ref is format-valid");

        // Vault / awssm are format-valid even though resolution is unimplemented.
        validate_secret_ref("urn:zeroship:vault:secret/x").expect("vault format-valid");
        validate_secret_ref("urn:zeroship:awssm:prod/x").expect("awssm format-valid");
        validate_secret_ref("arn:aws:secretsmanager:us-east-1:123:secret:x")
            .expect("arn format-valid");

        // Malformed still propagates from validate.
        assert!(matches!(
            validate_secret_ref("urn:zeroship:nope:x"),
            Err(SecretError::Malformed(_))
        ));
    }

    // obtain_secret: CLI/env value (non-empty) wins over any file reference and
    // is returned verbatim. Use check_config so we never touch env/file.
    #[test]
    fn obtain_secret_cli_beats_file_ref() {
        let out = obtain_secret(
            "MASTER_KEY",
            "literal-from-cli",
            Some("urn:zeroship:env:SHOULD_BE_IGNORED"),
            true,
        );
        assert_eq!(out, "literal-from-cli");
    }

    // obtain_secret: empty CLI + a file reference => the reference is used. In
    // check-config mode it is returned raw (no resolution).
    #[test]
    fn obtain_secret_empty_cli_uses_file_ref_check_config() {
        let out = obtain_secret(
            "MASTER_KEY",
            "",
            Some("urn:zeroship:env:SOME_VAR"),
            true,
        );
        assert_eq!(out, "urn:zeroship:env:SOME_VAR");
    }

    // obtain_secret: empty CLI + a file env-reference => resolves the env var in
    // non-check mode (real boot path).
    #[test]
    fn obtain_secret_empty_cli_resolves_file_env_ref() {
        let _guard = ENV_GUARD.lock().expect("env guard");
        let key = format!("ZEROSHIP_TEST_OBTAIN_ENV_{}", std::process::id());
        let reference = format!("urn:zeroship:env:{key}");

        std::env::set_var(&key, "resolved-from-env");
        let out = obtain_secret("MASTER_KEY", "", Some(&reference), false);
        std::env::remove_var(&key);

        assert_eq!(out, "resolved-from-env");
    }

    // obtain_secret: empty CLI + a file file-reference => resolves the file
    // contents in non-check mode.
    #[test]
    fn obtain_secret_empty_cli_resolves_file_file_ref() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("zeroship_obtain_secret_{}.txt", std::process::id()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _cleanup = Cleanup(path.clone());
        {
            let mut f = std::fs::File::create(&path).expect("create temp secret");
            f.write_all(b"secret-from-file\n").expect("write secret");
        }
        let reference = format!("urn:zeroship:file:{}", path.to_str().expect("utf8 path"));

        let out = obtain_secret("MASTER_KEY", "", Some(&reference), false);
        assert_eq!(out, "secret-from-file");
    }

    // obtain_secret: empty CLI + no file => empty string (caller's default logic
    // takes over). Holds in both check and non-check modes (an empty value is a
    // valid literal, not a reference, so resolution is a no-op passthrough).
    #[test]
    fn obtain_secret_empty_cli_empty_file_is_empty() {
        assert_eq!(obtain_secret("MASTER_KEY", "", None, true), "");
        assert_eq!(obtain_secret("MASTER_KEY", "", None, false), "");
    }

    #[test]
    fn obtain_secret_empty_file_value_is_no_override() {
        // An empty/whitespace `[secrets]` entry means "no override" (fall through to
        // the default), NOT a rejected literal. Pre-fix this hit the literal-reject
        // exit path and would have aborted the test process.
        assert_eq!(obtain_secret("MASTER_KEY", "", Some(""), false), "");
        assert_eq!(obtain_secret("MASTER_KEY", "", Some("   "), true), "");
    }

    // obtain_secret: the literal-in-file rejection path calls std::process::exit,
    // which cannot run in-process. Instead, pin the predicate that gates it:
    // a plaintext literal in `[secrets]` is NOT a reference (=> rejected), while
    // a urn:/arn: value IS a reference (=> accepted).
    #[test]
    fn obtain_secret_file_literal_is_the_rejected_path() {
        // A plaintext literal is not a secret reference: obtain_secret would exit.
        assert!(!is_secret_ref("plainsecret"));
        // A DSN-looking literal (colons) is still a literal, not a reference.
        assert!(!is_secret_ref("postgres://u:p@h/db"));
        // A urn:/arn: value is a reference and is accepted by obtain_secret.
        assert!(is_secret_ref("urn:zeroship:env:X"));
        assert!(is_secret_ref("urn:zeroship:vault:secret/x"));
        assert!(is_secret_ref("arn:aws:secretsmanager:us-east-1:123:secret:x"));
    }

    // obtain_secret: a well-formed file reference is accepted as a reference
    // (check-config returns it raw without resolution).
    #[test]
    fn obtain_secret_accepts_env_ref_as_reference() {
        let out = obtain_secret("CONTROL_KEY", "", Some("urn:zeroship:env:X"), true);
        assert_eq!(out, "urn:zeroship:env:X");
    }
}
