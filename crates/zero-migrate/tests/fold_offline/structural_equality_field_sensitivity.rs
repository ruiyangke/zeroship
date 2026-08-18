//! **THE FAILURE DIRECTION**, made into a runnable measurement.
//!
//! `docs/proposals/single-fold-and-effects.md` section D:
//!
//! > A new field is compared by default rather than silently ignored, so the failure
//! > direction flips from "invisible" to "noisy".
//!
//! That is a property, and this file is the property test. It asks ONE question of a
//! type: **if I change exactly one field, does `==` notice?** A type that answers "yes"
//! for every field cannot silently ignore a field added tomorrow. A type that answers
//! "no" for eleven of twenty-one already does, right now, for eleven fields.
//!
//! ## Why this is not a compile-error test
//!
//! Adding a field to `crates/zero-migrate/src/model/schema_model.rs` is ALSO a compile
//! error in every named comparator there, because each destructures with no `..`. That
//! is a good tripwire and it is deliberately not what this file measures, because a
//! compile error only fires for someone editing THIS crate. The property below fires on
//! the VALUE, so it also holds for a field whose comparison was changed rather than
//! added, and it produces a failure message naming the field rather than a type error
//! naming a pattern.
//!
//! ## What each probe list is
//!
//! Each list destructures its type with NO `..` rest pattern and routes every binding
//! through [`ProbeSet::probe`]. So a field added to `ColumnSnapshot` or to
//! `schema_model::Column` is a compile error HERE until a mutation for it exists, and a
//! field with a mutation that does not change equality is a test FAILURE. Two gates,
//! different failure modes, neither sufficient alone - the same shape
//! `tests/support/carriers.rs` uses for the rename carrier inventory, carried across to
//! its sibling problem rather than reinvented.
//!
//! ## The recorded status quo
//!
//! [`SILENTLY_IGNORED_BY_COLUMN_SNAPSHOT_EQ`] is not a threshold to be relaxed. It is the
//! measurement that motivates the whole change, pinned so that a future edit to
//! `ColumnSnapshot::eq` cannot quietly move it in either direction: adding a field to
//! that impl without updating the list fails, and so does removing one.

use crate::support::field_probes::{
    self, base_constraint_snapshot, base_index_snapshot, base_table_snapshot, ProbeSet,
};
use zero_migrate::model::schema_model;
use zero_migrate::{
    ColumnCollationSnapshot, ColumnSnapshot, GeneratedColumnSnapshot, GeneratedKindSnapshot,
    IdDefaultSnapshot, IdentityCol, IndexSortOrder, IndexStorageParams, TableStrictness,
    ValueFormat,
};

/// Run the property: every declared field must change `==`.
///
/// Returns the fields that did NOT, in declaration order, so the caller can either
/// assert emptiness (the model) or assert the exact recorded set (today's snapshot
/// types).
fn fields_invisible_to_equality<T: Clone + PartialEq + std::fmt::Debug>(
    base: &T,
    set: &ProbeSet<T>,
) -> Vec<&'static str> {
    let mut invisible = Vec::new();
    for probe in &set.probes {
        let mut mutated = base.clone();
        (probe.mutate)(&mut mutated);
        // A mutation that changes nothing is a BROKEN INSTRUMENT, and it fails toward a
        // false result in whichever direction the caller is asserting: it reads as
        // "invisible" for the model (a false RED) and as "compared" for the snapshot
        // types (a false GREEN). `Debug` is the only witness available, because
        // `PartialEq` is the thing under test and cannot be used to check its own input.
        assert_ne!(
            format!("{base:?}"),
            format!("{mutated:?}"),
            "the probe for `{}` did not actually change the value, so it can prove \
             nothing about equality. Fix the mutation, never the assertion.",
            probe.field
        );
        if *base == mutated {
            invisible.push(probe.field);
        }
    }
    invisible
}

