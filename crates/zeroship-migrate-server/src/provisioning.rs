//! Per-project migrator-role + schema provisioning, over the raw compio
//! `Client`.
//!
//! Phase F Stage 3 moved this into the service. The published `zero-migrate`
//! engine is driver-free and no longer exports a `provision_migrator` free
//! function (it was `&compio_postgres::Client`-typed, so it did not survive the
//! engine's decoupling from a concrete network driver). The engine still exports
//! [`migrator_role_name`](zeroship_migrate_postgres::role::migrator_role_name) (pure identifier
//! derivation); this module supplies the DDL that establishes the least-privilege
//! `migrator_<project>_<hash>` role.
//!
//! The provisioning runs over the SAME raw compio `Client` the service already
//! holds (borrowed from the [`CompioPgSession`](crate::session::CompioPgSession)
//! that wraps it for the engine's `SqlSession` seam) — the engine's neutral
//! apply path never provisions; that is the platform admin principal's job.
//!
//! # The grant set (least privilege — line-2 DB-privilege defense)
//!
//! The migrator role gets EXACTLY:
//! - `NOSUPERUSER NOCREATEROLE NOCREATEDB NOLOGIN NOBYPASSRLS` — no escalation
//!   surface.
//! - OWNS the project schema (its DDL + `ALTER DEFAULT PRIVILEGES` targets work),
//!   with `CREATE, USAGE` on it. The engine journal now lives in that same schema
//!   under the `__zeroship_` prefix, so the migrator owns the journal too. That is
//!   accepted: an owner's privileges are implicit and cannot be revoked away, so
//!   the only honest position is that the record of what ran belongs to the tenant
//!   whose schema it describes. The platform keeps its own record in
//!   `zeroship.app_schema_applies` and never treats this one as a trust anchor.
//! - `search_path` = project schema FIRST, then extension schema(s) (default
//!   `public`, resolution-only) so unqualified `vector(N)`/`geography(...)`
//!   resolve.
//! - `REVOKE ALL` then `GRANT USAGE` on the extension schema(s): resolve the
//!   shared extension types, never create/write there.
//! - No grant on `control`/`auth`/`billing`/other project schemas — deny-by-absence.
//!
//! Every statement is idempotent (role creation guarded on `pg_roles`, all
//! GRANT/ALTER/REVOKE naturally idempotent), so it is safe to run on every apply.

use compio_postgres::Client;
use uuid::Uuid;
use zeroship_migrate::ExecutorConfig;

/// The narrow, precreated role that owns every app's workflow journal schema.
///
/// The platform migration creates it
/// (`db/migrations-ts/20260702000100_schema_roles_extensions.ts`); nothing in
/// this service, and nothing in the worker, may create it or a schema for it.
pub const WORKFLOW_OWNER_ROLE: &str = "zeroship_workflow_owner";

/// Error provisioning a migrator role.
#[derive(Debug, thiserror::Error)]
pub enum ProvisionRoleError {
    /// A database error during provisioning.
    #[error("role provisioning db error: {0}")]
    Db(#[from] compio_postgres::Error),
    /// The derived role name was empty or otherwise unusable.
    #[error("invalid migrator role name derived from project id '{0}'")]
    BadRoleName(String),
}

fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

fn quote_lit(value: &str) -> String {
    value.replace('\'', "''")
}

/// Retry a batch statement over the brief `tuple concurrently updated` catalog
/// contention that concurrent role/grant DDL can produce (matches the in-tree
/// `exec_retry`).
pub(crate) async fn exec_retry(admin: &Client, sql: &str) -> Result<(), compio_postgres::Error> {
    const MAX_ATTEMPTS: u32 = 8;
    let mut attempt = 0;
    loop {
        match admin.batch_execute(sql).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                let transient = e
                    .as_db_error()
                    .is_some_and(|db| db.message().contains("tuple concurrently updated"));
                attempt += 1;
                if transient && attempt < MAX_ATTEMPTS {
                    compio::time::sleep(std::time::Duration::from_millis(u64::from(attempt) * 10))
                        .await;
                    continue;
                }
                return Err(e);
            }
        }
    }
}

