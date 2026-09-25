//! Everything the reconciler does TO a cluster.
//!
//! The privilege invariant decides the shape of every statement here. The
//! worker runs creator code, so nothing creator code can assume may reach
//! schema change or another database. That is enforced by role membership in
//! the cluster's own catalog, not by a check in a process creator code shares:
//!
//! ```text
//! zs_db_<dbs>_mig     owns schema db_<dbs>; named by no binding
//! zs_db_<dbs>_rw      USAGE on db_<dbs>, plus the column-listed DML an apply emits
//! zs_db_<dbs>_ro      USAGE on db_<dbs>, plus the column-listed SELECT an apply emits
//! zs_db_<dbs>_unmask  USAGE on db_<dbs>, plus SELECT on the __zs_raw__ columns ALONE
//! zs_bind_<bnd>       NOLOGIN, no privileges of its own, inherits exactly ONE database role
//! zeroship_worker     LOGIN, may ASSUME each live binding role and inherits none of them
//! ```
//!
//! `crates/zeroship-data-orm/tests/postgres_tenant_fence.rs` measures that
//! ladder against the deploy pin. The three grant options are the whole fence
//! and none of them is decoration:
//!
//! - `WITH SET FALSE` on the binding-to-database edge stops the shared worker
//!   login assuming a DATABASE role, which would carry every co-tenant
//!   binding's privileges at once.
//! - `WITH INHERIT FALSE` on the worker-to-binding edge stops a binding's
//!   privileges being ambient on that login, so a statement that forgets to
//!   narrow fails closed. The `NOINHERIT` role ATTRIBUTE is not a substitute:
//!   `pg_auth_members` records the option per membership at grant time and
//!   flipping the attribute afterwards does not reach back into a membership
//!   that already exists.
//! - `WITH INHERIT FALSE` on the binding-to-unmask edge is the same property one
//!   rung down: a session narrowed to the binding holds no privilege on the
//!   real-value columns, so an ordinary `SELECT *` is refused and only the
//!   statement that deliberately assumes the unmask role reaches the plaintext.
//!   `SET` is deliberately left at its default there, unlike the capability
//!   edge: `SET ROLE` is authorized against the memberships of the role that
//!   CONNECTED, so the data plane can only assume the unmask role BECAUSE the
//!   worker reaches it transitively through a live binding. A revoked binding
//!   takes that reachability with it.
//!
//! # Per-column grants are not emitted here
//!
//! The capability and unmask roles get `USAGE` on the schema and nothing else.
//! Which COLUMNS each of them may read or write is a fact about the TABLES an
//! apply creates, not about the database, so it belongs to the apply path:
//! [`crate::capability_grants::grant_capability_columns`] converges it over the
//! live catalog on every apply, withholding a masked field's real-value column
//! from the capability roles, granting them the column that holds its mask, and
//! giving the withheld one to the unmask role alone.
//!
//! A reconciler that granted table-wide privileges here would return the
//! plaintext of every classified column: a table-level `GRANT SELECT` beside a
//! column list does not narrow it, it widens it.

use compio_postgres::Client;
use zeroship_core::database_derivation;
use zeroship_core::database_role::{self, DatabaseCapability, RoleNameTooLong};
use zeroship_core::{BindingId, DatabaseId};

use crate::apply::{quote_ident, quote_lit, WORKER_ROLE};
use crate::provisioning::exec_retry;

/// The login the CDC relay opens logical decoding with.
///
/// It is a different process under a different login from the worker's, which
/// is what keeps `REPLICATION` off the login that runs creator code: the
/// worker's boot posture refuses that attribute outright.
pub const RELAY_ROLE: &str = "zeroship_cdc";

/// The platform's own schema on a tenant cluster.
///
/// It holds no table, no function, no `SECURITY DEFINER`, no
/// `EXECUTE ... TO PUBLIC`, and no `GRANT USAGE ON SCHEMA` to any app or
/// binding role - an audit of the routine grants that left `USAGE` in place
/// would be checking the lock and not the door. Its presence is what a
/// bootstrapped cluster is recognised by.
pub const ADMIN_SCHEMA: &str = "__zeroship_admin";

