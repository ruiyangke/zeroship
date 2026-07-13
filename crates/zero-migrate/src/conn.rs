//! Connection + executor configuration.
//!
//! The executor runs **out-of-band at deploy** (not the request hot path) over
//! the injected `SqlSession` driver seam. This module owns the connection helper
//! and the per-run [`ExecutorConfig`] (which project, which schema, which meta
//! schema, and the mandatory `statement_timeout` / `lock_timeout` budgets).

use std::time::Duration;


/// Error opening a migrator connection.
///
/// Compiles as an (uninhabited) enum — the connection is now supplied by the
/// host through the `SqlSession` seam, so no in-crate connect path exists to
/// construct it.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
}

/// The **Postgres confinement parameters** — the per-engine apply-confinement
/// strategy inputs that are PG-shaped and meaningless to a non-PG engine
/// (part of the multi-engine abstraction).
///
/// These are NOT engine-agnostic. The PG apply leaf (`crate::apply::executor::pg`)
/// reads them to emit its confinement bracket — `SET LOCAL search_path` (built
/// from `project_schema` + `extension_schemas`), `SET LOCAL ROLE <migrator>`,
/// and the mandatory `SET LOCAL statement_timeout` / `lock_timeout` — plus the
/// `meta_schema` the journal lives in. The SQLite engine confines through a
/// runtime two-mode authorizer (`Arc<AtomicU8>` mode-flip) instead and carries
/// NONE of these; it builds an [`ExecutorConfig`] via [`ExecutorConfig::new`]
/// whose `pg` block is inert default.
///
/// Grouping these into one named struct keeps the neutral [`ExecutorConfig`]
/// from being PG-shaped at the type level: a non-PG backend never reads `pg`,
/// and the confinement STRATEGY stays where it already lives — in each
/// backend's apply leaf (PG's `SET ROLE`/search_path/timeout bracket; SQLite's
/// mode-flip), NOT in the neutral core.
#[derive(Debug, Clone)]
pub struct PgConfinement {
    /// The per-project **meta schema** that holds the append-only
    /// `schema_migrations` journal. Separate from the project
    /// schema so a creator migration can't touch its own history.
    pub meta_schema: String,
    /// Mandatory per-statement timeout. Maps to `SET statement_timeout`.
    /// Bounds how long a statement may **run**; a runaway DDL/DML is cancelled
    /// after this. This is the long-running-statement budget (default 60s).
    pub statement_timeout: Duration,
    /// Mandatory, **separate, SHORT** lock-ACQUISITION timeout (the
    /// safe-migration lock-safety envelope — strong_migrations / Atlas PG101 &
    /// PG103). Maps to `SET lock_timeout`. This is NOT folded into
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
    /// [`crate::model::migration::MigrationFlags::lock_timeout_ms`] (mirrors
    /// `timeout_ms`), so the conservative fail-fast default stays in force for
    /// every other migration in the same deploy.
    pub lock_timeout: Duration,
    /// The least-privilege `migrator` role the apply flow runs each migration's
    /// DDL + journal writes under, via `SET ROLE` / `RESET ROLE` (the
    /// DB-privilege defense layer). `None` runs as the connecting
    /// (admin) role — used only by tests / single-tenant dev where the role
    /// model is not provisioned. In the platform this is always `Some`,
    /// matching a role created by [`crate::apply::role::provision_migrator`].
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

impl PgConfinement {
    /// The default PG confinement for a project whose journal lives in
    /// `meta_schema` (the `<project_schema>_migrations` namespace by default):
    /// conservative non-zero timeouts (no indefinite locks), no `SET ROLE`
    /// (the platform sets it via [`ExecutorConfig::with_migrator_role`]), and
    /// `public` as the extension-type resolution schema.
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

/// Per-run executor configuration.
///
/// The engine-agnostic fields (project identity + trust posture) live directly
/// on this struct; the **PG-specific confinement parameters** are grouped under
/// [`pg`](Self::pg) (a [`PgConfinement`]) so the neutral core is not PG-shaped
/// at the type level. A non-PG backend (SQLite, which confines via a runtime
/// mode-flip) never reads `pg` (part of the multi-engine abstraction).
///
/// The PG `statement_timeout` + `lock_timeout` under [`pg`](Self::pg) are
/// **mandatory** (no indefinite locks / `DoS`). They are applied per
/// migration before its SQL runs.
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// The project id (`prj_…`) — its bytes seed the apply-serializing advisory
    /// lock (`pg_advisory_lock(hashtext(project_id))`).
    pub project_id: String,
    /// The one schema this project's migrations own and may touch. Pinned into
    /// `search_path` for every apply, and the [`crate::guard::SqlGuard`]'s
    /// confinement target.
    pub project_schema: String,
    /// The **Postgres confinement parameters** (meta schema, migrator role,
    /// timeouts, extension-resolution schemas). PG-shaped by construction; read
    /// ONLY by the PG apply leaf. A SQLite-backed [`ExecutorConfig`] carries the
    /// inert [`PgConfinement::new`] default and never consults this — its
    /// confinement is the runtime authorizer mode-flip.
    pub pg: PgConfinement,
    /// PRIVATE (`pub(crate)`). The trust posture every executor-path guard build
    /// derives from. `Confined` for the creator path (set by
    /// [`ExecutorConfig::new`]); `Platform` ONLY via [`ExecutorConfig::platform`]
    /// and `Trusted` ONLY via [`ExecutorConfig::trusted`] when the standalone CLI
    /// feature is enabled (both require an
    /// [`OperatorCapability`] token). Not `pub`, so the control plane — outside
    /// this crate — can neither name `Platform`/`Trusted` (the enum is
    /// `#[non_exhaustive]`) nor reach the constructors: it cannot flip the
    /// executor into a privileged profile.
    pub(crate) trust: crate::model::policy::TrustProfile,
    /// PRIVATE (`pub(crate)`). The schema allowlist a Platform guard permits
    /// references to (e.g. `zero_migrate` / `public`). Empty for
    /// Confined (the `project_schema` is the sole permitted schema there) and for
    /// Trusted (no cross-schema confinement at all).
    pub(crate) platform_schemas: Vec<String>,
    /// PRIVATE (`pub(crate)`). The `CREATE EXTENSION` allowlist a Platform guard
    /// permits (e.g. `citext` / `uuid-ossp`). Empty for Confined and Trusted.
    pub(crate) platform_exts: Vec<String>,
    /// PRIVATE (`pub(crate)`). The OPERATOR capability token, present ONLY on a
    /// config built via [`ExecutorConfig::platform`] or, in standalone CLI/test
    /// builds, [`ExecutorConfig::trusted`].
    /// It rides here so the executor-path guard builds
    /// ([`ExecutorConfig::guard_config`]) can mint the privileged
    /// [`GuardConfig`](crate::guard::GuardConfig) without a fresh out-of-band mint
    /// — the holder already proved operator legitimacy when it constructed this
    /// config. `None` for every Confined config.
    pub(crate) operator_cap: Option<crate::model::capability::OperatorCapability>,
}

impl ExecutorConfig {
    /// A config with sane default timeouts for the named project + schema.
    ///
    /// The meta schema defaults to `<project_schema>_migrations` so it sits
    /// beside the project schema but is a distinct namespace.
    #[must_use]
    pub fn new(project_id: impl Into<String>, project_schema: impl Into<String>) -> Self {
        let project_schema = project_schema.into();
        let meta_schema = format!("{project_schema}_migrations");
        Self {
            project_id: project_id.into(),
            project_schema,
            // The PG-specific confinement block (meta schema, timeouts, role,
            // extension-resolution schemas). Inert for a SQLite-backed config.
            pg: PgConfinement::new(meta_schema),
            // Confined by default — the creator path. `Platform` is reachable
            // ONLY via `ExecutorConfig::platform` (token-gated).
            trust: crate::model::policy::TrustProfile::Confined,
            platform_schemas: Vec::new(),
            platform_exts: Vec::new(),
            operator_cap: None,
        }
    }

