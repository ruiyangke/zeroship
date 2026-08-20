//! The one lowered-plan step VALUE that crosses the backend boundary.
//!
//! `zero_migrate::render::step` holds the rest of the lowered-plan vocabulary and
//! stays in the engine: `PlanStep`, `RenameStep`, `AlterPrimaryKeyStep` and
//! `DialectScope` reach `render::declarative`, `render::expand_contract` and
//! `model::backfill`, none of which a vendor crate may name. Only [`BindValue`] is
//! in the contract, because it is the currency of
//! [`DmlRenderer::bind_bytes`](crate::renderer::DmlRenderer::bind_bytes) — the
//! vendors disagree about the CARRIER of a binary value, which is a spelling
//! decision, so each one answers it.
//!
//! It is re-exported from `zero_migrate::render::step::BindValue`, the path every
//! existing caller uses.

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
