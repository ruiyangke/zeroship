//! Deploy-time DB migration — the schema-authority §8 wiring.
//!
//! P6 makes `zeroship-migrate` the **single schema authority** for creator
//! apps: a `.zship` ships its versioned migration files (carried on the
//! manifest as content-addressed blobs, see
//! [`zeroship_bundle::MigrationFileEntry`]), and the control-plane deploy step
//! provisions the app's per-app role + schema and **applies the pending
//! migrations BEFORE the go-live commit**. The runtime no longer migrates.
//!
//! # Where this slots into the deploy handler
//!
//! ```text
//! deploy::ingest (writes blobs + manifest)            [api.rs ~505]
//!   → reconstruct migration files from blobs           [api.rs, this module]
//!   → apply_bundle_migrations (THIS module)            ← migrate phase
//!       provision schema "<app_id>" + migrator role
//!       load_dir → engine.plan (Confined) → engine.apply
//!   → set_deploy_with_manifest (go-live)               [api.rs ~605]
//! ```
//!
//! Only on a **successful** migrate does the handler reach the go-live commit.
//! A migrate failure returns an error and the old bundle keeps serving its
//! already-migrated schema (schema-authority §8.3).
//!
//! # The app identity is the trusted path id
//!
//! `app_id` is the **path parameter** the deploy handler already authorized
//! (`AppsDeploy` on `Resource::App{id}`); it is never read from a request body.
//! The per-app schema is `"<app_id>"` and the project id seeding the advisory
//! lock + journal + the least-privilege `migrator_<app_id>` role is the same
//! `app_id` (the plugin-db per-app-schema model). So a creator can only ever
//! migrate the schema they were authorized to deploy.
//!
//! # Profile: Confined, no shadow dry-run (v1)
//!
//! Migrations run under the **Confined** profile (the full SQL deny-list +
//! single-schema confinement to `"<app_id>"`) and the least-privilege
//! `migrator_<app_id>` role (line-2 DB-privilege defense). We **skip the
//! shadow-DB dry-run** at deploy: the shadow path needs a `CREATEDB` admin DSN,
//! and the in-line safety (the engine re-runs the guard on every `up` + the
//! migrator role) is the defense. A future revision MAY add an optional
//! `--admin-db` (CREATEDB) config and run the shadow when present; v1 does not.
//!
//! # Destructive + approval-gated migrations (incl. online `renameColumn`)
//!
//! The deploy path passes [`Approval::None`]. A destructive migration (DROP /
//! TRUNCATE / lossy type change) is therefore **refused** at deploy (the engine
//! gate returns [`EngineError::ApprovalRequired`]) and no go-live happens —
//! destructive schema change goes through the out-of-band `submit_migration`
//! surface / expand-contract across deploys (schema-authority §8.4), not a
//! creator's routine deploy.
//!
//! An online `renameColumn` lowered on the IR path (§2.6) is in the SAME
//! approval-gated class — and this is symmetric across BOTH dialects:
//!
//! - **PG leg (deploy-wired, but refused here):** a PG `renameColumn` lowers to a
//!   `PlanStep::OnlineRename(PgExpandContract)`. Its EXPAND's backfill MUTATES data,
//!   so `run_expand_pg` requires [`Approval::Approved`] and returns
//!   [`zeroship_migrate::OnlineError::Approval`] otherwise. Because
//!   `apply_bundle_ir_migrations` applies under [`Approval::None`] (like every
//!   routine deploy), a `renameColumn` shipped in a `.zship` lowers successfully —
//!   the live-fact type-gate (the `table_snapshots` populated below) runs and
//!   reconciles the IR type against the live column — then is **refused at the
//!   approval gate** ([`DeployMigrateError::OnlineExpand`]); no go-live. So while
//!   the PG rename is type-reconciliation-wired, it is NOT completable through a
//!   routine deploy: like any approval-gated op it must go through the out-of-band
//!   APPROVED-apply surface, which PR2 does NOT wire. (Pinned by
//!   `deploy_migrate_renamecolumn_refused_at_approval_gate_on_routine_deploy`.)
//! - **SQLite leg (not deploy-wired at all):** the SQLite IR-rename rebuild leg is
//!   engine-proven but has no production/dev deploy entry point constructing a
//!   SQLite-dialect `LiveSchema` (see the `sqlite_schemas` note below); a
//!   SQLite-targeted IR rename fails closed before lowering.
//!
//! NEITHER leg of an IR `renameColumn` therefore COMPLETES through a routine wired
//! deploy in PR2 — the PG leg is type-reconciliation-wired but approval-gate-refused;
//! the SQLite leg is unwired. Wiring an approved IR-apply surface (and the SQLite
//! IR-deploy entry point) is the out-of-band / CLI-rewire wave (gated on this PR).
//!
//! ## FOLLOW-UP (named wave): approved IR-apply surface + SQLite-dialect LiveSchema
//!
//! Tracked, intentionally NOT in PR2's scope (spec §2.6.2 lines 1278/1281 scope the
//! PR2 e2e to engine-level dual-leg proof; an approved IR-apply surface is a later
//! CLI-rewire wave). To make an online IR `renameColumn` actually *shippable* a
//! follow-up wave must land TWO things:
//!   1. **Approved IR-apply surface** — an out-of-band entry (mirroring the `.sql`
//!      `submit_migration` surface) that drives `apply_bundle_ir_migrations` under
//!      [`Approval::Approved`] so the PG expand backfill (`run_expand_pg`) is no
//!      longer refused at the gate. Routine `.zship` deploy stays `Approval::None`.
//!   2. **SQLite-dialect `LiveSchema` construction** — a SQLite IR-deploy entry that
//!      populates `sqlite_schemas` from the live SDK `Value`s (the SQLite analogue of
//!      this PG `table_snapshots` introspection), so the SQLite rebuild leg has a
//!      production/dev entry point instead of failing closed before lowering.
//! Until both land, an online rename is engine-proven (PR2) but not go-live-wired.

use std::collections::BTreeMap;
use std::path::Path;

use uuid::Uuid;
use zeroship_migrate::{
    compute_manifest, connect, load_dir_migrations, provision_migrator, Approval, ApprovalScope,
    ConnectError, DeclarativeApplyError, DriftError, EngineError, ExecutorConfig, IrAuthor,
    LiveSchema, LoadAndLowerGuardedError, LoaderError, LockMode, MigrationBackend, MigrationEngine,
    PostgresBackend, RoleError, SqlDialect,
};

