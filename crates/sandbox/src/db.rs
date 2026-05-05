//! Pg-backed non-secret state for the sandbox controller (Phase 0).
//!
//! See `docs/proposals/sandbox-pg-state.md` (Draft v8) for the
//! design. Phase 0 lands the schema, the migration runner, and the
//! `Database` handle. Live integration with the backends ships in
//! Phase 1; this module's surface in Phase 0 is deliberately small:
//!
//! - [`Database::from_env`] parses env, opens the pool, validates HA
//!   env vars (R-NN: `lease_ttl >= 4 * heartbeat`).
//! - [`Database::ensure_schema_at_version`] is the boot gate — the
//!   designated migrator (`SANDBOX_PG_RUN_MIGRATIONS=1`) applies
//!   pending migrations forward-only; everyone else polls
//!   `MAX(version)` until the schema reaches `target`.
//! - [`Database::run_pending_migrations`] is the migrator-side
//!   forward-only apply loop, race-tolerant against simultaneous
//!   migrators via the PRIMARY KEY on `schema_migrations.version`
//!   (§ 7.1 fallback).
//! - [`Database::ping`] / [`Database::current_schema_version`] are
//!   for tests and `/readyz`.
//!
//! ## What this module deliberately does NOT do (Phase 0)
//!
//! - No call sites are wired. `insert_sandbox`, `record_event`, etc.
//!   ship in Phase 1.
//! - No worker queue. The `flume` dep is declared in `Cargo.toml`
//!   for Phase 1's § 14.14 worker pool.
//! - No advisory locks. ANYWHERE. Concurrency is enforced by the
//!   designated-migrator pattern + UNIQUE-constraint race-tolerance
//!   on `sandbox.schema_migrations.version` (D-4 / § 7.1).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use compio_postgres::{Config, NoTls, Pool, PoolConfig};
use uuid::Uuid;

// ────────────────────────────────────────────────────────────────────
// Embedded migrations
// ────────────────────────────────────────────────────────────────────
//
// Migration files are read at compile time via `include_str!`. Each
// file is a single transactional migration (the runner wraps it in
// BEGIN/COMMIT) plus an idempotent guard around every CREATE so a
// loser of a two-migrator race can replay safely (D-4 / § 7.1).

const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    description: "initial schema (hosts, sandboxes, shares, events, deleted_sandboxes)",
    sql: include_str!("../migrations/0001_initial.sql"),
}];

/// The latest migration version this binary was built against. Boot
/// path passes this as `target_version` to
/// [`Database::ensure_schema_at_version`]; non-migrator controllers
/// poll until the schema reaches at least this version.
pub const LATEST_MIGRATION_VERSION: i64 = 1;

#[derive(Debug, Clone, Copy)]
struct Migration {
    version: i64,
    description: &'static str,
    sql: &'static str,
}

impl Migration {
    /// SHA-256 of the SQL body, hex-encoded. Recorded in
    /// `sandbox.schema_migrations.sha256` so operators can detect a
    /// migration whose source-file content drifted from what was
    /// applied historically.
    fn sha256_hex(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(self.sql.as_bytes());
        hex::encode(h.finalize())
    }
}

// ────────────────────────────────────────────────────────────────────
// Public configuration / errors
// ────────────────────────────────────────────────────────────────────

/// Connection + behavior config for [`Database`]. Built by
/// [`Database::from_env`] from the env vars enumerated in § 5.6 of
/// the design.
#[derive(Debug, Clone)]
pub struct DbConfig {
    /// Primary DSN as the `sandbox_app` role. Must start with
    /// `postgres://` or `postgresql://`. Phase 0 only verifies the
    /// scheme; production hardening (host allow-list,
    /// `sslmode=verify-full`) lands with Phase 1's connection
    /// security review.
    pub dsn: String,
    /// Stable controller identity (`hst_<base62>`-derived UUID).
    /// Generated once and persisted at `<state_dir>/host_id` so the
    /// identity survives restarts; an operator who wants a fresh
    /// identity deletes the file. (§ 10.1)
    pub host_id: Uuid,
    /// `=1` from `SANDBOX_PG_RUN_MIGRATIONS`. Tags this process as
    /// the deployment's designated migrator (D-4 / § 7.1).
    pub run_migrations: bool,
    /// Maximum seconds a non-migrator waits for the schema to reach
    /// `target_version`. From `SANDBOX_PG_BOOT_TIMEOUT_SECS`,
    /// default 60 (per § 5.6 default 300; Phase 0 ships a tighter
    /// 60s default for fail-fast tests + dev — operators raise it
    /// in production).
    pub boot_timeout_secs: u64,
    /// Pool max-size. From `SANDBOX_PG_POOL_MAX`, default 16 (D-17).
    pub pool_max: usize,
}

