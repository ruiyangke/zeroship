//! **The behaviour-preservation kernel for `schema_model`**, shared by the PostgreSQL
//! and MySQL legs.
//!
//! Step 2 of `docs/proposals/single-fold-and-effects.md` section G adds a neutral model,
//! a vendor side table, and one named comparator per question. It is required to change
//! NOTHING. Two claims make that falsifiable, and both are checked here against
//! snapshots taken from a live server through engine-emitted SQL rather than against
//! hand-built fixtures:
//!
//! 1. **The split is LOSSLESS.** `TableSnapshot -> (Table, VendorFacts) -> TableSnapshot`
//!    is the identity. If it is not, the neutral model cannot serve catalog identity and
//!    the spike is refuted at its first step.
//! 2. **Every named comparator answers IDENTICALLY** to the hand-written `eq` it was
//!    extracted from, on every ordered pair of real objects.
//!
//! ## Why the round trip is compared through `Debug` and not through `==`
//!
//! `==` is the lossy thing under test. `ColumnSnapshot::eq` ignores eleven of twenty-one
//! fields, so a round trip that dropped `mysql_physical_type` would compare EQUAL and
//! the test would pass while the model silently lost a vendor fact. `Debug` is total on
//! these types - `ColumnSnapshot`'s hand-written impl prints all twenty-one, hiding only
//! ones that are at their default, and a value that went from present to lost moves from
//! shown to hidden, so the strings differ. Every other type derives it.
//!
//! ## Why every assertion also counts
//!
//! A comparator equivalence that compared zero pairs, or that found every pair unequal,
//! would pass while proving nothing. [`Coverage`] is returned and asserted by the
//! callers, so a fixture that stops populating a facet fails here rather than quietly
//! reducing what the run measured.

use std::collections::BTreeMap;

use zero_migrate::model::schema_model::{
    self, column_shape_identity, constraint_shape_identity, drift_identity, index_pairing_identity,
    index_shape_identity, table_shape_identity, SchemaModel,
};
use zero_migrate::{ColumnSnapshot, ConstraintSnapshot, IndexSnapshot, TableSnapshot};

/// What one equivalence run actually looked at. Asserted by the callers so a shrinking
/// fixture is a failure rather than a quieter pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Coverage {
    /// Ordered column pairs compared.
    pub column_pairs: usize,
    /// Of those, how many the comparator called the SAME column.
    pub column_pairs_equal: usize,
    /// Ordered index pairs compared.
    pub index_pairs: usize,
    /// Of those, how many `index_pairing_identity` accepted.
    pub index_pairs_paired: usize,
    /// Ordered constraint pairs compared.
    pub constraint_pairs: usize,
    /// Ordered table pairs compared.
    pub table_pairs: usize,
    /// Column pairs on which `drift_identity` and `column_shape_identity` DISAGREED.
    ///
    /// Expected to be zero on real introspected data: the two differ only on
    /// `generated_kind`, and a producer that populates it populates it on both sides.
    /// The disagreement is demonstrated separately, on a real column mutated the way the
    /// real event mutates it.
    pub drift_stricter_than_shape: usize,
}

/// Every table in `tables`, re-keyed with `prefix` so two snapshots can share one
/// [`SchemaModel`] without their vendor keys colliding.
fn prefixed(
    prefix: &str,
    tables: &BTreeMap<String, TableSnapshot>,
) -> BTreeMap<String, TableSnapshot> {
    tables
        .iter()
        .map(|(name, table)| (format!("{prefix}{name}"), table.clone()))
        .collect()
}