/// A cluster-side step that did not complete.
#[derive(Debug, thiserror::Error)]
pub enum ClusterError {
    /// The cluster refused or could not answer.
    #[error("cluster statement: {0}")]
    Query(#[from] compio_postgres::Error),
    /// A derived role name would not fit a `PostgreSQL` identifier.
    #[error(transparent)]
    RoleName(#[from] RoleNameTooLong),
    /// The shared worker login holds a database role directly.
    ///
    /// One such membership carries every co-tenant binding's privileges on that
    /// database at once, and revoking any single binding leaves it standing.
    /// `PostgreSQL` will not complain about it, so the reconciler has to.
    #[error(
        "the shared worker login {WORKER_ROLE} directly holds the database role(s) {roles}; \
         a direct membership restores access that revoking a binding cannot withdraw"
    )]
    WorkerHoldsDatabaseRoles { roles: String },
}

/// Apply the datastore bootstrap corpus.
///
/// # Why there is no cluster-side journal
///
/// Every step below states a DESIRED STATE rather than a delta: a role is
/// created if absent and its attribute set is asserted, the schema is
/// `IF NOT EXISTS`, and the revokes are idempotent. A journal exists to stop a
/// delta being applied twice; there is no delta here, so a journal would only
/// add a table this design has already said the platform schema on a tenant
/// cluster must not carry. Re-runnability comes from the statements, and the
/// convergence signal is control-side: `datastores.status`.
///
/// Running it on every pass is deliberate. A corpus that only ran while the row
/// said `pending` could never reach a cluster that was bootstrapped before the
/// corpus grew a step.
///
/// # The logins carry no password, and that is the fail-closed direction
///
/// A cluster's authentication material is deployment, not product. The
/// provisioning login this service connects with is already created by hand on
/// a fresh cluster for exactly that reason, and baking a known password into a
/// tenant cluster's worker login would be worse than the manual step it saves.
/// A role with no password cannot authenticate under `scram` or `md5`, so an
/// unconfigured cluster refuses the worker rather than admitting it.
///
/// # Errors
///
/// [`ClusterError::Query`] on any DDL failure.
pub async fn apply_bootstrap_corpus(admin: &Client) -> Result<(), ClusterError> {
    let worker_q = quote_ident(WORKER_ROLE);
    let worker_lit = quote_lit(WORKER_ROLE);
    let relay_q = quote_ident(RELAY_ROLE);
    let relay_lit = quote_lit(RELAY_ROLE);
    let admin_schema_q = quote_ident(ADMIN_SCHEMA);

    // The worker login. NOSUPERUSER / NOCREATEDB / NOCREATEROLE keep schema
    // change out of the process that runs creator code; NOREPLICATION keeps
    // logical decoding in the relay; NOBYPASSRLS keeps it inside whatever
    // policies a creator declares. `ALTER ROLE` after the guarded create is
    // what converges a role an operator made by hand with a wider attribute
    // set, and it deliberately names no PASSWORD.
    exec_retry(
        admin,
        &format!(
            "DO $bootstrap_worker$ BEGIN
                IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{worker_lit}') THEN
                    EXECUTE 'CREATE ROLE {worker_q} LOGIN';
                END IF;
             END $bootstrap_worker$"
        ),
    )
    .await?;
    exec_retry(
        admin,
        &format!(
            "ALTER ROLE {worker_q} WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE \
             NOREPLICATION NOBYPASSRLS INHERIT"
        ),
    )
    .await?;

    // The relay login. REPLICATION is the one attribute it has and the worker
    // must not: logical decoding consults no column ACL, so the process that
    // holds it is the one that must not execute creator code.
    exec_retry(
        admin,
        &format!(
            "DO $bootstrap_relay$ BEGIN
                IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{relay_lit}') THEN
                    EXECUTE 'CREATE ROLE {relay_q} LOGIN';
                END IF;
             END $bootstrap_relay$"
        ),
    )
    .await?;
    exec_retry(
        admin,
        &format!(
            "ALTER ROLE {relay_q} WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE \
             NOINHERIT REPLICATION NOBYPASSRLS"
        ),
    )
    .await?;

    // The platform's own schema, empty and revoked from PUBLIC.
    exec_retry(
        admin,
        &format!(
            "CREATE SCHEMA IF NOT EXISTS {admin_schema_q};
             REVOKE ALL ON SCHEMA {admin_schema_q} FROM PUBLIC;"
        ),
    )
    .await?;

    // `citext` is what the platform's own case-insensitive text columns lower
    // to, so a creator declaring one on a tenant cluster needs it resolvable.
    // It is contrib, shipped with every standard distribution, and a cluster
    // without it fails here loudly rather than at a creator's first apply.
    exec_retry(admin, "CREATE EXTENSION IF NOT EXISTS citext WITH SCHEMA public").await?;

    Ok(())
}

