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
use std::time::{Duration, Instant};

use compio_postgres::{Config, Pool, PoolConfig};
use uuid::Uuid;

// ────────────────────────────────────────────────────────────────────
// Embedded migrations
// ────────────────────────────────────────────────────────────────────
//
// Migration files are read at compile time via `include_str!`. Each
// file is a single transactional migration (the runner wraps it in
// BEGIN/COMMIT) plus an idempotent guard around every CREATE so a
// loser of a two-migrator race can replay safely (D-4 / § 7.1).

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "initial schema (hosts, sandboxes, shares, events, deleted_sandboxes)",
        sql: include_str!("../migrations/0001_initial.sql"),
    },
    Migration {
        version: 2,
        description: "sandboxes.status CHECK accepts 'unreachable'",
        sql: include_str!("../migrations/0002_sandbox_status_unreachable.sql"),
    },
    Migration {
        version: 3,
        description: "shares.token_id CHECK accepts base64url alphabet",
        sql: include_str!("../migrations/0003_share_token_id_alphabet.sql"),
    },
];

/// The latest migration version this binary was built against. Boot
/// path passes this as `target_version` to
/// [`Database::ensure_schema_at_version`]; non-migrator controllers
/// poll until the schema reaches at least this version.
pub const LATEST_MIGRATION_VERSION: i64 = 3;

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
    /// Round-1 fixer / IMPORTANT #7: a CAS-guarded UPDATE matched
    /// zero rows because the row's `generation` had advanced past
    /// the caller's `expected_generation`. Distinct from `NotFound`
    /// — the row exists, just at a generation we no longer own.
    /// Carries the typed-id and the stale expected counter so the
    /// caller's audit log surfaces a precise lost-leadership event.
    #[error("CAS lost for sandbox {sandbox_id}: expected generation {expected_generation}")]
    CasLost {
        sandbox_id: String,
        expected_generation: i64,
    },
    /// Round-1 fixer / IMPORTANT #7: the targeted row does not exist
    /// (or has been tombstoned, or the tenant fence excluded it).
    /// Carries the typed-id so the caller can include it in the
    /// audit log without re-deriving the string.
    #[error("not found: {sandbox_id}")]
    NotFound { sandbox_id: String },
}

/// Result alias for the module.
pub type Result<T> = std::result::Result<T, DatabaseError>;

// ────────────────────────────────────────────────────────────────────
// Database handle
// ────────────────────────────────────────────────────────────────────

/// Boot-time config snapshot. The pg connection pool itself is
/// `!Send` (`compio_postgres::Pool` uses `Rc` / `RefCell` internally
/// — see `crates/compio-postgres/src/pool.rs:15-17`); ntex requires
/// the worker factory closure to be `Send + Clone` so we cannot
/// stash a `Pool` inside the shared `Arc<AppState>`.
///
/// Phase-0 design: `Database` holds the resolved DSN + config and
/// builds a transient pool inline for migration runs (one boot
/// pass) and for `ping`. Phase 1 introduces a per-ntex-worker
/// thread-local pool when call sites actually consume it.
///
/// `Database` itself is `Send + Sync` (just String + Copy fields),
/// so `Arc<Database>` plumbs cleanly through `AppState` without
/// breaking ntex's worker-factory bounds.
#[derive(Debug, Clone)]
pub struct Database {
    config: DbConfig,
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

        // Eagerly verify the DSN connects + auths so a misconfigured
        // controller fails fast at boot rather than at first call.
        // The transient pool is dropped immediately; Phase-1 call
        // sites build per-worker pools when they need them.
        let mut pool_cfg = PoolConfig::default();
        pool_cfg.max_size = pool_max;
        let pool = Pool::connect_with_config(&dsn, pool_cfg)
            .await
            .map_err(DatabaseError::Pg)?;
        // Smoke-test the connection.
        let client = pool.get().await.map_err(DatabaseError::Pg)?;
        let _ = client.query("SELECT 1", &[]).await.map_err(DatabaseError::Pg)?;
        drop(client);
        drop(pool);

