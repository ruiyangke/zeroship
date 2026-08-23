//! Connection + executor configuration.
//!
//! The executor runs **out-of-band at deploy** (not the request hot path) over
//! the injected `SqlSession` driver seam. This module owns the connection helper
//! and the per-run [`ExecutorConfig`] (which project, which schema, which meta
//! schema, and the mandatory `statement_timeout` / `lock_timeout` budgets).
//!
//! It sits with the backend contract because [`ExecutorConfig`] is the most-named
//! type in that contract: every one of `MigrationBackend`'s I/O methods takes a
//! `&ExecutorConfig`, and so does every `CrossDeployObligations` method and
//! `OnlineSchemaChange::run_online_backfill`. A config the traits cannot be written without
//! cannot live above the traits.
//!
//! The two private fields survived the crate boundary that would normally dissolve
//! a `pub(crate)`. `effective` and `guard_mode` are still unnameable from outside
//! this module: their only in-crate readers ([`ExecutorConfig::guard_config_for`],
//! [`ExecutorConfig::effective`], the builders) came with them, and the engine's
//! one write site went through the public
//! [`with_effective_policy`](ExecutorConfig::with_effective_policy) setter that
//! already existed. The one member that WOULD have had to widen —
//! `search_path_clause`, a PostgreSQL-only `search_path` builder — was relocated
//! to the PostgreSQL backend instead of being made `pub`.

use std::time::Duration;
use zero_migrate_ir::dialect::DialectId;

/// Error opening a migrator connection.
///
/// Compiles as an (uninhabited) enum — the connection is now supplied by the
/// host through the `SqlSession` seam, so no in-crate connect path exists to
/// construct it.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {}

