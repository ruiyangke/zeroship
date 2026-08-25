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
//! Three impls — PostgreSQL (schema-qualified DDL: access methods, WITH storage
//! params, `COMMENT ON COLUMN` sentinels), SQLite (unqualified `main` DDL: inline
//! `/* … */` sentinels, plain B-tree indexes), and MySQL (backtick-qualified DDL,
//! native enum folding, and inline foreign-key-supporting indexes). Each method body is
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

use crate::fold::CatalogFoldPolicy;
use crate::snapshot::{
    canonical_index_sort_order, ColumnSnapshot, ConstraintSnapshot, GeneratedColumnSnapshot,
    IndexElementSnapshot, IndexSnapshot, TableSnapshot,
};
use zero_migrate_ir::dialect::DialectId;
use zero_migrate_ir::ir::IndexSortOrder;
use zero_migrate_ir::ir::PartitionBounds;
use zero_migrate_ir::ir::{ExclusionMethod, ExclusionOperator};

/// True if `index_name` is the implicit index a PRIMARY KEY materialises. It is
/// created/dropped by the PK clause, never by a standalone CREATE/DROP INDEX, so
/// the differ never emits DDL for it.
///
/// `policy` is the REGISTERED backend, and it is a parameter rather than a
/// convention because there is no convention: this predicate used to spell one
/// shipping vendor's `<table>_pkey` here, in the crate whose whole purpose is to
/// name no vendor, and the cost was paid by the other backends. A server that calls
/// every primary key `PRIMARY` had to report `<table>_pkey` instead so this
/// comparison kept matching — a backend impersonating another to satisfy a shared
/// check. Asking the backend is what removes the need to impersonate one.
#[must_use]
pub fn is_pk_index(policy: &dyn CatalogFoldPolicy, table: &str, index_name: &str) -> bool {
    index_name == policy.implicit_primary_key_name(table)
}

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
/// dialect uses removes the divergence.
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
    /// TO — the routing decision that produces this slice is made in the engine's
    /// `DeclarativeAuthor::lower_create_table`, gated on
    /// `Capability::AlterTableAddConstraint`.
    ///
    /// Deliberately NOT an intra-doc link: `DeclarativeAuthor` lives in the
    /// `zero-migrate` engine, which depends on this crate and not the other way
    /// round. A resolvable link here would require the arrow this whole split
    /// exists to remove, so the reference stays prose.
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

    /// The deterministic enum-CHECK name for every element of
    /// [`TableSnapshot::columns`], in the same order and with the same length.
    /// Core derives these names because the naming authority
    /// (`plan::author::cap_ident_name`) lives above this contract and must not be
    /// copied into a vendor.
    ///
    /// # Exactness of the precompute
    ///
    /// The former MySQL emitter computed
    /// `check_constraint_name(table, column.name, "enum")` inside its column loop.
    /// Within one create, `table` and the literal kind `"enum"` are invariant; the
    /// result is therefore a pure function of that loop element's `column.name`.
    /// Core maps that same function over the same ordered `columns` slice, so element
    /// `i` is byte-identical to the old call for column `i`. Duplicate column names
    /// retain duplicate entries and receive the same answer just as before; long-name
    /// truncation and hashing also run through the same function before the vendor is
    /// called.
    pub enum_check_names: Vec<String>,
}

pub trait DdlEmitter {
    /// Which backend owns this emitter.
    ///
    /// Required, with no default: registry wiring can assert that a vendor did not
    /// pair its descriptor with another backend's DDL factory.
    fn dialect(&self) -> DialectId;

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

    /// Render this vendor's inline / stand-alone foreign-key clause.
    ///
    /// Required, with no shared fallback. Even a backend whose capability set makes
    /// stand-alone `ADD CONSTRAINT` unreachable must state its own clause spelling.
    fn fk_clause(&self, fk: &ConstraintSnapshot) -> String;

    /// Render this backend's table reference for an `ALTER TABLE` statement.
    fn alter_table_ref(&self, table: &str) -> String;

    /// Render the vendor-specific removal of a named foreign key, or explicitly
    /// refuse when this backend must rebuild a table instead.
    fn drop_foreign_key_up(&self, table: &str, name: &str) -> Option<String>;