/// The names one database's four roles carry on the cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseRoles {
    pub schema: String,
    pub migrator: String,
    pub readwrite: String,
    pub readonly: String,
    /// The one role an apply grants `SELECT` on a masked field's real-value
    /// column to. Neither capability role holds it.
    pub unmask: String,
}

impl DatabaseRoles {
    /// # Errors
    /// [`RoleNameTooLong`] rather than a truncated name, because a truncated
    /// role name resolves to a DIFFERENT database's role.
    pub fn derive(database: &DatabaseId) -> Result<Self, RoleNameTooLong> {
        Ok(Self {
            schema: database_derivation::schema_name(database),
            migrator: database_derivation::migrator_role_name(database)?,
            readwrite: database_derivation::capability_role_name(
                database,
                DatabaseCapability::ReadWrite,
            )?,
            readonly: database_derivation::capability_role_name(
                database,
                DatabaseCapability::ReadOnly,
            )?,
            unmask: database_derivation::unmask_role_name(database)?,
        })
    }

    #[must_use]
    pub fn for_capability(&self, capability: DatabaseCapability) -> &str {
        match capability {
            DatabaseCapability::ReadWrite => &self.readwrite,
            DatabaseCapability::ReadOnly => &self.readonly,
        }
    }
}

/// Create one database's schema, its four roles and their schema grants.
///
/// Every statement converges to a desired state, so a second call changes
/// nothing and a crash part way through is recovered by calling again.
///
/// # Errors
///
/// [`ClusterError::RoleName`] on a name `PostgreSQL` would truncate,
/// [`ClusterError::Query`] on any DDL failure.
pub async fn converge_database(
    admin: &mut Client,
    database: &DatabaseId,
) -> Result<(), ClusterError> {
    let roles = DatabaseRoles::derive(database)?;
    let schema_q = quote_ident(&roles.schema);
    let migrator_q = quote_ident(&roles.migrator);
    let readwrite_q = quote_ident(&roles.readwrite);
    let readonly_q = quote_ident(&roles.readonly);
    let unmask_q = quote_ident(&roles.unmask);
    let migrator_lit = quote_lit(&roles.migrator);
    let readwrite_lit = quote_lit(&roles.readwrite);
    let readonly_lit = quote_lit(&roles.readonly);
    let unmask_lit = quote_lit(&roles.unmask);

    // ONE transaction: the schema and the role that owns it are one fact. A
    // cluster carrying the schema without its owner has a schema no apply can
    // write, and one carrying the roles without the schema has four roles
    // granted on nothing.
    let transaction = admin.transaction().await?;
    transaction
        .batch_execute(&format!(
            "DO $database_roles$ BEGIN
                IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{migrator_lit}') THEN
                    EXECUTE 'CREATE ROLE {migrator_q} NOLOGIN NOSUPERUSER NOCREATEDB \
                             NOCREATEROLE NOREPLICATION NOBYPASSRLS INHERIT';
                END IF;
                IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{readwrite_lit}') THEN
                    EXECUTE 'CREATE ROLE {readwrite_q} NOLOGIN NOSUPERUSER NOCREATEDB \
                             NOCREATEROLE NOREPLICATION NOBYPASSRLS INHERIT';
                END IF;
                IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{readonly_lit}') THEN
                    EXECUTE 'CREATE ROLE {readonly_q} NOLOGIN NOSUPERUSER NOCREATEDB \
                             NOCREATEROLE NOREPLICATION NOBYPASSRLS INHERIT';
                END IF;
                IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{unmask_lit}') THEN
                    EXECUTE 'CREATE ROLE {unmask_q} NOLOGIN NOSUPERUSER NOCREATEDB \
                             NOCREATEROLE NOREPLICATION NOBYPASSRLS INHERIT';
                END IF;
                -- Handing the schema to its owner needs membership in the owner.
                IF NOT pg_has_role(current_user, '{migrator_lit}', 'MEMBER') THEN
                    EXECUTE 'GRANT {migrator_q} TO ' || quote_ident(current_user);
                END IF;
             END $database_roles$;
             CREATE SCHEMA IF NOT EXISTS {schema_q};
             ALTER SCHEMA {schema_q} OWNER TO {migrator_q};
             GRANT CREATE, USAGE ON SCHEMA {schema_q} TO {migrator_q};
             GRANT USAGE ON SCHEMA {schema_q} TO {readwrite_q};
             GRANT USAGE ON SCHEMA {schema_q} TO {readonly_q};
             GRANT USAGE ON SCHEMA {schema_q} TO {unmask_q};
             ALTER ROLE {migrator_q} SET search_path = {schema_q}, public;"
        ))
        .await?;
    transaction.commit().await?;
    Ok(())
}

