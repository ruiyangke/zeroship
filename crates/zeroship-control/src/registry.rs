//! Registry — application CRUD backed by PostgreSQL (compio-postgres).

use std::collections::HashMap;

use compio_postgres::error::SqlState;
use compio_postgres::{Client, NoTls};
use zeroship_core::app_derivation;
use zeroship_core::app_id::AppId;
use zeroship_core::user_id::UserId;
use zeroship_core::types::{
    AppNetPolicy, AppNetPolicyLimits, AppRecord, AppRuntimeLimits, AppVersionInfo,
    GatewayFamilyRevocation, GatewayPrincipalLifecycle, GatewaySnapshot, NetEgressEntry, RouteEntry,
    RouteMap, VersionMap,
    FREE_TIER_NET_POLICY_LIMITS, FREE_TIER_RUNTIME_LIMITS,
};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum RegistryError {
    NotFound(String),
    AlreadyExists(String),
    Database(String),
    InvalidInput(String),
    /// The operation is refused because it would violate a business invariant
    /// that is not a simple uniqueness clash — e.g. hard-deleting an app that has
    /// financial history (an `invoice_lines` row pins it via `ON DELETE RESTRICT`,
    /// and a billed app must be ANONYMIZED, not deleted). A TYPED conflict so the
    /// caller can map it to a clear status instead of leaking a raw DB error.
    Conflict(String),
    /// The requested app name is a hostname label the platform edge already
    /// claims (`crates/zeroship-control/src/reserved_names.rs`). Typed separately from
    /// [`Self::InvalidInput`] because the two are different outcomes with
    /// different fixes: a charset failure says the name is malformed, this says
    /// a well-formed name is unavailable. A caller that cannot tell them apart
    /// tells the creator to fix the wrong thing.
    ReservedName(String),
    /// The global default FX is missing, so the platform cannot price any
    /// inheriting plan (billing-v2 MAJOR-2). A billing sweep that hits this
    /// must ABORT (bill no one) rather than emit base-only $0 invoices — it is
    /// surfaced as a distinct variant so the sweep can fail closed instead of
    /// logging-and-continuing past a revenue leak.
    FxUnresolved,
    /// The deploy was refused because its runtime schema descriptor does not
    /// name the schema the app's database actually holds.
    ///
    /// Typed separately from [`Self::Conflict`] because the remedy is a command
    /// the creator runs, and the response body carries it. Both fields are
    /// `Option` because the two refusals are different mistakes:
    ///
    /// - `descriptor = Some`, `applied = Some(other)` / `None` — the code was
    ///   built against a schema this app has not migrated to. Run the migration.
    /// - `descriptor = None`, `applied = Some(_)` — the artifact declares NO
    ///   schema at all while the app has one. Deploying it would boot the app
    ///   with `env.db` uninstalled over a live database.
    SchemaNotApplied {
        descriptor_sha256: Option<String>,
        applied_sha256: Option<String>,
    },
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(s) => write!(f, "not found: {s}"),
            Self::AlreadyExists(s) => write!(f, "already exists: {s}"),
            Self::Database(s) => write!(f, "database: {s}"),
            Self::InvalidInput(s) => write!(f, "invalid input: {s}"),
            Self::Conflict(s) => write!(f, "conflict: {s}"),
            Self::ReservedName(s) => write!(f, "reserved name: {s}"),
            Self::FxUnresolved => write!(
                f,
                "global default FX missing — platform cannot price; aborting billing sweep \
                 rather than billing $0"
            ),
            Self::SchemaNotApplied {
                descriptor_sha256,
                applied_sha256,
            } => match descriptor_sha256 {
                Some(descriptor) => write!(
                    f,
                    "schema not applied: this build's runtime descriptor is {descriptor}, but \
                     the app's newest applied migration recorded {}",
                    applied_sha256.as_deref().unwrap_or("no descriptor at all")
                ),
                None => write!(
                    f,
                    "schema descriptor missing: this artifact declares no runtime schema, but \
                     the app has applied migrations (newest descriptor {})",
                    applied_sha256.as_deref().unwrap_or("unrecorded")
                ),
            },
        }
    }
}

impl From<compio_postgres::Error> for RegistryError {
    fn from(e: compio_postgres::Error) -> Self {
        if matches!(e.code(), Some(code) if code == &SqlState::UNIQUE_VIOLATION) {
            Self::AlreadyExists("resource already exists".into())
        } else if e.code() == Some(&SqlState::T_R_DEADLOCK_DETECTED) {
            Self::Database(format!("retryable deadlock: {e}"))
        } else {
            let msg = e.to_string();
            let full = match source_chain(&e) {
                Some(chain) => format!("{msg}: {chain}"),
                None => msg,
            };
            Self::Database(full)
        }
    }
}

/// Walk an error's `source()` chain and concatenate the messages. Useful for
/// reaching through the opaque `compio_postgres::Error` wrapper to the
/// `DbError` inside.
fn source_chain(err: &dyn std::error::Error) -> Option<String> {
    let mut out = String::new();
    let mut cur: Option<&dyn std::error::Error> = err.source();
    while let Some(e) = cur {
        if !out.is_empty() {
            out.push_str(" | ");
        }
        out.push_str(&e.to_string());
        cur = e.source();
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Application registry backed by PostgreSQL. Stores the DB URL and creates a
/// fresh connection per query — suitable for the low-traffic control plane.
///
/// `Clone` is cheap (just a `String` copy) so `AppState` can hold a separate
/// handle alongside the `EnvStore`'s internal one.
#[derive(Clone, Debug)]
pub struct Registry {
    db_url: String,
}

/// Open a new compio-postgres connection and detach its driver task onto the
/// compio runtime. Returns the [`Client`] handle.
async fn open_conn(url: &str) -> Result<Client, compio_postgres::Error> {
    let (client, connection) = compio_postgres::connect(url, NoTls).await?;
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            tracing::error!(error = %e, "control: pg connection error");
        }
    })
    .detach();
    Ok(client)
}

impl Registry {
    /// Connect to validate the database is reachable, then return a `Registry`.
    ///
    /// The schema is owned by zeroship-migrate (`db/migrations-ts`, platform
    /// profile), applied out of band before the service boots (the `migrate`
    /// compose step / `deploy/ops/db-migrate.sh`).
    /// `Registry` never creates or alters tables.
    pub async fn new(db_url: &str) -> Result<Self, String> {
        // Fail fast if the database is unreachable; the schema must already
        // exist. Dropping `conn` sends Terminate and exits the driver task.
        let conn = open_conn(db_url).await.map_err(|e| e.to_string())?;
        drop(conn);

        Ok(Self {
            db_url: db_url.to_string(),
        })
    }

    /// Open a fresh connection. Crate-internal: stores + internal
    /// handlers are the only callers. Integration tests that need
    /// raw DB access go through narrow `__…_for_test` helpers on
    /// the store types (e.g. `EnvStore::__raw_ciphertext_for_test`).
    pub(crate) async fn conn(&self) -> Result<Client, RegistryError> {
        open_conn(&self.db_url).await.map_err(RegistryError::from)
    }

