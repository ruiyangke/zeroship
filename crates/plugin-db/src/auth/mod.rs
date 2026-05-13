//! P8c — SECURITY DEFINER trust anchor + HMAC-signed session init
//! for the C1 reactive-query subsystem.
//!
//! ## What this module ships
//!
//! Per the zeroship-db-v2 proposal (R5-R8 of the review loop, line 285
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
//! ## Backwards compatibility
//!
//! The hardened path is **opt-in** per the proposal's gradual-migration
//! guidance. `replication::ensure_publication_and_slot` and the rest of
//! P8a/P8b continue to function unchanged when bootstrap has not been
//! run; callers that pass `--harden` (or invoke
//! `bootstrap::ensure_admin_schema` themselves) get the hardened path.

pub mod bootstrap;
pub mod keys;
pub mod session;

pub use bootstrap::{ensure_admin_schema, BootstrapOutcome};
pub use keys::{rotate_session_keys, RotationOutcome};
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

/// Default lifetime for a minted session token. The signed payload
/// includes `expires_at`, so an HTTP-roundtripped token that misses
/// this window is rejected by `init_session`. Five minutes is enough
/// for any sane connection-acquire round-trip and short enough that a
/// captured token can't be replayed long.
pub const DEFAULT_TOKEN_TTL_SECS: i64 = 300;

/// How long the nonce-replay-protection table retains a row. Must
/// outlive `DEFAULT_TOKEN_TTL_SECS` plus the key-rotation grace window
/// so a captured-and-late-arriving signature cannot bypass replay
/// detection by being delayed past the nonce's GC.
pub const NONCE_RETENTION_SECS: i64 = 25 * 3600;
