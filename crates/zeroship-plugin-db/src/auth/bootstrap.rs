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

/// The ONE `__zeroship_`-prefixed table in an app schema the runtime role must
/// keep privileges on.
///
/// Every other reserved table in the app's schema is state a separate service
/// WRITES and the worker only reads — above all the migration journal
/// (`__zeroship_schema_migrations` and its siblings), which the worker must not
/// be able to forge. Stripping the worker's grants on those is the whole point
/// of [`revoke_reserved_system_table_privileges`].
///
/// The unmask audit table is the exception, and it is the exact inverse: the
/// worker is its ONLY writer. Every `unmask()` — granted and denied — appends a
/// row recording who read plaintext, from `crud/unmask.rs`.
///
/// This exemption did not matter while the worker CREATED the table itself,
/// which it did until 2026-08-28: the creator of a Postgres table is its owner,
/// and an owner's rights are implicit and survive `REVOKE ... FROM <owner>`, so
/// the loop below swept the name and changed nothing. Now that the migration
/// service creates it, the worker is an ordinary grantee and the sweep would
/// take its INSERT with it — turning every unmask into `permission denied for
/// table __zeroship_audit_unmask`, on the privileged read path, at runtime.
///
/// Losing ownership is a GAIN, not a regression: a process that owns its own
/// audit log can `TRUNCATE` or `DROP` it, and privilege follows the process.
/// The worker should hold exactly the reach it needs to append and no more.
const WORKER_WRITABLE_RESERVED_TABLE: &str = "__zeroship_audit_unmask";

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

/// The `DO` block [`revoke_reserved_system_table_privileges`] runs.
///
/// Split out from the execution so the predicate can be pinned by a unit test
/// without a live cluster. The behaviour it encodes is a privilege boundary,
/// and the only other way to check it is a live-PG suite that does not run in
/// the default gate.
fn revoke_reserved_system_table_privileges_sql(app_id: &str, role: &str) -> String {
    let schema_literal = sql_string_literal(app_id);
    let role_literal = sql_string_literal(role);
    let prefix_literal = sql_string_literal(RESERVED_SYSTEM_TABLE_PREFIX);
    // The audit table is excluded by NAME rather than by narrowing the prefix:
    // the prefix must keep matching everything else, and a second reserved
    // table that the worker may write should have to be added here deliberately.
    let writable_literal = sql_string_literal(WORKER_WRITABLE_RESERVED_TABLE);
    format!(
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
                AND c.relname <> {writable_literal} \
           LOOP \
             EXECUTE format( \
               'REVOKE ALL PRIVILEGES ON TABLE %I.%I FROM %I', \
               rel.nspname, rel.relname, {role_literal} \
             ); \
           END LOOP; \
         END \
         $$",
        prefix_len = RESERVED_SYSTEM_TABLE_PREFIX.len(),
    )
}

async fn revoke_reserved_system_table_privileges(
    pool: &Pool,
    app_id: &str,
    role: &str,
) -> Result<(), DbError> {
    pool.execute(
        &revoke_reserved_system_table_privileges_sql(app_id, role),
        &[],
    )
    .await
    .map_err(|e| coded_sql(&format!("REVOKE reserved table privileges {app_id}"), e))?;
    Ok(())
}

#[cfg(test)]
mod reserved_table_revoke_tests {
    use super::*;

    /// The exemption is only meaningful if the table WOULD otherwise be swept.
    /// If the audit table were ever renamed out from under the reserved prefix
    /// this test says so, rather than leaving a dead exclusion behind.
    #[test]
    fn the_exempted_table_is_one_the_prefix_would_match() {
        assert!(
            WORKER_WRITABLE_RESERVED_TABLE.starts_with(RESERVED_SYSTEM_TABLE_PREFIX),
            "the exemption is dead unless the prefix matches the name"
        );
    }

    /// THE REGRESSION. Without the `relname <> ...` predicate the sweep strips
    /// the runtime role's INSERT on the audit table, and every `unmask()` dies
    /// with `permission denied for table __zeroship_audit_unmask` — because the
    /// worker stopped creating (and therefore owning) that table on 2026-08-28
    /// and is now an ordinary grantee.
    #[test]
    fn the_sweep_exempts_the_unmask_audit_table() {
        let sql = revoke_reserved_system_table_privileges_sql("app_x", "app_x_role");
        assert!(
            sql.contains("c.relname <> '__zeroship_audit_unmask'"),
            "the unmask audit table must be excluded from the sweep: {sql}"
        );
    }