    pub(crate) fn workflow_store_db_url(&self) -> &str {
        &self.db_url
    }

    /// Validate that `plan_id` names a real, UNARCHIVED plan in the catalog.
    /// Returns a clean [`RegistryError::InvalidInput`] (not a raw FK violation)
    /// for an unknown or archived plan — the server-side gate that closes the
    /// CT-A1 free-text self-escalation. Runs on a borrowed connection so it
    /// composes inside an existing transaction.
    async fn validate_plan<C: compio_postgres::GenericClient + Sync>(
        conn: &C,
        plan_id: &str,
    ) -> Result<(), RegistryError> {
        let rows = conn
            .query(
                "SELECT archived FROM zeroship.plans WHERE id = $1",
                &[&plan_id],
            )
            .await?;
        match rows.first() {
            None => Err(RegistryError::InvalidInput(format!(
                "unknown plan '{plan_id}' (not in the plan catalog)"
            ))),
            Some(row) => {
                let archived: bool = row.get("archived");
                if archived {
                    Err(RegistryError::InvalidInput(format!(
                        "plan '{plan_id}' is archived and cannot be assigned"
                    )))
                } else {
                    Ok(())
                }
            }
        }
    }

    // -- App CRUD -----------------------------------------------------------

    /// Create a new application inside `project_id`. Returns the created
    /// `AppRecord`.
    ///
    /// # No membership row is written here any more
    ///
    /// `zeroship.app_members` is deleted. An app carries no membership of its
    /// own: authority over it is the caller's ORGANIZATION seat, narrowed by
    /// the project the app sits in, and `apps.project_id -> projects.
    /// organization_id` is the only path between the two. Writing an app-level
    /// row would be writing a second answer to a question that now has one.
    ///
    /// # The rank predicate is in the INSERT
    ///
    /// The caller's authority is not taken on trust from the handler that
    /// already ran Cedar. The INSERT's `SELECT` joins the caller's live
    /// organization seat and, when they are below admin, their project seat -
    /// and applies the SAME narrowing rule
    /// [`zeroship_authz::effective_project_rank`] applies. A caller removed
    /// from the project between the Cedar call and this statement inserts
    /// nothing, and zero rows IS the refusal.
    ///
    /// # `project_id: None` is the zero-config path, not a default
    ///
    /// `None` means "wherever this owner's own apps go": their personal
    /// organization's default project, minted on demand by
    /// [`crate::organizations::ensure_personal_project`]. A creator's first
    /// deploy must stay ONE step, and the alternative - make them create an
    /// organization, then a project, then an app - is three round trips of
    /// decisions they have no information to make yet.
    ///
    /// It is an `Option` rather than a separate method so there is exactly one
    /// implementation of that rule, reached by production and by every fixture.
    /// The HTTP handler still resolves the project explicitly before it calls
    /// this, because the Cedar gate has to NAME the project it is authorizing
    /// against - so on that path this argument is always `Some`.
    ///
    /// Runs on a DEDICATED owned connection so the RAII `transaction()` guard
    /// owns it and an aborted txn never poisons a shared handle.
    pub async fn create_app(
        &self,
        name: &str,
        plan_id: &str,
        owner_id: &UserId,
        project_id: Option<&str>,
    ) -> Result<AppRecord, RegistryError> {
        validate_app_name(name)?;

        // Resolve the landing place BEFORE the app transaction opens.
        // `ensure_personal_project` runs its own transaction and is idempotent,
        // so holding one open across it would be a nested lock for no gain.
        let personal_project;
        let project_id = match project_id {
            Some(project_id) => project_id,
            None => {
                personal_project =
                    crate::organizations::ensure_personal_project(self, owner_id)
                        .await
                        .map_err(|err| {
                            RegistryError::Database(format!(
                                "provision personal project for {}: {err:?}",
                                owner_id.as_str()
                            ))
                        })?;
                personal_project.as_str()
            }
        };

        let mut conn = self.conn().await?;
        let tx = conn.transaction().await?;

        // Server-side plan gate: the plan must exist and be
        // unarchived in the catalog. Checked inside the txn before the INSERT
        // so an invalid plan returns a clean InvalidInput AND never leaves a
        // half-written app row (the FK would also reject it, but this gives a
        // typed error and an archived-plan check the FK can't).
        Self::validate_plan(&tx, plan_id).await?;

        // `zeroship.apps.id` carries no database default: a SQL-side generator
        // would be a second minter beside `AppId::mint`, and one producer per
        // identifier is what makes a derived name (schema, role, publication,
        // …) answerable. This is that one producer.
        let app_id = AppId::mint();
        let rows = tx
            .query(
                &format!(
                    // `organization_id` is taken from the SAME project row the
                    // app is being placed in, so the pair the composite key
                    // `(project_id, organization_id) -> projects(id,
                    // organization_id)` checks is true by construction rather
                    // than by a second lookup that could disagree.
                    "INSERT INTO zeroship.apps (id, name, plan_id, project_id, organization_id) \
                     SELECT $1, $2, $3, p.id, p.organization_id \
                       FROM zeroship.projects p \
                       LEFT JOIN zeroship.organization_members m \
                              ON m.organization_id = p.organization_id AND m.user_id = $5 \
                       LEFT JOIN zeroship.organization_roles organization_role \
                              ON organization_role.role = m.role \
                       LEFT JOIN zeroship.project_members pm \
                              ON pm.project_id = p.id AND pm.user_id = $5 \
                       LEFT JOIN zeroship.organization_roles project_role \
                              ON project_role.role = pm.role \
                      WHERE p.id = $4 AND {effective} >= {developer} \
                     RETURNING id, name, plan_id, deploy_hash, \
                               archived_at::text, created_at::text, updated_at::text",
                    effective = crate::organizations::effective_project_rank_sql(
                        "organization_role.rank",
                        "project_role.rank",
                        &crate::organizations::ladder_rank_of(crate::organizations::ROLE_ADMIN),
                    ),
                    developer = crate::organizations::ladder_rank_of(crate::organizations::ROLE_DEVELOPER),
                ),
                &[&app_id.as_str(), &name, &plan_id, &project_id, &owner_id.as_str()],
            )
            .await?;
        let Some(row) = rows.first() else {
            // Zero rows means the project does not exist or the caller's
            // narrowed rank does not reach `developer` there. Both are the
            // caller's problem, so neither may be reported as a database
            // failure - `RegistryError::Database` would surface as a 500 and
            // send them looking at the platform instead of at their seat.
            return Err(RegistryError::Conflict(format!(
                "cannot create an app in project {project_id}: it does not exist, or you do not \
                 hold developer authority there"
            )));
        };
        let record = row_to_record(row)?;

        tx.commit().await?;
        Ok(record)
    }