/// The **apply-confinement parameters** — the per-run inputs that bound what a
/// migration may touch and for how long.
///
/// Every field here is read by **more than one dialect**, which is why the block
/// is named for the concept and not for an engine. The measured readership:
///
/// | field                  | PostgreSQL | MySQL | SQLite |
/// |------------------------|-----------|-------|--------|
/// | `meta_schema`          | yes (`<meta>.schema_migrations`) | yes (same journal SQL) | no (its journal is an attached `_mig` database) |
/// | `statement_timeout`    | yes (`SET statement_timeout`) | yes (`max_execution_time`) | no |
/// | `lock_timeout`         | yes (`SET lock_timeout`) | yes (`innodb_lock_wait_timeout`) | no |
/// | `project_lock_timeout` | **no** (`pg_advisory_lock` takes no timeout) | yes (`GET_LOCK`) | yes (application-file lock) |
///
/// The genuinely PostgreSQL-only settings — the ones no other engine has any
/// use for — stay visibly PostgreSQL under [`postgres`](Self::postgres).
///
/// The confinement STRATEGY still lives in each backend's apply leaf (PG's
/// `SET ROLE`/`search_path`/timeout bracket; SQLite's two-mode authorizer
/// `Arc<AtomicU8>` mode-flip), NOT in this neutral core — this struct carries
/// only the inputs.
#[derive(Debug, Clone)]
pub struct ConfinementConfig {
    /// The per-project **meta schema** that holds the append-only
    /// `schema_migrations` journal. Separate from the project
    /// schema so a creator migration can't touch its own history.
    ///
    /// Read by the PostgreSQL journal and by MySQL's, which spell the same
    /// `<meta>.schema_migrations` namespace. SQLite does not read it: its
    /// journal lives in a separately attached `_mig` database file.
    pub meta_schema: String,
    /// Mandatory per-statement timeout. Bounds how long a statement may
    /// **run**; a runaway DDL/DML is cancelled after this. This is the
    /// long-running-statement budget (default 60s).
    ///
    /// Maps to `SET statement_timeout` on PostgreSQL and to
    /// `max_execution_time` on MySQL. Both engines read a zero as "no limit",
    /// so a zero is refused at `apply::timeout::resolve_timeout_ms` (crate-private,
    /// so named rather than linked) rather than clamped.
    pub statement_timeout: Duration,
    /// Mandatory, **separate, SHORT** lock-ACQUISITION timeout (the
    /// safe-migration lock-safety envelope — strong_migrations / Atlas PG101 &
    /// PG103). Maps to `SET lock_timeout` on PostgreSQL and to
    /// `innodb_lock_wait_timeout` on MySQL (rounded UP to whole seconds, MySQL's
    /// unit). This is NOT folded into
    /// [`statement_timeout`](Self::statement_timeout): the two bound different
    /// things.
    ///
    /// `lock_timeout` bounds only how long a statement waits to **acquire** a
    /// lock before failing with `55P03 lock_not_available`; `statement_timeout`
    /// bounds how long it **runs** once it holds the lock. On a populated, live
    /// multi-tenant table a blocking DDL (e.g. `ALTER TABLE`) takes an
    /// `ACCESS EXCLUSIVE` lock; if any long-running transaction holds a
    /// conflicting lock, the DDL queues behind it — AND, because it is itself
    /// waiting on an `ACCESS EXCLUSIVE` lock, every subsequent query on that
    /// table queues behind the DDL. That is a tenant-wide availability outage
    /// for the lifetime of the wait. A long (statement-class) `lock_timeout`
    /// makes the outage last that long; a SHORT one makes the DDL fail fast
    /// (`55P03`), roll back cleanly (the two-phase recovery handles the abort —
    /// a lock-timeout failure is retryable, never data-corrupting), and free the
    /// table immediately. The operator retries during a quieter window.
    ///
    /// Default: **3s** (`Duration::from_secs(3)`). Short enough that a blocking
    /// DDL cannot stall a live tenant table for more than a few seconds, long
    /// enough to absorb ordinary brief lock contention without spuriously
    /// failing.
    ///
    /// This field is the executor-WIDE default. For a planned maintenance
    /// window, a single migration raises ITS OWN lock-acquisition budget via the
    /// per-migration override
    /// [`zero_migrate_ir::migration::MigrationFlags::lock_timeout_ms`] (mirrors
    /// `timeout_ms`), so the conservative fail-fast default stays in force for
    /// every other migration in the same deploy.
    pub lock_timeout: Duration,
    /// How long a deploy waits for the PROJECT lock another deploy already
    /// holds. Separate from [`Self::lock_timeout`] because the two answer
    /// different questions: `lock_timeout` is a DDL availability budget, bounding
    /// how long ONE statement blocks live application traffic, so it is
    /// deliberately short. This one bounds how long a whole deploy queues behind
    /// a peer deploy, where the right answer is longer than most migrations take.
    ///
    /// Coupling them meant tightening the DDL budget to protect application
    /// traffic also shortened the deploy queue, and 3 seconds is shorter than
    /// many real migrations. The default matches the value MySQL already used for
    /// this concept (`PROJECT_LOCK_TIMEOUT_SECS`, zero-migrate-mysql/src/backend/session.rs).
    ///
    /// Read by the SQLite application-file lock directly, and by MySQL's
    /// `GET_LOCK` rounded UP to whole seconds (MySQL's unit) - so a value under a
    /// second still waits a second there, while a zero stays a single attempt on
    /// both. PostgreSQL's `pg_advisory_lock` takes no timeout and waits, which is
    /// the open question in the queue-versus-fail-fast ticket rather than
    /// something this field decides.
    pub project_lock_timeout: Duration,
    /// The settings **only PostgreSQL reads** — its `SET ROLE` principal and the
    /// extension-resolution schemas its `search_path` needs. Kept under a
    /// PostgreSQL-named block because no other engine has a use for either: they
    /// are not shared concepts wearing a vendor hat, they are genuinely one
    /// vendor's.
    pub postgres: PostgresConfinement,
}

