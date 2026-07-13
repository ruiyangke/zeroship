//! The least-privilege per-project `migrator` role — **second-line DB-privilege
//! defense**.
//!
//! The SQL guard ([`crate::guard::SqlGuard`]) is the first line: it parses every `up`
//! and denies the dangerous surface at submission. But a parser can be evaded
//! by **runtime-constructed SQL** — e.g. `DO $$ … EXECUTE format('… %I …', s) …`
//! where the target schema is computed at execution and never appears as a
//! parseable identifier the guard can confine. The guard documents these as
//! residuals. **The second line backstops them:** the migration's DDL runs under a
//! dedicated, deliberately under-privileged Postgres role that has *no grants*
//! on `control` / `auth` / `billing` / other projects' schemas, so the same op
//! that slips past parse **fails with `permission denied` at execution**.
//!
//! # Role model — `NOLOGIN` + `SET ROLE` (not a login role)
//!
//! `provision_migrator` creates a deterministic `migrator_<project>_<hash>` role
//! as **`NOLOGIN`**. The
//! executor connects as the privileged admin/control role and runs each
//! migration under `SET ROLE` for that role (with `RESET ROLE` on exit, scoped
//! exactly like the executor's session-GUC restore). This is chosen
//! over a `LOGIN` role because:
//!
//! - **No per-role passwords / secrets.** A login role needs a credential to be
//!   provisioned, rotated, and handed to the executor; `SET ROLE` reuses the
//!   admin connection the executor already holds.
//! - **No connection churn.** The executor already owns one admin session +
//!   the project advisory lock; switching the *effective* role inside it keeps
//!   the lock and the journal writes on one session.
//! - **Same DB-enforced confinement.** `SET ROLE` to a `NOSUPERUSER` role makes
//!   privilege checks run as that role — a superuser admin that `SET ROLE`s to
//!   a non-superuser is fully constrained by the target role's grants (a
//!   superuser only bypasses checks while it is *itself* the effective role).
//!
//! # The grant set (least privilege)
//!
//! The migrator role gets **exactly**:
//!
//! - `NOSUPERUSER NOCREATEROLE NOCREATEDB NOLOGIN NOBYPASSRLS` — cannot escalate
//!   (`CREATE ROLE`, `ALTER SYSTEM`, `CREATE DATABASE` all denied by attribute).
//! - **owns** the project schema (so its DDL + `ALTER DEFAULT PRIVILEGES`
//!   targets work and objects it creates are owned/usable by it), with
//!   `CREATE, USAGE` on it.
//! - **NO access whatsoever to the meta schema.** The
//!   migrator must not be able to forge the journal. A migration's `up` runs as
//!   the migrator, so if the migrator could `INSERT` into the journal it could
//!   plant a `completed` row (silently suppressing a future legitimate
//!   migration: `pending = set − completed`) or a bogus checksum (wedging the
//!   apply on `ChecksumDrift`). All journal / inflight I/O is therefore done by
//!   the **executor as the admin role** — the migrator gets neither `USAGE` on
//!   the meta schema nor any grant on `schema_migrations` /
//!   `schema_migrations_inflight`. The journal is unforgeable by deny-by-absence.
//! - `search_path` set to the project schema **first**, then the extension
//!   schema(s) (default `public`), via `ALTER ROLE`. The project schema is the
//!   sole writable resolution target; the extension schema(s) ride at the end
//!   purely so an unqualified extension TYPE/function the engine emits
//!   (pgvector's `vector(N)`, `PostGIS`'s `geography(...)`) resolves. The meta
//!   schema is off the migrator's path — defense-in-depth so an unqualified name
//!   in an `up` can never resolve to the journal even if a grant were ever
//!   reintroduced.
//! - **`REVOKE ALL` then `GRANT USAGE` on the extension schema(s) (`public`)**:
//!   the migrator cannot stage objects there (no `CREATE`) nor reach existing
//!   tables (no per-object grant), but `USAGE` lets it *resolve* the shared
//!   extension types. USAGE is resolution-only — it relaxes nothing about
//!   cross-schema **write** confinement. (Matches a data-plane runtime, which
//!   references the same unqualified `vector`/`geography` types with `public`
//!   reachable on its connection path.)
//! - **No grant whatsoever** on any other project schema — any schema outside
//!   the migrator's own. Deny-by-absence: a role only has what it is
//!   granted, so an unmentioned schema is unreachable. This is the second-line
//!   backstop.
//!
//! # Known residuals (tracked, no behavior change)
//!
//! - **`CREATE FUNCTION … SET search_path`** is denied by the guard but is
//!   NOT role-backstopped. This is harmless: functions the migrator creates are
//!   `INVOKER` by default, so they run with the *caller's* privileges (no
//!   escalation), and `SECURITY DEFINER` (which would run as the function owner,
//!   the migrator) is itself guard-denied. So the second-line role gives no extra
//!   confinement here, and none is needed. Tracked only.
//! - **`pg_roles` enumeration.** The migrator can read `pg_roles` (a
//!   cluster-global catalog `USAGE`-free to all roles). Accepted: role names are
//!   not secrets and there is no privilege to enumerate. No change.
//!
//! # Idempotency
//!
//! `provision_migrator` is safe to run on every deploy: role creation is guarded
//! on `pg_roles`, and every `GRANT` / `ALTER` / `REVOKE` is naturally idempotent
//! (re-granting an existing grant is a no-op, re-altering `search_path` is a no-op).
//! Schema ownership is only (re)assigned when it differs.