/// **Claim 1.** The neutral/vendor split loses nothing.
///
/// # Panics
/// When the round trip is not the identity, naming the table whose `Debug` changed.
pub fn assert_roundtrip_is_lossless(label: &str, tables: &BTreeMap<String, TableSnapshot>) {
    assert!(
        !tables.is_empty(),
        "{label}: the round-trip check was handed an EMPTY table map, so it proves \
         nothing. A vacuous pass is worse than a failure here."
    );
    let rebuilt = SchemaModel::from_tables(tables).to_tables();
    for (name, original) in tables {
        let round_tripped = rebuilt.get(name).unwrap_or_else(|| {
            panic!("{label}: table {name:?} did not survive the round trip at all")
        });
        // Compared per OBJECT rather than per table, so the failure names the column or
        // index that lost a field instead of printing two whole tables and leaving the
        // reader to diff them.
        assert_eq!(
            original.columns.len(),
            round_tripped.columns.len(),
            "{label}: table {name:?} changed column COUNT across the round trip"
        );
        for (before, after) in original.columns.iter().zip(&round_tripped.columns) {
            assert_eq!(
                format!("{before:#?}"),
                format!("{after:#?}"),
                "{label}: `ColumnSnapshot -> (Column, VendorFacts) -> ColumnSnapshot` is \
                 not the identity for {name}.{}. A field was dropped by the split, so \
                 the neutral model cannot serve catalog identity.",
                before.name
            );
        }
        for (before, after) in original.indexes.iter().zip(&round_tripped.indexes) {
            assert_eq!(
                format!("{before:#?}"),
                format!("{after:#?}"),
                "{label}: the round trip is not the identity for index {}",
                before.name
            );
        }
        assert_eq!(
            format!("{original:#?}"),
            format!("{round_tripped:#?}"),
            "{label}: `TableSnapshot -> (Table, VendorFacts) -> TableSnapshot` is not \
             the identity for table {name:?} at TABLE level, though every column and \
             index round-tripped. A table-level field was dropped."
        );
    }
    assert_eq!(
        tables.len(),
        rebuilt.len(),
        "{label}: the round trip changed the table COUNT"
    );
}

