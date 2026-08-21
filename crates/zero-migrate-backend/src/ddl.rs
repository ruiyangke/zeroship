//! The per-dialect DDL EMISSION seam: the [`DdlEmitter`] trait and the
//! [`CreateTableRequest`] its widest method takes.
//!
//! The third of this crate's vendor traits to arrive, and it came for the same
//! reason as [`crate::renderer::DmlRenderer`] and [`crate::schema::SchemaRenderer`]:
//! declared in the engine, it would have forced every vendor crate to depend on the
//! engine, which already depends on every vendor. Cargo refuses that cycle, so the
//! CONTRACT sits below both and the arrow never comes back.
//!
//! Everything below this line is the engine's own header for the seam, moved with
//! it unchanged. It describes what the trait isolates and what deliberately stayed
//! in the differ; both are still true, and the only thing that changed is which
//! crate the words live in.
//!
//! DdlEmitter — the per-dialect EMISSION seam.
//!
//! The differ's diff-COMPARISON is dialect-neutral; only the final DDL spelling
//! differs by dialect. This trait isolates exactly those emission concerns — the
//! ADD COLUMN statement (incl. mask/encrypted sentinel spelling), the CREATE INDEX
//! up/down (access-method + WITH + qualification), and the DROP table/column/index
//! qualification — so `DeclarativeAuthor`'s render methods are thin callers and the
//! dialect choice is made ONCE (via `DeclarativeAuthor::emitter`).
//!
//! Two impls — `PgEmitter` (schema-qualified PG DDL: access methods, WITH storage
//! params, `COMMENT ON COLUMN` sentinels) and `SqliteEmitter` (unqualified `main`
//! DDL: inline `/* … */` sentinels, plain B-tree indexes). Each method body is
//! the EXACT former `if is_sqlite { … } else { … }` arm, moved VERBATIM — code
//! motion, not a rewrite, so the bytes are unchanged (the goldens prove
//! it). The ROUTING branches (FK inline-vs-defer, rebuild-vs-ALTER, policy-injected
//! index skip, the unreachable guard) stay in `diff()` — they are diff-logic.
//!
//! The CREATE TABLE renderers ARE extracted, as of [`DdlEmitter::create_table`].
//! The header used to say they were not, on the grounds that "column/constraint
//! spelling is large enough to remain dialect-specific" — which is true of the
//! BODIES and says nothing about the seam. What actually blocked it was that the
//! three renderers took three DIFFERENT parameter lists; see
//! [`CreateTableRequest`] for why that divergence was apparent rather than real.
//!
//! The three per-dialect ADAPTERS that briefly stood between the call sites and
//! that method are gone: every caller now builds its own [`CreateTableRequest`]
//! and asks an emitter directly. The adapters had been the last place where a
//! caller's dialect was inferred from WHICH function it called rather than stated,
//! and the `lower_create_table` site shows what that bought — one request, built

use crate::snapshot::{
    ColumnSnapshot, ConstraintSnapshot, GeneratedColumnSnapshot, IndexSnapshot, TableSnapshot,
};
use zero_migrate_ir::dialect::SqlDialect;

// once, handed to whichever emitter the dialect selects.

