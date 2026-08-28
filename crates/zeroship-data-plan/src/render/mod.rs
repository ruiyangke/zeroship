//! Lowering a plan to SQL.
//!
//! # The output carries its parameters, always
//!
//! [`RenderedSql`] is a statement plus a parameter list. A value never appears
//! in the statement text, so two reads differing only in their arguments
//! produce **identical SQL** - which is the property the driver's
//! prepared-statement cache keys on.
//!
//! That cache is worth being precise about, because SC-3 corrects an earlier
//! draft of its own on this point. The machinery exists in the driver
//! (`Client::new_with_statement_cache`, `statement_cache_execution_threshold`;
//! `libs/compio-postgres/src/bind.rs`, `libs/compio-postgres/src/prepare.rs`),
//! and **it is switched off**: `statement_cache_capacity` defaults to 0 and
//! `plugin-db` never calls `prepare_cached`, so every operation today sends an
//! unnamed statement that `PostgreSQL` parses and plans from scratch. Those are
//! carried-forward citations from SC-3 rather than measurements taken here.
//! Nothing in this crate turns the cache on; what it does is make the cache
//! worth turning on.
//!
//! # Determinism, and which specification of it this implements
//!
//! Rendering a given plan shape must be **byte-stable**. If a lowering iterated
//! a hash map to emit a projection list or an `AND` chain, column order would
//! vary between runs, the SQL text with it, and every call would be a cache
//! miss - a silent performance regression no correctness test would notice.
//!
//! SC-3 carries two specifications of the arm that checks this. The one
//! implemented here is the **current** one: *two semantically identical plans
//! built by opposite insertion permutations, asserted against one canonical
//! SQL/parameter fixture, plus a mutation that deletes the canonical sort and
//! proves the arm turns red.* The **superseded** one - render the same plan
//! twice and assert the SQL matches - is not implemented, and SC-3 records why
//! in its own words: rendering one in-memory unordered map twice can preserve
//! that instance's iteration order, so a non-canonical implementation passes.
//! It is a probabilistic arm, not a discriminating one.
//!
//! The mutation half is `permuted_conjuncts_diverge_without_the_canonical_sort`
//! in [`postgres`], which renders raw, un-canonicalised predicates through the
//! same private writer the real path uses and asserts the two permutations come
//! out **different**. Without it, "the two permutations agree" would also be
//! true of a renderer that had never been given two different inputs.
//!
//! # Parameters are TYPED, so a binary value is not a tagged string
//!
//! The shape this replaces is `BuiltQuery { sql: String, params: Vec<String> }`
//! (`zeroship_schema::query`), where every parameter is a `String` and binary
//! values are smuggled through by two different mechanisms for one concept
//! (`query.rs:106-122`): `PostgreSQL` wraps the placeholder in SQL as
//! `decode($N, 'base64')::bytea`, `SQLite` emits a bare `$N` and prefixes the
//! PARAM VALUE with `SQLITE_BINARY_BIND_PREFIX` (`query.rs:593`) for the session
//! actor to detect and rebind as a BLOB, and `MySQL` wraps with `FROM_BASE64(?)`.
//!
//! Three things go wrong there, and they are worth separating:
//!
//! 1. the value's TYPE is invisible to the type system;
//! 2. one dialect difference leaks into two unrelated layers - SQL text on one
//!    arm, param value on the other - so there is no single place to read what
//!    a dialect does with bytes;
//! 3. a sentinel PREFIX on a string parameter is forgeable in principle by any
//!    value that happens to start with those bytes. Whether that is reachable
//!    today is not established here and is not claimed; the point is that a
//!    typed parameter removes the question instead of answering it.
//!
//! [`Literal::Bytes`] is a variant, so (1) and (3) do not arise: a `Vec<u8>` is
//! not a `String` and carries no prefix to forge. [`ValueFormat`] fixes (2) by
//! putting every dialect's spelling of every parameter type in one trait, and
//! the trait deliberately has **no default methods** - the same enforcement
//! `SchemaRenderer` uses (`query.rs:124-130`), so a second dialect cannot
//! compile until it has spelled all of them, and a new [`Literal`] variant
//! breaks [`placeholder_for`]'s exhaustive match rather than silently
//! inheriting someone else's spelling.

pub mod postgres;

use crate::literal::Literal;

