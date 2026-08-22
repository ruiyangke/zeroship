//! Backend-owned table-rebuild policy and its neutral input vocabulary.
//!
//! Core decides *when* a backend's registered structural strategy selects a
//! rebuild and assembles the resulting migration plan. The backend that owns the
//! rebuild grammar decides how stored table text is retargeted, which structural
//! differences require rebuilding, and how a pure rename preserves catalog text.

use crate::error::{DeclarativeError, IrLowerError};
use crate::snapshot::TableSnapshot;
use zero_migrate_ir::dialect::DialectId;
use zero_migrate_ir::ir::Op;

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
