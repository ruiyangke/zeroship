//! The RENAME CARRIER INVENTORY: every field of a `TableSnapshot` that can spell a
//! COLUMN NAME, enumerated by EXHAUSTIVE DESTRUCTURING rather than by a comment.
//!
//! A "carrier" is a place a column's name is stored as TEXT rather than as a
//! structured reference into the column list. When a column is renamed, every carrier
//! has to be rewritten, or the snapshot keeps spelling the OLD name and the engine
//! either reports phantom drift forever (when the field is compared) or emits SQL
//! naming a column that no longer exists (when the field is spliced into DDL).
//!
//! ## Why this is a PROOF and not a list
//!
//! `docs/proposals/single-fold-and-effects.md` section H asks for a rename carrier
//! sweep whose deliverable "FAILS when a new carrier is added and not handled", in the
//! spirit of its "Op exhaustiveness" item, and says explicitly that a list in a
//! comment is not a proof. The mechanism here is the language's own:
//!
//!   * every snapshot type is destructured with NO `..` REST PATTERN, so adding a
//!     field to `TableSnapshot`, `ColumnSnapshot`, `GeneratedColumnSnapshot`,
//!     `IndexSnapshot`, `IndexElementSnapshot` or `ConstraintSnapshot` is a COMPILE
//!     ERROR here until it is classified;
//!   * every binding must then be routed through exactly one of three classifiers -
//!     [`CarrierSet::field`], [`never_a_column_name`] or [`must_not_follow`] - each of
//!     which demands a reason from the author;
//!   * and `rename_carrier_sweep_pg` / `rename_carrier_sweep_sqlite` assert that every
//!     path [`CarrierSet::field`] declares is actually POPULATED by their fixture, so
//!     classifying a new field as a carrier and then never exercising it is a test
//!     failure too, not a silent hole.
//!
//! The failure mode this closes is the one `docs/review-log.md` records at F113: four
//! carriers of the same arm were found ONE AT A TIME, each by the corpus rather than by
//! a survey, because "having fixed one instance actively suppresses the search for the
//! others".
//!
//! ## The classification, and what each class costs to get wrong
//!
//! * **carrier** - a rename must follow it. Getting this wrong leaves a snapshot
//!   naming a dead column.
//! * **`never_a_column_name`** - the field cannot hold one (a bool, a type token, a
//!   collation name, an operator class, free prose). Getting this wrong is the same
//!   failure as missing a carrier, which is why each one carries its reason inline.
//! * **`must_not_follow`** - the field CAN spell a column name, but rewriting it would
//!   invent state the live database does not have. Both members are MEASURED on
//!   PostgreSQL 18.4 rather than assumed: an index or constraint created as `t_a_idx`
//!   over `a` is still named `t_a_idx` after `a` becomes `b`.

use std::collections::BTreeMap;

use zero_migrate::{
    ColumnSnapshot, ConstraintSnapshot, GeneratedColumnSnapshot, IndexElementSnapshot,
    IndexSnapshot, SchemaSnapshot, TableSnapshot,
};

/// One carrier site: the field path, and every string it currently spells.
#[derive(Debug, Clone)]
pub struct Carrier {
    /// The field path, e.g. `TableSnapshot::indexes[].predicate`.
    pub field: &'static str,
    /// Every string this field holds across the snapshot the inventory walked. A
    /// carrier declared but not populated by a fixture reports an EMPTY vec, which the
    /// sweep tests treat as a failure - an unexercised carrier proves nothing.
    pub spellings: Vec<String>,
}

/// The accumulator. Aggregates by field path, so walking twenty columns still yields
/// ONE `TableSnapshot::columns[].name` entry holding twenty spellings.
#[derive(Default)]
pub struct CarrierSet {
    by_field: BTreeMap<&'static str, Vec<String>>,
}

impl CarrierSet {
    /// Declare one CARRIER field and contribute whatever it currently spells.
    ///
    /// Always call this, even when the value is `None` / empty: the path is what makes
    /// the inventory a fixed checklist, and an empty entry is exactly the signal the
    /// sweep tests use to catch a carrier no fixture exercises.
    pub fn field<I: IntoIterator<Item = String>>(&mut self, field: &'static str, spellings: I) {
        self.by_field.entry(field).or_default().extend(spellings);
    }