/// The confinement settings **only the PostgreSQL backend reads**.
///
/// The MySQL and SQLite backends read neither field, and would have nothing to do
/// with them if they did — MySQL has no `SET ROLE`-per-transaction confinement
/// model and SQLite has neither roles nor schemas.
///
/// # Where they are read from, measured
///
/// This doc used to claim every field was referenced "solely from
/// `apply/backend/postgres/` and the precondition evaluator". Half of that has
/// become true — the precondition evaluator IS the PostgreSQL backend now — and the
/// other half was never true, which is why the claim is replaced by the measurement
/// rather than trimmed. (That backend is `zero-migrate-postgres/src/backend/` since
/// the execution half left the engine; the paths below are relative to it.)
///
/// `migrator_role` is read only from the PostgreSQL backend
/// (`session`, `backfill_sql`, `primary_key_sql`, `precondition`) and written by
/// [`ExecutorConfig::with_migrator_role`], the host's provisioning seam.
///
/// `extension_schemas` is read from exactly ONE place, and that place is now the
/// PostgreSQL backend too: `search_path_clause`, in that crate's
/// `backend/session.rs`. It used to be a method on the neutral
/// [`ExecutorConfig`] in this file, and this doc named that as the real reason the
/// neutral [`ConfinementConfig`] still carried a vendor-typed field — "relocating
/// the field without first relocating `search_path_clause` would only move the
/// coupling". That relocation has happened: a `search_path` is PostgreSQL's
/// concept, all three callers were already in that file, and all three passed
/// `POSTGRES` as the dialect.
///
/// So what is left here is only DATA, and only the vendor that reads it reads it.
/// The block stays because a per-dialect carrier for run-time config does not
/// exist: `BackendVendor` holds `&'static dyn` policy objects, and these are
/// per-project host input.
#[derive(Debug, Clone)]
pub struct PostgresConfinement {
    /// The least-privilege `migrator` role the apply flow runs each migration's
    /// DDL + journal writes under, via `SET ROLE` / `RESET ROLE` (the
    /// DB-privilege defense layer). `None` runs as the connecting
    /// (admin) role — used only by tests / single-tenant dev where the role
    /// model is not provisioned. In the platform this is always `Some`, matching
    /// the deterministic name returned by the PostgreSQL backend crate's
    /// `role::migrator_role_name` and provisioned by the host.
    pub migrator_role: Option<String>,
    /// The schema(s) that host shared **extension types/functions** the engine
    /// emits UNQUALIFIED (e.g. pgvector's `vector(N)`, `PostGIS`'s
    /// `geography(POINT,4326)`). pgvector / `PostGIS` install into `public` on the
    /// platform image (and the dev `pgvector/pgvector:pg16`), so this defaults to
    /// `["public"]`.
    ///
    /// These schemas are appended (after the project schema) to the migrator's
    /// `search_path` so unqualified extension types/functions RESOLVE, and the
    /// migrator is granted **`USAGE` only** on them (lookup, never CREATE/write).
    /// This matches plugin-db's RUNTIME, which references the same unqualified
    /// `vector`/`geography` types with `public` reachable on its connection path.
    ///
    /// SECURITY: `USAGE` permits *resolving* objects in the schema; it does NOT
    /// permit creating objects there (that needs `CREATE`, which stays revoked)
    /// nor writing existing tables (that needs per-table grants the migrator never
    /// receives). So the cross-schema **write** confinement is unchanged — these
    /// schemas are resolution-only.
    pub extension_schemas: Vec<String>,
}

impl Default for PostgresConfinement {
    /// No `SET ROLE` (the platform sets it via
    /// [`ExecutorConfig::with_migrator_role`]) and `public` as the
    /// extension-type resolution schema.
    fn default() -> Self {
        Self {
            // Defaults to no SET ROLE; the platform sets this to the provisioned
            // deterministic per-project migrator role. Tests opt in explicitly.
            migrator_role: None,
            // Extension types/functions (pgvector `vector`, PostGIS `geography`)
            // live in `public` on the platform/dev image. Resolution-only; the
            // migrator gets USAGE (not CREATE) on these — see the field doc.
            extension_schemas: vec!["public".to_string()],
        }
    }
}