/// **Claim 2.** Every named comparator answers identically to the `eq` it replaces, on
/// every ordered pair drawn from both snapshots.
///
/// # Panics
/// On the first disagreement, naming the objects and both verdicts.
#[must_use]
pub fn assert_comparators_match_partial_eq(
    label: &str,
    live: &BTreeMap<String, TableSnapshot>,
    folded: &BTreeMap<String, TableSnapshot>,
) -> Coverage {
    let mut all = prefixed("live:", live);
    all.extend(prefixed("folded:", folded));
    let model = SchemaModel::from_tables(&all);

    let mut coverage = Coverage::default();

    // ---- columns -----------------------------------------------------------
    let columns: Vec<(&String, &ColumnSnapshot, &schema_model::Column)> = all
        .iter()
        .flat_map(|(table_name, table)| {
            let modelled = &model.tables[table_name];
            table
                .columns
                .iter()
                .zip(&modelled.columns)
                .map(move |(snapshot, column)| (table_name, snapshot, column))
        })
        .collect();

    for (left_table, left_snapshot, left_column) in &columns {
        for (right_table, right_snapshot, right_column) in &columns {
            coverage.column_pairs += 1;
            let by_eq = left_snapshot == right_snapshot;
            let by_comparator =
                model.column_shape_identity(left_table, left_column, right_table, right_column);
            assert_eq!(
                by_eq, by_comparator,
                "{label}: `column_shape_identity` disagreed with `ColumnSnapshot::eq` \
                 for {left_table}.{} vs {right_table}.{}\n  eq         = {by_eq}\n  \
                 comparator = {by_comparator}\n  left  = {left_snapshot:#?}\n  right = \
                 {right_snapshot:#?}",
                left_snapshot.name, right_snapshot.name
            );
            if by_eq {
                coverage.column_pairs_equal += 1;
            }

            // `drift_identity` must be STRICTLY STRONGER, never weaker. A pair it calls
            // the same column that shape identity calls different would mean the
            // extraction lost a comparison rather than added one.
            let by_drift =
                model.column_drift_identity(left_table, left_column, right_table, right_column);
            assert!(
                !by_drift || by_comparator,
                "{label}: `drift_identity` accepted a pair `column_shape_identity` \
                 rejected, for {left_table}.{} vs {right_table}.{}. Drift is defined as \
                 shape PLUS `generated_kind`, so it can only ever be stricter.",
                left_snapshot.name,
                right_snapshot.name
            );
            if by_comparator && !by_drift {
                coverage.drift_stricter_than_shape += 1;
            }
        }
    }

    // ---- indexes -----------------------------------------------------------
    let indexes: Vec<(&IndexSnapshot, &schema_model::Index)> = all
        .iter()
        .flat_map(|(table_name, table)| {
            let modelled = &model.tables[table_name];
            table.indexes.iter().zip(&modelled.indexes)
        })
        .collect();

    for (left_snapshot, left_index) in &indexes {
        for (right_snapshot, right_index) in &indexes {
            coverage.index_pairs += 1;

            assert_eq!(
                *left_snapshot == *right_snapshot,
                index_shape_identity(left_index, right_index),
                "{label}: `index_shape_identity` disagreed with `IndexSnapshot::eq` for \
                 {:?} vs {:?}",
                left_snapshot.name,
                right_snapshot.name
            );

            // `same_definition_except_name` is `pub(crate)`, so the name-free question is
            // asked through the public surface by renaming a clone: `IndexSnapshot::eq`
            // is literally `name == name && same_definition_except_name`, so with the
            // names forced equal the remaining answer IS the pairing answer.
            let mut renamed = (*left_snapshot).clone();
            renamed.name.clone_from(&right_snapshot.name);
            let by_eq = renamed == **right_snapshot;
            let by_comparator = index_pairing_identity(left_index, right_index);
            assert_eq!(
                by_eq, by_comparator,
                "{label}: `index_pairing_identity` disagreed with \
                 `same_definition_except_name` for {:?} vs {:?}\n  left  = \
                 {left_snapshot:#?}\n  right = {right_snapshot:#?}",
                left_snapshot.name, right_snapshot.name
            );
            if by_comparator {
                coverage.index_pairs_paired += 1;
            }
        }
    }

    // ---- constraints -------------------------------------------------------
    let constraints: Vec<(&ConstraintSnapshot, &schema_model::Constraint)> = all
        .iter()
        .flat_map(|(table_name, table)| {
            let modelled = &model.tables[table_name];
            table.constraints.iter().zip(&modelled.constraints)
        })
        .collect();

    for (left_snapshot, left_constraint) in &constraints {
        for (right_snapshot, right_constraint) in &constraints {
            coverage.constraint_pairs += 1;
            assert_eq!(
                *left_snapshot == *right_snapshot,
                constraint_shape_identity(left_constraint, right_constraint),
                "{label}: `constraint_shape_identity` disagreed with \
                 `ConstraintSnapshot::eq` for {:?} vs {:?}",
                left_snapshot.name,
                right_snapshot.name
            );
        }
    }

    // ---- tables ------------------------------------------------------------
    for (left_name, left_snapshot) in &all {
        for (right_name, right_snapshot) in &all {
            coverage.table_pairs += 1;
            let by_eq = left_snapshot == right_snapshot;
            let by_comparator = model.table_shape_identity(
                left_name,
                &model.tables[left_name],
                right_name,
                &model.tables[right_name],
            );
            assert_eq!(
                by_eq, by_comparator,
                "{label}: `table_shape_identity` disagreed with `TableSnapshot::eq` for \
                 {left_name:?} vs {right_name:?}"
            );

            // `rename_equivalence_identity` is deliberately defined as table shape
            // today, because that is what `pure_sqlite_column_rename` asks through
            // `TableSnapshot::eq`. Pinning the two together is what makes a future
            // divergence a DECISION rather than an accident.
            assert_eq!(
                by_comparator,
                schema_model::rename_equivalence_identity(
                    &model.tables[left_name],
                    &model.tables[right_name]
                ),
                "{label}: `rename_equivalence_identity` diverged from \
                 `table_shape_identity` for {left_name:?} vs {right_name:?}. That may \
                 be correct one day, but it is a behaviour change and needs its own \
                 measurement in the review log."
            );
        }
    }

    coverage
}

