//! Version + capability reporting for [`crate::handlers::version`].
//!
//! The controller calls `/version` at session-create time (and may
//! cache for the lifetime of the session) to learn which protocol
//! features the agent supports. Adding a new endpoint or wire change
//! means **bumping `PROTOCOL_VERSION` and adding a capability string**
//! — old controllers gracefully feature-detect.

/// Wire-protocol version. Sent on every response as
/// `X-Sbx-Protocol: <N>`.
///
/// **Bump when a breaking change ships:**
///   - existing endpoint changes URL, method, or required field shape
///   - existing endpoint changes the meaning of a status code
///   - existing field is removed or its type changes incompatibly
///   - **auth scheme changes** (e.g., HMAC → mTLS)
///
/// **Do NOT bump for additive changes:**
///   - new endpoint added (announce via [`CAPABILITIES`])
///   - new optional response field added (older clients ignore it)
///   - new audit event kind
///
/// Controllers prefer feature-detection over version comparison —
/// see [`CAPABILITIES`] — but the protocol version is the single
/// breaking-change tripwire so we can't drift silently.
pub const PROTOCOL_VERSION: u32 = 1;

/// Capability strings, stable identifiers. Controllers do feature
/// detection by membership in this list, not by version comparison.
///
/// **Add new entries here when shipping new endpoints / behaviors.
/// Never remove or rename an existing capability** — it's a stable
/// contract. Deprecate by adding a successor and noting the old one
/// is unmaintained in docs.
pub const CAPABILITIES: &[&str] = &[
    "exec",                  // POST /exec
    "exec.timeout-output",   // /exec preserves partial output on timeout
    "exec.size-cap",         // /exec stdout/stderr capped, `truncated` flag
    "exec.env-isolation",    // SANDBOX_AGENT_* never visible to the child
    "files.crud",            // GET / PUT / DELETE /files/{path}*
    "files.tree",            // GET /tree
    "files.tree-truncated",  // /tree response includes `truncated: bool`
    "fs.no-symlink-escape",  // openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)
    "auth.hmac-v1",          // X-Sbx-{Timestamp,Nonce,Signature} HMAC-SHA256
];

/// Agent crate version (`Cargo.toml`).
pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short git commit at build time. `"unknown"` if the build wasn't in
/// a git checkout.
pub const GIT_COMMIT: &str = env!("AGENT_GIT_COMMIT");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_version_matches_cargo_pkg() {
        // Sanity: AGENT_VERSION reflects the crate's Cargo.toml.
        assert_eq!(AGENT_VERSION, env!("CARGO_PKG_VERSION"));
        assert!(!AGENT_VERSION.is_empty());
    }

    #[test]
    fn git_commit_present() {
        // build.rs writes either a short hash or "unknown" — both
        // are acceptable but the env var must be set.
        assert!(!GIT_COMMIT.is_empty());
    }

    #[test]
    fn protocol_version_is_1() {
        assert_eq!(PROTOCOL_VERSION, 1);
    }

    #[test]
    fn capabilities_nonempty_and_unique() {
        assert!(!CAPABILITIES.is_empty(), "must advertise at least one capability");
        let mut sorted: Vec<&str> = CAPABILITIES.to_vec();
        sorted.sort();
        let len_before = sorted.len();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            len_before,
            "duplicate entries in CAPABILITIES (each must be unique)",
        );
    }

    #[test]
    fn capability_strings_are_well_formed() {
        // Convention: `category.feature[-vN]` — lowercase ASCII +
        // digits, dots and dashes. Digits are allowed for version
        // suffixes like `auth.hmac-v1`.
        for cap in CAPABILITIES {
            assert!(!cap.is_empty(), "empty capability string");
            assert!(
                cap.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-'),
                "capability {cap:?} contains chars outside [a-z0-9.-]",
            );
            assert!(
                !cap.starts_with('.') && !cap.ends_with('.'),
                "capability {cap:?} has leading/trailing dot",
            );
        }
    }

    #[test]
    fn known_capabilities_present() {
        // Baseline v1 contract: removing any of these is a wire
        // breakage that would force PROTOCOL_VERSION to bump.
        let expected = [
            "exec",
            "files.crud",
            "files.tree",
            "fs.no-symlink-escape",
            "auth.hmac-v1",
        ];
        for e in expected {
            assert!(
                CAPABILITIES.contains(&e),
                "missing baseline v1 capability: {e}"
            );
        }
    }
}