    /// Render the `up` of an in-place column RETYPE, or explicitly refuse when this
    /// backend has no offline spelling for one.
    ///
    /// `ty` is the target type as this backend's own
    /// [`SchemaRenderer::column_type`](crate::schema::SchemaRenderer::column_type)
    /// spelled it, so the vendor is reading back its own bytes rather than being
    /// handed a neutral name to re-map.
    ///
    /// `cast_value` is CORE's answer to a question core owns: whether the column's
    /// generation contract permits the existing value to be cast into the new type.
    /// It is carried as an answer rather than as the snapshot the predicate reads
    /// for the same reason [`CreateTableRequest::injected_indexes`] is a name list —
    /// the predicate lives above this contract and must not be copied into a vendor.
    /// A backend whose retype takes no cast clause ignores it.
    ///
    /// # Why this is `Option` and what the `None` means
    ///
    /// The same thing it means on [`Self::drop_foreign_key_up`] and the partition
    /// family: *this backend does not spell this operation here*. A backend that
    /// restates the whole column definition at APPLY, from the definition the server
    /// itself reports, cannot write the statement offline — the definition is not in
    /// the op. It says so by declaring
    /// [`CatalogFoldPolicy::restates_column_type_at_apply`]
    /// and returning `None` here, and the render layer never arrives.
    ///
    /// Required, with no default body. That is the whole point of the method: the
    /// verb, any cast clause, and any cast operator are three separate vendor
    /// decisions, and before this seam existed a backend had nowhere to state any of
    /// them. It received one vendor's answers to all three by omission.
    fn alter_column_type_up(
        &self,
        table: &str,
        column: &str,
        ty: &str,
        cast_value: bool,
    ) -> Option<String>;

    /// Render the `(up, down)` of a NULLABILITY change, or explicitly refuse.
    ///
    /// `nullable` is the DESIRED state, so `true` relaxes and `false` tightens; the
    /// `down` is the inverse statement. Which of the two directions is gated is not
    /// asked here — that is a safety judgement core makes about the operation, not a
    /// spelling — so a backend states only the two statements.
    ///
    /// Required, with no default body.
    fn alter_column_nullability(
        &self,
        table: &str,
        column: &str,
        nullable: bool,
    ) -> Option<(String, String)>;

    /// Render a column DEFAULT change, or explicitly refuse.
    ///
    /// `default_sql` is `Some(literal)` for a set and `None` for a drop. The two are
    /// one method because they are one statement with two tails, which is why the
    /// `down` of a set and the `up` of a drop come out byte-identical.
    ///
    /// Required, with no default body — and this is the member of the family a
    /// SHIPPING backend other than the one whose grammar core used to write already
    /// reaches, so the seam is not a precaution here.
    fn alter_column_default(
        &self,
        table: &str,
        column: &str,
        default_sql: Option<&str>,
    ) -> Option<String>;

    /// Names of snapshot indexes this vendor emits inside [`Self::create_table`]
    /// rather than as follow-on `CREATE INDEX` units.
    ///
    /// Required, with no default: PostgreSQL explicitly returns none, SQLite returns
    /// the policy-injected names carried by the request, and MySQL applies its own
    /// foreign-key-supporting-index rule. Core consumes only this answer; it does not
    /// encode any vendor's rule.
    fn indexes_inlined_by_create(&self, req: &CreateTableRequest<'_>) -> Vec<String>;

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

    /// Render native CREATE PARTITION relation DDL as `(up, down)`, or explicitly
    /// refuse when this backend collapses authored partitions into their parent.
    fn create_partition(
        &self,
        name: &str,
        of: &str,
        bounds: &PartitionBounds,
    ) -> Option<(String, String)>;

    /// Render native ATTACH PARTITION DDL as `(up, down)`, or explicitly refuse.
    fn attach_partition(
        &self,
        parent: &str,
        name: &str,
        bounds: &PartitionBounds,
    ) -> Option<(String, String)>;

    /// Render native DETACH PARTITION DDL, or explicitly refuse.
    fn detach_partition(&self, parent: &str, name: &str, concurrently: bool) -> Option<String>;

    /// Render native DROP PARTITION relation DDL, or explicitly refuse.
    ///
    /// All four methods are required with no default so a future backend must
    /// state the complete boundary instead of silently borrowing another
    /// vendor's partition grammar.
    fn drop_partition(&self, name: &str, cascade: bool) -> Option<String>;
}

/// One element of an exclusion constraint, with its target already rendered.
///
/// `target` arrives quoted (a column) or parenthesised (an expression), because both of
/// those go through the backend's own quoter and expression renderer before they get
/// here. `operator` stays structured so the vendor picks the spelling.
#[derive(Debug, Clone, Copy)]
pub struct ExclusionElementParts<'a> {
    /// The already-rendered column reference or parenthesised expression.
    pub target: &'a str,
    /// The comparison this element excludes on, unspelled.
    pub operator: ExclusionOperator,
}