/// Everything a backend needs to spell ONE `CREATE TABLE`, and nothing about which
/// backend is spelling it.
///
/// The three renderers this unifies did not differ in what they NEEDED — they
/// differed in what each had been handed. PostgreSQL and MySQL took an
/// `inline_fks` slice; SQLite took a `&ResolvedInject` and no FK slice at all.
/// That reads like three incompatible contracts and is not: BOTH are decisions
/// CORE already made before any vendor is consulted (which foreign keys inline
/// rather than defer to an `ALTER TABLE ADD CONSTRAINT`; which indexes the active
/// policy injected). Carrying both and letting each backend read the fields its
/// dialect uses costs one `Option` and removes the divergence.
// `#[derive(Debug)]` is the one addition the crate boundary forced. The struct was
// module-private in the engine, where `missing_debug_implementations` does not
// apply; it is public API here, where it does. It carries no behaviour.
#[derive(Debug)]
pub struct CreateTableRequest<'a> {
    /// The UNQUALIFIED table name. Each backend qualifies it its own way — with a
    /// project schema on PostgreSQL and MySQL, not at all on SQLite, where the
    /// app file IS `main`.
    pub table: &'a str,
    /// The resolved desired shape: columns, constraints, indexes, partitioning.
    pub snapshot: &'a TableSnapshot,
    /// The foreign keys CORE decided to INLINE in the create rather than defer.
    ///
    /// SQLite ignores this and inlines every `FOREIGN KEY` straight off
    /// `snapshot.constraints`, because it has no late `ADD CONSTRAINT` to defer
    /// TO — the routing decision that produces this slice is made in
    /// [`DeclarativeAuthor::lower_create_table`], gated on
    /// `Capability::AlterTableAddConstraint`.
    pub inline_fks: &'a [&'a ConstraintSnapshot],
    /// The names of `snapshot.indexes` the active policy INJECTED, resolved by
    /// core before any vendor is consulted.
    ///
    /// Only SQLite reads it: it emits the injected indexes INSIDE the create
    /// payload instead of as follow-on `CREATE INDEX` units, which is why the
    /// differ skips re-emitting them on that dialect. An EMPTY slice means "the
    /// caller had no inject to give", which today is only ever a PostgreSQL or
    /// MySQL caller — neither of which looks at this field.
    ///
    /// # Why this is a name list and not the `ResolvedInject` it used to be
    ///
    /// The field was `Option<&ResolvedInject>` and SQLite read it through exactly
    /// one expression: `is_injected_index(table, &idx.name, inj)`. That predicate
    /// lives in the engine and cannot leave it — it resolves an inject spec's
    /// columns through `zero_migrate::schema::query::index_name`, i.e. the ENGINE's
    /// index-naming convention, which is a decision core makes and not a spelling a
    /// vendor is asked for.
    ///
    /// So core answers the question once and carries the answer. That is exactly
    /// equivalent rather than approximately so, and the reason is worth stating
    /// because "we precomputed it" is usually where a behaviour change hides:
    /// within one `create_table` call `table` and the inject are LOOP-INVARIANT, so
    /// the predicate is a pure function of `idx.name` alone. Selecting the matching
    /// names up front and testing membership therefore admits exactly the same
    /// indexes for every input — including the degenerate case of two entries in
    /// `snapshot.indexes` sharing a name, where a name-keyed predicate necessarily
    /// returns the same answer for both.
    pub injected_indexes: &'a [String],
}

