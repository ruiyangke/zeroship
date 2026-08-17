//! What a `setColumnType` may do to a column that carries a GENERATION contract -
//! `GENERATED … AS IDENTITY` or `GENERATED … AS (expr)` - and what SQL it renders
//! for one.
//!
//! Both answers are the SERVER'S, measured on live PostgreSQL 18.4 and pinned by
//! `set_column_type_generation_contracts_pg`; this file pins the OFFLINE half, on
//! both routes a column's generation contract can reach the lower:
//!
//!   * DECLARED IN THIS ENVELOPE - a `createTable` / `addColumn` earlier in the
//!     same op list, which no live catalog has seen yet; and
//!   * LIVE - `LiveSchema::table_snapshots`, where PostgreSQL introspection
//!     records `attidentity` as `ColumnSnapshot::identity` and `attgenerated` as
//!     `ColumnSnapshot::generated_kind`.
//!
//! Both routes matter and neither subsumes the other. The first is the only one
//! that exists for a create-then-retype envelope; the second is the only one that
//! exists for the ordinary case where the column was created by an EARLIER
//! migration. A fix that reads only one of them closes half the gap.
//!
//! THE TWO VERDICTS:
//!
//! | source column | target                    | verdict                        |
//! |---------------|---------------------------|--------------------------------|
//! | identity      | smallInt / int / bigInt   | lowers, `USING` cast emitted   |
//! | identity      | anything else             | REFUSED at authoring time      |
//! | generated     | any                       | lowers WITHOUT the `USING`     |
//! | ordinary      | any                       | lowers, `USING` cast emitted   |
//!
//! The identity row is a refusal because PostgreSQL will not honour the change:
//! `identity column type must be smallint, integer, or bigint`. The generated row
//! is NOT a refusal, because the server DOES honour the change - it refuses only
//! the `USING` clause this engine used to attach unconditionally (`cannot specify
//! USING when altering type of generated column`). Refusing there would deny a
//! migration the database accepts.

mod support;

use std::collections::BTreeMap;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::render::fold::fold_ops;
use zero_migrate::schema::query::SqlDialect;
use zero_migrate::{IrAuthor, LiveSchema, PlanStep};

const SCHEMA: &str = "public";

const IDENTITY_COL: &str =
    r#"{"name":"v","type":"int","nullable":false,"identity":{"always":false}}"#;
const GENERATED_COL: &str =
    r#"{"name":"v","type":"int","generated":{"expr":{"node":"colRef","name":"c0"},"stored":true}}"#;
const ORDINARY_COL: &str = r#"{"name":"v","type":"int"}"#;

fn parse(bytes: &str) -> MigrationIr {
    serde_json::from_str(bytes).expect("the envelope parses")
}

/// `createTable` then `setColumnType`, in ONE envelope: the column's generation
/// contract is known only from the op stream.
fn declared_in_envelope(create_col: &str, to_type: &str) -> MigrationIr {
    parse(&format!(
        r#"{{"ir_version":1,"name":"n","ops":[
            {{"op":"createTable","name":"a","columns":[
                {{"name":"c0","type":"int","nullable":false}},
                {create_col}
            ],"primaryKey":["c0"]}},
            {{"op":"setColumnType","table":"a","column":"v","toType":{to_type}}}
        ]}}"#
    ))
}

/// `addColumn` then `setColumnType`, in ONE envelope.
///
/// This is the shape the `createTable` route does NOT cover, and it is the reason
/// [`LiveSchema::declared_column_generation`] exists at all: the `createTable` arm
/// of the lower already publishes the whole desired `TableSnapshot` into
/// `table_snapshots`, so a create-then-retype envelope is answered by the live map
/// even with no live database — but the `addColumn` arm publishes NOTHING, so
/// without a declared record an added identity or generated column reads as an
/// ordinary one, and both of this file's rules silently do not apply to it.
fn added_in_envelope(add_col: &str, to_type: &str) -> MigrationIr {
    parse(&format!(
        r#"{{"ir_version":1,"name":"n","ops":[
            {{"op":"createTable","name":"a","columns":[
                {{"name":"c0","type":"int","nullable":false}}
            ],"primaryKey":["c0"]}},
            {{"op":"addColumn","table":"a","column":"v",{add_col}}},
            {{"op":"setColumnType","table":"a","column":"v","toType":{to_type}}}
        ]}}"#
    ))
}

/// The `setColumnType` ALONE, as a later migration authors it.
fn retype_only(to_type: &str) -> MigrationIr {
    parse(&format!(
        r#"{{"ir_version":1,"name":"n","ops":[
            {{"op":"setColumnType","table":"a","column":"v","toType":{to_type}}}
        ]}}"#
    ))
}