/// What a successful deploy-migrate produced (for logging / the deploy log).
#[derive(Debug, Clone, Default)]
pub struct MigrateOutcome {
    /// Migration version ids applied this deploy (empty ⇒ already up to date).
    pub applied: Vec<String>,
    /// Migration version ids skipped because already journaled.
    pub skipped: Vec<String>,
    /// **PR7 online-rename go-live.** The CONTRACT (C1/C2) migrations of any PG
    /// online `renameColumn` whose EXPAND completed this deploy, surfaced as
    /// *pending* — they are NOT applied in this deploy (the cross-deploy
    /// expand-contract partition, §2.0.2): the new column is live + dual-written,
    /// app code migrates from `<from>` to `<to>`, and a SUBSEQUENT approved deploy
    /// applies the contract to drop the old column. Empty on the routine
    /// (`Approval::None`) path — an online expand is refused there before it can
    /// produce a pending contract. The deploy log records these version ids so the
    /// operator/control plane knows a follow-up contract deploy is owed.
    pub pending_contract: Vec<String>,
}

/// A deploy-time migration failure. The deploy handler maps this to an HTTP
/// error and **does not commit go-live** — the old bundle keeps serving.
#[derive(Debug, thiserror::Error)]
pub enum DeployMigrateError {
    /// Opening the admin connection failed.
    #[error("deploy-migrate connect: {0}")]
    Connect(#[from] ConnectError),
    /// Creating the per-app schema `"<app_id>"` failed (admin DDL).
    #[error("deploy-migrate provision schema: {0}")]
    ProvisionSchema(compio_postgres::Error),
    /// Provisioning the least-privilege `migrator_<app_id>` role failed.
    #[error("deploy-migrate provision role: {0}")]
    ProvisionRole(#[from] RoleError),
    /// Loading / parsing the reconstructed migration directory failed (bad
    /// filename grammar, duplicate version, orphan down, unparseable body, …).
    #[error("deploy-migrate load migrations: {0}")]
    Load(#[from] LoaderError),
    /// The engine refused or failed the apply: a guard denial, a destructive
    /// migration without approval (refused at deploy), checksum drift, or a
    /// mid-apply DB error.
    #[error("deploy-migrate apply: {0}")]
    Apply(#[from] EngineError),
    /// A creator `.ir.json` failed the fail-closed LOAD GATE (malformed, future
    /// `ir_version`, structural reject incl. the bare-name DropIndex, ownership
    /// violation, checksum-hint mismatch) or its GUARD-PER-FRAGMENT lower (§6.1.1):
    /// a guard-denied rendered fragment carries the exact op-index + kind
    /// attribution (the production deploy path routes through
    /// `load_and_lower_guarded`, so this attribution reaches the 422 the creator
    /// sees). A creator-fault — the deploy handler maps this to a 422; no go-live.
    #[error("deploy-migrate IR load/guarded-lower ({file}): {source}")]
    Ir {
        /// The `.ir.json` filename the gate / guard refused.
        file: String,
        /// The fail-closed gate / guard-per-fragment lower error.
        #[source]
        source: LoadAndLowerGuardedError,
    },
    /// Reading the `.ir.json` file from the reconstructed migrations dir failed.
    #[error("deploy-migrate read IR file ({file}): {message}")]
    IrRead {
        /// The `.ir.json` filename.
        file: String,
        /// The I/O error.
        message: String,
    },
    /// Introspecting the live schema (to build the IR ownership registry + the
    /// FK-inline live-table set) failed.
    #[error("deploy-migrate live snapshot: {0}")]
    Snapshot(#[from] DriftError),
    /// A rename's online expand/backfill failed while applying an IR plan via
    /// `apply_plan`. REACHABLE since PR2: an IR `renameColumn` lowers to a
    /// `PlanStep::OnlineRename(PgExpandContract)`, whose EXPAND backfill is
    /// approval-gated. Because the routine deploy applies under [`Approval::None`],
    /// the dominant occurrence is [`zeroship_migrate::OnlineError::Approval`] — a
    /// `renameColumn` shipped in a `.zship` is REFUSED at this gate (no go-live) and
    /// must go through the out-of-band approved-apply surface (PR2 does not wire it;
    /// see the module-level "Destructive + approval-gated migrations" doc). Other
    /// `OnlineError` variants surface a genuine mid-expand failure.
    #[error("deploy-migrate IR online expand: {0}")]
    OnlineExpand(#[from] zeroship_migrate::OnlineError),
}

/// Map the plan orchestrator's [`DeclarativeApplyError`] onto the deploy error.
///
/// The IR deploy path routes through `MigrationEngine::apply_plan` (§5.2), which
/// returns [`DeclarativeApplyError`]: its `Plain` arm wraps the SAME
/// [`EngineError`] the prior `engine.apply` path returned (so a destructive-without-
/// approval refusal, a guard denial, or checksum drift stays a
/// [`DeployMigrateError::Apply`] — the tests' match arm is unchanged), and its
/// `Expand` arm (unreachable on PR1 pure-DDL) maps to [`DeployMigrateError::OnlineExpand`].
impl From<DeclarativeApplyError> for DeployMigrateError {
    fn from(e: DeclarativeApplyError) -> Self {
        match e {
            DeclarativeApplyError::Plain(inner) => DeployMigrateError::Apply(inner),
            DeclarativeApplyError::Expand(inner) => DeployMigrateError::OnlineExpand(inner),
        }
    }
}

/// Quote a SQL identifier (double embedded quotes, wrap in `"`). Mirrors the
/// engine's `quote_ident` so the schema name is never raw-interpolated.
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Provision the app's per-app role + schema and apply the bundle's pending
/// migrations under the **Confined** profile, BEFORE go-live (§8).
///
/// `migrate_dsn` is a PRIVILEGED provisioning DSN with `CREATEROLE` + `CREATE`
/// on the database — a SEPARATE admin role, **not** the control-plane
/// `zeroship_control` role (which is BYPASSRLS but has neither privilege; see
/// V0025). It is wired from `--provision-db` / `PROVISION_DATABASE_URL`.
/// `app_id` is the trusted,
/// already-authorized path id; the per-app schema + project id + migrator role
/// are all derived from it. `migrations_dir` holds the migration files the
/// deploy handler reconstructed from the bundle's blobs (Flyway `V<NNNN>__…` or
/// dbmate-shaped). An **empty** directory is a clean no-op (the app ships no
/// schema).
///
/// On success the schema + journal are committed; the caller then commits
/// go-live. On ANY error the caller MUST NOT commit go-live.
///
/// # Errors
/// [`DeployMigrateError`] on connect / schema-provision / role-provision /
/// load / apply failure.
pub async fn apply_bundle_migrations(
    migrate_dsn: &str,
    app_id: &Uuid,
    migrations_dir: &Path,
) -> Result<MigrateOutcome, DeployMigrateError> {
    // The routine `.zship` deploy is NEVER auto-approved: a destructive op or an
    // online expand is refused at the approval gate (no go-live). The AI/creator
    // never auto-applies a gated migration. The scope is irrelevant under
    // `Approval::None` (no destructive op ever runs), so it carries
    // `ApprovalScope::All` for byte-identical behavior. The journal actor is the
    // static routine marker (`"deploy"`/`"deploy-ir"`) — no operator approved it.
    apply_bundle_migrations_with_approval(
        migrate_dsn,
        app_id,
        migrations_dir,
        Approval::None,
        &ApprovalScope::All,
        &DeployActor::Routine,
    )
    .await
}

/// **PR9c CRITICAL (forensic attribution)** — who drove a deploy's migrate phase,
/// stamped into the §2.2 immutable journal's `applied_by`/actor so an
/// operator-approved go-live is auditably DISTINCT from a routine deploy. The
/// routine path records the static marker; the approved path records the
/// operator/admin principal who passed `?approved_versions=` (authorized by the
/// operator-only [`Action::AppsApproveMigration`](zeroship_authz::Action) gate in
/// `api.rs`, NOT the bundle author's `apps:deploy`). Defeating the static
/// `"deploy"` string the critique flagged — the journal can now record WHO
/// approved.
#[derive(Debug, Clone)]
pub enum DeployActor {
    /// Routine fail-closed deploy — no operator approval. The journal records the
    /// static marker (`"deploy"` for the `.sql` leg, `"deploy-ir"` for the IR leg),
    /// byte-identical to pre-PR9c.
    Routine,
    /// Operator-approved go-live. The journal records the approver's principal so a
    /// destructive/online completion is forensically attributable to the human who
    /// approved it.
    Approved {
        /// The approving operator/admin principal (a control-plane user id).
        approver: String,
    },
}

impl DeployActor {
    /// The journal `applied_by`/actor string for the `.sql` leg.
    fn sql_actor(&self) -> String {
        match self {
            Self::Routine => "deploy".to_string(),
            Self::Approved { approver } => format!("deploy-approved:{approver}"),
        }
    }

    /// The journal `applied_by`/actor string for the IR (`.ir.json`) leg.
    fn ir_actor(&self) -> String {
        match self {
            Self::Routine => "deploy-ir".to_string(),
            Self::Approved { approver } => format!("deploy-ir-approved:{approver}"),
        }
    }
}

/// **PR7 online-rename go-live SEAM (engine-wired, deploy-handler deferred)** — the
/// APPROVED out-of-band apply surface (§2.6.2 / §2.0.2). This is a library SEAM, NOT
/// an end-to-end deployable rename: it has NO production caller (the routine deploy at
/// `api.rs` uses [`Approval::None`]), and that test-only status is LOAD-BEARING and
/// pinned (see WIRING PRECONDITIONS below). Identical to [`apply_bundle_migrations`]
/// except it carries
/// [`Approval::Approved`] into the engine, so an approval-gated step **completes**:
/// a PG online `renameColumn`'s EXPAND (E1..E3 + the dual-write backfill) is applied
/// under the held project lock and its CONTRACT (C1/C2) is surfaced as
/// [`MigrateOutcome::pending_contract`] for a later approved contract deploy. This is
/// the deliberate, reviewed approval seam the routine deploy refuses — it is the
/// entry point the control plane drives ONLY after an explicit operator/AI approval
/// of the gated migration set (design §1.6: the AI never auto-rolls-forward a gated
/// change). A destructive DDL op also applies here (approval covers the whole set),
/// so callers MUST gate access to this surface on a real approval decision.
///
/// WIRING PRECONDITIONS (HARD — do NOT wire this surface into a production deploy
/// handler until the remaining one is satisfied; the regression test
/// `production_deploy_handler_never_wires_the_unguarded_approved_go_live_surface`
/// fails RED the instant it is wired):
///
/// 1. §2.0.3 CROSS-DEPLOY PENDING-CONTRACT INTERLOCK — **SATISFIED (PR9a).** The
///    owed contract IS now journaled as a durable outstanding obligation (keyed on a
///    deterministic, re-lower-stable version, §2.0.1), the §2.0.3(2) fail-closed
///    refusal IS implemented (a subsequent deploy whose ops touch a table with an
///    OUTSTANDING pending contract is refused with `TABLE_HAS_PENDING_CONTRACT`), the
///    §2.0.3(3) orphan case IS surfaced by `status`, and `resolve-pending
///    --apply|--abort` discharges it. The whole-deploy project advisory lock is held
///    across the entire multi-file IR loop, so the obligation read-back is race-free.
///    This precondition is met; it is no longer what gates the surface.
/// 2. PER-VERSION APPROVAL SCOPING — **SATISFIED (PR9b).** Approval is now SCOPED to
///    the operator's individually-reviewed `reviewed_versions`: a destructive op (DDL
///    drop/truncate/lossy, destructive DML, SQLite rebuild, PG online-rename EXPAND
///    backfill) whose version-id is NOT in that set is fail-closed REFUSED with
///    [`EngineError::ApprovalNotScoped`] even inside this approved deploy. So approving
///    one reviewed online rename can no longer blanket-authorize an unrelated
///    co-bundled `dropTable`/`dropColumn`. An EMPTY `reviewed_versions` authorizes
///    NOTHING destructive (fail-closed). This precondition is met.
///
/// Both wiring preconditions are now SATISFIED. This surface remains
/// LIBRARY-ONLY (no production handler call) until PR9c wires it into the deploy
/// handler and flips the guard test — the guard test
/// `production_deploy_handler_never_wires_the_unguarded_approved_go_live_surface`
/// stays GREEN by construction here (the string is not introduced into `api.rs`).
///
/// `reviewed_versions` is the operator's individually-reviewed version-id set. It is
/// threaded through to BOTH legs (the `.sql` `apply_verified_scoped` path and the IR
/// `apply_plan_with_touched_and_depends_scoped` path) as an
/// [`ApprovalScope::Versions`]; a destructive op outside it is refused. The non-
/// destructive (additive) ops always run regardless of the set — scope only ever
/// further-restricts destruction.
///
/// # Errors
/// [`DeployMigrateError`] on connect / provision / load / apply failure (incl. a
/// genuine mid-expand `OnlineExpand` failure that is NOT the approval refusal, and a
/// [`DeployMigrateError::Apply`] wrapping [`EngineError::ApprovalNotScoped`] when a
/// destructive op's version is outside `reviewed_versions`).
pub async fn apply_bundle_migrations_approved(
    migrate_dsn: &str,
    app_id: &Uuid,
    migrations_dir: &Path,
    reviewed_versions: &[String],
    actor: &DeployActor,
) -> Result<MigrateOutcome, DeployMigrateError> {
    let scope = ApprovalScope::Versions(reviewed_versions.iter().cloned().collect());
    apply_bundle_migrations_with_approval(
        migrate_dsn,
        app_id,
        migrations_dir,
        Approval::Approved,
        &scope,
        actor,
    )
    .await
}

/// **PR9c — the production deploy-handler routing seam.** The SINGLE place that maps an
/// operator-approved version-id set to one of the two apply surfaces, so the HTTP deploy
/// handler (`api.rs::run_deploy_migrations`) and the go-live e2e drive the EXACT SAME
/// routing code (no test-only copy of the branch).
///
/// **Fail-closed by the empty set.** An EMPTY `approved_versions` ⇒ the ROUTINE
/// [`apply_bundle_migrations`] (`Approval::None`): an online-rename EXPAND / any
/// destructive op is refused before go-live. A NON-EMPTY set ⇒ the SCOPED
/// [`apply_bundle_migrations_approved`] (`ApprovalScope::Versions`), where ONLY the
/// listed versions may run their online/destructive ops and everything outside the set
/// stays refused (`ApprovalNotScoped`). NEVER a blanket bundle-wide approval — the
/// scoped surface builds `Versions` internally, and the §2.0.3 cross-deploy interlock +
/// the per-version scope are inherited from the apply-plan path it routes through.
pub async fn apply_bundle_migrations_routed(
    migrate_dsn: &str,
    app_id: &Uuid,
    migrations_dir: &Path,
    approved_versions: &[String],
    // PR9c CRITICAL: the operator/admin approver identity for forensic attribution.
    // On the EMPTY-set routine path this is ignored (the routine actor marker is
    // recorded); on the NON-EMPTY approved path it is stamped into the immutable
    // journal so the go-live is auditably distinct from a routine deploy. The
    // handler MUST have authorized this principal via `Action::AppsApproveMigration`
    // (operator-only) before passing a non-empty set — see `api.rs`.
    actor: &DeployActor,
) -> Result<MigrateOutcome, DeployMigrateError> {
    if approved_versions.is_empty() {
        apply_bundle_migrations(migrate_dsn, app_id, migrations_dir).await
    } else {
        // NOTE: kept single-line on `migrate_dsn` so the PR9c guard test's structural
        // pin (`apply_bundle_migrations_approved(migrate_dsn`) matches — it proves the
        // non-empty branch routes to the SCOPED approved surface.
        apply_bundle_migrations_approved(migrate_dsn, app_id, migrations_dir, approved_versions, actor).await
    }
}

async fn apply_bundle_migrations_with_approval(
    migrate_dsn: &str,
    app_id: &Uuid,
    migrations_dir: &Path,
    approval: Approval,
    scope: &ApprovalScope,
    actor: &DeployActor,
) -> Result<MigrateOutcome, DeployMigrateError> {
    // The per-app schema + project id are the trusted path id. The plugin-db
    // model maps app_id → schema "<app_id>"; the engine uses the same id to seed
    // the advisory lock, journal, and the migrator_<app_id> role name.
    let schema = app_id.to_string();

    // Load the migration set FIRST — a malformed directory is a deploy error we
    // surface before touching the DB (no schema/role provisioned for a bundle
    // that can't load).
    // PR0 (`op.*` DSL §5.2): `load_dir` now returns `Vec<AppliedPlan>`. The
    // platform/control deploy path is the trusted `.sql` path (every file is a
    // single-step plan), and its apply runs over the FLAT `Migration` set
    // (`apply_verified` + the integrity-manifest fold), so we load the flat form
    // via `load_dir_migrations` — byte-identical to the pre-PR0 behavior. The
    // IR-path apply (PR1+) routes `Vec<AppliedPlan>` through `apply_plan`.
    let migrations = load_dir_migrations(migrations_dir)?;

    // Open the admin connection (CREATEROLE + CREATE SCHEMA). Detaches its
    // driver loop onto the compio runtime.
    let conn = connect(migrate_dsn).await?;

    // (a) Provision the per-app schema. Idempotent: IF NOT EXISTS. The migrator
    //     role provisioning (below) reassigns ownership to the migrator, so the
    //     migrator's DDL + ALTER DEFAULT PRIVILEGES resolve to an owner it
    //     controls.
    conn.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS {}",
        quote_ident(&schema)
    ))
    .await
    .map_err(DeployMigrateError::ProvisionSchema)?;

    // Build the Confined executor config (full deny-list + single-schema
    // confinement to "<app_id>"), running migrations under the least-privilege
    // migrator_<app_id> role (line-2 DB defense).
    let role = zeroship_migrate::migrator_role_name(&schema)?;
    let exec_cfg =
        ExecutorConfig::new(schema.clone(), schema.clone()).with_migrator_role(role.clone());

    // (b) Provision the migrator role (idempotent). Owns the project schema,
    //     no access to the meta schema (unforgeable journal), no reach into
    //     control/auth/other schemas.
    provision_migrator(&conn, &exec_cfg).await?;

    // (b.5) PR9c HIGH — BUNDLE-LEVEL PRE-APPLY SCOPE GATE (no half-state). Under an
    //       APPROVED deploy, refuse the WHOLE bundle BEFORE applying ANY file if a
    //       co-bundled destructive / online-rename-EXPAND step's version is OUTSIDE the
    //       operator's approved set. This makes the refusal ATOMIC: pre-fix, an earlier
    //       approved online-rename EXPAND committed per-step (PG DDL is transactional only
    //       WITHIN a step), then a later out-of-scope op got refused — leaving a
    //       half-renamed table (live dual-write trigger + duplicated column + a journaled
    //       `TABLE_HAS_PENDING_CONTRACT`) that fail-closed every future deploy touching it,
    //       even though the creator saw a 4xx. By validating the entire bundle's scope up
    //       front, no EXPAND ever commits ahead of a guaranteed-later refusal. The per-step
    //       scope gate in the apply loop is RETAINED as defense-in-depth; this gate makes
    //       the refusal whole-bundle. A no-op on the routine (`ApprovalScope::All`) path.
    prevalidate_bundle_scope(&conn, app_id, migrations_dir, scope).await?;

    // (c) Plan (Confined guard) + apply PENDING via the integrity-manifest seam
    //     (`apply_verified`). Approval::None ⇒ a destructive migration is refused
    //     at deploy (no go-live); additive-forward is the routine path. The engine
    //     independently re-runs the guard + the migrator role on every up (defense
    //     in depth) — we do not skip those.
    //
    //     H2 — INTEGRITY MANIFEST: the manifest gate (`manifest.rs`) detects a
    //     creator / AI-author / build-pipeline tampering the migration SET between
    //     authoring/review and apply (reorder / edit / insert / remove). For that
    //     guarantee to hold, the EXPECTED hash MUST come from a TRUSTED, OUT-OF-BAND
    //     source — NOT from the same `.zship` the migrations arrived in (an attacker
    //     who can edit the migrations can edit a hash shipped alongside them, and the
    //     check would vacuously pass; see manifest.rs "Trust model").
    //
    //     No such build-side stamp exists yet: the `.zship` manifest carries only
    //     per-file blob hashes (which travel WITH the migrations — self-consistency,
    //     not an independent expectation), and the control DB stores no migration
    //     manifest hash. So we CANNOT honestly pass an `expected` hash here — doing
    //     so against a bundle-derived value would be a FAKE "verified". Instead we
    //     compute the manifest over the loaded set and LOG it for traceability /
    //     incident forensics, and route through `apply_verified(expected: None)` so
    //     the gate is wired and threading a trusted stamp later is a one-line change.
    //
    //     FOLLOW-UP (REQUIRED for the SEC defense to bite): the build/review side
    //     must stamp `compute_manifest(...)` at authoring time and persist it
    //     out-of-band (control DB, keyed by app + bundle), and this call must then
    //     pass `Some(&expected)` so a tampered/reordered set is REFUSED before any
    //     DDL. Until then this is traceability only, NOT tamper-prevention.
    //
    //     PR9c NOTE — now a LIVE gap, not a dormant one. With the approved go-live
    //     path active (`?approved_versions=` COMPLETES destructive/online ops), an
    //     operator's approval of a REVIEWED version set does not bind the BYTES that
    //     run: a set reordered/edited between review and apply is not refused here.
    //     The runbook (`docs/runbooks/db-migrations.md`, "Operator-approved creator
    //     go-live") documents this — treat approved go-live as integrity-traceable
    //     but NOT tamper-prevented until the H2 stamp lands.
    let manifest = compute_manifest(&migrations);
    tracing::info!(
        app_id = %app_id,
        migration_count = migrations.len(),
        manifest = %manifest.as_str(),
        "deploy-migrate: computed migration-set integrity manifest (traceability only — \
         no trusted build-side stamp to verify against yet; see H2 follow-up)"
    );
    let engine = MigrationEngine::new();
    let guard_cfg = zeroship_migrate::GuardConfig::confined(schema.clone());
    // P6a genericized `MigrationEngine::apply` over `MigrationBackend`; the
    // platform/control deploy path is Postgres, so wrap the connection in the
    // PG backend (behavior-identical to the pre-seam `&Client` call).
    let backend = PostgresBackend::new(&conn);
    let outcome = engine
        .apply_verified_scoped(
            &migrations,
            &guard_cfg,
            // No trusted expectation available (see the H2 note above). NEVER pass a
            // bundle-derived hash here — it would be a vacuous self-check.
            None,
            // PR7: the approval threads from the entry point — `Approval::None` on the
            // routine `.zship` deploy (a destructive `.sql` migration is refused), and
            // `Approval::Approved` on the out-of-band approved-apply surface.
            approval,
            // PR9b: the per-version scope threads from the entry point too — `All` on
            // the routine deploy (irrelevant under `Approval::None`), the operator's
            // reviewed version set on the approved surface, so a co-bundled destructive
            // `.sql` migration outside the reviewed set is fail-closed refused.
            scope,
            &backend,
            &exec_cfg,
            // PR9c: the journal actor — the static `"deploy"` marker on the routine path,
            // or `deploy-approved:<approver>` on an operator-approved go-live, so the
            // §2.2 immutable journal records WHO approved a destructive completion.
            &actor.sql_actor(),
        )
        .await?;

    // (d) CREATOR `.ir.json` PATH (§5.2/§6/§8.6). A `.zship` may ship `.ir.json`
    //     artifacts (the op.* DSL output) alongside / instead of `.sql`. Each is
    //     routed through the fail-closed IR LOAD GATE
    //     (`IrAuthor::load_and_lower`: deserialize → ir_version → validate_ir →
    //     server-stamped ownership → checksum-hint), with the deploy-target dialect
    //     (Postgres here) threaded in, then LOWERED + applied under the SAME
    //     Confined guard + migrator role. This is the production caller of the IR
    //     gate (previously the gate was exported but unreached).
    let ir_outcome = apply_bundle_ir_migrations(
        &backend,
        app_id,
        migrations_dir,
        &exec_cfg,
        &guard_cfg,
        approval,
        scope,
        actor,
    )
    .await?;

    let mut applied = outcome.applied;
    applied.extend(ir_outcome.applied);
    let mut skipped = outcome.skipped;
    skipped.extend(ir_outcome.skipped);
    Ok(MigrateOutcome {
        applied,
        skipped,
        pending_contract: ir_outcome.pending_contract,
    })
}

/// **PR9b — the reviewer-facing "what needs approval" primitive.** Read-only plan a
/// bundle and return the full set of per-version scope-keys an APPROVED deploy would
/// require approval for — exactly the version-ids
/// [`apply_bundle_migrations_approved`]'s `reviewed_versions` should carry when the
/// operator approves the WHOLE bundle.
///
/// This is the in-process source of truth the control-plane approval endpoint (PR9c)
/// surfaces to the reviewer, and the helper the approved-go-live e2e tests use to
/// approve exactly the destructive ops the bundle ships (so the test approves the
/// real reviewed set, never a blanket pass). It runs the SAME load + guarded-lower
/// pipeline the real deploy does (so the version-ids are byte-identical), but applies
/// NOTHING — it only collects each scope-gated step's
/// [`PlanStep::approval_scope_version`](zeroship_migrate::PlanStep::approval_scope_version).
///
/// The live schema must already exist (the bundle's earlier deploys created the
/// tables a rename/backfill/drop targets) — this is read-only introspection, no
/// provisioning. An empty / additive-only bundle returns an empty set (nothing needs
/// approval).
///
/// # Errors
/// [`DeployMigrateError`] on connect / load / IR gate / introspection failure (the
/// same fail-closed errors the real deploy surfaces, since it runs the same pipeline).
pub async fn plan_reviewed_versions(
    migrate_dsn: &str,
    app_id: &Uuid,
    migrations_dir: &Path,
) -> Result<Vec<String>, DeployMigrateError> {
    let conn = connect(migrate_dsn).await?;
    let reviewed = collect_scope_gated_versions(&conn, app_id, migrations_dir).await?;
    Ok(reviewed.into_iter().collect())
}

/// **PR9c HIGH (bundle atomicity) + PR9b reviewer plan — the SHARED scope-gate
/// enumerator.** Lower the WHOLE bundle (`.sql` + every `.ir.json`, version-ordered)
/// read-only — applying NOTHING — and collect the full set of per-version scope-keys
/// any destructive / online-rename-EXPAND step would require approval for. This is the
/// SINGLE source of truth shared by:
///   • [`plan_reviewed_versions`] (the reviewer-facing "what needs approval" list), and
///   • the pre-apply bundle-scope gate ([`prevalidate_bundle_scope`]) that refuses a
///     co-bundled out-of-scope op BEFORE any earlier file's EXPAND can commit.
///
/// Because it runs the SAME load + guarded-lower pipeline the real apply does
/// (advancing the ownership registry + live-set per file across this bundle's
/// freshly-created tables), the version-ids are byte-identical to what the apply loop's
/// per-step scope gate keys on — so a pre-validation pass can NEVER drift from the apply
/// pass and let a half-state slip through.
async fn collect_scope_gated_versions(
    conn: &compio_postgres::Client,
    app_id: &Uuid,
    migrations_dir: &Path,
) -> Result<std::collections::BTreeSet<String>, DeployMigrateError> {
    let schema = app_id.to_string();
    let app = schema.clone();

    let mut reviewed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    // (1) `.sql` leg: a destructive `.sql` migration's version is its own version-id.
    let migrations = load_dir_migrations(migrations_dir)?;
    for m in &migrations {
        if m.flags.destructive {
            reviewed.insert(m.version.as_str().to_string());
        }
    }

    // (2) `.ir.json` leg: lower each file (read-only) the SAME way the deploy does and
    //     collect every scope-gated step's scope-version. Discover IR files.
    let mut ir_files: Vec<std::path::PathBuf> = Vec::new();
    let read = std::fs::read_dir(migrations_dir).map_err(|e| DeployMigrateError::IrRead {
        file: migrations_dir.display().to_string(),
        message: e.to_string(),
    })?;
    for entry in read {
        let entry = entry.map_err(|e| DeployMigrateError::IrRead {
            file: migrations_dir.display().to_string(),
            message: e.to_string(),
        })?;
        let path = entry.path();
        if path.is_file()
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".ir.json"))
        {
            ir_files.push(path);
        }
    }
    if !ir_files.is_empty() {
        ir_files.sort();
        let exec_cfg = ExecutorConfig::new(schema.clone(), schema.clone());
        let guard_cfg = zeroship_migrate::GuardConfig::confined(schema.clone());
        let backend = PostgresBackend::new(conn);
        // Seed the ownership registry + live facts from the LIVE catalog — the same
        // introspection the apply loop runs (read-only). Advance per file across this
        // bundle's freshly-created tables so a later file's ops lower correctly.
        let live = backend.snapshot_schema(&exec_cfg).await?;
        let mut registry: BTreeMap<String, String> =
            live.tables.keys().map(|t| (t.clone(), app.clone())).collect();
        let mut live_schema = LiveSchema {
            tables: live.tables.keys().cloned().collect(),
            unique_indexes: live
                .tables
                .values()
                .flat_map(|t| t.indexes.iter())
                .filter(|idx| idx.unique)
                .map(|idx| idx.name.clone())
                .collect(),
            table_snapshots: live.tables.clone(),
            table_ownership: live.tables.keys().map(|t| (t.clone(), app.clone())).collect(),
            sqlite_schemas: std::collections::BTreeMap::new(),
        };
        for path in &ir_files {
            let file = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("<unknown>")
                .to_string();
            let bytes = std::fs::read_to_string(path).map_err(|e| DeployMigrateError::IrRead {
                file: file.clone(),
                message: e.to_string(),
            })?;
            let author = IrAuthor::new(app.clone(), app.clone(), SqlDialect::Postgres);
            let lowered = author
                .load_and_lower_guarded(&bytes, &app, &registry, &live_schema, &guard_cfg)
                .map_err(|source| DeployMigrateError::Ir { file: file.clone(), source })?;
            for step in &lowered.plan.steps {
                if let Some(v) = step.approval_scope_version() {
                    reviewed.insert(v.to_string());
                }
            }
            for t in lowered.created_tables {
                registry.entry(t.clone()).or_insert_with(|| app.clone());
                live_schema.tables.insert(t);
            }
        }
    }

    Ok(reviewed)
}

/// **PR9c HIGH (bundle atomicity / no half-state) — the PRE-APPLY bundle-scope gate.**
/// Under an APPROVED deploy ([`ApprovalScope::Versions`]), refuse the WHOLE bundle BEFORE
/// applying ANY file if ANY scope-gated step's version is OUTSIDE the operator's approved
/// set. This closes the self-inflicted half-state the critique found: pre-fix, the
/// multi-file apply committed each file per-step (PG DDL is transactional only WITHIN a
/// step), so an earlier approved online-rename EXPAND could durably COMMIT (live dual-write
/// trigger + duplicated column + a journaled `TABLE_HAS_PENDING_CONTRACT` obligation) and
/// THEN a later co-bundled out-of-scope op got refused — leaving a half-renamed table that
/// fail-closes every future deploy touching it, even though the creator saw a 4xx and
/// reasonably believes "nothing happened".
///
/// By validating the ENTIRE bundle's scope up front (option (b) of the fix), no EXPAND ever
/// commits ahead of a guaranteed-later refusal: a bundle whose approved set does not cover
/// every destructive/online-rename version is rejected wholesale, applying NOTHING. The
/// per-step scope gate inside the apply loop is RETAINED as defense-in-depth; this gate
/// makes the refusal atomic.
///
/// Only consulted on the [`ApprovalScope::Versions`] (approved) path — the routine
/// `ApprovalScope::All` / `Approval::None` deploy refuses each destructive op individually
/// at apply, and additive-only routine deploys have no scope-gated step to pre-validate.
async fn prevalidate_bundle_scope(
    conn: &compio_postgres::Client,
    app_id: &Uuid,
    migrations_dir: &Path,
    scope: &ApprovalScope,
) -> Result<(), DeployMigrateError> {
    let ApprovalScope::Versions(_) = scope else {
        return Ok(());
    };
    let gated = collect_scope_gated_versions(conn, app_id, migrations_dir).await?;
    if let Some(unscoped) = gated.iter().find(|v| !scope.admits(v)) {
        return Err(DeployMigrateError::from(EngineError::ApprovalNotScoped {
            version: unscoped.clone(),
        }));
    }
    Ok(())
}

/// Discover + apply the bundle's `.ir.json` creator artifacts (§5.2/§8.6).
///
/// For each `*.ir.json` file in `migrations_dir` (version-ordered by filename),
/// the fail-closed IR LOAD GATE runs ([`IrAuthor::load_and_lower`]: deserialize →
/// `ir_version` → `validate_ir` → server-stamped ownership → advisory checksum
/// hint), with the deploy-target dialect (Postgres) threaded in (§2.4.1), then
/// the validated, owned ops are LOWERED to migrations and applied under the SAME
/// Confined guard + least-priv migrator role as the `.sql` path. The ownership
/// registry + the FK-inline live-table set are introspected from the LIVE schema
/// (the per-app schema `"<app_id>"`, all tables owned by `app_id`).
///
/// An empty / IR-free directory is a clean no-op.
///
/// # Errors
/// [`DeployMigrateError::Ir`] on a fail-closed gate refusal / lower failure (a
/// creator fault → 422); [`DeployMigrateError::Snapshot`] / [`DeployMigrateError::IrRead`]
/// / [`DeployMigrateError::Apply`] on introspection / I/O / apply failure.
async fn apply_bundle_ir_migrations(
    backend: &PostgresBackend<'_>,
    app_id: &Uuid,
    migrations_dir: &Path,
    exec_cfg: &ExecutorConfig,
    guard_cfg: &zeroship_migrate::GuardConfig,
    approval: Approval,
    scope: &ApprovalScope,
    actor: &DeployActor,
) -> Result<MigrateOutcome, DeployMigrateError> {
    // Discover `*.ir.json` files, version-ordered by filename (deterministic).
    let mut ir_files: Vec<std::path::PathBuf> = Vec::new();
    let read = std::fs::read_dir(migrations_dir).map_err(|e| DeployMigrateError::IrRead {
        file: migrations_dir.display().to_string(),
        message: e.to_string(),
    })?;
    for entry in read {
        let entry = entry.map_err(|e| DeployMigrateError::IrRead {
            file: migrations_dir.display().to_string(),
            message: e.to_string(),
        })?;
        let path = entry.path();
        if path.is_file()
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".ir.json"))
        {
            ir_files.push(path);
        }
    }
    if ir_files.is_empty() {
        return Ok(MigrateOutcome::default());
    }
    ir_files.sort();