impl ConfinementConfig {
    /// The default confinement for a project whose journal lives in
    /// `meta_schema` (the `<project_schema>_migrations` namespace by default):
    /// conservative non-zero timeouts (no indefinite locks) and the default
    /// PostgreSQL-only block.
    #[must_use]
    fn new(meta_schema: String) -> Self {
        Self {
            meta_schema,
            // Conservative defaults; callers tune per deploy. Non-zero so a
            // runaway migration cannot hold locks indefinitely.
            //
            // The two are deliberately SPLIT (lock-safety envelope): a long
            // RUNNING budget (60s) and a SHORT lock-ACQUISITION budget (3s) so a
            // blocking DDL behind a conflicting lock fails fast (55P03) instead
            // of stalling every query on a live tenant table. See the
            // `lock_timeout` field doc for the full rationale.
            statement_timeout: Duration::from_secs(60),
            lock_timeout: Duration::from_secs(3),
            // Ten seconds, matching the value MySQL already hardcoded for the
            // same concept. A deploy queueing behind a peer is not competing with
            // live application traffic, so it does not want the 3s DDL budget.
            project_lock_timeout: Duration::from_secs(10),
            postgres: PostgresConfinement::default(),
        }
    }
}

/// Per-run executor configuration.
///
/// The project identity + trust posture live directly on this struct; the
/// **confinement parameters** (journal namespace + the three timeout budgets)
/// are grouped under [`confinement`](Self::confinement), and the settings only
/// PostgreSQL reads sit one level further in
/// [`confinement.postgres`](ConfinementConfig::postgres).
///
/// The `statement_timeout` + `lock_timeout` budgets are **mandatory** (no
/// indefinite locks / `DoS`) and are applied per migration before its SQL runs —
/// on PostgreSQL as `SET statement_timeout` / `SET lock_timeout`, on MySQL as
/// `max_execution_time` / `innodb_lock_wait_timeout`. SQLite reads neither; it
/// uses `project_lock_timeout` when acquiring its application-file lock, which
/// is the same budget MySQL passes to `GET_LOCK` and which PostgreSQL alone does
/// not read (`pg_advisory_lock` takes no timeout).
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// The project id (`prj_…`) — its bytes seed the apply-serializing advisory
    /// lock (`pg_advisory_lock(hashtext(project_id))`).
    pub project_id: String,
    /// The one schema this project's migrations own and may touch. Pinned into
    /// `search_path` for every apply, and the registered line-1
    /// guard's confinement target.
    pub project_schema: String,
    /// The **confinement parameters** — the journal's meta schema and the three
    /// timeout budgets, each read by more than one dialect, plus the
    /// PostgreSQL-only role and extension-schema settings nested under
    /// [`postgres`](ConfinementConfig::postgres).
    pub confinement: ConfinementConfig,
    /// PRIVATE (`pub(crate)`). The caller-authored composed policy every
    /// executor-path guard uses. The guard is built from this single policy source
    /// for every composable decision.
    pub(crate) effective: zero_migrate_policy::EffectivePolicy,
    /// PRIVATE (`pub(crate)`). The root/host-set [`GuardMode`] the executor stamps onto
    /// every guard it builds. `Off` ONLY for the Trusted (dbmate-like) posture — the
    /// belt-skip is this posture, NOT a policy grant. Confined/Platform stay `Enforced`.
    ///
    /// [`GuardMode`]: crate::guard::GuardMode
    pub(crate) guard_mode: crate::guard::GuardMode,
}

impl ExecutorConfig {
    /// A config with sane default timeouts and an explicit policy for the named
    /// project and schema.
    ///
    /// The meta schema defaults to `<project_schema>_migrations` so it sits
    /// beside the project schema but is a distinct namespace.
    #[must_use]
    pub fn new(
        project_id: impl Into<String>,
        project_schema: impl Into<String>,
        effective: zero_migrate_policy::EffectivePolicy,
    ) -> Self {
        let project_schema = project_schema.into();
        let meta_schema = format!("{project_schema}_migrations");
        Self {
            project_id: project_id.into(),
            effective,
            project_schema,
            // The confinement block (meta schema, timeouts) plus the
            // PostgreSQL-only nested settings, which are inert on a
            // non-PostgreSQL backend.
            confinement: ConfinementConfig::new(meta_schema),
            // Confined/Platform run the full belt; only `trusted()` flips this to `Off`.
            guard_mode: crate::guard::GuardMode::Enforced,
        }
    }

