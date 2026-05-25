//! Version + capability reporting for [`crate::handlers::version`].
//!
//! The controller calls `/version` at session-create time (and may
//! cache for the lifetime of the session) to learn which protocol
//! features the agent supports. Adding a new endpoint or wire change
//! means **bumping `PROTOCOL_VERSION` and adding a capability string**.
//!
//! **Per-capability semantic is NOT uniform.** Some entries in
//! [`CAPABILITIES`] are genuinely feature-detected by the controller
//! (see [`crate::proxy_ws`] for `proxy.ws-v1`); others are listed for
//! diagnostic / version-pin visibility only and are treated as
//! mandatory by the controller. The current contract is: the
//! controller assumes a compatible agent on the other end (validated
//! at boot via the signed-probe handshake in
//! `crates/sandbox/src/backend/nomad_ch.rs::wait_for_agent_livez`),
//! and individual feature paths opt-in to actual negotiation when
//! there's a real downgrade scenario worth supporting. Treat
//! [`CAPABILITIES`] as a versioned implementation manifest, not a
//! universal negotiation surface — read the per-entry comment to see
//! which side it falls on.

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

/// Capability strings, stable identifiers.
///
/// **Add new entries here when shipping new endpoints / behaviors.
/// Never remove or rename an existing capability** — it's a stable
/// contract. Deprecate by adding a successor and noting the old one
/// is unmaintained in docs.
///
/// **Negotiation semantic — per entry, not uniform.** Some capabilities
/// (`proxy.ws-v1`, `auth.ed25519-v1.1`) are genuinely feature-detected
/// by the controller. Others are listed for diagnostic visibility but
/// are mandatory in practice — the controller does NOT branch on their
/// presence and would fail on the call path if a non-compliant agent
/// answered. The deployment story is "agent ≥ pinned version always
/// has the mandatory caps; older agents would fail elsewhere in the
/// signed handshake / envelope layer before this list mattered." Per-
/// entry comments below say which side each entry sits on. Adding a
/// new entry does NOT automatically make the controller feature-detect
/// it; the controller side has to be wired through deliberately.
pub const CAPABILITIES: &[&str] = &[
    "exec",                  // POST /exec
    "exec.timeout-output",   // /exec preserves partial output on timeout
    "exec.size-cap",         // /exec stdout/stderr capped, `truncated` flag
    "exec.env-isolation",    // SANDBOX_AGENT_* never visible to the child
    "files.crud",            // GET / PUT / DELETE /files/{path}*
    "files.tree",            // GET /tree
    "files.tree-truncated",  // /tree response includes `truncated: bool`
    "fs.no-symlink-escape",  // openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)
    "auth.ed25519-v1",       // X-Sbx-{Timestamp,Nonce,Signature} Ed25519
    "auth.ed25519-v1.1",     // v1.1 canonical (path+query, ED25519-V1.1 tag) for /proxy/...
    "auth.ed25519-v1.1-ws",  // v1.1-ws canonical (ED25519-V1.1-WS tag) for WS Upgrade (Phase 2)
    "proxy.http-v1",         // ANY /proxy/{port}/{path*} HTTP forward (Phase 1)
    "proxy.ws-v1",           // WS Upgrade on the dedicated compio listener (Phase 2)
    // Bug #22 fix (cluster smoke 2026-05-23): POST /_clock_resync —
    // signed handshake the controller issues post-CH-`--restore` to
    // repair the guest's frozen-at-snapshot CLOCK_REALTIME.
    //
    // **Mandatory (not feature-detected).** The controller at
    // `crates/sandbox/src/restore_handler.rs` calls
    // `clock_resync_post_restore` unconditionally on every wake — there
    // is no skip-with-fallback path. Listed here for diagnostic /
    // version-pin visibility. Pre-clock-resync agents would also lack
    // R8-A4 envelope handling and R8-DEPLOY1 sandbox_id binding, so
    // they'd fail upstream of this call anyway; making this
    // negotiable would be YAGNI plumbing for a downgrade scenario
    // that can't happen in practice. If a future agent ever needs
    // this resync to be skippable (e.g., a fast-path that avoids
    // CH-`--restore`'s clock freeze), wire feature-detection into
    // the controller at the same time.
    "clock.resync-v1",
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
            "auth.ed25519-v1",
        ];
        for e in expected {
            assert!(
                CAPABILITIES.contains(&e),
                "missing baseline v1 capability: {e}"
            );
        }
    }

    #[test]
    fn mandatory_clock_resync_v1_present() {
        // R7-API2 regression guard. `clock.resync-v1` is documented
        // as MANDATORY (not feature-detected) — the controller at
        // `crates/sandbox/src/restore_handler.rs` calls
        // `clock_resync_post_restore` unconditionally on every wake.
        // If someone removes the capability without simultaneously
        // wiring a feature-detect skip path on the controller side,
        // wake will start failing in the field. This test catches
        // the silent removal.
        assert!(
            CAPABILITIES.contains(&"clock.resync-v1"),
            "clock.resync-v1 is a mandatory controller-side capability; \
             removing it requires wiring controller feature-detection first \
             (see restore_handler.rs `clock_resync_post_restore`)",
        );
    }

    #[test]
    fn mandatory_proxy_ws_v1_present() {
        // R11-T3 regression guard, paired with
        // `mandatory_clock_resync_v1_present`. `proxy.ws-v1` is named
        // by the R7-API2 commit body and the const-level doc comment
        // as one of the genuinely feature-detected capabilities — the
        // controller (see `crates/sandbox-agent/src/proxy_ws.rs` and
        // the WS Upgrade path) keys behavior off its presence. Silent
        // removal without simultaneously updating the controller-side
        // detection would leave the controller branching on a string
        // that no agent advertises, masking a Phase-2 regression. The
        // capability list is a versioned implementation manifest, not
        // an arbitrary set; pin the entry.
        assert!(
            CAPABILITIES.contains(&"proxy.ws-v1"),
            "proxy.ws-v1 is a feature-detected capability; removing it \
             without updating controller-side detection (proxy_ws WS Upgrade \
             path) would silently break Phase 2 WS proxy",
        );
    }

    #[test]
    fn mandatory_auth_ed25519_v1_1_present() {
        // R11-T3 regression guard, paired with
        // `mandatory_clock_resync_v1_present`. `auth.ed25519-v1.1` is
        // the v1.1 canonical signing scheme (path+query, ED25519-V1.1
        // tag) used for `/proxy/...` requests and is named by the
        // const-level doc comment as feature-detected by the
        // controller. Silent removal without updating the
        // controller's signature-building code would either fall back
        // to v1 (wrong canonical form, signature mismatch on /proxy)
        // or break the call path entirely. Pin the entry so the
        // dependency is explicit.
        assert!(
            CAPABILITIES.contains(&"auth.ed25519-v1.1"),
            "auth.ed25519-v1.1 is a feature-detected capability; removing it \
             without updating controller-side signature canonicalization \
             would break /proxy request authentication",
        );
    }
}
