//! Phase F Stage 4a — the platform-schema migrate path on the PUBLISHED engine.
//!
//! This module is the reusable core behind the `zeroship-platform-migrate` bin (and
//! the gated integration test). It rebuilds the retired in-tree `zeroship-migrate`
//! CLI's platform-apply behaviour entirely on the standalone `zero-migrate` engine:
//!
//! 1. discover `db/migrations-ts/*.ts` in filename order,
//! 2. AUTHOR each via zeroship-runtime's V8 + the standalone v1 recorder (the S2
//!    `author_v1_envelope` mechanism) → `{ ir_version:1, name, ops }`,
//! 3. LOWER each via `IrAuthor::load_and_lower_guarded` under a **Platform** guard
//!    (cross-schema `public`, the `citext`/`uuid-ossp` extension allowlist, roles /
//!    grants / functions — the widened operator posture the platform schema needs),
//! 4. APPLY each via `MigrationEngine::apply_plan_with_touched_and_depends_scoped`
//!    over `PostgresBackend::new_generic(&CompioPgSession)`, recording to the
//!    platform journal (`<project_schema>_migrations`).
//!
//! # The two operator-side Platform consumers (LOWER + APPLY)
//!
//! Both halves consume the same explicitly authored Platform policy:
//!
//! - LOWER — `GuardConfig::from_policy(platform_effective, SqlDialect::Postgres)`:
//!   every op the platform schema uses — `schema` / `extension` / `role` /
//!   `domain` / `sequence` / `createFunction` / `raw` / `table().trigger()` /
//!   `table().comment()` / `column().comment()` / `setRls` / `policy()` /
//!   `currentSetting` / `grant` / `revoke` / `dropFunction` / `table().drop()` —
//!   authors on the standalone v1 recorder and lowers under the Platform guard.
//!   The DSL/op support is COMPLETE — there is NO missing op type.
//! - APPLY — `ExecutorConfig::new(project_id, project_schema, platform_effective)`:
//!   `MigrationEngine`'s executor derives its first-pass guard from that same policy, so it
//!   admits the platform DDL (CREATE SCHEMA / roles / grants / cross-schema
//!   `public` / functions) the confined creator posture denies.

use std::path::{Path, PathBuf};

use zero_migrate::driver::SqlSession;
use zero_migrate::guard::GuardConfig;
use zero_migrate::{
    effective_policy_from_charter_toml, resolve_create_table_policy, Approval, ApprovalScope,
    ExecutorConfig, IrAuthor, LiveSchema, LockMode, MigrationBackend, MigrationEngine, MigrationId,
    MigrationIr, Phase, PlanStep, PostgresBackend, RenameStep, SqlDialect,
};
use zero_migrate_policy::EffectivePolicy as PdpPolicy;

/// The platform (author-owned, no-inject) ceiling. `resolve_create_table_policy` over
/// its composed effective policy is a pass-through (no injects) and carries the
/// complete privileged guard/executor posture.
const PLATFORM_CEILING_TOML: &str = include_str!("../policies/platform.policy.toml");

fn platform_effective(project_schema: &str) -> PdpPolicy {
    const PLACEHOLDER: &str = "\"__ZEROSHIP_PROJECT_SCHEMA__\"";
    assert_eq!(
        PLATFORM_CEILING_TOML.matches(PLACEHOLDER).count(),
        3,
        "platform charter must bind all three namespace grants"
    );
    let project_schema =
        serde_json::to_string(project_schema).expect("project schema serializes as TOML");
    let charter = PLATFORM_CEILING_TOML.replace(PLACEHOLDER, &project_schema);
    effective_policy_from_charter_toml(&charter).expect("embedded platform charter composes")
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    #[test]
    fn platform_charter_retains_the_project_and_public_schema_allowlist() {
        let policy = platform_effective("zeroship");
        let guard = GuardConfig::from_policy(policy, SqlDialect::Postgres);
        let Some(zero_migrate::SchemaScope::Allowlist(mut schemas)) = guard.schema_scope() else {
            panic!("platform charter must produce a schema allowlist");
        };
        schemas.sort();
        assert_eq!(schemas, vec!["public", "zeroship"]);
    }
}

use crate::CompioPgSession;

mod author;

/// The owner-app label stamped on every platform migration (ownership is enforced
/// UPSTREAM by the IR-load gate; for the platform schema the whole `zeroship`
/// schema is owned by this single synthetic platform app). Mirrors the in-tree
/// `PLATFORM_OWNER_APP` the retired CLI stamped.
const PLATFORM_OWNER_APP: &str = "zeroship_platform";

/// Inputs for a platform migrate run.
#[derive(Debug, Clone)]
pub struct PlatformMigrateConfig {
    /// Admin Postgres DSN (a superuser/owner on :5440 for platform DDL — roles,
    /// grants, RLS, functions).
    pub database_url: String,
    /// The `db/migrations-ts` directory holding the `*.ts` platform migrations.
    pub migrations_dir: PathBuf,
    /// The primary platform schema (conventionally `zeroship`).
    pub project_schema: String,
    /// The advisory-lock / journal project id (conventionally `zeroship`).
    pub project_id: String,
}

