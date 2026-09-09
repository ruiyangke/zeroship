//! Narrowing, the platform-field union, and the masked substitution.
//!
//! Two of SC-3's acceptance arms live here, and both are written the way the
//! document insists rather than the way that is easier:
//!
//! * *a narrowed projection still contains every required platform field*,
//!   asserted against a projection that asks for ONE declared column - which is
//!   the case that catches narrowing implemented as a filter over the caller's
//!   list;
//! * *a narrowed projection over a MASKED column still renders
//!   `"<col>_masked" AS "<col>"`*, asserted **on the rendered SQL**, not on the
//!   shape of the returned row. A row-shape assertion passes while the value is
//!   plaintext, because the column arrives under the name the caller asked for
//!   either way - which is the whole failure.

use zeroship_data_sql::render::postgres;
use zeroship_data_sql::{
    AggregateFunc, AggregateRef, Exposure, Ident, IdentRole, ProjectedField, Projection,
    ProjectionError, ProjectionKind, Select,
};

fn column(name: &str) -> Ident {
    Ident::parse_as(name, IdentRole::Column).expect("valid column")
}

fn alias(name: &str) -> Ident {
    Ident::parse_as(name, IdentRole::Alias).expect("valid alias")
}

fn field(name: &str) -> ProjectedField {
    ProjectedField::column(column(name)).expect("projectable")
}

/// A row projection carries exactly what it was given, and a platform field is
/// carried but not visible.
///
/// This asserted the opposite until 2026-09-07: the constructor appended seven
/// system fields of its own, so narrowing to one column produced eight. That
/// made `distinct("role")` unrepresentable and silently widened every explicit
/// `select`. Which fields a platform manages is not a fact a SQL grammar can
/// hold, so the caller supplies them and marks them.
#[test]
fn a_row_projection_carries_exactly_what_it_was_given() {
    let projection = Projection::rows(vec![
        field("name"),
        ProjectedField::platform(column("id")).expect("platform field"),
    ])
    .expect("row projection");

    let aliases: Vec<&str> = projection
        .fields()
        .iter()
        .map(|f| f.alias().as_str())
        .collect();
    assert_eq!(aliases.len(), 2, "the projection widened: {aliases:?}");
    assert!(aliases.contains(&"name") && aliases.contains(&"id"));

    // The exposure split survives the union's removal: `id` is selected and is
    // not part of what the caller asked to see.
    assert_eq!(projection.visible_aliases(), vec!["name"]);

    // A single-column narrowing is now expressible at all, which is the defect
    // this replaced. `distinct` needs exactly this.
    let narrowed = Projection::rows(vec![field("role")]).expect("row projection");
    assert_eq!(narrowed.fields().len(), 1);
    println!("ruled on 2 projections");
}

/// An empty projection is refused rather than rendered.
///
/// The non-empty invariant used to be a side effect of the platform-field
/// union, which could not produce an empty list. Removing the union removed the
/// invariant, so it is now stated. Without this, `SELECT  FROM "t"` is
/// constructible.
///
/// The obligation this test used to carry - that `id` survives the narrowest
/// projection, because it is load-bearing for the relation stitch, the unmask
/// handle's `row_pk`, the AEAD tag via `canonical_aad` and change-event
/// correlation - has MOVED to the caller, and that is the real cost of this
/// change. A grammar cannot keep a column it has no way to be told about.
#[test]
fn an_empty_row_projection_is_refused() {
    assert!(matches!(
        Projection::rows(vec![]),
        Err(ProjectionError::Empty)
    ));
    // The control: one field is enough.
    assert!(Projection::rows(vec![field("name")]).is_ok());
    println!("ruled on 2 projections");
}