    /// Get an app by primary key.
    pub async fn get_app(&self, id: &AppId) -> Result<Option<AppRecord>, RegistryError> {
        let conn = self.conn().await?;
        let rows = conn
            .query(
                "SELECT id, name, plan_id, deploy_hash, archived_at::text, \
                        created_at::text, updated_at::text \
                 FROM zeroship.apps WHERE id = $1",
                &[&id.as_str()],
            )
            .await?;
        rows.first().map(row_to_record).transpose()
    }

    /// Get an app by unique name.
    pub async fn get_app_by_name(&self, name: &str) -> Result<Option<AppRecord>, RegistryError> {
        let conn = self.conn().await?;
        let rows = conn
            .query(
                "SELECT id, name, plan_id, deploy_hash, archived_at::text, \
                        created_at::text, updated_at::text \
                 FROM zeroship.apps WHERE name = $1",
                &[&name],
            )
            .await?;
        rows.first().map(row_to_record).transpose()
    }

    /// List the apps `owner_id` can reach, ordered by name.
    ///
    /// This is the creator-facing listing: the `/api/apps` GET grants every
    /// creator `apps:read` on the platform surface (self-service policy), but
    /// the data it returns MUST be scoped to apps the principal actually
    /// reaches — otherwise the broadened gate becomes a fleet-wide cross-tenant
    /// read.
    ///
    /// "Reaches" is now the narrowing rule rather than a membership row: the
    /// caller's organization seat, ceilinged by their project seat when they
    /// are below admin, must clear `viewer`. An organization developer with no
    /// row on a project sees none of that project's apps — which is the whole
    /// point of per-project narrowing, and is a behaviour the old app-level
    /// membership could not express.
    pub async fn list_apps_for_owner(
        &self,
        owner_id: &UserId,
    ) -> Result<Vec<AppRecord>, RegistryError> {
        let conn = self.conn().await?;
        let rows = conn
            .query(
                &format!(
                    "SELECT a.id, a.name, a.plan_id, a.deploy_hash, \
                            a.archived_at::text, a.created_at::text, a.updated_at::text \
                     FROM zeroship.apps a \
                     JOIN zeroship.projects p ON p.id = a.project_id \
                     LEFT JOIN zeroship.organization_members m \
                            ON m.organization_id = p.organization_id AND m.user_id = $1 \
                     LEFT JOIN zeroship.organization_roles organization_role \
                            ON organization_role.role = m.role \
                     LEFT JOIN zeroship.project_members pm \
                            ON pm.project_id = p.id AND pm.user_id = $1 \
                     LEFT JOIN zeroship.organization_roles project_role \
                            ON project_role.role = pm.role \
                     WHERE {effective} >= {viewer} \
                     ORDER BY a.name",
                    effective = crate::organizations::effective_project_rank_sql(
                        "organization_role.rank",
                        "project_role.rank",
                        &crate::organizations::ladder_rank_of(crate::organizations::ROLE_ADMIN),
                    ),
                    viewer = crate::organizations::ladder_rank_of(crate::organizations::ROLE_VIEWER),
                ),
                &[&owner_id.as_str()],
            )
            .await?;
        rows.iter().map(row_to_record).collect()
    }

    /// Archive an app without deleting any app-attributed state.
    ///
    /// The first transition records its timestamp; retries return the same
    /// record without moving that timestamp or `updated_at`. Route publication
    /// and workflow admission enforce this marker. A deploy may replace the
    /// retained artifact while archived, but cannot make it routable or
    /// schedulable. Database schema and role lifecycle is intentionally absent
    /// here: control has no provisioning DSN and privileged teardown belongs to
    /// migrate-server.
    pub async fn archive_app(&self, id: &AppId) -> Result<Option<AppRecord>, RegistryError> {
        let mut conn = self.conn().await?;
        let tx = conn.transaction().await?;
        // Workflow admissions and the worker's final claim take the shared
        // form of this lock. Once archive returns, no claim can have crossed
        // the marker; work admitted before it may still finish.
        tx.query_one(
            "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
            &[&app_derivation::lifecycle_lock_seed(id)],
        )
        .await?;
        let rows = tx
            .query(
                "UPDATE zeroship.apps \
                    SET archived_at = COALESCE(archived_at, NOW()), \
                        updated_at = CASE WHEN archived_at IS NULL THEN NOW() ELSE updated_at END \
                  WHERE id = $1 AND deleted_at IS NULL \
                  RETURNING id, name, plan_id, deploy_hash, \
                            archived_at::text, created_at::text, updated_at::text",
                &[&id.as_str()],
            )
            .await?;
        tx.commit().await?;
        rows.first().map(row_to_record).transpose()
    }

    /// Restore an archived app. Retained name, manifests, billing evidence, and
    /// workflow records become active through their normal polling paths. The
    /// retained deploy is checked against the latest applied schema descriptor
    /// while the app row is locked; restore must not bypass the deploy gate.
    ///
    /// A DELETED app is reported absent rather than restored. Deletion is the
    /// terminal transition (`crate::organizations::delete_app`) and archive is
    /// the reversible one; a restore that could undo the terminal step would
    /// make the two the same transition with different names. `zeroship_authz`
    /// already denies every app-scoped action on a deleted app, because it
    /// resolves an app's organization through the project the delete detaches -
    /// so this guard is the one that holds if that ever stops being true.
    pub async fn unarchive_app(&self, id: &AppId) -> Result<Option<AppRecord>, RegistryError> {
        let mut conn = self.conn().await?;
        let tx = conn.transaction().await?;
        tx.query_one(
            "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
            &[&app_derivation::lifecycle_lock_seed(id)],
        )
        .await?;
        let state = tx
            .query(
                "SELECT archived_at IS NOT NULL AS archived, manifest_json \
                   FROM zeroship.apps \
                  WHERE id = $1 AND deleted_at IS NULL \
                  FOR UPDATE",
                &[&id.as_str()],
            )
            .await?;
        let Some(state) = state.first() else {
            tx.commit().await?;
            return Ok(None);
        };

        if state.get::<_, bool>("archived") {
            let apply_in_progress: bool = tx
                .query_one(
                    "SELECT EXISTS ( \
                         SELECT 1 FROM zeroship.app_schema_applies \
                          WHERE app_id = $1 AND status = 'submitted' \
                     ) AS applying",
                    &[&id.as_str()],
                )
                .await?
                .get("applying");
            if apply_in_progress {
                return Err(RegistryError::Conflict(format!(
                    "app {} has a schema apply in progress; retry restore after it finishes",
                    id.as_str()
                )));
            }
            let manifest_json: Option<String> = state.get("manifest_json");
            let descriptor_sha256 = match manifest_json {
                Some(json) => {
                    let manifest = serde_json::from_str::<zeroship_bundle::Manifest>(&json)
                        .map_err(|error| {
                            RegistryError::Conflict(format!(
                                "archived app {} has an invalid retained manifest: {error}",
                                id.as_str()
                            ))
                        })?;
                    manifest.validate().map_err(|error| {
                        RegistryError::Conflict(format!(
                            "archived app {} has an invalid retained manifest: {error}",
                            id.as_str()
                        ))
                    })?;
                    manifest.runtime_descriptor.map(|entry| entry.hash)
                }
                None => None,
            };
            let applied_sha256 = tx
                .query(
                    "SELECT m.descriptor_sha256 \
                       FROM zeroship.app_schema_applies m \
                      WHERE m.app_id = $1 AND m.status = 'applied' \
                      ORDER BY m.applied_at DESC NULLS LAST, m.submitted_at DESC, \
                               m.migration_id DESC \
                      LIMIT 1",
                    &[&id.as_str()],
                )
                .await?
                .first()
                .map(|row| row.get::<_, String>("descriptor_sha256"));
            if descriptor_sha256 != applied_sha256 {
                return Err(RegistryError::SchemaNotApplied {
                    descriptor_sha256,
                    applied_sha256,
                });
            }
        }

        let rows = tx
            .query(
                "UPDATE zeroship.apps \
                    SET archived_at = NULL, \
                        updated_at = CASE WHEN archived_at IS NOT NULL THEN NOW() ELSE updated_at END \
                  WHERE id = $1 AND deleted_at IS NULL \
                  RETURNING id, name, plan_id, deploy_hash, \
                            archived_at::text, created_at::text, updated_at::text",
                &[&id.as_str()],
            )
            .await?;
        tx.commit().await?;
        rows.first().map(row_to_record).transpose()
    }