/// What a platform migrate run produced.
#[derive(Debug, Default)]
pub struct PlatformMigrateReport {
    /// Number of `.ts` files discovered + processed.
    pub files: usize,
    /// The versions/names newly applied this run.
    pub applied: Vec<String>,
    /// The versions/names skipped (already applied — idempotent re-run).
    pub skipped: Vec<String>,
}

/// A platform migrate error, attributed to a file where possible.
#[derive(Debug)]
pub enum PlatformMigrateError {
    /// A filesystem read of the migrations dir / a `.ts` file failed.
    Read { path: String, message: String },
    /// Connecting the compio session failed.
    Connect(String),
    /// Provisioning the primary schema failed.
    Provision(String),
    /// Creating, reading, or writing the per-file completion ledger failed.
    Ledger(String),
    /// A previously applied migration file no longer has the recorded source bytes.
    ChecksumMismatch {
        file: String,
        applied_checksum: String,
        current_checksum: String,
    },
    /// V8 authoring a `.ts` into a v1 envelope failed.
    Author { file: String, message: String },
    /// Folding the confined table-shape / resolving policy failed.
    Shape { file: String, message: String },
    /// The fail-closed load gate / guarded lower rejected the envelope. THIS is the
    /// arm that surfaces an engine op-support gap (an op the published engine
    /// genuinely cannot lower on Postgres).
    Lower { file: String, message: String },
    /// The engine's apply (executor guard / DDL execution / journal) failed.
    Apply { file: String, message: String },
    /// Reading the live schema snapshot failed.
    Snapshot(String),
}

impl std::fmt::Display for PlatformMigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read { path, message } => write!(f, "read {path}: {message}"),
            Self::Connect(m) => write!(f, "connect: {m}"),
            Self::Provision(m) => write!(f, "provision schema: {m}"),
            Self::Ledger(m) => write!(f, "platform migration completion ledger: {m}"),
            Self::ChecksumMismatch {
                file,
                applied_checksum,
                current_checksum,
            } => write!(
                f,
                "migration file {file} was edited after it was applied: source checksum \
                 mismatch (applied {applied_checksum}, current {current_checksum})"
            ),
            Self::Author { file, message } => write!(f, "author {file}: {message}"),
            Self::Shape { file, message } => write!(f, "table-shape {file}: {message}"),
            Self::Lower { file, message } => {
                write!(
                    f,
                    "lower {file}: {message} (possible engine op-support gap)"
                )
            }
            Self::Apply { file, message } => write!(f, "apply {file}: {message}"),
            Self::Snapshot(m) => write!(f, "snapshot live schema: {m}"),
        }
    }
}

impl std::error::Error for PlatformMigrateError {}

/// Live facts advanced between files: the table-ownership registry + the live
/// schema (so a later file sees tables created by an earlier one). Mirrors S3's
/// `PostgresIrApplyState`.
struct ApplyState {
    registry: std::collections::BTreeMap<String, String>,
    live_schema: LiveSchema,
}

/// One discovered migration and the exact source bytes used for both hashing and
/// V8 authoring.
struct MigrationFile {
    path: PathBuf,
    filename: String,
    source: Vec<u8>,
    checksum: String,
}

impl MigrationFile {
    fn source_str(&self) -> Result<&str, PlatformMigrateError> {
        std::str::from_utf8(&self.source).map_err(|e| PlatformMigrateError::Read {
            path: self.path.display().to_string(),
            message: format!("migration source is not UTF-8: {e}"),
        })
    }
}

/// Discover `db/migrations-ts/*.ts` files, deterministically ordered by filename.
fn discover_ts_files(dir: &Path) -> Result<Vec<PathBuf>, PlatformMigrateError> {
    let read = std::fs::read_dir(dir).map_err(|e| PlatformMigrateError::Read {
        path: dir.display().to_string(),
        message: e.to_string(),
    })?;
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in read {
        let entry = entry.map_err(|e| PlatformMigrateError::Read {
            path: dir.display().to_string(),
            message: e.to_string(),
        })?;
        let path = entry.path();
        if path.is_file()
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".ts"))
        {
            files.push(path);
        }
    }
    // Sort on the FILE NAME. The ordering that matters is the leading version in
    // each migration's filename; sorting the full path happens to agree only
    // while every entry shares one parent directory, which is a fact about the
    // current layout rather than about the ordering itself.
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    Ok(files)
}