        let config = DbConfig {
            dsn,
            host_id,
            run_migrations,
            boot_timeout_secs,
            pool_max,
        };
        Ok(Self { config })
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
        // Smoke-test the connection so callers get a clean error
        // when the test fixture is misconfigured.
        let mut pool_cfg = PoolConfig::default();
        pool_cfg.max_size = 4;
        let pool = Pool::connect_with_config(&dsn, pool_cfg)
            .await
            .map_err(DatabaseError::Pg)?;
        let _client = pool.get().await.map_err(DatabaseError::Pg)?;
        drop(_client);
        drop(pool);
        let config = DbConfig {
            dsn,
            host_id: Uuid::now_v7(),
            run_migrations,
            boot_timeout_secs,
            pool_max: 4,
        };
        Ok(Self { config })
    }

    /// Cheap accessor for the controller's stable identity.
    pub fn host_id(&self) -> Uuid {
        self.config.host_id
    }

    /// Resolved DSN as the `sandbox_app` role, password injected if
    /// a `SANDBOX_DATABASE_PASSWORD_PATH` was configured. Phase 1
    /// uses this to build per-ntex-worker connection pools when
    /// call sites actually need pooled access.
    pub fn dsn(&self) -> &str {
        &self.config.dsn
    }

    /// Configured pool max-size (D-17 default 16). Phase 1 reads
    /// this when constructing the per-worker pool.
    pub fn pool_max(&self) -> usize {
        self.config.pool_max
    }

    /// Open a transient connection pool. Used by the boot-time
    /// migration runner + `ping`; Phase 1 introduces a per-worker
    /// long-lived pool. Returned `Pool` is `!Send` and lives only
    /// on the caller's compio thread.
    async fn open_pool(&self) -> Result<Pool> {
        let mut cfg = PoolConfig::default();
        cfg.max_size = self.config.pool_max.max(2);
        Pool::connect_with_config(&self.config.dsn, cfg)
            .await
            .map_err(DatabaseError::Pg)
    }

    /// Cheap connectivity check — used by `/readyz` and by integration
    /// tests as the "are we connected?" gate.
    pub async fn ping(&self) -> Result<()> {
        let pool = self.open_pool().await?;
        let client = pool.get().await.map_err(DatabaseError::Pg)?;
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
        let pool = self.open_pool().await?;
        let client = pool.get().await.map_err(DatabaseError::Pg)?;
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
        let pool = self.open_pool().await?;
        Self::ensure_schema_migrations_table(&pool).await?;

        let current = self.current_schema_version_with_pool(&pool).await?;
        let mut applied: u64 = 0;
        for m in MIGRATIONS.iter().filter(|m| m.version > current) {
            Self::apply_one_migration(&pool, *m).await?;
            applied += 1;
        }
        Ok(applied)
    }

    async fn current_schema_version_with_pool(&self, pool: &Pool) -> Result<i64> {
        let client = pool.get().await.map_err(DatabaseError::Pg)?;
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
        Ok(row.get::<_, i64>(0))
    }

    async fn ensure_schema_migrations_table(pool: &Pool) -> Result<()> {
        // Two simultaneous migrators racing on the bootstrap CREATE
        // SCHEMA hit a 23505 (`pg_namespace_nspname_index`) because
        // pg's IF NOT EXISTS is not race-safe. Same race-tolerance
        // shape as the migration runner: catch the duplicate-DDL
        // SQLSTATEs and treat as success.
        let client = pool.get().await.map_err(DatabaseError::Pg)?;
        match client
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
        {
            Ok(()) => Ok(()),
            Err(e) if is_concurrent_ddl_race(&e) => {
                tracing::info!(
                    code = %e.code().map(|c| c.code()).unwrap_or(""),
                    "sandbox bootstrap: concurrent-DDL race on schema_migrations create; treating as success"
                );
                Ok(())
            }
            Err(e) => Err(DatabaseError::Pg(e)),
        }
    }

    async fn apply_one_migration(pool: &Pool, m: Migration) -> Result<()> {
        let mut client = pool.get().await.map_err(DatabaseError::Pg)?;

        // The DDL body. `batch_execute` inside a TX runs every
        // statement in the file under one transaction (BEGIN issued
        // by `client.transaction()`). Any statement that is itself
        // not transactionable would have to land in a Phase-2
        // migration with `Down-Compatible: …` markup — Phase 0
        // ships only `0001_initial.sql`, all of which is plain DDL
        // wrapped in IF NOT EXISTS guards.
        let tx = client.transaction().await.map_err(DatabaseError::Pg)?;
        if let Err(e) = tx.batch_execute(m.sql).await {
            let _ = tx.rollback().await;
            // Concurrent-DDL race against another migrator — pg's
            // `IF NOT EXISTS` is not atomic; the loser sees a
            // duplicate_schema / duplicate_table / duplicate_object
            // / unique_violation SQLSTATE. Treat as race-tolerant
            // success (§ 6.5 partition note + § 7.1). The
            // bookkeeping INSERT below either lands (we win the
            // version) or itself unique_violations (we lose).
            if !is_concurrent_ddl_race(&e) {
                return Err(DatabaseError::MigrationFailed {
                    version: m.version,
                    reason: e.to_string(),
                });
            }
            tracing::info!(
                version = m.version,
                code = %e.code().map(|c| c.code()).unwrap_or(""),
                "sandbox migration body: concurrent-DDL race; retrying bookkeeping insert only"
            );
            // Re-acquire a fresh TX for the bookkeeping insert.
            // The body's effects are now durable (the winner
            // committed them); we just need to record OUR row, or
            // recognise that the winner's row is present.
            return Self::insert_bookkeeping_row(pool, m).await;
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

    /// Insert ONLY the `schema_migrations` bookkeeping row, used by
    /// the race-tolerance retry path: we hit a duplicate_schema /
    /// duplicate_table / etc on the body, which means the winner
    /// has already committed the body's effects. We just need to
    /// either land our row (if the winner hadn't reached the
    /// INSERT yet) or recognise the winner's row (unique_violation
    /// on our INSERT).
    async fn insert_bookkeeping_row(pool: &Pool, m: Migration) -> Result<()> {
        let client = pool.get().await.map_err(DatabaseError::Pg)?;
        let sha = m.sha256_hex();
        match client
            .execute(
                "INSERT INTO sandbox.schema_migrations \
                     (version, sha256, description) \
                 VALUES ($1::BIGINT, $2::TEXT, $3::TEXT) \
                 ON CONFLICT (version) DO NOTHING",
                &[&m.version, &sha, &m.description.to_string()],
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => Err(DatabaseError::Pg(e)),
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────

/// Shape check for `sandbox.shares.token_id`, mirroring migration
/// 0003's CHECK (`^tok_[A-Za-z0-9_-]{20,40}$`). Belt-and-suspenders
/// before the SQL round-trip so a malformed id surfaces a clean
/// `Validation` error rather than a SQLSTATE 23514 buried in the
/// pg driver wrapper.
fn validate_share_token_id_shape(token_id: &str) -> Result<()> {
    let suffix = match token_id.strip_prefix("tok_") {
        Some(s) => s,
        None => {
            return Err(DatabaseError::Validation(format!(
                "share token_id must start with 'tok_': {token_id:?}"
            )));
        }
    };
    if !(20..=40).contains(&suffix.len()) {
        return Err(DatabaseError::Validation(format!(
            "share token_id suffix length out of range (20..=40): {token_id:?}"
        )));
    }
    if !suffix
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(DatabaseError::Validation(format!(
            "share token_id suffix contains non-base64url chars: {token_id:?}"
        )));
    }
    Ok(())
}

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
/// the migration runner's race-tolerance fallback on the bookkeeping
/// INSERT into `sandbox.schema_migrations` (§ 7.1).
fn is_unique_violation(e: &compio_postgres::Error) -> bool {
    e.code() == Some(&compio_postgres::error::SqlState::UNIQUE_VIOLATION)
}

/// True iff `e` indicates a concurrent DDL race we can safely treat
/// as success — the loser of two simultaneous `CREATE SCHEMA IF NOT
/// EXISTS` / `CREATE TABLE IF NOT EXISTS` / `CREATE INDEX IF NOT
/// EXISTS` calls. Pg's `IF NOT EXISTS` clauses are NOT atomic
/// against concurrent creators (the existence check + the create
/// are separate statements internally); the loser sees one of these
/// SQLSTATEs.
///
/// Same race-tolerance shape § 6.5 calls out for the partition
/// CREATE on the worker side: "the loser sees 42P07
/// duplicate_object and proceeds".
fn is_concurrent_ddl_race(e: &compio_postgres::Error) -> bool {
    use compio_postgres::error::SqlState;
    match e.code() {
        Some(c) => {
            *c == SqlState::UNIQUE_VIOLATION
                || *c == SqlState::DUPLICATE_SCHEMA
                || *c == SqlState::DUPLICATE_TABLE
                || *c == SqlState::DUPLICATE_OBJECT
        }
        None => false,
    }
}


// ────────────────────────────────────────────────────────────────────
// Phase-1 row types + write methods (round-8: pg as system of record)
// ────────────────────────────────────────────────────────────────────

/// One row's-worth of takeover RETURNING data. Used by the Phase-2
/// takeover task to update its in-memory generation map after a
/// successful CAS-guarded ownership rebind (§ 11.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakenSandbox {
    /// Typed-id (`sbx_<base62>`) of the row whose ownership now
    /// belongs to this controller.
    pub sandbox_id: String,
    /// Post-takeover `generation` value. Subsequent CAS-guarded
    /// UPDATEs must carry this as `expected_generation`.
    pub generation: i64,
}

/// One row from `sandbox.sandboxes`. Mirrors the persistent shape of
/// the registry's [`crate::backend::SandboxInfo`] plus host/owner +
/// CAS counter for the Phase-2 lease-based takeover.
#[derive(Debug, Clone)]
pub struct SandboxRow {
    pub sandbox_id: String,
    pub user_id: String,
    pub project_id: String,
    pub backend: String,
    pub vm_index: Option<i32>,
    pub agent_url: Option<String>,
    pub host_id: String,
    pub generation: i64,
    pub status: SandboxStatus,
    pub key_fp: String,
    pub created_at_secs: u64,
    pub started_at_secs: Option<u64>,
    pub stopped_at_secs: Option<u64>,
    pub last_used_at_secs: u64,
}

/// Pg `sandboxes.status` values. Round-8: `Unreachable` is a Phase-1
/// addition so the boot loop can mark agents that 200-don't-respond
/// without losing the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxStatus {
    Starting,
    Running,
    Stopping,
    Stopped,
    Lost,
    Recreating,
    Orphan,
    Unreachable,
}

impl SandboxStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Lost => "lost",
            Self::Recreating => "recreating",
            Self::Orphan => "orphan",
            Self::Unreachable => "unreachable",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        Some(match s {
            "starting" => Self::Starting,
            "running" => Self::Running,
            "stopping" => Self::Stopping,
            "stopped" => Self::Stopped,
            "lost" => Self::Lost,
            "recreating" => Self::Recreating,
            "orphan" => Self::Orphan,
            "unreachable" => Self::Unreachable,
            _ => return None,
        })
    }
}

