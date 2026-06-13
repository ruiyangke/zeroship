//! Registry — application CRUD backed by PostgreSQL (compio-postgres).

use std::collections::HashMap;

use compio_postgres::error::SqlState;
use compio_postgres::{Client, NoTls};
use uuid::Uuid;
use zeroship_core::auth::hash_api_key;
use zeroship_core::types::{
    AppRecord, AppRuntimeLimits, AppVersionInfo, RouteEntry, RouteMap, VersionMap,
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
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(s) => write!(f, "not found: {s}"),
            Self::AlreadyExists(s) => write!(f, "already exists: {s}"),
            Self::Database(s) => write!(f, "database: {s}"),
            Self::InvalidInput(s) => write!(f, "invalid input: {s}"),
        }
    }
}

impl From<compio_postgres::Error> for RegistryError {
    fn from(e: compio_postgres::Error) -> Self {
        if matches!(e.code(), Some(code) if code == &SqlState::UNIQUE_VIOLATION) {
            Self::AlreadyExists("resource already exists".into())
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
    /// The schema is owned by Liquibase (`db/changelog`), applied out of band
    /// before the service boots (the `migrate` compose step / `ops/db-migrate.sh`).
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

    // -- App CRUD -----------------------------------------------------------

    /// Create a new application owned by `owner_id`. Returns the created
    /// `AppRecord`.
    ///
    /// The `zeroship.apps` row and the creator's `zeroship.app_members(owner)`
    /// row are written in ONE transaction, so the principal is bound to their
    /// own app atomically — the app is never visible (or routable) without its
    /// owner membership. That owner row is what authorizes the creator for every
    /// per-app action through the `app_owner` Cedar policy; without it a
    /// default-role creator would be locked out of the app they just created
    /// (finding F3 / C1 over-restriction).
    ///
    /// Runs on a DEDICATED owned connection so the RAII `transaction()` guard
    /// owns it and an aborted txn never poisons a shared handle.
    pub async fn create_app(
        &self,
        name: &str,
        plan_id: &str,
        owner_id: &Uuid,
    ) -> Result<AppRecord, RegistryError> {
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

        let api_key = Uuid::new_v4().to_string();
        let key_hash = hash_api_key(&api_key);
        let mut conn = self.conn().await?;
        let tx = conn.transaction().await?;

        let rows = tx
            .query(
                "INSERT INTO zeroship.apps (name, plan_id, api_key, api_key_hash) \
                 VALUES ($1, $2, $3, $4) \
                 RETURNING id, name, plan_id, deploy_hash, api_key, \
                           created_at::text, updated_at::text",
                &[&name, &plan_id, &api_key, &key_hash],
            )
            .await?;
        let record = rows
            .first()
            .map(row_to_record)
            .ok_or_else(|| RegistryError::Database("insert ok but read-back failed".into()))?;

        // Bind the creating principal as the app's owner in the SAME txn.
        // `app_members.app_id` is a `uuid` column — bind the `Uuid` directly.
        tx.execute(
            "INSERT INTO zeroship.app_members (app_id, user_id, role) VALUES ($1, $2, 'owner')",
            &[&record.id, owner_id],
        )
        .await?;

        tx.commit().await?;
        Ok(record)
    }

    /// Get an app by primary key.
    pub async fn get_app(&self, id: &Uuid) -> Result<Option<AppRecord>, RegistryError> {
        let conn = self.conn().await?;
        let rows = conn
            .query(
                "SELECT id, name, plan_id, deploy_hash, api_key, created_at::text, updated_at::text \
                 FROM zeroship.apps WHERE id = $1",
                &[id],
            )
            .await?;
        Ok(rows.first().map(row_to_record))
    }

    /// Get an app by unique name.
    pub async fn get_app_by_name(&self, name: &str) -> Result<Option<AppRecord>, RegistryError> {
        let conn = self.conn().await?;
        let rows = conn
            .query(
                "SELECT id, name, plan_id, deploy_hash, api_key, created_at::text, updated_at::text \
                 FROM zeroship.apps WHERE name = $1",
                &[&name],
            )
            .await?;
        Ok(rows.first().map(row_to_record))
    }

    /// List all apps ordered by name. Fleet-wide — for platform staff
    /// (admin/readonly/support) only. Ordinary creators must use
    /// [`Registry::list_apps_for_owner`] so the list never leaks other tenants'
    /// apps.
    pub async fn list_apps(&self) -> Result<Vec<AppRecord>, RegistryError> {
        let conn = self.conn().await?;
        let rows = conn
            .query(
                "SELECT id, name, plan_id, deploy_hash, api_key, created_at::text, updated_at::text \
                 FROM zeroship.apps ORDER BY name",
                &[],
            )
            .await?;
        Ok(rows.iter().map(row_to_record).collect())
    }

    /// List the apps `owner_id` is a member of (any role), ordered by name.
    ///
    /// This is the creator-facing listing: the `/api/apps` GET grants every
    /// creator `apps:read` on the platform surface (self-service policy), but
    /// the data it returns MUST be scoped to apps the principal actually belongs
    /// to — otherwise the broadened gate becomes a fleet-wide cross-tenant read.
    pub async fn list_apps_for_owner(
        &self,
        owner_id: &Uuid,
    ) -> Result<Vec<AppRecord>, RegistryError> {
        let conn = self.conn().await?;
        let rows = conn
            .query(
                "SELECT a.id, a.name, a.plan_id, a.deploy_hash, a.api_key, \
                        a.created_at::text, a.updated_at::text \
                 FROM zeroship.apps a \
                 JOIN zeroship.app_members m ON m.app_id = a.id \
                 WHERE m.user_id = $1 \
                 ORDER BY a.name",
                &[owner_id],
            )
            .await?;
        Ok(rows.iter().map(row_to_record).collect())
    }

    /// Delete an app by id. Returns true if the app row was deleted.
    ///
    /// ATOMIC: deletes the `zeroship.apps` row AND the per-app
    /// `zeroship.oauth_clients` row (`client_id = client_id_for_app(id)`) in ONE
    /// transaction, so the real FK chain cascades every dependent row in the
    /// single `zeroship` schema in one shot:
    ///   - apps          → {gateway_sessions, app_members, app_session_anchors
    ///                       (by app_id), app_oauth_clients, app_usage,
    ///                       app_vars, app_secrets, …}
    ///   - oauth_clients → {oauth_grants, app_user_identities,
    ///                       app_session_anchors (by client_id)}
    ///
    /// Deleting the per-app oauth_clients row is what closes the relay arm:
    /// `app_user_identities.app_client_id` FKs into `oauth_clients(client_id)`
    /// ON DELETE CASCADE, so this single txn replaces the former best-effort
    /// alias-revoke companion UPDATE (no orphaned live aliases possible).
    ///
    /// Runs on a DEDICATED owned connection (`conn()` → fresh mutable `Client`)
    /// so the RAII `transaction()` guard owns it; an aborted txn never poisons a
    /// shared handle.
    pub async fn delete_app(&self, id: &Uuid) -> Result<bool, RegistryError> {
        let client_id = crate::app_oauth_client::client_id_for_app(id);
        let mut conn = self.conn().await?;
        let tx = conn.transaction().await?;
        let n = tx
            .execute("DELETE FROM zeroship.apps WHERE id = $1", &[id])
            .await?;
        // Delete the per-app oauth_clients row in the SAME txn. Its FK children
        // (oauth_grants, app_user_identities, app_session_anchors-by-client_id)
        // cascade. Keyed on the deterministic `client_id_for_app(id)` — the same
        // value `provision_app_oauth_client` wrote — so no extension-table read
        // is needed (and the app_oauth_clients row is already cascade-gone with
        // the apps row above; this targets the shared oauth_clients row).
        tx.execute(
            "DELETE FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await?;
        tx.commit().await?;
        Ok(n > 0)
    }

    /// Set the deploy hash (content-addressable bundle hash) for an app.
    pub async fn set_deploy_hash(&self, id: &Uuid, hash: &str) -> Result<bool, RegistryError> {
        let conn = self.conn().await?;
        let n = conn
            .execute(
                "UPDATE zeroship.apps SET deploy_hash = $1, \
                 updated_at = NOW() WHERE id = $2",
                &[&hash, id],
            )
            .await?;
        Ok(n > 0)
    }

    /// Atomic deploy commit. Sets `deploy_hash` and `manifest_json` in
    /// the same UPDATE so the gateway never observes a half-applied
    /// deploy. Used by the .zship ingest path.
    pub async fn set_deploy_with_manifest(
        &self,
        id: &Uuid,
        deploy_hash: &str,
        manifest_json: &str,
    ) -> Result<bool, RegistryError> {
        let conn = self.conn().await?;
        let n = conn
            .execute(
                "UPDATE zeroship.apps SET deploy_hash = $1, manifest_json = $2, \
                 updated_at = NOW() WHERE id = $3",
                &[&deploy_hash, &manifest_json, id],
            )
            .await?;
        Ok(n > 0)
    }

    /// Fetch the raw manifest JSON for an app, if it has one. Used by
    /// the legacy `internal::get_asset` shim while the gateway still
    /// asks the control plane for asset bytes.
    /// Remove this once the gateway switches to BlobStore directly.
    pub async fn get_manifest_json(&self, id: &Uuid) -> Result<Option<String>, RegistryError> {
        let conn = self.conn().await?;
        let rows = conn
            .query("SELECT manifest_json FROM zeroship.apps WHERE id = $1", &[id])
            .await?;
        Ok(rows.first().and_then(|r| r.get::<_, Option<String>>("manifest_json")))
    }

    /// Change the plan for an app.
    pub async fn set_plan(&self, id: &Uuid, plan_id: &str) -> Result<bool, RegistryError> {
        let conn = self.conn().await?;
        let n = conn
            .execute(
                "UPDATE zeroship.apps SET plan_id = $1, \
                 updated_at = NOW() WHERE id = $2",
                &[&plan_id, id],
            )
            .await?;
        Ok(n > 0)
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
    pub async fn get_versions(&self) -> Result<VersionMap, RegistryError> {
        let conn = self.conn().await?;
        let rows = conn
            .query(
                "SELECT id, deploy_hash, plan_id, env_version, manifest_json FROM zeroship.apps",
                &[],
            )
            .await?;
        let mut map = HashMap::new();
        for row in &rows {
            let id: Uuid = row.get("id");
            let hash: Option<String> = row.get("deploy_hash");
            let plan_id: String = row.get("plan_id");
            let env_version: i64 = row.get("env_version");
            let manifest_json: Option<String> = row.get("manifest_json");
            let manifest = manifest_json.as_deref().and_then(|j| {
                match serde_json::from_str::<zeroship_bundle::Manifest>(j) {
                    Ok(m) => Some(m),
                    Err(e) => {
                        tracing::warn!(
                            app_id = %id,
                            error = %e,
                            "registry: versions: manifest parse failure — emitting None"
                        );
                        None
                    }
                }
            });
            map.insert(id, AppVersionInfo {
                deploy_hash: hash,
                runtime: runtime_limits_for_plan(&plan_id),
                plan_id,
                env_version,
                manifest,
            });
        }
        Ok(map)
    }

    /// Bump the env_version counter for an app — called by `EnvStore`
    /// after every var/secret mutation. Best-effort: failure is logged
    /// upstream, the mutation has already committed; worst case the
    /// worker takes one extra reconcile interval to refetch env.
    pub(crate) async fn bump_env_version(&self, app_id: Uuid) -> Result<(), RegistryError> {
        let conn = self.conn().await?;
        conn.execute(
            "UPDATE zeroship.apps SET env_version = env_version + 1 WHERE id = $1",
            &[&app_id],
        )
        .await?;
        Ok(())
    }

    /// Build the full route table for the gateway.
    ///
    /// The `manifest_json` column carries the per-app routing manifest
    /// (dispatch rules + asset maps) emitted by the build adapter. NULL
    /// or invalid → synthesize [`Manifest::passthrough`] so dispatch is
    /// always defined (legacy fallback path was removed).
    pub async fn get_routes(&self) -> Result<RouteMap, RegistryError> {
        let conn = self.conn().await?;
        // LEFT JOIN control.app_oauth_clients (§1.5): a provisioned app yields
        // Some(oauth_client_id)/Some(sector_identifier); an un-provisioned app
        // (no extension row) yields NULL ⇒ None. The join key is the app id.
        let rows = conn
            .query(
                "SELECT a.id, a.name, a.plan_id, a.api_key_hash, a.deploy_hash, \
                        a.manifest_json, c.client_id AS oauth_client_id, \
                        c.sector_identifier \
                 FROM zeroship.apps a \
                 LEFT JOIN zeroship.app_oauth_clients c ON c.app_id = a.id",
                &[],
            )
            .await?;
        let mut map = HashMap::new();
        for row in &rows {
            let id: Uuid = row.get("id");
            let manifest_json: Option<String> = row.get("manifest_json");
            let manifest = manifest_json
                .as_deref()
                .and_then(|j| match serde_json::from_str::<zeroship_bundle::Manifest>(j) {
                    Ok(m) => match m.validate() {
                        Ok(()) => Some(m),
                        Err(e) => {
                            tracing::warn!(app_id = %id, error = %e, "registry: invalid manifest — using passthrough");
                            None
                        }
                    },
                    Err(e) => {
                        tracing::warn!(app_id = %id, error = %e, "registry: manifest parse failure — using passthrough");
                        None
                    }
                })
                .unwrap_or_else(zeroship_bundle::Manifest::passthrough);
            map.insert(
                id,
                RouteEntry {
                    name: row.get("name"),
                    plan_id: row.get("plan_id"),
                    api_key_hash: row.get("api_key_hash"),
                    deploy_hash: row.get("deploy_hash"),
                    manifest,
                    // OAuth identity fields (§1.5), populated by the LEFT JOIN
                    // on `control.app_oauth_clients` above. A provisioned app
                    // (Slice 1d created its per-app client) yields
                    // `Some(client_id)`/`Some(sector_identifier)`; an
                    // un-provisioned app has no extension row, so the join
                    // produces NULL ⇒ `None`. The gateway's
                    // `CompiledRoute`/browser-auth/Bearer arm consume these.
                    oauth_client_id: row.get("oauth_client_id"),
                    sector_identifier: row.get("sector_identifier"),
                },
            );
        }
        Ok(map)
    }

    // -- Usage / Metering ---------------------------------------------------
    //
    // Usage ingest + reads moved to `crate::metering::Metering` (the
    // idempotent, period-aggregated pipeline backed by
    // `zeroship.usage_aggregates` + `zeroship.usage_reports_seen`). The old
    // raw-additive `record_usage`/`get_usage` over `zeroship.app_usage`
    // (no idempotency, no period, no custom metrics) are gone — pre-launch,
    // no deprecated aliases.
}

fn runtime_limits_for_plan(plan_id: &str) -> AppRuntimeLimits {
    match plan_id {
        "free" => AppRuntimeLimits {
            cpu_limit_ms: Some(50),
            wall_timeout_ms: Some(5_000),
            heap_limit_mb: Some(64),
        },
        "pro" => AppRuntimeLimits {
            cpu_limit_ms: Some(30_000),
            wall_timeout_ms: Some(30_000),
            heap_limit_mb: Some(256),
        },
        "unlimited" | "enterprise" => AppRuntimeLimits {
            cpu_limit_ms: None,
            wall_timeout_ms: None,
            heap_limit_mb: None, // platform default (128 MB)
        },
        _ => AppRuntimeLimits {
            cpu_limit_ms: Some(50),
            wall_timeout_ms: Some(5_000),
            heap_limit_mb: Some(64),
        },
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a query row into an `AppRecord`.
///
/// Columns: id (UUID), name (TEXT), plan_id (UUID), deploy_hash (TEXT | NULL),
///          api_key (TEXT), created_at (BIGINT), updated_at (BIGINT).
fn row_to_record(row: &compio_postgres::Row) -> AppRecord {
    AppRecord {
        id: row.get("id"),
        name: row.get("name"),
        plan_id: row.get("plan_id"),
        deploy_hash: row.get("deploy_hash"),
        api_key: row.get("api_key"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}