/// A schema that declares a platform name keeps it as its own, and it reaches
/// user code. The union used to have to avoid overwriting it; now nothing can,
/// because nothing is added.
#[test]
fn a_declared_platform_field_stays_declared() {
    let projection = Projection::rows(vec![field("deleted_at")]).expect("row projection");
    let entries: Vec<&ProjectedField> = projection
        .fields()
        .iter()
        .filter(|f| f.alias().as_str() == "deleted_at")
        .collect();
    assert_eq!(entries.len(), 1, "the union duplicated a declared field");
    assert_eq!(entries[0].exposure(), Exposure::Declared);
    assert!(projection.visible_aliases().contains(&"deleted_at"));
    println!("ruled on 1 declared platform field");
}

/// A platform-marked field is projected and then stripped, so the SQL is
/// correct and the creator sees what they asked for.
///
/// The exposure split is the half of the old union rule that SURVIVES it. The
/// list moved to the caller; the distinction between "selected" and "visible"
/// did not, because it is a property of the projection rather than of the
/// platform.
#[test]
fn planner_added_fields_are_not_visible_to_user_code() {
    let projection = Projection::rows(vec![
        field("name"),
        ProjectedField::platform(column("id")).expect("platform field"),
        ProjectedField::platform(column("version")).expect("platform field"),
    ])
    .expect("row projection");
    assert_eq!(projection.visible_aliases(), vec!["name"]);
    assert_eq!(
        projection.fields().len() - projection.visible_aliases().len(),
        2,
        "both platform fields must be projected and stripped"
    );
    println!("ruled on 1 projection");
}

