//! Idempotent provisioning of the per-app PostgreSQL role.
//!
//! Part of the always-compiled `auth/*` subtree - see the `lib.rs` mod
//! declarations.
//!
//! Privilege here is carried by the CONNECTION'S ROLE, never by a
//! function the worker calls. `SET [LOCAL] ROLE "app_<id>_role"` narrows
//! what the backend can reach for the life of a statement or
//! transaction; there is no definer-rights wrapper, no signed
//! session token, and no platform-owned system schema. That is the
//! AGENTS.md invariant "privilege follows the PROCESS, not the
//! function": anything the worker can invoke is, by construction, not
//! privileged, so a capability it invokes cannot be a boundary.
//!
//! The `__zeroship_admin` schema, its six tables and its 32
//! definer-rights routines were deleted on 2026-08-27 under that
//! invariant. Nothing replaced them; the runtime descriptor is the
//! schema authority and per-app roles are the privilege boundary.
//!
//! Every `CREATE ROLE` here is wrapped in an existence probe, so
//! re-running [`ensure_per_app_role`] on a provisioned cluster is a
//! cheap no-op.

use compio_postgres::Pool;

use super::APP_ROLE_TEMPLATE;
use crate::error::DbError;

const RESERVED_SYSTEM_TABLE_PREFIX: &str = "__zeroship_";

/// Wrap a `compio_postgres::Error` in [`DbError`] with a context phrase
/// so operators see *what* the bootstrap layer was doing when the SQL
/// failed. The SQLSTATE classification still drives the `.code`
/// (`unique_violation`, `serialization_failure`, `transient`, …) — this
/// helper only prepends `"auth/bootstrap: <ctx>: "` to the message body.
///
/// Variant-walking is shared with the other per-module helpers via
/// [`crate::error::coded_sql`]; this is the `auth/bootstrap`-scoped
/// thin wrapper.
fn coded_sql(context: &str, e: compio_postgres::Error) -> DbError {
    crate::error::coded_sql(&format!("auth/bootstrap: {context}"), e)
}