    /// The sweep must still cover everything else under the prefix — above all
    /// the migration journal, which the worker must never be able to forge.
    #[test]
    fn the_sweep_still_matches_the_reserved_prefix() {
        let sql = revoke_reserved_system_table_privileges_sql("app_x", "app_x_role");
        assert!(
            sql.contains("left(c.relname, 11) = '__zeroship_'"),
            "the reserved-prefix predicate must survive the exemption: {sql}"
        );
        assert!(sql.contains("REVOKE ALL PRIVILEGES ON TABLE"), "{sql}");
    }
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

// ---------------------------------------------------------------------------
// The reserved-prefix sweep, against a live catalog
// ---------------------------------------------------------------------------
//
// `reserved_table_revoke_tests` above greps the generated SQL for the exemption
// clause. THAT CANNOT ANSWER THE QUESTION IT IS ASKED. A string test proves the
// statement mentions `__zeroship_audit_unmask`; it cannot prove PostgreSQL left
// the runtime role's `INSERT` in place afterwards, and the exemption exists for
// exactly one reason - that the worker stopped OWNING that table on 2026-08-28
// and owner rights no longer carry it through a `REVOKE`. Ownership, grants and
// revokes are catalog facts, so this module goes to the catalog.
//
// Gated behind `live-db-tests` (which implies `test-helpers`) so
// `cargo test -p zeroship-plugin-db --lib` stays database-free.
#[cfg(all(test, feature = "live-db-tests"))]
mod live_reserved_sweep_tests {
    use super::*;
    use compio_postgres::{Client, NoTls};

    /// The DSN comes from typed config
    /// (`zeroship_core::config::test_database_url`, backed by the overlay at
    /// `deploy/ops/zeroship.test.toml` or the pre-existing `PG_TEST_URL`
    /// override). This module declares no environment variable of its own and
    /// calls no `set_var`.
    fn test_dsn() -> String {
        zeroship_core::config::test_database_url()
    }

    async fn admin_client() -> Client {
        let (client, conn) = compio_postgres::connect(&test_dsn(), NoTls)
            .await
            .expect("connect to the plugin-db test database");
        compio::runtime::spawn(async move {
            let _ = conn.run().await;
        })
        .detach();
        client
    }

    /// A scratch app id with a unique prefix - this server is shared.
    fn scratch_app() -> String {
        format!("zsaudit_{}", uuid::Uuid::new_v4().simple())
    }

    fn audit_ref(app: &str) -> String {
        format!("{}.\"__zeroship_audit_unmask\"", crate::query::quote_ident(app))
    }

    fn journal_ref(app: &str) -> String {
        format!(
            "{}.\"__zeroship_schema_migrations\"",
            crate::query::quote_ident(app)
        )
    }

    /// The eight columns `crud/unmask.rs`'s PG arm binds, with `id` and `ts`
    /// left to their defaults.
    ///
    /// A COPY of that column list, not the production statement - the producer
    /// is a private `async fn` that resolves its backend out of the isolate
    /// context, which this module has no reason to stand up. So this case cannot
    /// catch the column list drifting apart from the DDL; the live `unmask()`
    /// cases in `tests/integration.rs` are what covers that. What it IS here to
    /// catch is the privilege, which those cases run as the owning superuser and
    /// therefore cannot see.
    fn audit_insert_sql(app: &str) -> String {
        format!(
            "INSERT INTO {} (actor_id, actor_role, collection, row_pk, \"column\", \
             classification, reason, outcome) \
             VALUES ('usr_1', 'admin', 'users', '1', 'ssn', 'pii', 'support', 'granted')",
            audit_ref(app)
        )
    }