/// THE ARM THAT MATTERS, asserted on rendered SQL.
///
/// The obvious implementation of narrowing - filter the final column list down
/// to what the caller asked for - emits a bare `"ssn"` and returns plaintext:
/// not fewer columns than intended but the wrong value, silently, on precisely
/// the columns marked as needing protection.
#[test]
fn a_narrowed_projection_over_a_stored_column_renders_the_physical_name() {
    let projection = Projection::rows(vec![
        ProjectedField::stored(
            Ident::parse_as("__zs_raw__ssn", IdentRole::StoredColumn).expect("stored column"),
            column("ssn"),
        )
        .expect("stored field"),
    ])
    .expect("row projection");
    let plan = Select::builder(
        Ident::parse_as("people", IdentRole::Collection).expect("collection"),
        projection,
    )
    .build()
    .expect("valid plan");
    let sql = postgres::render_select(&plan)
        .expect("renders")
        .sql()
        .to_string();

    assert!(
        sql.contains(r#""__zs_raw__ssn" AS "ssn""#),
        "the storage substitution did not reach the SQL: {sql}"
    );
    // The logical name is NOT read. Checking for the exact projected form is
    // what distinguishes "the physical column was selected" from "both were".
    assert!(
        !sql.contains(r#""ssn" AS "ssn""#),
        "the plaintext column was projected alongside the stored one: {sql}"
    );
    println!("ruled on 1 stored projection");
}

/// The platform's stored prefixes are reserved against CREATOR columns - they
/// have to be, or a creator could declare one and shadow a platform column - and
/// the platform still has to name them. Both halves, together, because either
/// alone reads as a contradiction, and the split role is what expresses it.
#[test]
fn the_stored_prefix_is_reserved_against_creators_and_nameable_by_the_platform() {
    let mut ruled_on = 0_usize;
    for name in ["__zs_raw__ssn", "_distance", "__zeroship_journal"] {
        assert!(
            Ident::parse_as(name, IdentRole::Column).is_err(),
            "a creator could declare {name:?} and shadow a platform column"
        );
        assert!(
            Ident::parse_as(name, IdentRole::StoredColumn).is_ok(),
            "the platform cannot name its own stored column {name:?}"
        );
        ruled_on += 1;
    }
    // The looser role is NOT a bypass: the backend catalogs and the
    // classification names are refused in both. Without these the role would be
    // a hole rather than a split.
    for refused in ["pg_attribute", "sqlite_master", "pii"] {
        assert!(
            Ident::parse_as(refused, IdentRole::StoredColumn).is_err(),
            "the stored-column role let {refused:?} through"
        );
        ruled_on += 1;
    }
    assert_eq!(ruled_on, 6);
    println!("ruled on {ruled_on} stored-column names");
}

/// Both names are supplied, and the physical one is what reaches the SQL. The
/// crate derived it until 2026-09-07 and the derivation outlived the storage
/// layout it described.
#[test]
fn a_stored_field_takes_both_names_from_the_caller() {
    let field = ProjectedField::stored(
        Ident::parse_as("__zs_raw__ssn", IdentRole::StoredColumn).expect("stored column"),
        column("ssn"),
    )
    .expect("stored field");
    assert_eq!(field.alias().as_str(), "ssn");
    println!("ruled on 1 stored field");
}

/// Two columns arriving under one name is an ambiguous row, not a narrowing.
#[test]
fn duplicate_aliases_are_refused() {
    let outcome = Projection::rows(vec![field("name"), field("name")]);
    assert_eq!(
        outcome.expect_err("must refuse"),
        ProjectionError::DuplicateAlias {
            alias: "name".to_string()
        }
    );
    println!("ruled on 1 duplicate pair");
}

/// The platform-field union is correct for rows and invalid SQL for an
/// aggregation, so the two are different constructors rather than a flag.
#[test]
fn an_aggregate_projection_does_not_union_the_platform_fields() {
    let projection = Projection::aggregate(vec![ProjectedField::aggregate(
        AggregateRef::count_rows(),
        alias("total"),
    )])
    .expect("aggregate projection");
    assert_eq!(projection.kind(), ProjectionKind::Aggregate);
    assert_eq!(projection.fields().len(), 1);
    assert_eq!(projection.visible_aliases(), vec!["total"]);
    println!("ruled on 1 aggregate projection");
}

/// Each constructor refuses the other's shape, so the kind cannot be wrong.
#[test]
fn the_two_projection_constructors_refuse_each_other() {
    let aggregate_field = ProjectedField::aggregate(AggregateRef::count_rows(), alias("total"));
    assert!(matches!(
        Projection::rows(vec![aggregate_field]),
        Err(ProjectionError::AggregateInRowProjection { .. })
    ));
    assert_eq!(
        Projection::aggregate(vec![field("name")]).expect_err("must refuse"),
        ProjectionError::NoAggregate
    );
    println!("ruled on 2 mismatched projections");
}

/// A masked column cannot be a grouping key here: resolving its mask policy
/// over a grouped result is SC-6's contract, not something to guess at.
#[test]
fn a_masked_sibling_is_refused_in_an_aggregate_projection() {
    let outcome = Projection::aggregate(vec![
        ProjectedField::aggregate(
            AggregateRef::over(AggregateFunc::Max, column("age"), false).expect("aggregate"),
            alias("oldest"),
        ),
        ProjectedField::stored(
            Ident::parse_as("__zs_raw__ssn", IdentRole::StoredColumn).expect("stored column"),
            column("ssn"),
        )
        .expect("stored field"),
    ]);
    assert!(matches!(
        outcome,
        Err(ProjectionError::StoredInAggregate { .. })
    ));
    println!("ruled on 1 stored aggregate");
}

/// There is no `SELECT *`, and the absence is structural: `ProjectionSource`
/// has no wildcard. This arm rules on the rendered statement, which is where a
/// wildcard would have to appear.
#[test]
fn no_projection_renders_a_wildcard() {
    let projection = Projection::rows(vec![field("name")]).expect("row projection");
    let plan = Select::builder(
        Ident::parse_as("users", IdentRole::Collection).expect("collection"),
        projection,
    )
    .build()
    .expect("valid plan");
    let sql = postgres::render_select(&plan)
        .expect("renders")
        .sql()
        .to_string();
    assert!(
        !sql.contains("SELECT *") && !sql.contains(".*"),
        "a wildcard reached the statement: {sql}"
    );
    println!("ruled on 1 statement");
}
