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
//!
//! The functions take the plain [`HarnessBinding`](super::HarnessBinding) and
//! return the driver's own error, so a consumer that classifies failures maps it
//! into its own error type rather than the testkit naming that type.

use super::{quote_ident, HarnessBinding};
use compio_postgres::Pool;
use zeroship_core::database_derivation;

async fn create_role_if_missing(
    pool: &Pool,
    name: &str,
    attrs: &str,
) -> Result<bool, compio_postgres::Error> {
    let exists = !pool
        .query_text_params("SELECT 1 FROM pg_roles WHERE rolname = $1", &[name])
        .await?
        .is_empty();
    if exists {
        return Ok(false);
    }
    pool.execute(&format!(r#"CREATE ROLE "{name}" {attrs}"#), &[])
        .await?;
    Ok(true)
}

/// Build the transaction-scoped role switch the data plane sends.
#[must_use]
pub fn set_local_role_sql(binding: &HarnessBinding) -> String {
    format!("SET LOCAL ROLE {}", quote_ident(&binding.session_role()))
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
    binding: &HarnessBinding,
) -> Result<BindingLadderOutcome, compio_postgres::Error> {
    let capability = database_derivation::capability_role_name(
        binding.database(),
        binding.capability(),
    )
    .expect("a minted database composes a legal capability role name");
    let unmask = binding.unmask_role();
    let binding_role = binding.session_role();

    let schema = quote_ident(&binding.schema());
    let capability_q = quote_ident(&capability);
    let unmask_q = quote_ident(&unmask);
    let binding_q = quote_ident(&binding_role);

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
        &binding_role,
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
        pool.execute(&statement, &[]).await?;
    }

    Ok(BindingLadderOutcome {
        created_role: created,
    })
}

/// Drop a binding's roles after its schema has been removed.
pub async fn drop_binding_ladder(
    pool: &Pool,
    binding: &HarnessBinding,
) -> Result<(), compio_postgres::Error> {
    let roles = vec![
        binding.session_role(),
        database_derivation::capability_role_name(binding.database(), binding.capability())
            .expect("a minted database composes a legal capability role name"),
        binding.unmask_role(),
    ];
    for role in roles {
        pool.execute(
            &format!("DROP ROLE IF EXISTS {}", quote_ident(&role)),
            &[],
        )
        .await?;
    }
    Ok(())
}