/// Idempotently provision the least-privilege `migrator` role for a project.
///
/// Run by an admin/control
/// principal with `CREATEROLE`. Establishes the role, makes it own the project
/// schema, pins its `search_path`, and revokes write reach on the meta + extension
/// schemas. The project schema is expected to exist (created by the caller before
/// this).
///
/// # Errors
/// [`ProvisionRoleError::BadRoleName`] if the project id yields no valid role name;
/// [`ProvisionRoleError::Db`] on any DDL failure (e.g. the caller lacks `CREATEROLE`).
pub async fn provision_migrator(
    admin: &Client,
    cfg: &ExecutorConfig,
) -> Result<(), ProvisionRoleError> {
    let role = zeroship_migrate_postgres::role::migrator_role_name(&cfg.project_id)
        .map_err(|_| ProvisionRoleError::BadRoleName(cfg.project_id.clone()))?;
    let role_q = quote_ident(&role);
    let role_lit = quote_lit(&role);
    let proj_q = quote_ident(&cfg.project_schema);

    // 1. Create the role idempotently with the locked-down attribute set.
    exec_retry(
        admin,
        &format!(
            "DO $prov$ BEGIN
                IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{role_lit}') THEN
                    EXECUTE 'CREATE ROLE {role_q} \
                             NOSUPERUSER NOCREATEROLE NOCREATEDB NOLOGIN NOBYPASSRLS';
                END IF;
             END $prov$"
        ),
    )
    .await?;

    // 2. The admin must be able to SET ROLE to the migrator.
    exec_retry(
        admin,
        &format!(
            "DO $mem$ BEGIN
                IF NOT pg_has_role(current_user, '{role_lit}', 'MEMBER') THEN
                    EXECUTE 'GRANT {role_q} TO ' || quote_ident(current_user);
                END IF;
             END $mem$"
        ),
    )
    .await?;

    // 3. The migrator OWNS the project schema.
    exec_retry(admin, &format!("ALTER SCHEMA {proj_q} OWNER TO {role_q}")).await?;

    // 4. CREATE + USAGE on the project schema (explicit + idempotent).
    exec_retry(
        admin,
        &format!("GRANT CREATE, USAGE ON SCHEMA {proj_q} TO {role_q}"),
    )
    .await?;

    // 4b. Default privileges for the migrator's own objects.
    exec_retry(
        admin,
        &format!(
            "ALTER DEFAULT PRIVILEGES FOR ROLE {role_q} IN SCHEMA {proj_q} \
             GRANT ALL ON TABLES TO {role_q}; \
             ALTER DEFAULT PRIVILEGES FOR ROLE {role_q} IN SCHEMA {proj_q} \
             GRANT ALL ON SEQUENCES TO {role_q};"
        ),
    )
    .await?;

    // THERE IS NO STEP 5 ANY MORE, and its removal is the change rather than an
    // omission. It used to REVOKE ALL on the meta schema from the migrator, so the
    // journal was unforgeable by deny-by-absence. The journal now lives IN the
    // project schema, which this role OWNS, so there is nothing left to deny:
    // owner privileges are implicit and cannot be revoked away. A creator can
    // destroy their own journal, which is accepted - it is their database and
    // corrupting it breaks only them - and it is why the platform keeps its own
    // record in `zeroship.app_schema_applies` rather than trusting this one.
    //
    // KEEPING IT WAS NOT MERELY VACUOUS, WHICH IS WHY THIS NOTE EXISTS. With
    // `meta_schema == project_schema` the revoke named the PROJECT schema and
    // undid step 4 four statements earlier, so every apply failed with
    // `permission denied for schema <app_uuid>`. Measured 2026-08-28 against a
    // live PostgreSQL 16: `apply_api_accepts_apps_migrate_owner_and_applies_ir_pg`
    // answered 422 with exactly that message until this block was deleted.

    // 6. Pin search_path: project schema FIRST, then extension schema(s).
    let mut path_parts = vec![proj_q.clone()];
    for ext in &zeroship_migrate_postgres::confinement::of(cfg).extension_schemas {
        if ext != &cfg.project_schema {
            path_parts.push(quote_ident(ext));
        }
    }
    exec_retry(
        admin,
        &format!(
            "ALTER ROLE {role_q} SET search_path = {}",
            path_parts.join(", ")
        ),
    )
    .await?;

    // 7. Confine the extension schema(s) to RESOLUTION-ONLY (REVOKE ALL, GRANT USAGE).
    for ext in &zeroship_migrate_postgres::confinement::of(cfg).extension_schemas {
        if ext == &cfg.project_schema {
            continue;
        }
        let ext_q = quote_ident(ext);
        exec_retry(admin, &format!("REVOKE ALL ON SCHEMA {ext_q} FROM {role_q}")).await?;
        exec_retry(admin, &format!("GRANT USAGE ON SCHEMA {ext_q} TO {role_q}")).await?;
    }

    Ok(())
}

/// The schema an app's durable-workflow journal tables live in: `app_<uuid>`.
///
/// Distinct from the app's DATA schema, which is the bare `<uuid>`. Kept in
/// sync with `zeroship_plugin_workflow::store::pg::app_schema_for`, which
/// derives the same name on the read/write side; this crate does not depend on
/// that one, so the derivation is duplicated rather than shared.
#[must_use]
pub fn workflow_journal_schema_name(app_id: &Uuid) -> String {
    format!("app_{}", app_id.as_hyphenated())
}