    let app = app_id.to_string();

    // Introspect the LIVE schema ONCE to SEED the IR ownership registry + the
    // FK-inline live-table set: every live table in the per-app schema is owned by
    // the deploying app, so the registry maps each live table → `app_id` and the
    // same key-set is the live-table set. Both are MUTABLE and ADVANCE as each
    // `.ir.json` applies (below) — a `createTable` in `0001.ir.json` makes that
    // table owned-by-the-deployer + live for `0002.ir.json`, so a same-deploy
    // migration that touches an earlier file's table resolves ownership / inlines
    // FKs correctly. (Pre-fix these were seeded once and never advanced, so a
    // legitimate multi-file deploy FAILED CLOSED on ownership / mis-deferred FKs.)
    let live = backend.snapshot_schema(exec_cfg).await?;
    let mut registry: BTreeMap<String, String> =
        live.tables.keys().map(|t| (t.clone(), app.clone())).collect();
    // The IR-path Lower's live facts: the live table set (FK inline-vs-defer) PLUS
    // the set of index NAMES the live catalog reports as UNIQUE. The latter is the
    // AUTHORITATIVE source for the `dropIndex` destructive/approval gate — a drop of
    // a live-unique index lowers `destructive + requires_approval` regardless of the
    // IR's advisory `unique` hint (a hostile/buggy author cannot under-declare it to
    // bypass the gate). Introspected the SAME way the differ's `render_drop_index`
    // reads `IndexSnapshot::unique`.
    let mut live_schema = LiveSchema {
        tables: live.tables.keys().cloned().collect(),
        unique_indexes: live
            .tables
            .values()
            .flat_map(|t| t.indexes.iter())
            .filter(|idx| idx.unique)
            .map(|idx| idx.name.clone())
            .collect(),
        // PR2 — carry the FULL introspected per-table column structure so the PG
        // `renameColumn` leg can reconcile the IR-carried column type against the
        // LIVE `from` column's actual `data_type` (the IR-path mirror of the
        // declarative `RenameHintTypeMismatch`): a rename whose IR `ty` disagrees
        // with the live column fails closed BEFORE any dual-write is authored, and a
        // rename whose live `from` column is absent fails closed rather than trust
        // the IR type alone. The whole live snapshot is already in hand, so this is
        // free; the PG expand-contract author still needs only `{from,to,ty}` to
        // author the sequence — the snapshot is consulted ONLY for the type gate.
        //
        // DEPLOY-WIRING HONESTY (PG leg): populating this makes the type-gate REACH
        // the live column on the production path — but a PG `renameColumn` still does
        // NOT COMPLETE through this routine deploy. Its expand-contract EXPAND backfill
        // is approval-gated; this path applies under `Approval::None` (see the loop
        // below), so a lowered rename is REFUSED at the approval gate
        // (`DeployMigrateError::OnlineExpand` ⇐ `OnlineError::Approval`), exactly like a
        // destructive op. The type reconciliation is wired; the APPROVED apply is the
        // out-of-band wave (gated on this PR). This is symmetric with the SQLite leg's
        // not-deploy-wired note below — see the module-level "Destructive +
        // approval-gated migrations" doc. Pinned by the control-plane e2e
        // `deploy_migrate_renamecolumn_refused_at_approval_gate_on_routine_deploy`.
        table_snapshots: live.tables.clone(),
        // Every live table in this per-app schema is owned by the deploying app
        // (the registry is seeded from exactly this set, below). Carried for
        // completeness; the PG rename leg does not consult it (cross-app authority
        // is enforced upstream by the IR-load gate's registry check), but populating
        // it keeps the live-facts bundle honest rather than fabricating ownership.
        table_ownership: live.tables.keys().map(|t| (t.clone(), app.clone())).collect(),
        // The SQLite SDK-schema `Value`s (`sqlite_schemas`) are NOT introspectable
        // from a PG catalog and are unused on this PG-targeted deploy path (a PG
        // rename lowers to expand-contract, never the SQLite 12-step rebuild). The
        // SQLite IR-rename rebuild leg is ENGINE-PROVEN (the `IrAuthor`/differ unit +
        // temp-file e2e in `ir_rename_pr2_sqlite.rs`) but is NOT YET DEPLOY-WIRED:
        // no production or dev/CLI path constructs a SQLite-dialect `LiveSchema` with
        // these facts today. Wiring a SQLite IR-deploy entry point (the dev-tier peer
        // of this PG introspection) is the CLI-rewire wave (gated on this PR). Until
        // then a SQLite-targeted IR rename would fail closed (no `table_snapshots`/
        // `sqlite_schemas`), never silently emit a wrong rebuild.
        sqlite_schemas: std::collections::BTreeMap::new(),
    };

