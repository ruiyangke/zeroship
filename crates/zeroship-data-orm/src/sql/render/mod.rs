//! Render typed plans as SQL plus native bind parameters.
//!
//! Canonical plan ordering keeps equivalent queries stable across input insertion
//! orders. Values remain parameters so they do not change statement text.
//! `ValueFormat` requires each dialect to supply its own placeholder and expression
//! spellings.

pub mod postgres;

use crate::sql::literal::Literal;

/// Every spelling that is a dialect's rather than the grammar's.
///
/// Most of them are placeholders, and the two that are not
/// ([`ValueFormat::current_timestamp_expr`])
/// arrived with the write family. They are here rather than as constants in
/// [`postgres`] for the reason the trait exists: one place to read what a
/// dialect does, and a compiler error rather than a silent inheritance when a
/// second dialect is added.
///
/// **No default methods, deliberately.** A method with a default is a spelling
/// a new dialect inherits without anyone deciding it should, which is how one
/// backend ends up quietly emulating another - the failure mode SC-3's decision
/// 2 rejects. Copied from `SchemaRenderer` (`query.rs:124-130`) rather than
/// invented.
///
/// A full implementation compiles:
///
/// ```
/// use zeroship_data_orm::sql::render::ValueFormat;
/// struct Whole;
/// impl ValueFormat for Whole {
///     fn dialect_name(&self) -> &'static str { "whole" }
///     fn bool_placeholder(&self, slot: usize) -> String { format!("${slot}") }
///     fn int_placeholder(&self, slot: usize) -> String { format!("${slot}") }
///     fn float_placeholder(&self, slot: usize) -> String { format!("${slot}") }
///     fn text_placeholder(&self, slot: usize) -> String { format!("${slot}") }
///     fn bytes_placeholder(&self, slot: usize) -> String { format!("${slot}") }
///     fn vector_placeholder(&self, slot: usize) -> String { format!("${slot}::vector") }
///     fn current_timestamp_expr(&self) -> &'static str { "NOW()" }
/// }
/// ```
///
/// A partial one does not, which is the property this trait exists for:
///
/// ```compile_fail
/// use zeroship_data_orm::sql::render::ValueFormat;
/// struct Half;
/// impl ValueFormat for Half {
///     fn dialect_name(&self) -> &'static str { "half" }
///     fn bool_placeholder(&self, slot: usize) -> String { format!("${slot}") }
/// }
/// ```
///
/// And so does one that spells every placeholder but neither of the two the
/// write family added - which is the case a `bytes_placeholder`-shaped trait
/// would have let through:
///
/// ```compile_fail
/// use zeroship_data_orm::sql::render::ValueFormat;
/// struct PlaceholdersOnly;
/// impl ValueFormat for PlaceholdersOnly {
///     fn dialect_name(&self) -> &'static str { "placeholders-only" }
///     fn bool_placeholder(&self, slot: usize) -> String { format!("${slot}") }
///     fn int_placeholder(&self, slot: usize) -> String { format!("${slot}") }
///     fn float_placeholder(&self, slot: usize) -> String { format!("${slot}") }
///     fn text_placeholder(&self, slot: usize) -> String { format!("${slot}") }
///     fn bytes_placeholder(&self, slot: usize) -> String { format!("${slot}") }
/// }
/// ```
///
/// The search family added a third, and it gets its own arm rather than being
/// folded into the one above. That is the point of the pattern: an impl written
/// against the trait as it stood *before* this family compiled, and the reason
/// it must not now is that it would otherwise inherit some other dialect's idea
/// of how a vector reaches the statement - which is exactly how the two
/// mechanisms described on [`ValueFormat::vector_placeholder`] came to exist.
///
/// ```compile_fail
/// use zeroship_data_orm::sql::render::ValueFormat;
/// struct BeforeSearch;
/// impl ValueFormat for BeforeSearch {
///     fn dialect_name(&self) -> &'static str { "before-search" }
///     fn bool_placeholder(&self, slot: usize) -> String { format!("${slot}") }
///     fn int_placeholder(&self, slot: usize) -> String { format!("${slot}") }
///     fn float_placeholder(&self, slot: usize) -> String { format!("${slot}") }
///     fn text_placeholder(&self, slot: usize) -> String { format!("${slot}") }
///     fn bytes_placeholder(&self, slot: usize) -> String { format!("${slot}") }
///     fn current_timestamp_expr(&self) -> &'static str { "NOW()" }
/// }
/// ```
pub trait ValueFormat {
    /// The dialect's name, for refusal messages.
    fn dialect_name(&self) -> &'static str;
    fn bool_placeholder(&self, slot: usize) -> String;
    fn int_placeholder(&self, slot: usize) -> String;
    fn float_placeholder(&self, slot: usize) -> String;
    fn text_placeholder(&self, slot: usize) -> String;
    /// The spelling that gets raw bytes into a binary column.
    ///
    /// This is the method the old design needed three unrelated mechanisms for.
    /// Because the parameter is already typed, a driver that binds `Vec<u8>` as
    /// binary needs no wrapper at all - so the `PostgreSQL` implementation
    /// **deletes** `decode($N, 'base64')::bytea` rather than relocating it.
    fn bytes_placeholder(&self, slot: usize) -> String;
    /// The expression that reads **the server's** clock.
    ///
    /// `NOW()` on `PostgreSQL`, `CURRENT_TIMESTAMP` on `SQLite` - a divergence
    /// `query.rs` already carries per dialect (`current_timestamp_expr`, reached
    /// through `renderer(dialect)` at `query.rs:3915`). It is a spelling, not a
    /// value: nothing about it is caller-supplied, and the reason it is not a
    /// bound parameter at all is that a worker's clock is not the database's.
    fn current_timestamp_expr(&self) -> &'static str;
    /// The spelling that gets a query vector into the statement.
    ///
    /// The same job as [`ValueFormat::bytes_placeholder`], for the search
    /// family's operand, and it exists because the shipped code carries a
    /// vector by **two unrelated mechanisms for one concept** - the failure
    /// this trait was introduced to end:
    ///
    /// * `PostgreSQL` builds a text literal `[1,2,3]`, binds it as a `String`,
    ///   and casts it in the SQL
    ///   (`crates/zeroship-data-orm/src/sql/compile.rs`);
    /// * `SQLite` encodes the same values as a little-endian `f32` buffer and
    ///   **interpolates it into the statement as an `x'..'` literal**, binding
    ///   nothing at all, because the session actor's parameter surface is
    ///   `&[&str]` and has no binary channel
    ///   (`crates/zeroship-data-orm/src/backend/sqlite/mod.rs`).
    ///
    /// With [`zeroship_data_orm::sql::QueryVector`] a typed parameter, the value is the numbers
    /// and this method is the only place a dialect's spelling of them lives.
    /// `PostgreSQL` still needs the `::vector` cast - the type's OID is
    /// allocated at extension-install time and is not known to the driver, so a
    /// text cast sidesteps the binary type-discovery handshake - and that cast
    /// is a spelling, which is why it is here rather than in the plan.
    fn vector_placeholder(&self, slot: usize) -> String;
}

/// The single exhaustive dispatch from a value's type to its spelling.
///
/// One match, in one place. A new [`Literal`] variant fails to compile here,
/// which is what stops it inheriting `text_placeholder` by accident.
pub(crate) fn placeholder_for(format: &dyn ValueFormat, slot: usize, value: &Literal) -> String {
    match value {
        Literal::Bool(_) => format.bool_placeholder(slot),
        Literal::Int(_) => format.int_placeholder(slot),
        Literal::Float(_) => format.float_placeholder(slot),
        Literal::Text(_) | Literal::Json(_) => format.text_placeholder(slot),
        Literal::Bytes(_) => format.bytes_placeholder(slot),
        Literal::Vector(_) => format.vector_placeholder(slot),
    }
}
