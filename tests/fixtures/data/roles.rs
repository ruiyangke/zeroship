//! Privileged PostgreSQL provisioning used by owned test databases.
#![allow(dead_code)]
use compio_postgres::Pool;

use zeroship_core::database_role::per_app_role_name;

pub(crate) const APP_ROLE_TEMPLATE: &str = "__zeroship_app_role_template";

use zeroship_data_orm::backend::pg_error;

use zeroship_data_orm::error::DbError;

pub(crate) const RESERVED_SYSTEM_TABLE_PREFIX: &str = "__zeroship_";

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
///
/// Read from [`zeroship_data_sql::internal::AUDIT_UNMASK_TABLE`] rather than restated:
/// the grant recipe and the INSERT that uses the grant must name one relation,
/// and until 2026-09-04 they were two independent literals in two files.
pub(crate) const WORKER_WRITABLE_RESERVED_TABLE: &str =
    zeroship_data_sql::internal::AUDIT_UNMASK_TABLE;

/// Wrap a `compio_postgres::Error` in [`DbError`] with a context phrase
/// so operators see *what* the bootstrap layer was doing when the SQL
/// failed. The SQLSTATE classification still drives the `.code`
/// (`unique_violation`, `serialization_failure`, `transient`, …) — this
/// helper only prepends `"auth/bootstrap: <ctx>: "` to the message body.
///
/// Variant-walking is shared with the other per-module helpers via
/// [`zeroship_data_orm::backend::pg_error::coded_sql`]; this is the
/// `auth/bootstrap`-scoped
/// thin wrapper.
pub(crate) fn coded_sql(context: &str, e: compio_postgres::Error) -> DbError {
    pg_error::coded_sql(&format!("auth/bootstrap: {context}"), e)
}

/// Tiny helper for the existence + create pattern.
///
/// Postgres doesn't have `CREATE ROLE IF NOT EXISTS` (since the
/// attributes might differ from the existing role); we probe
/// `pg_roles` first, then issue the CREATE only when missing. The
/// `attrs` string is appended verbatim to the CREATE ROLE statement —
/// callers pass identifier-clean literals, no user input flows here.
pub(crate) async fn create_role_if_missing(
    pool: &Pool,
    name: &str,
    attrs: &str,
) -> Result<bool, DbError> {
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

pub(crate) fn sql_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

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
pub fn set_local_role_sql(app_id: &str) -> Result<String, DbError> {
    let role = per_app_role_name(app_id)?;
    Ok(format!(
        "SET LOCAL ROLE {}",
        zeroship_data_sql::compile::quote_ident(&role)
    ))
}

/// Result of `ensure_per_app_role` (which is behind `test-helpers`, so a
/// default build has this type without its producer) — distinguishes "created the role
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
    let schema = zeroship_data_sql::compile::quote_ident(app_id);
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
pub(crate) fn revoke_reserved_system_table_privileges_sql(app_id: &str, role: &str) -> String {
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
pub(crate) fn revoke_worker_unmask_audit_privileges_sql(app_id: &str, role: &str) -> String {
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
pub(crate) fn grant_worker_unmask_audit_append_privileges_sql(app_id: &str, role: &str) -> String {
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

pub(crate) async fn set_worker_unmask_audit_append_privileges(
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

pub(crate) async fn revoke_reserved_system_table_privileges(
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