/// **Claim 2b, and the one that makes each comparator's individual TERMS falsifiable.**
///
/// [`assert_comparators_match_partial_eq`] sweeps every ordered pair of REAL objects,
/// which is the right instrument for catching a comparator that ADDED a term - the added
/// term fires on a real pair the `eq` calls equal. It is a WEAK instrument for catching
/// one that DROPPED a term, and the weakness is specific: any two distinct real columns
/// differ in `name`, so the first term already decides and the later ones are never
/// reached. Only the (live X, folded X) pairs go deeper, and those agree on everything.
///
/// So this takes a REAL object out of a REAL snapshot and changes exactly ONE field,
/// using `support::field_probes` - the same exhaustive-destructure inventory the failure-
/// direction test uses, so a new field cannot escape either. Every term of every
/// comparator then has a pair that turns on it alone, and a dropped term is a failure
/// naming the field.
///
/// The model is rebuilt from the MUTATED snapshot rather than mutated directly, so the
/// two sides cannot drift apart: there is one probe list, not two.
///
/// # Panics
/// On the first field where the comparator and the `eq` disagree, naming it.
#[must_use]
pub fn assert_each_field_moves_both_verdicts_together(
    label: &str,
    tables: &BTreeMap<String, TableSnapshot>,
) -> usize {
    let mut checked = 0_usize;

    for (table_name, table) in tables {
        for (position, original) in table.columns.iter().enumerate() {
            for probe in &super::field_probes::column_snapshot_probes().probes {
                let mut mutated_tables = tables.clone();
                let mutated = &mut mutated_tables
                    .get_mut(table_name)
                    .expect("the table was just cloned")
                    .columns[position];
                (probe.mutate)(mutated);
                let mutated = mutated.clone();

                let mut both: BTreeMap<String, TableSnapshot> = prefixed("a:", tables);
                both.extend(prefixed("b:", &mutated_tables));
                let model = SchemaModel::from_tables(&both);
                let left_table = format!("a:{table_name}");
                let right_table = format!("b:{table_name}");

                let by_eq = *original == mutated;
                let by_comparator = model.column_shape_identity(
                    &left_table,
                    &model.tables[&left_table].columns[position],
                    &right_table,
                    &model.tables[&right_table].columns[position],
                );
                assert_eq!(
                    by_eq, by_comparator,
                    "{label}: changing ONLY `{}` on the real column \
                     {table_name}.{} moved `ColumnSnapshot::eq` to {by_eq} but \
                     `column_shape_identity` (+ its vendor half) to {by_comparator}. \
                     The comparator is not the extraction it claims to be.",
                    probe.field, original.name
                );
                checked += 1;
            }
        }

        for (position, original) in table.indexes.iter().enumerate() {
            for probe in &super::field_probes::index_snapshot_probes().probes {
                let mut mutated_tables = tables.clone();
                let mutated = &mut mutated_tables
                    .get_mut(table_name)
                    .expect("the table was just cloned")
                    .indexes[position];
                (probe.mutate)(mutated);
                let mutated = mutated.clone();

                let mut both: BTreeMap<String, TableSnapshot> = prefixed("a:", tables);
                both.extend(prefixed("b:", &mutated_tables));
                let model = SchemaModel::from_tables(&both);
                let left = &model.tables[&format!("a:{table_name}")].indexes[position];
                let right = &model.tables[&format!("b:{table_name}")].indexes[position];

                assert_eq!(
                    *original == mutated,
                    index_shape_identity(left, right),
                    "{label}: changing ONLY `{}` on the real index {} moved \
                     `IndexSnapshot::eq` and `index_shape_identity` apart",
                    probe.field,
                    original.name
                );

                // The pairing question, asked through the public surface by forcing the
                // names equal - `IndexSnapshot::eq` is `name == name &&
                // same_definition_except_name`, so what remains IS the pairing answer.
                let mut renamed = original.clone();
                renamed.name.clone_from(&mutated.name);
                assert_eq!(
                    renamed == mutated,
                    index_pairing_identity(left, right),
                    "{label}: changing ONLY `{}` on the real index {} moved \
                     `same_definition_except_name` and `index_pairing_identity` apart",
                    probe.field,
                    original.name
                );
                checked += 1;
            }
        }

        for original in &table.constraints {
            for probe in &super::field_probes::constraint_snapshot_probes().probes {
                let mut mutated = original.clone();
                (probe.mutate)(&mut mutated);
                assert_eq!(
                    *original == mutated,
                    constraint_shape_identity(
                        &schema_model::Constraint::from_snapshot(original),
                        &schema_model::Constraint::from_snapshot(&mutated)
                    ),
                    "{label}: changing ONLY `{}` on the real constraint {} moved \
                     `ConstraintSnapshot::eq` and `constraint_shape_identity` apart",
                    probe.field,
                    original.name
                );
                checked += 1;
            }
        }
    }

    assert!(
        checked >= 100,
        "{label}: only {checked} field mutations were checked, which is too few to be \
         a meaningful sweep of the comparators' terms"
    );
    checked
}