/// Every `ColumnSnapshot` field that can differ while `==` reports EQUAL.
///
/// MEASURED, not asserted from the impl body. Eleven of twenty-one. Each one is
/// individually defensible - every entry has a reason in its own field doc - and the
/// point of section D is that being individually defensible is not the same as being
/// manageable, because the list is consulted implicitly by every consumer of column
/// equality and there are three different questions among them.
///
/// Four of the eleven are VENDOR facts (`catalog_uuid_format_check`,
/// `mysql_default_generated`, `mysql_text_storage`, `mysql_physical_type`). In the
/// neutral model those are not excluded, they are ABSENT - they live in
/// `schema_model::VendorFacts` and a neutral comparator cannot name them.
const SILENTLY_IGNORED_BY_COLUMN_SNAPSHOT_EQ: &[&str] = &[
    "ColumnSnapshot::default",
    "ColumnSnapshot::ddl_type_override",
    "ColumnSnapshot::inline_checks",
    "ColumnSnapshot::generated",
    "ColumnSnapshot::generated_kind",
    "ColumnSnapshot::catalog_uuid_format_check",
    "ColumnSnapshot::mysql_default_generated",
    "ColumnSnapshot::mysql_text_storage",
    "ColumnSnapshot::mysql_physical_type",
    "ColumnSnapshot::encryption_sentinel",
    "ColumnSnapshot::comment_sentinel",
];

// ---------------------------------------------------------------------------
// `schema_model::Column` - the type as it should be
// ---------------------------------------------------------------------------

fn model_column_probes() -> ProbeSet<schema_model::Column> {
    let mut set: ProbeSet<schema_model::Column> = ProbeSet::new();
    // EXHAUSTIVE, no `..`: a new `Column` field breaks this line.
    let schema_model::Column {
        name,
        data_type,
        nullable,
        default,
        ddl_type_override,
        inline_checks,
        generated,
        generated_kind,
        identity,
        value_format,
        id_default,
        case_sensitive,
        collation,
        encryption_sentinel,
        comment_sentinel,
        comment,
    } = schema_model::Column::default();

    set.probe("Column::name", name, |c| c.name = "renamed".to_string());
    set.probe("Column::data_type", data_type, |c| {
        c.data_type = "bigint".to_string();
    });
    set.probe("Column::nullable", nullable, |c| c.nullable = true);
    set.probe("Column::default", default, |c| {
        c.default = Some("now()".to_string());
    });
    set.probe("Column::ddl_type_override", ddl_type_override, |c| {
        c.ddl_type_override = Some("public.mood".to_string());
    });
    set.probe("Column::inline_checks", inline_checks, |c| {
        c.inline_checks = vec!["CHECK (x > 0)".to_string()];
    });
    set.probe("Column::generated", generated, |c| {
        c.generated = Some(GeneratedColumnSnapshot {
            expr: "(a + 1)".to_string(),
            source: None,
            stored: true,
        });
    });
    set.probe("Column::generated_kind", generated_kind, |c| {
        c.generated_kind = Some(GeneratedKindSnapshot::Stored);
    });
    set.probe("Column::identity", identity, |c| {
        c.identity = Some(IdentityCol { always: true });
    });
    set.probe("Column::value_format", value_format, |c| {
        c.value_format = Some(ValueFormat::Ulid);
    });
    set.probe("Column::id_default", id_default, |c| {
        c.id_default = Some(IdDefaultSnapshot::Absent);
    });
    set.probe("Column::case_sensitive", case_sensitive, |c| {
        c.case_sensitive = Some(false);
    });
    set.probe("Column::collation", collation, |c| {
        c.collation = Some(ColumnCollationSnapshot {
            schema: None,
            name: "C".to_string(),
        });
    });
    set.probe("Column::encryption_sentinel", encryption_sentinel, |c| {
        c.encryption_sentinel = Some("/* zero-migrate:enc */".to_string());
    });
    set.probe("Column::comment_sentinel", comment_sentinel, |c| {
        c.comment_sentinel = Some("zero-migrate:mask:kind=email".to_string());
    });
    set.probe("Column::comment", comment, |c| {
        c.comment = Some("a note".to_string());
    });

    set
}