    /// Build the [`GuardConfig`](crate::guard::GuardConfig) every executor-path
    /// guard site uses (the two static first-passes + rollback + the
    /// precondition evaluator).
    ///
    /// `Confined` ⇒ `GuardConfig::confined(self.project_schema)` (byte-identical
    /// to the old hardcoded `GuardConfig { project_schema, ext: [] }`).
    /// `Platform` ⇒ `GuardConfig::platform(&cap, self.platform_schemas,
    /// self.platform_exts)`, re-using the token that rides on this config — so
    /// the Platform plan is not re-denied by the executor's own guard.
    /// `Trusted` ⇒ `GuardConfig::trusted(&cap)` — the deny-list is skipped
    /// entirely (the public dbmate-like posture), re-using the operator token.
    ///
    /// A privileged profile WITHOUT a token (never constructible via the
    /// `pub(crate)` ctors, which always stamp one) FAILS CLOSED to Confined.
    #[must_use]
    pub(crate) fn guard_config(&self) -> crate::guard::GuardConfig {
        match (self.trust, self.operator_cap.as_ref()) {
            (crate::model::policy::TrustProfile::Platform, Some(cap)) => crate::guard::GuardConfig::platform(
                cap,
                self.platform_schemas.clone(),
                self.platform_exts.clone(),
            ),
            #[cfg(test)]
            (crate::model::policy::TrustProfile::Trusted, Some(cap)) => {
                crate::guard::GuardConfig::trusted(cap)
            }
            // Confined, or a (never-constructed) privileged-without-token: fail
            // closed to Confined.
            _ => crate::guard::GuardConfig::confined(self.project_schema.clone()),
        }
    }