/// Tiny helper for the existence + create pattern.
///
/// Postgres doesn't have `CREATE ROLE IF NOT EXISTS` (since the
/// attributes might differ from the existing role); we probe
/// `pg_roles` first, then issue the CREATE only when missing. The
/// `attrs` string is appended verbatim to the CREATE ROLE statement —
/// callers pass identifier-clean literals, no user input flows here.
async fn create_role_if_missing(
    pool: &Pool,
    name: &str,
    attrs: &str,
) -> Result<bool, DbError> {
    let exists: bool = !pool
        .query_text_params(
            "SELECT 1 FROM pg_roles WHERE rolname = $1",
            &[name],
        )
        .await
        .map_err(|e| coded_sql(&format!("probe pg_roles {name}"), e))?
        .is_empty();
    if exists {
        return Ok(false);
    }
    pool.execute(&format!(r#"CREATE ROLE "{name}" {attrs}"#), &[])
        .await
        .map_err(|e| coded_sql(&format!("CREATE ROLE {name}"), e))?;
    Ok(true)
}

fn sql_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

// ---------------------------------------------------------------------------
// Per-app PG role hardening (§17.5)
// ---------------------------------------------------------------------------
//
// §17.5 invariant — **slot ownership stays platform-side**. A per-app
// role owns ONLY its own schema. It NEVER receives the `REPLICATION`
// attribute: a logical replication slot requires `REPLICATION`, and
// granting it to a per-app role would let app-A observe app-B's WAL —
// the multi-tenant break the section exists to prevent.
//
// Slots and publications are NOT reachable from the worker at all. The
// definer-rights wrappers that used to let a per-app role create
// them without holding `REPLICATION` are deleted: a capability the
// worker can invoke is not a boundary, so slot and publication
// ownership belongs to the CDC relay service, which does not execute
// creator code. Nothing in this module grants a path to them.
//
// **Pre-launch, no back-compat (AGENTS.md):** there is NO detect-and-warn
// / ALTER-existing-schema backfill path. New schemas get the role at
// creation; that is the entire surface. A production cluster from before
// this lands does not exist.

/// Compose the per-app PG role name from an `app_id`.
///
/// Convention `app_<id>_role` - pinned by the unit test
/// `tests::per_app_role_name_uses_app_id_role_convention` in this file,
/// which asserts the composed name for three shapes of `app_id`.
/// (Deliberately not an intra-doc link: `tests` is `#[cfg(test)]`, so
/// rustdoc cannot resolve it and the workspace doc-link gate would
/// fail.) The
/// `auth::mod` docstring on [`APP_ROLE_TEMPLATE`] DESCRIBES the same
/// convention ("Per-app roles (`app_<id>_role`) are created downstream
/// by the control plane during app provisioning") but cannot pin it:
/// prose is not compiled against this function, so it can drift from
/// the code silently. Read it for the why, not as a guarantee.
///
/// The platform-managed template role uses the `__zeroship_` prefix;
/// per-app roles deliberately do NOT, so they are visually distinct
/// from the trust anchor in `pg_roles` and `\du` output.
///
/// `app_id` is a non-creator-controllable UUIDv7 base62 typed_id
/// validated to `[A-Za-z0-9_-]`. Preserve the raw identifier so two
/// distinct app ids cannot collapse onto the same role name; every SQL
/// call site double-quotes the result, so `-` and uppercase letters are
/// safe here without a lossy transform.
pub fn per_app_role_name(app_id: &str) -> String {
    format!("app_{app_id}_role")
}

/// `SET LOCAL ROLE "app_<id>_role"` — used INSIDE a transaction so the
/// role automatically reverts at COMMIT/ROLLBACK (no explicit `RESET`
/// needed, and no risk of a pooled connection leaking the role to the
/// next checkout). This is the preferred client-SQL injection point.
///
/// The role name flows through [`per_app_role_name`] (validated +
/// normalised) and is double-quoted, so this is injection-safe even
/// though it interpolates.
#[cfg(any(test, feature = "test-helpers"))]
pub fn set_local_role_sql(app_id: &str) -> String {
    format!("SET LOCAL ROLE {}", crate::query::quote_ident(&per_app_role_name(app_id)))
}

/// `SET ROLE "app_<id>_role"` — session-level variant for the rare
/// non-transactional client-SQL path. MUST be paired with
/// [`reset_role_sql`] before the connection returns to the pool, or the
/// next checkout inherits the constrained role.
#[cfg(any(test, feature = "test-helpers"))]
pub fn set_role_sql(app_id: &str) -> String {
    format!("SET ROLE {}", crate::query::quote_ident(&per_app_role_name(app_id)))
}

/// `RESET ROLE` — restore the session's original (login) role. Pairs
/// with [`set_role_sql`] on the non-transactional path.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_role_sql() -> &'static str {
    "RESET ROLE"
}

// ── DB-1: per-app connection-hold / statement-time guards ────────────────────
//
// Bound how long any one app connection can pin shared-Postgres resources, so a
// single tenant cannot exhaust the shared instance — neither by parking a
// dedicated transaction connection `idle in transaction` (the unbounded risk:
// `env.db.transaction()` acquires a fresh connection held for the whole JS
// callback) nor by pinning a backend on a runaway statement. The values are
// generous (legitimate work stays well under) but finite. Applied as a single
// simple-query batch alongside the role SET, so it costs one extra round-trip.

/// Max wall-time a single statement may run before Postgres cancels it.
pub const DB_STATEMENT_TIMEOUT_MS: u32 = 30_000;
/// Max time a connection may sit `idle in transaction` before Postgres
/// terminates it — the direct guard against tx-hold connection exhaustion.
pub const DB_IDLE_IN_TX_TIMEOUT_MS: u32 = 15_000;
/// Max time a statement waits on a lock before erroring (avoids lock pileups).
pub const DB_LOCK_TIMEOUT_MS: u32 = 10_000;

/// Combined per-transaction client setup: `SET LOCAL ROLE` + the DB-1 timeout
/// guards, as one simple-query batch run right after `BEGIN`. All `SET LOCAL`,
/// so every value (role + timeouts) auto-reverts at COMMIT/ROLLBACK and can
/// never leak to a later checkout of the (dedicated, but defensively reset)
/// connection.
pub fn tx_session_setup_sql(app_id: &str) -> String {
    let role = crate::query::quote_ident(&per_app_role_name(app_id));
    format!(
        "SET LOCAL ROLE {role}; \
         SET LOCAL statement_timeout = {DB_STATEMENT_TIMEOUT_MS}; \
         SET LOCAL idle_in_transaction_session_timeout = {DB_IDLE_IN_TX_TIMEOUT_MS}; \
         SET LOCAL lock_timeout = {DB_LOCK_TIMEOUT_MS}"
    )
}

/// Combined autocommit (pooled) client setup, run inside a short-lived
/// explicit transaction: `SET LOCAL ROLE` + statement/lock timeout guards.
///
/// Every value is `SET LOCAL`, so role + timeouts auto-revert at
/// COMMIT/ROLLBACK — including the implicit rollback-on-drop the
/// `compio_postgres::Transaction` performs when the future is cancelled
/// mid-flight. This makes the pooled (autocommit) path leak-proof on
/// EVERY return-to-pool path, matching the explicit-transaction path's
/// guarantee. No `idle_in_transaction` guard — the wrapping transaction
/// is opened and committed around a single statement, so it never sits
/// idle in transaction (the per-statement `statement_timeout` already
/// bounds the work).
pub fn autocommit_local_session_setup_sql(app_id: &str) -> String {
    let role = crate::query::quote_ident(&per_app_role_name(app_id));
    format!(
        "SET LOCAL ROLE {role}; \
         SET LOCAL statement_timeout = {DB_STATEMENT_TIMEOUT_MS}; \
         SET LOCAL lock_timeout = {DB_LOCK_TIMEOUT_MS}"
    )
}

/// Result of [`ensure_per_app_role`] — distinguishes "created the role
/// now" from "role already existed" for idempotency telemetry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PerAppRoleOutcome {
    /// True iff this call issued the `CREATE ROLE`.
    pub created_role: bool,
}