    /// The inventory, ordered by field path.
    #[must_use]
    pub fn into_carriers(self) -> Vec<Carrier> {
        self.by_field
            .into_iter()
            .map(|(field, spellings)| Carrier { field, spellings })
            .collect()
    }
}

/// Classify a field as UNABLE to hold a column name. `why` is not decoration: this is
/// the arm that silently drops a carrier when it is wrong, so the reason has to survive
/// review next to the binding it excuses.
fn never_a_column_name<T>(_field: &'static str, _value: T, _why: &'static str) {}

/// Classify a field that CAN spell a column name but that a rename must NOT follow,
/// because following it would invent state the live database does not have. `measured`
/// records the observation that settles it.
fn must_not_follow<T>(_field: &'static str, _value: T, _measured: &'static str) {}

/// Walk EVERY table of a snapshot and return the aggregated carrier inventory.
///
/// The union across tables is deliberate: no single PostgreSQL table can hold every
/// carrier at once (a partitioned table accepts no EXCLUDE constraint), so the sweep
/// fixtures spread the carriers over more than one table and rely on this union to make
/// the checklist whole again.
#[must_use]
pub fn carriers_of_schema(snapshot: &SchemaSnapshot) -> Vec<Carrier> {
    let mut set = CarrierSet::default();
    for table in snapshot.tables.values() {
        walk_table(table, &mut set);
    }
    set.into_carriers()
}

/// Walk one table's carriers into `set`.
pub fn walk_table(table: &TableSnapshot, set: &mut CarrierSet) {
    // EXHAUSTIVE, no `..`: a new `TableSnapshot` field breaks this line.
    let TableSnapshot {
        columns,
        indexes,
        constraints,
        runtime_options,
        partition_by,
        comment,
        stored_create_sql,
    } = table;

    never_a_column_name(
        "TableSnapshot::runtime_options",
        runtime_options,
        "`TableRuntimeOptions` is two bools and a strictness enum (`soft_delete`, \
         `versioning`, `strictness`). Soft delete IMPLIES a deleted-at column, but the \
         option does not spell its name - the column is materialised into `columns` \
         like any other and is renamed there.",
    );
    never_a_column_name(
        "TableSnapshot::comment",
        comment,
        "a user-authored catalog comment is free prose. A comment that MENTIONS a \
         column is not a reference to it, and PostgreSQL does not rewrite \
         `obj_description` on a rename either (measured on 18.4), so following it \
         would diverge from live rather than converge on it.",
    );
    must_not_follow(
        "TableSnapshot::stored_create_sql",
        stored_create_sql,
        "SQLite's verbatim `sqlite_master.sql`, which spells every column name. It is \
         INTROSPECTION-ONLY - the fold never produces one (asserted directly by the \
         sweep tests, not assumed here) - and SQLite rewrites its own stored text on \
         both `ALTER TABLE ... RENAME COLUMN` and the 12-step rebuild. Following it \
         offline would fabricate a `CREATE TABLE` text no catalog ever stored.",
    );

    // A CARRIER the section-H list does not name, and the one with the worst failure
    // mode of the set: `TableSnapshot::eq` COMPARES `partition_by`, so a stale
    // partition key is drift the differ REPORTS, on every introspection, forever.
    // Measured on PostgreSQL 18.4: `pg_get_partkeydef` reads `partattrs` as attribute
    // NUMBERS, so `RANGE (created_at)` becomes `RANGE (event_day)` the instant the
    // rename commits.
    set.field(
        "TableSnapshot::partition_by.columns",
        partition_by
            .iter()
            .flat_map(|spec| spec.columns().iter().cloned()),
    );

    for column in columns {
        walk_column(column, set);
    }
    for index in indexes {
        walk_index(index, set);
    }
    for constraint in constraints {
        walk_constraint(constraint, set);
    }
}

