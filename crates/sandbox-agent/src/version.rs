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
    "auth.bearer-file",      // token provisioned via file mount, not env
];

/// Agent crate version (`Cargo.toml`).
pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short git commit at build time. `"unknown"` if the build wasn't in
/// a git checkout.
pub const GIT_COMMIT: &str = env!("AGENT_GIT_COMMIT");
