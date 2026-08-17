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

/// Validate the dedicated platform-token mint bearer.
///
/// The key crosses a service boundary and authorizes access-token issuance, so
/// it must contain at least 32 bytes of secret material. Leading and trailing
/// whitespace is ignored because both mint consumers treat it as transport
/// padding rather than credential material.
///
/// # Errors
///
/// Returns an explanatory error when `value` is empty or shorter than 32 bytes
/// after trimming transport whitespace.
pub fn validate_platform_mint_key(label: &str, value: &str) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{label} is required; set a strong (>=32 byte) value"));
    }

    if value.len() < 32 {
        return Err(format!(
            "{label} is too short ({} bytes); minimum 32 bytes",
            value.len()
        ));
    }

    Ok(())
}

/// Validate the shared requirement for a stash signing key.
///
/// The single stash validator across every binary: non-empty, raw UTF-8 length
/// of at least 32 bytes.
///
/// `label` is the caller's operator-facing spelling. It is a PARAMETER because
/// this validator is shared by two binaries that read two DIFFERENT variables -
/// gateway reads `gateway.stash_signing_key` and auth reads
/// `auth.stash_signing_key` - so no single name baked in here can be right for
/// both. It previously interpolated a bare `STASH_SIGNING_KEY`, which was right
/// for neither and named nothing a binary reads.
///
/// # Errors
///
/// Returns an explanatory error when `value` is a known public development
/// value, empty, or shorter than 32 bytes.
pub fn validate_stash_key(label: &str, value: &str) -> Result<(), String> {
    reject_known_weak(label, value, KNOWN_WEAK_STASH_KEYS)?;
    if value.is_empty() {
        return Err(format!("{label} is required; set a strong (>=32 byte) value"));
    }

    if value.len() < 32 {
        return Err(format!(
            "{label} is too short ({} bytes); minimum 32 bytes",
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
/// `label` is the caller's operator-facing spelling, for the same reason as
/// [`validate_stash_key`]: this module must not bake in a name, because a name
/// baked in here is invisible to the config contract and rots the moment the
/// declaration is renamed. The bare `PAIRWISE_SALT` it used to interpolate had
/// already stopped being settable.
///
/// # Errors
///
/// Returns an explanatory error when `value` is a known public development
/// value, empty, or shorter than 32 bytes.
pub fn validate_pairwise_salt(label: &str, value: &str) -> Result<(), String> {
    reject_known_weak(label, value, KNOWN_WEAK_PAIRWISE_SALTS)?;
    if value.is_empty() {
        return Err(format!(
            "{label} is required; set a strong (>=32 byte) value \
             (identical on gateway + control, never rotated without a migration)"
        ));
    }

    if value.len() < 32 {
        return Err(format!(
            "{label} is too short ({} bytes); minimum 32 bytes",
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
/// `label` is the caller's operator-facing spelling, for the same reason as
/// [`validate_stash_key`]. The bare `WORKER_KEY` it used to interpolate is not
/// settable: the identity is declared in `crates/config-macros/src/shared.rs`
/// and projects to `ZEROSHIP_WORKER_KEY`.
///
/// # Errors
///
/// Returns an explanatory error when `value` is a known public development
/// value, empty, or shorter than 32 bytes.
pub fn validate_worker_key(label: &str, value: &str) -> Result<(), String> {
    reject_known_weak(label, value, KNOWN_WEAK_WORKER_KEYS)?;
    if value.is_empty() {
        return Err(format!("{label} is required; set a strong (>=32 byte) value"));
    }

    if value.len() < 32 {
        return Err(format!(
            "{label} is too short ({} bytes); minimum 32 bytes",
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

/// Run a strength validator against a resolved secret's material.
///
/// The single bridge between [`crate::config::Secret`] and the `&str`
/// validators above, so every binary answers "does this secret still get
/// validated" the same way. Three cases, and only the middle one is new:
///
/// * material present (any real boot, and a `--check-config` run whose secret
///   is an in-memory literal) - the validator runs on the real material;
/// * configured but unread - only reachable under `--check-config` for a source
///   that would need I/O. There is nothing to check, and checking the reference
///   TEXT instead of the secret is what the old `is_secret_ref` dance did;
/// * unsupplied - the validator runs on `""`, which is how each one already
///   produces its own "X is required" message rather than a generic one.
///
/// # Errors
///
/// Propagates the validator's message unchanged.
pub fn validate_secret_material<F>(
    secret: &crate::config::Secret<String>,
    validate: F,
) -> Result<(), String>
where
    F: FnOnce(&str) -> Result<(), String>,
{
    match secret.expose_secret() {
        Some(material) => validate(material),
        None if secret.is_configured() => Ok(()),
        None => validate(""),
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
    /// A secret file is readable or writable by group or other.
    ///
    /// Distinct from [`SecretError::FileIo`] because "could not read" sends an
    /// operator looking for a missing file or a bad mount, and the fix here is
    /// a `chmod`. Neither the path nor the mode is secret material.
    #[error("secret file '{path}' has mode {mode:04o}; group and other permissions must be zero (chmod 600 '{path}')")]
    InsecurePermissions {
        /// The offending path.
        path: String,
        /// The full st_mode, so the refusal names what it actually saw.
        mode: u32,
    },
    /// A secret file's permissions could not be determined, so the owner-only
    /// policy cannot be enforced on it.
    #[error("secret file '{path}' cannot be permission-checked on this platform, so owner-only access cannot be enforced; secret files require a unix filesystem")]
    UndeterminableMode {
        /// The path whose mode is unknown.
        path: String,
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

/// Read a secret file, enforcing owner-only permissions and stripping exactly
/// one trailing line ending.
///
/// Shared by the `urn:zeroship:file:` reference arm and the generated `-file`
/// path flag, so the two spellings of "the secret is in this file" cannot
/// disagree about either the permission policy or trailing whitespace.
///
/// THE POLICY IS REFUSE, not warn. `zeroship dev init` writes 0600 and until
/// this function checked, nothing ever re-verified it: a later `chmod`, a file
/// restored from a backup, a checkout, or a docker bind mount with permissive
/// host modes silently downgraded every credential in the deployment with no
/// signal at all. Refusing is what ssh does with a private key and what the
/// four loaders in this tree that already check do
/// (`crates/gateway/src/signing.rs`, `crates/auth/src/oidc/{refresh,signing}.rs`,
/// `crates/authn/src/lib.rs`, all `mode & 0o077 != 0`); a warning in a boot log
/// is a signal nobody reads.
///
/// The mode is taken from the OPEN HANDLE, not from the path. Stat-then-open
/// leaves a window in which the file checked is not the file read; `fstat` on
/// the descriptor that produced the bytes cannot be raced that way.
///
/// # Errors
///
/// Returns [`SecretError::FileIo`] when the file cannot be opened or read,
/// [`SecretError::InsecurePermissions`] when it is group- or other-accessible,
/// and [`SecretError::UndeterminableMode`] when the mode is unavailable.
pub fn read_secret_file(path: &str) -> Result<String, SecretError> {
    use std::io::Read as _;

    let io = |source: std::io::Error| SecretError::FileIo {
        path: path.to_owned(),
        source,
    };

    let mut file = std::fs::File::open(path).map_err(io)?;
    enforce_owner_only(path, file_mode(&file.metadata().map_err(io)?))?;

    let mut contents = String::new();
    file.read_to_string(&mut contents).map_err(io)?;

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

/// The permission bits that must be clear on a secret file: any group or other
/// access at all, not merely read. Write and execute are included because a
/// group-writable credential is a credential a second account can replace.
const GROUP_AND_OTHER_BITS: u32 = 0o077;

/// The mode of an open file, or `None` where the platform has no such concept.
#[cfg(unix)]
fn file_mode(metadata: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt as _;
    Some(metadata.permissions().mode())
}

#[cfg(not(unix))]
fn file_mode(_metadata: &std::fs::Metadata) -> Option<u32> {
    None
}

/// Apply the owner-only policy to a mode that may not be knowable.
///
/// Split out from [`file_mode`] so the `None` arm is reachable from a unix test
/// run. A permission check that quietly returns `Ok` where it cannot measure
/// anything is worse than no check, because it reads as coverage on exactly the
/// platform nobody verified - so this FAILS CLOSED instead. There is no opt out:
/// the platform is io_uring-only and has no non-unix deployment to accommodate.
fn enforce_owner_only(path: &str, mode: Option<u32>) -> Result<(), SecretError> {
    match mode {
        Some(mode) if mode & GROUP_AND_OTHER_BITS != 0 => Err(SecretError::InsecurePermissions {
            path: path.to_owned(),
            mode,
        }),
        Some(_) => Ok(()),
        None => Err(SecretError::UndeterminableMode {
            path: path.to_owned(),
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

    use crate::config::{Secret, SourceKind};

    use super::{
        decoded_master_key_len, enforce_owner_only, is_loopback_url, parse_secret_ref,
        read_secret_file, require_nonempty,
        validate_platform_mint_key, validate_secret_material,
        resolve_secret, validate_master_key_material, validate_pairwise_salt, validate_secret_ref,
        validate_stash_key, validate_worker_key, SecretError, SecretRef, KNOWN_WEAK_MASTER_KEYS,
        KNOWN_WEAK_PAIRWISE_SALTS, KNOWN_WEAK_STASH_KEYS, KNOWN_WEAK_WORKER_KEYS,
    };

    #[test]
    fn require_nonempty_rejects_missing_values() {
        let err = require_nonempty("ZEROSHIP_CONTROL_KEY / --control-key-file", "")
            .expect_err("missing secret");
        assert_eq!(err, "ZEROSHIP_CONTROL_KEY / --control-key-file is required");
        require_nonempty("ZEROSHIP_CONTROL_KEY / --control-key-file", "secret").expect("present");
    }

    #[test]
    fn validate_platform_mint_key_requires_32_bytes() {
        assert!(validate_platform_mint_key(SENTINEL, "")
            .expect_err("empty mint key")
            .contains("required"));
        assert!(validate_platform_mint_key(SENTINEL, "short")
            .expect_err("short mint key")
            .contains("minimum 32 bytes"));
        validate_platform_mint_key(SENTINEL, "0123456789abcdef0123456789abcdef")
            .expect("strong mint key");
    }

    /// A label no declaration could ever produce, so a message that carries it
    /// can only have got it from the caller.
    const SENTINEL: &str = "ZEROSHIP_SENTINEL_LABEL";

    /// Make a fixture readable only by its owner, which every secret file this
    /// module reads must be. `std::fs::write` leaves 0644 under the usual 022
    /// umask, and `read_secret_file` refuses that.
    fn owner_only(path: &std::path::Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .expect("chmod 0600");
        }
        #[cfg(not(unix))]
        let _ = path;
    }

    // S4: one unified stash validator. Empty and short keys are rejected and
    // values of at least 32 bytes are accepted.
    #[test]
    fn validate_stash_key_unified() {
        for weak in KNOWN_WEAK_STASH_KEYS {
            assert!(
                validate_stash_key(SENTINEL, weak)
                    .expect_err("known public stash key")
                    .contains("known public")
            );
        }

        // empty rejected
        assert!(validate_stash_key(SENTINEL, "")
            .expect_err("empty key")
            .contains("required"));

        // short (<32) rejected
        assert!(validate_stash_key(SENTINEL, "short")
            .expect_err("short key")
            .contains("too short"));

        // exactly 32 ok
        validate_stash_key(SENTINEL, "0123456789abcdef0123456789abcdef").expect("strong key");
    }

    /// THE DEFECT. Three validators interpolated a bare `STASH_SIGNING_KEY`,
    /// `PAIRWISE_SALT` and `WORKER_KEY` into their refusals. None of those is a
    /// variable any binary reads: the shared identities in
    /// `crates/config-macros/src/shared.rs` project to `ZEROSHIP_WORKER_KEY`
    /// and `ZEROSHIP_PAIRWISE_SALT`, and the stash key is not shared at all -
    /// it is `gateway.stash_signing_key` and `auth.stash_signing_key`, two
    /// different variables behind one validator. An operator who followed any
    /// of these refusals set a variable the binary does not read.
    ///
    /// The fix is that this module names NOTHING. Every refusal carries only
    /// the caller's label, so the operator-facing spelling lives next to the
    /// declaration it must agree with, where each binary's own diagnostic test
    /// checks it against the set of names that binary really reads.
    #[test]
    fn every_refusal_names_the_callers_label_and_invents_no_name_of_its_own() {
        // Drive every message-producing arm of every labelled validator.
        let messages: Vec<String> = [
            validate_stash_key(SENTINEL, KNOWN_WEAK_STASH_KEYS[0]),
            validate_stash_key(SENTINEL, ""),
            validate_stash_key(SENTINEL, "short"),
            validate_pairwise_salt(SENTINEL, KNOWN_WEAK_PAIRWISE_SALTS[0]),
            validate_pairwise_salt(SENTINEL, ""),
            validate_pairwise_salt(SENTINEL, "short"),
            validate_worker_key(SENTINEL, KNOWN_WEAK_WORKER_KEYS[0]),
            validate_worker_key(SENTINEL, ""),
            validate_worker_key(SENTINEL, "short"),
            validate_master_key_material(SENTINEL, KNOWN_WEAK_MASTER_KEYS[0]),
            validate_master_key_material(SENTINEL, "YWJj"),
            validate_master_key_material(SENTINEL, "not!base64!"),
            validate_platform_mint_key(SENTINEL, ""),
            validate_platform_mint_key(SENTINEL, "short"),
            require_nonempty(SENTINEL, ""),
        ]
        .into_iter()
        .map(|result| result.expect_err("each input above must be refused"))
        .collect();

        for message in &messages {
            let tokens = crate::config::env_like_tokens(message);
            assert_eq!(
                tokens,
                vec![SENTINEL.to_owned()],
                "refusal {message:?} must name the caller's label and nothing else; \
                 a name spelled inside this module is invisible to the config \
                 contract and rots when the declaration is renamed"
            );
        }

        // The one-variable partner. Same scanner, same messages, only the label
        // changes - and a DIFFERENT label must show through, so the assertion
        // above cannot be passing because the scanner sees nothing or because
        // the messages are constant.
        let other = "ZEROSHIP_OTHER_LABEL";
        assert_eq!(
            crate::config::env_like_tokens(
                &validate_worker_key(other, "short").expect_err("short key")
            ),
            vec![other.to_owned()]
        );

        // Does NOT cover whether the label a given binary passes is a name that
        // binary actually reads. That is per-binary wiring, asserted by each
        // binary's own `every_startup_diagnostic_names_a_variable_*_reads` test.
    }

    /// MAJOR fix (pairwise_salt secret lifecycle) — the dedicated pairwise-salt
    /// secret has its OWN validator with the SAME strength posture as the stash
    /// key while remaining a distinct input with its own lifecycle.
    #[test]
    fn validate_pairwise_salt_enforces_strength() {
        for weak in KNOWN_WEAK_PAIRWISE_SALTS {
            assert!(
                validate_pairwise_salt(SENTINEL, weak)
                    .expect_err("known public pairwise salt")
                    .contains("known public")
            );
        }

        // empty rejected, short rejected, exactly 32 ok
        assert!(validate_pairwise_salt(SENTINEL, "")
            .expect_err("empty")
            .contains("required"));
        assert!(validate_pairwise_salt(SENTINEL, "short")
            .expect_err("short")
            .contains("too short"));
        validate_pairwise_salt(SENTINEL, "0123456789abcdef0123456789abcdef").expect("strong salt");
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
                validate_worker_key(SENTINEL, weak)
                    .expect_err("known public worker key")
                    .contains("known public")
            );
        }

        // empty rejected (an empty key would HMAC-verify against a zero-length
        // key any party can compute)
        assert!(validate_worker_key(SENTINEL, "")
            .expect_err("empty key")
            .contains("required"));

        // short (<32) rejected
        assert!(validate_worker_key(SENTINEL, "short")
            .expect_err("short key")
            .contains("too short"));

        // exactly 32 ok
        validate_worker_key(SENTINEL, "0123456789abcdef0123456789abcdef").expect("strong key");
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


    // The bridge every binary uses to keep its strength validators running on
    // the RESOLVED material. Each arm is paired with a control differing in one
    // variable, because the interesting failure is the middle one silently
    // swallowing a real boot.
    #[test]
    fn a_strength_validator_runs_on_material_and_skips_only_the_unread_case() {
        let seen = std::cell::RefCell::new(Vec::new());
        let record = |value: &str| -> Result<(), String> {
            seen.borrow_mut().push(value.to_owned());
            if value.len() >= 32 { Ok(()) } else { Err(format!("too short: {}", value.len())) }
        };

        // 1. Material present: the validator sees the real material.
        let strong = "0123456789abcdef0123456789abcdef";
        validate_secret_material(
            &Secret::supplied(SourceKind::Env, Some(strong.to_owned())),
            record,
        )
        .expect("strong material passes");
        assert_eq!(seen.borrow().as_slice(), [strong.to_owned()]);

        // 1b. The one-variable partner: weak material still FAILS, so arm 1 is
        // not passing because the validator was never called.
        seen.borrow_mut().clear();
        validate_secret_material(
            &Secret::supplied(SourceKind::Env, Some("short".to_owned())),
            record,
        )
        .expect_err("weak material is rejected");
        assert_eq!(seen.borrow().as_slice(), ["short".to_owned()]);

        // 2. Configured but unread - only a dry run reaches this. Nothing to
        // check, so the validator must not be called at all.
        seen.borrow_mut().clear();
        validate_secret_material(&Secret::supplied(SourceKind::CliFile, None), record)
            .expect("an unread secret is not judged");
        assert!(seen.borrow().is_empty(), "the validator must not run on nothing");

        // 3. Unsupplied: the validator runs on "" and produces its own message.
        seen.borrow_mut().clear();
        let error = validate_secret_material(&Secret::<String>::absent(), record)
            .expect_err("an unset secret is rejected");
        assert_eq!(error, "too short: 0");
        assert_eq!(seen.borrow().as_slice(), [String::new()]);

        // Does NOT cover whether any given binary actually calls this. That is
        // per-binary wiring, asserted by each binary's own boot-guard tests.
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
        // A secret file must be owner-only to be readable at all; the default
        // umask here would leave it 0644 and the resolve below would refuse it.
        owner_only(&path);
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

        // Does NOT cover file PERMISSIONS beyond needing them owner-only to get
        // this far; the policy itself is
        // `a_group_or_world_accessible_secret_file_is_refused`.
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
        owner_only(&path);
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

    /// THE READ-SIDE DEFECT. Nothing in this platform permission-checked a
    /// secret file on READ. `zeroship dev init` writes 0600 and no later reader
    /// re-verified it, so a chmod, a restore from backup, a checkout, or a
    /// docker bind mount with permissive host modes silently downgraded every
    /// credential in the deployment with no signal at all.
    ///
    /// The policy is REFUSE, not warn. It is what ssh does with a private key,
    /// it is what the two loaders in this tree that already check
    /// (`crates/gateway/src/signing.rs` and `crates/auth/src/oidc/refresh.rs`,
    /// both `mode & 0o077 != 0`) already do, and a warning in a boot log is a
    /// signal nobody reads.
    #[cfg(unix)]
    #[test]
    fn a_group_or_world_accessible_secret_file_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("zs_secret_mode_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tempdir");
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(dir.clone());

        let write = |name: &str, mode: u32| -> String {
            let path = dir.join(name);
            std::fs::write(&path, b"file-secret-value\n").expect("write secret");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                .expect("chmod");
            path.to_str().expect("utf8 path").to_owned()
        };

        // Every mode that lets a second local account read the credential.
        for mode in [0o644, 0o640, 0o604, 0o666, 0o660, 0o606, 0o700 | 0o004] {
            let path = write(&format!("insecure_{mode:o}"), mode);
            match read_secret_file(&path) {
                Err(SecretError::InsecurePermissions { path: p, mode: m }) => {
                    assert_eq!(p, path);
                    assert_eq!(m & 0o777, mode, "the refusal must report the real mode");
                }
                other => panic!("mode {mode:o} must be refused, got {other:?}"),
            }
        }

        // THE ONE-VARIABLE PARTNER: the same file, the same content, the same
        // reader, only the mode differs - and owner-only is accepted with the
        // contents intact. Without this the loop above would also pass if
        // `read_secret_file` had simply started failing on everything.
        let ok = write("owner_only", 0o600);
        assert_eq!(read_secret_file(&ok).expect("0600 is accepted"), "file-secret-value");
        let ok_x = write("owner_only_exec", 0o700);
        assert_eq!(read_secret_file(&ok_x).expect("0700 is accepted"), "file-secret-value");

        // The refusal reaches the two public entry points, not just the helper.
        let insecure = write("insecure_via_urn", 0o644);
        assert!(matches!(
            resolve_secret(&format!("urn:zeroship:file:{insecure}")),
            Err(SecretError::InsecurePermissions { .. })
        ));

        // Does NOT cover the DIRECTORY holding the file: a 0600 secret under a
        // 0777 directory is still accepted here, because directory mode governs
        // rename/unlink rather than read of an existing file.
    }

    /// THE TRAP. A mode check that quietly does nothing where the mode cannot
    /// be determined is worse than no check, because it reads as coverage.
    /// `enforce_owner_only` therefore FAILS CLOSED on `None` - the value the
    /// non-unix arm of `file_mode` produces - rather than returning `Ok`.
    ///
    /// The decision is split out from the platform layer precisely so this arm
    /// is reachable from a unix test run. Every other permission check in this
    /// tree has a `#[cfg(not(unix))] fn ... { Ok(()) }` arm that nothing
    /// exercises; this one is asserted.
    #[test]
    fn an_undeterminable_mode_fails_closed() {
        match enforce_owner_only("/some/secret", None) {
            Err(SecretError::UndeterminableMode { path }) => assert_eq!(path, "/some/secret"),
            other => panic!("an undeterminable mode must be refused, got {other:?}"),
        }

        // The one-variable partners: same call, same path, only the mode
        // differs. A determinable owner-only mode passes and a determinable
        // group-readable mode is refused, so the assertion above cannot be
        // passing because the function refuses everything.
        enforce_owner_only("/some/secret", Some(0o100_600)).expect("owner-only passes");
        assert!(matches!(
            enforce_owner_only("/some/secret", Some(0o100_640)),
            Err(SecretError::InsecurePermissions { .. })
        ));
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