fn walk_column(column: &ColumnSnapshot, set: &mut CarrierSet) {
    // EXHAUSTIVE, no `..`: a new `ColumnSnapshot` field breaks this line.
    let ColumnSnapshot {
        name,
        data_type,
        nullable,
        default,
        ddl_type_override,
        inline_checks,
        generated,
        generated_kind,
        identity,
        sqlite_rowid,
        value_format,
        catalog_uuid_format_check,
        id_default,
        mysql_default_generated,
        case_sensitive,
        collation,
        mysql_text_storage,
        mysql_physical_type,
        encryption_sentinel,
        comment_sentinel,
        comment,
    } = column;

    set.field("TableSnapshot::columns[].name", [name.clone()]);
    set.field(
        "TableSnapshot::columns[].inline_checks",
        inline_checks.iter().cloned(),
    );

    never_a_column_name(
        "TableSnapshot::columns[].data_type",
        data_type,
        "a TYPE name (`text`, `integer`, `timestamp with time zone`), never a column.",
    );
    never_a_column_name("TableSnapshot::columns[].nullable", nullable, "a bool.");
    never_a_column_name(
        "TableSnapshot::columns[].default",
        default,
        "a column DEFAULT cannot reference another column on any dialect this crate \
         targets: PostgreSQL rejects a non-constant column reference in `DEFAULT`, and \
         MySQL's expression defaults are restricted the same way. So the text can \
         spell a FUNCTION or a literal, never a sibling column.",
    );
    never_a_column_name(
        "TableSnapshot::columns[].ddl_type_override",
        ddl_type_override,
        "a dialect-rendered TYPE spelling (`DECIMAL(30, 10)`, a schema-qualified enum \
         name).",
    );
    never_a_column_name(
        "TableSnapshot::columns[].generated_kind",
        generated_kind,
        "the STRUCTURAL half of `generated`: a three-state enum over \
         `pg_attribute.attgenerated`. It survives a rename by construction.",
    );
    never_a_column_name(
        "TableSnapshot::columns[].identity",
        identity,
        "`IdentityCol` is a single `always: bool`.",
    );
    never_a_column_name(
        "TableSnapshot::columns[].sqlite_rowid",
        sqlite_rowid,
        "a bool.",
    );
    never_a_column_name(
        "TableSnapshot::columns[].value_format",
        value_format,
        "a closed ID-format enum plus a TypeID PREFIX. The prefix is an application \
         namespace (`user`, `org`), not a column reference.",
    );
    never_a_column_name(
        "TableSnapshot::columns[].catalog_uuid_format_check",
        catalog_uuid_format_check,
        "a bool.",
    );
    never_a_column_name(
        "TableSnapshot::columns[].id_default",
        id_default,
        "the narrow semantic projection of `default` - a literal or a recognised ID \
         generator - and `default` itself cannot name a column.",
    );
    never_a_column_name(
        "TableSnapshot::columns[].mysql_default_generated",
        mysql_default_generated,
        "an `Option<bool>`.",
    );
    never_a_column_name(
        "TableSnapshot::columns[].case_sensitive",
        case_sensitive,
        "an `Option<bool>`.",
    );
    never_a_column_name(
        "TableSnapshot::columns[].collation",
        collation,
        "a COLLATION's schema and name (`pg_catalog`.`C`), which is a distinct catalog \
         object from a column.",
    );
    never_a_column_name(
        "TableSnapshot::columns[].mysql_text_storage",
        mysql_text_storage,
        "a closed MySQL storage-class enum.",
    );
    never_a_column_name(
        "TableSnapshot::columns[].mysql_physical_type",
        mysql_physical_type,
        "a closed MySQL physical-type enum.",
    );
    never_a_column_name(
        "TableSnapshot::columns[].encryption_sentinel",
        encryption_sentinel,
        "the AEAD contract marker gen-types reads. It names the ENCRYPTION scheme, not \
         a column, and it travels with the column it is stamped on.",
    );
    never_a_column_name(
        "TableSnapshot::columns[].comment_sentinel",
        comment_sentinel,
        "the `zero-migrate:`-prefixed marker half of a column comment. Like \
         `encryption_sentinel` it describes THIS column's contract and carries no \
         reference to another.",
    );
    never_a_column_name(
        "TableSnapshot::columns[].comment",
        comment,
        "free prose, for the same reason `TableSnapshot::comment` is.",
    );

    if let Some(generated) = generated {
        // EXHAUSTIVE, no `..`: a new `GeneratedColumnSnapshot` field breaks this line.
        let GeneratedColumnSnapshot {
            expr,
            source,
            stored,
        } = generated;
        set.field("TableSnapshot::columns[].generated.expr", [expr.clone()]);
        // The AST beside the rendering. It is the carrier the rendered `expr` is
        // REGENERATED from, so it has to follow the rename first; a `source` naming a
        // dead column would re-render a body naming a dead column on the next call.
        set.field(
            "TableSnapshot::columns[].generated.source",
            source.iter().map(|source| format!("{source:?}")),
        );
        never_a_column_name(
            "TableSnapshot::columns[].generated.stored",
            stored,
            "a bool: STORED vs VIRTUAL.",
        );
    } else {
        set.field("TableSnapshot::columns[].generated.expr", []);
        set.field("TableSnapshot::columns[].generated.source", []);
    }
}

