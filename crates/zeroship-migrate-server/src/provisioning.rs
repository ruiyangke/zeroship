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
use zeroship_core::app_derivation;
use zeroship_id::AppId;
use zeroship_migrate::ExecutorConfig;
use zeroship_migrate_postgres::confinement::PostgresConfinementExt;
use zeroship_migrate_postgres::role::migrator_role_name;

use crate::policy::confined_guard_policy_for_schema;

/// The precreated role granted workflow provisioning access to creator schemas.
///
/// The platform migration creates it
/// (`db/migrations-ts/20260818000200_worker_database_authority.ts`); nothing in
/// this service, and nothing in the worker, may create the role. Provisioning
/// preserves the creator migrator's ownership of an existing schema.
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

/// Error creating the data schema and its least-privilege migrator role.
#[derive(Debug, thiserror::Error)]
pub enum ProvisionDatabaseError {
    /// PostgreSQL refused creation of the data schema.
    #[error("database schema provision: {0}")]
    Schema(#[source] compio_postgres::Error),
    /// PostgreSQL refused or could not derive the migrator role.
    #[error(transparent)]
    Role(#[from] ProvisionRoleError),
}

fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

fn quote_lit(value: &str) -> String {
    value.replace('\'', "''")
}

/// Build the executor identity shared by explicit database creation and apply.
///
/// The runtime journal stays in the data schema, so `meta_schema` must be reset
/// after `ExecutorConfig::new` derives its default companion schema.
pub(crate) fn migrator_executor_config(
    schema: &str,
) -> Result<(ExecutorConfig, String), ProvisionRoleError> {
    let role = migrator_role_name(schema)
        .map_err(|_| ProvisionRoleError::BadRoleName(schema.to_string()))?;
    let mut config = ExecutorConfig::new(
        schema.to_string(),
        schema.to_string(),
        confined_guard_policy_for_schema(schema)
            .expect("embedded no-inject confined guard charter must bind and compose"),
    )
    .with_migrator_role(role.clone());
    config.confinement.meta_schema = schema.to_string();
    Ok((config, role))
}

/// Idempotently create one database's data schema and migrator role.
///
/// This is the complete create verb. Runtime roles, audit tables, workflow
/// grants, publications, and apply-ledger rows remain apply-time concerns.
pub async fn provision_database(
    admin: &Client,
    schema: &str,
) -> Result<(), ProvisionDatabaseError> {
    let (config, _) = migrator_executor_config(schema)?;
    admin
        .batch_execute(&format!(
            "CREATE SCHEMA IF NOT EXISTS {}",
            quote_ident(schema)
        ))
        .await
        .map_err(ProvisionDatabaseError::Schema)?;
    provision_migrator(admin, &config).await?;
    Ok(())
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
    let role = migrator_role_name(&cfg.project_id)
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
        exec_retry(
            admin,
            &format!("REVOKE ALL ON SCHEMA {ext_q} FROM {role_q}"),
        )
        .await?;
        exec_retry(admin, &format!("GRANT USAGE ON SCHEMA {ext_q} TO {role_q}")).await?;
    }

    Ok(())
}

/// The schema an app's durable-workflow journal tables live in.
///
/// Workflow journals share the creator's data schema. The canonical app
/// derivation keeps migration provisioning and runtime storage aligned.
#[must_use]
pub fn workflow_journal_schema_name(app_id: &AppId) -> String {
    app_derivation::schema_name(app_id)
}

/// Grant workflow provisioning access within an app's shared creator schema.
///
/// Guarded on the role existing rather than creating it: the migration identity
/// creates no platform role, it only delegates to roles the platform migration
/// precreated. An existing schema keeps its owner so creator migrations retain
/// their authority after runtime provisioning.
pub(crate) fn workflow_journal_schema_sql(app_schema: &str) -> String {
    let schema_q = quote_ident(app_schema);
    let owner_q = quote_ident(WORKFLOW_OWNER_ROLE);
    format!(
        "DO $workflow_journal$ BEGIN
            IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{owner_lit}') THEN
                CREATE SCHEMA IF NOT EXISTS {schema_q} AUTHORIZATION {owner_q};
                GRANT CREATE, USAGE ON SCHEMA {schema_q} TO {owner_q};
            END IF;
         END $workflow_journal$",
        owner_lit = quote_lit(WORKFLOW_OWNER_ROLE),
    )
}

/// Idempotently provision workflow access to an app's shared creator schema.
///
/// Runs [`workflow_journal_schema_sql`], the one generator for this DDL; the
/// apply path embeds the same text through its runtime role provisioning plan.
/// Creator database provisioning owns the schema lifecycle. This helper can
/// create a missing schema for privileged provisioning callers, but never
/// transfers ownership of an existing creator schema to the workflow role.
///
/// Exported because callers outside the apply path need a deployed app's
/// schema and grants to exist without running a creator migration. Callers must
/// hold the provisioning principal. Runtime app roles retain data privileges
/// without schema creation authority.
///
/// # Errors
/// Any database error from the DDL - including a permission failure, when the
/// connection is not the admin principal this expects.
pub async fn provision_workflow_journal_schema(
    admin: &Client,
    app_id: &AppId,
) -> Result<(), compio_postgres::Error> {
    exec_retry(
        admin,
        &workflow_journal_schema_sql(&workflow_journal_schema_name(app_id)),
    )
    .await
}

/// The unqualified name of the per-app unmask audit table.
///
/// The PostgreSQL and SQLite creators and the ORM writer are kept in sync by
/// `crates/zeroship-data-v8/tests/audit_table_parity.rs`.
pub const AUDIT_UNMASK_TABLE: &str = "__zeroship_audit_unmask";

/// The DDL that gives an app its unmask audit table, in the app's OWN schema.
///
/// # Why this is here and not in the worker
///
/// The migration service owns this DDL so the creator runtime emits no schema
/// changes. The table stays in the app schema and the runtime writes it through
/// ordinary parameterized SQL.
///
/// # Ordering against the runtime role
///
/// Runtime role provisioning must run after this table is created so its broad
/// table and sequence grants include the audit objects. The apply path repeats
/// role provisioning after migration execution for the same reason.
///
/// # Idempotence
///
/// `IF NOT EXISTS` throughout, so it is safe on every apply, which is how a
/// redeploy of an app provisioned before this table existed acquires it.
#[must_use]
pub fn audit_unmask_table_sql(app_schema: &str) -> String {
    let schema_q = quote_ident(app_schema);
    let schema_lit = quote_lit(app_schema);
    let table_q = quote_ident(AUDIT_UNMASK_TABLE);
    let table_lit = quote_lit(AUDIT_UNMASK_TABLE);
    format!(
        r#"CREATE TABLE IF NOT EXISTS {schema_q}.{table_q} (
            id              BIGSERIAL PRIMARY KEY,
            ts              TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            actor_id        TEXT NULL,
            actor_role      TEXT NULL,
            -- The actor claim the DB-3 fence REFUSED, verbatim, as sent.
            --
            -- `sanitize_app_actor` strips a claim naming a reserved system kind
            -- so app code cannot impersonate the platform. Stripping it also
            -- erased the only evidence anyone tried: a forged `kind: "auto"`
            -- and a caller who sent no actor both arrived as `actor: None` and
            -- audited identically. This column keeps the attempt without ever
            -- letting it reach `actor_id` / `actor_role`, which stay empty for
            -- an unauthenticated call. UNTRUSTED - it is what a handler sent.
            claimed_actor   TEXT NULL,
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
            ON {schema_q}.{table_q} (row_pk, "column", ts);
        DO $audit_unmask_contract$
        BEGIN
            IF pg_get_serial_sequence(
                format('%I.%I', '{schema_lit}', '{table_lit}'),
                'id'
            ) IS NULL THEN
                RAISE EXCEPTION 'serial sequence missing for %.%.id',
                    '{schema_lit}', '{table_lit}';
            END IF;
        END
        $audit_unmask_contract$;"#
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
    use uuid::Uuid;

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
        assert!(
            sql.contains(&format!(r#""app"."{AUDIT_UNMASK_TABLE}""#)),
            "{sql}"
        );
    }

    #[compio::test]
    async fn a_preexisting_audit_table_without_its_identity_sequence_is_refused() {
        let client = crate::test_database::connect().await;

        let schema = Uuid::new_v4().to_string();
        let schema_q = quote_ident(&schema);
        let table_q = quote_ident(AUDIT_UNMASK_TABLE);
        client
            .batch_execute(&format!(
                "CREATE SCHEMA {schema_q};
                 CREATE TABLE {schema_q}.{table_q} (
                    id BIGINT PRIMARY KEY,
                    ts TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                    actor_id TEXT,
                    actor_role TEXT,
                    claimed_actor TEXT,
                    collection TEXT NOT NULL,
                    row_pk TEXT NOT NULL,
                    \"column\" TEXT NOT NULL,
                    classification TEXT NOT NULL,
                    reason TEXT,
                    request_id TEXT,
                    outcome TEXT NOT NULL CHECK (outcome IN ('granted', 'denied'))
                 );"
            ))
            .await
            .expect("create malformed audit fixture");

        let error = provision_audit_unmask_table(&client, &schema)
            .await
            .expect_err("a missing identity sequence must fail provisioning");
        client
            .batch_execute(&format!("DROP SCHEMA {schema_q} CASCADE"))
            .await
            .expect("remove malformed audit fixture");

        let message = error
            .as_db_error()
            .map(compio_postgres::error::DbError::message)
            .unwrap_or_default();
        assert!(message.contains("serial sequence missing"), "{message}");
    }
}
