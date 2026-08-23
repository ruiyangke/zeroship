//! The lowered-plan step VALUES that cross the backend boundary.
//!
//! [`BindValue`] is here because it is the currency of
//! [`DmlRenderer::bind_bytes`](crate::renderer::DmlRenderer::bind_bytes) — the
//! vendors disagree about the CARRIER of a binary value, which is a spelling
//! decision, so each one answers it.
//!
//! The three STRUCTURED steps are here because a backend is what executes them.
//! [`AlterPrimaryKeyStep`], [`AlterColumnTypeStep`] and
//! [`SynchronizeIdentityStep`] deliberately stay structured until apply — each
//! must read the live catalog under the migration lock before it can spell its
//! statement — so they are `MigrationBackend` arguments, not rendered DDL. Each
//! carries a [`Migration`], an [`AlterPrimaryKeyAction`] and `String`s, and
//! nothing else.
//!
//! # What is still in the engine, and the ONE thing holding it
//!
//! `zero_migrate::render::step` keeps `PlanStep`, `RenameStep` and
//! `DialectScope`. `PlanStep::OnlineRename` carries a `RenameStep`, which carries
//! `render::declarative::TableRebuild`, which carries
//! `render::plan::TableRebuildSpec` — and that spec's `sequence_policy` field is
//! typed `zero_migrate_sqlite::SqliteSequencePolicy`. A vendor type cannot come
//! DOWN into the contract crate the vendors sit above, so the chain stops there
//! rather than at anything about `PlanStep` itself.
//!
//! Every item here is re-exported from `zero_migrate::render::step`, the path
//! every existing caller uses.

use zero_migrate_ir::ir::AlterPrimaryKeyAction;
use zero_migrate_ir::migration::Migration;

/// A typed scalar bound into a parameterized `PlanStep::Dml` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindValue {
    /// SQL `NULL`.
    Null,
    /// A boolean.
    Bool(bool),
    /// An exact 64-bit integer (the only integer domain the IR admits).
    Int(i64),
    /// A decimal/float carried as its canonical string form (numeric
    /// domain: no `f64` in the IR identity).
    Decimal(String),
    /// A UTF-8 text value.
    Text(String),
    /// Exact binary bytes. SQLite binds this variant directly as a BLOB. The
    /// PostgreSQL and MySQL renderers use a text bind wrapped in the dialect's
    /// base64 decoder because their schema-blind host seams infer text values.
    Bytes(Vec<u8>),
}

/// One explicit primary-key lifecycle mutation.
///
/// Unlike ordinary rendered DDL, this remains structured until apply so the
/// backend can verify the exact live primary key, identity facets, candidate
/// uniqueness, and inbound foreign keys while it holds the migration lock.
#[derive(Debug, Clone)]
pub struct AlterPrimaryKeyStep {
    /// Journal marker and approval metadata for this operation.
    pub migration: Migration,
    /// Effective target schema selected during lowering.
    pub schema: String,
    /// Bare target table name.
    pub table: String,
    /// Exact add/replace/drop contract authored in the IR.
    pub action: AlterPrimaryKeyAction,
}

/// One column retype that the target dialect spells by RESTATING the column.
///
/// MySQL has no `ALTER COLUMN … TYPE`. It has `MODIFY COLUMN`, which takes the
/// COMPLETE column definition and silently DISCARDS every facet the statement
/// leaves out — measured in `tests/mysql_engine/mysql_setcolumntype_restate.rs`,
/// where a bare `MODIFY COLUMN label varchar(128)` destroys the column's
/// `NOT NULL`, its `DEFAULT`, its `COLLATE` and its `COMMENT` in one statement,
/// with no warning.
///
/// So the statement cannot be written until the current definition is known, and
/// the current definition is not in the op: `Op::SetColumnType` carries one field.
/// This step stays STRUCTURED until apply for exactly the reason
/// [`AlterPrimaryKeyStep`] does — the backend reads `SHOW CREATE TABLE` under the
/// migration lock and restates the clause the server itself reports, so the answer
/// cannot go stale between the read and the `ALTER`.
///
/// WHY NOT AT LOWER TIME, which would need no new step at all: the apply path DOES
/// have the live column there (the engine's `LiveSchema::table_snapshots` is populated
/// from a real catalog read by `engine.rs` before it lowers). But
/// `ColumnSnapshot` is a LOSSY projection of a MySQL column — the same test
/// measures that `COLUMN_COMMENT` is never read, that `EXTRA` is read only for
/// `auto_increment` / `DEFAULT_GENERATED` so `ON UPDATE CURRENT_TIMESTAMP` cannot
/// be spelled, and that the generated-column facet is deliberately left `None`.
/// Restating from it would drop those four facets SILENTLY, which is strictly
/// worse than today's refusal.
#[derive(Debug, Clone)]
pub struct AlterColumnTypeStep {
    /// Journal marker and approval metadata for this operation.
    pub migration: Migration,
    /// Effective target schema selected during lowering.
    pub schema: String,
    /// Bare target table name.
    pub table: String,
    /// Bare target column name.
    pub column: String,
    /// The dialect-rendered target type, exactly as the renderer spells it.
    pub ddl_type: String,
}

/// One explicit import-time identity-generator reconciliation.
///
/// This remains structured until apply so the backend validates the live
/// identity/sequence association and performs a monotonic comparison while the
/// project migration lock is held. `writes_quiesced` is the operator's named
/// assertion; it is audit/status metadata, not something the engine can prove.
#[derive(Debug, Clone)]
pub struct SynchronizeIdentityStep {
    /// Journal marker and execution metadata for this operation.
    pub migration: Migration,
    /// Effective target schema selected during lowering.
    pub schema: String,
    /// Bare target table name.
    pub table: String,
    /// Bare identity column name.
    pub column: String,
    /// Named maintenance window or invariant asserted by the operator.
    pub writes_quiesced: String,
}