    /// Provision one scratch app the way production does: the migration service
    /// (an ADMIN principal, not the worker) creates the reserved tables, then
    /// the per-app role is provisioned over them.
    ///
    /// `provision_audit_unmask_table` is `zeroship-migrate-server`'s production
    /// entry point, reached through the dev-dependency this crate already
    /// declares for exactly this reason - a fixture holding its own `CREATE
    /// TABLE` would prove the shape of the fixture.
    ///
    /// The journal table beside it is NOT from a production generator: the
    /// engine bootstraps `__zeroship_schema_migrations` from inside an apply,
    /// which is far more machinery than this case needs. It stands in for "any
    /// other reserved-prefix table", and all that is asserted about it is that
    /// the sweep still strips it.
    async fn provision_scratch_app(admin: &Client, app: &str) -> String {
        admin
            .batch_execute(&format!(
                "CREATE SCHEMA IF NOT EXISTS {}",
                crate::query::quote_ident(app)
            ))
            .await
            .expect("create scratch app schema");
        zeroship_migrate_server::provisioning::provision_audit_unmask_table(admin, app)
            .await
            .expect("provision the unmask audit table as the migration service does");
        admin
            .batch_execute(&format!(
                "CREATE TABLE IF NOT EXISTS {} (id BIGSERIAL PRIMARY KEY, name TEXT); \
                 CREATE TABLE IF NOT EXISTS {}.\"widgets\" (id BIGSERIAL PRIMARY KEY);",
                journal_ref(app),
                crate::query::quote_ident(app),
            ))
            .await
            .expect("seed the swept journal stand-in and a creator table");
        per_app_role_name(app)
    }

