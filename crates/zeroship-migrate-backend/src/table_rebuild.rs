//! Backend-owned table-rebuild policy and its neutral input vocabulary.
//!
//! Core decides *when* a backend's registered structural strategy selects a
//! rebuild and assembles the resulting migration plan. The backend that owns the
//! rebuild grammar decides how stored table text is retargeted, which structural
//! differences require rebuilding, and how a pure rename preserves catalog text.

use crate::error::{DeclarativeError, IrLowerError};
use crate::snapshot::TableSnapshot;
use zeroship_migrate_ir::dialect::DialectId;
use zeroship_migrate_ir::ir::Op;
use zeroship_migrate_ir::migration::Migration;

/// A rename hint that has been **verified** against the desired/live snapshots
/// (matched an actual drop+add pair with identical types). The diff routes each
/// one through the selected backend's rename strategy. `ty` is the shared
/// `information_schema` data-type spelling of the two matched columns.
#[derive(Debug, Clone)]
pub struct ResolvedRename {
    /// The table containing the verified rename.
    pub table: String,
    /// The live column name.
    pub from: String,
    /// The desired column name.
    pub to: String,
    /// The matched information-schema type spelling.
    pub ty: String,
}

/// The one policy-injection fact a rebuild renderer consumes.
///
/// This keeps the backend contract below the engine's policy-resolution model:
/// the engine implements the projection for its resolved injection type, while a
/// vendor sees only the already-resolved primary-key columns.
pub trait InjectedPrimaryKey {
    /// The already-resolved injected primary-key columns, when the policy pins
    /// one for this table.
    fn primary_key(&self) -> Option<&[String]>;
}

/// Vendor-owned table-rebuild decisions and stored-DDL rewrites.
///
/// Every method is required. A backend that does not support table rebuilds
/// registers no policy through [`crate::schema::SchemaRenderer`]; a backend that
/// does support them supplies every answer itself rather than inheriting a shared
/// spelling or parser.
pub trait TableRebuildPolicy: std::fmt::Debug + Sync {
    /// Refuse an authored rename sequence this backend cannot rebuild safely
    /// within one migration.
    fn refuse_repeat_column_rename_target(
        &self,
        dialect: &DialectId,
        ops: &[Op],
    ) -> Result<(), IrLowerError>;

    /// Recognize the one verified rename shape whose stored CREATE text can be
    /// preserved through the rebuild.
    fn pure_column_rename<'a>(
        &self,
        live: &TableSnapshot,
        desired: &TableSnapshot,
        renames: &[&'a ResolvedRename],
    ) -> Option<&'a ResolvedRename>;

    /// Retarget the referenced table token in one foreign-key definition.
    fn retarget_foreign_key_definition(&self, definition: &str, target: &str) -> Option<String>;

    /// Retarget self-referential field definitions in the SDK-shaped schema
    /// projection used to render a replacement table.
    fn retarget_self_references_in_schema(
        &self,
        schema: &mut serde_json::Value,
        table: &str,
        target: &str,
    );

    /// Rebuild a pure rename from the backend's catalog-stored CREATE text.
    fn stored_create_for_pure_rename(
        &self,
        table: &str,
        temporary_table: &str,
        snapshot: &TableSnapshot,
    ) -> Result<String, DeclarativeError>;

    /// Recover an authored table-level primary-key clause not supplied by the
    /// active policy injection.
    fn authored_primary_key_clause(
        &self,
        table: &str,
        snapshot: &TableSnapshot,
        inject: &dyn InjectedPrimaryKey,
    ) -> Result<Option<String>, DeclarativeError>;

    /// Insert one table constraint into an already-rendered CREATE statement.
    fn append_table_constraint(
        &self,
        table: &str,
        create_sql: &str,
        constraint: &str,
    ) -> Result<String, DeclarativeError>;

    /// Return the first backend-owned reason an existing table must be rebuilt,
    /// or `None` when every observed change has a native path.
    fn existing_table_needs_rebuild(
        &self,
        table: &str,
        live: &TableSnapshot,
        desired: &TableSnapshot,
        renames: &[&ResolvedRename],
    ) -> Option<String>;
}