fn model_index_probes() -> ProbeSet<schema_model::Index> {
    let mut set: ProbeSet<schema_model::Index> = ProbeSet::new();
    let base = base_index();
    // EXHAUSTIVE, no `..`: a new `Index` field breaks this line.
    let schema_model::Index {
        name,
        unique,
        columns,
        elements,
        access_method,
        predicate,
        include,
        with,
        comment,
        expr_cascade_columns,
    } = base;

    set.probe("Index::name", name, |i| i.name = "renamed_idx".to_string());
    set.probe("Index::unique", unique, |i| i.unique = true);
    set.probe("Index::columns", columns, |i| {
        i.columns = vec!["b".to_string()];
    });
    set.probe("Index::elements", elements, |i| {
        i.elements = vec![schema_model::IndexElement::Column {
            name: "b".to_string(),
            order: Some(IndexSortOrder::Desc),
        }];
    });
    set.probe("Index::access_method", access_method, |i| {
        i.access_method = "gin".to_string();
    });
    set.probe("Index::predicate", predicate, |i| {
        i.predicate = Some("(a > 0)".to_string());
    });
    set.probe("Index::include", include, |i| {
        i.include = vec!["c".to_string()];
    });
    set.probe("Index::with", with, |i| {
        i.with = Some(IndexStorageParams {
            pages_per_range: None,
            fillfactor: Some(70),
        });
    });
    set.probe("Index::comment", comment, |i| {
        i.comment = Some("a note".to_string());
    });
    set.probe("Index::expr_cascade_columns", expr_cascade_columns, |i| {
        i.expr_cascade_columns = Some(vec!["a".to_string()]);
    });

    set
}

fn model_constraint_probes() -> ProbeSet<schema_model::Constraint> {
    let mut set: ProbeSet<schema_model::Constraint> = ProbeSet::new();
    let base = base_constraint();
    // EXHAUSTIVE, no `..`: a new `Constraint` field breaks this line.
    let schema_model::Constraint {
        name,
        kind,
        definition,
        comment,
        cascade_columns,
    } = base;

    set.probe("Constraint::name", name, |c| {
        c.name = "renamed_ck".to_string();
    });
    set.probe("Constraint::kind", kind, |c| c.kind = "UNIQUE".to_string());
    set.probe("Constraint::definition", definition, |c| {
        c.definition = "CHECK ((a > 1))".to_string();
    });
    set.probe("Constraint::comment", comment, |c| {
        c.comment = Some("a note".to_string());
    });
    set.probe("Constraint::cascade_columns", cascade_columns, |c| {
        c.cascade_columns = Some(vec!["a".to_string()]);
    });

    set
}

fn model_table_probes() -> ProbeSet<schema_model::Table> {
    let mut set: ProbeSet<schema_model::Table> = ProbeSet::new();
    // EXHAUSTIVE, no `..`: a new `Table` field breaks this line.
    let schema_model::Table {
        columns,
        indexes,
        constraints,
        runtime_options,
        partition_by,
        comment,
    } = schema_model::Table::default();

    set.probe("Table::columns", columns, |t| {
        t.columns = vec![schema_model::Column::default()];
    });
    set.probe("Table::indexes", indexes, |t| {
        t.indexes = vec![base_index()];
    });
    set.probe("Table::constraints", constraints, |t| {
        t.constraints = vec![base_constraint()];
    });
    set.probe("Table::runtime_options", runtime_options, |t| {
        t.runtime_options.soft_delete = true;
        t.runtime_options.strictness = TableStrictness::Off;
    });
    set.probe("Table::partition_by", partition_by, |t| {
        t.partition_by = Some(zero_migrate::PartitionSpec::Range {
            columns: vec!["a".to_string()],
            collapse: false,
        });
    });
    set.probe("Table::comment", comment, |t| {
        t.comment = Some("a note".to_string());
    });

    set
}

fn base_index() -> schema_model::Index {
    schema_model::Index {
        name: "t_a_idx".to_string(),
        unique: false,
        columns: vec!["a".to_string()],
        elements: vec![schema_model::IndexElement::Column {
            name: "a".to_string(),
            order: None,
        }],
        access_method: "btree".to_string(),
        predicate: None,
        include: Vec::new(),
        with: None,
        comment: None,
        expr_cascade_columns: None,
    }
}