/// Everything a backend needs to spell one exclusion constraint.
///
/// A request struct rather than a long parameter list, for the reason
/// [`CreateTableRequest`] is one: the arguments are same-typed and easy to transpose,
/// and rustc's "provide the argument" suggestion silently reorders same-typed arguments
/// when a signature changes.
#[derive(Debug, Clone, Copy)]
pub struct ExclusionConstraintRequest<'a> {
    /// The index access method the constraint is built on, unspelled.
    pub method: ExclusionMethod,
    /// The elements, in authored order. Never empty — the engine refuses that earlier.
    pub elements: &'a [ExclusionElementParts<'a>],
    /// An already-rendered `WHERE` predicate, without the keyword.
    pub where_predicate: Option<&'a str>,
    /// `Some(true)` for `DEFERRABLE`, `Some(false)` for `NOT DEFERRABLE`, `None` to say
    /// nothing and take the server's default.
    pub deferrable: Option<bool>,
    /// `Some(true)` for `INITIALLY DEFERRED`, `Some(false)` for `INITIALLY IMMEDIATE`.
    /// Only meaningful alongside `deferrable: Some(true)`.
    pub initially_deferred: Option<bool>,
}

/// Render an index element's canonical order suffix.
///
/// This helper is shared by all three DDL emitters and lives once in the contract so
/// vendor crates do not duplicate the bytes or call the snapshot comparison helper
/// directly.
#[must_use]
pub fn render_index_order_suffix(order: Option<IndexSortOrder>) -> &'static str {
    match canonical_index_sort_order(order) {
        Some(IndexSortOrder::Desc) => " DESC",
        Some(IndexSortOrder::Asc) | None => "",
    }
}

/// Whether a physical b-tree index supports `columns` as its leading key.
///
/// This is structural snapshot comparison, shared by core validation and the
/// MySQL create emitter; it contains no vendor spelling or routing decision.
#[must_use]
pub fn index_supports_fk_columns(index: &IndexSnapshot, columns: &[String]) -> bool {
    index.predicate.is_none()
        && !index.only
        && index.access_method.eq_ignore_ascii_case("btree")
        && index.columns.starts_with(columns)
        && index.elements.len() >= columns.len()
        && index
            .elements
            .iter()
            .take(columns.len())
            .zip(columns)
            .all(|(element, column)| {
                matches!(
                    element,
                    IndexElementSnapshot::Column {
                        name,
                        opclass: None,
                        collation: None,
                        ..
                    } if name == column
                )
            })
}

/// Whether a primary-key or unique constraint supports `columns` as its leading
/// canonical key. This is shared structural comparison, not DDL spelling.
#[must_use]
pub fn constraint_supports_fk_columns(constraint: &ConstraintSnapshot, columns: &[String]) -> bool {
    matches!(constraint.kind.as_str(), "PRIMARY KEY" | "UNIQUE")
        && fk_definition_column_group(&constraint.definition, 0)
            .is_some_and(|(key, _)| key.starts_with(columns))
}

fn split_constraintdef_column_list(list: &str) -> Vec<String> {
    let mut cols = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    let mut chars = list.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                current.push(ch);
                if matches!(chars.peek(), Some('"')) {
                    current.push(chars.next().unwrap());
                } else {
                    in_quote = !in_quote;
                }
            }
            ',' if !in_quote => {
                let col = unquote_constraintdef_column(&current);
                if !col.is_empty() {
                    cols.push(col);
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    let col = unquote_constraintdef_column(&current);
    if !col.is_empty() {
        cols.push(col);
    }
    cols
}

fn unquote_constraintdef_column(token: &str) -> String {
    let trimmed = token.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        trimmed[1..trimmed.len() - 1].replace("\"\"", "\"")
    } else {
        trimmed.to_string()
    }
}