fn load_migration_file(path: PathBuf) -> Result<MigrationFile, PlatformMigrateError> {
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("<unknown>")
        .to_string();
    let source = std::fs::read(&path).map_err(|e| PlatformMigrateError::Read {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    let checksum = zero_migrate::manifest_entry::sha256_hex(&source);
    Ok(MigrationFile {
        path,
        filename,
        source,
        checksum,
    })
}

/// Derive the STABLE per-file version anchor from a `db/migrations-ts` filename:
/// its leading `NNNNNNNNNNNNNN_` timestamp prefix (the standard
/// filename-as-version convention). This is the deterministic identity every
/// lowered step of the file is re-stamped from (see [`restamp_stable_versions`]),
/// so an idempotent re-run reproduces byte-identical journal versions and the
/// engine's already-applied skip matches across runs.
///
/// Falls back to the whole file stem when a filename has no digit prefix (never
/// the case for the committed platform migrations, all `NNNN_slug.ts`).
fn version_prefix_from_filename(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("migration");
    match stem.split_once('_') {
        Some((prefix, _)) if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit()) => {
            prefix.to_string()
        }
        _ => stem.to_string(),
    }
}

/// Derive the migration name label from a filename: strip the `.ts` extension and
/// the leading `NNNNNNNNNNNNNN_` timestamp prefix, leaving the human slug.
fn name_from_filename(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("migration");
    match stem.split_once('_') {
        Some((prefix, rest)) if prefix.chars().all(|c| c.is_ascii_digit()) && !rest.is_empty() => {
            rest.to_string()
        }
        _ => stem.to_string(),
    }
}

/// Seed the apply state from the live project schema.
async fn seed_state(
    session: &CompioPgSession,
    project_schema: &str,
    owner_app: &str,
) -> Result<ApplyState, PlatformMigrateError> {
    let live = zero_migrate::snapshot_schema(session, project_schema)
        .await
        .map_err(|e| PlatformMigrateError::Snapshot(e.to_string()))?;
    let registry: std::collections::BTreeMap<String, String> = live
        .tables
        .keys()
        .map(|t| (t.clone(), owner_app.to_string()))
        .collect();
    let live_schema = LiveSchema {
        tables: live.tables.keys().cloned().collect(),
        unique_indexes: live
            .tables
            .values()
            .flat_map(|t| t.indexes.iter())
            .filter(|idx| idx.unique)
            .map(|idx| idx.name.clone())
            .collect(),
        table_snapshots: live.tables.clone(),
        partitions: live.partitions.clone(),
        table_ownership: live
            .tables
            .keys()
            .map(|t| (t.clone(), owner_app.to_string()))
            .collect(),
        sqlite_schemas: std::collections::BTreeMap::new(),
        logical_columns: std::collections::BTreeMap::new(),
    };
    Ok(ApplyState {
        registry,
        live_schema,
    })
}

/// The immutable Platform authoring/lowering context threaded across all files.
struct LowerCtx {
    project_schema: String,
    owner_app: &'static str,
    guard_cfg: GuardConfig,
    policy: PdpPolicy,
}

impl LowerCtx {
    fn new(project_schema: &str) -> Self {
        let policy = platform_effective(project_schema);
        Self {
            project_schema: project_schema.to_string(),
            owner_app: PLATFORM_OWNER_APP,
            guard_cfg: GuardConfig::from_policy(policy.clone(), SqlDialect::Postgres),
            policy,
        }
    }
}

fn author_and_resolve_file(
    ctx: &LowerCtx,
    migration: &MigrationFile,
) -> Result<MigrationIr, PlatformMigrateError> {
    let file = migration.filename.clone();
    let source = migration.source_str()?;

    let name = name_from_filename(&migration.path);
    let envelope = author::author_v1_envelope(source, &name).map_err(|message| {
        PlatformMigrateError::Author {
            file: file.clone(),
            message,
        }
    })?;

    resolve_shape(&envelope, &ctx.policy, &ctx.project_schema, &file)
}

fn lower_resolved_file(
    ctx: &LowerCtx,
    state: &ApplyState,
    file: &str,
    resolved: &MigrationIr,
) -> Result<zero_migrate::render::lower::LoweredArtifact, PlatformMigrateError> {
    let bytes = serde_json::to_string(resolved).map_err(|e| PlatformMigrateError::Shape {
        file: file.to_string(),
        message: format!("re-serialize resolved IR envelope: {e}"),
    })?;

    let ir_author = IrAuthor::new(
        &ctx.project_schema,
        ctx.owner_app,
        SqlDialect::Postgres,
        &ctx.policy,
    );
    let ir_author = match ctx.guard_cfg.schema_scope() {
        Some(scope) => ir_author.with_schema_scope(scope),
        None => ir_author,
    };
    ir_author
        .load_and_lower_guarded(
            &bytes,
            ctx.owner_app,
            &state.registry,
            &state.live_schema,
            &ctx.guard_cfg,
        )
        .map_err(|e| PlatformMigrateError::Lower {
            file: file.to_string(),
            message: e.to_string(),
        })
}

/// Author, resolve, and lower one migration against the current live state.
fn author_and_lower_file(
    ctx: &LowerCtx,
    state: &ApplyState,
    migration: &MigrationFile,
) -> Result<(MigrationIr, zero_migrate::render::lower::LoweredArtifact), PlatformMigrateError> {
    let resolved = author_and_resolve_file(ctx, migration)?;
    let lowered = lower_resolved_file(ctx, state, &migration.filename, &resolved)?;
    Ok((resolved, lowered))
}

/// Per-file span of the order-preserving version space. Each file is assigned an
/// ordinal (its position in the deterministic sorted-filename order); its lowered
/// steps occupy `[file_ordinal * FILE_VERSION_STRIDE, +step_index]`. The stride is
/// a large power of two — comfortably above any per-file lowered step count
/// (the largest platform file lowers a few hundred steps) — so no two files' spans
/// overlap and the WITHIN-file step order is preserved. `file_ordinal` (<= a few
/// dozen) × the stride stays far below [`VERSION_CEILING`] (2^48), so the
/// numeric-version → id encoding never saturates the 48-bit ordering field.
const FILE_VERSION_STRIDE: u64 = 1 << 20;

/// The per-file completion ledger stored beside the engine journal.
pub const PLATFORM_MIGRATION_LEDGER_TABLE: &str = "platform_migration_files";

#[derive(Debug, Default)]
struct FileJournalState {
    completed: Vec<(u64, String)>,
    has_started: bool,
}

impl FileJournalState {
    fn completed_versions(&self) -> impl Iterator<Item = String> + '_ {
        self.completed.iter().map(|(_, version)| version.clone())
    }

    fn is_complete_legacy_range(&self, file_ordinal: usize) -> bool {
        if self.has_started || self.completed.is_empty() {
            return false;
        }
        let base = (file_ordinal as u64) * FILE_VERSION_STRIDE;
        self.completed
            .iter()
            .enumerate()
            .all(|(step_index, (numeric, version))| {
                let expected = base + step_index as u64;
                *numeric == expected
                    && version == zero_migrate::migration_id_for_version(expected).as_str()
            })
    }
}