/// The role one binding's three edges hang off.
///
/// Composed once so the grant, the revoke and the reap cannot disagree about
/// which role name a binding means.
///
/// # Errors
/// [`RoleNameTooLong`] on a name `PostgreSQL` would truncate. The binding id is
/// the LAST component, so a truncation drops the bytes that tell two bindings
/// apart and revoking either would withdraw the other's access.
pub fn binding_role(binding: &BindingId) -> Result<String, RoleNameTooLong> {
    database_derivation::binding_role_name(binding)
}

/// Grant one binding's three edges.
///
/// Exactly the statements
/// `crates/zeroship-data-orm/tests/postgres_tenant_fence.rs` measures, in that
/// order, with the binding role minted first because a `GRANT` naming a role
/// that does not exist is an error rather than a no-op.
///
/// A re-grant over an existing membership is how this converges rather than
/// merely succeeding the first time: `GRANT ... WITH INHERIT FALSE` over an
/// inheriting membership flips `inherit_option` in place, and a `REVOKE` first
/// would open a window in which the worker holds no membership at all and a
/// concurrent narrow fails spuriously.
///
/// # Errors
/// [`ClusterError::RoleName`] on an untruncatable name,
/// [`ClusterError::Query`] on any DDL failure.
pub async fn grant_binding(
    admin: &Client,
    binding: &BindingId,
    database: &DatabaseId,
    capability: DatabaseCapability,
) -> Result<String, ClusterError> {
    let roles = DatabaseRoles::derive(database)?;
    let binding_name = binding_role(binding)?;
    for statement in grant_binding_statements(
        &binding_name,
        roles.for_capability(capability),
        &roles.unmask,
    ) {
        exec_retry(admin, &statement).await?;
    }
    Ok(binding_name)
}

/// The four statements one binding's three edges are, in the order the fence
/// requires.
///
/// Composed here rather than at each driver so the grant and the re-grant that
/// converges an existing membership issue the same SQL. A second spelling would
/// be a second fence, and the grant options are the whole of it.
///
/// The unmask edge carries `INHERIT FALSE` and not `SET FALSE`, which is the
/// opposite pairing from the capability edge beside it, and the difference is
/// the whole point of both:
///
/// - the capability role must never be ASSUMABLE, because assuming it would
///   carry every co-tenant binding's grants on that database at once, so it is
///   inherited and not settable;
/// - the unmask role must never be AMBIENT, because a session that merely
///   narrowed to the binding would then get the plaintext out of an ordinary
///   `SELECT *` with no audit row behind it, so it is settable and not
///   inherited.
///
/// It is granted to the BINDING rather than to the worker login because that is
/// what makes it revocable: `SET ROLE` authorizes against the memberships of
/// the role that connected, and the worker reaches the unmask role only by
/// walking through a live binding, so withdrawing the worker-to-binding edge
/// withdraws this reachability with it.
fn grant_binding_statements(
    binding_name: &str,
    capability_role: &str,
    unmask_role: &str,
) -> [String; 4] {
    let binding_q = quote_ident(binding_name);
    let binding_lit = quote_lit(binding_name);
    let capability_q = quote_ident(capability_role);
    let unmask_q = quote_ident(unmask_role);
    let worker_q = quote_ident(WORKER_ROLE);
    [
        format!(
            "DO $binding_role$ BEGIN
                IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{binding_lit}') THEN
                    EXECUTE 'CREATE ROLE {binding_q} NOLOGIN NOSUPERUSER NOCREATEDB \
                             NOCREATEROLE NOREPLICATION NOBYPASSRLS INHERIT';
                END IF;
             END $binding_role$"
        ),
        format!("GRANT {capability_q} TO {binding_q} WITH SET FALSE"),
        format!("GRANT {unmask_q} TO {binding_q} WITH INHERIT FALSE"),
        format!("GRANT {binding_q} TO {worker_q} WITH INHERIT FALSE"),
    ]
}