/// One row from `sandbox.shares`. Mirrors the share-token mint
/// audit metadata (the token bytes themselves are NEVER stored).
#[derive(Debug, Clone)]
pub struct ShareRow {
    pub token_id: String,
    pub sandbox_id: String,
    pub port: u16,
    pub scope: String,
    pub secret_version: i32,
    pub issued_at_secs: u64,
    pub expires_at_secs: u64,
    pub iss: Option<String>,
}

/// Public-facing view of a `sandbox.shares` row (the `secret_version`
/// is exposed but no secret bytes — `sandbox.shares` doesn't carry
/// any to begin with).
#[derive(Debug, Clone)]
pub struct ShareMetadata {
    pub token_id: String,
    pub port: u16,
    pub scope: String,
    pub secret_version: i32,
    pub issued_at_secs: u64,
    pub expires_at_secs: u64,
    pub iss: Option<String>,
    pub last_used_at_secs: u64,
    pub use_count: i64,
}

/// One row's-worth of audit material destined for `sandbox.events`.
/// `kind` follows the open-enum convention from § 6.5; the `data`
/// JSONB payload is enforced ≤ 8 KiB by a CHECK constraint on the
/// table.
#[derive(Debug, Clone)]
pub struct EventRow {
    pub event_id: String,
    pub sandbox_id: String,
    pub user_id: String,
    pub kind: String,
    pub data_json: String,
}

impl Database {
    /// Build a fresh `EventRow` with a freshly-minted typed-id for
    /// `event_id`. Caller fills `kind` + `data_json`.
    pub fn new_event(sandbox_id: &str, user_id: &str, kind: &str, data_json: String) -> EventRow {
        EventRow {
            event_id: zeroship_core::typed_id::generate("evt"),
            sandbox_id: sandbox_id.to_string(),
            user_id: user_id.to_string(),
            kind: kind.to_string(),
            data_json,
        }
    }

    /// INSERT the host row (idempotent ON CONFLICT (host_id) DO
    /// UPDATE). Matches the boot-time host upsert from § 10.1 step 2.
    /// Round-8 Phase 1: hostname/region/backend default to "" /
    /// "us-local-1" / pg's CHECK-passing default unless the operator
    /// supplies them via env. Phase-2 wires the heartbeat task that
    /// updates `last_heartbeat`.
    pub async fn upsert_host(&self, hostname: &str, backend: &str) -> Result<()> {
        let pool = self.open_pool().await?;
        let client = pool.get().await.map_err(DatabaseError::Pg)?;
        let host_id_typed = format!(
            "hst_{}",
            zeroship_core::typed_id::uuid_to_base62(&self.config.host_id)
        );
        // boot_id CHECK is `^[0-9A-Za-z]{20,40}$` — no prefix. Use a
        // bare base62 UUIDv7 (22 chars), not a `boot_<…>` typed-id.
        let boot_id_typed = zeroship_core::typed_id::uuid_to_base62(&Uuid::now_v7());
        let region = std::env::var("SANDBOX_REGION").unwrap_or_else(|_| "us-local-1".to_string());
        client
            .execute(
                "INSERT INTO sandbox.hosts (host_id, boot_id, hostname, region, backend, status) \
                 VALUES ($1::TEXT, $2::TEXT, $3::TEXT, $4::TEXT, $5::TEXT, 'alive') \
                 ON CONFLICT (host_id) DO UPDATE SET \
                     boot_id = EXCLUDED.boot_id, \
                     hostname = EXCLUDED.hostname, \
                     status = 'alive', \
                     last_heartbeat = now()",
                &[
                    &host_id_typed,
                    &boot_id_typed,
                    &hostname.to_string(),
                    &region,
                    &backend.to_string(),
                ],
            )
            .await
            .map_err(DatabaseError::Pg)?;
        Ok(())
    }

