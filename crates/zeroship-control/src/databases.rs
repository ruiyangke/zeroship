//! Databases and their bindings - the project-owned data resource.
//!
//! A PROJECT owns databases, an APP reaches one through a binding, and a
//! binding may only join an app and a database in the same project. That last
//! predicate is the whole design of
//! `docs/proposals/2026-08-28-app-database-decoupling.md`: it is carried by two
//! composite foreign keys over a shared `project_id`
//! (`db/migrations-ts/20260919000300_database_placement_keys.ts`), so it holds
//! on every write to either side rather than being checked once at issuance.
//!
//! # Authority is the PROJECT seat, and it is compared where the effect happens
//!
//! A database carries no rank of its own. `zeroship_authz::authority::resolve`
//! reaches `zeroship.databases.project_id` directly - never through a binding,
//! because an app's binding is data access for running code and must not read
//! back as authority over the database for the app's own members. Every
//! mutation here therefore evaluates the SAME narrowing
//! [`crate::organizations::effective_project_rank_sql`] applies, **inside the
//! statement that performs the effect**, against the caller's live seat. Zero
//! rows affected IS the refusal. Cedar runs first in the handler and is a real
//! fence, but it is never the only one: a member removed between the Cedar call
//! and the write finds a statement that matches nothing.
//!
//! The threshold is `developer`, which is where `deploy/policies/creator/
//! organization_develop.cedar` puts `database:write`. The read surface is
//! `viewer`, from `deploy/policies/creator/organization_read.cedar`.
//!
//! # NOTHING HERE REACHES `status = 'active'`
//!
//! Control DECLARES; a per-cluster reconciler converges
//! (`zeroship_migrate_server::datastore`), inside the service that holds the
//! cluster's privileged credential. A row created here therefore stops at
//! `provisioning`, and a binding at `pending`, until that loop has made the
//! cluster match. That is deliberate: a transition written here would be a
//! claim about a cluster this process cannot reach and holds no credential for.
//!
//! # Control never inserts a datastore
//!
//! A cluster registers itself through the service that holds its credential,
//! which is the only proof it exists and is reachable. [`place`] READS
//! `zeroship.datastores`; nothing in this module writes it, and the migration
//! that creates the table grants Control `select, insert, update` there for the
//! registration surface that owns it rather than for this one.
//!
//! # A database is addressed by its id, always
//!
//! `databases.name` is display text for dashboards and the CLI's local
//! dereference. There is no `(project, name) -> database_id` resolution on any
//! wire in this module, and adding one would make a renameable string an
//! identifier.