/// The DDL that gives an app its workflow journal schema, owned by the narrow
/// [`WORKFLOW_OWNER_ROLE`].
///
/// Guarded on the role existing rather than creating it: the migration identity
/// creates no platform role, it only delegates to roles the platform migration
/// precreated.
pub(crate) fn workflow_journal_schema_sql(app_schema: &str) -> String {
    let schema_q = quote_ident(app_schema);
    let owner_q = quote_ident(WORKFLOW_OWNER_ROLE);
    format!(
        "DO $workflow_journal$ BEGIN
            IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{owner_lit}') THEN
                CREATE SCHEMA IF NOT EXISTS {schema_q} AUTHORIZATION {owner_q};
                ALTER SCHEMA {schema_q} OWNER TO {owner_q};
                GRANT CREATE, USAGE ON SCHEMA {schema_q} TO {owner_q};
            END IF;
         END $workflow_journal$",
        owner_lit = quote_lit(WORKFLOW_OWNER_ROLE),
    )
}

/// Idempotently provision an app's workflow journal schema, as an admin
/// principal.
///
/// Runs [`workflow_journal_schema_sql`], the one generator for this DDL; the
/// apply path embeds the same text (`apply::runtime_dependents_sql`). Nothing
/// else in the platform creates `app_<uuid>`: the worker and the control plane
/// provision the journal TABLES into it (`PgStore::provision`) holding no
/// CREATE on the database and no authority to make a schema of their own - a
/// process running creator code must not be able to author schemas
/// (2a44ea8ef). So the schema has to exist first, and in production it exists
/// because a deploy's migration apply created it.
///
/// Exported because callers outside the apply path need a deployed app's
/// journal schema to exist and must get it from the production statement rather
/// than a CREATE SCHEMA of their own: control's workflow tests seed an app by
/// INSERTing into `zeroship.apps`, which skips the apply that would have called
/// this.
///
/// # Errors
/// Any database error from the DDL - including a permission failure, when the
/// connection is not the admin principal this expects.
pub async fn provision_workflow_journal_schema(
    admin: &Client,
    app_id: &Uuid,
) -> Result<(), compio_postgres::Error> {
    exec_retry(
        admin,
        &workflow_journal_schema_sql(&workflow_journal_schema_name(app_id)),
    )
    .await
}

/// The unqualified name of the per-app unmask audit table.
///
/// Shared with the data plane's INSERT (`zeroship-plugin-db`'s
/// `crud/unmask.rs`), which is the only writer. The SQLite peer of this constant
/// is `zeroship_migrate_sqlite::backend::AUDIT_UNMASK_TABLE`.
pub const AUDIT_UNMASK_TABLE: &str = "__zeroship_audit_unmask";