    // ────────────────────────────────────────────────────────────────
    // Phase 2 — periodic heartbeat + lease-based takeover (§ 11)
    // ────────────────────────────────────────────────────────────────

    /// Round-1 fixer / IMPORTANT #8: mark THIS controller's host row
    /// `status='draining'`. Called from
    /// [`crate::AppState::trigger_shutdown`] so peers see the
    /// drain-intent before our heartbeat goes silent (without this
    /// hint, peers wait the full lease_ttl before taking over).
    /// Idempotent: running twice is a no-op past the first apply.
    pub async fn set_host_draining(&self) -> Result<()> {
        let pool = self.open_pool().await?;
        let client = pool.get().await.map_err(DatabaseError::Pg)?;
        let host_id_typed = format!(
            "hst_{}",
            zeroship_core::typed_id::uuid_to_base62(&self.config.host_id)
        );
        client
            .execute(
                "UPDATE sandbox.hosts \
                    SET status = 'draining', drain_started_at = COALESCE(drain_started_at, now()) \
                  WHERE host_id = $1::TEXT",
                &[&host_id_typed],
            )
            .await
            .map_err(DatabaseError::Pg)?;
        Ok(())
    }

    /// Bump `last_heartbeat = now()` for THIS controller's host row.
    /// Called periodically by [`crate::spawn_heartbeat_task`]. The
    /// pg-side `now()` is the canonical wall clock for lease-window
    /// decisions (§ 12 R-MM); this UPDATE is the one place a
    /// controller's identity meets pg's clock.
    ///
    /// Returns `Err` if pg is unavailable; the heartbeat task logs
    /// and continues so a transient outage doesn't crash the
    /// controller. If the row is missing (an operator manually
    /// deleted it, or boot's `upsert_host` was skipped), this UPDATE
    /// affects 0 rows but does not error — the next `upsert_host` at
    /// boot would re-create the row.
    pub async fn heartbeat(&self) -> Result<()> {
        let pool = self.open_pool().await?;
        let client = pool.get().await.map_err(DatabaseError::Pg)?;
        let host_id_typed = format!(
            "hst_{}",
            zeroship_core::typed_id::uuid_to_base62(&self.config.host_id)
        );
        client
            .execute(
                "UPDATE sandbox.hosts \
                    SET last_heartbeat = now() \
                  WHERE host_id = $1::TEXT",
                &[&host_id_typed],
            )
            .await
            .map_err(DatabaseError::Pg)?;
        Ok(())
    }

    /// Read the pg-side `now() - last_heartbeat` for THIS controller's
    /// host row. Used by:
    ///   - the takeover task to refresh the
    ///     `sandbox_ha_heartbeat_lag_seconds` gauge, and
    ///   - the clock-rewind detector — § 12 R-MM: a healthy fleet
    ///     never sees a negative lag here, because pg's `now()` is
    ///     monotonic from pg's perspective. A negative lag indicates
    ///     pg's wall clock was rewound or the row's `last_heartbeat`
    ///     was set to a future timestamp.
    ///
    /// Returns `Ok(None)` if the row is absent (a misconfigured
    /// controller never upserted at boot); `Err` only on pg failure.
    pub async fn heartbeat_lag_seconds(&self) -> Result<Option<f64>> {
        let pool = self.open_pool().await?;
        let client = pool.get().await.map_err(DatabaseError::Pg)?;
        let host_id_typed = format!(
            "hst_{}",
            zeroship_core::typed_id::uuid_to_base62(&self.config.host_id)
        );
        let opt = client
            .query_opt(
                "SELECT EXTRACT(EPOCH FROM (now() - last_heartbeat))::DOUBLE PRECISION \
                   FROM sandbox.hosts \
                  WHERE host_id = $1::TEXT",
                &[&host_id_typed],
            )
            .await
            .map_err(DatabaseError::Pg)?;
        Ok(opt.map(|row| row.get::<_, f64>(0)))
    }