fn journal_state_by_file(
    entries: &[zero_migrate::AppliedEntry],
    file_count: usize,
) -> Result<Vec<FileJournalState>, PlatformMigrateError> {
    let mut states: Vec<FileJournalState> = (0..file_count).map(|_| Default::default()).collect();
    let covered_end = (file_count as u64) * FILE_VERSION_STRIDE;

    for entry in entries {
        let version = MigrationId::parse(&entry.version).map_err(|e| {
            PlatformMigrateError::Ledger(format!(
                "journal contains invalid migration version {}: {e}",
                entry.version
            ))
        })?;
        let numeric = version.timestamp_ms();
        if numeric >= covered_end {
            continue;
        }
        let file_ordinal = (numeric / FILE_VERSION_STRIDE) as usize;
        match entry.phase {
            Phase::Completed => states[file_ordinal]
                .completed
                .push((numeric, entry.version.clone())),
            Phase::Started => states[file_ordinal].has_started = true,
        }
    }

    for state in &mut states {
        state.completed.sort_by_key(|(numeric, _)| *numeric);
    }
    Ok(states)
}

fn quote_pg_ident(ident: &str) -> Result<String, PlatformMigrateError> {
    if ident.is_empty() || ident.contains('\0') {
        return Err(PlatformMigrateError::Ledger(
            "metadata schema is not a valid PostgreSQL identifier".to_string(),
        ));
    }
    Ok(format!("\"{}\"", ident.replace('"', "\"\"")))
}

async fn completion_ledger_exists(
    session: &CompioPgSession,
    meta_schema: &str,
) -> Result<bool, PlatformMigrateError> {
    let binds = [
        meta_schema.to_string().into(),
        PLATFORM_MIGRATION_LEDGER_TABLE.into(),
    ];
    let row = session
        .query_one(
            "SELECT EXISTS (\
                 SELECT 1 FROM information_schema.tables \
                  WHERE table_schema = $1 AND table_name = $2\
             ) AS present",
            &binds,
        )
        .await
        .map_err(|e| PlatformMigrateError::Ledger(format!("detect ledger table: {e}")))?;
    row.try_get("present")
        .map_err(|e| PlatformMigrateError::Ledger(format!("decode ledger table probe: {e}")))
}