use crate::id::base62_encode_bytes;
use sha2::{Digest, Sha256};

/// Error provisioning or deprovisioning a migrator role.
#[derive(Debug, thiserror::Error)]
pub enum RoleError {
    /// The derived role name was empty or otherwise unusable.
    #[error("invalid migrator role name derived from project id '{0}'")]
    BadRoleName(String),
    /// An engine-supplied identifier (role / project schema / meta schema /
    /// extension schema) was not quotable (empty or NUL-bearing) at a render
    /// seam — fail-closed rather than interpolate it. Maps
    /// [`crate::render::dml::IdentQuoteError`].
    #[error("role provisioning: {0}")]
    IdentQuote(#[from] crate::render::dml::IdentQuoteError),
}

/// Quote a SQL identifier (double embedded quotes, wrap in `"`), so a schema /
/// role name is never interpolated as raw SQL. Routes through the ONE crate-shared
/// engine seam ([`crate::render::dml::quote_ident_checked`]) — byte-identical to (and
/// uniformly self-defending with) `author`/`backfill`/`journal`/`dml`: fail-closed
/// on an empty / NUL identifier.
#[cfg(test)]
fn quote_ident(ident: &str) -> Result<String, RoleError> {
    Ok(crate::render::dml::quote_ident_checked(ident)?)
}

/// Test seam (see `dml::tests::all_engine_seams_render_uniformly`).
#[cfg(test)]
pub(crate) fn quote_ident_for_test(ident: &str) -> Result<String, RoleError> {
    quote_ident(ident)
}

/// Derive the deterministic migrator role name for a project.
///
/// `migrator_<readable-prefix>_<hash>` with the project id sanitized to the
/// Postgres identifier charset (`[a-z0-9_]`) and disambiguated by a base62
/// SHA-256 suffix over the raw project id. The result is always quoted at use
/// sites, so this is defense-in-depth, not the sole injection guard.
///
/// # Errors
/// [`RoleError::BadRoleName`] if the project id sanitizes to an empty string.
pub fn migrator_role_name(project_id: &str) -> Result<String, RoleError> {
    let sanitized: String = project_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        return Err(RoleError::BadRoleName(project_id.to_string()));
    }
    let suffix = base62_encode_bytes(&Sha256::digest(project_id.as_bytes()));
    const PREFIX: &str = "migrator_";
    const SEP_LEN: usize = 1;
    const PG_MAX_IDENT_BYTES: usize = 63;
    let prefix_budget = PG_MAX_IDENT_BYTES
        .saturating_sub(PREFIX.len())
        .saturating_sub(SEP_LEN)
        .saturating_sub(suffix.len());
    if prefix_budget == 0 {
        return Err(RoleError::BadRoleName(project_id.to_string()));
    }
    let readable: String = sanitized.chars().take(prefix_budget).collect();
    Ok(format!("{PREFIX}{readable}_{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::migrator_role_name;

    #[test]
    fn migrator_role_name_is_injective_across_lossy_sanitization() {
        let a = migrator_role_name("app_Alice").expect("role name");
        let b = migrator_role_name("app_alice").expect("role name");

        assert_ne!(
            a, b,
            "case-distinct project ids must not collapse to one migrator role"
        );
        assert!(
            a.len() <= 63,
            "postgres role name must fit in an identifier: {a}"
        );
        assert!(
            b.len() <= 63,
            "postgres role name must fit in an identifier: {b}"
        );
    }
}