/// Withdraw one binding's three edges, and leave its role standing.
///
/// NEVER `DROP ROLE` here. A revoked binding is terminal, and the data plane
/// says so by the SQLSTATE the server returns: `42501` when the role exists and
/// this session may not assume it. Dropping the role would answer `22023`
/// instead, which is the generic "no such role" every unconverged database also
/// produces, and a terminal refusal would be reported as an ordinary failure.
///
/// The worker edge goes first, so the moment anything is withdrawn the binding
/// has already stopped being assumable.
///
/// # Errors
/// [`ClusterError::RoleName`] on an untruncatable name,
/// [`ClusterError::Query`] on any DDL failure.
pub async fn revoke_binding(
    admin: &Client,
    binding: &BindingId,
    database: &DatabaseId,
    capability: DatabaseCapability,
) -> Result<String, ClusterError> {
    let roles = DatabaseRoles::derive(database)?;
    let binding_name = binding_role(binding)?;
    let binding_q = quote_ident(&binding_name);
    let binding_lit = quote_lit(&binding_name);
    let capability_q = quote_ident(roles.for_capability(capability));
    let capability_lit = quote_lit(roles.for_capability(capability));
    let unmask_q = quote_ident(&roles.unmask);
    let unmask_lit = quote_lit(&roles.unmask);
    let worker_q = quote_ident(WORKER_ROLE);
    let worker_lit = quote_lit(WORKER_ROLE);

    exec_retry(
        admin,
        &format!(
            "DO $revoke_binding$ BEGIN
                IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{binding_lit}') THEN
                    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{worker_lit}') THEN
                        EXECUTE 'REVOKE {binding_q} FROM {worker_q}';
                    END IF;
                    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{capability_lit}') THEN
                        EXECUTE 'REVOKE {capability_q} FROM {binding_q}';
                    END IF;
                    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{unmask_lit}') THEN
                        EXECUTE 'REVOKE {unmask_q} FROM {binding_q}';
                    END IF;
                END IF;
             END $revoke_binding$"
        ),
    )
    .await?;
    Ok(binding_name)
}

/// The database roles the shared worker login holds DIRECTLY.
///
/// Empty is the only acceptable answer. A direct membership hands one login
/// every co-tenant binding's privileges on that database at once and survives
/// the revocation of every binding, and `PostgreSQL` reports nothing unusual
/// about it, so this is read explicitly rather than inferred from the grants
/// the reconciler itself issued.
///
/// The predicate is deliberately WIDER than the composer: any role whose name
/// begins `zs_db_` counts. Over-reporting here costs a refused pass; under-
/// reporting costs an unrevocable cross-tenant grant.
///
/// # Errors
/// [`ClusterError::Query`] on any read failure.
pub async fn worker_direct_database_memberships(
    admin: &Client,
) -> Result<Vec<String>, ClusterError> {
    let rows = admin
        .query(
            "SELECT database_role.rolname AS role_name \
               FROM pg_auth_members membership \
               JOIN pg_roles database_role ON database_role.oid = membership.roleid \
               JOIN pg_roles member ON member.oid = membership.member \
              WHERE member.rolname = $1::text \
                AND left(database_role.rolname, 6) = 'zs_db_' \
              ORDER BY database_role.rolname",
            &[&WORKER_ROLE],
        )
        .await?;
    Ok(rows.iter().map(|row| row.get("role_name")).collect())
}

/// Refuse a cluster whose worker login already holds a database role directly.
///
/// # Errors
/// [`ClusterError::WorkerHoldsDatabaseRoles`] when any such membership exists,
/// [`ClusterError::Query`] on any read failure.
pub async fn require_no_direct_database_memberships(admin: &Client) -> Result<(), ClusterError> {
    let held = worker_direct_database_memberships(admin).await?;
    if held.is_empty() {
        return Ok(());
    }
    Err(ClusterError::WorkerHoldsDatabaseRoles {
        roles: held.join(", "),
    })
}

/// How the reap classified one `zs_`-prefixed role in the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClusterRole {
    /// A binding role the composer would produce for this binding. The name
    /// round-trips through
    /// [`zeroship_core::database_role::binding_role_name`] byte for byte, so
    /// nothing was inferred from the shape of the text.
    Binding { binding: BindingId },
    /// One of a database's four roles. Never a reap candidate: the migrator
    /// OWNS the schema, so dropping it is teardown and teardown has its own
    /// precondition.
    Database { database: DatabaseId },
    /// A `zs_`-prefixed role this cannot attribute to any declaration. Reported
    /// and never dropped.
    Unattributed,
}