    /// Atomic deploy commit. Sets `deploy_hash` and `manifest_json` in
    /// the same UPDATE so the gateway never observes a half-applied
    /// deploy. Used by the .zship ingest path. Archived apps accept this as a
    /// staged deploy: active-route and workflow projections continue to omit
    /// the app until restore.
    ///
    /// THIS STATEMENT IS WHAT "LIVE" MEANS. The gateway reads exactly these two
    /// columns ([`Self::get_gateway_snapshot`]) and polls them; the
    /// `zeroship.app_deploys` insert below is history. So the schema
    /// precondition is a PREDICATE ON THIS UPDATE, not a check in the handler.
    /// The precedent is [`Self::set_plan`] directly below: a separate
    /// validate-then-UPDATE there had a TOCTOU window and was replaced by an
    /// `EXISTS` subquery in the same statement. The same window here is worse,
    /// because what races is a concurrent migration.
    ///
    /// `descriptor_sha256` is `manifest.runtime_descriptor.hash` — the sha256 of
    /// the `schema.runtime.json` this artifact carries, or `None` for an app
    /// that declares no schema. The predicate has two arms and both are load-
    /// bearing:
    ///
    /// - Present: it must equal the descriptor recorded on the app's NEWEST
    ///   APPLIED migration row. Newest, not "any" — membership would let a
    ///   rollback through, since N-1's hash was applied once and an `IN (...)`
    ///   test passes while the database sits at N.
    /// - Absent: allowed only when the app has NO applied schema.
    ///   `runtime_descriptor` is `skip_serializing_if = "Option::is_none"` on a
    ///   creator-produced artifact, so a one-armed guard is bypassed by deleting
    ///   one JSON key — and the app then boots with `env.db` uninstalled over a
    ///   live database. A guard whose bypass is "omit the field" is not a guard.
    ///
    /// The ledger is `zeroship.app_schema_applies`, which the migration service
    /// writes one row of per APPLY REQUEST - including a request that applied
    /// nothing because every document was already journalled. That is deliberate:
    /// an engine upgrade which changes descriptor bytes without changing any
    /// schema is repaired by running a migrate that applies nothing, and it is
    /// only a repair because the row is still written. The engine's OWN journal
    /// cannot be read here and must not be: it lives in the app's schema, whose
    /// migrator role owns it and can drop it.
    ///
    /// There is no operator override, and that is not a policy about overrides.
    /// The remedy is a creator-reachable endpoint that is already mandatory in
    /// the golden path, and the case that feels like it needs one — a code
    /// rollback across a migration boundary — is not made safe by an override.
    pub async fn set_deploy_with_manifest(
        &self,
        id: &AppId,
        deploy_hash: &str,
        manifest_json: &str,
        descriptor_sha256: Option<&str>,
    ) -> Result<bool, RegistryError> {
        let mut conn = self.conn().await?;
        let tx = conn.transaction().await?;
        let n = tx
            .execute(
                "UPDATE zeroship.apps SET deploy_hash = $1, manifest_json = $2, \
                 updated_at = NOW() WHERE id = $3 AND deleted_at IS NULL \
                   AND CASE WHEN $4::text IS NULL \
                            THEN NOT EXISTS (SELECT 1 FROM zeroship.app_schema_applies \
                                              WHERE app_id = $3 AND status = 'applied') \
                            ELSE $4::text = (SELECT m.descriptor_sha256 \
                                               FROM zeroship.app_schema_applies m \
                                              WHERE m.app_id = $3 AND m.status = 'applied' \
                                              ORDER BY m.applied_at DESC NULLS LAST, \
                                                       m.submitted_at DESC, \
                                                       m.migration_id DESC \
                                              LIMIT 1) \
                       END",
                &[&deploy_hash, &manifest_json, &id.as_str(), &descriptor_sha256],
            )
            .await?;
        if n == 0 {
            // Zero rows is ambiguous: no such app, the app is DELETED, or the
            // schema predicate refused. Disambiguate for the MESSAGE only — the
            // decision was already taken atomically above, so this read cannot
            // re-open it.
            //
            // A deleted app reads as ABSENT rather than as a schema refusal,
            // which is both the truthful answer and the one `unarchive_app`
            // already gives (`Ok(None)`). Reporting `SchemaNotApplied` for a
            // corpse would send a creator to re-run a migration against an app
            // that no longer has a schema to migrate.
            let app_rows = tx
                .query(
                    "SELECT 1 FROM zeroship.apps WHERE id = $1 AND deleted_at IS NULL",
                    &[&id.as_str()],
                )
                .await?;
            if app_rows.is_empty() {
                tx.commit().await?;
                return Ok(false);
            }
            let applied_sha256 = tx
                .query(
                    "SELECT m.descriptor_sha256 FROM zeroship.app_schema_applies m \
                      WHERE m.app_id = $1 AND m.status = 'applied' \
                      ORDER BY m.applied_at DESC NULLS LAST, m.submitted_at DESC, \
                               m.migration_id DESC \
                      LIMIT 1",
                    &[&id.as_str()],
                )
                .await?
                .first()
                .and_then(|row| row.get::<_, Option<String>>("descriptor_sha256"));
            return Err(RegistryError::SchemaNotApplied {
                descriptor_sha256: descriptor_sha256.map(str::to_owned),
                applied_sha256,
            });
        }

        let deploy_id = format!("dep_{}", uuid::Uuid::new_v4().simple());
        let row = tx
            .query_one(
                "INSERT INTO zeroship.app_deploys \
                    (id, app_id, deploy_hash, manifest_json, activated_at) \
                 VALUES ($1, $2, $3, $4, now()) \
                 ON CONFLICT (app_id, deploy_hash) DO UPDATE SET \
                    manifest_json = EXCLUDED.manifest_json, \
                    activated_at = now() \
                 RETURNING id",
                &[&deploy_id, &id.as_str(), &deploy_hash, &manifest_json],
            )
            .await?;
        let deploy_id: String = row.get("id");
        crate::cron::workflow_schedules::reconcile_deploy_schedules(
            &tx,
            id,
            &deploy_id,
            deploy_hash,
            manifest_json,
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Fetch the raw manifest JSON for an app, if it has one. Used by
    /// the legacy `internal::get_asset` shim while the gateway still
    /// asks the control plane for asset bytes.
    /// Remove this once the gateway switches to BlobStore directly.
    pub async fn get_manifest_json(&self, id: &AppId) -> Result<Option<String>, RegistryError> {
        let conn = self.conn().await?;
        let rows = conn
            .query("SELECT manifest_json FROM zeroship.apps WHERE id = $1", &[&id.as_str()])
            .await?;
        Ok(rows.first().and_then(|r| r.get::<_, Option<String>>("manifest_json")))
    }

    /// Change the plan for an app.
    ///
    /// Race-free in ONE statement: the UPDATE only fires when the
    /// target plan EXISTS and is NOT archived, guarded by an `EXISTS` subquery in
    /// the same statement. A separate validate-then-UPDATE had a TOCTOU window —
    /// a plan archived between the check and the UPDATE would still be assigned
    /// (the FK only guards existence, and archive is an UPDATE not a delete).
    ///
    /// Translates the result: a matched+updated app row → `Ok(true)`. Zero rows
    /// is ambiguous (no such app OR the plan is unknown/archived), so we
    /// disambiguate with a follow-up read to return a clean typed error rather
    /// than a raw FK violation or a silent no-op.
    pub async fn set_plan(&self, id: &AppId, plan_id: &str) -> Result<bool, RegistryError> {
        let conn = self.conn().await?;
        let n = conn
            .execute(
                "UPDATE zeroship.apps SET plan_id = $1, updated_at = NOW() \
                 WHERE id = $2 AND deleted_at IS NULL \
                   AND EXISTS (SELECT 1 FROM zeroship.plans \
                               WHERE id = $1 AND NOT archived)",
                &[&plan_id, &id.as_str()],
            )
            .await?;
        if n > 0 {
            return Ok(true);
        }
        // Zero rows: the app doesn't exist, the app is DELETED, or the plan is
        // unknown/archived. Disambiguate so the caller gets a typed error for a
        // bad plan rather than a misleading `Ok(false)` (= "no such app").
        //
        // A deleted app counts as absent here, and the ordering matters: asking
        // only whether the row EXISTS sends a deleted app down the plan-guard
        // branch, where `validate_plan` finds nothing wrong and the fallthrough
        // blames a concurrent archive that never happened.
        let app_exists = conn
            .query(
                "SELECT 1 FROM zeroship.apps WHERE id = $1 AND deleted_at IS NULL",
                &[&id.as_str()],
            )
            .await?;
        if app_exists.is_empty() {
            return Ok(false); // genuinely no such app, or no longer one
        }
        // The app exists ⇒ the plan guard is why nothing updated. Reuse the
        // shared validator to produce the precise unknown-vs-archived message.
        Self::validate_plan(&conn, plan_id).await?;
        // validate_plan said the plan is fine yet the guarded UPDATE matched 0
        // rows — only possible under a concurrent archive between the two
        // statements. Report it as the same typed error class.
        Err(RegistryError::InvalidInput(format!(
            "plan '{plan_id}' is not assignable (archived concurrently)"
        )))
    }

    // -- Versions / Routes --------------------------------------------------

    /// Return every app's current deploy hash (used by workers to sync).
    ///
    /// Includes the per-app routing manifest inline so the worker can
    /// resolve the worker-bundle blob hash without an extra round trip.
    /// NULL `manifest_json` rows (apps that have not deployed yet) and
    /// rows whose JSON fails to parse both surface as `manifest: None`;
    /// the worker treats that as "no V8 isolate to load" and skips the
    /// app on its reconcile pass.
    ///
    /// Archived apps intentionally remain in this projection. Disappearance
    /// means database deprovisioning to the worker version poller, including
    /// CDC teardown. Archive is an app lifecycle transition, not database
    /// teardown; gateway routes and workflow selectors are the execution gates.
    pub async fn get_versions(&self) -> Result<VersionMap, RegistryError> {
        let conn = self.conn().await?;
        // LEFT JOIN the plan catalog so each app's runtime limits come from its
        // plan row rather than a hardcoded plan-name table. A
        // missing plan (NULL `runtime_limits_json`) falls back to the
        // conservative free-tier limits below, so the worker never receives
        // `(None, None, None)` for an unpriced app.
        let rows = conn
            .query(
                "SELECT a.id, a.deploy_hash, a.plan_id, a.env_version, a.manifest_json, \
                        p.runtime_limits_json, p.net_policy_limits_json \
                 FROM zeroship.apps a \
                 LEFT JOIN zeroship.plans p ON p.id = a.plan_id",
                &[],
            )
            .await?;
        // ORDER BY is a determinism device and must stay one. Verdicts compose
        // as deny-overrides on an unordered SET, so a rule's effect never
        // depends on where it sorts; the day that stops being true this clause
        // becomes security-relevant, which is the thing it must not become.
        let rule_rows = conn
            .query(
                "SELECT app_id, verdict, destination, port FROM zeroship.app_egress_rules \
                 ORDER BY app_id, kind, destination, port",
                &[],
            )
            .await?;
        let mut rules: HashMap<AppId, Vec<NetEgressEntry>> = HashMap::new();
        for row in &rule_rows {
            let app_id_raw: String = row.get("app_id");
            let Ok(app_id) = AppId::parse(&app_id_raw) else {
                tracing::error!(
                    app_id = %app_id_raw,
                    "registry: app_egress_rules row has a malformed app id; skipping"
                );
                continue;
            };
            let destination: String = row.get("destination");
            let verdict_text: String = row.get("verdict");
            let port_i32: i32 = row.get("port");
            let Ok(port) = u16::try_from(port_i32) else {
                tracing::error!(
                    app_id = %app_id.as_str(),
                    destination = %destination,
                    port = port_i32,
                    "registry: app_egress_rules row has out-of-range port; skipping"
                );
                continue;
            };
            // A verdict outside the column's CHECK is a hand-edited row. Read
            // it as REJECT rather than dropping it: a rule the control plane
            // did not write must never be able to WIDEN what an app reaches,
            // and reject is the only reading with that property.
            //
            // The SAME function the creator-facing endpoint reads with. Two
            // copies of this arm could disagree about a row, and then a creator
            // is shown one verdict while the runtime enforces the other.
            let verdict = crate::egress_rules::parse_verdict(&verdict_text);
            rules.entry(app_id).or_default().push(NetEgressEntry {
                verdict,
                destination,
                port,
            });
        }
        let mut map = HashMap::new();
        for row in &rows {
            let id_raw: String = row.get("id");
            let Ok(id) = AppId::parse(&id_raw) else {
                tracing::error!(
                    app_id = %id_raw,
                    "registry: apps row has a malformed id; skipping"
                );
                continue;
            };
            let hash: Option<String> = row.get("deploy_hash");
            let plan_id: String = row.get("plan_id");
            let env_version: i64 = row.get("env_version");
            let manifest_json: Option<String> = row.get("manifest_json");
            let runtime_limits_json: Option<serde_json::Value> = row.get("runtime_limits_json");
            let net_policy_limits_json: Option<serde_json::Value> =
                row.get("net_policy_limits_json");
            let egress = rules.remove(&id).unwrap_or_default();
            // The empty set IS the deny-by-default state, and that meaning is
            // unchanged by the reshape: no rows means `AppNetPolicy::default()`,
            // which the worker reads as `NetPolicy::Denied`. An app holding only
            // REJECT rules reaches the worker as a non-empty set that admits
            // nothing, which is the same outcome by the ordinary rule rather
            // than by this special case.
            let net_policy = if egress.is_empty() {
                AppNetPolicy::default()
            } else {
                let caps = net_policy_limits_from_catalog(net_policy_limits_json.as_ref(), &id);
                AppNetPolicy {
                    egress,
                    max_sockets: caps.max_sockets,
                    egress_ceiling_bytes: caps.egress_ceiling_bytes,
                }
            };
            let manifest = manifest_json.as_deref().and_then(|j| {
                match serde_json::from_str::<zeroship_bundle::Manifest>(j) {
                    Ok(m) => Some(m),
                    Err(e) => {
                        tracing::warn!(
                            app_id = %id.as_str(),
                            error = %e,
                            "registry: versions: manifest parse failure — emitting None"
                        );
                        None
                    }
                }
            });
            let runtime = runtime_limits_from_catalog(runtime_limits_json.as_ref(), &id);
            map.insert(id, AppVersionInfo {
                deploy_hash: hash,
                runtime,
                plan_id,
                env_version,
                manifest,
                net_policy,
            });
        }
        Ok(map)
    }

    /// Bump the env_version counter for an app — called by `EnvStore`
    /// after every var/secret mutation. Best-effort: failure is logged
    /// upstream, the mutation has already committed; worst case the
    /// worker takes one extra reconcile interval to refetch env.
    /// Build the full route table for the gateway.
    ///
    /// The `manifest_json` column carries the per-app routing manifest
    /// (dispatch rules + asset maps) emitted by the build adapter. NULL
    /// or invalid → synthesize [`zeroship_bundle::Manifest::passthrough`] so dispatch is
    /// always defined (legacy fallback path was removed).
    pub async fn get_routes(&self) -> Result<RouteMap, RegistryError> {
        let conn = self.conn().await?;
        // LEFT JOIN zeroship.app_oauth_clients: a provisioned app yields
        // Some(oauth_client_id)/Some(sector_identifier); an un-provisioned app
        // (no extension row) yields NULL ⇒ None. The join key is the app id.
        // LEFT JOIN zeroship.app_spend_state: an app with a spend row
        // carries its current `state` TEXT; an app without one yields NULL ⇒
        // default `SpendState::Allow` (the common, unrestricted case). The
        // gateway gates dispatch on this pulled value: spend state is PULLed on
        // the RouteEntry, not pushed.
        //
        // LEFT JOIN zeroship.organization_billing_status: payment/account state
        // is ORGANIZATION-keyed (one row per organization), and `apps` now
        // carries `organization_id`, so this is a plain equi-join on the row we
        // already have. An app whose organization has no status row
        // (free/cardless, the common case) yields NULL ⇒ default
        // `AccountState::Active`. The gateway gates dispatch on this pulled
        // value as an OUTER AND with spend (Suspended → 402 before spend is
        // even consulted).
        //
        // THE LATERAL THAT USED TO BE HERE IS GONE, AND WITH IT A WHOLE CLASS OF
        // BUG. While the billing subject was a human, this had to walk
        // `apps -> projects -> organization_members(role='owner')`, which FANS
        // OUT once per owner - so it needed a `LIMIT 1` ordered
        // most-restrictive-first (suspended > past_due > active > none) purely
        // so that seating a second owner with a good card could not relax
        // enforcement for everyone. An organization has exactly one billing
        // status row, so there is no fan-out to collapse and no ordering to get
        // wrong. `billing_read::billing_status` reads through the identical
        // join, which is what keeps the edge and the console from disagreeing
        // about one app.
        let rows = conn
            .query(
                "SELECT a.id, a.name, a.plan_id, a.deploy_hash, \
                        a.manifest_json, c.client_id AS oauth_client_id, \
                        c.sector_identifier, s.state AS spend_state, \
                        obs.state AS account_state \
                 FROM zeroship.apps a \
                 LEFT JOIN zeroship.app_oauth_clients c ON c.app_id = a.id \
                 LEFT JOIN zeroship.app_spend_state s ON s.app_id = a.id \
                 LEFT JOIN zeroship.organization_billing_status obs \
                        ON obs.organization_id = a.organization_id \
                 WHERE a.archived_at IS NULL",
                &[],
            )
            .await?;
        let mut map = HashMap::new();
        for row in &rows {
            let id_raw: String = row.get("id");
            let Ok(id) = AppId::parse(&id_raw) else {
                tracing::error!(
                    app_id = %id_raw,
                    "registry: apps row has a malformed id; skipping"
                );
                continue;
            };
            let manifest_json: Option<String> = row.get("manifest_json");
            let manifest = manifest_json
                .as_deref()
                .and_then(|j| match serde_json::from_str::<zeroship_bundle::Manifest>(j) {
                    Ok(m) => match m.validate() {
                        Ok(()) => Some(m),
                        Err(e) => {
                            // `error`, not `warn`: the passthrough fallback below
                            // installs a single `*` resource with `auth: Anon,
                            // publicly_accessible: true`, so an app that was
                            // auth-gated is now serving everything to anonymous
                            // callers. That is a security downgrade, not a
                            // degraded-service notice.
                            tracing::error!(
                                app_id = %id.as_str(),
                                error = %e,
                                "registry: invalid manifest — falling back to passthrough, \
                                 which serves EVERY route as anonymous-public"
                            );
                            None
                        }
                    },
                    Err(e) => {
                        tracing::error!(
                            app_id = %id.as_str(),
                            error = %e,
                            "registry: manifest parse failure — falling back to passthrough, \
                             which serves EVERY route as anonymous-public"
                        );
                        None
                    }
                })
                .unwrap_or_else(zeroship_bundle::Manifest::passthrough);
            map.insert(
                id,
                RouteEntry {
                    name: row.get("name"),
                    plan_id: row.get("plan_id"),
                    deploy_hash: row.get("deploy_hash"),
                    manifest,
                    // OAuth identity fields, populated by the LEFT JOIN on
                    // `zeroship.app_oauth_clients` above. A provisioned app yields
                    // `Some(client_id)`/`Some(sector_identifier)`; an
                    // un-provisioned app has no extension row, so the join
                    // produces NULL ⇒ `None`. The gateway's
                    // `CompiledRoute`/browser-auth/Bearer arm consume these.
                    oauth_client_id: row.get("oauth_client_id"),
                    sector_identifier: row.get("sector_identifier"),
                    // Spend state from the LEFT-JOINed app_spend_state.
                    // NULL (no spend row) ⇒ Allow; an unrecognised TEXT value
                    // fails closed to Block (defensive — should never happen,
                    // the engine only writes the four known states).
                    spend_state: row
                        .get::<_, Option<String>>("spend_state")
                        .as_deref()
                        .map_or(zeroship_core::types::SpendState::Allow, crate::spend::parse_spend_state),
                    // Creator account state from the LEFT-JOINed
                    // organization_billing_status (via the owner membership). NULL
                    // (no status row) ⇒ Active; an unrecognised TEXT value fails
                    // closed to Suspended (defensive — the writer only ever
                    // persists the three known states, guarded by a CHECK).
                    account_state: row
                        .get::<_, Option<String>>("account_state")
                        .as_deref()
                        .map_or(
                            zeroship_core::types::AccountState::Active,
                            crate::account_status::parse_account_state,
                        ),
                },
            );
        }
        Ok(map)
    }

    /// Build the complete gateway pull payload. Lifecycle state is fetched on
    /// every route cycle so stateless edge credentials are invalidated without
    /// a per-request database lookup.
    pub async fn get_gateway_snapshot(&self) -> Result<GatewaySnapshot, RegistryError> {
        let routes = self.get_routes().await?;
        let conn = self.conn().await?;
        let rows = conn
            .query(
                "SELECT u.id, u.disabled_at IS NOT NULL AS disabled, \
                        u.anonymized_at IS NOT NULL AS anonymized, \
                        u.deletion_requested_at IS NOT NULL AS deletion_requested, \
                        u.deletion_scheduled_for IS NOT NULL AS deletion_scheduled, \
                        aui.pairwise_sub \
                 FROM zeroship.users u \
                 LEFT JOIN zeroship.app_user_identities aui \
                   ON aui.global_user_id = u.id \
                 WHERE u.disabled_at IS NOT NULL \
                    OR u.anonymized_at IS NOT NULL \
                    OR u.deletion_requested_at IS NOT NULL \
                    OR u.deletion_scheduled_for IS NOT NULL \
                 ORDER BY u.id, aui.pairwise_sub",
                &[],
            )
            .await?;
        let mut by_user = HashMap::<UserId, GatewayPrincipalLifecycle>::new();
        for row in &rows {
            let user_id_raw: String = row.get("id");
            let Ok(user_id) = UserId::parse(&user_id_raw) else {
                tracing::error!(
                    user_id = %user_id_raw,
                    "registry: gateway snapshot: users row has a malformed id; skipping"
                );
                continue;
            };
            let lifecycle = by_user.entry(user_id.clone()).or_insert_with(|| {
                GatewayPrincipalLifecycle {
                    user_id,
                    disabled: row.get("disabled"),
                    anonymized: row.get("anonymized"),
                    deletion_requested: row.get("deletion_requested"),
                    deletion_scheduled: row.get("deletion_scheduled"),
                    pairwise_subjects: Vec::new(),
                }
            });
            if let Some(subject) = row.get::<_, Option<String>>("pairwise_sub") {
                lifecycle.pairwise_subjects.push(subject);
            }
        }
        let mut principal_lifecycle: Vec<_> = by_user.into_values().collect();
        principal_lifecycle.sort_by_key(|lifecycle| lifecycle.user_id.clone());
        let family_revocations = conn
            .query(
                "SELECT client_id, sub, \
                        CEIL(EXTRACT(EPOCH FROM revoked_after))::bigint AS revoked_after \
                 FROM zeroship.token_revocations \
                 WHERE revoked_after >= NOW() - INTERVAL '24 hours' \
                 ORDER BY client_id, sub",
                &[],
            )
            .await?
            .iter()
            .map(|row| GatewayFamilyRevocation {
                client_id: row.get("client_id"),
                subject: row.get("sub"),
                revoked_after: row.get("revoked_after"),
            })
            .collect();
        Ok(GatewaySnapshot {
            routes,
            principal_lifecycle,
            family_revocations,
        })
    }

    // -- Usage / Metering ---------------------------------------------------
    //
    // Usage snapshot writes + reads moved to `crate::metering::Metering`,
    // backed by `zeroship.usage_aggregates`. The old raw-additive
    // `record_usage`/`get_usage` over `zeroship.app_usage` are gone —
    // pre-launch, no deprecated aliases.
}

/// Derive an app's [`AppRuntimeLimits`] from its plan-catalog
/// `runtime_limits_json` (the LEFT-JOINed column in [`Registry::get_versions`]).
/// A NULL column (no plan row) or a parse failure falls back to the
/// conservative free-tier limits. Limits come from the catalog, not a
/// hardcoded plan-name table.
fn runtime_limits_from_catalog(
    json: Option<&serde_json::Value>,
    app_id: &AppId,
) -> AppRuntimeLimits {
    match json {
        Some(j) => match serde_json::from_value::<AppRuntimeLimits>(j.clone()) {
            Ok(limits) => limits,
            Err(e) => {
                tracing::warn!(
                    app_id = %app_id.as_str(),
                    error = %e,
                    "registry: plan runtime_limits_json parse failure — using free-tier fallback"
                );
                FREE_TIER_RUNTIME_LIMITS
            }
        },
        None => FREE_TIER_RUNTIME_LIMITS,
    }
}

/// Derive creator raw-TCP caps from the plan catalog. Destinations come from
/// `app_egress_rules`; this only controls socket count and per-dispatch egress
/// ceiling. Missing/corrupt catalog values fall back to the free-tier caps.
fn net_policy_limits_from_catalog(
    json: Option<&serde_json::Value>,
    app_id: &AppId,
) -> AppNetPolicyLimits {
    match json {
        Some(j) => match serde_json::from_value::<AppNetPolicyLimits>(j.clone()) {
            Ok(limits) => limits,
            Err(e) => {
                tracing::warn!(
                    app_id = %app_id.as_str(),
                    error = %e,
                    "registry: plan net_policy_limits_json parse failure — using free-tier fallback"
                );
                FREE_TIER_NET_POLICY_LIMITS
            }
        },
        None => FREE_TIER_NET_POLICY_LIMITS,
    }
}

// ---------------------------------------------------------------------------
// Name validation
// ---------------------------------------------------------------------------

/// Every rule a NEW app name must satisfy, in the order a creator should read
/// them: is it a legal name, and is it a name the platform can hand out.
///
/// Extracted from [`Registry::create_app`] so the rules can be ruled on without
/// a database. `create_app` calls it as its first statement and is the only
/// caller; `crates/zeroship-control/tests/reserved_app_names_test.rs` is what
/// binds the two together against the real route.
///
/// # Errors
///
/// [`RegistryError::InvalidInput`] for a malformed name and
/// [`RegistryError::ReservedName`] for a well-formed one the platform keeps.
/// The two are separate on purpose - see the variant docs.
pub fn validate_app_name(name: &str) -> Result<(), RegistryError> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(RegistryError::InvalidInput(
            "name must be 1-64 alphanumeric/hyphen/underscore".into(),
        ));
    }

