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

/// How a dialect spells the placeholder for each parameter type.
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
}