    /// Replace the guard's composed policy with the deployment's explicit policy.
    ///
    /// Host paths that resolve or lower authored IR under an
    /// [`EffectivePolicy`](zero_migrate_policy::EffectivePolicy)
    /// must carry that same policy into the executor's defense-in-depth guard.
    /// This setter changes no project identity or confinement mode.
    #[must_use]
    pub fn with_effective_policy(
        mut self,
        effective: zero_migrate_policy::EffectivePolicy,
    ) -> Self {
        self.effective = effective;
        self
    }

    /// Build the [`GuardConfig`](crate::guard::GuardConfig) every executor-path
    /// guard site uses for an explicitly selected backend.
    ///
    /// The caller-authored policy is preserved exactly. Trusted test configs differ
    /// only by their explicit host-selected [`GuardMode`](crate::guard::GuardMode).
    ///
    /// Public because the engine's `rollback_with_lock` takes its
    /// guard as an argument, so an out-of-crate driver has to be able to build the
    /// one this config implies. Composing a `GuardConfig` by hand from the same
    /// policy would drop the host-selected mode, and the resulting guard would
    /// admit what the executor's own guard sites deny.
    #[must_use]
    pub fn guard_config_for(&self, dialect: &DialectId) -> crate::guard::GuardConfig {
        crate::guard::GuardConfig::from_policy_with_mode(
            self.effective.clone(),
            dialect.clone(),
            self.guard_mode,
        )
        // Keep the security-critical fail-safe in GuardConfig: every non-Postgres
        // id forces Enforced even when a trusted host selected belt-off mode.
        .for_dialect(dialect.clone())
    }

    /// Build a **Platform** executor config. REQUIRES a
    /// [`OperatorCapability`](zero_migrate_ir::capability::OperatorCapability) token, mintable
    /// only through named in-crate seams, so neither the control plane
    /// (external; cannot name `Platform` nor mint the token) nor any in-crate
    /// module (`submit`/`engine`; cannot mint the token) can flip the executor
    /// into Platform. The caller must supply the explicitly authored policy.
    ///
    /// # The operator-side production Platform seam
    ///
    /// This is the public, token-gated seam an operator-side host uses to build a
    /// Platform-trust executor from an explicitly composed policy. An external
    /// crate can name [`TrustProfile::Platform`](zero_migrate_ir::policy::TrustProfile::Platform)
    /// (it is not fielded), but it can only reach this executor seam by holding
    /// the token minted through the engine's named production seam
    /// [`OperatorCapability::new`](zero_migrate_ir::capability::OperatorCapability::new).
    ///
    /// The napi host path is NOT the only legitimate Platform-apply producer: an
    /// operator-side native host (e.g. the platform's own migrate binary) applies
    /// its own trusted infra schema through this seam over an injected
    /// [`SqlSession`](crate::driver::SqlSession).
    #[must_use]
    pub fn platform(
        _cap: &zero_migrate_ir::capability::OperatorCapability,
        project_id: impl Into<String>,
        project_schema: impl Into<String>,
        effective: zero_migrate_policy::EffectivePolicy,
    ) -> Self {
        Self::new(project_id, project_schema, effective)
    }