    // The name IS the app's hostname label, so a name the platform edge
    // already routes elsewhere cannot be handed to a creator. Refused here,
    // at the only point a name is ever claimed, rather than at dispatch —
    // by the time a request arrives the name is already taken.
    if crate::reserved_names::is_reserved_app_name(name) {
        return Err(RegistryError::ReservedName(
            crate::reserved_names::reserved_name_message(name),
        ));
    }

    if name_reads_as_an_app_id(name) {
        return Err(RegistryError::ReservedName(format!(
            "'{name}' starts with '{APP_ID_PREFIX}', which is reserved for app IDs. \
             An app's name is a routing label and an app's id is its identity; a \
             name in the id namespace makes the two indistinguishable wherever one \
             string carries both. Choose a different app name."
        )));
    }

    Ok(())
}

/// The prefix an app id is spelled with, including its separator.
///
/// A literal rather than a `format!` of [`AppId::PREFIX`] so it can be a
/// `const`, and PINNED to that constant by
/// `name_validation_tests::the_reserved_prefix_is_the_one_the_id_type_uses` -
/// the reservation and the parse must name one namespace, and a second literal
/// nobody compares is how they would come to name two.
const APP_ID_PREFIX: &str = "app_";

/// True when `name` claims the app-id namespace.
///
/// THE WHOLE PREFIX IS RESERVED, not merely the names that parse as an id. The
/// charset rule above admits `_`, so `app_0123456789ABCDEFGHIJKL` is a legal
/// name AND a legal [`AppId`] - a single string that is both, which is what
/// makes any name-or-id discriminator unwritable. Reserving only the parsable
/// bodies would leave the boundary standing on base62 length and range checks:
/// a body one character short would be claimable, and the rule would be one no
/// creator could state and no reviewer could check.
///
/// Case-folded, for the same reason `is_reserved_app_name` folds: an app name
/// is a hostname label and hostnames are case-insensitive (RFC 4343), so
/// `App_x` and `app_x` name one origin.
fn name_reads_as_an_app_id(name: &str) -> bool {
    name.len() >= APP_ID_PREFIX.len()
        && name[..APP_ID_PREFIX.len()].eq_ignore_ascii_case(APP_ID_PREFIX)
}

