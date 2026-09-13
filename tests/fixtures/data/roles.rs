//! Test-host helpers for provisioning per-app PostgreSQL roles.
//!
//! The helper keeps schema isolation and DDL authority separate from table
//! names. Production role provisioning belongs to the migration service.
#![allow(dead_code)]

use compio_postgres::Pool;
use zeroship_core::database_role::per_app_role_name;
use zeroship_data_orm::{backend::pg_error, error::DbError};

pub(crate) const APP_ROLE_TEMPLATE: &str = "__zeroship_app_role_template";

/// Add database context while preserving the driver's SQLSTATE classification.
pub(crate) fn coded_sql(context: &str, error: compio_postgres::Error) -> DbError {
    pg_error::coded_sql(&format!("auth/bootstrap: {context}"), error)
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

/// Build the transaction-scoped role switch used on checked-out connections.
pub fn set_local_role_sql(app_id: &str) -> Result<String, DbError> {
    let role = per_app_role_name(app_id)?;
    Ok(format!(
        "SET LOCAL ROLE {}",
        zeroship_data_orm::sql::mapping::quote_ident(&role)
    ))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PerAppRoleOutcome {
    /// Whether this call created the role.
    pub created_role: bool,
}

/// Provision a runtime role with ordinary DML across its creator schema.
///
/// The role may use the schema and its sequences, but it cannot create schema
/// objects. Default privileges give later tables the same DML surface.
pub async fn ensure_per_app_role(pool: &Pool, app_id: &str) -> Result<PerAppRoleOutcome, DbError> {
    let role = per_app_role_name(app_id)?;
    let schema = zeroship_data_orm::sql::mapping::quote_ident(app_id);
    let quoted_role = zeroship_data_orm::sql::mapping::quote_ident(&role);

    create_role_if_missing(
        pool,
        APP_ROLE_TEMPLATE,
        "NOLOGIN NOREPLICATION NOCREATEDB NOCREATEROLE NOINHERIT",
    )
    .await?;
    let created = create_role_if_missing(
        pool,
        &role,
        &format!(
            "NOLOGIN NOREPLICATION NOCREATEDB NOCREATEROLE INHERIT IN ROLE \"{APP_ROLE_TEMPLATE}\""
        ),
    )
    .await?;

    for statement in [
        format!("GRANT USAGE ON SCHEMA {schema} TO {quoted_role}"),
        format!(
            "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA {schema} TO {quoted_role}"
        ),
        format!("GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA {schema} TO {quoted_role}"),
        format!(
            "ALTER DEFAULT PRIVILEGES IN SCHEMA {schema} \
             GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO {quoted_role}"
        ),
        format!(
            "ALTER DEFAULT PRIVILEGES IN SCHEMA {schema} \
             GRANT USAGE, SELECT ON SEQUENCES TO {quoted_role}"
        ),
    ] {
        pool.execute(&statement, &[])
            .await
            .map_err(|error| coded_sql(&format!("provision app role {app_id}"), error))?;
    }

    Ok(PerAppRoleOutcome {
        created_role: created,
    })
}

/// Drop a runtime role after its creator schema has been removed.
pub async fn drop_per_app_role(pool: &Pool, app_id: &str) -> Result<(), DbError> {
    let role = per_app_role_name(app_id)?;
    pool.execute(
        &format!(
            "DROP ROLE IF EXISTS {}",
            zeroship_data_orm::sql::mapping::quote_ident(&role)
        ),
        &[],
    )
    .await
    .map_err(|error| coded_sql(&format!("DROP ROLE {role}"), error))?;
    Ok(())
}