fn walk_index(index: &IndexSnapshot, set: &mut CarrierSet) {
    // EXHAUSTIVE, no `..`: a new `IndexSnapshot` field breaks this line.
    let IndexSnapshot {
        name,
        unique,
        columns,
        elements,
        access_method,
        predicate,
        include,
        with,
        only,
        opclass,
        nulls_not_distinct,
        comment,
        expr_cascade_columns,
    } = index;

    must_not_follow(
        "TableSnapshot::indexes[].name",
        name,
        "an index NAME commonly spells the column it covers, and PostgreSQL does NOT \
         rename it with the column - measured on 18.4, an index created as `t_a_idx` \
         over `a` is still `t_a_idx` after `a` becomes `b`. Rewriting it would invent \
         an index live does not have.",
    );
    never_a_column_name("TableSnapshot::indexes[].unique", unique, "a bool.");
    never_a_column_name(
        "TableSnapshot::indexes[].access_method",
        access_method,
        "a `pg_am.amname` (`btree`, `gin`, `gist`, `hnsw`).",
    );
    never_a_column_name(
        "TableSnapshot::indexes[].with",
        with,
        "`IndexStorageParams` is two integers (`pages_per_range`, `fillfactor`).",
    );
    never_a_column_name("TableSnapshot::indexes[].only", only, "a bool.");
    never_a_column_name(
        "TableSnapshot::indexes[].opclass",
        opclass,
        "an OPERATOR CLASS name (`vector_cosine_ops`), a distinct catalog object.",
    );
    never_a_column_name(
        "TableSnapshot::indexes[].nulls_not_distinct",
        nulls_not_distinct,
        "a bool.",
    );
    never_a_column_name("TableSnapshot::indexes[].comment", comment, "free prose.");

    set.field("TableSnapshot::indexes[].columns", columns.iter().cloned());
    set.field("TableSnapshot::indexes[].include", include.iter().cloned());
    set.field(
        "TableSnapshot::indexes[].predicate",
        predicate.iter().cloned(),
    );
    set.field(
        "TableSnapshot::indexes[].expr_cascade_columns",
        expr_cascade_columns.iter().flatten().cloned(),
    );

    for element in elements {
        // EXHAUSTIVE, no `_ =>` arm and no `..`: a new variant or a new field on
        // either variant breaks this match.
        match element {
            IndexElementSnapshot::Column {
                name,
                order,
                opclass,
                collation,
            } => {
                set.field(
                    "TableSnapshot::indexes[].elements[].Column.name",
                    [name.clone()],
                );
                never_a_column_name(
                    "TableSnapshot::indexes[].elements[].Column.order",
                    order,
                    "an ASC/DESC + NULLS ordering enum.",
                );
                never_a_column_name(
                    "TableSnapshot::indexes[].elements[].Column.opclass",
                    opclass,
                    "a per-column OPERATOR CLASS name (`text_pattern_ops`).",
                );
                never_a_column_name(
                    "TableSnapshot::indexes[].elements[].Column.collation",
                    collation,
                    "a per-column COLLATION name (`\"C\"`).",
                );
            }
            IndexElementSnapshot::Expr(expr) => {
                set.field("TableSnapshot::indexes[].elements[].Expr", [expr.clone()]);
            }
        }
    }
    // Declared unconditionally so an index with no expression key still puts the path
    // on the checklist.
    set.field("TableSnapshot::indexes[].elements[].Column.name", []);
    set.field("TableSnapshot::indexes[].elements[].Expr", []);
}