#[cfg(test)]
mod name_validation_tests {
    use super::*;

    /// The string that is a legal app NAME and a legal app ID at once.
    ///
    /// `create_app`'s charset rule admits `_`, and `AppId::parse` wants the
    /// `app_` prefix plus a base62 body, so this passes both. A creator who
    /// claims it holds a name that every `--app=` reads as an identity.
    const ID_SHAPED_NAME: &str = "app_0123456789abcdefghijkl000";

    /// The premise: this really is BOTH, so the collision is a fact rather
    /// than a worry about one. If `AppId::parse` ever stopped taking it, the
    /// refusal below would be guarding nothing and this arm says so first.
    #[test]
    fn the_id_shaped_name_really_does_parse_as_an_app_id() {
        assert!(
            AppId::parse(ID_SHAPED_NAME).is_ok(),
            "{ID_SHAPED_NAME} must be a real app id, or the refusal under test \
             is defending against a string nothing would confuse"
        );
    }

    /// The refusal, and its control one variable away: the same call, the same
    /// charset, a name that does not claim the id namespace.
    #[test]
    fn a_name_in_the_app_id_namespace_is_refused() {
        let err = validate_app_name(ID_SHAPED_NAME)
            .expect_err("a name that is also an app id must not be claimable");
        assert!(
            matches!(err, RegistryError::ReservedName(_)),
            "well-formed and unavailable, like a reserved label - not malformed; \
             got {err:?}"
        );

        // The whole prefix is reserved, not only the bodies that parse.
        for name in ["app_", "app_x", "APP_something", "App_Mixed"] {
            assert!(
                matches!(
                    validate_app_name(name),
                    Err(RegistryError::ReservedName(_))
                ),
                "{name} claims the id namespace and must be refused too"
            );
        }

        for ordinary in ["app", "apps", "application", "my-app", "app-store"] {
            assert!(
                validate_app_name(ordinary).is_ok(),
                "{ordinary} does not claim the id namespace and must stay \
                 claimable - a refusal that denied these would read as correct \
                 from the refusal side alone"
            );
        }
    }