    let engine = MigrationEngine::new();
    let mut applied: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    // PR7: the CONTRACT migrations of any online rename whose EXPAND completed this
    // deploy (PG leg only; the cross-deploy expand-contract partition, §2.0.2). Empty
    // on the routine `Approval::None` path (the expand is refused before producing one).
    let mut pending_contract: Vec<String> = Vec::new();
    // The lowered DDL `Migration`s across ALL `.ir.json` files this deploy — folded
    // into a SET-LEVEL integrity manifest (below) so the IR path has the SAME
    // traceability/anti-tamper seam the `.sql` path has (H2 follow-up, §8 point 5).
    let mut ir_lowered_all: Vec<zeroship_migrate::Migration> = Vec::new();

    // PR9a MED — hold the project advisory lock across the ENTIRE multi-file IR
    // loop, not per-file (§2.0.3(1) "the lock is held across the ENTIRE deploy").
    // Pre-fix each file applied with `LockMode::Acquire`, so the lock was taken AND
    // released PER FILE; between files it was free, letting a concurrent same-project
    // deploy interleave at file boundaries — a multi-file deploy was not atomic and
    // the cross-deploy interlock's "race-free by the held lock" argument did not
    // hold for the whole deploy. We now acquire ONCE here and drive every file with
    // `LockMode::AlreadyHeld` (skip the per-file acquire/release), releasing ONCE on
    // EVERY exit path below — mirroring `apply_declarative`'s H10 single-acquire /
    // single-release discipline. The same `pg_advisory_lock(hashtext(project))` key
    // means the whole IR deploy serializes against any concurrent same-project
    // deploy/rollback, while a DIFFERENT project (different key) never blocks.
    backend
        .acquire_project_lock(&exec_cfg.project_id)
        .await
        .map_err(|e| DeployMigrateError::from(EngineError::from(e)))?;