fn walk_constraint(constraint: &ConstraintSnapshot, set: &mut CarrierSet) {
    // EXHAUSTIVE, no `..`: a new `ConstraintSnapshot` field breaks this line.
    let ConstraintSnapshot {
        name,
        kind,
        definition,
        comment,
        cascade_columns,
    } = constraint;

    must_not_follow(
        "TableSnapshot::constraints[].name",
        name,
        "a constraint NAME commonly spells its column, and PostgreSQL does not rename \
         it with the column - measured on 18.4 alongside the index name above.",
    );
    never_a_column_name(
        "TableSnapshot::constraints[].kind",
        kind,
        "one of `PRIMARY KEY`, `FOREIGN KEY`, `UNIQUE`, `CHECK`, `EXCLUDE`.",
    );
    never_a_column_name(
        "TableSnapshot::constraints[].comment",
        comment,
        "free prose.",
    );

    // Split by KIND, because the failure mode and the sound rewrite differ per kind and
    // a single `definition` entry would let a fixed kind mask a broken one. Every kind
    // is routed, so a new one added to the vocabulary lands in the catch-all and is
    // swept rather than silently skipped.
    let field = match kind.as_str() {
        "PRIMARY KEY" => "TableSnapshot::constraints[].definition (PRIMARY KEY)",
        "FOREIGN KEY" => "TableSnapshot::constraints[].definition (FOREIGN KEY)",
        "UNIQUE" => "TableSnapshot::constraints[].definition (UNIQUE)",
        "CHECK" => "TableSnapshot::constraints[].definition (CHECK)",
        "EXCLUDE" => "TableSnapshot::constraints[].definition (EXCLUDE)",
        _ => "TableSnapshot::constraints[].definition (other)",
    };
    set.field(field, [definition.clone()]);
    set.field(
        "TableSnapshot::constraints[].cascade_columns",
        cascade_columns.iter().flatten().cloned(),
    );
}

/// Every carrier path the inventory can declare. The sweep tests assert their fixture
/// populates ALL of them, which is what stops a newly classified carrier from sitting
/// unexercised.
///
/// `EXCLUDE` is deliberately absent: the fold writes an EMPTY `definition` for an
/// exclusion constraint on purpose (PostgreSQL canonicalises the body differently from
/// the offline renderer, so drift tracks an EXCLUDE by name + kind), and the sweep
/// asserts that emptiness directly rather than listing a path that can never carry a
/// spelling.
pub const REQUIRED_CARRIER_FIELDS: &[&str] = &[
    "TableSnapshot::columns[].generated.expr",
    "TableSnapshot::columns[].generated.source",
    "TableSnapshot::columns[].inline_checks",
    "TableSnapshot::columns[].name",
    "TableSnapshot::constraints[].cascade_columns",
    "TableSnapshot::constraints[].definition (CHECK)",
    "TableSnapshot::constraints[].definition (FOREIGN KEY)",
    "TableSnapshot::constraints[].definition (PRIMARY KEY)",
    "TableSnapshot::constraints[].definition (UNIQUE)",
    "TableSnapshot::indexes[].columns",
    "TableSnapshot::indexes[].elements[].Column.name",
    "TableSnapshot::indexes[].elements[].Expr",
    "TableSnapshot::indexes[].expr_cascade_columns",
    "TableSnapshot::indexes[].include",
    "TableSnapshot::indexes[].predicate",
    "TableSnapshot::partition_by.columns",
];