    /// The reservation and the id parse must name ONE namespace.
    #[test]
    fn the_reserved_prefix_is_the_one_the_id_type_uses() {
        assert_eq!(
            APP_ID_PREFIX,
            format!("{}_", AppId::PREFIX),
            "the refusal must reserve exactly the prefix AppId::parse reads"
        );
    }

    /// The extraction is behaviour-neutral for the rules that were already
    /// there: a reserved label and a malformed name still answer differently.
    #[test]
    fn the_earlier_rules_survive_the_extraction() {
        assert!(matches!(
            validate_app_name("console"),
            Err(RegistryError::ReservedName(_))
        ));
        assert!(matches!(
            validate_app_name("not a legal name!"),
            Err(RegistryError::InvalidInput(_))
        ));
        assert!(matches!(
            validate_app_name(""),
            Err(RegistryError::InvalidInput(_))
        ));
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a query row into an `AppRecord`.
///
/// Columns: id (TEXT, the canonical `app_<base62>` rendering), name (TEXT),
///          plan_id (TEXT), deploy_hash (TEXT | NULL), archived_at
///          (TEXT | NULL), created_at (TEXT), updated_at (TEXT).
///
/// # Errors
///
/// [`RegistryError::Database`] if the `id` column does not hold a canonical
/// app id. `AppId` has no `FromSql` impl - decoding through a fallible
/// `String` read and parse is the only route in, and it is what keeps a
/// malformed row from being mistaken for a real app.
fn row_to_record(row: &compio_postgres::Row) -> Result<AppRecord, RegistryError> {
    let id_raw: String = row.get("id");
    let id = AppId::parse(&id_raw).map_err(|e| {
        RegistryError::Database(format!("apps.id {id_raw} is not a canonical app id: {e}"))
    })?;
    Ok(AppRecord {
        id,
        name: row.get("name"),
        plan_id: row.get("plan_id"),
        deploy_hash: row.get("deploy_hash"),
        archived_at: row.get("archived_at"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}