async fn load_completion_ledger(
    session: &CompioPgSession,
    meta_schema: &str,
) -> Result<std::collections::BTreeMap<String, String>, PlatformMigrateError> {
    let meta = quote_pg_ident(meta_schema)?;
    let rows = session
        .query(
            &format!(
                "SELECT filename, checksum FROM {meta}.{PLATFORM_MIGRATION_LEDGER_TABLE} \
                 ORDER BY filename"
            ),
            &[],
        )
        .await
        .map_err(|e| PlatformMigrateError::Ledger(format!("read ledger rows: {e}")))?;
    let mut ledger = std::collections::BTreeMap::new();
    for row in rows {
        let filename: String = row
            .try_get("filename")
            .map_err(|e| PlatformMigrateError::Ledger(format!("decode ledger filename: {e}")))?;
        let checksum: String = row
            .try_get("checksum")
            .map_err(|e| PlatformMigrateError::Ledger(format!("decode ledger checksum: {e}")))?;
        ledger.insert(filename, checksum);
    }
    Ok(ledger)
}

async fn initialize_completion_ledger(
    session: &CompioPgSession,
    meta_schema: &str,
    files: &[MigrationFile],
    journal: &[FileJournalState],
) -> Result<(), PlatformMigrateError> {
    let meta = quote_pg_ident(meta_schema)?;
    session
        .batch("BEGIN")
        .await
        .map_err(|e| PlatformMigrateError::Ledger(format!("begin ledger setup: {e}")))?;

    let result: Result<(), PlatformMigrateError> = async {
        session
            .batch(&format!(
                "CREATE TABLE {meta}.{PLATFORM_MIGRATION_LEDGER_TABLE} (\
                     filename TEXT PRIMARY KEY, \
                     applied_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                     checksum TEXT NOT NULL CHECK (checksum ~ '^[0-9a-f]{{64}}$')\
                 )"
            ))
            .await
            .map_err(|e| PlatformMigrateError::Ledger(format!("create ledger table: {e}")))?;

        for (file_ordinal, migration) in files.iter().enumerate() {
            if !journal[file_ordinal].is_complete_legacy_range(file_ordinal) {
                continue;
            }
            let binds = [
                migration.filename.clone().into(),
                migration.checksum.clone().into(),
            ];
            session
                .exec(
                    &format!(
                        "INSERT INTO {meta}.{PLATFORM_MIGRATION_LEDGER_TABLE} \
                             (filename, checksum) VALUES ($1, $2)"
                    ),
                    &binds,
                )
                .await
                .map_err(|e| {
                    PlatformMigrateError::Ledger(format!(
                        "backfill ledger row for {}: {e}",
                        migration.filename
                    ))
                })?;
        }
        Ok(())
    }
    .await;

    match result {
        Ok(()) => session
            .batch("COMMIT")
            .await
            .map_err(|e| PlatformMigrateError::Ledger(format!("commit ledger setup: {e}"))),
        Err(error) => {
            let _ = session.batch("ROLLBACK").await;
            Err(error)
        }
    }
}

async fn insert_completion_ledger_row(
    session: &CompioPgSession,
    meta_schema: &str,
    migration: &MigrationFile,
) -> Result<(), PlatformMigrateError> {
    let meta = quote_pg_ident(meta_schema)?;
    let binds = [
        migration.filename.clone().into(),
        migration.checksum.clone().into(),
    ];
    let inserted = session
        .exec(
            &format!(
                "INSERT INTO {meta}.{PLATFORM_MIGRATION_LEDGER_TABLE} \
                     (filename, checksum) VALUES ($1, $2)"
            ),
            &binds,
        )
        .await
        .map_err(|e| {
            PlatformMigrateError::Ledger(format!(
                "record completed file {}: {e}",
                migration.filename
            ))
        })?;
    if inserted != 1 {
        return Err(PlatformMigrateError::Ledger(format!(
            "record completed file {} affected {inserted} rows",
            migration.filename
        )));
    }
    Ok(())
}