pub trait DdlEmitter {
    /// Render a `CREATE TABLE` as its STRUCTURAL statement list — the create
    /// itself plus whatever the dialect attaches to it. `join(";\n")` over the
    /// list is the canonical `up`, and the list (not the joined string) is what
    /// the guard-per-statement lower consumes, so a string-literal DEFAULT
    /// carrying an interior `;\n` is never re-split mid-statement.
    ///
    /// What "attaches to it" differs, and that IS the vendor's business: PostgreSQL
    /// appends one `COMMENT ON COLUMN` per sentinel-carrying column, SQLite appends
    /// the policy-injected `CREATE INDEX`es, MySQL appends nothing.
    fn create_table(&self, req: &CreateTableRequest<'_>) -> Vec<String>;

    /// Render an `ALTER TABLE … ADD COLUMN …` as `(up_statements, down)`. The mask
    /// / encrypted sentinel spelling differs by dialect: PG appends a trailing
    /// `COMMENT ON COLUMN` as a SEPARATE structural statement; `SQLite` rides the
    /// sentinel inline in the column clause (a single statement). Returning the
    /// per-statement list (not a `;\n`-joined string) keeps the guard-per-statement
    /// lower from re-splitting a string-literal DEFAULT that itself contains `;\n`.
    /// `join(";\n")` over the list is the canonical `up`.
    fn add_column(&self, table: &str, c: &ColumnSnapshot) -> (Vec<String>, Option<String>);

    /// Render a `CREATE … INDEX …` as `(up, down)`. PG emits the access-method
    /// (`USING …`), the `WITH (lists=…)` storage param, and qualifies; `SQLite`
    /// emits a plain unqualified b-tree index.
    fn create_index(&self, table: &str, idx: &IndexSnapshot) -> (String, String);

    /// Render the `up` of a `DROP TABLE` (qualification differs).
    fn drop_table_up(&self, table: &str) -> String;

    /// Render an `ALTER TABLE <old> RENAME TO <new>` as `(up, down)`. The `down`
    /// is the inverse rename (`new` → `old`). On PG the table-ref is
    /// schema-qualified, but the RENAME TARGET is a BARE name — Postgres rejects a
    /// schema-qualified target (`… RENAME TO "schema"."t"` is a syntax error); the
    /// renamed table stays in the same schema. On SQLite both are unqualified
    /// `main` names.
    fn rename_table(&self, table: &str, to: &str) -> (String, String);

    /// Render the `up` of an `ALTER TABLE … DROP COLUMN …` (qualification differs).
    fn drop_column_up(&self, table: &str, col: &str) -> String;

    /// Render the `up` of a `DROP INDEX …`. PG qualifies the index name; `SQLite`
    /// must emit it unqualified (a qualified `DROP INDEX "schema"."ix"` silently
    /// no-ops on `SQLite` — the dangerous silent-drift mode).
    fn drop_index_up(&self, table: Option<&str>, idx_name: &str) -> String;
}

// ===========================================================================
// The COLUMN-CLAUSE spellings every backend shares.
//
// Moved here from `zero_migrate::render::declarative`, where they were private
// siblings of the three `DdlEmitter` impls. All three impls call every one of
// them, from BOTH `create_table` and `add_column`, so a helper the vendors share
// cannot stay above the vendors — that is the same arrow the trait itself moved
// to satisfy.
//
// They are dialect-PARAMETERIZED, not dialect-specific: each takes the
// `SqlDialect` it is spelling for, or takes none at all because the clause is
// identical on all three. Nothing here resolves a vendor or names one it was not
// handed, so the boundary rule in `zero_migrate::render::backends`'s header is
// unchanged — the caller has already decided which vendor it is.
//
// Moved VERBATIM: same bodies, same names, same order of tests over the same
// snapshot fields. The engine's own non-emitter render paths still call them, now
// across the crate boundary rather than from a private sibling.
//
// `pub` HERE IS WIDER THAN THE `pub(crate)` / private THEY HAD, and there is no
// modifier that says "visible to the engine and to a vendor but to nobody else".
// The property that matters is the one the boundary rule states and it is
// unchanged: these SPELL, they do not COMPARE. None of them decides whether two
// schemas differ, which is what
// `zero-migrate/tests/dialect_matrix/backend_snapshot_privates_stay_core_only.rs`
// forbids a vendor from reaching for.

/// Sentinel prefix on a [`ColumnSnapshot::default`] marking a STORED generated
/// column (the `__fts` tsvector). When the `default` body starts with this
/// prefix, the emitter writes `GENERATED ALWAYS AS (<expr>) STORED` instead of a
/// plain `DEFAULT <expr>` clause. The remainder after the prefix is the
/// generation expression. Generated-column expressions are emission-only metadata
/// (excluded from `ColumnSnapshot` equality), so this never participates in drift.
pub const GENERATED_PREFIX: &str = "GENERATED:";

/// Render a column's trailing `DEFAULT <expr>` or `GENERATED ALWAYS AS (<expr>)
/// STORED` clause from its (emission-only) `default` body. Empty string when the
/// column has no default. A `GENERATED:`-prefixed body becomes the stored
/// generated-column clause (the `__fts` generated column); any other body is a
/// plain default.
#[must_use]
pub fn default_clause(default: Option<&str>) -> String {
    match default {
        Some(d) => {
            if let Some(expr) = d.strip_prefix(GENERATED_PREFIX) {
                format!(" GENERATED ALWAYS AS ({expr}) STORED")
            } else {
                format!(" DEFAULT {d}")
            }
        }
        None => String::new(),
    }
}

/// Render the `GENERATED ALWAYS AS (<expr>) STORED|VIRTUAL` clause a generated
/// column carries, or the empty string. Identical on all three dialects.
#[must_use]
pub fn generated_clause(generated: Option<&GeneratedColumnSnapshot>) -> String {
    match generated {
        Some(g) => {
            let storage = if g.stored { "STORED" } else { "VIRTUAL" };
            format!(" GENERATED ALWAYS AS ({}) {storage}", g.expr)
        }
        None => String::new(),
    }
}

/// Whether this column is `SQLite`'s rowid-alias `INTEGER PRIMARY KEY
/// AUTOINCREMENT` shape — an auto-increment identity, inline PK, over one of the
/// integer storage classes.
///
/// Both [`primary_key_clause`] and [`null_clause`] gate on it, and so does the
/// engine's own type renderer, which is why it sits beside them rather than in the
/// SQLite vendor: three callers in two crates, and the answer is read off the
/// snapshot without asking any vendor anything.
#[must_use]
pub fn sqlite_auto_increment_identity_pk(c: &ColumnSnapshot, inline_pk: bool) -> bool {
    matches!(c.identity, Some(identity) if !identity.always)
        && inline_pk
        && matches!(
            c.data_type.to_ascii_lowercase().as_str(),
            "integer" | "bigint" | "smallint" | "int" | "int2" | "int4" | "int8"
        )
}

/// The trailing ` PRIMARY KEY` (or `SQLite`'s ` PRIMARY KEY AUTOINCREMENT`) clause
/// an inline single-column PK carries, or the empty string.
#[must_use]
pub fn primary_key_clause(c: &ColumnSnapshot, dialect: SqlDialect, inline_pk: bool) -> &'static str {
    if matches!(dialect, SqlDialect::Sqlite) && sqlite_auto_increment_identity_pk(c, inline_pk) {
        " PRIMARY KEY AUTOINCREMENT"
    } else if inline_pk {
        " PRIMARY KEY"
    } else {
        ""
    }
}