/// Every `zs_`-prefixed role on the cluster, classified by DERIVATION.
///
/// The catalog predicate only narrows the scan. What a name MEANS is decided by
/// recomposing it from the parsed parts and demanding the composer produce the
/// same bytes, so a role that merely looks like a binding - a different
/// separator, a trailing component, an id that is not canonical - lands in
/// [`ClusterRole::Unattributed`] rather than being swept.
///
/// # Errors
/// [`ClusterError::Query`] on any read failure.
pub async fn classify_platform_roles(
    admin: &Client,
) -> Result<Vec<(String, ClusterRole)>, ClusterError> {
    let rows = admin
        .query(
            "SELECT rolname FROM pg_roles WHERE left(rolname, 3) = 'zs_' ORDER BY rolname",
            &[],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| {
            let name: String = row.get("rolname");
            let classified = classify_role_name(&name);
            (name, classified)
        })
        .collect())
}

/// Classify one catalog role name by recomposing it.
#[must_use]
pub fn classify_role_name(name: &str) -> ClusterRole {
    if let Some(rest) = name.strip_prefix("zs_bind_") {
        if let Ok(binding) = BindingId::parse(rest) {
            if database_role::binding_role_name(rest).as_deref() == Ok(name) {
                return ClusterRole::Binding { binding };
            }
        }
        return ClusterRole::Unattributed;
    }
    if let Some(rest) = name.strip_prefix("zs_db_") {
        if let Some((database_text, suffix)) = rest.rsplit_once('_') {
            if let Ok(database) = DatabaseId::parse(database_text) {
                let composed = match suffix {
                    "mig" => database_role::database_migrator_role_name(database_text),
                    "rw" => database_role::database_capability_role_name(
                        database_text,
                        DatabaseCapability::ReadWrite,
                    ),
                    "ro" => database_role::database_capability_role_name(
                        database_text,
                        DatabaseCapability::ReadOnly,
                    ),
                    "unmask" => database_role::database_unmask_role_name(database_text),
                    _ => return ClusterRole::Unattributed,
                };
                if composed.as_deref() == Ok(name) {
                    return ClusterRole::Database { database };
                }
            }
        }
        return ClusterRole::Unattributed;
    }
    ClusterRole::Unattributed
}

/// Tear one database down: its schema, then its four roles.
///
/// ONLY reachable from an explicit `deleting` declaration. A schema is never
/// dropped because nothing names it: a reaped role is recoverable - the next
/// pass re-grants it - and a dropped schema is not, so absence may authorize
/// the first and never the second.
///
/// The order is the design's and each step re-keys. The schema goes first
/// because the migrator OWNS it and a role cannot be dropped while it owns
/// objects; the roles go after for the same reason. No shared CDC object is
/// touched: the relay-owned slot, stream and publication live for the whole
/// datastore.
///
/// # Errors
/// [`ClusterError::RoleName`] on an untruncatable name,
/// [`ClusterError::Query`] on any DDL failure.
pub async fn drop_database(
    admin: &mut Client,
    database: &DatabaseId,
) -> Result<(), ClusterError> {
    let roles = DatabaseRoles::derive(database)?;
    let schema_q = quote_ident(&roles.schema);
    let migrator_lit = quote_lit(&roles.migrator);
    let readwrite_lit = quote_lit(&roles.readwrite);
    let readonly_lit = quote_lit(&roles.readonly);
    let unmask_lit = quote_lit(&roles.unmask);

    let transaction = admin.transaction().await?;
    transaction
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {schema_q} CASCADE;
             DO $drop_database_roles$
             DECLARE
                 role_name text;
             BEGIN
                 FOREACH role_name IN ARRAY ARRAY['{migrator_lit}', '{readwrite_lit}', '{readonly_lit}', '{unmask_lit}']
                 LOOP
                     IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = role_name) THEN
                         -- Clears the privileges and remaining ownerships that
                         -- would otherwise make DROP ROLE report a dependency.
                         EXECUTE format('DROP OWNED BY %I', role_name);
                         EXECUTE format('DROP ROLE %I', role_name);
                     END IF;
                 END LOOP;
             END $drop_database_roles$;"
        ))
        .await?;
    transaction.commit().await?;
    Ok(())
}

