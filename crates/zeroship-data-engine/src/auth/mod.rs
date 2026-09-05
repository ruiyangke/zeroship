//! Per-app PostgreSQL role provisioning, plus the shared helpers the
//! SQLite session minter reuses.
//!
//! ## What this module ships
//!
//! One thing: the **per-app PG role**. `zeroship-core::database_role`
//! composes the role name. `bootstrap.rs` creates it under the
//! [`APP_ROLE_TEMPLATE`] anchor with `NOLOGIN NOREPLICATION`, scopes its grants
//! to the app's own schema, and emits the `SET [LOCAL] ROLE` + timeout batch the
//! data plane runs per transaction. Privilege is carried by the connection's
//! role - by the PROCESS - and nothing here hands the worker a capability it
//! can invoke.
//!
//! ## What was deleted, and why it is not coming back
//!
//! An earlier design put a `__zeroship_admin` schema here, owned by a
//! `__zeroship_platform_role`, holding six tables (`hmac_keys`,
//! `session_ctx`, `session_nonces`, `column_keys`, `pitr_targets`,
//! `mask_policies`) and 32 `SECURITY DEFINER` routines, with the
//! per-app role granted `EXECUTE` on the "safe" ones. On top of it sat
//! an HMAC session anchor: `sign_session` minted a PID-bound token,
//! `init_session` verified it and wrote a `session_ctx` row, and
//! `rotate_session_keys` rolled the secret.
//!
//! All of it is deleted (operator decision, 2026-08-27), not reduced.
//! The reason is the AGENTS.md invariant "privilege follows the
//! PROCESS, not the function": a `SECURITY DEFINER` wrapper the worker
//! can call is reachable by anything that reaches the worker, so it
//! does not create a boundary - it creates the appearance of one. If
//! the worker can do it, it is not privileged and belongs in the app's
//! own schema under ordinary parameterised SQL. If it must be
//! privileged, it belongs to a service that does not execute creator
//! code (the migration service, the CDC relay, the control plane) - not
//! to a routine the worker invokes.
//!
//! Concretely: slot and publication ownership belongs to the CDC relay,
//! and the runtime descriptor - not a platform-owned schema epoch - is
//! the schema authority.
//!
//! ## The last residue went 2026-09-04
//!
//! `auth/util.rs` outlived the anchor by a week. It held the helpers the
//! two `SessionMinter` impls had to share so both arms produced
//! byte-identical tokens - TTL default, `/dev/urandom` fallback, an ISO
//! timestamp formatter, a hex codec, Hinnant's civil-from-days. The
//! SQLite impl was deleted on 2026-09-02 and the PG one went with the
//! anchor, so nothing was left to keep byte-identical.
//!
//! It survived an audit because a chain hides its own root:
//! `iso_timestamp_after` calls `format_unix_millis` calls
//! `civil_from_days`, and `hex_decode` calls `hex_nibble`, so every link
//! but the root scored as called. Only the roots had zero references,
//! and a mention count reported an 8-symbol dead module as 2. Rank by
//! reachability from a live consumer, not by how often a name appears.
//! (`hex_decode`/`hex_nibble` have a surviving private twin in
//! `zeroship-data-core`'s `encryption/keys.rs`, so nothing was lost.)

// Per-app PG role provisioning. Always compiled: the data plane's
// `SET LOCAL ROLE` batch comes from here on every transaction.
pub mod bootstrap;

/// A template role that per-app roles inherit membership from. Per-app
/// roles (`app_<id>_role`) are created by
/// `bootstrap::ensure_per_app_role`, which also creates this anchor
/// on first use.
///
/// It carries no grants of its own. It exists so a cluster-wide audit
/// reads one membership edge per app rather than N unrelated roles;
/// nothing is granted THROUGH it, and in particular there is no
/// `EXECUTE` on any privileged routine to inherit - there are no
/// privileged routines left.
pub const APP_ROLE_TEMPLATE: &str = "__zeroship_app_role_template";