/// The live schema an EARLIER migration leaves behind, built by folding that
/// migration's own ops - so `identity` / `generated_kind` arrive on the same
/// carriers PostgreSQL introspection fills in, rather than being hand-set here.
fn live_after_create(create_col: &str) -> LiveSchema {
    let ir = parse(&format!(
        r#"{{"ir_version":1,"name":"n","ops":[
            {{"op":"createTable","name":"a","columns":[
                {{"name":"c0","type":"int","nullable":false}},
                {create_col}
            ],"primaryKey":["c0"]}}
        ]}}"#
    ));
    let effective = support::operator_charter(SCHEMA);
    let folded = fold_ops(&ir.ops, SqlDialect::Postgres, SCHEMA, &effective)
        .expect("the create folds to a snapshot");
    LiveSchema::from_catalog_snapshot(folded, "app")
}

/// Lower `ir` against `live` and return the ALTER COLUMN TYPE statement, or the
/// refusal.
fn alter_statement(
    dialect: SqlDialect,
    ir: &MigrationIr,
    live: &LiveSchema,
) -> Result<String, String> {
    let effective = support::operator_charter(SCHEMA);
    let author = IrAuthor::new(SCHEMA, "app", dialect, &effective);
    let steps = author
        .lower_steps(ir, live)
        .map_err(|error| error.to_string())?;
    let statements: Vec<String> = steps
        .iter()
        .filter_map(|step| match step {
            PlanStep::Ddl(migration) => Some(migration.up.clone()),
            _ => None,
        })
        .filter(|up| up.contains("ALTER COLUMN"))
        .collect();
    match statements.len() {
        1 => Ok(statements.into_iter().next().expect("one statement")),
        other => Err(format!("expected ONE alter-column statement, got {other}")),
    }
}

/// The declared-field facets whose lane the differ owns, spelled as the SDK
/// descriptor spells them.
const GENERATED_FIELD: &str =
    r#","generated":{"expr":{"node":"colRef","name":"c0"},"stored":true}"#;
const IDENTITY_FIELD: &str = r#","identity":{"always":false}"#;

/// Diff a one-table descriptor set against ITSELF at an older type, and return the
/// `ALTER COLUMN` statements the differ planned - or its refusal.
///
/// The live side is built from the same descriptor path as the desired side, so
/// the ONLY difference between them is the column's declared type. That keeps the
/// leg about the retype rather than about two snapshot producers disagreeing.
fn differ_alters(facet: &str, live_ty: &str, desired_ty: &str) -> Result<Vec<String>, String> {
    let effective = support::operator_charter(SCHEMA);
    let desired_of = |ty: &str| {
        let descriptor: zero_migrate::CollectionDescriptor = serde_json::from_str(&format!(
            r#"{{"name":"a","owner_app":"app","fields":[
                {{"name":"c0","type":"int","required":true,"primaryKey":true}},
                {{"name":"v","type":"{ty}"{facet}}}
            ]}}"#
        ))
        .expect("the descriptor parses");
        zero_migrate::desired_snapshot_for_dialect(
            SCHEMA,
            std::slice::from_ref(&descriptor),
            SqlDialect::Postgres,
            &effective,
        )
        .expect("the descriptor set resolves")
    };
    let live = desired_of(live_ty).snapshot;
    let desired = desired_of(desired_ty);
    let ownership: std::collections::HashMap<String, String> =
        [("a".to_string(), "app".to_string())].into_iter().collect();
    let plan = zero_migrate::DeclarativeAuthor::new(SCHEMA, "app")
        .diff(&desired, &live, &ownership, &[], &effective)
        .map_err(|error| error.to_string())?;
    Ok(plan
        .all_migrations()
        .into_iter()
        .map(|migration| migration.up)
        .filter(|up| up.contains("ALTER COLUMN"))
        .collect())
}

/// The same envelope through BOTH routes, so no verdict below can be satisfied by
/// a fix that reads only one of them.
fn both_routes(create_col: &str, to_type: &str) -> BTreeMap<&'static str, Result<String, String>> {
    let mut out = BTreeMap::new();
    out.insert(
        "declared-in-envelope",
        alter_statement(
            SqlDialect::Postgres,
            &declared_in_envelope(create_col, to_type),
            &LiveSchema::default(),
        ),
    );
    out.insert(
        "live",
        alter_statement(
            SqlDialect::Postgres,
            &retype_only(to_type),
            &live_after_create(create_col),
        ),
    );
    out
}

// ---------------------------------------------------------------------------
// GAP 1 - an IDENTITY column may only become smallint / integer / bigint.
// ---------------------------------------------------------------------------