/// Re-stamp EVERY lowered migration's journal `version` with a DETERMINISTIC,
/// ORDER-PRESERVING id anchored on the migration's `db/migrations-ts` FILENAME (its
/// deterministic sorted position + the step's position within the file) so a re-run
/// reproduces byte-identical versions and the engine's already-applied skip matches
/// across runs.
///
/// # Why this is needed
///
/// The published engine's declarative builder mints ADDITIVE-DDL migration
/// versions with `MigrationId::generate()` — a fresh, RANDOM `mig_…` on every
/// lowering (only SCOPE-GATED destructive DDL is re-stamped with a deterministic
/// `ddl_step_version`). The engine's already-applied skip keys on the journal
/// `version`, so a second run of THIS one-shot re-lowers the identical `.ts` into a
/// migration with a DIFFERENT random version — the journal has no matching row, the
/// step is treated as pending, re-executes, and fails (`type "account_state"
/// already exists`). That regresses the retired CLI's re-runnable one-shot posture
/// the docker-compose `migrate` service depends on.
///
/// # What this does
///
/// For one file's lowered plan, each migration is re-stamped to
/// `migration_id_for_version(file_ordinal * FILE_VERSION_STRIDE + step_index)`. That
/// engine helper places the numeric version in the id's high 48 bits, so ASCENDING
/// id string order == ascending numeric order. The engine runs a pending batch in
/// version order (`order_pending` degrades to ascending-version order when there are
/// no `depends_on` edges), so this monotonic, plan-order-preserving numbering keeps
/// each file's steps executing in their lowered order (e.g. a table's CREATE before
/// the CHECK/FK that references it). A raw `MigrationId::derive` content-hash id
/// would be deterministic but NOT order-preserving, reshuffling the batch into hash
/// order and breaking those intra-file ordering dependencies.
///
/// The numbering is a pure function of (sorted filename position, lowered step
/// index), both deterministic across runs, so the same committed set re-lowers to
/// byte-identical versions. The migration `checksum` is over
/// `up`/`down`/`flags`/`owner_app`/`depends_on` (NOT the version), so re-stamping
/// never triggers checksum drift.
///
/// Intra-file `depends_on` edges (e.g. a deferred FK migration depending on its
/// table-create migration) are remapped through the old→new version map in the SAME
/// pass, so the engine's `order_pending` topological sort still resolves.
///
/// The platform migrations are pure DDL, so every step is a `PlanStep::Ddl`. Any
/// other step kind is an unexpected shape for the platform path; we fail closed
/// rather than silently leave a non-deterministic (or unremapped) version behind.
fn restamp_stable_versions(
    lowered: &mut zero_migrate::render::lower::LoweredArtifact,
    file_ordinal: usize,
    version_prefix: &str,
) -> Result<(), PlatformMigrateError> {
    let base = (file_ordinal as u64) * FILE_VERSION_STRIDE;

    // Pass 1 — assign each Ddl migration a deterministic, order-preserving new
    // version and record the old→new mapping (for the depends_on remap). Fail closed
    // on any non-Ddl step (unexpected for the pure-DDL platform path).
    let mut remap: std::collections::HashMap<String, MigrationId> =
        std::collections::HashMap::new();
    for (step_index, step) in lowered.plan.steps.iter().enumerate() {
        match step {
            PlanStep::Ddl(m) => {
                let version = base + step_index as u64;
                debug_assert!(
                    (step_index as u64) < FILE_VERSION_STRIDE,
                    "platform file {version_prefix} lowered more steps than the version stride"
                );
                remap.insert(
                    m.version.as_str().to_string(),
                    zero_migrate::migration_id_for_version(version),
                );
            }
            PlanStep::Dml { .. }
            | PlanStep::Backfill { .. }
            | PlanStep::AlterPrimaryKey(_)
            | PlanStep::SynchronizeIdentity(_)
            | PlanStep::OnlineRename(RenameStep::PgExpandContract(_))
            | PlanStep::OnlineRename(RenameStep::SqliteRebuild(_)) => {
                return Err(PlatformMigrateError::Apply {
                    file: version_prefix.to_string(),
                    message: format!(
                        "unexpected non-DDL plan step at index {step_index} in a platform \
                         migration (only pure DDL is supported); cannot stamp a stable version"
                    ),
                });
            }
        }
    }

    // Pass 2 — rewrite each migration's version + remap its depends_on edges.
    for step in &mut lowered.plan.steps {
        if let PlanStep::Ddl(m) = step {
            if let Some(new_version) = remap.get(m.version.as_str()) {
                m.version = new_version.clone();
            }
            for dep in &mut m.depends_on {
                if let Some(new_dep) = remap.get(dep.as_str()) {
                    *dep = new_dep.clone();
                }
            }
        }
    }

    // Keep the plan's outer version marker in agreement with its first step (the
    // engine borrows `plan.version` from the first step's version).
    if let Some(PlanStep::Ddl(first)) = lowered.plan.steps.first() {
        lowered.plan.version = first.version.clone();
    }

    Ok(())
}

/// Fold the tables an artifact creates into the live state so a later file sees
/// them (the cross-file registry + live-set advance).
fn advance_state(state: &mut ApplyState, owner_app: &str, created_tables: &[String]) {
    for t in created_tables {
        state
            .registry
            .entry(t.clone())
            .or_insert_with(|| owner_app.to_string());
        state.live_schema.tables.insert(t.clone());
    }
}

fn advance_authored_logical_columns(
    ctx: &LowerCtx,
    state: &mut ApplyState,
    file: &str,
    resolved: &MigrationIr,
) -> Result<(), PlatformMigrateError> {
    state
        .live_schema
        .advance_logical_columns(resolved, SqlDialect::Postgres, &ctx.project_schema, None)
        .map_err(|e| PlatformMigrateError::Lower {
            file: file.to_string(),
            message: format!("advance authored logical columns: {e}"),
        })
}