/// Errors surfaced by [`Database`] at boot or during migration
/// apply. Live-path call-site errors land in Phase 1.
#[derive(Debug, thiserror::Error)]
pub enum DatabaseError {
    /// Underlying pg driver failure (connect / TLS / auth / query).
    #[error("pg: {0}")]
    Pg(#[from] compio_postgres::Error),
    /// Non-migrator boot path observed `MAX(version) < target_version`
    /// at the moment we last polled. Distinct from `BootTimeout`:
    /// `SchemaTooOld` is the snapshot diagnosis, `BootTimeout` is
    /// the give-up after waiting.
    #[error("schema version {observed} below required {required}")]
    SchemaTooOld { observed: i64, required: i64 },
    /// Non-migrator boot path waited up to `boot_timeout_secs` and
    /// the schema never reached `target_version`. Operator must
    /// tag a designated migrator (`SANDBOX_PG_RUN_MIGRATIONS=1`) or
    /// fix whatever is keeping the migrator from making progress.
    #[error("boot timeout waiting for schema version {required}")]
    BootTimeout { required: i64 },
    /// Designated migrator failed to apply a specific version.
    /// `reason` carries the underlying pg error message; the runner
    /// rolls the open transaction back before surfacing this.
    #[error("migration failed at version {version}: {reason}")]
    MigrationFailed { version: i64, reason: String },
    /// Boot-time configuration validation failed (DSN scheme, HA
    /// env vars, host_id parse, etc). Refuses to start.
    #[error("validation: {0}")]
    Validation(String),
}

/// Result alias for the module.
pub type Result<T> = std::result::Result<T, DatabaseError>;

// ────────────────────────────────────────────────────────────────────
// Database handle
// ────────────────────────────────────────────────────────────────────

/// Owned connection-pool wrapper + boot-time config snapshot.
///
/// `compio_postgres::Pool` is `!Send` (uses `Rc` / `RefCell`
/// internally — see `crates/compio-postgres/src/pool.rs:15-17`).
/// `Arc<Database>` is therefore `!Send` and lives on a single compio
/// runtime thread. That matches the rest of the controller, which is
/// already single-threaded per process (§ 5).
pub struct Database {
    pool: Pool,
    config: DbConfig,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Database {
    /// Construct from env. `Ok(None)` is the disabled-by-absence
    /// shape — the operator did not set `SANDBOX_DATABASE_URL`, so
    /// pg integration is off. Mirrors
    /// `crates/sandbox/src/persist.rs::Persistence::from_env`'s
    /// disabled-shape contract.
    ///
    /// Validation order (any failure aborts boot):
    ///   1. DSN present + scheme is `postgres://` or `postgresql://`.
    ///   2. HA env-var sanity (R-NN: `lease_ttl >= 4 * heartbeat`).
    ///   3. Pool numbers parse + are within sane bounds.
    ///   4. Host identity loads (env → file → generate-and-persist).
    ///   5. Pool opens (TCP + auth + warm `min_idle` connections).
    pub async fn from_env() -> Result<Option<Self>> {
        let dsn = match std::env::var("SANDBOX_DATABASE_URL") {
            Ok(v) if !v.is_empty() => v,
            _ => return Ok(None),
        };
        Self::from_env_with_dsn(dsn).await.map(Some)
    }

    /// Build with an explicit DSN — used by tests that want a
    /// per-test pg fixture without round-tripping through env.
    pub async fn from_env_with_dsn(dsn: String) -> Result<Self> {
        validate_dsn_scheme(&dsn)?;
        let dsn = inject_password_if_configured(dsn)?;

        // HA env validation (Phase 0 even though the takeover task
        // ships in Phase 4; § 15 / R-NN).
        validate_ha_env_vars()?;

        let run_migrations = matches!(
            std::env::var("SANDBOX_PG_RUN_MIGRATIONS").as_deref(),
            Ok("1")
        );

        let boot_timeout_secs = parse_env_u64("SANDBOX_PG_BOOT_TIMEOUT_SECS", 60)?;
        let pool_max = parse_env_usize("SANDBOX_PG_POOL_MAX", 16)?;
        if pool_max == 0 {
            return Err(DatabaseError::Validation(
                "SANDBOX_PG_POOL_MAX must be >= 1".into(),
            ));
        }

        let host_id = load_or_generate_host_id()?;

        // Open the pool eagerly (warm `min_idle` connections — see
        // pool.rs::connect_with_config). Any connect error here is a
        // boot-time failure.
        let mut pool_cfg = PoolConfig::default();
        pool_cfg.max_size = pool_max;
        let pool =
            Pool::connect_with_config(&dsn, pool_cfg)
                .await
                .map_err(|e| DatabaseError::Pg(e))?;

        let config = DbConfig {
            dsn,
            host_id,
            run_migrations,
            boot_timeout_secs,
            pool_max,
        };
        Ok(Self { pool, config })
    }

    /// Used by tests + the integration test suite: build from a
    /// `(dsn, run_migrations, boot_timeout_secs)` triple, skipping
    /// env. Persists no host_id file. Visible across the crate
    /// boundary for `tests/sandbox_pg_e2e.rs` — production callers
    /// use [`Database::from_env`].
    #[doc(hidden)]
    pub async fn from_test_config(
        dsn: String,
        run_migrations: bool,
        boot_timeout_secs: u64,
    ) -> Result<Self> {
        validate_dsn_scheme(&dsn)?;
        let mut pool_cfg = PoolConfig::default();
        pool_cfg.max_size = 4;
        let pool =
            Pool::connect_with_config(&dsn, pool_cfg)
                .await
                .map_err(|e| DatabaseError::Pg(e))?;
        let config = DbConfig {
            dsn,
            host_id: Uuid::now_v7(),
            run_migrations,
            boot_timeout_secs,
            pool_max: 4,
        };
        Ok(Self { pool, config })
    }

    /// Cheap accessor for the controller's stable identity.
    pub fn host_id(&self) -> Uuid {
        self.config.host_id
    }

    /// Borrow the underlying pool. Phase-1 call sites use this to
    /// run the actual INSERT/UPDATE/SELECT statements; Phase 0 only
    /// the migration runner consumes it.
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Cheap connectivity check — used by `/readyz` and by integration
    /// tests as the "are we connected?" gate.
    pub async fn ping(&self) -> Result<()> {
        let client = self.pool.get().await.map_err(DatabaseError::Pg)?;
        let rows = client
            .query("SELECT 1", &[])
            .await
            .map_err(DatabaseError::Pg)?;
        if rows.len() != 1 {
            return Err(DatabaseError::Validation(format!(
                "ping returned {} rows; expected 1",
                rows.len()
            )));
        }
        Ok(())
    }

    /// Read `MAX(version)` from `sandbox.schema_migrations`. Returns
    /// 0 if the table does not yet exist (fresh database). Used by
    /// the boot-path wait loop and by tests.
    pub async fn current_schema_version(&self) -> Result<i64> {
        let client = self.pool.get().await.map_err(DatabaseError::Pg)?;
        // Check existence first so we can return 0 cleanly for a
        // fresh database without spamming pg with an error.
        let exists: bool = client
            .query_one(
                "SELECT EXISTS (
                     SELECT 1 FROM pg_catalog.pg_tables
                      WHERE schemaname = 'sandbox' AND tablename = 'schema_migrations'
                 )",
                &[],
            )
            .await
            .map_err(DatabaseError::Pg)?
            .get(0);
        if !exists {
            return Ok(0);
        }
        let row = client
            .query_one(
                "SELECT COALESCE(MAX(version), 0)::BIGINT FROM sandbox.schema_migrations",
                &[],
            )
            .await
            .map_err(DatabaseError::Pg)?;
        let v: i64 = row.get(0);
        Ok(v)
    }