    /// Build a **Platform** executor config. REQUIRES a
    /// [`OperatorCapability`](crate::model::capability::OperatorCapability) token, mintable
    /// only through named in-crate seams, so neither the control plane
    /// (external; cannot name `Platform` nor mint the token) nor any in-crate
    /// module (`submit`/`engine`; cannot mint the token) can flip the executor
    /// into Platform. `schemas` is the cross-schema allowlist; `extensions` is
    /// the `CREATE EXTENSION` allowlist.
    ///
    /// This ctor is `#[cfg(test)]`-only: the operator-side CLI was retired into
    /// the `zero-migrate-engine` TS CLI, and production Platform
    /// applies flow through the napi host path. The token stays the in-crate
    /// enforcement primitive.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn platform(
        cap: &crate::model::capability::OperatorCapability,
        project_id: impl Into<String>,
        project_schema: impl Into<String>,
        schemas: Vec<String>,
        extensions: Vec<String>,
    ) -> Self {
        let mut cfg = Self::new(project_id, project_schema);
        cfg.trust = crate::model::policy::TrustProfile::Platform;
        cfg.platform_schemas = schemas;
        cfg.platform_exts = extensions;
        cfg.operator_cap = Some(cap.clone());
        cfg
    }

    /// Build a **Trusted** executor config — the public dbmate-like posture.
    /// REQUIRES an
    /// [`OperatorCapability`](crate::model::capability::OperatorCapability) token, EXACTLY
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
    /// sole Trusted producer was retired into the `zero-migrate-engine` TS CLI.
    /// The token stays the in-crate enforcement primitive.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn trusted(
        cap: &crate::model::capability::OperatorCapability,
        project_id: impl Into<String>,
        project_schema: impl Into<String>,
    ) -> Self {
        let mut cfg = Self::new(project_id, project_schema);
        cfg.trust = crate::model::policy::TrustProfile::Trusted;
        // No schema allowlist, no extension allowlist — Trusted has no
        // confinement and no deny-list; these stay inert/empty.
        cfg.operator_cap = Some(cap.clone());
        // `migrator_role` stays `None` (the `new()` default): Trusted runs as the
        // connecting role, exactly like Platform's admin (no `SET ROLE`).
        cfg
    }

    /// Set the least-privilege `migrator_role` the apply
    /// flow runs migrations under. Builder convenience.
    #[must_use]
    pub fn with_migrator_role(mut self, role: impl Into<String>) -> Self {
        self.pg.migrator_role = Some(role.into());
        self
    }

    /// The `search_path` clause value pinned for every apply (a comma-joined,
    /// double-quoted schema list).
    ///
    /// - **Confined** ⇒ the project schema **only** (byte-identical to the old
    ///   hardcoded single-schema pin; the meta schema stays OFF the path so an
    ///   unqualified `up` name can never resolve to the journal).
    /// - **Platform** ⇒ the full configured schema allowlist (e.g.
    ///   `"zero_migrate", "public"`). A multi-schema changelog relies on
    ///   this: a first migration's `CREATE EXTENSION citext` is deliberately unqualified and
    ///   must resolve a creation target (`public`) — and at that point the
    ///   project schema does not yet exist, so a project-schema-only path would
    ///   error `3F000 no schema has been selected to create in`. Cross-schema
    ///   resolution between the project schema and `public` also needs them all
    ///   on the path, matching a deployment where the `postgres`
    ///   principal runs with `search_path = <project>, public`.
    /// - **Trusted** ⇒ the project schema (the `_` fallback). Trusted has no
    ///   confinement — pinning the project schema is merely the default
    ///   resolution target; an explicitly-qualified reference to any other schema
    ///   still resolves (and is no longer guard-blocked), preserving dbmate
    ///   parity. The operator owns the DB, so this pin is convenience, not a
    ///   boundary.
    ///
    /// Every element is an **engine-supplied** identifier (project schema, platform
    /// schemas, extension schemas), so each is rendered through the ONE shared
    /// engine seam ([`crate::render::dml::quote_ident_checked`]) — fail-closed on an empty
    /// / NUL name, byte-identical to the prior `escape_quote_ident` for every real
    /// schema. So the whole quoting surface (not just the DDL/journal seams) is
    /// uniformly self-defending.
    ///
    /// # Errors
    ///
    /// [`crate::render::dml::IdentQuoteError`] if any configured schema is empty or carries
    /// a NUL byte (an engine-internal misconfiguration; never reachable from a
    /// well-formed `ExecutorConfig`).
    pub(crate) fn search_path_clause(&self) -> Result<String, crate::render::dml::IdentQuoteError> {
        let quote = |s: &str| crate::render::dml::quote_ident_checked(s);
        match self.trust {
            crate::model::policy::TrustProfile::Platform if !self.platform_schemas.is_empty() => self
                .platform_schemas
                .iter()
                .map(|s| quote(s))
                .collect::<Result<Vec<_>, _>>()
                .map(|parts| parts.join(", ")),
            // Confined / Trusted: the project schema is first (the sole writable
            // resolution target — `CREATE TABLE foo` lands here, not in an
            // extension schema), followed by the extension schema(s) so an
            // UNQUALIFIED extension type (`vector(N)`, `geography(...)`) resolves.
            // The extension schemas carry USAGE only (provisioned in
            // `role::provision_migrator`), so this is resolution, not write reach.
            _ => {
                let mut parts = vec![quote(&self.project_schema)?];
                for ext in &self.pg.extension_schemas {
                    // Avoid duplicating the project schema if it (oddly) appears.
                    if ext != &self.project_schema {
                        parts.push(quote(ext)?);
                    }
                }
                Ok(parts.join(", "))
            }
        }
    }

    /// `statement_timeout` in whole milliseconds (the unit `SET` takes).
    #[must_use]
    pub fn statement_timeout_ms(&self) -> u64 {
        u64::try_from(self.pg.statement_timeout.as_millis()).unwrap_or(u64::MAX)
    }

    /// `lock_timeout` in whole milliseconds (the unit `SET` takes).
    #[must_use]
    pub fn lock_timeout_ms(&self) -> u64 {
        u64::try_from(self.pg.lock_timeout.as_millis()).unwrap_or(u64::MAX)
    }
}