/// AUTHOR + LOWER every `db/migrations-ts/*.ts` file in order, threading the live
/// state across files, WITHOUT applying. This is the fully-reachable proof half of
/// the pipeline: it exercises the V8 authoring + the published engine's
/// fail-closed load gate + Platform-guarded lower for every platform migration,
/// over an EMPTY starting `state` (no DB required). Returns the per-file lowered
/// artifacts so a caller can inspect the generated plan steps.
///
/// Used by the gated integration test to assert every platform migration authors
/// and lowers cleanly on the standalone (v1) engine.
pub fn author_and_lower_all(
    migrations_dir: &Path,
    project_schema: &str,
) -> Result<Vec<(String, zero_migrate::render::lower::LoweredArtifact)>, PlatformMigrateError> {
    let files = discover_ts_files(migrations_dir)?
        .into_iter()
        .map(load_migration_file)
        .collect::<Result<Vec<_>, _>>()?;
    let ctx = LowerCtx::new(project_schema);
    // Empty live state: authoring + lowering do not consult the DB. Cross-file
    // table references (e.g. functions/triggers/grants targeting tables created in
    // an earlier file) resolve through the advancing registry/live-set.
    let mut state = ApplyState {
        registry: std::collections::BTreeMap::new(),
        live_schema: LiveSchema::default(),
    };
    let mut out = Vec::with_capacity(files.len());
    for migration in &files {
        let file = migration.filename.clone();
        let (resolved, lowered) = author_and_lower_file(&ctx, &state, migration)?;
        advance_authored_logical_columns(&ctx, &mut state, &file, &resolved)?;
        advance_state(&mut state, ctx.owner_app, &lowered.created_tables);
        out.push((file, lowered));
    }
    Ok(out)
}