/// Read one parenthesized column group from the canonical constraint-definition
/// form, beginning the search at `offset`.
#[must_use]
pub fn fk_definition_column_group(definition: &str, offset: usize) -> Option<(Vec<String>, usize)> {
    let open = definition[offset..].find('(')? + offset;
    let mut in_quote = false;
    let mut chars = definition[open + 1..].char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        match ch {
            '"' => {
                if matches!(chars.peek(), Some((_, '"'))) {
                    let _ = chars.next();
                } else {
                    in_quote = !in_quote;
                }
            }
            ')' if !in_quote => {
                let close = open + 1 + idx;
                return Some((
                    split_constraintdef_column_list(&definition[open + 1..close]),
                    close + 1,
                ));
            }
            _ => {}
        }
    }
    None
}

/// Extract the bare referenced table from a canonical foreign-key definition.
#[must_use]
pub fn fk_target_table(definition: &str) -> Option<String> {
    let after = definition.split("REFERENCES").nth(1)?.trim_start();
    // The target token is up to the first '(' or whitespace (e.g. `prj.authors`).
    let end = after
        .find(|c: char| c == '(' || c.is_whitespace())
        .unwrap_or(after.len());
    let qualified = after[..end].trim();
    // Strip a `<schema>.` prefix to get the bare table. The table part may be
    // quoted (`"My Table"`) even when the schema is not; handle a quoted tail.
    let bare = match qualified.rsplit_once('.') {
        Some((_schema, table)) => table,
        None => qualified,
    };
    let target = bare.trim().trim_matches('"');
    if target.is_empty() {
        None
    } else {
        Some(target.to_string())
    }
}

/// Extract the policy tail following the referenced-column group in a canonical
/// foreign-key definition.
#[must_use]
pub fn fk_policy_tail(definition: &str) -> String {
    let Some(after_ref) = definition.split_once("REFERENCES") else {
        return String::new();
    };
    let offset = definition.len() - after_ref.1.len();
    fk_definition_column_group(definition, offset)
        .map(|(_, end)| definition[end..].to_string())
        .unwrap_or_default()
}

/// Extract the local columns from a canonical foreign-key definition.
#[must_use]
pub fn fk_local_columns(definition: &str) -> Vec<String> {
    fk_definition_column_group(definition, 0)
        .map(|(cols, _)| cols)
        .unwrap_or_default()
}

/// Extract the referenced columns from a canonical foreign-key definition.
#[must_use]
pub fn fk_referenced_columns(definition: &str) -> Vec<String> {
    let Some(after_ref) = definition.split_once("REFERENCES") else {
        return vec!["id".to_string()];
    };
    let offset = definition.len() - after_ref.1.len();
    fk_definition_column_group(definition, offset)
        .map(|(cols, _)| {
            if cols.is_empty() {
                vec!["id".to_string()]
            } else {
                cols
            }
        })
        .unwrap_or_else(|| vec!["id".to_string()])
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
// They are dialect-neutral: every helper left here has one spelling shared by all
// three backends. Nothing here resolves a vendor or names one it was not handed,
// so the boundary rule in `zero_migrate::render::backends`'s header is unchanged —
// the caller has already decided which vendor it is.
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

/// The columns of `table`'s PRIMARY KEY, read off the implicit unique index the
/// snapshot carries under the name `policy` gives it, or `None` when the table has
/// no PK.
#[must_use]
pub fn primary_key_columns<'a>(
    policy: &dyn CatalogFoldPolicy,
    table: &str,
    t: &'a TableSnapshot,
) -> Option<&'a [String]> {
    let pk_name = policy.implicit_primary_key_name(table);
    t.indexes
        .iter()
        .find(|idx| idx.name == pk_name && idx.unique)
        .map(|idx| idx.columns.as_slice())
}

/// Whether `column` is the WHOLE primary key, and therefore takes the PK inline on
/// its own column clause rather than as a table-level constraint.
#[must_use]
pub fn inline_pk_for_column(
    policy: &dyn CatalogFoldPolicy,
    table: &str,
    t: &TableSnapshot,
    column: &str,
) -> bool {
    matches!(primary_key_columns(policy, table, t), Some(cols) if cols == [column])
}

/// Whether a PRIMARY KEY constraint must be rendered as a TABLE-level clause —
/// true for a composite PK, false for the single-column case
/// [`inline_pk_for_column`] already inlined.
#[must_use]
pub fn should_render_table_pk(
    policy: &dyn CatalogFoldPolicy,
    table: &str,
    t: &TableSnapshot,
    constraint: &ConstraintSnapshot,
) -> bool {
    constraint.kind == "PRIMARY KEY"
        && !matches!(primary_key_columns(policy, table, t), Some(cols) if cols.len() == 1)
}
