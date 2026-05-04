//! Registry — application CRUD backed by PostgreSQL (compio-postgres).

use std::collections::HashMap;

use zeroship_core::auth::hash_api_key;
use zeroship_core::types::{AppRecord, AppRuntimeLimits, AppVersionInfo, RouteEntry, RouteMap, VersionMap};
use compio_postgres::{Client, NoTls};
use uuid::Uuid;

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
        let msg = e.to_string();
        // compio-postgres's Error is opaque — peek at the full chain (the
        // underlying DbError's SQLSTATE or message) via the Display/source
        // fallback. UNIQUE violations flow through the chain as "23505 /
        // duplicate key value violates unique constraint".
        let full = format!("{msg}: {}", source_chain(&e));
        if full.contains("duplicate key") || full.contains("unique") || full.contains("23505") {
            Self::AlreadyExists(full)
        } else {
            Self::Database(full)
        }
    }
}

/// Walk an error's `source()` chain and concatenate the messages. Useful for
/// reaching through the opaque `compio_postgres::Error` wrapper to the
/// `DbError` inside.
fn source_chain(err: &dyn std::error::Error) -> String {
    let mut out = String::new();
    let mut cur: Option<&dyn std::error::Error> = err.source();
    while let Some(e) = cur {
        if !out.is_empty() {
            out.push_str(" | ");
        }
        out.push_str(&e.to_string());
        cur = e.source();
    }
    out
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
            eprintln!("control: pg connection error: {e}");
        }
    })
    .detach();
    Ok(client)
}

impl Registry {
    /// Connect to the database, run schema migrations, and return a `Registry`.
    pub async fn new(db_url: &str) -> Result<Self, String> {
        let conn = open_conn(db_url).await.map_err(|e| e.to_string())?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS apps (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                name TEXT NOT NULL UNIQUE,
                plan_id TEXT NOT NULL DEFAULT 'free',
                deploy_hash TEXT,
                api_key TEXT NOT NULL,
                api_key_hash TEXT NOT NULL DEFAULT '',
                env_version BIGINT NOT NULL DEFAULT 0,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )",
            &[],
        )
        .await
        .map_err(|e| format!("migration: {e}"))?;

        // Backfill column for deployments that predate env_version.
        conn.execute(
            "ALTER TABLE apps ADD COLUMN IF NOT EXISTS env_version BIGINT NOT NULL DEFAULT 0",
            &[],
        )
        .await
        .map_err(|e| format!("migration: {e}"))?;