    // PR9c: the IR-leg journal actor — static `"deploy-ir"` on the routine path, or
    // `deploy-ir-approved:<approver>` on an operator-approved go-live (computed once).
    let ir_actor = actor.ir_actor();

    // Run the whole file loop under the held lock, capturing the result so the lock
    // is released on EVERY path (success/error/early-return) before we surface it.
    let loop_result: Result<(), DeployMigrateError> = async {
    for path in &ir_files {
        let file = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        let bytes = std::fs::read_to_string(path).map_err(|e| DeployMigrateError::IrRead {
            file: file.clone(),
            message: e.to_string(),
        })?;

        // The FAIL-CLOSED gate + GUARD-PER-FRAGMENT lower (§6.1.1), with the
        // deploy-target dialect (Postgres). Routing through `load_and_lower_guarded`
        // (not plain `load_and_lower`) means a guard denial reaches the creator with
        // the exact op-index + kind attribution, not a bare whole-`up` denial. It
        // returns ONE `AppliedPlan` per file (§2.0 / §5.2) whose `checksum` is the
        // dialect-neutral `Checksum::of_ir` over the op list and whose `Ddl` steps'
        // journaled checksums are stamped with that SAME op-list anchor (§5.3 drift
        // anchor — NOT the per-dialect rendered SQL).
        let author = IrAuthor::new(app.clone(), app.clone(), SqlDialect::Postgres);
        let lowered = author
            .load_and_lower_guarded(&bytes, &app, &registry, &live_schema, guard_cfg)
            .map_err(|source| DeployMigrateError::Ir { file: file.clone(), source })?;

        // Fold this file's lowered migrations into the set-level manifest tally.
        ir_lowered_all.extend(lowered.migrations());

        // Route the file's plan through the SINGLE shared plan orchestrator
        // `apply_plan` (§5.2 — realizing the PR0 AppliedPlan/apply_plan plumbing on
        // the IR path), NOT the flat `engine.apply`. `LockMode::AlreadyHeld` reuses
        // the WHOLE-deploy project advisory lock acquired before this loop (PR9a MED
        // — §2.0.3(1)); `apply_with_lock_backend` inside re-runs the Confined guard +
        // the destructive/approval gate under `Approval::None`, so a destructive op
        // is refused at deploy exactly like the `.sql` path. For PR1's pure-DDL ops
        // every step is `Ddl` (coalesced into one batch — byte-identical journaling
        // to the pre-fix `engine.apply` path). §2.0.3 — thread the artifact's full
        // op-list touched-set into the engine's cross-deploy pending-contract
        // interlock. The read-back inside the held project lock fail-closed refuses
        // ANY op (DDL or DML) touching a table with an outstanding online-rename
        // contract from a prior deploy (mapped to a deploy error → the creator's
        // 4xx). Because the lock is held for the WHOLE loop, that read-back sees a
        // consistent committed obligation set across all files, never a mid-deploy
        // interleave from a racing same-project deploy.
        // §2.0.4 — ALSO thread the artifact's plan-level `depends_on`, so a
        // dependent plan whose dependency's online-rename contract is still pending
        // is fail-closed refused at APPLY (with `DEPENDENCY_PENDING_CONTRACT`),
        // EVEN when this file touches a DIFFERENT table than the pending one (the
        // case the touched-table refusal does not cover — the §2.0.4 double-bind).
        let outcome = engine
            .apply_plan_with_touched_and_depends_scoped(
                &lowered.plan.steps,
                &lowered.touched_tables,
                &lowered.depends_on,
                approval,
                // PR9b: the per-version scope — `All` on the routine `Approval::None`
                // deploy (a destructive op is refused at the approval gate before scope
                // ever matters), the operator's reviewed version set on the approved
                // surface, so an unreviewed co-bundled destructive IR op (a `dropColumn`
                // / the C2 of an unrelated rename) is fail-closed refused with
                // `ApprovalNotScoped` even inside the approved deploy.
                scope,
                backend,
                exec_cfg,
                &ir_actor,
                LockMode::AlreadyHeld,
            )
            .await
            .map_err(DeployMigrateError::from)?;
        applied.extend(outcome.applied.applied);
        skipped.extend(outcome.applied.skipped);
        // PR7 go-live: a completed online-rename EXPAND surfaces its CONTRACT (C1/C2)
        // as pending — applied in a SUBSEQUENT approved deploy, not this one (§2.0.2).
        pending_contract.extend(
            outcome
                .pending_contract
                .iter()
                .map(|m| m.version.as_str().to_string()),
        );

        // ADVANCE the cross-file registry + live-set with THIS file's freshly-
        // created tables (now applied), so the NEXT `.ir.json` sees them as
        // owned-by-the-deployer + live.
        for t in lowered.created_tables {
            registry.entry(t.clone()).or_insert_with(|| app.clone());
            live_schema.tables.insert(t);
        }
    }
    Ok(())
    }
    .await;