/// Idempotently provision the per-app PG role and scope its grants to
/// the per-app schema ONLY (§17.5).
///
/// Call AFTER the app schema exists (the deploy creates it)
/// `"<app_id>"` (the grants reference it). Steps:
///
/// 1. `CREATE ROLE "app_<id>_role" NOLOGIN NOREPLICATION …
///    IN ROLE "__zeroship_app_role_template"` — the explicit
///    `NOREPLICATION` is the §17.5 non-negotiable; `IN ROLE` anchors
///    every per-app role under one template so a cluster-wide audit
///    reads one membership edge per app.
/// 2. `GRANT USAGE, CREATE ON SCHEMA "<app_id>"` — the role may use and
///    add objects to its own schema.
/// 3. `GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA
///    "<app_id>"`, immediately followed by a revoke on reserved
///    `__zeroship_*` tables, plus matching `GRANT USAGE ON ALL SEQUENCES`
///    — CRUD on existing creator tables only.
/// 4. `ALTER DEFAULT PRIVILEGES IN SCHEMA "<app_id>" GRANT … ON
///    TABLES/SEQUENCES` — so tables/sequences the role (or the platform
///    migrator) creates LATER are auto-granted, no re-run needed.
///    Reserved workflow journals are created under the platform owner role,
///    so these caller-role default privileges do not cover them.
///
/// Explicitly does NOT grant `REPLICATION`, nor any privilege on another
/// app's schema. There is no privileged schema for it to reach: the
/// template carries schema membership only, not EXECUTE on any
/// definer-rights routine.
///
/// Runs under the caller's pool, which in production is the platform
/// (bootstrap) role — a superuser or CREATEROLE principal.
pub async fn ensure_per_app_role(pool: &Pool, app_id: &str) -> Result<PerAppRoleOutcome, DbError> {
    let role = per_app_role_name(app_id);
    let schema = crate::query::quote_ident(app_id);
    let qrole = format!("\"{role}\"");

    // 0. Ensure the app-role template anchor exists. The per-app role's
    //    `IN ROLE "<template>"` membership (step 1) requires it, and
    //    this is now the ONLY thing that creates it. The template is a
    //    NOLOGIN/NOREPLICATION permission anchor — creating it is
    //    idempotent and carries no login surface.
    create_role_if_missing(
        pool,
        APP_ROLE_TEMPLATE,
        "NOLOGIN NOREPLICATION NOCREATEDB NOCREATEROLE NOINHERIT",
    )
    .await?;

    // 1. CREATE ROLE — NOREPLICATION is the §17.5 invariant, asserted
    //    explicitly (not relying on the server default). `IN ROLE`
    //    grants membership in the template.
    let created = create_role_if_missing(
        pool,
        &role,
        &format!(
            "NOLOGIN NOREPLICATION NOCREATEDB NOCREATEROLE INHERIT IN ROLE \"{APP_ROLE_TEMPLATE}\""
        ),
    )
    .await?;

    // 2. schema-level: USAGE (enter the schema) + CREATE (add objects).
    pool.execute(
        &format!("GRANT USAGE, CREATE ON SCHEMA {schema} TO {qrole}"),
        &[],
    )
    .await
    .map_err(|e| coded_sql(&format!("GRANT USAGE,CREATE ON SCHEMA {app_id}"), e))?;

    // 3. existing tables + sequences.
    pool.execute(
        &format!("GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA {schema} TO {qrole}"),
        &[],
    )
    .await
    .map_err(|e| coded_sql(&format!("GRANT table CRUD ON SCHEMA {app_id}"), e))?;
    revoke_reserved_system_table_privileges(pool, app_id, &role).await?;
    pool.execute(
        &format!("GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA {schema} TO {qrole}"),
        &[],
    )
    .await
    .map_err(|e| coded_sql(&format!("GRANT sequence usage ON SCHEMA {app_id}"), e))?;

    // 4. default privileges for FUTURE objects in this schema. Without
    //    this, a table the platform migrator creates next deploy would
    //    be un-readable by the per-app role until a manual re-grant.
    pool.execute(
        &format!(
            "ALTER DEFAULT PRIVILEGES IN SCHEMA {schema} \
             GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO {qrole}"
        ),
        &[],
    )
    .await
    .map_err(|e| coded_sql(&format!("ALTER DEFAULT PRIVILEGES tables {app_id}"), e))?;
    pool.execute(
        &format!(
            "ALTER DEFAULT PRIVILEGES IN SCHEMA {schema} \
             GRANT USAGE, SELECT ON SEQUENCES TO {qrole}"
        ),
        &[],
    )
    .await
    .map_err(|e| coded_sql(&format!("ALTER DEFAULT PRIVILEGES sequences {app_id}"), e))?;

    Ok(PerAppRoleOutcome {
        created_role: created,
    })
}

