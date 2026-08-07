//! Per-project migrator-role + schema provisioning, over the raw compio
//! `Client`.
//!
//! Phase F Stage 3 moved this into the service. The published `zero-migrate`
//! engine is driver-free and no longer exports a `provision_migrator` free
//! function (it was `&compio_postgres::Client`-typed, so it did not survive the
//! engine's decoupling from a concrete network driver). The engine still exports
//! [`migrator_role_name`](zero_migrate::migrator_role_name) (pure identifier
//! derivation); this module supplies the DDL that establishes the least-privilege
//! `migrator_<project>_<hash>` role.
//!
//! The provisioning runs over the SAME raw compio `Client` the service already
//! holds (borrowed from the [`CompioPgSession`](zeroship_migrate_adapter::CompioPgSession)
//! that wraps it for the engine's `SqlSession` seam) — the engine's neutral
//! apply path never provisions; that is the platform admin principal's job.
//!
//! # The grant set (least privilege — line-2 DB-privilege defense)
//!
//! The migrator role gets EXACTLY:
//! - `NOSUPERUSER NOCREATEROLE NOCREATEDB NOLOGIN NOBYPASSRLS` — no escalation
//!   surface.
//! - OWNS the project schema (its DDL + `ALTER DEFAULT PRIVILEGES` targets work),
//!   with `CREATE, USAGE` on it.
//! - NO access whatsoever to the meta schema — the journal is unforgeable by
//!   deny-by-absence (all journal I/O runs as the admin role).
//! - `search_path` = project schema FIRST, then extension schema(s) (default
//!   `public`, resolution-only) so unqualified `vector(N)`/`geography(...)`
//!   resolve. Meta schema stays OFF the path.
//! - `REVOKE ALL` then `GRANT USAGE` on the extension schema(s): resolve the
//!   shared extension types, never create/write there.
//! - No grant on `control`/`auth`/`billing`/other project schemas — deny-by-absence.
//!
//! Every statement is idempotent (role creation guarded on `pg_roles`, all
//! GRANT/ALTER/REVOKE naturally idempotent), so it is safe to run on every apply.

use compio_postgres::Client;
use zero_migrate::ExecutorConfig;

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
    let role = zero_migrate::migrator_role_name(&cfg.project_id)
        .map_err(|_| ProvisionRoleError::BadRoleName(cfg.project_id.clone()))?;
    let role_q = quote_ident(&role);
    let role_lit = quote_lit(&role);
    let proj_q = quote_ident(&cfg.project_schema);
    let meta_q = quote_ident(&cfg.pg.meta_schema);

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

    // 5. The migrator must have NO access to the meta schema (journal is
    //    unforgeable by deny-by-absence). REVOKE explicitly + idempotently.
    exec_retry(
        admin,
        &format!(
            "DO $revoke$ BEGIN
                IF EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = '{meta_lit}') THEN
                    EXECUTE 'REVOKE ALL ON ALL TABLES IN SCHEMA {meta_q} FROM {role_q}';
                    EXECUTE 'REVOKE ALL ON SCHEMA {meta_q} FROM {role_q}';
                END IF;
             END $revoke$",
            meta_lit = quote_lit(&cfg.pg.meta_schema),
        ),
    )
    .await?;

    // 6. Pin search_path: project schema FIRST, then extension schema(s).
    let mut path_parts = vec![proj_q.clone()];
    for ext in &cfg.pg.extension_schemas {
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
    for ext in &cfg.pg.extension_schemas {
        if ext == &cfg.project_schema {
            continue;
        }
        let ext_q = quote_ident(ext);
        exec_retry(admin, &format!("REVOKE ALL ON SCHEMA {ext_q} FROM {role_q}")).await?;
        exec_retry(admin, &format!("GRANT USAGE ON SCHEMA {ext_q} TO {role_q}")).await?;
    }

    Ok(())
}