/// The `db_<dbs>` schemas on this cluster, by the database each one derives from.
///
/// Reported, never swept. A schema with no declaring row is a cleanup task an
/// operator can see; a schema this dropped on an absence is an outage.
///
/// # Errors
/// [`ClusterError::Query`] on any read failure.
pub async fn database_schemas(admin: &Client) -> Result<Vec<(String, DatabaseId)>, ClusterError> {
    let rows = admin
        .query(
            "SELECT nspname FROM pg_catalog.pg_namespace \
              WHERE left(nspname, 3) = 'db_' ORDER BY nspname",
            &[],
        )
        .await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let name: String = row.get("nspname");
            let database = DatabaseId::parse(name.strip_prefix("db_")?).ok()?;
            (database_derivation::schema_name(&database) == name).then_some((name, database))
        })
        .collect())
}

/// Drop one binding role.
///
/// Guarded on existence so a concurrent drop is not an error, and the worker
/// membership is withdrawn first so the role has no dependent membership when
/// it goes.
///
/// # Errors
/// [`ClusterError::Query`] on any DDL failure.
pub async fn drop_binding_role(admin: &Client, role: &str) -> Result<(), ClusterError> {
    exec_retry(admin, &drop_binding_role_sql(role)).await?;
    Ok(())
}