    /// Scan `sandbox.hosts` for hosts whose lease has expired —
    /// `status='alive'` AND `last_heartbeat < now() - lease_ttl`.
    /// Returns the typed-id strings (`hst_...`) of dead hosts. The
    /// takeover task pairs each with a CAS UPDATE per § 11.2.
    ///
    /// `lease_ttl_secs` is taken from `SANDBOX_HA_LEASE_TTL_SECS`
    /// (default 60 s, validated at boot to be >= 4 × heartbeat).
    pub async fn dead_hosts(&self, lease_ttl_secs: u64) -> Result<Vec<String>> {
        let pool = self.open_pool().await?;
        let client = pool.get().await.map_err(DatabaseError::Pg)?;
        let lease_ttl_i64 = i64::try_from(lease_ttl_secs)
            .map_err(|e| DatabaseError::Validation(format!("lease_ttl overflow: {e}")))?;
        let rows = client
            .query(
                "SELECT host_id FROM sandbox.hosts \
                  WHERE status = 'alive' \
                    AND last_heartbeat < now() - make_interval(secs => $1::BIGINT)",
                &[&lease_ttl_i64],
            )
            .await
            .map_err(DatabaseError::Pg)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(r.get::<_, String>("host_id"));
        }
        Ok(out)
    }

    /// Outcome of a takeover UPDATE: which sandbox rows changed
    /// owner, and what their new `generation` is. The new owner
    /// must update its in-memory map with these generations so any
    /// subsequent CAS-guarded write carries the right value.
    ///
    /// Empty vec == takeover lost (the dead host's heartbeat
    /// resumed between `dead_hosts()` and the UPDATE; the EXISTS
    /// clause filtered it out per § 11.2).
    pub async fn takeover_sandboxes_from_host(
        &self,
        dead_host_typed: &str,
        my_host: Uuid,
        lease_ttl_secs: u64,
    ) -> Result<Vec<TakenSandbox>> {
        // Self-takeover protection (§ 12.x). Refuse to touch our own
        // row even if env-var misconfiguration somehow surfaces it
        // as "dead". This is defensive: heartbeat + dead_hosts
        // shouldn't ever return self, but we belt-and-suspenders.
        let my_host_typed = format!(
            "hst_{}",
            zeroship_core::typed_id::uuid_to_base62(&my_host)
        );
        if dead_host_typed == my_host_typed {
            return Err(DatabaseError::Validation(format!(
                "refusing self-takeover: dead_host == my_host == {my_host_typed}",
            )));
        }
        // Validate dead_host shape (`hst_<base62>`). Belt-and-
        // suspenders — the WHERE clause already binds via $N, but
        // refusing malformed input early surfaces bugs in callers.
        let _ = zeroship_core::typed_id::parse_with_prefix(dead_host_typed, "hst")
            .map_err(|e| DatabaseError::Validation(e.to_string()))?;

        let pool = self.open_pool().await?;
        let mut client = pool.get().await.map_err(DatabaseError::Pg)?;
        let lease_ttl_i64 = i64::try_from(lease_ttl_secs)
            .map_err(|e| DatabaseError::Validation(format!("lease_ttl overflow: {e}")))?;

        // Two-step in one TX:
        //   (1) The CAS-guarded UPDATE per § 11.2 — atomic with the
        //       EXISTS check on the dead host's heartbeat.
        //   (2) Mark the dead host's row as 'dead' if (and only if)
        //       its lease is STILL expired at this instant. If the
        //       host heart-beated back to life between (1) and (2),
        //       the WHERE clause misses and we leave status='alive'.
        let tx = client.transaction().await.map_err(DatabaseError::Pg)?;
        let rows = tx
            .query(
                "UPDATE sandbox.sandboxes \
                    SET host_id = $1::TEXT, \
                        generation = generation + 1, \
                        last_used_at = now() \
                  WHERE host_id = $2::TEXT \
                    AND status IN ('starting', 'running') \
                    AND deleted_at IS NULL \
                    AND EXISTS ( \
                        SELECT 1 FROM sandbox.hosts \
                         WHERE host_id = $2::TEXT \
                           AND status = 'alive' \
                           AND last_heartbeat < now() - make_interval(secs => $3::BIGINT) \
                    ) \
                  RETURNING sandbox_id, generation",
                &[&my_host_typed, &dead_host_typed.to_string(), &lease_ttl_i64],
            )
            .await
            .map_err(DatabaseError::Pg)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(TakenSandbox {
                sandbox_id: r.get::<_, String>("sandbox_id"),
                generation: r.get::<_, i64>("generation"),
            });
        }

        // Mark the dead host's row 'dead' atomically with the
        // takeover. Race-tolerant against a heartbeat that resumed
        // mid-TX: the WHERE clause checks `last_heartbeat < now() -
        // lease_ttl` again, so a host that came back to life between
        // (1) and here keeps `status='alive'`. Note that the inner
        // takeover above also conditioned on the same predicate, so
        // a takeover-with-zero-rows + still-alive host leaves the
        // host's status untouched as expected.
        tx.execute(
            "UPDATE sandbox.hosts \
                SET status = 'dead' \
              WHERE host_id = $1::TEXT \
                AND status = 'alive' \
                AND last_heartbeat < now() - make_interval(secs => $2::BIGINT)",
            &[&dead_host_typed.to_string(), &lease_ttl_i64],
        )
        .await
        .map_err(DatabaseError::Pg)?;
        tx.commit().await.map_err(DatabaseError::Pg)?;
        Ok(out)
    }

    /// INSERT a fresh sandbox row at create time. The caller supplies
    /// `host_id` (the controller's stable UUIDv7), `key_fp`, and
    /// `agent_url`; everything else comes from `info`.
    pub async fn insert_sandbox(
        &self,
        info: &crate::backend::SandboxInfo,
        host_id: Uuid,
        key_fp: &str,
        agent_url: Option<&str>,
        vm_index: Option<i32>,
    ) -> Result<()> {
        // Belt-and-suspenders parse-then-pass.
        let _ = zeroship_core::typed_id::parse_with_prefix(&info.sandbox_id, "sbx")
            .map_err(|e| DatabaseError::Validation(e.to_string()))?;
        let _ = zeroship_core::typed_id::parse_with_prefix(&info.user_id, "usr")
            .map_err(|e| DatabaseError::Validation(e.to_string()))?;
        let _ = zeroship_core::typed_id::parse_with_prefix(&info.project_id, "prj")
            .map_err(|e| DatabaseError::Validation(e.to_string()))?;

        let pool = self.open_pool().await?;
        let client = pool.get().await.map_err(DatabaseError::Pg)?;
        let host_id_typed = format!(
            "hst_{}",
            zeroship_core::typed_id::uuid_to_base62(&host_id)
        );
        let agent_url_owned = agent_url.map(|s| s.to_string());
        client
            .execute(
                "INSERT INTO sandbox.sandboxes \
                    (sandbox_id, user_id, project_id, backend, vm_index, \
                     agent_url, host_id, generation, status, key_fp, \
                     created_at, started_at, last_used_at) \
                 VALUES ($1::TEXT, $2::TEXT, $3::TEXT, $4::TEXT, $5::INTEGER, \
                         $6::TEXT, $7::TEXT, 0, 'running', $8::TEXT, \
                         now(), now(), now())",
                &[
                    &info.sandbox_id,
                    &info.user_id,
                    &info.project_id,
                    &info.backend,
                    &vm_index,
                    &agent_url_owned,
                    &host_id_typed,
                    &key_fp.to_string(),
                ],
            )
            .await
            .map_err(DatabaseError::Pg)?;
        Ok(())
    }

    /// CAS-guarded status update. Returns the new generation on
    /// success.
    ///
    /// Round-1 fixer / IMPORTANT #7: the failure modes are typed —
    /// `Err(CasLost)` when the row exists at a higher generation
    /// (a peer took over via § 11.2), `Err(NotFound)` when the row
    /// is absent (deleted, never existed, or filtered out by the
    /// tenant fence). Pre-fix, both cases collapsed to
    /// `Validation("CAS missed …")` and the handler matched on
    /// substring. Today the handler matches on the variant.
    ///
    /// Round-1 fixer / IMPORTANT #9: `expected_user_id` is the
    /// optional tenant fence. When `Some`, the WHERE clause appends
    /// `AND user_id = $expected_user_id` so a misrouted call can't
    /// modify rows owned by a different user.
    pub async fn update_sandbox_status(
        &self,
        sandbox_id: Uuid,
        status: SandboxStatus,
        expected_generation: i64,
        expected_user_id: Option<&str>,
    ) -> Result<i64> {
        let pool = self.open_pool().await?;
        let client = pool.get().await.map_err(DatabaseError::Pg)?;
        let sandbox_id_typed = format!(
            "sbx_{}",
            zeroship_core::typed_id::uuid_to_base62(&sandbox_id)
        );
        let stopped_at_clause = match status {
            SandboxStatus::Stopped | SandboxStatus::Lost | SandboxStatus::Orphan => {
                ", stopped_at = COALESCE(stopped_at, now())"
            }
            _ => "",
        };
        let expected_user_owned = expected_user_id.map(|s| s.to_string());
        let sql = format!(
            "UPDATE sandbox.sandboxes \
                SET status = $1::TEXT, \
                    generation = generation + 1, \
                    last_used_at = now()\
                    {stopped_at_clause} \
              WHERE sandbox_id = $2::TEXT \
                AND generation = $3::BIGINT \
                AND ($4::TEXT IS NULL OR user_id = $4::TEXT) \
                AND deleted_at IS NULL \
              RETURNING generation"
        );
        let opt = client
            .query_opt(
                &sql,
                &[
                    &status.as_str().to_string(),
                    &sandbox_id_typed,
                    &expected_generation,
                    &expected_user_owned,
                ],
            )
            .await
            .map_err(DatabaseError::Pg)?;
        if let Some(row) = opt {
            return Ok(row.get::<_, i64>(0));
        }
        // The CAS missed. Distinguish "row exists, different
        // generation" (CasLost) from "row absent / wrong tenant"
        // (NotFound) so the caller can audit-log precisely.
        let lookup = client
            .query_opt(
                "SELECT generation FROM sandbox.sandboxes \
                  WHERE sandbox_id = $1::TEXT \
                    AND ($2::TEXT IS NULL OR user_id = $2::TEXT) \
                    AND deleted_at IS NULL",
                &[&sandbox_id_typed, &expected_user_owned],
            )
            .await
            .map_err(DatabaseError::Pg)?;
        if lookup.is_some() {
            Err(DatabaseError::CasLost {
                sandbox_id: sandbox_id_typed,
                expected_generation,
            })
        } else {
            Err(DatabaseError::NotFound {
                sandbox_id: sandbox_id_typed,
            })
        }
    }

    /// Read a single sandbox row by typed-id. Round-1 fixer /
    /// CRITICAL #4: the post-takeover rehydrate path needs the row's
    /// fields (user_id, project_id, agent_url, key_fp, generation,
    /// status) so it can call `restore::probe_and_register_one`.
    /// Returns `Ok(None)` for an absent / tombstoned row; the caller
    /// treats that as "skip" (the row got tombstoned mid-takeover).
    pub async fn get_sandbox_row(&self, sandbox_id: Uuid) -> Result<Option<SandboxRow>> {
        let pool = self.open_pool().await?;
        let client = pool.get().await.map_err(DatabaseError::Pg)?;
        let sandbox_id_typed = format!(
            "sbx_{}",
            zeroship_core::typed_id::uuid_to_base62(&sandbox_id)
        );
        let opt = client
            .query_opt(
                "SELECT sandbox_id, user_id, project_id, backend, vm_index, \
                        agent_url, host_id, generation, status, key_fp, \
                        EXTRACT(EPOCH FROM created_at)::BIGINT AS created_at_secs, \
                        EXTRACT(EPOCH FROM started_at)::BIGINT AS started_at_secs, \
                        EXTRACT(EPOCH FROM stopped_at)::BIGINT AS stopped_at_secs, \
                        EXTRACT(EPOCH FROM last_used_at)::BIGINT AS last_used_at_secs \
                   FROM sandbox.sandboxes \
                  WHERE sandbox_id = $1::TEXT \
                    AND deleted_at IS NULL",
                &[&sandbox_id_typed],
            )
            .await
            .map_err(DatabaseError::Pg)?;
        Ok(opt.map(|r| {
            let status_str: &str = r.get("status");
            let started_at_opt: Option<i64> = r.try_get("started_at_secs").ok();
            let stopped_at_opt: Option<i64> = r.try_get("stopped_at_secs").ok();
            SandboxRow {
                sandbox_id: r.get("sandbox_id"),
                user_id: r.get("user_id"),
                project_id: r.get("project_id"),
                backend: r.get("backend"),
                vm_index: r.try_get("vm_index").ok(),
                agent_url: r.try_get("agent_url").ok(),
                host_id: r.get("host_id"),
                generation: r.get::<_, i64>("generation"),
                status: SandboxStatus::from_str_opt(status_str)
                    .unwrap_or(SandboxStatus::Lost),
                key_fp: r.get("key_fp"),
                created_at_secs: r.get::<_, i64>("created_at_secs").max(0) as u64,
                started_at_secs: started_at_opt.map(|v| v.max(0) as u64),
                stopped_at_secs: stopped_at_opt.map(|v| v.max(0) as u64),
                last_used_at_secs: r.get::<_, i64>("last_used_at_secs").max(0) as u64,
            }
        }))
    }

    /// List all running sandbox rows owned by `host_id` (the
    /// controller's stable UUIDv7). Used by restart-restore.
    pub async fn list_running_sandboxes_for_host(
        &self,
        host_id: Uuid,
    ) -> Result<Vec<SandboxRow>> {
        let pool = self.open_pool().await?;
        let client = pool.get().await.map_err(DatabaseError::Pg)?;
        let host_id_typed = format!(
            "hst_{}",
            zeroship_core::typed_id::uuid_to_base62(&host_id)
        );
        let rows = client
            .query(
                "SELECT sandbox_id, user_id, project_id, backend, vm_index, \
                        agent_url, host_id, generation, status, key_fp, \
                        EXTRACT(EPOCH FROM created_at)::BIGINT AS created_at_secs, \
                        EXTRACT(EPOCH FROM started_at)::BIGINT AS started_at_secs, \
                        EXTRACT(EPOCH FROM stopped_at)::BIGINT AS stopped_at_secs, \
                        EXTRACT(EPOCH FROM last_used_at)::BIGINT AS last_used_at_secs \
                   FROM sandbox.sandboxes \
                  WHERE host_id = $1::TEXT \
                    AND status = 'running' \
                    AND deleted_at IS NULL",
                &[&host_id_typed],
            )
            .await
            .map_err(DatabaseError::Pg)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let status_str: &str = r.get("status");
            let started_at_opt: Option<i64> = r.try_get("started_at_secs").ok();
            let stopped_at_opt: Option<i64> = r.try_get("stopped_at_secs").ok();
            out.push(SandboxRow {
                sandbox_id: r.get("sandbox_id"),
                user_id: r.get("user_id"),
                project_id: r.get("project_id"),
                backend: r.get("backend"),
                vm_index: r.try_get("vm_index").ok(),
                agent_url: r.try_get("agent_url").ok(),
                host_id: r.get("host_id"),
                generation: r.get::<_, i64>("generation"),
                status: SandboxStatus::from_str_opt(status_str)
                    .unwrap_or(SandboxStatus::Lost),
                key_fp: r.get("key_fp"),
                created_at_secs: r.get::<_, i64>("created_at_secs").max(0) as u64,
                started_at_secs: started_at_opt.map(|v| v.max(0) as u64),
                stopped_at_secs: stopped_at_opt.map(|v| v.max(0) as u64),
                last_used_at_secs: r.get::<_, i64>("last_used_at_secs").max(0) as u64,
            });
        }
        Ok(out)
    }

    /// Move a sandbox row to the `deleted_sandboxes` tombstone in one
    /// TX. After this call, the row is gone from `sandboxes` but the
    /// tombstone keeps the operator's audit trail (and prevents the
    /// boot reconciler from re-INSERTing from a sealed-record orphan
    /// — though round-8 unlinks orphans rather than re-INSERTing).
    ///
    /// `expected_host_id` is round-1 fixer / CRITICAL #3 ownership
    /// fence: when `Some`, the DELETE is gated on `host_id =
    /// $expected` so a controller that LOST the lease can't yank the
    /// row out from under the legitimate new owner. The handler
    /// passes its own host_id; admin-tooling (Phase 3) passes
    /// `None` for cross-owner cleanup.
    ///
    /// `expected_user_id` is round-1 fixer / IMPORTANT #9 tenant
    /// fence: when `Some`, both the tombstone and the DELETE are
    /// gated on `user_id = $expected`. Defense in depth — the
    /// in-memory registry already filters by owner, but a SQL-level
    /// guard means a misrouted call can't leak a row across tenants.
    ///
    /// Returns `Err(DatabaseError::NotFound)` when the DELETE
    /// matched 0 rows (either the row is gone or the fence rejected
    /// our predicate). Caller distinguishes the legitimate-not-found
    /// case from the contended case by checking `expected_*` and
    /// re-reading the row.
    pub async fn delete_sandbox(
        &self,
        sandbox_id: Uuid,
        expected_host_id: Option<Uuid>,
        expected_user_id: Option<&str>,
    ) -> Result<()> {
        let pool = self.open_pool().await?;
        let mut client = pool.get().await.map_err(DatabaseError::Pg)?;
        let sandbox_id_typed = format!(
            "sbx_{}",
            zeroship_core::typed_id::uuid_to_base62(&sandbox_id)
        );
        let expected_host_typed = expected_host_id.map(|h| {
            format!(
                "hst_{}",
                zeroship_core::typed_id::uuid_to_base62(&h)
            )
        });
        let expected_user_owned = expected_user_id.map(|s| s.to_string());
        let tx = client.transaction().await.map_err(DatabaseError::Pg)?;
        // Tombstone INSERT first (with the user_id from the row).
        // The SELECT honours the same fences as the DELETE so we
        // never tombstone a row we don't have authority over.
        tx.execute(
            "INSERT INTO sandbox.deleted_sandboxes (sandbox_id, user_id) \
             SELECT sandbox_id, user_id FROM sandbox.sandboxes \
              WHERE sandbox_id = $1::TEXT \
                AND ($2::TEXT IS NULL OR host_id = $2::TEXT) \
                AND ($3::TEXT IS NULL OR user_id = $3::TEXT) \
             ON CONFLICT (sandbox_id) DO NOTHING",
            &[
                &sandbox_id_typed,
                &expected_host_typed,
                &expected_user_owned,
            ],
        )
        .await
        .map_err(DatabaseError::Pg)?;
        let n = tx
            .execute(
                "DELETE FROM sandbox.sandboxes \
                  WHERE sandbox_id = $1::TEXT \
                    AND ($2::TEXT IS NULL OR host_id = $2::TEXT) \
                    AND ($3::TEXT IS NULL OR user_id = $3::TEXT)",
                &[
                    &sandbox_id_typed,
                    &expected_host_typed,
                    &expected_user_owned,
                ],
            )
            .await
            .map_err(DatabaseError::Pg)?;
        tx.commit().await.map_err(DatabaseError::Pg)?;
        if n == 0 {
            return Err(DatabaseError::NotFound {
                sandbox_id: sandbox_id_typed,
            });
        }
        Ok(())
    }

    /// INSERT a share-token row (mint).
    pub async fn insert_share(&self, share: &ShareRow) -> Result<()> {
        // `token_id` is `tok_<raw_tid>` where `tid` is a random
        // base64url string from `preview_share::fresh_token_id`. It
        // is NOT a typed-id (the suffix is not base62-encoded UUID
        // bytes), so we apply only a shape check that mirrors the
        // pg-side CHECK constraint (migration 0003).
        validate_share_token_id_shape(&share.token_id)?;
        let _ = zeroship_core::typed_id::parse_with_prefix(&share.sandbox_id, "sbx")
            .map_err(|e| DatabaseError::Validation(e.to_string()))?;
        let pool = self.open_pool().await?;
        let client = pool.get().await.map_err(DatabaseError::Pg)?;
        let port_i16 = i16::try_from(share.port)
            .map_err(|e| DatabaseError::Validation(format!("port out of range: {e}")))?;
        let issued_at_secs = i64::try_from(share.issued_at_secs)
            .map_err(|e| DatabaseError::Validation(format!("issued_at overflow: {e}")))?;
        let expires_at_secs = i64::try_from(share.expires_at_secs)
            .map_err(|e| DatabaseError::Validation(format!("expires_at overflow: {e}")))?;
        client
            .execute(
                "INSERT INTO sandbox.shares \
                    (token_id, sandbox_id, port, scope, secret_version, \
                     issued_at, expires_at, iss) \
                 VALUES ($1::TEXT, $2::TEXT, $3::SMALLINT, $4::TEXT, $5::INTEGER, \
                         to_timestamp($6::BIGINT), to_timestamp($7::BIGINT), $8::TEXT)",
                &[
                    &share.token_id,
                    &share.sandbox_id,
                    &port_i16,
                    &share.scope,
                    &share.secret_version,
                    &issued_at_secs,
                    &expires_at_secs,
                    &share.iss,
                ],
            )
            .await
            .map_err(DatabaseError::Pg)?;
        Ok(())
    }

    /// List share-token audit rows for `sandbox_id` filtered by
    /// `port`. Returns metadata only — no secret bytes (the `shares`
    /// table doesn't carry them).
    pub async fn list_shares_for_sandbox(
        &self,
        sandbox_id: Uuid,
        port: u16,
    ) -> Result<Vec<ShareMetadata>> {
        let pool = self.open_pool().await?;
        let client = pool.get().await.map_err(DatabaseError::Pg)?;
        let sandbox_id_typed = format!(
            "sbx_{}",
            zeroship_core::typed_id::uuid_to_base62(&sandbox_id)
        );
        let port_i16 = i16::try_from(port)
            .map_err(|e| DatabaseError::Validation(format!("port out of range: {e}")))?;
        let rows = client
            .query(
                "SELECT token_id, port, scope, secret_version, \
                        EXTRACT(EPOCH FROM issued_at)::BIGINT AS issued_at_secs, \
                        EXTRACT(EPOCH FROM expires_at)::BIGINT AS expires_at_secs, \
                        iss, \
                        COALESCE(EXTRACT(EPOCH FROM last_used_at)::BIGINT, 0) AS last_used_at_secs, \
                        use_count \
                   FROM sandbox.shares \
                  WHERE sandbox_id = $1::TEXT \
                    AND port = $2::SMALLINT \
                    AND deleted_at IS NULL",
                &[&sandbox_id_typed, &port_i16],
            )
            .await
            .map_err(DatabaseError::Pg)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let port_i: i16 = r.get("port");
            out.push(ShareMetadata {
                token_id: r.get("token_id"),
                port: port_i.max(0) as u16,
                scope: r.get("scope"),
                secret_version: r.get::<_, i32>("secret_version"),
                issued_at_secs: r.get::<_, i64>("issued_at_secs").max(0) as u64,
                expires_at_secs: r.get::<_, i64>("expires_at_secs").max(0) as u64,
                iss: r.try_get("iss").ok(),
                last_used_at_secs: r.get::<_, i64>("last_used_at_secs").max(0) as u64,
                use_count: r.get::<_, i64>("use_count"),
            });
        }
        Ok(out)
    }

    /// Rotate the per-sandbox secret-version. Phase-1 model: the
    /// in-memory `PreviewSecrets.sv_current` is the canonical counter;
    /// pg's per-share `secret_version` rows track which secret a
    /// given share was minted under. Rotate-and-clear (the explicit
    /// DELETE flow) marks every existing share row revoked so the
    /// validator can refuse them.
    ///
    /// Returns the count of rows revoked. The caller is responsible
    /// for bumping the in-memory ring's `sv_current`.
    pub async fn rotate_share_secret(&self, sandbox_id: Uuid) -> Result<u64> {
        let pool = self.open_pool().await?;
        let client = pool.get().await.map_err(DatabaseError::Pg)?;
        let sandbox_id_typed = format!(
            "sbx_{}",
            zeroship_core::typed_id::uuid_to_base62(&sandbox_id)
        );
        let n = client
            .execute(
                "UPDATE sandbox.shares \
                    SET revoked_at = now() \
                  WHERE sandbox_id = $1::TEXT \
                    AND deleted_at IS NULL \
                    AND revoked_at IS NULL",
                &[&sandbox_id_typed],
            )
            .await
            .map_err(DatabaseError::Pg)?;
        Ok(n)
    }

    /// INSERT an audit-pipe row.
    pub async fn insert_event(&self, event: &EventRow) -> Result<()> {
        let _ = zeroship_core::typed_id::parse_with_prefix(&event.event_id, "evt")
            .map_err(|e| DatabaseError::Validation(e.to_string()))?;
        let _ = zeroship_core::typed_id::parse_with_prefix(&event.sandbox_id, "sbx")
            .map_err(|e| DatabaseError::Validation(e.to_string()))?;
        let _ = zeroship_core::typed_id::parse_with_prefix(&event.user_id, "usr")
            .map_err(|e| DatabaseError::Validation(e.to_string()))?;
        let pool = self.open_pool().await?;
        let client = pool.get().await.map_err(DatabaseError::Pg)?;
        // JSONB binary mapping for `String` is not in the workspace's
        // postgres-types feature set; cast TEXT → JSONB inside SQL so
        // we can keep the `String` parameter binding.
        client
            .execute(
                "INSERT INTO sandbox.events \
                    (event_id, sandbox_id, user_id, kind, ts, data) \
                 VALUES ($1::TEXT, $2::TEXT, $3::TEXT, $4::TEXT, now(), \
                         CAST($5::TEXT AS JSONB))",
                &[
                    &event.event_id,
                    &event.sandbox_id,
                    &event.user_id,
                    &event.kind,
                    &event.data_json,
                ],
            )
            .await
            .map_err(DatabaseError::Pg)?;
        Ok(())
    }
}

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