    /// Build a **Trusted** executor config — the public dbmate-like posture.
    /// REQUIRES an
    /// [`OperatorCapability`](zero_migrate_ir::capability::OperatorCapability) token, EXACTLY
    /// like [`ExecutorConfig::platform`], mintable only through named in-crate
    /// seams. So neither the control plane (external; cannot
    /// name `Trusted` nor mint the token) nor any in-crate creator-path module
    /// (`submit`/`engine`; cannot mint the token) can flip the executor into
    /// Trusted.
    ///
    /// Trusted runs as the **connecting role** (`migrator_role = None`, like
    /// Platform's admin), with **no schema confinement** and **no deny-list**
    /// (the executor's [`guard_config`](Self::guard_config) returns the Trusted
    /// guard, whose `check()` skips the deny-list/cross-schema/body walks). The
    /// destructive flags are still derived, so a caller's `--yes`-style approval
    /// gate still applies.
    ///
    /// This ctor is `#[cfg(test)]`-only: the operator-side CLI that used to be the
    /// sole Trusted producer was retired into the `zero-migrate-cli` TS CLI.
    /// The token stays the in-crate enforcement primitive.
    // `#[allow(dead_code)]`: the sole in-crate consumer (the live-Postgres
    // Trusted-apply tests) is gated behind a running DB and currently absent, but
    // this `pub(crate)` ctor stays as the pinned in-crate Trusted-config primitive,
    // so it must not be deleted. What keeps a separate integration crate out is the
    // `#[cfg(test)]` below plus `pub(crate)`, not the capability token it takes -
    // that token is freely mintable and authorises nothing. The unforgeable input is
    // the composed `EffectivePolicy`, pinned by the T8 `compile_fail` doctests in
    // `zero_migrate_backend::guard` (NOT by any `tests/trybuild_*`, which has never
    // existed here).
    #[must_use]
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn trusted(
        _cap: &zero_migrate_ir::capability::OperatorCapability,
        project_id: impl Into<String>,
        project_schema: impl Into<String>,
        effective: zero_migrate_policy::EffectivePolicy,
    ) -> Self {
        let mut cfg = Self::new(project_id, project_schema, effective);
        // The belt-skip is not a grant. It is the root/host-set `GuardMode::Off`
        // stamped here, which `guard_config_for()` threads into every guard this config
        // builds.
        cfg.guard_mode = crate::guard::GuardMode::Off;
        // `migrator_role` stays `None` (the `new()` default): Trusted runs as the
        // connecting role, exactly like Platform's admin (no `SET ROLE`).
        cfg
    }

    /// Set the least-privilege `migrator_role` the apply
    /// flow runs migrations under. Builder convenience.
    #[must_use]
    pub fn with_migrator_role(mut self, role: impl Into<String>) -> Self {
        self.confinement.postgres.migrator_role = Some(role.into());
        self
    }

    /// The caller-authored composed policy this config was built with.
    ///
    /// This exists so the `effective` FIELD can stay private now that the engine
    /// paths that read a policy off a config live one crate above it — the same
    /// reason [`GuardConfig::effective`](crate::guard::GuardConfig::effective)
    /// exists, and it grants exactly as little. It is a read-only borrow of a
    /// policy the caller already holds: [`ExecutorConfig::new`] TOOK it,
    /// [`with_effective_policy`](Self::with_effective_policy) replaces it, and
    /// `self.guard_config_for(d).effective()` already returns it by a longer route.
    /// The struct-literal boundary is unaffected — an external crate still cannot
    /// NAME `effective` or `guard_mode`.
    #[must_use]
    pub const fn effective(&self) -> &zero_migrate_policy::EffectivePolicy {
        &self.effective
    }

    /// `statement_timeout` in whole milliseconds (the unit `SET` takes).
    #[must_use]
    pub fn statement_timeout_ms(&self) -> u64 {
        u64::try_from(self.confinement.statement_timeout.as_millis()).unwrap_or(u64::MAX)
    }

    /// Lock-acquisition timeout in whole milliseconds, sent to PostgreSQL as
    /// `SET lock_timeout`. This is the DDL availability budget: how long one
    /// statement may block live traffic. It does NOT bound how long a deploy
    /// queues for the project lock - see [`Self::project_lock_timeout_ms`].
    #[must_use]
    pub fn lock_timeout_ms(&self) -> u64 {
        u64::try_from(self.confinement.lock_timeout.as_millis()).unwrap_or(u64::MAX)
    }

    /// Project-lock wait budget in whole milliseconds - how long a deploy queues
    /// behind a peer deploy holding the same project. Read by the SQLite
    /// application-file lock.
    #[must_use]
    pub fn project_lock_timeout_ms(&self) -> u64 {
        u64::try_from(self.confinement.project_lock_timeout.as_millis()).unwrap_or(u64::MAX)
    }
}
