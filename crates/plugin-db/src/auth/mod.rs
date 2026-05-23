//! P8c — SECURITY DEFINER trust anchor + HMAC-signed session init
//! for the C1 reactive-query subsystem.
//!
//! ## What this module ships
//!
//! Per the zeroship-db proposal (R5-R8 of the review loop, line 285
//! onward), the C1 replication-slot owner and audit-table writer must
//! be a **platform service role** (`__zeroship_platform_role`), not the
//! per-app role. App roles get USAGE on a privileged admin schema and
//! EXECUTE on SECURITY DEFINER wrappers — never direct DML on slots,
//! publications, or audit tables.
//!
//! ### Stages
//!
//! 1. **Admin schema + platform role** (`bootstrap.rs`)
//!    `__zeroship_admin` schema owned by `__zeroship_platform_role`,
//!    plus `__zeroship_app_role_template` whose grants per-app roles
//!    inherit.
//!
//! 2. **SECURITY DEFINER wrappers** (`bootstrap.rs`)
//!    `__zeroship_admin.ensure_publication_and_slot(app)`,
//!    `__zeroship_admin.drop_abandoned_slots(threshold_bytes)`,
//!    `__zeroship_admin.watchdog()`.
//!
//! 3. **HMAC session init** (this file + `session.rs`)
//!    Per-session token minted by the platform via
//!    `__zeroship_admin.sign_session(actor_kind, actor_id, pid,
//!    nonce, expires_at)` — the raw HMAC key never leaves Postgres.
//!    Verification via `__zeroship_admin.init_session(actor_kind,
//!    actor_id, signature, nonce, expires_at)`; constant-time HMAC
//!    compare; nonce replay protection via
//!    `__zeroship_admin.session_nonces`.
//!
//! 4. **Key rotation primitives** (`keys.rs`)
//!    `__zeroship_admin.hmac_keys (key_id, secret, created_at,
//!    retired_at)`; `__zeroship_admin.rotate_session_keys()` retires
//!    `current` to `previous` and inserts a fresh one; verification
//!    function accepts both within the grace window. The maintenance
//!    cron that drives daily rotation is **deferred** (Stage 4
//!    partial — Rust primitives shipped, scheduling is the control
//!    plane's job).
//!
//! ## Bootstrap ceremony
//!
//! The very first HMAC secret is generated **inside Postgres** by
//! `pgcrypto.gen_random_bytes(32)` during the bootstrap function. The
//! raw bytes never cross the wire. The platform asks the DB to sign
//! tokens via `__zeroship_admin.sign_session` over a privileged
//! connection (the runtime maintains a separate "platform pool" — see
//! `crates/plugin-db/src/auth/session.rs`).
//!
//! This sidesteps the "where does the first secret come from?"
//! problem entirely: the secret lives in the DB from the moment the
//! cluster is bootstrapped; an env-var bootstrap is unnecessary.
//!
//! ## Compile-time gating (`hardening` Cargo feature)
//!
//! The entire subtree is gated behind `#[cfg(feature = "hardening")]`
//! at `lib.rs` (commit `2fa9472e`, cycle 10:47). Default builds do
//! NOT compile this module — `crate::auth::*` is invisible to
//! `cargo build -p zeroship-plugin-db --lib`. The eventual control-
//! plane wire-up (per the auth-r1 design) flips the feature on; until
//! then `replication::ensure_publication_and_slot` and the rest of
//! P8a/P8b continue to function unchanged using the per-app role
//! directly. Integration tests probe this surface via
//! `required-features = ["test-helpers", "hardening"]` on the
//! `[[test]] integration` target.
//!
//! The original `--harden` CLI flag in the proposal is one possible
//! runtime opt-in once this module ships; it is NOT how the subtree
//! is currently gated.

// `util` is reachable whenever the `auth` module itself is reachable
// (`any(feature = "hardening", feature = "sqlite")`). It owns the
// helpers — TTL default, getrandom fallback, ISO timestamp formatter,
// hex codec — that BOTH the PG free fns in `session.rs` and the
// upcoming SQLite `SessionMinter` impl in `backend/sqlite/session_minter.rs`
// need. See `docs/proposals/p3-sqlite-auth-implementation-plan.md` §6
// (H-1).
pub mod util;

// The PG-side `bootstrap` / `keys` / `session` modules stay gated to
// the `hardening` feature: they speak SECURITY DEFINER + `compio_postgres`
// and aren't reachable from the SQLite arm.
#[cfg(feature = "hardening")]
pub mod bootstrap;
#[cfg(feature = "hardening")]
pub mod keys;
#[cfg(feature = "hardening")]
pub mod session;

#[cfg(feature = "hardening")]
pub use bootstrap::{ensure_admin_schema, BootstrapOutcome};
#[cfg(feature = "hardening")]
pub use keys::{rotate_session_keys, RotationOutcome};
#[cfg(feature = "hardening")]
pub use session::{init_session, mint_session_token, MintedToken, SessionInit};

// Re-export the schema/role names so other modules (replication.rs,
// the runtime control plane) can address them without stringly-typed
// literals duplicated across the codebase.

/// The privileged schema that owns every C1 platform object.
pub const ADMIN_SCHEMA: &str = "__zeroship_admin";

/// The platform service role — owns the admin schema, replication
/// slots, publications. Workers running the WAL consumer connect under
/// this role; app code does not.
pub const PLATFORM_ROLE: &str = "__zeroship_platform_role";

/// A template role that per-app roles inherit grants from. Provides
/// USAGE on `__zeroship_admin` + EXECUTE on the safe-to-call wrapper
/// functions. Per-app roles (`app_<id>_role`) are created downstream
/// by the control plane during app provisioning — they're outside
/// P8c's scope.
pub const APP_ROLE_TEMPLATE: &str = "__zeroship_app_role_template";

// Token-lifetime / nonce-retention constants moved to
// `crate::auth::util` in P3 PR 1 so they're reachable from both the
// PG arm (gated by `hardening`) and the SQLite arm (gated by `sqlite`).
// Re-exported here for back-compat with existing in-crate callers.
pub use util::{DEFAULT_TOKEN_TTL_SECS, NONCE_RETENTION_SECS};