/// The DDL that gives an app its unmask audit table, in the app's OWN schema.
///
/// # Why this is here and not in the worker
///
/// It used to be in the worker. `crud/unmask.rs` called
/// `ensure_audit_unmask_table` from `write_audit_unmask_row`, so every single
/// `unmask()` dispatch - granted AND denied - issued this `CREATE TABLE IF NOT
/// EXISTS` plus three `CREATE INDEX IF NOT EXISTS` before it could log anything.
/// Eight DDL statements on the privileged read path, emitted by the process that
/// executes creator code. Schema change belongs to `zeroship-migrate`; the data
/// plane emits none.
///
/// Deleting the DDL without moving it was not an option: the audit row is where
/// authorization and provenance for a plaintext read live, so dropping the
/// writer's dependency while keeping the writer would have kept the call and
/// lost the record. This function is the Postgres creator; the SQLite creator is
/// `zeroship_migrate_sqlite::backend::audit_unmask_sql`.
///
/// # Placement
///
/// The APP'S OWN SCHEMA, beside `__zeroship_schema_migrations` and its siblings,
/// not `__zeroship_admin`. That is not a weakening: the platform's system schema
/// is for state a separate service WRITES and the worker only READS, and this
/// table is the other way round. The worker is the sole writer, over ordinary
/// parameterised SQL, with provenance enforced at the Rust call boundary rather
/// than at the SQL boundary - the position `zeroship-plugin-db`'s `audit.rs`
/// already argues for the sibling audit log, and the reason neither needs a
/// `SECURITY DEFINER` wrapper. App-scoped audit data also stays queryable by an
/// operator holding only the app's schema.
///
/// # Ordering against the runtime role
///
/// This must run BEFORE THE LAST `apply::provision_runtime_app_role`. That
/// function explicitly finds the audit table and its owned serial sequence,
/// clears every additive privilege, then grants only table INSERT and sequence
/// USAGE. Both lookups are no-ops while the table is absent, so a table created
/// after the last call would be unreachable to the worker.
///
/// "THE LAST" IS THE ACCURATE READING, and this paragraph said "BEFORE
/// `provision_runtime_app_role`" until it was measured. `apply_ir_request` calls
/// that function TWICE - once before `apply_sealed` and once after - so a table
/// created between the two is granted by the second call and stays
/// reachable. The strict sentence describes a constraint the code does not
/// actually impose. `live_audit_unmask_provisioning::
/// the_audit_table_must_be_provisioned_before_the_runtime_role` rules on all
/// three positions against a live catalog; its third arm is what would go red
/// if the second call were ever removed.
///
/// THAT CASE PROVES THE CONSTRAINT, NOT THE CALL ORDER. It runs the two
/// functions itself, so swapping them inside `apply_ir_request` leaves it green.
/// The order that ships is bound by `apply_api_test::
/// a_real_apply_leaves_the_runtime_role_able_to_write_the_unmask_audit_row_pg`,
/// which applies through the HTTP surface and then writes an audit row by the
/// worker's own identity chain. Both are needed and neither replaces the other:
/// the first says WHY the order matters, the second says the code still has it.
///
/// # Idempotence
///
/// `IF NOT EXISTS` throughout, so it is safe on every apply, which is how a
/// redeploy of an app provisioned before this table existed acquires it.
#[must_use]
pub fn audit_unmask_table_sql(app_schema: &str) -> String {
    let schema_q = quote_ident(app_schema);
    let table_q = quote_ident(AUDIT_UNMASK_TABLE);
    format!(
        r#"CREATE TABLE IF NOT EXISTS {schema_q}.{table_q} (
            id              BIGSERIAL PRIMARY KEY,
            ts              TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            actor_id        TEXT NULL,
            actor_role      TEXT NULL,
            collection      TEXT NOT NULL,
            row_pk          TEXT NOT NULL,
            "column"        TEXT NOT NULL,
            classification  TEXT NOT NULL,
            reason          TEXT NULL,
            request_id      TEXT NULL,
            outcome         TEXT NOT NULL CHECK (outcome IN ('granted', 'denied'))
        );
        CREATE INDEX IF NOT EXISTS "{AUDIT_UNMASK_TABLE}_ts_idx"
            ON {schema_q}.{table_q} (ts);
        CREATE INDEX IF NOT EXISTS "{AUDIT_UNMASK_TABLE}_actor_idx"
            ON {schema_q}.{table_q} (actor_id, ts);
        CREATE INDEX IF NOT EXISTS "{AUDIT_UNMASK_TABLE}_row_idx"
            ON {schema_q}.{table_q} (row_pk, "column", ts);"#
    )
}

/// Idempotently establish an app's unmask audit table, as an admin principal.
///
/// Runs [`audit_unmask_table_sql`], the one generator for this DDL. Exported for
/// the same reason [`provision_workflow_journal_schema`] is: a caller that needs
/// a deployed app's audit table to exist must get it from the production
/// statement rather than a `CREATE TABLE` of its own, or the test proves the
/// shape of its own fixture instead of the shape production builds.
///
/// # Errors
/// Any database error from the DDL, including a permission failure when the
/// connection is not the admin principal this expects.
pub async fn provision_audit_unmask_table(
    admin: &Client,
    app_schema: &str,
) -> Result<(), compio_postgres::Error> {
    exec_retry(admin, &audit_unmask_table_sql(app_schema)).await
}

#[cfg(test)]
mod audit_unmask_tests {
    use super::*;

    /// The identifier goes through `quote_ident`, which doubles embedded quotes.
    /// The app schema is a UUID in production, but this generator is exported and
    /// a caller passing anything else must not be able to break out of the
    /// identifier.
    #[test]
    fn app_schema_is_quoted_not_interpolated_raw() {
        let sql = audit_unmask_table_sql("a\"; DROP SCHEMA public; --");
        assert!(
            sql.contains(r#""a""; DROP SCHEMA public; --""#),
            "the schema must survive as ONE doubled-quote identifier; got: {sql}"
        );
        assert!(
            !sql.contains("\n        DROP SCHEMA"),
            "no statement boundary may be reachable from the schema name: {sql}"
        );
    }

    /// Re-run on every apply, so every statement has to be re-runnable.
    #[test]
    fn every_statement_is_idempotent() {
        let sql = audit_unmask_table_sql("app");
        assert_eq!(
            sql.matches("IF NOT EXISTS").count(),
            4,
            "one table + three indexes, all IF NOT EXISTS: {sql}"
        );
    }

    /// The data plane INSERTs into the name this constant carries; if the DDL
    /// stopped naming it, every unmask would fail at runtime rather than here.
    #[test]
    fn ddl_creates_the_table_the_constant_names() {
        let sql = audit_unmask_table_sql("app");
        assert!(sql.contains(&format!(r#""app"."{AUDIT_UNMASK_TABLE}""#)), "{sql}");
    }
}
