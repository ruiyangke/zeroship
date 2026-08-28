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

use zeroship_data_plan::render::postgres;
use zeroship_data_plan::{
    AggregateFunc, AggregateRef, Exposure, Ident, IdentRole, ProjectedField, Projection,
    ProjectionError, ProjectionKind, Select, PLATFORM_FIELD_NAMES,
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

/// Narrowing to one column still carries all seven platform fields, because
/// the union is in the constructor rather than in a caller's memory.
#[test]
fn a_narrowed_projection_keeps_every_platform_field() {
    let projection = Projection::rows(vec![field("name")]).expect("row projection");
    let aliases: Vec<&str> = projection
        .fields()
        .iter()
        .map(|f| f.alias.as_str())
        .collect();

    let mut ruled_on = 0_usize;
    for required in PLATFORM_FIELD_NAMES {
        assert!(
            aliases.contains(required),
            "narrowing dropped the platform field {required}; present: {aliases:?}"
        );
        ruled_on += 1;
    }
    assert_eq!(ruled_on, 7);
    assert_eq!(
        projection.fields().len(),
        8,
        "expected one declared column plus seven platform fields"
    );
    println!("ruled on {ruled_on} platform fields");
}

/// `id` in particular, because it is load-bearing in four independent ways -
/// the relation stitch, the unmask handle's `row_pk`, the AEAD tag via
/// `canonical_aad`, and change-event correlation - and a narrowing
/// implementation could be written, reviewed and shipped against any one of
/// them with the other three never exercised.
#[test]
fn the_primary_key_survives_the_narrowest_possible_projection() {
    let projection = Projection::rows(vec![]).expect("row projection");
    assert!(projection
        .fields()
        .iter()
        .any(|f| f.alias.as_str() == "id"));
    assert_eq!(
        projection.fields().len(),
        7,
        "an empty declared list must still yield the platform fields"
    );
    println!("ruled on 1 empty projection");
}

/// The union is not a blanket overwrite: a schema that declares a platform name
/// keeps it as its own, and it reaches user code.
#[test]
fn a_declared_platform_field_stays_declared() {
    let projection = Projection::rows(vec![field("deleted_at")]).expect("row projection");
    let entries: Vec<&ProjectedField> = projection
        .fields()
        .iter()
        .filter(|f| f.alias.as_str() == "deleted_at")
        .collect();
    assert_eq!(entries.len(), 1, "the union duplicated a declared field");
    assert_eq!(entries[0].exposure, Exposure::Declared);
    assert!(projection.visible_aliases().contains(&"deleted_at"));
    println!("ruled on 1 declared platform field");
}

/// The other half of the union rule: what the planner added is projected and
/// then stripped, so the SQL is correct and the creator sees what they asked
/// for.
#[test]
fn planner_added_fields_are_not_visible_to_user_code() {
    let projection = Projection::rows(vec![field("name")]).expect("row projection");
    assert_eq!(projection.visible_aliases(), vec!["name"]);
    assert_eq!(
        projection.fields().len() - projection.visible_aliases().len(),
        7,
        "seven platform fields must be projected and stripped"
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
fn a_narrowed_projection_over_a_masked_column_renders_the_sibling() {
    let projection = Projection::rows(vec![
        ProjectedField::masked(column("ssn")).expect("masked field"),
    ])
    .expect("row projection");
    let plan = Select::builder(
        Ident::parse_as("people", IdentRole::Collection).expect("collection"),
        projection,
    )
    .build()
    .expect("valid plan");
    let sql = postgres::render_select(&plan).expect("renders").sql().to_string();

    assert!(
        sql.contains(r#""ssn_masked" AS "ssn""#),
        "the mask substitution did not reach the SQL: {sql}"
    );
    // The parent column is NOT read. Checking for the exact projected form is
    // what distinguishes "the sibling was selected" from "both were".
    assert!(
        !sql.contains(r#""ssn" AS "ssn""#),
        "the plaintext column was projected alongside the sibling: {sql}"
    );
    println!("ruled on 1 masked projection");
}

/// The sibling suffix is reserved on COLUMN names - it has to be, or a creator
/// could declare `ssn_masked` and shadow the platform's own sibling - and the
/// platform still has to name it. Both halves, together, because either alone
/// reads as a contradiction.
#[test]
fn the_sibling_name_is_reserved_against_creators_and_derived_for_the_platform() {
    assert!(
        Ident::parse_as("ssn_masked", IdentRole::Column).is_err(),
        "a creator could declare the sibling name and shadow the platform's"
    );
    // `ProjectedField::masked` takes ONE ident and derives the other, so there
    // is no argument position a caller could pass an arbitrary sibling in.
    let field = ProjectedField::masked(column("ssn")).expect("masked field");
    assert_eq!(field.alias.as_str(), "ssn");
    println!("ruled on 1 reserved name and 1 derivation");
}

/// The derived name is not capped or hashed here, and refusing beats guessing:
/// the migration side applies its own cap, and a second implementation of it
/// would project a column that does not exist.
#[test]
fn an_overlong_masked_sibling_is_refused_rather_than_truncated() {
    let parent = column(&"p".repeat(60));
    assert!(matches!(
        ProjectedField::masked(parent),
        Err(ProjectionError::MaskedSiblingName { .. })
    ));
    // The control: a parent short enough to leave room is fine.
    assert!(ProjectedField::masked(column(&"p".repeat(56))).is_ok());
    println!("ruled on 2 parent lengths");
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
    let aggregate_field =
        ProjectedField::aggregate(AggregateRef::count_rows(), alias("total"));
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
        ProjectedField::masked(column("ssn")).expect("masked field"),
    ]);
    assert!(matches!(
        outcome,
        Err(ProjectionError::MaskedSiblingInAggregate { .. })
    ));
    println!("ruled on 1 masked aggregate");
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
    let sql = postgres::render_select(&plan).expect("renders").sql().to_string();
    assert!(
        !sql.contains("SELECT *") && !sql.contains(".*"),
        "a wildcard reached the statement: {sql}"
    );
    println!("ruled on 1 statement");
}
