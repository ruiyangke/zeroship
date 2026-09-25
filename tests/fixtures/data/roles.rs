//! Test-host helpers for provisioning one binding's `PostgreSQL` role ladder.
//!
//! The ladder is the production one, composed through the same
//! `zeroship_core::database_derivation` functions the cluster reconciler
//! (`zeroship_migrate_server::datastore::cluster`) creates it with. A fixture
//! that hand-spelled a role name would provision an object the data plane never
//! asks for, and every narrow would fail at `SET LOCAL ROLE` instead of
//! measuring what the test is about.
//!
//! Which COLUMNS a capability role may touch is the apply path's business in
//! production; here the helper grants the whole schema, which is what a fixture
//! that creates its own tables needs.
#![allow(dead_code)]

use compio_postgres::Pool;
use zeroship_core::database_derivation;
use zeroship_core::database_role::DatabaseCapability;
use zeroship_data_orm::binding::DbBinding;
use zeroship_data_orm::{backend::pg_error, error::DbError};

/// Add database context while preserving the driver's SQLSTATE classification.
pub(crate) fn coded_sql(context: &str, error: compio_postgres::Error) -> DbError {
    pg_error::coded_sql(&format!("fixture/roles: {context}"), error)
}

pub(crate) async fn create_role_if_missing(
    pool: &Pool,
    name: &str,
    attrs: &str,
) -> Result<bool, DbError> {
    let exists = !pool
        .query_text_params("SELECT 1 FROM pg_roles WHERE rolname = $1", &[name])
        .await
        .map_err(|error| coded_sql(&format!("probe pg_roles {name}"), error))?
        .is_empty();
    if exists {
        return Ok(false);
    }
    pool.execute(&format!(r#"CREATE ROLE "{name}" {attrs}"#), &[])
        .await
        .map_err(|error| coded_sql(&format!("CREATE ROLE {name}"), error))?;
    Ok(true)
}

/// Build the transaction-scoped role switch the data plane sends.
pub fn set_local_role_sql(binding: &DbBinding) -> Result<String, DbError> {
    let role = binding.session_role().ok_or_else(|| {
        DbError::config(
            "binding_not_resolved",
            "a fixture narrowing to a role needs a binding that names one",
        )
    })?;
    Ok(format!(
        "SET LOCAL ROLE {}",
        zeroship_data_orm::sql::mapping::quote_ident(role)
    ))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BindingLadderOutcome {
    /// Whether this call created the binding role.
    pub created_role: bool,
}

/// Provision one binding's whole ladder: the schema, the capability role that
/// carries its privileges, the database's unmask role, the binding role that
/// reaches both, and the membership the connecting login assumes it through.
///
/// The grant options are the fence and are spelled here the way
/// `zeroship_migrate_server::datastore::cluster::grant_binding` spells them:
/// `WITH SET FALSE` on the binding-to-database edge so no session can assume
/// the capability role itself, `WITH INHERIT FALSE` on the binding-to-unmask
/// edge so a session that merely narrowed to the binding does not get a masked
/// field's real value out of an ordinary `SELECT`, and `WITH INHERIT FALSE` on
/// the login edge so the binding's privileges are never ambient on the
/// connection.
///
/// The unmask role must exist here even for a fixture that grants the whole
/// schema, because the data plane NAMES it: an audited raw-column read assumes
/// it for exactly that statement
/// (`zeroship_data_orm::backend::postgres::pg_session_sql::unmask_elevation_sql`),
/// so a ladder without it fails at `SET LOCAL ROLE` with `22023` rather than
/// measuring what its test is about.
pub async fn ensure_binding_ladder(
    pool: &Pool,
    binding: &DbBinding,
) -> Result<BindingLadderOutcome, DbError> {
    let database = binding.database().ok_or_else(|| {
        DbError::config(
            "binding_not_resolved",
            "a fixture ladder needs a binding that addresses a database",
        )
    })?;
    let binding_role = binding.session_role().ok_or_else(|| {
        DbError::config(
            "binding_not_resolved",
            "a fixture ladder needs a binding that names a role",
        )
    })?;
    let capability = database_derivation::capability_role_name(database, DatabaseCapability::ReadWrite)?;
    let unmask = database_derivation::unmask_role_name(database)?;

    let schema = zeroship_data_orm::sql::mapping::quote_ident(binding.schema().as_str());
    let capability_q = zeroship_data_orm::sql::mapping::quote_ident(&capability);
    let unmask_q = zeroship_data_orm::sql::mapping::quote_ident(&unmask);
    let binding_q = zeroship_data_orm::sql::mapping::quote_ident(binding_role);

    create_role_if_missing(
        pool,
        &capability,
        "NOLOGIN NOREPLICATION NOCREATEDB NOCREATEROLE INHERIT",
    )
    .await?;
    create_role_if_missing(
        pool,
        &unmask,
        "NOLOGIN NOREPLICATION NOCREATEDB NOCREATEROLE INHERIT",
    )
    .await?;
    let created = create_role_if_missing(
        pool,
        binding_role,
        "NOLOGIN NOREPLICATION NOCREATEDB NOCREATEROLE INHERIT",
    )
    .await?;

    for statement in [
        format!("CREATE SCHEMA IF NOT EXISTS {schema}"),
        format!("GRANT USAGE ON SCHEMA {schema} TO {capability_q}"),
        format!(
            "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA {schema} TO {capability_q}"
        ),
        format!("GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA {schema} TO {capability_q}"),
        format!(
            "ALTER DEFAULT PRIVILEGES IN SCHEMA {schema} \
             GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO {capability_q}"
        ),
        format!(
            "ALTER DEFAULT PRIVILEGES IN SCHEMA {schema} \
             GRANT USAGE, SELECT ON SEQUENCES TO {capability_q}"
        ),
        format!("GRANT USAGE ON SCHEMA {schema} TO {unmask_q}"),
        format!("GRANT SELECT ON ALL TABLES IN SCHEMA {schema} TO {unmask_q}"),
        format!(
            "ALTER DEFAULT PRIVILEGES IN SCHEMA {schema} \
             GRANT SELECT ON TABLES TO {unmask_q}"
        ),
        format!("GRANT {capability_q} TO {binding_q} WITH SET FALSE"),
        format!("GRANT {unmask_q} TO {binding_q} WITH INHERIT FALSE"),
        format!("GRANT {binding_q} TO CURRENT_USER WITH INHERIT FALSE"),
    ] {
        pool.execute(&statement, &[])
            .await
            .map_err(|error| {
                coded_sql(&format!("provision ladder for {}", binding.app_id()), error)
            })?;
    }

    Ok(BindingLadderOutcome {
        created_role: created,
    })
}

/// Drop a binding's roles after its schema has been removed.
pub async fn drop_binding_ladder(pool: &Pool, binding: &DbBinding) -> Result<(), DbError> {
    let mut roles = Vec::new();
    if let Some(role) = binding.session_role() {
        roles.push(role.to_owned());
    }
    if let Some(database) = binding.database() {
        roles.push(database_derivation::capability_role_name(
            database,
            DatabaseCapability::ReadWrite,
        )?);
        roles.push(database_derivation::unmask_role_name(database)?);
    }
    for role in roles {
        pool.execute(
            &format!(
                "DROP ROLE IF EXISTS {}",
                zeroship_data_orm::sql::mapping::quote_ident(&role)
            ),
            &[],
        )
        .await
        .map_err(|error| coded_sql(&format!("DROP ROLE {role}"), error))?;
    }
    Ok(())
}
