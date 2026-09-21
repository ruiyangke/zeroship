//! Per-project migrator-role + schema provisioning, over the raw compio
//! `Client`.
//!
//! The published `zero-migrate` engine is driver-free, so it cannot own this
//! DDL: it exports only
//! [`migrator_role_name`](zeroship_migrate_postgres::role::migrator_role_name)
//! (pure identifier derivation), and this module supplies the DDL that
//! establishes the least-privilege `migrator_<project>_<hash>` role.
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
//!   with `CREATE, USAGE` on it. The engine journal lives in that same schema
//!   under the `__zeroship_` prefix, so the migrator owns the journal too. That is
//!   accepted: an owner's privileges are implicit and cannot be revoked away, so
//!   the only honest position is that the record of what ran belongs to the tenant
//!   whose schema it describes. The platform keeps no counter-record, so this
//!   journal is the only record of what ran - and it belongs to the tenant.
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
use zeroship_migrate::ExecutorConfig;
use zeroship_migrate_postgres::confinement::PostgresConfinementExt;
use zeroship_migrate_postgres::role::migrator_role_name;

use crate::policy::confined_guard_policy_for_schema;

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

/// Build the executor identity for a schema whose owning role is already
/// named.
///
/// A creator database's schema is created by the cluster reconciler and owned
/// by `zs_db_<dbs>_mig`, a name derived from the DATABASE. Re-deriving an owner
/// from the schema text here would compose a different role, and
/// [`provision_migrator`] would then hand the reconciler's schema to it - which
/// the reconciler's next pass takes straight back, leaving objects owned by one
/// role inside a schema owned by another.
///
/// The runtime journal stays in the data schema, so `meta_schema` must be reset
/// after `ExecutorConfig::new` derives its default companion schema.
pub(crate) fn migrator_executor_config_for_role(schema: &str, role: &str) -> ExecutorConfig {
    let mut config = ExecutorConfig::new(
        schema.to_string(),
        schema.to_string(),
        confined_guard_policy_for_schema(schema)
            .expect("embedded no-inject confined guard charter must bind and compose"),
    )
    .with_migrator_role(role.to_string());
    config.confinement.meta_schema = schema.to_string();
    config
}

/// The same identity for a PLATFORM schema, whose owner is derived from the
/// schema because no other entity names it.
pub(crate) fn migrator_executor_config(
    schema: &str,
) -> Result<(ExecutorConfig, String), ProvisionRoleError> {
    let role = migrator_role_name(schema)
        .map_err(|_| ProvisionRoleError::BadRoleName(schema.to_string()))?;
    Ok((migrator_executor_config_for_role(schema, &role), role))
}

/// Idempotently create one schema and the least-privilege migrator role that
/// owns it.
///
/// Schema-addressed, and it serves the PLATFORM schema-bundle path, whose
/// schemas no database entity names and whose owner is therefore derived from
/// the schema text. A CREATOR database's schema is not created here: the
/// cluster reconciler owns that, because the schema's owner and its two
/// capability roles are derived from the database id and must be minted in one
/// transaction with the epoch row that names them.
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
    // THE ROLE COMES OFF THE CONFIG, not off a second derivation. The engine
    // brackets its DDL in the config's `migrator_role`, so a role provisioned
    // under any other name would be established, granted and made an owner
    // while the apply ran as something else.
    let role = zeroship_migrate_postgres::confinement::of(cfg)
        .migrator_role
        .clone()
        .ok_or_else(|| ProvisionRoleError::BadRoleName(cfg.project_id.clone()))?;
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

    // No REVOKE on the meta schema: the journal lives IN the project schema,
    // which this role OWNS, so there is nothing to deny - owner privileges are
    // implicit and cannot be revoked away. A creator can destroy their own
    // journal, which is accepted - it is their database and corrupting it
    // breaks only them - and the platform keeps no counter-record to
    // fall back on.
    //
    // A revoke here would not be merely vacuous: with
    // `meta_schema == project_schema` it would name the PROJECT schema and undo
    // the CREATE, USAGE grant above, so every apply would fail with
    // `permission denied for schema <app_uuid>`.

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

/// The grants that let a bound session write an unmask audit row.
///
/// # Why the audit table is granted here and creator tables are not
///
/// This table is PLATFORM DDL inside a creator schema: its shape is fixed, it
/// carries no classified column, and the data plane writes it through the same
/// narrowed session that read the row being unmasked. Which COLUMNS of a
/// CREATOR table each capability may touch is derived from the owner's own
/// migration IR, so those grants are per column and belong with the DDL that
/// creates them. This one is per table and belongs with the DDL above.
///
/// **Both capabilities, including read-only.** An unmask is a READ that
/// produced plaintext, so a read-only binding performs them and its audit row
/// has to land. `INSERT` and the sequence are the whole grant: no `SELECT`, so
/// no session can read another actor's audit trail back through the app.
#[must_use]
pub fn audit_unmask_capability_grants_sql(schema: &str, readwrite: &str, readonly: &str) -> String {
    let schema_q = quote_ident(schema);
    let table_q = quote_ident(AUDIT_UNMASK_TABLE);
    let readwrite_q = quote_ident(readwrite);
    let readonly_q = quote_ident(readonly);
    let schema_lit = quote_lit(schema);
    let table_lit = quote_lit(AUDIT_UNMASK_TABLE);
    let readwrite_lit = quote_lit(readwrite);
    let readonly_lit = quote_lit(readonly);
    format!(
        r"GRANT INSERT ON {schema_q}.{table_q} TO {readwrite_q}, {readonly_q};
        DO $audit_unmask_grant$
        DECLARE
            audit_sequence text;
        BEGIN
            audit_sequence := pg_get_serial_sequence(
                format('%I.%I', '{schema_lit}', '{table_lit}'),
                'id'
            );
            IF audit_sequence IS NULL THEN
                RAISE EXCEPTION 'serial sequence missing for %.%.id',
                    '{schema_lit}', '{table_lit}';
            END IF;
            EXECUTE format(
                'GRANT USAGE, SELECT ON SEQUENCE %s TO %I, %I',
                audit_sequence, '{readwrite_lit}', '{readonly_lit}'
            );
        END
        $audit_unmask_grant$;"
    )
}

/// Idempotently give a database's capability roles the audit-write grant.
///
/// # Errors
/// Any database error, including a missing identity sequence on a table that
/// predates the contract above.
pub async fn grant_audit_unmask_to_capabilities(
    admin: &Client,
    schema: &str,
    readwrite: &str,
    readonly: &str,
) -> Result<(), compio_postgres::Error> {
    exec_retry(
        admin,
        &audit_unmask_capability_grants_sql(schema, readwrite, readonly),
    )
    .await
}

/// Idempotently establish an app's unmask audit table, as an admin principal.
///
/// Runs [`audit_unmask_table_sql`], the one generator for this DDL. Exported so a
/// caller that needs the table reaches this statement rather than writing a
/// `CREATE TABLE` of its own - otherwise a test proves the shape of its own
/// fixture instead of the shape production builds.
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