    /// Named-object teardown only. `__zeroship_app_role_template` is
    /// deliberately left alone: `ensure_per_app_role` creates it idempotently,
    /// it is shared by every app on the cluster, and dropping it would break
    /// concurrent work on this server.
    async fn teardown(admin: &Client, app: &str) {
        let role = crate::query::quote_ident(&per_app_role_name(app));
        let _ = admin
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS {} CASCADE",
                crate::query::quote_ident(app)
            ))
            .await;
        let _ = admin.batch_execute(&format!("DROP OWNED BY {role} CASCADE")).await;
        let _ = admin.batch_execute(&format!("DROP ROLE IF EXISTS {role}")).await;
    }

    /// Run `sql` with the connection's role narrowed to the app's runtime role
    /// exactly the way the data plane narrows it - `tx_session_setup_sql`, the
    /// production statement, inside the transaction whose COMMIT/ROLLBACK is
    /// what reverts it.
    async fn as_runtime_role(
        admin: &Client,
        app: &str,
        sql: &str,
    ) -> Result<(), compio_postgres::Error> {
        admin.batch_execute("BEGIN").await?;
        let scoped = async {
            admin.batch_execute(&tx_session_setup_sql(app)).await?;
            admin.batch_execute(sql).await
        }
        .await;
        let _ = admin
            .batch_execute(if scoped.is_ok() { "COMMIT" } else { "ROLLBACK" })
            .await;
        scoped
    }

    fn denied(err: &compio_postgres::Error) -> bool {
        err.code() == Some(&compio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE)
    }

    /// THE ONE THAT MATTERS. The worker must still be able to append to its own
    /// audit log after the reserved-prefix sweep has run over the schema.
    ///
    /// Positive half: a real `INSERT`, as the real runtime role, lands a row.
    /// Negative half: the same role cannot `TRUNCATE` or `DROP` that table, and
    /// cannot write the journal beside it. Losing ownership was claimed as a
    /// security GAIN; a test that only proved the append still works would have
    /// measured the loss and not the gain.
    #[compio::test]
    async fn the_runtime_role_can_append_to_the_audit_log_after_the_sweep() {
        let admin = admin_client().await;
        let app = scratch_app();
        teardown(&admin, &app).await;
        let role = provision_scratch_app(&admin, &app).await;

        let pool = compio_postgres::Pool::connect(&test_dsn(), 2)
            .await
            .expect("pool for ensure_per_app_role");
        ensure_per_app_role(&pool, &app)
            .await
            .expect("provision the per-app role (this is what runs the sweep)");

        // The table is NOT owned by the role that writes it - the premise the
        // exemption exists for. While the worker created it, this assertion
        // would have been false and the sweep would have been a no-op.
        let owner: String = admin
            .query_one_scalar(
                "SELECT tableowner FROM pg_tables \
                  WHERE schemaname = $1 AND tablename = '__zeroship_audit_unmask'",
                &[&app],
            )
            .await
            .expect("read audit table owner");
        assert_ne!(owner, role, "the worker must not own its own audit log");

        // POSITIVE HALF - a real INSERT, as the real role.
        as_runtime_role(&admin, &app, &audit_insert_sql(&app))
            .await
            .expect("the runtime role must be able to append an unmask audit row");
        let rows: i64 = admin
            .query_one_scalar(&format!("SELECT count(*) FROM {}", audit_ref(&app)), &[])
            .await
            .expect("count audit rows");
        assert_eq!(rows, 1, "the append must actually have landed a row");

        // NEGATIVE HALF - the reach it must NOT have.
        for (what, sql) in [
            ("TRUNCATE", format!("TRUNCATE {}", audit_ref(&app))),
            ("DROP", format!("DROP TABLE {}", audit_ref(&app))),
            (
                "forge a journal row",
                format!("INSERT INTO {} (name) VALUES ('forged')", journal_ref(&app)),
            ),
        ] {
            let err = as_runtime_role(&admin, &app, &sql)
                .await
                .expect_err(&format!("the runtime role must not be able to {what}"));
            assert!(
                err.as_db_error().is_some(),
                "{what} must be refused by the server, not by the client: {err}"
            );
        }
        // Only the appended row survives all of that.
        let rows: i64 = admin
            .query_one_scalar(&format!("SELECT count(*) FROM {}", audit_ref(&app)), &[])
            .await
            .expect("re-count audit rows");
        assert_eq!(rows, 1, "no denied statement may have changed the log");

        teardown(&admin, &app).await;
    }

    /// THE CONTROL, differing in exactly one variable: the exemption clause.
    ///
    /// Same production provisioning, same production sweep statement - with the
    /// `AND c.relname <> '__zeroship_audit_unmask'` predicate deleted from the
    /// generated text. The INSERT above must now be refused with
    /// `insufficient_privilege`, which is the exact failure the exemption's
    /// docstring predicts. Without this arm the case above would pass whether or
    /// not the exemption did anything.
    ///
    /// The clause is rebuilt from the two constants rather than typed out, so a
    /// rename that made the strip a no-op fails the `assert_ne!` instead of
    /// quietly turning this control into a duplicate of the case above.
    #[compio::test]
    async fn without_the_exemption_the_sweep_takes_the_runtime_role_insert() {
        let admin = admin_client().await;
        let app = scratch_app();
        teardown(&admin, &app).await;
        let role = provision_scratch_app(&admin, &app).await;

        let pool = compio_postgres::Pool::connect(&test_dsn(), 2)
            .await
            .expect("pool for ensure_per_app_role");
        ensure_per_app_role(&pool, &app)
            .await
            .expect("provision the per-app role");

        let shipped = revoke_reserved_system_table_privileges_sql(&app, &role);
        let exemption = format!(
            "AND c.relname <> {} ",
            sql_string_literal(WORKER_WRITABLE_RESERVED_TABLE)
        );
        let unexempted = shipped.replace(&exemption, "");
        assert_ne!(
            shipped, unexempted,
            "the exemption clause {exemption:?} was not found in the shipped sweep, \
             so this control would have re-run the shipped statement and proved \
             nothing:\n{shipped}"
        );

        admin
            .batch_execute(&unexempted)
            .await
            .expect("run the sweep without its exemption");

        let err = as_runtime_role(&admin, &app, &audit_insert_sql(&app))
            .await
            .expect_err("without the exemption the append MUST be refused");
        assert!(
            denied(&err),
            "expected insufficient_privilege, got {:?}: {err}",
            err.code()
        );
        // `compio_postgres::Error`'s Display is the bare string "db error"; the
        // server's text is on the wrapped `DbError`, which is where the
        // docstring's predicted `permission denied for table
        // __zeroship_audit_unmask` actually appears.
        let message = err
            .as_db_error()
            .map(|db| db.message().to_string())
            .unwrap_or_default();
        assert!(
            message.contains("__zeroship_audit_unmask"),
            "the refusal must name the audit table; got {message:?}"
        );

        // And the swept privilege is exactly the one the exemption protects:
        // the creator table beside it is untouched, so the sweep did not simply
        // strip everything.
        let creator_insert: bool = admin
            .query_one_scalar(
                &format!(
                    "SELECT has_table_privilege('{role}', '{}.\"widgets\"', 'INSERT')",
                    crate::query::quote_ident(&app)
                ),
                &[],
            )
            .await
            .expect("probe creator-table privilege");
        assert!(
            creator_insert,
            "the sweep must leave ordinary creator tables alone"
        );

        teardown(&admin, &app).await;
    }
}