use chrono::{DateTime, Utc};
use compio_postgres::error::SqlState;
use compio_postgres::GenericClient;
use ntex::web::{
    self,
    types::{Json, Path, State},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use zeroship_authz::{Action as AuthzAction, Resource};
use zeroship_core::database_role::DatabaseCapability;
use zeroship_core::project_id::ProjectId;
use zeroship_core::{AppId, BindingId, DatabaseId, UserId};

use crate::audit::{self, Action as AuditAction, AuditEntry};
use crate::authz_guard::AuthzGuard;
use crate::env_handlers::admin_rate_limit;
use crate::http_util;
use crate::organizations::{
    effective_project_rank_sql, ladder_rank_of, lock_project_organization, OrganizationError,
    ROLE_ADMIN, ROLE_DEVELOPER,
};
use crate::registry::{Registry, RegistryError};
use crate::AppState;

/// A database or binding body is two short strings. Far more than that and far
/// less than anything worth streaming.
pub const DATABASE_PAYLOAD_BYTES: usize = 4 * 1024;

/// The two capabilities `database_bindings_capability_check` admits.
///
/// Named here because a caller's value is refused before it reaches a
/// statement, so the refusal names the two legal values rather than surfacing a
/// constraint name. The pair is pinned to the schema by
/// `binding_capabilities_are_the_two_the_check_admits`.
///
/// The TEXT comes from [`DatabaseCapability::as_wire`], which is also what the
/// cluster reconciler parses the stored value back through when it composes a
/// binding's role name. A second spelling here would let this surface write a
/// capability the reconciler cannot read.
pub const CAPABILITY_READWRITE: &str = DatabaseCapability::ReadWrite.as_wire();
pub const CAPABILITY_READONLY: &str = DatabaseCapability::ReadOnly.as_wire();

/// The status a freshly created database stops at.
///
/// It advances only when a cluster reconciler has made the cluster match -
/// created the schema, minted the three database roles and applied the column
/// grants. Nothing in this process can do that, so nothing in this process
/// writes any other value.
const STATUS_PROVISIONING: &str = "provisioning";

/// The status a database reaches once the reconciler has made its cluster
/// match: the schema exists, the three roles exist, the grants are applied.
const STATUS_ACTIVE: &str = "active";

/// The statuses a database may take a NEW BINDING in, as an allowlist.
///
/// Stated as what is admitted rather than what is refused, so a value added to
/// `databases_status_check` later is non-bindable until someone decides it
/// should be. The refusing spelling has the opposite default: it named
/// `deleting` and let `draining` through, which is the gap dbd-reconciler
/// measured against a real cluster.
const BINDABLE_STATUSES: [&str; 2] = [STATUS_PROVISIONING, STATUS_ACTIVE];

/// The status a database stops at when a caller deletes it.
///
/// The ROW SURVIVES the delete, because the schema and its data survive it. No
/// process here can drop them, and the cluster reconciler that can must be told
/// to rather than left to infer it: a reconciler cannot tell a failed read of
/// the declarations from an empty one, so a rule of "drop whatever no row
/// names" turns a network error into data loss. Roles are reaped by absence
/// because the next pass re-grants them; a schema is dropped only against this
/// value. The reconciler removes the row once the drop has committed, which is
/// also what frees the name under `databases_project_name_key`.
const STATUS_DELETING: &str = "deleting";

/// The status a freshly declared binding stops at, for the same reason.
const BINDING_STATUS_PENDING: &str = "pending";

/// The datastore status [`place`] admits.
///
/// `pending` is not yet bootstrapped and `draining` is leaving rotation, so a
/// cluster that is unreachable or half-bootstrapped is never chosen. The
/// operator's lever is this column.
const DATASTORE_STATUS_ACTIVE: &str = "active";

/// A display name is a label a human reads, not an identifier.
const MAX_DATABASE_NAME_LEN: usize = 64;

// ---------------------------------------------------------------------------
// Bodies and records
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateDatabaseBody {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct BindDatabaseBody {
    pub app_id: String,
    pub capability: String,
}

#[derive(Debug, Serialize)]
pub struct DatabaseRecord {
    pub id: String,
    pub project_id: String,
    pub execution_zone_id: String,
    pub datastore_id: String,
    pub name: String,
    pub schema_epoch: i32,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One edge, with the app it names rendered for a human.
///
/// `app_name` rides along because a creator reading "which apps reach this
/// database" is asking about apps they named, and an answer in `app_` ids alone
/// makes them look every one of them up. The id is still the identity: nothing
/// resolves a binding by its app's name.
#[derive(Debug, Serialize)]
pub struct BindingRecord {
    pub id: String,
    pub app_id: String,
    pub app_name: String,
    pub database_id: String,
    pub project_id: String,
    pub capability: String,
    pub status: String,
    pub generation: i32,
    pub observed_generation: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One app holding a binding that refuses a delete. Carried by
/// [`DatabaseError::DatabaseHasBindings`] so the refusal names what to unbind.
#[derive(Debug, Serialize)]
pub struct BoundApp {
    pub app_id: String,
    pub app_name: String,
    pub capability: String,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum DatabaseError {
    ProjectNotFound,
    /// The database does not exist. ONE variant, and it is also what a caller
    /// whose reach does not include the database's project would see from the
    /// Cedar gate in front of this module - so it is never an oracle over
    /// somebody else's database id.
    DatabaseNotFound,
    /// The app named by a bind does not exist, is deleted, or belongs to
    /// another project. ONE variant for all three, because
    /// `crate::organizations::delete_app` detaches an app from its project, so
    /// after a delete there is no longer anything that could tell "gone" from
    /// "elsewhere" without re-deriving a project the row no longer has.
    ///
    /// Carries the database's project so the caller is told which project both
    /// endpoints have to be in.
    AppNotInProject {
        app_id: String,
        project_id: String,
    },
    Invalid(String),
    /// A well-formed name that is already taken in this project
    /// (`databases_project_name_key`). Typed separately from [`Self::Invalid`]
    /// because the two are different mistakes with different fixes.
    NameTaken(String),
    /// A well-formed name that claims the database-id namespace. Separate from
    /// [`Self::Invalid`] for the reason
    /// [`crate::registry::RegistryError::ReservedName`] gives: a charset
    /// failure says the name is malformed, this says a legal name is
    /// unavailable.
    ReservedName(String),
    /// The effect statement matched no row because the caller's narrowed rank
    /// does not clear the threshold. Carries which authority was wanted.
    Insufficient(String),
    /// PLACEMENT FOUND NO CLUSTER. There is no active datastore in the
    /// project's execution zone, so there is nowhere to put a database.
    ///
    /// A typed refusal at create time rather than a fallback: a zone with no
    /// capacity must fail loudly rather than overload a cluster, and there is
    /// no second zone to fall back to - a project sits in exactly one, and that
    /// is what makes "same project" imply "can share".
    NoDatastoreInZone {
        execution_zone_id: String,
    },
    /// Delete was asked for while apps still bind the database. The refusal
    /// names them, because the remedy is one unbind per app.
    ///
    /// `database_bindings_database_project_fkey` is `ON DELETE RESTRICT`, so
    /// this is not a policy layered over a permissive schema; it is the same
    /// rule stated where the caller can read it, with the remedy named.
    DatabaseHasBindings(Vec<BoundApp>),
    /// The database is not in a status that takes a new binding, and the
    /// refusal carries which status it is in.
    ///
    /// Bind and delete serialize on the same organization lock, so without this
    /// a bind arriving just after a delete would hand an app a binding to a
    /// schema the reconciler is about to drop - and the binding would outlive
    /// the schema it names. Carrying the status rather than naming one case
    /// keeps the refusal honest as `databases_status_check` grows.
    DatabaseNotBindable {
        status: String,
    },
    /// This app already binds this database
    /// (`database_bindings_natural_key`). Carries the capability the live
    /// binding holds, because that is the fact a caller re-binding is usually
    /// trying to change - and changing it is a role rotation, not an UPDATE.
    AlreadyBound {
        app_id: String,
        capability: String,
    },
    BindingNotFound {
        app_id: String,
    },
    /// The organization behind the project was closed.
    OrganizationDissolved(DateTime<Utc>),
    Db,
}

impl DatabaseError {
    #[must_use]
    #[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
    pub fn into_response(self) -> web::HttpResponse {
        match self {
            Self::ProjectNotFound => {
                web::HttpResponse::NotFound().json(&json!({"error": "project not found"}))
            }
            Self::DatabaseNotFound => {
                web::HttpResponse::NotFound().json(&json!({"error": "database not found"}))
            }
            Self::AppNotInProject { app_id, project_id } => {
                web::HttpResponse::Conflict().json(&json!({
                    "error": "app not in project",
                    "detail": "a binding may only join an app and a database in the SAME \
                               project. That app does not exist, has been deleted, or belongs \
                               to another project; move neither - create a database in the \
                               app's own project instead",
                    "app_id": app_id,
                    "project_id": project_id,
                }))
            }
            Self::Invalid(detail) => web::HttpResponse::BadRequest()
                .json(&json!({"error": "invalid request", "detail": detail})),
            Self::NameTaken(name) => web::HttpResponse::Conflict().json(&json!({
                "error": "name taken",
                "detail": format!(
                    "this project already has a database called {name:?}. A database being \
                     deleted still holds its name until its schema has been dropped"
                ),
                "name": name,
            })),
            Self::ReservedName(detail) => web::HttpResponse::Conflict()
                .json(&json!({"error": "reserved name", "detail": detail})),
            Self::Insufficient(detail) => web::HttpResponse::Forbidden()
                .json(&json!({"error": "insufficient authority", "detail": detail})),
            Self::NoDatastoreInZone { execution_zone_id } => {
                // 503, not 409: the request is well formed and the caller has
                // done nothing wrong. What is missing is operator capacity in
                // this execution zone, and the fix is an operator registering
                // or un-draining a cluster there.
                web::HttpResponse::ServiceUnavailable().json(&json!({
                    "error": "no datastore in zone",
                    "detail": "this project's execution zone has no active database cluster, \
                               so there is nowhere to place a database. A project sits in \
                               exactly one zone, so there is no second zone to fall back to; \
                               an operator has to register or un-drain a cluster in this one",
                    "execution_zone_id": execution_zone_id,
                }))
            }
            Self::DatabaseNotBindable { status } => {
                web::HttpResponse::Conflict().json(&json!({
                    "error": "database not bindable",
                    "detail": format!(
                        "this database is {status:?}, so it cannot take a new binding; only a \
                         provisioning or active database can. A database being deleted is \
                         having its schema dropped, so bind a database you create instead"
                    ),
                    "status": status,
                }))
            }
            Self::DatabaseHasBindings(bound) => {
                let apps = bound
                    .iter()
                    .map(|app| app.app_name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                web::HttpResponse::Conflict().json(&json!({
                    "error": "database has bindings",
                    "detail": format!(
                        "a database is deleted only once no app binds it; these still do: \
                         {apps}. Unbind each one with DELETE \
                         /api/databases/{{database_id}}/bindings/{{app_id}} first"
                    ),
                    "bindings": bound,
                }))
            }
            Self::AlreadyBound { app_id, capability } => {
                web::HttpResponse::Conflict().json(&json!({
                    "error": "already bound",
                    "detail": format!(
                        "that app already binds this database with capability {capability:?}. \
                         A binding is an explicit grant and is never silently replaced; \
                         unbind it first"
                    ),
                    "app_id": app_id,
                    "capability": capability,
                }))
            }
            Self::BindingNotFound { app_id } => web::HttpResponse::NotFound().json(&json!({
                "error": "binding not found",
                "detail": "that app holds no binding to this database",
                "app_id": app_id,
            })),
            Self::OrganizationDissolved(at) => web::HttpResponse::Conflict().json(&json!({
                "error": "organization dissolved",
                "detail": "the organization behind this project was closed and accepts no \
                           further changes",
                "dissolved_at": at,
            })),
            Self::Db => {
                web::HttpResponse::InternalServerError().json(&json!({"error": "database error"}))
            }
        }
    }
}

impl From<RegistryError> for DatabaseError {
    fn from(err: RegistryError) -> Self {
        tracing::error!(error = %err, "control: database store connection failed");
        Self::Db
    }
}

/// The two conditions [`lock_project_organization`] can report, in this
/// module's vocabulary.
///
/// It names them rather than collapsing the whole enum, so a third condition
/// appearing there arrives here as a logged 500 instead of being silently
/// reported as one of these two.
impl From<OrganizationError> for DatabaseError {
    fn from(err: OrganizationError) -> Self {
        match err {
            OrganizationError::ProjectNotFound => Self::ProjectNotFound,
            OrganizationError::Dissolved(at) => Self::OrganizationDissolved(at),
            other => {
                tracing::error!(
                    error = ?other,
                    "control: unexpected organization refusal on a database operation"
                );
                Self::Db
            }
        }
    }
}

/// The fallback mapper: log the statement that failed and report a database
/// error.
///
/// It deliberately does NOT classify. Which constraint fired is knowable only
/// at the call site, and each caller that can produce one maps it there
/// ([`create_database`] for the name key, [`bind_database`] for the natural key
/// and the two composite edges). [`delete_database`] maps none: it sets
/// `status = 'deleting'` rather than removing the row, so the RESTRICT on
/// `database_bindings_database_project_fkey` cannot fire there. That edge is
/// restricted when the cluster reconciler removes the row, in
/// `zeroship_migrate_server::datastore::control`. A caller reaching this with a
/// constraint violation has a case nobody has thought about, and reporting that
/// as a 500 is the honest answer.
fn db_error(err: &compio_postgres::Error, context: &str) -> DatabaseError {
    tracing::error!(error = %err, context, "control: database statement failed");
    DatabaseError::Db
}

fn is_unique_violation(err: &compio_postgres::Error) -> bool {
    matches!(err.code(), Some(code) if code == &SqlState::UNIQUE_VIOLATION)
}

fn is_foreign_key_violation(err: &compio_postgres::Error) -> bool {
    matches!(err.code(), Some(code) if code == &SqlState::FOREIGN_KEY_VIOLATION)
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// The prefix a database id is spelled with, including its separator.
///
/// A literal rather than a `format!` of [`DatabaseId::PREFIX`] so it can be a
/// `const`, and pinned to that constant by
/// `the_reserved_prefix_is_the_one_the_id_type_uses`. The reservation and the
/// parse must name one namespace, and a second literal nobody compares is how
/// they would come to name two.
const DATABASE_ID_PREFIX: &str = "dbs_";

/// Every rule a database display name must satisfy.
///
/// # The whole `dbs_` prefix is reserved
///
/// The CLI dereferences a local label from `zeroship.jsonc` to a `dbs_` id
/// before any request, so a label and an id meet in one place. A name that is
/// ALSO a legal [`DatabaseId`] would make that dereference ambiguous: the CLI
/// could not tell whether `dbs_0123456789abcdefghijklm` is the id to send or a
/// label to look up. Reserving only the names that parse as an id would leave
/// the boundary standing on base36 length checks - a body one character short
/// would be claimable, and the rule would be one no creator could state.
///
/// # Errors
///
/// [`DatabaseError::Invalid`] for a malformed name, [`DatabaseError::
/// ReservedName`] for a well-formed one in the id namespace.
pub fn validate_database_name(name: &str) -> Result<(), DatabaseError> {
    if name.is_empty()
        || name.len() > MAX_DATABASE_NAME_LEN
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(DatabaseError::Invalid(format!(
            "name must be 1-{MAX_DATABASE_NAME_LEN} alphanumeric/hyphen/underscore characters"
        )));
    }
    if name.len() >= DATABASE_ID_PREFIX.len()
        && name[..DATABASE_ID_PREFIX.len()].eq_ignore_ascii_case(DATABASE_ID_PREFIX)
    {
        return Err(DatabaseError::ReservedName(format!(
            "{name:?} starts with {DATABASE_ID_PREFIX:?}, which is reserved for database ids. \
             A database's name is a label the CLI dereferences locally and a database's id is \
             its identity; a name in the id namespace makes the two indistinguishable wherever \
             one string carries both"
        )));
    }
    Ok(())
}

/// The capability off the wire, refused before it reaches a statement.
fn validate_capability(capability: &str) -> Result<&str, DatabaseError> {
    match capability {
        CAPABILITY_READWRITE => Ok(CAPABILITY_READWRITE),
        CAPABILITY_READONLY => Ok(CAPABILITY_READONLY),
        other => Err(DatabaseError::Invalid(format!(
            "capability must be {CAPABILITY_READWRITE:?} or {CAPABILITY_READONLY:?}, not \
             {other:?}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// SQL fragments that must exist exactly once
// ---------------------------------------------------------------------------

/// The caller's seat on a project, narrowed, as the LEFT JOIN chain plus the
/// rank expression every statement here compares.
///
/// `project` is the SQL expression naming the project row already in scope
/// (`p.id`), and `actor` is the placeholder token carrying the caller's user
/// id. Both are tokens this crate writes, never caller data.
///
/// Written once because four mutations and two reads need it and two spellings
/// would be two answers. It is the same chain
/// [`crate::registry::Registry::create_app`] uses, calling the same
/// [`effective_project_rank_sql`].
fn project_seat_joins(project: &str, actor: &str) -> String {
    format!(
        "LEFT JOIN zeroship.organization_members m \
                ON m.organization_id = p.organization_id AND m.user_id = {actor} \
         LEFT JOIN zeroship.organization_roles organization_role \
                ON organization_role.role = m.role \
         LEFT JOIN zeroship.project_members pm \
                ON pm.project_id = {project} AND pm.user_id = {actor} \
         LEFT JOIN zeroship.organization_roles project_role \
                ON project_role.role = pm.role"
    )
}

/// The narrowed rank of the seat [`project_seat_joins`] brought into scope.
fn project_seat_rank() -> String {
    effective_project_rank_sql(
        "organization_role.rank",
        "project_role.rank",
        &ladder_rank_of(ROLE_ADMIN),
    )
}

// ---------------------------------------------------------------------------
// Placement
// ---------------------------------------------------------------------------

/// Choose the cluster a new database is placed on: the active datastore in
/// `execution_zone_id` carrying the fewest databases, ties broken by id.
///
/// # It runs inside the caller's transaction
///
/// Placement uses facts Control already owns, because Control created every
/// database and knows exactly how many sit on each. Running it in the same
/// transaction that admits the placement is what makes the count it read the
/// count the INSERT is admitted against.
///
/// # It is a SEPARATE STATEMENT, not a subquery in the INSERT
///
/// Folding it into the INSERT's `SELECT` would make an empty placement
/// indistinguishable from an insufficient seat: both are zero rows, and the
/// caller would be told to fix their permissions when the real answer is that
/// an operator has no capacity in their zone. The two refusals have different
/// audiences, so they need different statements.
///
/// # The tie-break is deterministic
///
/// `ORDER BY count, id` rather than `ORDER BY count` alone: `datastores.id` is
/// a total order, so two concurrent creates in one zone agree on the choice
/// instead of scattering across equally-loaded clusters by plan accident. The
/// count is a balance heuristic, not a constraint - what makes the placement
/// CORRECT is `databases_placement_fkey`, which refuses at INSERT time any
/// pairing whose cluster is not in the database's zone.
///
/// # `active` only
///
/// `pending` is a cluster that has not finished bootstrapping and `draining` is
/// one leaving rotation, so neither is ever chosen. That column is the
/// operator's lever, and it is the only one: there are no capacity columns,
/// because a hand-maintained number on an entity row goes stale silently.
///
/// # Errors
///
/// [`DatabaseError::NoDatastoreInZone`] when the zone holds no active cluster.
async fn place<C: GenericClient + Sync>(
    tx: &C,
    execution_zone_id: &str,
) -> Result<String, DatabaseError> {
    let rows = tx
        .query(
            "SELECT d.id \
               FROM zeroship.datastores d \
              WHERE d.execution_zone_id = $1 \
                AND d.status = $2 \
              ORDER BY (SELECT count(*) FROM zeroship.databases db \
                         WHERE db.datastore_id = d.id), \
                       d.id \
              LIMIT 1",
            &[&execution_zone_id, &DATASTORE_STATUS_ACTIVE],
        )
        .await
        .map_err(|err| db_error(&err, "place database"))?;
    rows.first().map_or_else(
        || {
            Err(DatabaseError::NoDatastoreInZone {
                execution_zone_id: execution_zone_id.to_owned(),
            })
        },
        |row| Ok(row.get::<_, String>("id")),
    )
}

// ---------------------------------------------------------------------------
// Locks
// ---------------------------------------------------------------------------

/// Lock the organization behind a DATABASE, and refuse a dissolved one.
///
/// The peer of [`lock_project_organization`], keyed by the resource the three
/// database-scoped mutations name. Reading the database's project first and
/// then locking by that project would be two statements with a gap between
/// them; this is one.
///
/// Returns the organization id (for the audit row) and the project id (for the
/// refusals that have to name it).
async fn lock_database_organization<C: GenericClient + Sync>(
    tx: &C,
    database_id: &DatabaseId,
) -> Result<(String, String), DatabaseError> {
    let rows = tx
        .query(
            "SELECT o.id AS organization_id, p.id AS project_id, o.dissolved_at \
               FROM zeroship.databases d \
               JOIN zeroship.projects p ON p.id = d.project_id \
               JOIN zeroship.organizations o ON o.id = p.organization_id \
              WHERE d.id = $1 FOR UPDATE OF o",
            &[&database_id.as_str()],
        )
        .await
        .map_err(|err| db_error(&err, "lock database organization"))?;
    let Some(row) = rows.first() else {
        return Err(DatabaseError::DatabaseNotFound);
    };
    match row.get::<_, Option<DateTime<Utc>>>("dissolved_at") {
        Some(at) => Err(DatabaseError::OrganizationDissolved(at)),
        None => Ok((row.get("organization_id"), row.get("project_id"))),
    }
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// Every database in one project, ordered by name.
///
/// No per-row authority filter, and that is not an omission: the caller's reach
/// was decided by the Cedar gate at `Resource::Project` before this runs, and
/// every row returned belongs to that one project. This is the opposite shape
/// from [`crate::registry::Registry::list_apps_for_owner`], whose gate is
/// `Resource::Any` and which therefore MUST narrow its own rows.
///
/// # Errors
///
/// [`DatabaseError::Db`] when the statement fails.
pub async fn list_databases<C: GenericClient + Sync>(
    pg: &C,
    project_id: &str,
) -> Result<Vec<DatabaseRecord>, DatabaseError> {
    let rows = pg
        .query(
            "SELECT id, project_id, execution_zone_id, datastore_id, name, \
                    schema_epoch, status, created_at, updated_at \
               FROM zeroship.databases \
              WHERE project_id = $1 \
              ORDER BY name",
            &[&project_id],
        )
        .await
        .map_err(|err| db_error(&err, "list databases"))?;
    Ok(rows.iter().map(row_to_database).collect())
}

/// Every binding on one database with its capability, ordered by app name.
///
/// # Errors
///
/// [`DatabaseError::Db`] when the statement fails.
pub async fn list_bindings<C: GenericClient + Sync>(
    pg: &C,
    database_id: &DatabaseId,
) -> Result<Vec<BindingRecord>, DatabaseError> {
    let rows = pg
        .query(
            "SELECT b.id, b.app_id, a.name AS app_name, b.database_id, b.project_id, \
                    b.capability, b.status, b.generation, b.observed_generation, \
                    b.created_at, b.updated_at \
               FROM zeroship.database_bindings b \
               JOIN zeroship.apps a ON a.id = b.app_id \
              WHERE b.database_id = $1 \
              ORDER BY a.name",
            &[&database_id.as_str()],
        )
        .await
        .map_err(|err| db_error(&err, "list bindings"))?;
    Ok(rows.iter().map(row_to_binding).collect())
}

fn row_to_database(row: &compio_postgres::Row) -> DatabaseRecord {
    DatabaseRecord {
        id: row.get("id"),
        project_id: row.get("project_id"),
        execution_zone_id: row.get("execution_zone_id"),
        datastore_id: row.get("datastore_id"),
        name: row.get("name"),
        schema_epoch: row.get("schema_epoch"),
        status: row.get("status"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn row_to_binding(row: &compio_postgres::Row) -> BindingRecord {
    BindingRecord {
        id: row.get("id"),
        app_id: row.get("app_id"),
        app_name: row.get("app_name"),
        database_id: row.get("database_id"),
        project_id: row.get("project_id"),
        capability: row.get("capability"),
        status: row.get("status"),
        generation: row.get("generation"),
        observed_generation: row.get("observed_generation"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

// ---------------------------------------------------------------------------
// Mutations
// ---------------------------------------------------------------------------

/// Create a database in `project_id`: place it, then admit it.
///
/// Three facts have to agree and the statement makes them agree by
/// construction. The zone written is read from the PROJECT row the database is
/// placed in, so `databases_project_zone_fkey` cannot refuse it. The datastore
/// was chosen from that same zone by [`place`], so
/// `databases_placement_fkey` cannot refuse it either. And the rank predicate
/// is in the INSERT, so a caller whose seat lapsed between the Cedar call and
/// this statement inserts nothing.
///
/// The row stops at `provisioning`. A cluster reconciler is what advances it,
/// by creating the schema and the three database roles; this process holds no
/// credential for any tenant cluster and could not.
///
/// # Errors
///
/// [`DatabaseError::NoDatastoreInZone`] when the project's zone holds no active
/// cluster, [`DatabaseError::NameTaken`] on the project-name key,
/// [`DatabaseError::Insufficient`] when the caller's narrowed rank does not
/// reach `developer`, and the validation refusals.
pub async fn create_database(
    registry: &Registry,
    principal: &UserId,
    project_id: &str,
    body: &CreateDatabaseBody,
    source_ip: Option<&str>,
) -> Result<DatabaseRecord, DatabaseError> {
    let name = body.name.trim();
    validate_database_name(name)?;

    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| db_error(&err, "begin create database"))?;
    let organization_id = lock_project_organization(&tx, project_id).await?;

    // The zone is read under the lock, and it cannot move afterwards: the
    // trigger `projects_frozen_execution_zone` refuses any change to it. The
    // INSERT below re-reads it from the same row rather than binding this
    // value, so the column written is the project's own zone even if this read
    // and that statement could ever disagree.
    let zone = project_zone(&tx, project_id).await?;
    let datastore_id = place(&tx, &zone).await?;

    // `zeroship.databases.id` carries no database default: a SQL-side generator
    // would be a second minter beside `DatabaseId::mint`, and one producer per
    // identifier is what makes the derived schema name `db_<id>` answerable.
    let database_id = DatabaseId::mint();
    let sql = format!(
        "INSERT INTO zeroship.databases \
             (id, project_id, execution_zone_id, datastore_id, name, status) \
         SELECT $1, p.id, p.execution_zone_id, $3, $4, $5 \
           FROM zeroship.projects p \
           {joins} \
          WHERE p.id = $2 AND {rank} >= {developer} \
         RETURNING id, project_id, execution_zone_id, datastore_id, name, \
                   schema_epoch, status, created_at, updated_at",
        joins = project_seat_joins("p.id", "$6"),
        rank = project_seat_rank(),
        developer = ladder_rank_of(ROLE_DEVELOPER),
    );
    let rows = tx
        .query(
            &sql,
            &[
                &database_id.as_str(),
                &project_id,
                &datastore_id,
                &name,
                &STATUS_PROVISIONING,
                &principal.as_str(),
            ],
        )
        .await
        .map_err(|err| {
            if is_unique_violation(&err) {
                // `databases_project_name_key` is the only uniqueness this
                // statement can violate: the id was minted a line ago and the
                // identity key `(id, project_id)` carries it.
                DatabaseError::NameTaken(name.to_owned())
            } else {
                db_error(&err, "create database")
            }
        })?;
    let Some(row) = rows.first() else {
        // The project exists (`lock_project_organization` proved it), so the
        // only clause left is the rank predicate.
        return Err(DatabaseError::Insufficient(format!(
            "creating a database in project {project_id} needs developer authority there"
        )));
    };
    let record = row_to_database(row);

    audit_database_change(
        &tx,
        principal,
        AuditAction::DatabaseCreated,
        &organization_id,
        source_ip,
        &json!({
            "organization_id": organization_id,
            "project_id": record.project_id,
            "database_id": record.id,
            "datastore_id": record.datastore_id,
            "execution_zone_id": record.execution_zone_id,
            "name": record.name,
            "status": record.status,
        }),
    )
    .await;

    tx.commit()
        .await
        .map_err(|err| db_error(&err, "commit create database"))?;
    Ok(record)
}

/// The execution zone of one project, read inside the caller's transaction.
async fn project_zone<C: GenericClient + Sync>(
    tx: &C,
    project_id: &str,
) -> Result<String, DatabaseError> {
    let rows = tx
        .query(
            "SELECT execution_zone_id FROM zeroship.projects WHERE id = $1",
            &[&project_id],
        )
        .await
        .map_err(|err| db_error(&err, "read project execution zone"))?;
    rows.first()
        .map(|row| row.get("execution_zone_id"))
        .ok_or(DatabaseError::ProjectNotFound)
}

/// Mark a database for deletion, refusing while any app binds it.
///
/// # The row survives; the status changes
///
/// The schema and its data outlive this call, so the declaration that they
/// should go has to outlive it too. The cluster reconciler is the only process
/// that can drop them, and it must be TOLD to: it cannot tell a failed read of
/// the declarations from an empty one, so a rule of "drop whatever no row
/// names" would turn a network error into data loss. It removes the row once
/// the drop has committed, which is what frees the name under
/// `databases_project_name_key`.
///
/// Repeating the call is a no-op that succeeds: re-marking a `deleting`
/// database writes the value it already holds.
///
/// # The refusal names the apps
///
/// The `NOT EXISTS` clause is the rule, evaluated under the organization row
/// lock, and [`classify_delete_refusal`] turns zero rows into the list of apps
/// to unbind. `bind_database` takes the same lock and refuses a `deleting`
/// database, so a binding cannot be admitted between the check and the commit.
///
/// No schema and no role is dropped here, for the reason
/// [`crate::registry::Registry::archive_app`] already gives: Control holds no
/// provisioning DSN, and privileged teardown belongs to the migration service.
///
/// # Errors
///
/// [`DatabaseError::DatabaseHasBindings`] while bound,
/// [`DatabaseError::Insufficient`] below `developer`,
/// [`DatabaseError::DatabaseNotFound`] when it does not exist.
pub async fn delete_database(
    registry: &Registry,
    principal: &UserId,
    database_id: &DatabaseId,
    source_ip: Option<&str>,
) -> Result<(), DatabaseError> {
    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| db_error(&err, "begin delete database"))?;
    let (organization_id, project_id) = lock_database_organization(&tx, database_id).await?;

    let sql = format!(
        "UPDATE zeroship.databases d \
            SET status = $3, updated_at = NOW() \
           FROM zeroship.projects p \
           {joins} \
          WHERE d.id = $1 AND p.id = d.project_id AND {rank} >= {developer} \
            AND NOT EXISTS (SELECT 1 FROM zeroship.database_bindings b \
                             WHERE b.database_id = d.id) \
         RETURNING d.name",
        joins = project_seat_joins("p.id", "$2"),
        rank = project_seat_rank(),
        developer = ladder_rank_of(ROLE_DEVELOPER),
    );
    let rows = tx
        .query(
            &sql,
            &[
                &database_id.as_str(),
                &principal.as_str(),
                &STATUS_DELETING,
            ],
        )
        .await
        .map_err(|err| db_error(&err, "delete database"))?;
    let Some(row) = rows.first() else {
        return Err(classify_delete_refusal(&tx, database_id, &project_id, principal).await);
    };
    let name: String = row.get("name");

    audit_database_change(
        &tx,
        principal,
        AuditAction::DatabaseDeleted,
        &organization_id,
        source_ip,
        &json!({
            "organization_id": organization_id,
            "project_id": project_id,
            "database_id": database_id.as_str(),
            "name": name,
        }),
    )
    .await;

    tx.commit()
        .await
        .map_err(|err| db_error(&err, "commit delete database"))?;
    Ok(())
}

/// Which of the delete's three clauses refused, read in the caller's still-live
/// transaction under its lock.
///
/// Bindings first: a bound database refuses whatever the caller's seat is, so
/// telling an under-ranked caller to fix their permissions when the database is
/// also bound would name a remedy that does not unblock them.
async fn classify_delete_refusal<C: GenericClient + Sync>(
    tx: &C,
    database_id: &DatabaseId,
    project_id: &str,
    principal: &UserId,
) -> DatabaseError {
    let bound = match bound_apps(tx, database_id).await {
        Ok(bound) => bound,
        Err(err) => return err,
    };
    if !bound.is_empty() {
        return DatabaseError::DatabaseHasBindings(bound);
    }
    DatabaseError::Insufficient(format!(
        "deleting a database in project {project_id} needs developer authority there; {} does \
         not hold it",
        principal.as_str()
    ))
}

/// The apps binding a database, with the capability each holds.
async fn bound_apps<C: GenericClient + Sync>(
    tx: &C,
    database_id: &DatabaseId,
) -> Result<Vec<BoundApp>, DatabaseError> {
    let rows = tx
        .query(
            "SELECT b.app_id, a.name AS app_name, b.capability \
               FROM zeroship.database_bindings b \
               JOIN zeroship.apps a ON a.id = b.app_id \
              WHERE b.database_id = $1 \
              ORDER BY a.name",
            &[&database_id.as_str()],
        )
        .await
        .map_err(|err| db_error(&err, "read bound apps"))?;
    Ok(rows
        .iter()
        .map(|row| BoundApp {
            app_id: row.get("app_id"),
            app_name: row.get("app_name"),
            capability: row.get("capability"),
        })
        .collect())
}

/// Bind an app to a database with one capability.
///
/// # Both endpoints must already be in one project, and the statement says so
///
/// The INSERT's `SELECT` joins the app on `a.project_id = d.project_id`, taking
/// `project_id` from the DATABASE row. So the pair the two composite foreign
/// keys check is true by construction rather than by a second lookup that could
/// disagree, and an app in another project matches no row instead of producing
/// a `23503` a creator cannot read. The keys remain the enforcement - a raced
/// violation is mapped to the same typed refusal below rather than to a 500.
///
/// A DELETED app carries `project_id IS NULL`
/// (`crate::organizations::delete_app`), so the join excludes it for free: the
/// app has left its project, and a binding to it would be an edge into nothing.
///
/// The row stops at `pending`. Nothing grants any `PostgreSQL` role here; a
/// cluster reconciler is what mints `zs_bind_<binding>_e<epoch>` and advances
/// `observed_generation` to match.
///
/// # Errors
///
/// [`DatabaseError::AlreadyBound`] on the natural key,
/// [`DatabaseError::AppNotInProject`] when the app is absent, deleted or
/// elsewhere, [`DatabaseError::Insufficient`] below `developer`.
pub async fn bind_database(
    registry: &Registry,
    principal: &UserId,
    database_id: &DatabaseId,
    body: &BindDatabaseBody,
    source_ip: Option<&str>,
) -> Result<BindingRecord, DatabaseError> {
    let capability = validate_capability(body.capability.trim())?;
    let app_id = AppId::parse(body.app_id.trim())
        .map_err(|_| DatabaseError::Invalid("app_id must be an app id (app_...)".to_owned()))?;

    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| db_error(&err, "begin bind database"))?;
    let (organization_id, project_id) = lock_database_organization(&tx, database_id).await?;

    let binding_id = BindingId::mint();
    let sql = format!(
        "INSERT INTO zeroship.database_bindings \
             (id, app_id, database_id, project_id, capability, status) \
         SELECT $1, a.id, d.id, d.project_id, $4, $5 \
           FROM zeroship.databases d \
           JOIN zeroship.projects p ON p.id = d.project_id \
           JOIN zeroship.apps a ON a.id = $2 AND a.project_id = d.project_id \
           {joins} \
          WHERE d.id = $3 AND d.status = ANY($7) AND {rank} >= {developer} \
            AND NOT EXISTS (SELECT 1 FROM zeroship.database_bindings existing \
                             WHERE existing.app_id = a.id AND existing.database_id = d.id) \
         RETURNING id, app_id, \
                   (SELECT name FROM zeroship.apps WHERE id = $2) AS app_name, \
                   database_id, project_id, capability, status, generation, \
                   observed_generation, created_at, updated_at",
        joins = project_seat_joins("p.id", "$6"),
        rank = project_seat_rank(),
        developer = ladder_rank_of(ROLE_DEVELOPER),
    );
    let rows = tx
        .query(
            &sql,
            &[
                &binding_id.as_str(),
                &app_id.as_str(),
                &database_id.as_str(),
                &capability,
                &BINDING_STATUS_PENDING,
                &principal.as_str(),
                &BINDABLE_STATUSES.to_vec(),
            ],
        )
        .await
        .map_err(|err| {
            if is_unique_violation(&err) {
                // A RACE ONLY. `database_bindings_natural_key` is the one
                // uniqueness this statement can violate, and the `NOT EXISTS`
                // above already turns an existing binding into zero rows, which
                // [`classify_bind_refusal`] answers with the live capability.
                // Reaching here means a concurrent bind committed in between,
                // and the aborted transaction can no longer read that
                // capability - so it is reported empty rather than guessed, and
                // the caller is told to unbind first either way.
                DatabaseError::AlreadyBound {
                    app_id: app_id.as_str().to_owned(),
                    capability: String::new(),
                }
            } else if is_foreign_key_violation(&err) {
                DatabaseError::AppNotInProject {
                    app_id: app_id.as_str().to_owned(),
                    project_id: project_id.clone(),
                }
            } else {
                db_error(&err, "bind database")
            }
        })?;
    let Some(row) = rows.first() else {
        return Err(
            classify_bind_refusal(&tx, database_id, &app_id, &project_id, principal).await,
        );
    };
    let record = row_to_binding(row);

    audit_database_change(
        &tx,
        principal,
        AuditAction::DatabaseBound,
        &organization_id,
        source_ip,
        &json!({
            "organization_id": organization_id,
            "project_id": record.project_id,
            "database_id": record.database_id,
            "binding_id": record.id,
            "app_id": record.app_id,
            "capability": record.capability,
            "status": record.status,
        }),
    )
    .await;

    tx.commit()
        .await
        .map_err(|err| db_error(&err, "commit bind database"))?;
    Ok(record)
}

/// Which of the bind's clauses refused, read under the caller's lock.
///
/// The natural key is checked first because a live binding refuses whatever the
/// seat is, then the app's placement, then the seat. Each answer is the one
/// whose remedy actually unblocks the caller.
async fn classify_bind_refusal<C: GenericClient + Sync>(
    tx: &C,
    database_id: &DatabaseId,
    app_id: &AppId,
    project_id: &str,
    principal: &UserId,
) -> DatabaseError {
    let existing = tx
        .query(
            "SELECT capability FROM zeroship.database_bindings \
              WHERE database_id = $1 AND app_id = $2",
            &[&database_id.as_str(), &app_id.as_str()],
        )
        .await;
    match existing {
        Ok(rows) => {
            if let Some(row) = rows.first() {
                return DatabaseError::AlreadyBound {
                    app_id: app_id.as_str().to_owned(),
                    capability: row.get("capability"),
                };
            }
        }
        Err(err) => return db_error(&err, "classify bind: read existing binding"),
    }

    let unbindable = tx
        .query(
            "SELECT status FROM zeroship.databases \
              WHERE id = $1 AND status <> ALL($2)",
            &[&database_id.as_str(), &BINDABLE_STATUSES.to_vec()],
        )
        .await;
    match unbindable {
        Ok(rows) => {
            if let Some(row) = rows.first() {
                return DatabaseError::DatabaseNotBindable {
                    status: row.get("status"),
                };
            }
        }
        Err(err) => return db_error(&err, "classify bind: read database status"),
    }

    let placed = tx
        .query(
            "SELECT 1 FROM zeroship.apps WHERE id = $1 AND project_id = $2",
            &[&app_id.as_str(), &project_id],
        )
        .await;
    match placed {
        Ok(rows) if rows.is_empty() => {
            return DatabaseError::AppNotInProject {
                app_id: app_id.as_str().to_owned(),
                project_id: project_id.to_owned(),
            }
        }
        Ok(_) => {}
        Err(err) => return db_error(&err, "classify bind: read app placement"),
    }

    DatabaseError::Insufficient(format!(
        "binding an app to a database in project {project_id} needs developer authority there; \
         {} does not hold it",
        principal.as_str()
    ))
}

/// Withdraw one app's binding to a database.
///
/// # The row is DELETED, not marked
///
/// A binding row is the DECLARED edge, and the `PostgreSQL` role name is derived
/// from its id (`zs_bind_<binding>_e<epoch>`). A withdrawn edge whose row
/// survived would keep that name reserved, so a later re-bind would either
/// resurrect a role name an operator has already dropped or collide with
/// `database_bindings_natural_key`. Withdrawal is therefore the removal of the
/// declaration, and the reconciler's "converge every binding to those
/// databases" pass is what drops the roles a declaration no longer names - the
/// same pass that has to reap a role minted for a create whose row rolled back.
///
/// # Errors
///
/// [`DatabaseError::BindingNotFound`] when the app holds none,
/// [`DatabaseError::Insufficient`] below `developer`.
pub async fn unbind_database(
    registry: &Registry,
    principal: &UserId,
    database_id: &DatabaseId,
    app_id: &AppId,
    source_ip: Option<&str>,
) -> Result<(), DatabaseError> {
    let mut conn = registry.conn().await?;
    let tx = conn
        .transaction()
        .await
        .map_err(|err| db_error(&err, "begin unbind database"))?;
    let (organization_id, project_id) = lock_database_organization(&tx, database_id).await?;

    let sql = format!(
        "DELETE FROM zeroship.database_bindings b \
          USING zeroship.projects p \
          {joins} \
          WHERE b.database_id = $1 AND b.app_id = $2 AND p.id = b.project_id \
            AND {rank} >= {developer} \
         RETURNING b.id, b.capability",
        joins = project_seat_joins("p.id", "$3"),
        rank = project_seat_rank(),
        developer = ladder_rank_of(ROLE_DEVELOPER),
    );
    let rows = tx
        .query(
            &sql,
            &[
                &database_id.as_str(),
                &app_id.as_str(),
                &principal.as_str(),
            ],
        )
        .await
        .map_err(|err| db_error(&err, "unbind database"))?;
    let Some(row) = rows.first() else {
        return Err(classify_unbind_refusal(&tx, database_id, app_id, &project_id, principal).await);
    };
    let binding_id: String = row.get("id");
    let capability: String = row.get("capability");

    audit_database_change(
        &tx,
        principal,
        AuditAction::DatabaseUnbound,
        &organization_id,
        source_ip,
        &json!({
            "organization_id": organization_id,
            "project_id": project_id,
            "database_id": database_id.as_str(),
            "binding_id": binding_id,
            "app_id": app_id.as_str(),
            "capability": capability,
        }),
    )
    .await;

    tx.commit()
        .await
        .map_err(|err| db_error(&err, "commit unbind database"))?;
    Ok(())
}

/// Which of the unbind's clauses refused, read under the caller's lock.
async fn classify_unbind_refusal<C: GenericClient + Sync>(
    tx: &C,
    database_id: &DatabaseId,
    app_id: &AppId,
    project_id: &str,
    principal: &UserId,
) -> DatabaseError {
    let rows = tx
        .query(
            "SELECT 1 FROM zeroship.database_bindings WHERE database_id = $1 AND app_id = $2",
            &[&database_id.as_str(), &app_id.as_str()],
        )
        .await;
    match rows {
        Ok(rows) if rows.is_empty() => {
            return DatabaseError::BindingNotFound {
                app_id: app_id.as_str().to_owned(),
            }
        }
        Ok(_) => {}
        Err(err) => return db_error(&err, "classify unbind: read binding"),
    }
    DatabaseError::Insufficient(format!(
        "unbinding an app from a database in project {project_id} needs developer authority \
         there; {} does not hold it",
        principal.as_str()
    ))
}

/// Write one database-lifecycle row, INSIDE the caller's transaction.
///
/// Placement and binding are authority facts - which app may reach which data -
/// so the row lives or dies with the effect it describes, the same way
/// `crate::organizations` writes its authority changes. The DECISION row goes
/// the other way and is written by `zeroship_authz::enforce` on the shared
/// client, so a refusal's record survives the rollback of what it refused.
async fn audit_database_change<C: GenericClient + Sync>(
    tx: &C,
    actor: &UserId,
    action: AuditAction,
    organization_id: &str,
    source_ip: Option<&str>,
    detail: &serde_json::Value,
) {
    audit::log_in_tx(
        tx,
        AuditEntry {
            app_id: None,
            organization_id: None,
            actor_user_id: Some(actor),
            action,
            resource: Some(organization_id),
            source_ip,
        },
        detail,
    )
    .await;
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------
//
// EVERY handler calls `authz.require(...)` BEFORE opening a transaction, for
// the reason `crate::organizations` states: `enforce` writes the decision row
// on the shared client, so a refusal's record commits independently of the
// effect it refused.
//
// WHICH RESOURCE EACH GATE NAMES. Creating and listing name the PROJECT,
// because the database does not exist yet or is not singled out. Everything
// else names the DATABASE - and those are the calls that make a database row
// reachable at all, since `Resource::Database` is what
// `zeroship_authz::authority::resolve` turns into the owning project.

fn bad_id(kind: &str) -> web::HttpResponse {
    web::HttpResponse::BadRequest().json(&json!({"error": format!("bad {kind}")}))
}

fn parse_project_path(raw: &str) -> Result<String, web::HttpResponse> {
    ProjectId::parse(raw)
        .map(|id| id.as_str().to_string())
        .map_err(|_| bad_id("project_id"))
}

fn parse_database_path(raw: &str) -> Result<DatabaseId, web::HttpResponse> {
    DatabaseId::parse(raw).map_err(|_| bad_id("database_id"))
}

fn parse_app_path(raw: &str) -> Result<AppId, web::HttpResponse> {
    AppId::parse(raw).map_err(|_| bad_id("app_id"))
}

pub async fn list_databases_handler(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(resp) = admin_rate_limit(&req, &state).await {
        return resp;
    }
    let project_id = match parse_project_path(&path) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::DatabaseRead,
            Resource::Project {
                id: project_id.clone(),
            },
            &state,
        )
        .await
    {
        return resp;
    }
    match list_databases(state.control_pg.as_ref(), &project_id).await {
        Ok(records) => web::HttpResponse::Ok().json(&json!({ "databases": records })),
        Err(e) => e.into_response(),
    }
}

pub async fn create_database_handler(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    body: Json<CreateDatabaseBody>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(resp) = admin_rate_limit(&req, &state).await {
        return resp;
    }
    let project_id = match parse_project_path(&path) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::DatabaseWrite,
            Resource::Project {
                id: project_id.clone(),
            },
            &state,
        )
        .await
    {
        return resp;
    }
    let ip = http_util::source_ip(&req, state.trust_proxy);
    match create_database(
        &state.registry,
        &authz.principal_id,
        &project_id,
        &body,
        ip.as_deref(),
    )
    .await
    {
        Ok(record) => web::HttpResponse::Created().json(&record),
        Err(e) => e.into_response(),
    }
}

pub async fn delete_database_handler(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(resp) = admin_rate_limit(&req, &state).await {
        return resp;
    }
    let database_id = match parse_database_path(&path) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::DatabaseWrite,
            Resource::Database {
                id: database_id.clone(),
            },
            &state,
        )
        .await
    {
        return resp;
    }
    let ip = http_util::source_ip(&req, state.trust_proxy);
    match delete_database(
        &state.registry,
        &authz.principal_id,
        &database_id,
        ip.as_deref(),
    )
    .await
    {
        Ok(()) => web::HttpResponse::NoContent().finish(),
        Err(e) => e.into_response(),
    }
}

pub async fn list_bindings_handler(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(resp) = admin_rate_limit(&req, &state).await {
        return resp;
    }
    let database_id = match parse_database_path(&path) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::DatabaseRead,
            Resource::Database {
                id: database_id.clone(),
            },
            &state,
        )
        .await
    {
        return resp;
    }
    match list_bindings(state.control_pg.as_ref(), &database_id).await {
        Ok(records) => web::HttpResponse::Ok().json(&json!({ "bindings": records })),
        Err(e) => e.into_response(),
    }
}

pub async fn bind_database_handler(
    req: web::HttpRequest,
    path: Path<String>,
    authz: AuthzGuard,
    body: Json<BindDatabaseBody>,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(resp) = admin_rate_limit(&req, &state).await {
        return resp;
    }
    let database_id = match parse_database_path(&path) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::DatabaseWrite,
            Resource::Database {
                id: database_id.clone(),
            },
            &state,
        )
        .await
    {
        return resp;
    }
    let ip = http_util::source_ip(&req, state.trust_proxy);
    match bind_database(
        &state.registry,
        &authz.principal_id,
        &database_id,
        &body,
        ip.as_deref(),
    )
    .await
    {
        Ok(record) => web::HttpResponse::Created().json(&record),
        Err(e) => e.into_response(),
    }
}

pub async fn unbind_database_handler(
    req: web::HttpRequest,
    path: Path<(String, String)>,
    authz: AuthzGuard,
    state: State<Arc<AppState>>,
) -> web::HttpResponse {
    if let Some(resp) = admin_rate_limit(&req, &state).await {
        return resp;
    }
    let (raw_database, raw_app) = path.into_inner();
    let database_id = match parse_database_path(&raw_database) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let app_id = match parse_app_path(&raw_app) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = authz
        .require(
            AuthzAction::DatabaseWrite,
            Resource::Database {
                id: database_id.clone(),
            },
            &state,
        )
        .await
    {
        return resp;
    }
    let ip = http_util::source_ip(&req, state.trust_proxy);
    match unbind_database(
        &state.registry,
        &authz.principal_id,
        &database_id,
        &app_id,
        ip.as_deref(),
    )
    .await
    {
        Ok(()) => web::HttpResponse::NoContent().finish(),
        Err(e) => e.into_response(),
    }
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    let payload = || web::types::PayloadConfig::new(DATABASE_PAYLOAD_BYTES);
    cfg.service(
        web::resource("/api/projects/{project_id}/databases")
            .state(payload())
            .route(web::get().to(list_databases_handler))
            .route(web::post().to(create_database_handler)),
    )
    .service(
        web::resource("/api/databases/{database_id}")
            .route(web::delete().to(delete_database_handler)),
    )
    .service(
        web::resource("/api/databases/{database_id}/bindings")
            .state(payload())
            .route(web::get().to(list_bindings_handler))
            .route(web::post().to(bind_database_handler)),
    )
    .service(
        web::resource("/api/databases/{database_id}/bindings/{app_id}")
            .route(web::delete().to(unbind_database_handler)),
    );
}

#[cfg(test)]
mod tests {
    use super::{
        validate_capability, validate_database_name, DatabaseError, CAPABILITY_READONLY,
        CAPABILITY_READWRITE, DATABASE_ID_PREFIX,
    };
    use zeroship_core::DatabaseId;

    /// The reserved prefix and the id type must name ONE namespace. A second
    /// literal nobody compares is how they would come to name two.
    #[test]
    fn the_reserved_prefix_is_the_one_the_id_type_uses() {
        assert_eq!(DATABASE_ID_PREFIX, format!("{}_", DatabaseId::PREFIX));
    }

    /// A name in the id namespace is refused as RESERVED, not as malformed -
    /// the two are different mistakes with different fixes. The paired control
    /// is a legal name, so "refuses everything" cannot pass this.
    #[test]
    fn a_name_in_the_database_id_namespace_is_reserved() {
        let minted = DatabaseId::mint();
        for reserved in [
            minted.as_str(),
            "dbs_",
            "dbs_short",
            "DBS_0000123456789abcdefghijkl",
        ] {
            assert!(
                matches!(
                    validate_database_name(reserved),
                    Err(DatabaseError::ReservedName(_))
                ),
                "{reserved:?} must be refused as a reserved name"
            );
        }
        for legal in ["main", "analytics", "db-2", "db_2", "a"] {
            assert!(
                validate_database_name(legal).is_ok(),
                "{legal:?} is a legal display name"
            );
        }
    }

    #[test]
    fn a_malformed_name_is_invalid_rather_than_reserved() {
        for malformed in ["", "has space", "slash/es", &"x".repeat(65)] {
            assert!(
                matches!(
                    validate_database_name(malformed),
                    Err(DatabaseError::Invalid(_))
                ),
                "{malformed:?} must be refused as malformed"
            );
        }
        assert!(validate_database_name(&"x".repeat(64)).is_ok());
    }

    /// The two capabilities, and nothing else. The rejected set includes the
    /// spellings a caller is most likely to try.
    #[test]
    fn only_the_two_declared_capabilities_are_accepted() {
        assert_eq!(
            validate_capability(CAPABILITY_READWRITE).expect("readwrite"),
            CAPABILITY_READWRITE
        );
        assert_eq!(
            validate_capability(CAPABILITY_READONLY).expect("readonly"),
            CAPABILITY_READONLY
        );
        for rejected in ["", "read-write", "readWrite", "rw", "write", "admin", "owner"] {
            assert!(
                matches!(
                    validate_capability(rejected),
                    Err(DatabaseError::Invalid(_))
                ),
                "{rejected:?} is not a capability"
            );
        }
    }
}