    // RELEASE the whole-deploy project lock on EVERY path (PR9a MED). Surface the
    // loop's error first; a release failure is only logged (the lock auto-releases
    // on session end regardless), mirroring `apply_declarative`'s release-or-warn.
    if let Err(e) = backend.release_project_lock(&exec_cfg.project_id).await {
        tracing::warn!(
            error = %e,
            project = %exec_cfg.project_id,
            "deploy-migrate: failed to release whole-deploy project lock after IR loop (PR9a MED)"
        );
    }
    loop_result?;

    // SET-LEVEL integrity manifest over the discovered+lowered `.ir.json` set
    // (§8 point 5). Mirrors the `.sql` path's `compute_manifest` traceability log:
    // the IR path is the higher-risk creator/AI-authored surface, so it must emit
    // an equivalent set-level record (a reorder/insert/remove of IR files moves
    // this hash) for incident forensics — and so the H2 trusted-stamp follow-up has
    // an IR-side seam to thread an out-of-band expected hash through (see the
    // `apply_bundle_migrations` H2 note). Traceability only today: there is no
    // trusted build-side stamp to verify against yet, so we never pass an `expected`.
    if !ir_lowered_all.is_empty() {
        let ir_manifest = compute_manifest(&ir_lowered_all);
        tracing::info!(
            app_id = %app_id,
            ir_file_count = ir_files.len(),
            ir_migration_count = ir_lowered_all.len(),
            ir_manifest = %ir_manifest.as_str(),
            "deploy-migrate: computed .ir.json-set integrity manifest (traceability only — \
             no trusted build-side stamp to verify against yet; see H2 follow-up)"
        );
    }

    if !pending_contract.is_empty() {
        tracing::info!(
            app_id = %app_id,
            pending_contract = ?pending_contract,
            "deploy-migrate: online-rename EXPAND completed; CONTRACT (drop old column) is \
             pending a subsequent approved deploy (§2.0.2 cross-deploy expand-contract)"
        );
    }

    Ok(MigrateOutcome { applied, skipped, pending_contract })
}