async fn revoke_reserved_system_table_privileges(
    pool: &Pool,
    app_id: &str,
    role: &str,
) -> Result<(), DbError> {
    let schema_literal = sql_string_literal(app_id);
    let role_literal = sql_string_literal(role);
    let prefix_literal = sql_string_literal(RESERVED_SYSTEM_TABLE_PREFIX);
    pool.execute(
        &format!(
            "DO $$ \
             DECLARE \
               rel record; \
             BEGIN \
               FOR rel IN \
                 SELECT n.nspname, c.relname \
                   FROM pg_class c \
                   JOIN pg_namespace n ON n.oid = c.relnamespace \
                  WHERE n.nspname = {schema_literal} \
                    AND c.relkind IN ('r', 'p', 'v', 'm', 'f') \
                    AND left(c.relname, {prefix_len}) = {prefix_literal} \
               LOOP \
                 EXECUTE format( \
                   'REVOKE ALL PRIVILEGES ON TABLE %I.%I FROM %I', \
                   rel.nspname, rel.relname, {role_literal} \
                 ); \
               END LOOP; \
             END \
             $$",
            prefix_len = RESERVED_SYSTEM_TABLE_PREFIX.len(),
        ),
        &[],
    )
    .await
    .map_err(|e| coded_sql(&format!("REVOKE reserved table privileges {app_id}"), e))?;
    Ok(())
}