/// Every spelling that is a dialect's rather than the grammar's.
///
/// Most of them are placeholders, and the two that are not
/// ([`ValueFormat::current_timestamp_expr`], [`ValueFormat::row_identity_column`])
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
/// use zeroship_data_plan::render::ValueFormat;
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
///     fn row_identity_column(&self) -> &'static str { "ctid" }
/// }
/// ```
///
/// A partial one does not, which is the property this trait exists for:
///
/// ```compile_fail
/// use zeroship_data_plan::render::ValueFormat;
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
/// use zeroship_data_plan::render::ValueFormat;
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
/// use zeroship_data_plan::render::ValueFormat;
/// struct BeforeSearch;
/// impl ValueFormat for BeforeSearch {
///     fn dialect_name(&self) -> &'static str { "before-search" }
///     fn bool_placeholder(&self, slot: usize) -> String { format!("${slot}") }
///     fn int_placeholder(&self, slot: usize) -> String { format!("${slot}") }
///     fn float_placeholder(&self, slot: usize) -> String { format!("${slot}") }
///     fn text_placeholder(&self, slot: usize) -> String { format!("${slot}") }
///     fn bytes_placeholder(&self, slot: usize) -> String { format!("${slot}") }
///     fn current_timestamp_expr(&self) -> &'static str { "NOW()" }
///     fn row_identity_column(&self) -> &'static str { "ctid" }
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
    ///   (`crates/zeroship-schema/src/query.rs:4983-5013`);
    /// * `SQLite` encodes the same values as a little-endian `f32` buffer and
    ///   **interpolates it into the statement as an `x'..'` literal**, binding
    ///   nothing at all, because the session actor's parameter surface is
    ///   `&[&str]` and has no binary channel
    ///   (`crates/zeroship-plugin-db/src/backend/sqlite/mod.rs:1453-1460`).
    ///
    /// With [`crate::QueryVector`] a typed parameter, the value is the numbers
    /// and this method is the only place a dialect's spelling of them lives.
    /// `PostgreSQL` still needs the `::vector` cast - the type's OID is
    /// allocated at extension-install time and is not known to the driver, so a
    /// text cast sidesteps the binary type-discovery handshake - and that cast
    /// is a spelling, which is why it is here rather than in the plan.
    fn vector_placeholder(&self, slot: usize) -> String;
    /// The column that names one physical row, for a bounded write's subquery.
    ///
    /// `ctid` on `PostgreSQL`, `rowid` on `SQLite` - the same per-dialect choice
    /// `query.rs:3999-4003` and `:4284-4288` make, twice each, inline.
    ///
    /// It is emitted quoted like any other identifier, and it never comes from a
    /// caller: no [`crate::Ident`] a caller holds reaches this position.
    fn row_identity_column(&self) -> &'static str;
}

/// The single exhaustive dispatch from a value's type to its spelling.
///
/// One match, in one place. A new [`Literal`] variant fails to compile here,
/// which is what stops it inheriting `text_placeholder` by accident.
pub(crate) fn placeholder_for(
    format: &dyn ValueFormat,
    slot: usize,
    value: &Literal,
) -> String {
    match value {
        Literal::Bool(_) => format.bool_placeholder(slot),
        Literal::Int(_) => format.int_placeholder(slot),
        Literal::Float(_) => format.float_placeholder(slot),
        Literal::Text(_) => format.text_placeholder(slot),
        Literal::Bytes(_) => format.bytes_placeholder(slot),
        Literal::Vector(_) => format.vector_placeholder(slot),
    }
}

/// A statement and the values to bind to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedSql {
    sql: String,
    params: Vec<Literal>,
}

impl RenderedSql {
    pub(crate) const fn new(sql: String, params: Vec<Literal>) -> Self {
        Self { sql, params }
    }

    /// The statement text. Contains no value from the plan - only validated
    /// identifiers, keywords, and `$n` placeholders.
    #[must_use]
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// The values, in placeholder order: `params()[0]` binds to `$1`.
    #[must_use]
    pub fn params(&self) -> &[Literal] {
        &self.params
    }

    /// How many `$n` placeholders the statement text contains.
    ///
    /// This exists to check SC-3's third named invariant - *a plan's parameter
    /// count must equal the placeholders its lowering emits* - from the SQL
    /// side rather than from the renderer's own bookkeeping, which is the side
    /// that would be wrong if the bookkeeping were wrong.
    ///
    /// Scanning for `$` is sound here precisely because of the property above:
    /// no value reaches the statement, and an identifier cannot contain `$`
    /// (the charset is ASCII alphanumeric and underscore), so a `$` in the text
    /// is always a placeholder this renderer emitted.
    #[must_use]
    pub fn placeholder_count(&self) -> usize {
        let bytes = self.sql.as_bytes();
        bytes
            .iter()
            .enumerate()
            .filter(|(i, b)| **b == b'$' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit))
            .count()
    }

    /// The distinct `$n` indices the statement mentions, ascending.
    ///
    /// # Why this exists alongside [`RenderedSql::placeholder_count`]
    ///
    /// SC-3's third invariant - *a plan's parameter count must equal the
    /// placeholders its lowering emits* - was checkable by counting `$`
    /// occurrences while every family bound each value exactly once. The search
    /// family breaks that assumption legitimately: a distance expression is
    /// written in the select list and again in the `ORDER BY`, and the operand
    /// is one binding referenced twice, so the occurrence count exceeds the
    /// parameter count by design.
    ///
    /// Counting *distinct slots* restores the invariant in the form that was
    /// always the one meant, and it is strictly stronger than the count it
    /// generalises. `slots == 1..=params.len()` catches both directions:
    ///
    /// * a `$4` in the text with three parameters bound - the driver errors at
    ///   execution, naming neither the plan nor the clause;
    /// * a parameter bound and never referenced - which `PostgreSQL` also
    ///   rejects, and which a bare count would miss whenever some *other*
    ///   placeholder had been written twice, exactly the state this family
    ///   creates.
    ///
    /// Scanning for `$` is sound for the reason given above: no value reaches
    /// the statement text and no identifier may contain `$`.
    #[must_use]
    pub fn placeholder_slots(&self) -> Vec<usize> {
        let bytes = self.sql.as_bytes();
        let mut slots: Vec<usize> = Vec::new();
        let mut index = 0_usize;
        while index < bytes.len() {
            if bytes[index] != b'$' {
                index += 1;
                continue;
            }
            let start = index + 1;
            let mut end = start;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end == start {
                index += 1;
                continue;
            }
            // Every digit run here was written by `placeholder_for`, so it is a
            // slot number this renderer chose and fits a `usize` on any target
            // that could hold the parameter vector it indexes.
            if let Ok(slot) = self.sql[start..end].parse::<usize>() {
                slots.push(slot);
            }
            index = end;
        }
        slots.sort_unstable();
        slots.dedup();
        slots
    }
}