/// **The demonstration that drift and shape are genuinely two questions.**
///
/// Takes a REAL introspected column that the producer marked generated, and applies the
/// one mutation the real event applies: the column stops being generated
/// (`attgenerated` goes from `'s'` to `''`, which is `GeneratedKindSnapshot::Stored` to
/// `NotGenerated`). `column_shape_identity` calls the result the same column;
/// `drift_identity` does not.
///
/// This is not a fabricated pair. `GeneratedKindSnapshot`'s own doc names this exact
/// scenario as the reason the enum is closed rather than an `Option<bool>`: "the drift
/// comparison's whole subject is a column that stopped being generated".
///
/// # Panics
/// When no generated column is present (the fixture stopped exercising it), or when the
/// two comparators agree.
pub fn assert_drift_sees_what_shape_does_not(
    label: &str,
    tables: &BTreeMap<String, TableSnapshot>,
) {
    let model = SchemaModel::from_tables(tables);
    let generated = model
        .tables
        .values()
        .flat_map(|table| &table.columns)
        .find(|column| {
            matches!(
                column.generated_kind,
                Some(zero_migrate::GeneratedKindSnapshot::Stored)
                    | Some(zero_migrate::GeneratedKindSnapshot::Virtual)
            )
        })
        .unwrap_or_else(|| {
            panic!(
                "{label}: no column in the live snapshot carries a populated \
                 `generated_kind`, so this fixture cannot demonstrate the difference \
                 between drift and shape. Add a generated column rather than deleting \
                 the assertion."
            )
        })
        .clone();

    let mut no_longer_generated = generated.clone();
    no_longer_generated.generated_kind = Some(zero_migrate::GeneratedKindSnapshot::NotGenerated);

    assert!(
        column_shape_identity(&generated, &no_longer_generated),
        "{label}: `column_shape_identity` must be blind to `generated_kind`, because \
         `ColumnSnapshot::eq` is"
    );
    assert!(
        !drift_identity(&generated, &no_longer_generated),
        "{label}: `drift_identity` must SEE a column that stopped being generated. If \
         it does not, it is not the comparator `apply::drift` implements and the two \
         names are a lie."
    );
    assert!(
        !table_shape_identity(
            &schema_model::Table {
                columns: vec![generated.clone()],
                ..schema_model::Table::default()
            },
            &schema_model::Table {
                columns: vec![schema_model::Column {
                    name: format!("{}_other", generated.name),
                    ..generated
                }],
                ..schema_model::Table::default()
            }
        ),
        "{label}: table shape must still see a renamed column"
    );
}