/// The statement one binding role's removal is.
///
/// Composed here rather than at the caller for the reason
/// [`grant_binding_statements`] is: the worker membership has to be withdrawn
/// before the role goes, or `DROP ROLE` reports a dependency.
fn drop_binding_role_sql(role: &str) -> String {
    let role_q = quote_ident(role);
    let role_lit = quote_lit(role);
    let worker_q = quote_ident(WORKER_ROLE);
    let worker_lit = quote_lit(WORKER_ROLE);
    format!(
        "DO $reap_binding$ BEGIN
            IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{role_lit}') THEN
                IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{worker_lit}') THEN
                    EXECUTE 'REVOKE {role_q} FROM {worker_q}';
                END IF;
                EXECUTE 'DROP ROLE {role_q}';
            END IF;
         END $reap_binding$"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATABASE: &str = "dbs_03cgepu94hyemwpcipafo7264";
    const BINDING: &str = "bnd_03coc2qj4x2ae61h80zwlnnq6";

    fn database() -> DatabaseId {
        DatabaseId::parse(DATABASE).expect("canonical database id")
    }

    fn binding() -> BindingId {
        BindingId::parse(BINDING).expect("canonical binding id")
    }

    /// A name the composer produces is classified as what produced it.
    ///
    /// The control for every refusal below: without it, a classifier that
    /// returned `Unattributed` for everything would pass all of them while
    /// making the reap a no-op.
    #[test]
    fn every_composed_name_classifies_back_to_its_own_declaration() {
        let roles = DatabaseRoles::derive(&database()).expect("the fixture names fit");
        let mut checked = 0;
        for name in [
            &roles.migrator,
            &roles.readwrite,
            &roles.readonly,
            &roles.unmask,
        ] {
            assert_eq!(
                classify_role_name(name),
                ClusterRole::Database {
                    database: database()
                },
                "`{name}` is one of this database's four roles"
            );
            checked += 1;
        }
        for binding_id in [BINDING, "bnd_03cgepu94hyemwpcipafo7264"] {
            let name = database_role::binding_role_name(binding_id).expect("composes");
            assert_eq!(
                classify_role_name(&name),
                ClusterRole::Binding {
                    binding: BindingId::parse(binding_id).expect("canonical binding id"),
                },
                "`{name}` is binding {binding_id}"
            );
            checked += 1;
        }
        assert_eq!(checked, 6, "the arm must not pass over an empty set");
    }

    /// Anything the composer would NOT have produced is never a reap candidate.
    ///
    /// Each entry differs from a composable name in exactly one way, and the
    /// platform logins are in the list because they live on the same cluster:
    /// a sweep that attributed one of them would drop the login every app
    /// connects through.
    #[test]
    fn a_name_the_composer_would_not_produce_is_never_attributed() {
        for name in [
            // The platform's own logins and the relay's.
            WORKER_ROLE,
            RELAY_ROLE,
            "postgres",
            // A binding shape carrying a trailing component: PostgreSQL would
            // accept the role, and it is a name the composer never produces.
            "zs_bind_bnd_03coc2qj4x2ae61h80zwlnnq6_e1",
            // A binding id that is not canonical.
            "zs_bind_bnd_notanid",
            // An app-keyed runtime role from the pre-decoupling derivation.
            "app_app_02xfboclmnln2ar6iblni0000_role",
            // A database shape with an unknown capability suffix.
            "zs_db_dbs_03cgepu94hyemwpcipafo7264_owner",
            // One letter off the real-value role's suffix.
            "zs_db_dbs_03cgepu94hyemwpcipafo7264_unmasked",
            // A database id that is not canonical.
            "zs_db_notanid_rw",
            // The prefix alone.
            "zs_bind_",
            "zs_db_",
            "zs_",
        ] {
            assert_eq!(
                classify_role_name(name),
                ClusterRole::Unattributed,
                "`{name}` must never be attributed to a declaration"
            );
        }
    }

    /// The binding id travels into the role name the grant will send.
    #[test]
    fn the_binding_role_name_carries_the_binding_it_was_given() {
        assert_eq!(
            binding_role(&binding()).expect("composes"),
            format!("zs_bind_{BINDING}")
        );
        let other = BindingId::mint();
        assert_ne!(other, binding(), "the control: two mints are two bindings");
        assert_ne!(
            binding_role(&binding()).expect("composes"),
            binding_role(&other).expect("composes"),
            "two bindings must be two roles"
        );
    }

    /// One database's four roles are four names, and neither the migrator nor
    /// the unmask role is reachable through a capability.
    #[test]
    fn a_capability_never_composes_the_owner_role() {
        let roles = DatabaseRoles::derive(&database()).expect("the fixture names fit");
        let mut names = vec![
            roles.migrator.clone(),
            roles.readwrite.clone(),
            roles.readonly.clone(),
            roles.unmask.clone(),
        ];
        let composed = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), composed, "four distinct names: {names:?}");
        for capability in [DatabaseCapability::ReadWrite, DatabaseCapability::ReadOnly] {
            assert_ne!(
                roles.for_capability(capability),
                roles.unmask,
                "{capability:?} must not resolve to the role holding the real value"
            );
        }
        assert_eq!(
            roles.for_capability(DatabaseCapability::ReadWrite),
            roles.readwrite
        );
        assert_eq!(
            roles.for_capability(DatabaseCapability::ReadOnly),
            roles.readonly
        );
    }

    /// The three edges the grant emits, in the order the fence requires and
    /// with the grant option each one carries.
    ///
    /// The options are the whole fence and they are OPPOSITE on the two
    /// database edges, so an arm that only checked "the statement is present"
    /// would pass for the pairing that breaks it: `SET FALSE` on the unmask
    /// edge makes the audited read impossible, and `INHERIT FALSE` missing
    /// makes the plaintext ambient on every narrowed session.
    #[test]
    fn the_binding_edges_carry_the_grant_option_each_fence_needs() {
        let roles = DatabaseRoles::derive(&database()).expect("the fixture names fit");
        let binding_name = binding_role(&binding()).expect("composes");
        let statements = grant_binding_statements(
            &binding_name,
            roles.for_capability(DatabaseCapability::ReadWrite),
            &roles.unmask,
        );

        assert!(
            statements[0].contains("CREATE ROLE"),
            "the binding role is minted first, or the grants name nothing: {:?}",
            statements[0]
        );
        assert_eq!(
            statements[1],
            format!(
                "GRANT \"{}\" TO \"{binding_name}\" WITH SET FALSE",
                roles.readwrite
            ),
            "the capability edge is inherited and never assumable"
        );
        assert_eq!(
            statements[2],
            format!(
                "GRANT \"{}\" TO \"{binding_name}\" WITH INHERIT FALSE",
                roles.unmask
            ),
            "the unmask edge is assumable and never ambient"
        );
        assert_eq!(
            statements[3],
            format!("GRANT \"{binding_name}\" TO \"{WORKER_ROLE}\" WITH INHERIT FALSE"),
            "the worker edge is assumable and never ambient"
        );

        // The control: the readonly capability produces the same shape with the
        // other capability role, so the arms above are about the OPTION rather
        // than about a statement that happens to mention `rw`.
        let readonly_statements = grant_binding_statements(
            &binding_name,
            roles.for_capability(DatabaseCapability::ReadOnly),
            &roles.unmask,
        );
        assert_eq!(
            readonly_statements[1],
            format!(
                "GRANT \"{}\" TO \"{binding_name}\" WITH SET FALSE",
                roles.readonly
            )
        );
        assert_eq!(
            readonly_statements[2], statements[2],
            "the unmask edge does not vary with the capability: one database, \
             one real-value role"
        );
    }
}
