//! Explicit projection shape, field visibility and physical-column aliases.
//!
//! SQL assertions verify the selected storage column as well as its output name.

use zeroship_data_orm::sql::render::postgres;
use zeroship_data_orm::sql::{
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

/// A projection preserves the supplied fields and their visibility.
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

    // Platform fields are selected without being exposed to the caller.
    assert_eq!(projection.visible_aliases(), vec!["name"]);

    // A distinct query must be able to select a single field.
    let narrowed = Projection::rows(vec![field("role")]).expect("row projection");
    assert_eq!(narrowed.fields().len(), 1);
    println!("ruled on 2 projections");
}

/// Reject empty projections before they can render invalid SQL.
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

/// Planner-added fields remain selected but are omitted from visible aliases.
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

/// A stored projection selects the physical column under the requested logical name.
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
    // The logical alias must not also be selected as a source column.
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