#[test]
fn a_retype_of_an_identity_column_to_a_non_integer_type_is_refused_on_both_routes() {
    for to_type in [
        r#""text""#,
        r#"{"string":{"length":40}}"#,
        r#""uuid""#,
        r#""double""#,
        r#"{"decimal":{"precision":10,"scale":0}}"#,
    ] {
        for (route, verdict) in both_routes(IDENTITY_COL, to_type) {
            let error = verdict.expect_err(&format!(
                "{route}: setColumnType of an identity column to {to_type} must be REFUSED \
                 at authoring time - PostgreSQL answers `identity column type must be \
                 smallint, integer, or bigint` and the plan dies mid-deploy"
            ));
            assert!(
                error.contains("identity") && error.contains("\"v\""),
                "{route}: the refusal must name the column and say it is an identity \
                 column. Got: {error}"
            );
            assert!(
                error.contains("smallInt") || error.contains("smallint"),
                "{route}: the refusal must say which targets ARE legal, so the author \
                 can act on it. Got: {error}"
            );
        }
    }
}

#[test]
fn a_retype_of_an_identity_column_to_an_integer_type_still_lowers_on_both_routes() {
    // THE OVER-REFUSAL CONTROL for the refusal above. PostgreSQL accepts every one
    // of these - measured, `int GENERATED BY DEFAULT AS IDENTITY` widens to bigint
    // and narrows to smallint with `attidentity` intact - so refusing them would
    // deny a migration the database honours.
    for to_type in [r#""bigInt""#, r#""smallInt""#, r#""int""#] {
        for (route, verdict) in both_routes(IDENTITY_COL, to_type) {
            let up = verdict.unwrap_or_else(|error| {
                panic!("{route}: an identity column may become {to_type}: {error}")
            });
            assert!(
                up.contains(" USING "),
                "{route}: an identity column is not a generated column, so the cast the \
                 engine has always emitted stays. Got: {up}"
            );
        }
    }
}

#[test]
fn a_retype_of_an_ordinary_column_beside_an_identity_column_is_untouched() {
    // The other half of the over-refusal control: the refusal keys on THE COLUMN
    // BEING RETYPED, not on the table having an identity column somewhere.
    let ir = parse(
        r#"{"ir_version":1,"name":"n","ops":[
            {"op":"createTable","name":"a","columns":[
                {"name":"c0","type":"int","nullable":false},
                {"name":"id","type":"int","nullable":false,"identity":{"always":false}},
                {"name":"v","type":"int"}
            ],"primaryKey":["c0"]},
            {"op":"setColumnType","table":"a","column":"v","toType":"text"}
        ]}"#,
    );
    let up = alter_statement(SqlDialect::Postgres, &ir, &LiveSchema::default())
        .expect("a plain column beside an identity column retypes freely");
    assert!(
        up.contains(" USING ") && up.contains("TYPE text"),
        "the ordinary column keeps the ordinary lowering. Got: {up}"
    );
}

// ---------------------------------------------------------------------------
// GAP 2 - a GENERATED column retypes, but WITHOUT the `USING` cast.
// ---------------------------------------------------------------------------

#[test]
fn a_retype_of_a_generated_column_omits_the_using_cast_on_both_routes() {
    // NOT a refusal. Measured on PostgreSQL 18.4: `ALTER TABLE t ALTER COLUMN g
    // TYPE bigint` WITHOUT `USING` is ACCEPTED on a generated column, and
    // `attgenerated` survives it - the server recomputes the expression under the
    // new type. It refuses only the `USING` clause. So the fix is to stop emitting
    // the clause, not to deny the migration.
    for to_type in [r#""bigInt""#, r#""text""#, r#"{"string":{"length":40}}"#] {
        for (route, verdict) in both_routes(GENERATED_COL, to_type) {
            let up = verdict.unwrap_or_else(|error| {
                panic!(
                    "{route}: a generated column may be retyped - PostgreSQL accepts it: \
                     {error}"
                )
            });
            assert!(
                !up.contains(" USING "),
                "{route}: PostgreSQL answers `cannot specify USING when altering type of \
                 generated column`, so the engine must not emit one. Got: {up}"
            );
            assert!(
                up.contains("ALTER COLUMN \"v\" TYPE "),
                "{route}: dropping the cast must not drop the statement. Got: {up}"
            );
        }
    }
}

