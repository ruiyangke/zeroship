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
use zeroship_core::database_role::per_app_role_name;

use super::APP_ROLE_TEMPLATE;
use crate::backend::pg_error;
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
/// service creates it, the worker is an ordinary grantee. The reserved sweep
/// leaves this exact name for its dedicated recipe, which clears every additive
/// privilege before adding only INSERT and serial-sequence USAGE.
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
/// [`crate::backend::pg_error::coded_sql`]; this is the
/// `auth/bootstrap`-scoped
/// thin wrapper.
fn coded_sql(context: &str, e: compio_postgres::Error) -> DbError {
    pg_error::coded_sql(&format!("auth/bootstrap: {context}"), e)
}

/// Tiny helper for the existence + create pattern.
///
/// Postgres doesn't have `CREATE ROLE IF NOT EXISTS` (since the
/// attributes might differ from the existing role); we probe
/// `pg_roles` first, then issue the CREATE only when missing. The
/// `attrs` string is appended verbatim to the CREATE ROLE statement —
/// callers pass identifier-clean literals, no user input flows here.
async fn create_role_if_missing(pool: &Pool, name: &str, attrs: &str) -> Result<bool, DbError> {
    let exists: bool = !pool
        .query_text_params("SELECT 1 FROM pg_roles WHERE rolname = $1", &[name])
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

/// `SET LOCAL ROLE "app_<id>_role"` — used INSIDE a transaction so the
/// role automatically reverts at COMMIT/ROLLBACK (no explicit `RESET`
/// needed, and no risk of a pooled connection leaking the role to the
/// next checkout). This is the preferred client-SQL injection point.
///
/// The role name flows through the shared, length-checked composer and is
/// double-quoted, so this is injection-safe even though it interpolates.
///
/// # Errors
///
/// Returns a typed database error if the complete role name exceeds
/// PostgreSQL's identifier limit.
#[cfg(any(test, feature = "test-helpers"))]
pub fn set_local_role_sql(app_id: &str) -> Result<String, DbError> {
    let role = per_app_role_name(app_id)?;
    Ok(format!(
        "SET LOCAL ROLE {}",
        crate::query::quote_ident(&role)
    ))
}

// DELETED 2026-09-01: `set_role_sql` and `reset_role_sql`.
//
// They built the session-level `SET ROLE` / `RESET ROLE` pair, and their
// rustdoc described "the rare non-transactional client-SQL path" and said the
// two MUST be paired before the connection returns to the pool. No such path
// exists: both had ZERO callers anywhere in the workspace, tests included, and
// the only thing they documented was how to reintroduce a shape this crate
// deliberately refuses.
//
// That shape is cancellation-unsafe by construction. `SET ROLE` persists for
// the SESSION, so its `RESET ROLE` is a manual obligation on a pooled
// connection: cancel the statement between the two - a timeout, a dropped
// future, a panic - and the connection returns to the pool still wearing the
// constrained role, which the next checkout silently inherits. The pairing
// instruction is exactly the part that cannot be relied on.
//
// `SET LOCAL ROLE` inside a transaction has no such obligation: PostgreSQL
// reverts it at COMMIT or ROLLBACK whichever way the statement ends. That is
// why the roled funnel is the only production path, and why the survivor below
// is the LOCAL variant.
//
// Deleted rather than documented-away, per the pre-launch stance: a helper that
// exists is an invitation, and prose telling the reader not to accept it is
// weaker than not offering it.

// ── DB-1: per-app connection-hold / statement-time guards ────────────────────
//
// Bound how long any one app connection can pin shared-Postgres resources, so a
// single tenant cannot exhaust the shared instance — neither by parking a
// dedicated transaction connection `idle in transaction` (the unbounded risk:
// `env.db.transaction()` acquires a fresh connection held for the whole JS
// callback) nor by pinning a backend on a runaway statement. The values are
// generous (legitimate work stays well under) but finite. Applied as a single
// simple-query batch alongside the role SET, so it costs one extra round-trip.

// The three DB-1 budgets moved to `crate::budgets` on 2026-09-01. They are
// cross-backend POLICY - `transaction/driver.rs`'s `budgets()` derives the
// protocol execution deadline for BOTH backends from `DB_IDLE_IN_TX_TIMEOUT_MS`
// - whereas the `SET LOCAL ...` builders below are PostgreSQL DIALECT. Defining
// the numbers next to the PG strings made them read as a PG detail, which would
// have sent them to the wrong crate at the split: either PG dialect into the
// core, or the SQLite deadline losing the constant it reads.
//
// Re-exported here so the two builders below, and existing `bootstrap::DB_*`
// callers, keep resolving.
pub use crate::budgets::{DB_IDLE_IN_TX_TIMEOUT_MS, DB_LOCK_TIMEOUT_MS, DB_STATEMENT_TIMEOUT_MS};

// `tx_session_setup_sql` and `autocommit_local_session_setup_sql` moved to
// `crate::backend::pg_session_sql` on 2026-09-01. They render PostgreSQL GUCs -
// `SET LOCAL ROLE`, statement_timeout, idle_in_transaction_session_timeout,
// lock_timeout - which is vendor DIALECT, while this module is tiered ENGINE.
//
// The move is a prerequisite, not tidiness: the roled autocommit funnel is going
// down into the PG tier to break the PG -> ENGINE cycle, and it calls the
// autocommit builder. Leaving the builder here would have re-formed that exact
// cycle under a different symbol.
//
// Re-exported so existing `bootstrap::*_session_setup_sql` callers keep
// resolving; a `pub use` names no type and adds no edge.
pub use crate::backend::pg_session_sql::{
    autocommit_local_session_setup_sql, tx_session_setup_sql,
};

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
/// 2. `GRANT USAGE ON SCHEMA "<app_id>"` - the runtime may enter the
///    schema but may not author objects in it.
/// 3. Grant sequence access for existing and future creator objects. Table DML
///    is deliberately absent: the binding's column grants are its authority,
///    and a table-level grant would subsume them.
/// 4. Revoke every reserved `__zeroship_*` table, then give the unmask audit
///    table exactly INSERT and its serial sequence exactly USAGE.
///
/// Explicitly does NOT grant `REPLICATION`, nor any privilege on another
/// app's schema. There is no privileged schema for it to reach: the
/// template carries schema membership only, not EXECUTE on any
/// definer-rights routine.
///
/// Runs under the caller's pool, which in production is the platform
/// (bootstrap) role — a superuser or CREATEROLE principal.
pub async fn ensure_per_app_role(pool: &Pool, app_id: &str) -> Result<PerAppRoleOutcome, DbError> {
    let role = per_app_role_name(app_id)?;
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

    // 2. Schema-level USAGE only. Runtime code never authors schema objects.
    pool.execute(&format!("GRANT USAGE ON SCHEMA {schema} TO {qrole}"), &[])
        .await
        .map_err(|e| coded_sql(&format!("GRANT USAGE ON SCHEMA {app_id}"), e))?;

    // 3. Sequences only. The binding layer owns column-level table grants.
    pool.execute(
        &format!("GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA {schema} TO {qrole}"),
        &[],
    )
    .await
    .map_err(|e| coded_sql(&format!("GRANT sequence usage ON SCHEMA {app_id}"), e))?;
    revoke_reserved_system_table_privileges(pool, app_id, &role).await?;
    set_worker_unmask_audit_append_privileges(pool, app_id, &role).await?;

    // 4. Future sequences. Future tables still require explicit column grants.
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

/// Remove every additive privilege from the exact unmask audit objects.
///
/// This runs as its own statement before the narrow grants. If a malformed
/// audit table makes the grant step fail, this deny remains committed instead
/// of rolling back with that failure. ACL-bearing objects of the wrong kind at
/// the reserved name also lose their blanket privileges and get nothing back.
fn revoke_worker_unmask_audit_privileges_sql(app_id: &str, role: &str) -> String {
    let schema_literal = sql_string_literal(app_id);
    let role_literal = sql_string_literal(role);
    let audit_literal = sql_string_literal(WORKER_WRITABLE_RESERVED_TABLE);
    format!(
        "DO $$ \
         DECLARE \
           audit_rel record; \
           sequence_rel record; \
         BEGIN \
           SELECT n.nspname, c.relname, c.relkind \
             INTO audit_rel \
             FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
            WHERE n.nspname = {schema_literal} \
              AND c.relname = {audit_literal} \
              AND c.relkind IN ('r', 'p', 'v', 'm', 'f', 'S'); \
           IF FOUND THEN \
             IF audit_rel.relkind = 'S' THEN \
               EXECUTE format( \
                 'REVOKE ALL PRIVILEGES ON SEQUENCE %I.%I FROM %I', \
                 audit_rel.nspname, audit_rel.relname, {role_literal} \
               ); \
             ELSE \
               EXECUTE format( \
                 'REVOKE ALL PRIVILEGES ON TABLE %I.%I FROM %I', \
                 audit_rel.nspname, audit_rel.relname, {role_literal} \
               ); \
               IF audit_rel.relkind IN ('r', 'p') THEN \
                 SELECT n.nspname, c.relname \
                   INTO sequence_rel \
                   FROM pg_class c \
                   JOIN pg_namespace n ON n.oid = c.relnamespace \
                  WHERE c.oid = pg_get_serial_sequence( \
                          format('%I.%I', audit_rel.nspname, audit_rel.relname), \
                          'id' \
                        )::regclass \
                    AND c.relkind = 'S'; \
                 IF FOUND THEN \
                   EXECUTE format( \
                     'REVOKE ALL PRIVILEGES ON SEQUENCE %I.%I FROM %I', \
                     sequence_rel.nspname, sequence_rel.relname, {role_literal} \
                   ); \
                 END IF; \
               END IF; \
             END IF; \
           END IF; \
         END \
         $$"
    )
}

/// Grant the unmask audit table its exact append-only recipe.
///
/// PostgreSQL grants are additive, so this runs only after
/// [`revoke_worker_unmask_audit_privileges_sql`] has cleared every table and
/// sequence privilege. The lookup is a no-op when the audit table has not been
/// provisioned yet, preserving [`ensure_per_app_role`]'s schema-only
/// precondition. A real audit table without its contractually required
/// `BIGSERIAL` sequence is rejected after the deny has committed.
fn grant_worker_unmask_audit_append_privileges_sql(app_id: &str, role: &str) -> String {
    let schema_literal = sql_string_literal(app_id);
    let role_literal = sql_string_literal(role);
    let audit_literal = sql_string_literal(WORKER_WRITABLE_RESERVED_TABLE);
    format!(
        "DO $$ \
         DECLARE \
           audit_rel record; \
           sequence_rel record; \
         BEGIN \
           SELECT n.nspname, c.relname \
             INTO audit_rel \
             FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
            WHERE n.nspname = {schema_literal} \
              AND c.relname = {audit_literal} \
              AND c.relkind IN ('r', 'p'); \
           IF FOUND THEN \
             SELECT n.nspname, c.relname \
               INTO sequence_rel \
               FROM pg_class c \
               JOIN pg_namespace n ON n.oid = c.relnamespace \
              WHERE c.oid = pg_get_serial_sequence( \
                      format('%I.%I', audit_rel.nspname, audit_rel.relname), \
                      'id' \
                    )::regclass \
                AND c.relkind = 'S'; \
             IF NOT FOUND THEN \
               RAISE EXCEPTION 'serial sequence missing for %.%.id', \
                 audit_rel.nspname, audit_rel.relname; \
             END IF; \
             EXECUTE format( \
               'GRANT USAGE ON SEQUENCE %I.%I TO %I', \
               sequence_rel.nspname, sequence_rel.relname, {role_literal} \
             ); \
             EXECUTE format( \
               'GRANT INSERT ON TABLE %I.%I TO %I', \
               audit_rel.nspname, audit_rel.relname, {role_literal} \
             ); \
           END IF; \
         END \
         $$"
    )
}

async fn set_worker_unmask_audit_append_privileges(
    pool: &Pool,
    app_id: &str,
    role: &str,
) -> Result<(), DbError> {
    pool.execute(
        &revoke_worker_unmask_audit_privileges_sql(app_id, role),
        &[],
    )
    .await
    .map_err(|e| coded_sql(&format!("revoke unmask audit privileges {app_id}"), e))?;
    pool.execute(
        &grant_worker_unmask_audit_append_privileges_sql(app_id, role),
        &[],
    )
    .await
    .map_err(|e| coded_sql(&format!("set unmask audit append privileges {app_id}"), e))?;
    Ok(())
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
    let role = per_app_role_name(app_id)?;
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
    fn set_local_role_sql_shape_is_quoted_and_correct() {
        assert_eq!(
            set_local_role_sql("app_demo").unwrap(),
            r#"SET LOCAL ROLE "app_app_demo_role""#
        );
    }

    /// The emitted statement must stay LOCAL.
    ///
    /// `SET ROLE` and `SET LOCAL ROLE` differ by one word and by whether the
    /// change survives the transaction. The session form leaves a pooled
    /// connection wearing the constrained role if the statement is cancelled
    /// before its `RESET`, so dropping `LOCAL` here would reintroduce the
    /// cancellation-unsafe pair deleted above - and every other assertion in
    /// this file would still pass.
    #[test]
    fn the_role_statement_is_transaction_scoped_not_session_scoped() {
        let sql = set_local_role_sql("app_demo").unwrap();
        assert!(
            sql.starts_with("SET LOCAL ROLE "),
            "the role fence must be transaction-scoped: {sql}",
        );
    }

    #[test]
    fn set_role_sql_doubles_embedded_quote_via_quote_ident() {
        // DB-15: the role name is the single statement enforcing per-tenant
        // role separation. It MUST flow through quote_ident (doubling any
        // embedded `"`), not a hand-written `"{}"` splice — even though app_id
        // is validated upstream, this boundary must not rely on that.
        assert_eq!(
            set_local_role_sql(r#"a"b"#).unwrap(),
            r#"SET LOCAL ROLE "app_a""b_role""#
        );
    }

    // The three session-setup SQL tests moved to
    // `crate::backend::pg_session_sql` with the functions they cover.

    #[test]
    fn create_role_attrs_assert_noreplication() {
        // §17.5 NON-NEGOTIABLE: the per-app CREATE ROLE attribute string
        // MUST contain NOREPLICATION. This is a source-level guard so a
        // future edit that drops the attribute (relying on the server
        // default) fails the test — the default is overridable per
        // cluster (`rolreplication` inheritance is subtle), so we assert
        // it explicitly. The literal lives in `ensure_per_app_role`;
        // mirror it here.
        let attrs = format!(
            "NOLOGIN NOREPLICATION NOCREATEDB NOCREATEROLE INHERIT IN ROLE \"{APP_ROLE_TEMPLATE}\""
        );
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
        format!(
            "{}.\"__zeroship_audit_unmask\"",
            crate::query::quote_ident(app)
        )
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
        per_app_role_name(app).expect("scratch app role name")
    }

    /// Named-object teardown only. `__zeroship_app_role_template` is
    /// deliberately left alone: `ensure_per_app_role` creates it idempotently,
    /// it is shared by every app on the cluster, and dropping it would break
    /// concurrent work on this server.
    async fn teardown(admin: &Client, app: &str) {
        let role =
            crate::query::quote_ident(&per_app_role_name(app).expect("scratch app role name"));
        let _ = admin
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS {} CASCADE",
                crate::query::quote_ident(app)
            ))
            .await;
        let _ = admin
            .batch_execute(&format!("DROP OWNED BY {role} CASCADE"))
            .await;
        let _ = admin
            .batch_execute(&format!("DROP ROLE IF EXISTS {role}"))
            .await;
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
            admin
                .batch_execute(&tx_session_setup_sql(app).expect("scratch app role name"))
                .await?;
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

    async fn assert_audit_verb_refused(what: &str, statement: impl FnOnce(&str) -> String) {
        let admin = admin_client().await;
        let app = scratch_app();
        teardown(&admin, &app).await;
        provision_scratch_app(&admin, &app).await;

        let pool = compio_postgres::Pool::connect(&test_dsn(), 2)
            .await
            .expect("pool for ensure_per_app_role");
        ensure_per_app_role(&pool, &app)
            .await
            .expect("provision the per-app role (this is what runs the sweep)");

        let outcome = as_runtime_role(&admin, &app, &statement(&app)).await;
        teardown(&admin, &app).await;

        let err = outcome.expect_err(&format!(
            "the runtime role must not be able to {what} the unmask audit table"
        ));
        assert!(
            denied(&err),
            "expected insufficient_privilege for audit {what}, got {:?}: {err}",
            err.code()
        );
    }

    #[compio::test]
    async fn a_wrong_kind_audit_relation_keeps_no_runtime_privileges() {
        let admin = admin_client().await;
        let app = scratch_app();
        teardown(&admin, &app).await;
        admin
            .batch_execute(&format!(
                "CREATE SCHEMA {}; \
                 CREATE VIEW {} AS SELECT 1::bigint AS id",
                crate::query::quote_ident(&app),
                audit_ref(&app),
            ))
            .await
            .expect("create a wrong-kind relation at the reserved audit name");

        let pool = compio_postgres::Pool::connect(&test_dsn(), 2)
            .await
            .expect("pool for ensure_per_app_role");
        ensure_per_app_role(&pool, &app)
            .await
            .expect("provision the per-app role over the wrong-kind relation");
        let role = per_app_role_name(&app).expect("scratch app role name");
        let privileges = admin
            .query_text_params(
                "SELECT privilege_type \
                   FROM information_schema.table_privileges \
                  WHERE grantee = $1 \
                    AND table_schema = $2 \
                    AND table_name = $3 \
                  ORDER BY privilege_type",
                &[role.as_str(), app.as_str(), WORKER_WRITABLE_RESERVED_TABLE],
            )
            .await
            .expect("query wrong-kind audit privileges")
            .into_iter()
            .map(|row| row.get::<_, String>("privilege_type"))
            .collect::<Vec<_>>();
        teardown(&admin, &app).await;

        assert!(
            privileges.is_empty(),
            "a non-table at the audit name must keep no worker privileges: {privileges:?}"
        );
    }

    #[compio::test]
    async fn a_malformed_audit_table_fails_closed_without_runtime_privileges() {
        let admin = admin_client().await;
        let app = scratch_app();
        teardown(&admin, &app).await;
        admin
            .batch_execute(&format!(
                "CREATE SCHEMA {}; \
                 CREATE TABLE {} (id BIGINT PRIMARY KEY)",
                crate::query::quote_ident(&app),
                audit_ref(&app),
            ))
            .await
            .expect("create an audit table without its required serial sequence");

        let pool = compio_postgres::Pool::connect(&test_dsn(), 2)
            .await
            .expect("pool for ensure_per_app_role");
        let outcome = ensure_per_app_role(&pool, &app).await;
        let role = per_app_role_name(&app).expect("scratch app role name");
        let privileges = admin
            .query_text_params(
                "SELECT privilege_type \
                   FROM information_schema.table_privileges \
                  WHERE grantee = $1 \
                    AND table_schema = $2 \
                    AND table_name = $3 \
                  ORDER BY privilege_type",
                &[role.as_str(), app.as_str(), WORKER_WRITABLE_RESERVED_TABLE],
            )
            .await
            .expect("query malformed audit table privileges")
            .into_iter()
            .map(|row| row.get::<_, String>("privilege_type"))
            .collect::<Vec<_>>();
        teardown(&admin, &app).await;

        assert!(
            outcome.is_err(),
            "a malformed audit table must be rejected after its privileges are denied"
        );
        assert!(
            privileges.is_empty(),
            "a malformed audit table must keep no worker privileges: {privileges:?}"
        );
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

        let table_privileges = admin
            .query_text_params(
                "SELECT privilege_type \
                   FROM information_schema.table_privileges \
                  WHERE grantee = $1 \
                    AND table_schema = $2 \
                    AND table_name = $3 \
                  ORDER BY privilege_type",
                &[role.as_str(), app.as_str(), WORKER_WRITABLE_RESERVED_TABLE],
            )
            .await
            .expect("query the audit table's exact grants")
            .into_iter()
            .map(|row| row.get::<_, String>("privilege_type"))
            .collect::<Vec<_>>();
        println!("information_schema.table_privileges for {role}: {table_privileges:?}");
        assert_eq!(
            table_privileges,
            vec!["INSERT"],
            "the runtime role must hold exactly INSERT on its audit table"
        );

        let audit = audit_ref(&app);
        let sequence_privileges = admin
            .query_text_params(
                "SELECT \
                    has_sequence_privilege($1, pg_get_serial_sequence($2, 'id'), 'USAGE') AS usage, \
                    has_sequence_privilege($1, pg_get_serial_sequence($2, 'id'), 'SELECT') AS sel, \
                    has_sequence_privilege($1, pg_get_serial_sequence($2, 'id'), 'UPDATE') AS upd",
                &[role.as_str(), audit.as_str()],
            )
            .await
            .expect("query the audit serial sequence's exact grants");
        let sequence_privileges = &sequence_privileges[0];
        assert!(
            sequence_privileges.get::<_, bool>("usage"),
            "the audit serial sequence needs USAGE for nextval"
        );
        assert!(
            !sequence_privileges.get::<_, bool>("sel"),
            "the audit serial sequence must not grant SELECT"
        );
        assert!(
            !sequence_privileges.get::<_, bool>("upd"),
            "the audit serial sequence must not grant UPDATE"
        );

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

    #[compio::test]
    async fn the_runtime_role_cannot_select_the_unmask_audit_log() {
        assert_audit_verb_refused("SELECT", |app| {
            format!("SELECT 1 FROM {} LIMIT 0", audit_ref(app))
        })
        .await;
    }

    #[compio::test]
    async fn the_runtime_role_cannot_update_the_unmask_audit_log() {
        assert_audit_verb_refused("UPDATE", |app| {
            format!("UPDATE {} SET reason = NULL WHERE false", audit_ref(app))
        })
        .await;
    }

    #[compio::test]
    async fn the_runtime_role_cannot_delete_from_the_unmask_audit_log() {
        assert_audit_verb_refused("DELETE FROM", |app| {
            format!("DELETE FROM {} WHERE false", audit_ref(app))
        })
        .await;
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
        admin
            .batch_execute(&format!(
                "GRANT INSERT ON TABLE {}.\"widgets\" TO {}",
                crate::query::quote_ident(&app),
                crate::query::quote_ident(&role),
            ))
            .await
            .expect("give the control table one explicit privilege");

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