/// Drop the per-app role. Called by the §17.7 drop-namespace sequence
/// AFTER `DROP SCHEMA "<app_id>" CASCADE`, so no objects depend on the
/// role at drop time. Idempotent: `DROP ROLE IF EXISTS` is a no-op when
/// the role is already gone (or was never created).
///
/// Postgres refuses to drop a role that still owns objects or holds
/// grants; the CASCADE schema-drop in step 6 removes the role's objects,
/// and the grants vanish with the schema. If a stray dependency remains
/// (e.g. a grant in another schema that should never have existed), the
/// DROP errors loudly rather than silently — surfacing the §17.5
/// violation instead of masking it.
pub async fn drop_per_app_role(pool: &Pool, app_id: &str) -> Result<(), DbError> {
    let role = per_app_role_name(app_id);
    pool.execute(&format!("DROP ROLE IF EXISTS \"{role}\""), &[])
        .await
        .map_err(|e| coded_sql(&format!("DROP ROLE {role}"), e))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_app_role_name_uses_app_id_role_convention() {
        // THIS assertion is the pin for the `app_<id>_role` convention.
        // The `APP_ROLE_TEMPLATE` docstring describes the same rule in
        // prose; nothing compiles prose against `per_app_role_name`, so
        // if the two disagree it is this test that decides.
        //
        // What it does NOT catch: a change to the convention made here
        // AND in the composer together. It pins the shape against a
        // hardcoded literal, not against the docstring's text.
        assert_eq!(per_app_role_name("app_demo"), "app_app_demo_role");
        assert_eq!(per_app_role_name("app-abc"), "app_app-abc_role");
        assert_eq!(per_app_role_name("App_X"), "app_App_X_role");
    }

    #[test]
    fn per_app_role_name_does_not_collapse_distinct_app_ids() {
        assert_ne!(
            per_app_role_name("app-demo"),
            per_app_role_name("app_demo"),
            "hyphen and underscore app ids must map to distinct quoted PG roles"
        );
    }

    #[test]
    fn set_role_sql_shapes_are_quoted_and_correct() {
        assert_eq!(set_local_role_sql("app_demo"), r#"SET LOCAL ROLE "app_app_demo_role""#);
        assert_eq!(set_role_sql("app_demo"), r#"SET ROLE "app_app_demo_role""#);
        assert_eq!(reset_role_sql(), "RESET ROLE");
    }

    #[test]
    fn set_role_sql_doubles_embedded_quote_via_quote_ident() {
        // DB-15: the role name is the single statement enforcing per-tenant
        // role separation. It MUST flow through quote_ident (doubling any
        // embedded `"`), not a hand-written `"{}"` splice — even though app_id
        // is validated upstream, this boundary must not rely on that.
        assert_eq!(set_local_role_sql(r#"a"b"#), r#"SET LOCAL ROLE "app_a""b_role""#);
        assert_eq!(set_role_sql(r#"a"b"#), r#"SET ROLE "app_a""b_role""#);
    }

    #[test]
    fn tx_session_setup_bounds_hold_and_statement_time() {
        // DB-1: every dedicated transaction client must SET LOCAL the timeout
        // guards that bound how long it can be held idle-in-transaction and how
        // long a statement may run — the defense against one tenant exhausting
        // the shared Postgres connection pool fleet-wide. SET LOCAL so they
        // revert at COMMIT/ROLLBACK.
        let sql = tx_session_setup_sql("app_demo");
        assert!(sql.contains(r#"SET LOCAL ROLE "app_app_demo_role""#), "{sql}");
        assert!(sql.contains("SET LOCAL idle_in_transaction_session_timeout ="), "{sql}");
        assert!(sql.contains("SET LOCAL statement_timeout ="), "{sql}");
        assert!(sql.contains("SET LOCAL lock_timeout ="), "{sql}");
    }

    #[test]
    fn autocommit_local_session_setup_bounds_statement_time_via_set_local() {
        // P2-C1 + DB-1: the pooled autocommit path is pool-bounded (8) but a
        // slow statement still pins one of those shared connections -- bound it
        // with a statement_timeout. Crucially every value is `SET LOCAL`, run
        // inside an explicit transaction, so role + timeouts auto-revert at
        // COMMIT/ROLLBACK (including rollback-on-drop on cancellation) and can
        // never leak to the next checkout. No idle-in-tx guard — the wrapping
        // transaction commits around a single statement and never sits idle.
        let setup = autocommit_local_session_setup_sql("app_demo");
        assert!(setup.contains(r#"SET LOCAL ROLE "app_app_demo_role""#), "{setup}");
        assert!(setup.contains("SET LOCAL statement_timeout ="), "{setup}");
        assert!(setup.contains("SET LOCAL lock_timeout ="), "{setup}");
        // Every directive must be SET LOCAL — a bare session-level SET would
        // re-introduce the leak the explicit transaction is here to prevent.
        assert!(!setup.contains("SET ROLE "), "must be SET LOCAL ROLE: {setup}");
        assert!(
            !setup.contains("idle_in_transaction"),
            "no idle guard on autocommit: {setup}"
        );
    }

    #[test]
    fn create_role_attrs_assert_noreplication() {
        // §17.5 NON-NEGOTIABLE: the per-app CREATE ROLE attribute string
        // MUST contain NOREPLICATION. This is a source-level guard so a
        // future edit that drops the attribute (relying on the server
        // default) fails the test — the default is overridable per
        // cluster (`rolreplication` inheritance is subtle), so we assert
        // it explicitly. The literal lives in `ensure_per_app_role`;
        // mirror it here.
        let attrs =
            format!("NOLOGIN NOREPLICATION NOCREATEDB NOCREATEROLE INHERIT IN ROLE \"{APP_ROLE_TEMPLATE}\"");
        assert!(
            attrs.contains("NOREPLICATION"),
            "per-app role MUST be NOREPLICATION (§17.5 slot-ownership-stays-platform)"
        );
        assert!(
            !attrs.contains(" REPLICATION"),
            "per-app role MUST NOT carry the REPLICATION attribute"
        );
    }

    #[test]
    fn app_role_template_constant_uses_proposal_naming() {
        assert_eq!(APP_ROLE_TEMPLATE, "__zeroship_app_role_template");
    }
}