/// The trailing ` NOT NULL` clause, or the empty string.
///
/// Two dialects suppress it where the column is implicitly non-null already:
/// `SQLite`'s rowid alias, and MySQL's `AUTO_INCREMENT`.
#[must_use]
pub fn null_clause(c: &ColumnSnapshot, dialect: SqlDialect, inline_pk: bool) -> &'static str {
    if c.nullable
        || (matches!(dialect, SqlDialect::Sqlite) && sqlite_auto_increment_identity_pk(c, inline_pk))
        || (matches!(dialect, SqlDialect::Mysql)
            && matches!(c.identity, Some(identity) if !identity.always))
    {
        ""
    } else {
        " NOT NULL"
    }
}

/// The inline `CHECK (...)` clauses a column carries, space-prefixed, or the empty
/// string. The bodies are built by the engine; this only joins them.
#[must_use]
pub fn inline_checks_clause(c: &ColumnSnapshot) -> String {
    if c.inline_checks.is_empty() {
        String::new()
    } else {
        format!(" {}", c.inline_checks.join(" "))
    }
}

/// The columns of `table`'s PRIMARY KEY, read off the implicit `<table>_pkey`
/// unique index the snapshot carries, or `None` when the table has no PK.
#[must_use]
pub fn primary_key_columns<'a>(table: &str, t: &'a TableSnapshot) -> Option<&'a [String]> {
    t.indexes
        .iter()
        .find(|idx| idx.name == format!("{table}_pkey") && idx.unique)
        .map(|idx| idx.columns.as_slice())
}

/// Whether `column` is the WHOLE primary key, and therefore takes the PK inline on
/// its own column clause rather than as a table-level constraint.
#[must_use]
pub fn inline_pk_for_column(table: &str, t: &TableSnapshot, column: &str) -> bool {
    matches!(primary_key_columns(table, t), Some(cols) if cols == [column])
}

/// Whether a PRIMARY KEY constraint must be rendered as a TABLE-level clause —
/// true for a composite PK, false for the single-column case
/// [`inline_pk_for_column`] already inlined.
#[must_use]
pub fn should_render_table_pk(
    table: &str,
    t: &TableSnapshot,
    constraint: &ConstraintSnapshot,
) -> bool {
    constraint.kind == "PRIMARY KEY"
        && !matches!(primary_key_columns(table, t), Some(cols) if cols.len() == 1)
}