fn base_constraint() -> schema_model::Constraint {
    schema_model::Constraint {
        name: "t_a_check".to_string(),
        kind: "CHECK".to_string(),
        definition: "CHECK ((a > 0))".to_string(),
        comment: None,
        cascade_columns: None,
    }
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// **The RED, recorded.** This is the state of the tree before the neutral model, and
/// the reason section D exists. It is pinned in BOTH directions so it cannot drift: a
/// field added to `ColumnSnapshot::eq` fails here, and so does a field removed from it.
#[test]
fn eleven_of_twenty_one_column_snapshot_fields_are_invisible_to_equality_today() {
    let set = field_probes::column_snapshot_probes();
    assert_eq!(
        set.probes.len(),
        21,
        "the `ColumnSnapshot` probe list drifted from the type's field count"
    );

    let invisible = fields_invisible_to_equality(&ColumnSnapshot::default(), &set);
    assert_eq!(
        invisible, SILENTLY_IGNORED_BY_COLUMN_SNAPSHOT_EQ,
        "`ColumnSnapshot::eq`'s exclusion list changed. This is not a test to relax: \
         update the constant AND say in the review log which consumer the change was \
         for, because every consumer of column equality inherits it."
    );
}

/// **The GREEN.** Every field of the neutral column participates in structural
/// equality, so a field added tomorrow is compared by default and the mistake is noisy.
///
/// NEUTER: replace `#[derive(..., PartialEq)]` on `schema_model::Column` with a
/// hand-written `eq` that copies `ColumnSnapshot::eq`'s inclusion list, and this fails
/// naming the seven neutral fields it drops.
#[test]
fn every_field_of_the_neutral_column_is_compared_by_default() {
    let set = model_column_probes();
    assert_eq!(
        set.probes.len(),
        16,
        "the `Column` probe list drifted from the type's field count"
    );

    let invisible = fields_invisible_to_equality(&schema_model::Column::default(), &set);
    assert!(
        invisible.is_empty(),
        "`schema_model::Column` derives `PartialEq`, so NO field may be invisible to \
         `==`. These were: {invisible:?}. If a consumer wants one ignored, that belongs \
         in a NAMED comparator beside the others in `model/schema_model.rs`, never in \
         this type's equality."
    );
}

/// The same property for the other three model types.
#[test]
fn every_field_of_the_neutral_index_constraint_and_table_is_compared_by_default() {
    let index = model_index_probes();
    assert_eq!(index.probes.len(), 10, "`Index` probe list drifted");
    let invisible = fields_invisible_to_equality(&base_index(), &index);
    assert!(invisible.is_empty(), "`Index` ignored {invisible:?}");

    let constraint = model_constraint_probes();
    assert_eq!(
        constraint.probes.len(),
        5,
        "`Constraint` probe list drifted"
    );
    let invisible = fields_invisible_to_equality(&base_constraint(), &constraint);
    assert!(invisible.is_empty(), "`Constraint` ignored {invisible:?}");

    let table = model_table_probes();
    assert_eq!(table.probes.len(), 6, "`Table` probe list drifted");
    let invisible = fields_invisible_to_equality(&schema_model::Table::default(), &table);
    assert!(invisible.is_empty(), "`Table` ignored {invisible:?}");
}

/// The other three snapshot types, measured for the record.
///
/// `TableSnapshot` ignores two of seven; `IndexSnapshot` ignores four of thirteen;
/// `ConstraintSnapshot` ignores one of five. All seven shrink to ZERO in the model,
/// because every one is either a vendor fact that moved into `VendorFacts` or an
/// emission-only field that moved onto a named comparator.
#[test]
fn the_other_snapshot_types_ignore_seven_more_fields_between_them() {
    let table = field_probes::table_snapshot_probes();
    assert_eq!(table.probes.len(), 7, "`TableSnapshot` probe list drifted");
    assert_eq!(
        fields_invisible_to_equality(&base_table_snapshot(), &table),
        [
            "TableSnapshot::runtime_options",
            "TableSnapshot::stored_create_sql"
        ],
        "`TableSnapshot::eq`'s exclusion list changed"
    );

    let index = field_probes::index_snapshot_probes();
    assert_eq!(index.probes.len(), 13, "`IndexSnapshot` probe list drifted");
    assert_eq!(
        fields_invisible_to_equality(&base_index_snapshot(), &index),
        [
            "IndexSnapshot::only",
            "IndexSnapshot::opclass",
            "IndexSnapshot::nulls_not_distinct",
            "IndexSnapshot::expr_cascade_columns"
        ],
        "`IndexSnapshot::eq`'s exclusion list changed"
    );

    let constraint = field_probes::constraint_snapshot_probes();
    assert_eq!(
        constraint.probes.len(),
        5,
        "`ConstraintSnapshot` probe list drifted"
    );
    assert_eq!(
        fields_invisible_to_equality(&base_constraint_snapshot(), &constraint),
        ["ConstraintSnapshot::cascade_columns"],
        "`ConstraintSnapshot::eq`'s exclusion list changed"
    );
}