/// Run all platform migrations end to end under one project advisory lock.
pub async fn run_platform_migrations(
    cfg: &PlatformMigrateConfig,
) -> Result<PlatformMigrateReport, PlatformMigrateError> {
    let files = discover_ts_files(&cfg.migrations_dir)?
        .into_iter()
        .map(load_migration_file)
        .collect::<Result<Vec<_>, _>>()?;

    // ── driver: open a native compio session + provision the primary schema ──
    let session = CompioPgSession::connect(&cfg.database_url)
        .await
        .map_err(|e| PlatformMigrateError::Connect(e.to_string()))?;
    session
        .batch(&format!(
            "CREATE SCHEMA IF NOT EXISTS \"{}\"",
            cfg.project_schema
        ))
        .await
        .map_err(|e| PlatformMigrateError::Provision(e.to_string()))?;

    let ctx = LowerCtx::new(&cfg.project_schema);
    let owner_app = ctx.owner_app;

    // The executor consumes the exact same authored policy used for lowering.
    let exec_cfg = ExecutorConfig::new(
        cfg.project_id.clone(),
        cfg.project_schema.clone(),
        ctx.policy.clone(),
    );

    let backend = PostgresBackend::new_generic(&session);
    let engine = MigrationEngine::new();

    backend
        .ensure_journal(&exec_cfg)
        .await
        .map_err(|e| PlatformMigrateError::Ledger(format!("initialize engine journal: {e}")))?;
    backend
        .acquire_project_lock(&exec_cfg)
        .await
        .map_err(|e| PlatformMigrateError::Apply {
            file: "<platform runner>".to_string(),
            message: format!("acquire project lock: {e}"),
        })?;

    let result: Result<PlatformMigrateReport, PlatformMigrateError> = async {
        let journal_entries = zero_migrate::applied(&session, &exec_cfg)
            .await
            .map_err(|e| PlatformMigrateError::Ledger(format!("read engine journal: {e}")))?;
        let journal = journal_state_by_file(&journal_entries, files.len())?;
        if !completion_ledger_exists(&session, &exec_cfg.pg.meta_schema).await? {
            initialize_completion_ledger(&session, &exec_cfg.pg.meta_schema, &files, &journal)
                .await?;
        }
        let mut ledger = load_completion_ledger(&session, &exec_cfg.pg.meta_schema).await?;

        let mut state = seed_state(&session, &cfg.project_schema, owner_app).await?;
        let mut report = PlatformMigrateReport {
            files: files.len(),
            ..Default::default()
        };

        for (index, migration) in files.iter().enumerate() {
            let file = migration.filename.clone();
            if let Some(applied_checksum) = ledger.get(&file) {
                if applied_checksum != &migration.checksum {
                    return Err(PlatformMigrateError::ChecksumMismatch {
                        file,
                        applied_checksum: applied_checksum.clone(),
                        current_checksum: migration.checksum.clone(),
                    });
                }
                // Rebuild authored contracts without lowering against the current
                // catalog or submitting any work to the executor.
                //
                // This must stay a FULL replay of the file's ops. Narrowing it to
                // a collector over table and column ops looks like an easy saving
                // and is wrong: the engine maintains the candidate-key lifecycle
                // in its create-index, drop-index, add-constraint, drop-constraint
                // and alter-primary-key arms too. A column made referenceable by a
                // later unique index in a skipped file would be dropped from the
                // rebuilt contracts, and a foreign key pointing at it would then be
                // rejected for a reason nothing in this file explains.
                let resolved = author_and_resolve_file(&ctx, migration)?;
                advance_authored_logical_columns(&ctx, &mut state, &file, &resolved)?;
                report.skipped.extend(journal[index].completed_versions());
                continue;
            }

            // AUTHOR (V8) + LOWER (Platform guard) only when this filename has no
            // durable completion record.
            let (resolved, mut lowered) = author_and_lower_file(&ctx, &state, migration)?;

            // Re-stamp every lowered step's journal version DETERMINISTICALLY from this
            // file's sorted position + step order. `index` is the file's deterministic
            // sorted-filename ordinal.
            let version_prefix = version_prefix_from_filename(&migration.path);
            restamp_stable_versions(&mut lowered, index, &version_prefix)?;

            // The engine reports the full completed journal as skipped, so retain only
            // the versions assigned to this file.
            let file_versions: std::collections::HashSet<String> = lowered
                .plan
                .steps
                .iter()
                .filter_map(|s| match s {
                    PlanStep::Ddl(m) => Some(m.version.as_str().to_string()),
                    _ => None,
                })
                .collect();

            let outcome = engine
                .apply_plan_with_touched_and_depends_scoped(
                    &lowered.plan.steps,
                    &lowered.touched_tables,
                    &lowered.depends_on,
                    // Operator-side unattended platform apply: the committed platform
                    // schema is trusted, and a destructive migration (e.g.
                    // `drop_metering_exports` DROP TABLE … CASCADE) is a deliberate,
                    // reviewed part of that committed set. This is the docker-compose
                    // one-shot posture — auto-approved, scope = all — matching the
                    // retired in-tree CLI's platform-apply behaviour.
                    Approval::Approved,
                    &ApprovalScope::All,
                    &backend,
                    &exec_cfg,
                    "phase-f-stage4a",
                    LockMode::AlreadyHeld,
                    None,
                )
                .await
                .map_err(|e| PlatformMigrateError::Apply {
                    file: file.clone(),
                    message: e.to_string(),
                })?;

            let applied: Vec<String> = outcome
                .applied
                .applied
                .into_iter()
                .filter(|v| file_versions.contains(v))
                .collect();
            let skipped: Vec<String> = outcome
                .applied
                .skipped
                .into_iter()
                .filter(|v| file_versions.contains(v))
                .collect();
            let completed: std::collections::HashSet<String> =
                zero_migrate::applied(&session, &exec_cfg)
                    .await
                    .map_err(|e| {
                        PlatformMigrateError::Ledger(format!(
                            "verify completed journal range for {file}: {e}"
                        ))
                    })?
                    .into_iter()
                    .filter(|entry| entry.phase == Phase::Completed)
                    .map(|entry| entry.version)
                    .collect();
            if !file_versions
                .iter()
                .all(|version| completed.contains(version))
            {
                return Err(PlatformMigrateError::Ledger(format!(
                    "engine returned success for {file} without completing every file version"
                )));
            }

            // Re-introspect the live catalog after each applied file so the next file
            // lowers against complete column snapshots while retaining authored value
            // formats that cannot be reconstructed from the physical catalog.
            let logical_columns = std::mem::take(&mut state.live_schema.logical_columns);
            let mut refreshed = seed_state(&session, &cfg.project_schema, owner_app).await?;
            refreshed.live_schema.logical_columns = logical_columns;
            advance_authored_logical_columns(&ctx, &mut refreshed, &file, &resolved)?;
            state = refreshed;

            insert_completion_ledger_row(&session, &exec_cfg.pg.meta_schema, migration).await?;
            ledger.insert(file.clone(), migration.checksum.clone());
            report.applied.extend(applied);
            report.skipped.extend(skipped);
        }

        Ok(report)
    }
    .await;

    let release = backend.release_project_lock(&exec_cfg).await;
    match (result, release) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(PlatformMigrateError::Apply {
            file: "<platform runner>".to_string(),
            message: format!("release project lock: {error}"),
        }),
    }
}

/// Fold the Platform table-shape policy into an envelope's createTable ops. The
/// platform posture is author-owned (no injects), so this is a pass-through today; it
/// stays on the shared resolver so a future platform ceiling that DOES inject a shape
/// is honoured without a code change.
fn resolve_shape(
    envelope: &str,
    policy: &PdpPolicy,
    default_schema: &str,
    file: &str,
) -> Result<MigrationIr, PlatformMigrateError> {
    let ir: MigrationIr =
        serde_json::from_str(envelope).map_err(|e| PlatformMigrateError::Shape {
            file: file.to_string(),
            message: format!("deserialize IR envelope: {e}"),
        })?;
    let resolved = resolve_create_table_policy(&ir, policy, default_schema).map_err(|e| {
        PlatformMigrateError::Shape {
            file: file.to_string(),
            message: format!("resolve table-shape policy: {e}"),
        }
    })?;
    Ok(resolved)
}