        // Per-app routing manifest (dispatch rules + asset maps).
        // NULL for apps deployed before the manifest era — gateway
        // falls back to its legacy dispatch in that case.
        conn.execute(
            "ALTER TABLE apps ADD COLUMN IF NOT EXISTS manifest_json TEXT",
            &[],
        )
        .await
        .map_err(|e| format!("migration: {e}"))?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS usage (
                app_id UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
                resource TEXT NOT NULL,
                value BIGINT NOT NULL DEFAULT 0,
                PRIMARY KEY (app_id, resource)
            )",
            &[],
        )
        .await
        .map_err(|e| format!("migration: {e}"))?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS usage_history (
                app_id UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
                period TEXT NOT NULL,
                counters JSONB NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )",
            &[],
        )
        .await
        .map_err(|e| format!("migration: {e}"))?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_usage_history_app ON usage_history(app_id, period)",
            &[],
        )
        .await
        .map_err(|e| format!("migration: {e}"))?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS app_vars (
                app_id UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
                key_name TEXT NOT NULL,
                value TEXT NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                PRIMARY KEY (app_id, key_name)
            )",
            &[],
        )
        .await
        .map_err(|e| format!("migration: {e}"))?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS app_secrets (
                app_id UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
                key_name TEXT NOT NULL,
                ciphertext BYTEA NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                PRIMARY KEY (app_id, key_name)
            )",
            &[],
        )
        .await
        .map_err(|e| format!("migration: {e}"))?;

        // Per-app opt-in list: secret names the creator has explicitly
        // allowed to surface in `process.env` for libraries (e.g.
        // LangChain) that defensively read `process.env.OPENAI_API_KEY`.
        // Empty by default — secrets stay out of `process.env` unless
        // listed here. One row per (app, key) so unique-constraints on
        // (app_id, key_name) prevent duplicates and ON DELETE CASCADE
        // cleans up when an app is removed. Sticky across deploys —
        // it's app-config, not deploy-config.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS app_env_expose (
                app_id UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
                key_name TEXT NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                PRIMARY KEY (app_id, key_name)
            )",
            &[],
        )
        .await
        .map_err(|e| format!("migration: {e}"))?;

        // Creator→Stripe-account link. One account per creator. The
        // creator_id here can be any UUID the platform wants to use as
        // its stable creator identifier (today that's auth_users.id).
        // `unlinked_at` is non-null when the creator has soft-deleted
        // the link — payouts (which FK on creator_id) survive so the
        // financial ledger stays intact.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS creator_accounts (
                creator_id UUID PRIMARY KEY,
                stripe_account_id TEXT NOT NULL,
                onboarded_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                unlinked_at TIMESTAMPTZ
            )",
            &[],
        )
        .await
        .map_err(|e| format!("migration: {e}"))?;
        // Backfill column for existing deployments.
        conn.execute(
            "ALTER TABLE creator_accounts ADD COLUMN IF NOT EXISTS unlinked_at TIMESTAMPTZ",
            &[],
        )
        .await
        .map_err(|e| format!("migration: {e}"))?;

        // Append-only history of link transitions. One row per
        // link_account (with unlinked_at populated by the next unlink
        // OR the next relink). Lets ops audit "did the creator's
        // Stripe account ever change."
        conn.execute(
            "CREATE TABLE IF NOT EXISTS creator_account_history (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                creator_id UUID NOT NULL,
                stripe_account_id TEXT NOT NULL,
                linked_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                unlinked_at TIMESTAMPTZ
            )",
            &[],
        )
        .await
        .map_err(|e| format!("migration: {e}"))?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_creator_account_history_creator
             ON creator_account_history(creator_id, linked_at DESC)",
            &[],
        )
        .await
        .map_err(|e| format!("migration: {e}"))?;

        // Ledger of revenue events. Keyed by Stripe's evt_xxx to keep
        // webhook delivery idempotent. `payload_hash` lets us detect
        // tampering: if the same event_id arrives with different body
        // bytes (shouldn't happen from Stripe; possible with a
        // compromised upstream or an attacker who reached the
        // webhook endpoint), we record the mismatch rather than
        // silently accepting the first value.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS payouts (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                creator_id UUID NOT NULL REFERENCES creator_accounts(creator_id) ON DELETE RESTRICT,
                event_id TEXT NOT NULL UNIQUE,
                event_type TEXT NOT NULL,
                gross_amount BIGINT NOT NULL,
                platform_fee BIGINT NOT NULL,
                net_amount BIGINT NOT NULL,
                currency TEXT NOT NULL,
                occurred_at TIMESTAMPTZ NOT NULL,
                payload_hash BYTEA,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )",
            &[],
        )
        .await
        .map_err(|e| format!("migration: {e}"))?;

        // Add column for existing deployments that predate the hash.
        conn.execute(
            "ALTER TABLE payouts ADD COLUMN IF NOT EXISTS payload_hash BYTEA",
            &[],
        )
        .await
        .map_err(|e| format!("migration: {e}"))?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_payouts_creator_time ON payouts(creator_id, occurred_at DESC)",
            &[],
        )
        .await
        .map_err(|e| format!("migration: {e}"))?;

        // Append-only audit log of secret/var/stripe mutations. Lets ops
        // answer "when did sk_live_xxx leak / who changed STRIPE_KEY."
        // `app_id` is nullable for events that don't scope to an app
        // (e.g., creator_account changes which key on creator_id).
        conn.execute(
            "CREATE TABLE IF NOT EXISTS app_audit (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                app_id UUID,
                creator_id UUID,
                actor TEXT NOT NULL,
                action TEXT NOT NULL,
                resource TEXT,
                source_ip TEXT,
                at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )",
            &[],
        )
        .await
        .map_err(|e| format!("migration: {e}"))?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_app_audit_app_at ON app_audit(app_id, at DESC)",
            &[],
        )
        .await
        .map_err(|e| format!("migration: {e}"))?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_app_audit_creator_at ON app_audit(creator_id, at DESC)",
            &[],
        )
        .await
        .map_err(|e| format!("migration: {e}"))?;

        // Dropping `conn` causes the driver task to send Terminate and exit.
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

    /// Create a new application. Returns the created `AppRecord`.
    pub async fn create_app(
        &self,
        name: &str,
        plan_id: &str,
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
        let conn = self.conn().await?;

        conn.execute(
            "INSERT INTO apps (name, plan_id, api_key, api_key_hash) VALUES ($1, $2, $3, $4)",
            &[&name, &plan_id, &api_key, &key_hash],
        )
        .await?;

        let rows = conn
            .query(
                "SELECT id, name, plan_id, deploy_hash, api_key, created_at::text, updated_at::text \
                 FROM apps WHERE name = $1",
                &[&name],
            )
            .await?;

        rows.first()
            .map(row_to_record)
            .ok_or_else(|| RegistryError::Database("insert ok but read-back failed".into()))
    }

    /// Get an app by primary key.
    pub async fn get_app(&self, id: &Uuid) -> Result<Option<AppRecord>, RegistryError> {
        let conn = self.conn().await?;
        let rows = conn
            .query(
                "SELECT id, name, plan_id, deploy_hash, api_key, created_at::text, updated_at::text \
                 FROM apps WHERE id = $1",
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
                 FROM apps WHERE name = $1",
                &[&name],
            )
            .await?;
        Ok(rows.first().map(row_to_record))
    }

    /// List all apps ordered by name.
    pub async fn list_apps(&self) -> Result<Vec<AppRecord>, RegistryError> {
        let conn = self.conn().await?;
        let rows = conn
            .query(
                "SELECT id, name, plan_id, deploy_hash, api_key, created_at::text, updated_at::text \
                 FROM apps ORDER BY name",
                &[],
            )
            .await?;
        Ok(rows.iter().map(row_to_record).collect())
    }

    /// Delete an app by id. Returns true if a row was deleted.
    pub async fn delete_app(&self, id: &Uuid) -> Result<bool, RegistryError> {
        let conn = self.conn().await?;
        let n = conn
            .execute("DELETE FROM apps WHERE id = $1", &[id])
            .await?;
        Ok(n > 0)
    }

    /// Set the deploy hash (content-addressable bundle hash) for an app.
    pub async fn set_deploy_hash(&self, id: &Uuid, hash: &str) -> Result<bool, RegistryError> {
        let conn = self.conn().await?;
        let n = conn
            .execute(
                "UPDATE apps SET deploy_hash = $1, \
                 updated_at = NOW() WHERE id = $2",
                &[&hash, id],
            )
            .await?;
        Ok(n > 0)
    }

    /// Atomic deploy commit. Sets `deploy_hash` and `manifest_json` in
    /// the same UPDATE so the gateway never observes a half-applied
    /// deploy. Used by the .zsapp ingest path.
    pub async fn set_deploy_with_manifest(
        &self,
        id: &Uuid,
        deploy_hash: &str,
        manifest_json: &str,
    ) -> Result<bool, RegistryError> {
        let conn = self.conn().await?;
        let n = conn
            .execute(
                "UPDATE apps SET deploy_hash = $1, manifest_json = $2, \
                 updated_at = NOW() WHERE id = $3",
                &[&deploy_hash, &manifest_json, id],
            )
            .await?;
        Ok(n > 0)
    }

    /// Fetch the raw manifest JSON for an app, if it has one. Used by
    /// the legacy `internal::get_asset` shim while the gateway still
    /// asks the control plane for asset bytes.
    /// TODO(phase 4): remove once the gateway switches to BlobStore directly.
    pub async fn get_manifest_json(&self, id: &Uuid) -> Result<Option<String>, RegistryError> {
        let conn = self.conn().await?;
        let rows = conn
            .query("SELECT manifest_json FROM apps WHERE id = $1", &[id])
            .await?;
        Ok(rows.first().and_then(|r| r.get::<_, Option<String>>("manifest_json")))
    }

    /// Change the plan for an app.
    pub async fn set_plan(&self, id: &Uuid, plan_id: &str) -> Result<bool, RegistryError> {
        let conn = self.conn().await?;
        let n = conn
            .execute(
                "UPDATE apps SET plan_id = $1, \
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
                "SELECT id, deploy_hash, plan_id, env_version, manifest_json FROM apps",
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
                        eprintln!(
                            "[registry] versions: manifest parse failure for {id}: {e} — emitting None"
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
            "UPDATE apps SET env_version = env_version + 1 WHERE id = $1",
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
        let rows = conn
            .query(
                "SELECT id, name, plan_id, api_key_hash, deploy_hash, manifest_json \
                 FROM apps",
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
                            eprintln!("[registry] invalid manifest for {id}: {e} — using passthrough");
                            None
                        }
                    },
                    Err(e) => {
                        eprintln!("[registry] manifest parse failure for {id}: {e} — using passthrough");
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
                },
            );
        }
        Ok(map)
    }

    // -- Usage / Metering ---------------------------------------------------

    /// Increment a usage counter for an app (upsert).
    pub async fn record_usage(
        &self,
        app_id: &Uuid,
        resource: &str,
        delta: i64,
    ) -> Result<(), RegistryError> {
        let conn = self.conn().await?;
        conn.execute(
            "INSERT INTO usage (app_id, resource, value) VALUES ($1, $2, $3) \
             ON CONFLICT (app_id, resource) DO UPDATE SET value = usage.value + $3",
            &[app_id, &resource, &delta],
        )
        .await?;
        Ok(())
    }

    /// Get all usage counters for an app.
    pub async fn get_usage(
        &self,
        app_id: &Uuid,
    ) -> Result<HashMap<String, i64>, RegistryError> {
        let conn = self.conn().await?;
        let rows = conn
            .query(
                "SELECT resource, value FROM usage WHERE app_id = $1",
                &[app_id],
            )
            .await?;
        let mut map = HashMap::new();
        for row in &rows {
            map.insert(row.get::<_, String>("resource"), row.get::<_, i64>("value"));
        }
        Ok(map)
    }
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