/// How a table rebuild treats the table's AUTOINCREMENT-style high-water mark.
///
/// NEUTRAL on purpose, and it did not used to be. This field on
/// [`TableRebuildSpec`] was typed `zeroship_migrate_sqlite::SqliteSequencePolicy` -
/// a type from a crate that sits ABOVE this one - which is what kept the whole
/// lowered-plan vocabulary (`TableRebuildSpec`, `TableRebuild`, `RenameStep`,
/// `PlanStep`) stranded in the engine. One field inverted the dependency for all
/// four.
///
/// The vendor still owns the BEHAVIOUR. This says only which of the two
/// transitions a rebuild is performing; what a high-water mark IS, where it is
/// stored, how it is captured and how it is restored are the backend's, and
/// `zero-migrate-sqlite` converts this into its own `SqliteSequencePolicy` at its
/// own boundary. A backend with no such counter ignores it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SequenceHighWaterPolicy {
    /// Carry the pre-rebuild high-water mark across the rebuild, so generated
    /// values continue from where they left off. The ordinary case.
    #[default]
    Preserve,
    /// Do not carry it across. The explicit identity-removal transition, for a
    /// rebuild whose validated target no longer has generated-identity.
    Reset,
}

/// The fully-resolved specification for ONE table rebuild.
#[derive(Debug, Clone)]
pub struct TableRebuildSpec {
    /// The existing table being rebuilt (the final name; the new table is renamed
    /// INTO this).
    pub table: String,
    /// The temp name the new table is created under, then renamed FROM.
    pub tmp_table: String,
    /// The new table's `CREATE TABLE <tmp> (...)` DDL.
    pub new_table_create: String,
    /// The columns to copy from the old table into the new one, as `(dest, src)`
    /// pairs of BARE identifiers.
    pub copy_columns: Vec<(String, String)>,
    /// EXTRA dependent DDL to replay AFTER the rename.
    pub recreate_objects: Vec<String>,
    /// Pure column renames to apply after the old table's captured indexes and
    /// triggers have been replayed. The stored-DDL rebuild path creates and
    /// copies the byte-faithful pre-rename shape first, then delegates the
    /// identifier rewrite to SQLite's own `ALTER TABLE ... RENAME COLUMN`
    /// parser so CHECKs, generated expressions, indexes, and triggers follow the
    /// rename without a lossy engine-side SQL rewrite.
    pub column_renames: Vec<(String, String)>,
    /// BARE names of columns being DROPPED by this rebuild.
    pub dropped_columns: Vec<String>,
    /// Whether the old table's `AUTOINCREMENT` high-water mark survives the
    /// rebuild. Ordinary rebuilds use
    /// [`SequenceHighWaterPolicy::Preserve`].
    pub sequence_policy: SequenceHighWaterPolicy,
    /// A human-readable description of what change drove the rebuild.
    pub reason: String,
}

impl TableRebuildSpec {
    /// The engine-chosen temp-table name for `table`.
    #[must_use]
    pub fn tmp_name(table: &str) -> String {
        format!("{table}__zero_migrate_rebuild")
    }
}

/// one SQLite 12-step table rebuild: the execution [`TableRebuildSpec`]
/// plus the [`Migration`] that carries its checksum / journal identity / approval
/// flags. The differ produces these for the existing-table ops SQLite cannot ALTER
/// natively.
///
/// NOTE: the engine DRIVES these rebuilds. `MigrationEngine::plan_declarative`
/// CARRIES the rebuilds into its `DeclarativeDeployPlan`, and the now-generic
/// `MigrationEngine::apply_declarative` drives each through
/// `MigrationBackend::rebuild_one` under the destructive/approval gate (the journal
/// migration is `destructive + requires_approval`, so an un-approved rebuild is
/// refused before any DDL). The old `plan_declarative` fail-close - a
/// `DeclarativeError` arm that refused the rebuild the engine now drives, deleted
/// after it outlived its last constructor - is gone. The direct, executor-internal
/// `SqliteBackend::rebuild_one` seam remains for tests; the engine path is the gated
/// production drive.
#[derive(Debug, Clone)]
pub struct TableRebuild {
    /// The journal migration: its `version` is the rebuild's identity, its
    /// `checksum` certifies the rebuild, and its flags (`destructive = true,
    /// requires_approval = true`) route it through the gate. Its `up` carries the
    /// new-table CREATE plus any newly planned schema-object DDL for
    /// inspection/checksum; the actual apply is structured (the `spec`), NOT a
    /// plain `up` execution.
    pub migration: Migration,
    /// The fully-resolved 12-step rebuild specification the backend executes.
    pub spec: TableRebuildSpec,
}