#[test]
fn a_retype_of_an_ordinary_column_keeps_the_using_cast() {
    // THE OVER-SUPPRESSION CONTROL. Without it a "fix" that stopped emitting
    // `USING` everywhere would pass the test above, and every narrowing retype
    // (`text -> int`) would stop applying.
    for to_type in [r#""bigInt""#, r#""text""#] {
        for (route, verdict) in both_routes(ORDINARY_COL, to_type) {
            let up = verdict
                .unwrap_or_else(|error| panic!("{route}: an ordinary column retypes: {error}"));
            assert!(
                up.contains(" USING "),
                "{route}: an ordinary column still needs the cast the engine has always \
                 emitted, or a narrowing retype stops applying. Got: {up}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// THE THIRD ROUTE - a column ADDED in this envelope.
// ---------------------------------------------------------------------------

#[test]
fn a_column_added_in_this_envelope_carries_its_generation_contract_to_a_later_retype() {
    // The leg that separates the two declared-side sources. `createTable` publishes
    // its whole desired snapshot into `table_snapshots` as it lowers, so a
    // create-then-retype envelope would pass every test above even with no declared
    // record kept at all. `addColumn` publishes nothing, so this is the shape that
    // proves `declared_column_generation` does work rather than duplicating work.
    let refusal = alter_statement(
        SqlDialect::Postgres,
        &added_in_envelope(
            r#""type":"int","nullable":false,"identity":{"always":false}"#,
            r#""text""#,
        ),
        &LiveSchema::default(),
    )
    .expect_err("an added identity column is an identity column");
    assert!(
        refusal.contains("IDENTITY") && refusal.contains("\"v\""),
        "the refusal must reach a column this envelope ADDED, not only one it \
         created a table for. Got: {refusal}"
    );

    let up = alter_statement(
        SqlDialect::Postgres,
        &added_in_envelope(
            r#""type":"int","generated":{"expr":{"node":"colRef","name":"c0"},"stored":true}"#,
            r#""bigInt""#,
        ),
        &LiveSchema::default(),
    )
    .expect("an added generated column may be retyped");
    assert!(
        !up.contains(" USING "),
        "and so must the cast suppression. Got: {up}"
    );
}

// ---------------------------------------------------------------------------
// THE OTHER LANE. `setColumnType` is not the only way this statement is rendered.
// ---------------------------------------------------------------------------

#[test]
fn the_declarative_differ_renders_a_generated_column_retype_the_same_way() {
    // `render_alter_column_type` serves BOTH lanes: the authored `setColumnType`
    // every other test here goes through, and the differ, which emits the same
    // statement when a DECLARED column's type stops matching the live one. The
    // differ was carrying the same cast and would have died at the server the same
    // way, so this pins the lane rather than assuming the shared renderer covers it.
    //
    // The differ needs no new knowledge to get this right: the desired column it
    // hands over is descriptor-derived, and `column_snapshot_for_field` populates
    // `generated_kind` for EVERY column it builds.
    let alters =
        differ_alters(GENERATED_FIELD, "int", "bigInt").expect("the differ plans the type change");
    assert_eq!(
        alters.len(),
        1,
        "the differ must notice int -> bigint on the generated column: {alters:?}"
    );
    assert!(
        !alters[0].contains(" USING "),
        "PostgreSQL refuses the cast on a generated column in this lane too. Got: {}",
        alters[0]
    );
}

#[test]
fn the_declarative_differ_refuses_an_identity_column_retype_the_same_way() {
    let refusal = differ_alters(IDENTITY_FIELD, "int", "string")
        .expect_err("the differ must not plan an ALTER the server refuses");
    assert!(
        refusal.contains("IDENTITY") && refusal.contains("a.v"),
        "the differ's refusal must name the column and the reason, like the lower's. \
         Got: {refusal}"
    );
    // THE OVER-REFUSAL CONTROL for this lane too.
    let alters = differ_alters(IDENTITY_FIELD, "int", "bigInt")
        .expect("an identity column may still be widened within the integer family");
    assert!(
        alters.len() == 1 && alters[0].contains(" USING "),
        "an identity column is not a generated column, so its cast stays: {alters:?}"
    );
}

// ---------------------------------------------------------------------------
// THE OTHER DIALECTS. Neither reaches this renderer, and each says so in its own
// words - recorded rather than assumed from PostgreSQL's rule.
// ---------------------------------------------------------------------------

#[test]
fn mysql_and_sqlite_refuse_the_whole_op_before_either_verdict_applies() {
    for (dialect, expected) in [
        (SqlDialect::Mysql, "not supported on MySQL"),
        (SqlDialect::Sqlite, "setColumnType"),
    ] {
        for create_col in [IDENTITY_COL, GENERATED_COL, ORDINARY_COL] {
            let error = alter_statement(
                dialect,
                &declared_in_envelope(create_col, r#""bigInt""#),
                &LiveSchema::default(),
            )
            .expect_err(&format!(
                "{dialect:?} has no native ALTER COLUMN this lane can render, so the op \
                 is refused for EVERY column shape - the two verdicts above are \
                 PostgreSQL's alone"
            ));
            assert!(
                error.contains(expected),
                "{dialect:?}: expected the existing refusal naming {expected:?}, got {error}"
            );
        }
    }
}