    /// The boot-path schema gate (§ 7.1). Returns `Ok(())` once the
    /// schema is at or past `target`. Designated migrator applies
    /// pending migrations; everyone else polls.
    pub async fn ensure_schema_at_version(&self, target: i64) -> Result<()> {
        if self.config.run_migrations {
            self.run_pending_migrations().await?;
        }
        // Even the designated migrator re-reads `MAX(version)` so a
        // failed-to-apply migration surfaces as `SchemaTooOld` rather
        // than silently passing — defensive belt-and-braces.
        let deadline = Instant::now() + Duration::from_secs(self.config.boot_timeout_secs);
        loop {
            let current = self.current_schema_version().await?;
            if current >= target {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(DatabaseError::BootTimeout { required: target });
            }
            // Poll cadence — § 7.1's sketch picks 2s.
            compio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Apply every embedded migration whose version is greater than
    /// `MAX(version)`. Each is wrapped in `BEGIN ... COMMIT`; if the
    /// `INSERT INTO schema_migrations` loses a race against a
    /// concurrent migrator (UNIQUE on `version`), the loser rolls
    /// back its TX, observes the migration is now applied, and
    /// continues. No advisory locks (§ 7.1, D-4).
    pub async fn run_pending_migrations(&self) -> Result<u64> {
        // Bootstrap: ensure the schema + table exist outside any
        // migration TX so a fresh database can be queried for
        // `MAX(version)` below. This is itself idempotent.
        self.ensure_schema_migrations_table().await?;

        let current = self.current_schema_version().await?;
        let mut applied: u64 = 0;
        for m in MIGRATIONS.iter().filter(|m| m.version > current) {
            self.apply_one_migration(*m).await?;
            applied += 1;
        }
        Ok(applied)
    }

    async fn ensure_schema_migrations_table(&self) -> Result<()> {
        let client = self.pool.get().await.map_err(DatabaseError::Pg)?;
        client
            .batch_execute(
                "CREATE SCHEMA IF NOT EXISTS sandbox; \
                 CREATE TABLE IF NOT EXISTS sandbox.schema_migrations ( \
                     version     BIGINT       PRIMARY KEY, \
                     applied_at  TIMESTAMPTZ  NOT NULL DEFAULT now(), \
                     sha256      TEXT         NOT NULL, \
                     description TEXT         NOT NULL DEFAULT '' \
                 )",
            )
            .await
            .map_err(DatabaseError::Pg)?;
        Ok(())
    }

    async fn apply_one_migration(&self, m: Migration) -> Result<()> {
        let mut client = self.pool.get().await.map_err(DatabaseError::Pg)?;

        // The DDL body. `batch_execute` inside a TX runs every
        // statement in the file under one transaction (BEGIN issued
        // by `client.transaction()`). Any statement that is itself
        // not transactionable would have to land in a Phase-2
        // migration with `Down-Compatible: …` markup — Phase 0
        // ships only `0001_initial.sql`, all of which is plain DDL
        // wrapped in IF NOT EXISTS guards.
        let tx = client.transaction().await.map_err(DatabaseError::Pg)?;
        if let Err(e) = tx.batch_execute(m.sql).await {
            // Surface the body's error as a well-typed
            // MigrationFailed; the TX rolls back automatically when
            // we drop it without commit.
            return Err(DatabaseError::MigrationFailed {
                version: m.version,
                reason: e.to_string(),
            });
        }

        let sha = m.sha256_hex();
        let insert_res = tx
            .execute(
                "INSERT INTO sandbox.schema_migrations \
                     (version, sha256, description) \
                 VALUES ($1::BIGINT, $2::TEXT, $3::TEXT)",
                &[&m.version, &sha, &m.description.to_string()],
            )
            .await;

        match insert_res {
            Ok(_) => {
                tx.commit().await.map_err(DatabaseError::Pg)?;
                tracing::info!(
                    version = m.version,
                    description = m.description,
                    "sandbox.schema_migrations: applied"
                );
                Ok(())
            }
            Err(e) if is_unique_violation(&e) => {
                // Race-tolerance fallback: another migrator inserted
                // this version while we were running our DDL. Our
                // DDL was idempotent; the unique_violation tells us
                // the row is now present. Roll back and treat as
                // success. (§ 7.1)
                let _ = tx.rollback().await;
                tracing::info!(
                    version = m.version,
                    "sandbox.schema_migrations: applied by concurrent migrator; skipping"
                );
                Ok(())
            }
            Err(e) => Err(DatabaseError::Pg(e)),
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────

fn validate_dsn_scheme(dsn: &str) -> Result<()> {
    if dsn.starts_with("postgres://") || dsn.starts_with("postgresql://") {
        Ok(())
    } else {
        Err(DatabaseError::Validation(format!(
            "SANDBOX_DATABASE_URL must start with postgres:// or postgresql://, got: {}",
            // Don't echo the full string in case it carries a
            // password (Round-2 § 13.1 forbids it but defense in
            // depth — log only the scheme prefix).
            dsn.split_once(':').map(|(s, _)| s).unwrap_or("(empty)")
        )))
    }
}

/// If `SANDBOX_DATABASE_PASSWORD_PATH` is set, read the file and
/// inject the password into the DSN. Otherwise return the DSN
/// unchanged. Phase 0 only enforces the file mount when the env
/// var is present; production-grade mode-0o400 enforcement and
/// host-allowlist checks land with the Phase-1 security review.
fn inject_password_if_configured(dsn: String) -> Result<String> {
    let Ok(path) = std::env::var("SANDBOX_DATABASE_PASSWORD_PATH") else {
        return Ok(dsn);
    };
    if path.is_empty() {
        return Ok(dsn);
    }
    let password = std::fs::read_to_string(&path)
        .map_err(|e| {
            DatabaseError::Validation(format!(
                "SANDBOX_DATABASE_PASSWORD_PATH={path:?}: {e}"
            ))
        })?
        .trim_end_matches(|c: char| c == '\n' || c == '\r')
        .to_string();
    // Validate the DSN is parseable BEFORE touching it so we don't
    // silently overwrite a hand-crafted DSN we don't recognize.
    let _: Config = dsn
        .parse()
        .map_err(|e: compio_postgres::Error| DatabaseError::Validation(e.to_string()))?;
    // Splice via a separator we can identify. The pg connection
    // string format permits `password=...` as a key=value pair OR
    // `postgresql://user:pass@host/...` URI syntax. We pick URI
    // syntax to match the rest of the codebase (the proposal's
    // examples + control-plane's `DATABASE_URL` use the URI form).
    Ok(splice_password_into_uri(&dsn, &password))
}

/// Inject `password` into `dsn` (URI form), preserving the rest of
/// the URI byte-for-byte.
fn splice_password_into_uri(dsn: &str, password: &str) -> String {
    // Parse `<scheme>://<userinfo>@<rest>`. If `userinfo` already
    // carries a password (`user:existing@host`), replace it.
    // Otherwise insert (`user@host` → `user:pass@host`). The pg
    // wire format is the platform's contract; we URL-encode the
    // password's reserved chars conservatively.
    let encoded = url_encode_password(password);
    if let Some((scheme, rest)) = dsn.split_once("://") {
        if let Some((userinfo, after)) = rest.split_once('@') {
            let user = userinfo.split_once(':').map(|(u, _)| u).unwrap_or(userinfo);
            return format!("{scheme}://{user}:{encoded}@{after}");
        }
    }
    // Fallback: caller-supplied DSN had no `@` — append as
    // `?password=` query param. The platform's deploys all use the
    // userinfo form, but we don't refuse the rare deploy that
    // doesn't.
    if dsn.contains('?') {
        format!("{dsn}&password={encoded}")
    } else {
        format!("{dsn}?password={encoded}")
    }
}

/// Percent-encode the characters that are reserved in URI
/// userinfo or query strings. Conservative — every non-alphanumeric
/// is encoded.
fn url_encode_password(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        let b = *byte;
        let unreserved = b.is_ascii_alphanumeric()
            || b == b'-'
            || b == b'_'
            || b == b'.'
            || b == b'~';
        if unreserved {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// HA env-var validator (Round-7 / R-NN; lands in Phase 0 even
/// though the takeover task that consumes the values ships in
/// Phase 4). Refuses boot when:
///   - heartbeat <= 0
///   - lease_ttl <= 0
///   - lease_ttl < 4 * heartbeat
fn validate_ha_env_vars() -> Result<()> {
    let heartbeat = parse_env_i64("SANDBOX_HA_HEARTBEAT_SECS", 5)?;
    let lease_ttl = parse_env_i64("SANDBOX_HA_LEASE_TTL_SECS", 60)?;

    if heartbeat <= 0 {
        return Err(DatabaseError::Validation(format!(
            "SANDBOX_HA_HEARTBEAT_SECS must be > 0, got {heartbeat}"
        )));
    }
    if lease_ttl <= 0 {
        return Err(DatabaseError::Validation(format!(
            "SANDBOX_HA_LEASE_TTL_SECS must be > 0, got {lease_ttl}"
        )));
    }
    // The 4× heartbeat lower-bound is the R-NN safety margin —
    // shorter TTLs cause spurious takeovers during routine
    // heartbeat jitter (a brief GC pause flips A's lease to
    // "expired" from B's view; B starts a takeover that A's next
    // heartbeat would have refuted). Default lease_ttl=60 with
    // heartbeat=5 gives a 12× safety margin.
    if lease_ttl < heartbeat.saturating_mul(4) {
        return Err(DatabaseError::Validation(format!(
            "SANDBOX_HA_LEASE_TTL_SECS={lease_ttl} must be >= 4 * \
             SANDBOX_HA_HEARTBEAT_SECS ({}); see R-NN in the design",
            heartbeat * 4
        )));
    }
    Ok(())
}

fn parse_env_i64(name: &str, default: i64) -> Result<i64> {
    match std::env::var(name) {
        Ok(s) => s.trim().parse::<i64>().map_err(|e| {
            DatabaseError::Validation(format!("{name}={s:?}: {e}"))
        }),
        Err(_) => Ok(default),
    }
}

fn parse_env_u64(name: &str, default: u64) -> Result<u64> {
    match std::env::var(name) {
        Ok(s) => s.trim().parse::<u64>().map_err(|e| {
            DatabaseError::Validation(format!("{name}={s:?}: {e}"))
        }),
        Err(_) => Ok(default),
    }
}

fn parse_env_usize(name: &str, default: usize) -> Result<usize> {
    match std::env::var(name) {
        Ok(s) => s.trim().parse::<usize>().map_err(|e| {
            DatabaseError::Validation(format!("{name}={s:?}: {e}"))
        }),
        Err(_) => Ok(default),
    }
}

/// Resolve the controller's stable identity. Order:
///   1. `SANDBOX_HOST_ID` env (typed-id `hst_<base62>` form). Parsed
///      via `zeroship_core::typed_id::parse_with_prefix("hst")`.
///   2. `<state_dir>/host_id` file (UUIDv7 hyphenated). State dir is
///      `SANDBOX_PERSIST_DIR/state/` (alongside `sealed-records/`),
///      defaulting to `/var/lib/zeroship/sandbox/state/`.
///   3. Fresh UUIDv7 — written to the file (mode 0600 on Unix) so
///      the next boot picks up the same identity.
fn load_or_generate_host_id() -> Result<Uuid> {
    if let Ok(s) = std::env::var("SANDBOX_HOST_ID") {
        let s = s.trim();
        if !s.is_empty() {
            // Be permissive: accept either typed-id form or raw
            // UUID. The proposal's § 10.1 lifecycle picks typed-id;
            // Phase-0 dev workflows often paste raw UUIDs.
            if let Ok(uuid) =
                zeroship_core::typed_id::parse_with_prefix(s, "hst")
            {
                return Ok(uuid);
            }
            if let Ok(uuid) = Uuid::parse_str(s) {
                return Ok(uuid);
            }
            return Err(DatabaseError::Validation(format!(
                "SANDBOX_HOST_ID={s:?}: expected typed-id (`hst_<base62>`) or hyphenated UUID"
            )));
        }
    }

    let path = host_id_file_path();
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim();
        if let Ok(uuid) = Uuid::parse_str(s) {
            return Ok(uuid);
        }
        // Corrupt file — refuse to silently regenerate (an operator
        // who wants a fresh identity deletes the file explicitly).
        return Err(DatabaseError::Validation(format!(
            "host_id file {path:?} contents not parseable as UUID"
        )));
    }

    // Generate + persist.
    let uuid = Uuid::now_v7();
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Err(DatabaseError::Validation(format!(
                "create state dir {parent:?}: {e}"
            )));
        }
    }
    write_host_id_file(&path, uuid).map_err(|e| {
        DatabaseError::Validation(format!("write host_id file {path:?}: {e}"))
    })?;
    Ok(uuid)
}

fn host_id_file_path() -> PathBuf {
    let base = std::env::var("SANDBOX_PERSIST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/var/lib/zeroship/sandbox"));
    base.join("state").join("host_id")
}

fn write_host_id_file(path: &std::path::Path, uuid: Uuid) -> std::io::Result<()> {
    let s = uuid.to_string();
    std::fs::write(path, s)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// True iff `e` is a SQLSTATE 23505 (unique_violation). Used for
/// the migration runner's race-tolerance fallback (§ 7.1).
fn is_unique_violation(e: &compio_postgres::Error) -> bool {
    e.code() == Some(&compio_postgres::error::SqlState::UNIQUE_VIOLATION)
}

// Suppress `unused_imports` for `Arc`/`NoTls` until Phase 1 wires them in.
#[allow(dead_code)]
fn _phase1_anchors(_: &Arc<()>, _: NoTls) {}

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    // Tests mutate process-global env via `std::env::{set_var,
    // remove_var}` (unsafe in 2024-edition stdlib). The `ENV_LOCK`
    // mutex serialises every call within this module so the
    // process-wide invariant holds. No other crate mutates these
    // env vars at test time.
    use super::*;

    /// Test fixture: every test that mutates env must hold this lock
    /// to avoid stomping on its peers. `cargo test` runs lib tests
    /// in parallel by default.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_env_clean<R>(f: impl FnOnce() -> R) -> R {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Clear every env var the validator reads so each test
        // starts from a known baseline.
        for k in [
            "SANDBOX_DATABASE_URL",
            "SANDBOX_DATABASE_PASSWORD_PATH",
            "SANDBOX_PG_RUN_MIGRATIONS",
            "SANDBOX_PG_BOOT_TIMEOUT_SECS",
            "SANDBOX_PG_POOL_MAX",
            "SANDBOX_HOST_ID",
            "SANDBOX_PERSIST_DIR",
            "SANDBOX_HA_HEARTBEAT_SECS",
            "SANDBOX_HA_LEASE_TTL_SECS",
        ] {
            // SAFETY: `remove_var` is documented as unsafe in newer
            // stdlib (Rust 2024) because env mutation is not
            // thread-safe. The ENV_LOCK above serializes test
            // accesses to env state.
            unsafe {
                std::env::remove_var(k);
            }
        }
        f()
    }

    fn set_env(k: &str, v: &str) {
        // SAFETY: caller holds the ENV_LOCK via with_env_clean.
        unsafe { std::env::set_var(k, v) }
    }

    // ─── HA env-var validator (R-NN) ─────────────────────────────

    #[test]
    fn from_env_validates_lease_ttl_minimum() {
        with_env_clean(|| {
            // Case 1 — TTL exactly 4× heartbeat: passes.
            set_env("SANDBOX_HA_LEASE_TTL_SECS", "20");
            set_env("SANDBOX_HA_HEARTBEAT_SECS", "5");
            validate_ha_env_vars().expect("20 >= 4*5 should pass");

            // Case 2 — TTL below 4× heartbeat: fails.
            set_env("SANDBOX_HA_LEASE_TTL_SECS", "10");
            set_env("SANDBOX_HA_HEARTBEAT_SECS", "5");
            let err = validate_ha_env_vars()
                .expect_err("10 < 4*5 should fail");
            match err {
                DatabaseError::Validation(msg) => {
                    assert!(msg.contains("SANDBOX_HA_LEASE_TTL_SECS"));
                }
                other => panic!("expected Validation, got {other:?}"),
            }

            // Case 3 — TTL = 0: fails on the > 0 check.
            set_env("SANDBOX_HA_LEASE_TTL_SECS", "0");
            set_env("SANDBOX_HA_HEARTBEAT_SECS", "5");
            let err = validate_ha_env_vars()
                .expect_err("ttl=0 should fail");
            assert!(matches!(err, DatabaseError::Validation(_)));

            // Case 4 — heartbeat = 0: fails on the > 0 check.
            set_env("SANDBOX_HA_LEASE_TTL_SECS", "60");
            set_env("SANDBOX_HA_HEARTBEAT_SECS", "0");
            let err = validate_ha_env_vars()
                .expect_err("heartbeat=0 should fail");
            assert!(matches!(err, DatabaseError::Validation(_)));

            // Case 5 — negative: parses as i64, fails the > 0
            // check (parse itself succeeds for negative input).
            set_env("SANDBOX_HA_LEASE_TTL_SECS", "-10");
            set_env("SANDBOX_HA_HEARTBEAT_SECS", "5");
            let err = validate_ha_env_vars()
                .expect_err("negative ttl should fail");
            assert!(matches!(err, DatabaseError::Validation(_)));

            // Case 6 — defaults pass (60s ttl, 5s heartbeat).
            unsafe {
                std::env::remove_var("SANDBOX_HA_LEASE_TTL_SECS");
                std::env::remove_var("SANDBOX_HA_HEARTBEAT_SECS");
            }
            validate_ha_env_vars()
                .expect("defaults (60, 5) should pass");
        });
    }

    // ─── DSN scheme validation ───────────────────────────────────

    #[test]
    fn from_env_validates_dsn_scheme() {
        validate_dsn_scheme("postgres://localhost/db").unwrap();
        validate_dsn_scheme("postgresql://localhost/db").unwrap();

        let err = validate_dsn_scheme("mysql://localhost/db")
            .expect_err("mysql should be rejected");
        assert!(matches!(err, DatabaseError::Validation(_)));

        let err = validate_dsn_scheme("")
            .expect_err("empty should be rejected");
        assert!(matches!(err, DatabaseError::Validation(_)));

        let err = validate_dsn_scheme("postgres-bogus://x")
            .expect_err("postgres-bogus should be rejected");
        assert!(matches!(err, DatabaseError::Validation(_)));
    }

    // ─── host_id resolution ──────────────────────────────────────

    #[test]
    fn from_env_generates_host_id_when_absent() {
        with_env_clean(|| {
            let tmp = tempdir();
            set_env("SANDBOX_PERSIST_DIR", tmp.path().to_str().unwrap());
            let uuid = load_or_generate_host_id().expect("generate");
            // File written; same value on next call.
            let again = load_or_generate_host_id().expect("re-load");
            assert_eq!(uuid, again, "host_id must be stable across calls");
            // The file itself contains the UUID (hyphenated form).
            let path = tmp.path().join("state").join("host_id");
            let on_disk = std::fs::read_to_string(&path).unwrap();
            assert_eq!(on_disk.trim(), uuid.to_string());
        });
    }

    #[test]
    fn from_env_loads_host_id_from_persistent_file() {
        with_env_clean(|| {
            let tmp = tempdir();
            set_env("SANDBOX_PERSIST_DIR", tmp.path().to_str().unwrap());
            // Pre-write a UUID; the loader must return that exact one.
            let preset = uuid::Uuid::now_v7();
            let path = tmp.path().join("state").join("host_id");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, preset.to_string()).unwrap();

            let uuid = load_or_generate_host_id().unwrap();
            assert_eq!(uuid, preset);
        });
    }

    #[test]
    fn from_env_accepts_typed_id_host_id_env() {
        with_env_clean(|| {
            let tmp = tempdir();
            set_env("SANDBOX_PERSIST_DIR", tmp.path().to_str().unwrap());
            let typed = zeroship_core::typed_id::generate("hst");
            set_env("SANDBOX_HOST_ID", &typed);
            let uuid = load_or_generate_host_id().unwrap();
            // The loaded UUID must match the embedded UUID inside
            // the typed-id.
            let (_, expected) = zeroship_core::typed_id::parse(&typed).unwrap();
            assert_eq!(uuid, expected);
        });
    }

    #[test]
    fn from_env_rejects_malformed_host_id_env() {
        with_env_clean(|| {
            let tmp = tempdir();
            set_env("SANDBOX_PERSIST_DIR", tmp.path().to_str().unwrap());
            set_env("SANDBOX_HOST_ID", "not-a-uuid-or-typed-id");
            let err = load_or_generate_host_id()
                .expect_err("garbage SANDBOX_HOST_ID must error");
            assert!(matches!(err, DatabaseError::Validation(_)));
        });
    }

    // ─── Unique-violation detection ──────────────────────────────

    #[test]
    fn url_encode_password_handles_reserved_chars() {
        assert_eq!(url_encode_password("abc"), "abc");
        assert_eq!(url_encode_password("a:b@c"), "a%3Ab%40c");
        assert_eq!(url_encode_password("p@ss w0rd"), "p%40ss%20w0rd");
    }

    #[test]
    fn splice_password_into_uri_replaces_userinfo() {
        let dsn = "postgres://alice@db.example/zeroship";
        let out = splice_password_into_uri(dsn, "secret");
        assert_eq!(out, "postgres://alice:secret@db.example/zeroship");

        let dsn = "postgres://alice:old@db.example/zeroship";
        let out = splice_password_into_uri(dsn, "new");
        assert_eq!(out, "postgres://alice:new@db.example/zeroship");
    }

    // ────────────────────────────────────────────────────────────
    // Tiny scoped tempdir helper. The crate already depends on
    // `std::fs`; pulling `tempfile` for a single helper is over-kill.
    // ────────────────────────────────────────────────────────────

    struct TempDir(PathBuf);
    impl TempDir {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tempdir() -> TempDir {
        // Unique-ish per-test directory under the system temp.
        let base = std::env::temp_dir();
        let name = format!(
            "zsbx-db-test-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        );
        let path = base.join(name);
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }
}
