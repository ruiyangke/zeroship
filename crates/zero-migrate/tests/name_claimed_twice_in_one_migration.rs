//! A migration may no longer claim the same NEW name twice.
//!
//! The mirror image of the use-after-drop family: instead of USING a name after
//! it stops being valid, this CREATES a name that is already taken by an earlier
//! op in the same envelope.
//!
//! ALL FOUR MEASURED FIRST, all lowered, all confirmed against live PostgreSQL:
//!
//!     CREATE TABLE "public"."a" (...)  x2      relation "a" already exists
//!     ALTER TABLE a ADD COLUMN "n" ... x2      column "n" of relation "a" already exists
//!     CREATE TABLE ... ("d" integer, "d" text) column "d" specified more than once
//!     CREATE TABLE b ...; ALTER TABLE a RENAME TO "b"
//!                                              relation "b" already exists
//!
//! THE ENGINE ALREADY REFUSES TWO MEMBERS of this same family, which is the
//! asymmetry this closes:
//!
//!   - `createEnum` twice is refused at lower: `duplicate definition`
//!   - `renameColumn` onto an existing column is refused at lower with
//!     "a rename cannot collide with an existing column"
//!
//! So the same authoring mistake met the operator at the gate or at the server
//! depending only on which object kind carried it.
//!
//! SCOPE, stated rather than implied: this tracks only names THIS ENVELOPE
//! creates. `validate_ir` has no live schema, so a `createTable` colliding with a
//! table that already exists in the database is a different question and is not
//! answered here.
//!
//! CONSTRAINT NAMES ARE DELIBERATELY OUT, and the reason is measured rather than
//! assumed. Against live PostgreSQL, the same CHECK name on two different tables
//! is ACCEPTED, while the same UNIQUE name is REJECTED with `relation "shared_u"
//! already exists` - because a UNIQUE constraint creates a schema-level index and
//! a CHECK does not. A per-envelope constraint-name rule would therefore have to
//! be kind-aware and dialect-aware, which is its own change with its own controls.
//! Refusing all repeats would reject CHECK constraints PostgreSQL accepts.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn verdict(ops: &str) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, Dialect::Postgres).map_err(|e| format!("{}: {}", e.code, e.reason))
}

const T: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":"int","nullable":true}],"primaryKey":["c0"]}"#;

/// Assert every needle, so a SIBLING's refusal cannot satisfy this test.
///
/// The audit that added this started by asking whether an assertion existed. That
/// was the wrong question: the original guard below checked only that the message
/// contained "already", which four of these five refusals do. The question that
/// matters is whether another rule in the same family would pass.
fn expect_refusal(ops: &str, needles: &[&str], what: &str) {
    let refusal = verdict(ops).expect_err(what);
    for needle in needles {
        assert!(
            refusal.contains(needle),
            "{needle:?} is missing, so a sibling rule is satisfying this test \
             instead of the one it names: {refusal}"
        );
    }
}

#[test]
fn creating_the_same_table_twice_is_refused() {
    expect_refusal(
        &format!("{T},{T}"),
        &["this createTable claims", "already created a table"],
        "the second createTable retakes a name",
    );
}

#[test]
fn adding_the_same_column_twice_is_refused() {
    expect_refusal(
        &format!(
            r#"{T},{{"op":"addColumn","table":"a","column":"n","type":"int","nullable":true}},{{"op":"addColumn","table":"a","column":"n","type":"text","nullable":true}}"#
        ),
        // The column name is what separates this from the sibling below; both
        // are addColumn refusals on the same table.
        &[
            r#"this addColumn claims the name "n""#,
            "already added or defined",
        ],
        "the second addColumn retakes a column name",
    );
}

#[test]
fn adding_a_column_the_create_table_already_defined_is_refused() {
    expect_refusal(
        &format!(
            r#"{T},{{"op":"addColumn","table":"a","column":"v","type":"text","nullable":true}}"#
        ),
        &[
            r#"this addColumn claims the name "v""#,
            "already added or defined",
        ],
        "the addColumn retakes a name the createTable defined",
    );
}

#[test]
fn a_create_table_naming_one_column_twice_is_refused() {
    expect_refusal(
        r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"d","type":"int","nullable":true},{"name":"d","type":"text","nullable":true}],"primaryKey":["c0"]}"#,
        &[r#"names column "d" more than once"#],
        "`column \"d\" specified more than once` is decidable from the op alone",
    );
}

#[test]
fn renaming_a_table_onto_a_live_name_is_refused() {
    // MEASURED, and not what this fixture's title implies: the refusal comes from
    // the TYPE namespace, because a rename carries the table's composite row type
    // and that check runs first. The relation-side rename check is real but is
    // not what fails here - deleting it would leave this test green, exactly the
    // trap the partition fixture had to be corrected for.
    expect_refusal(
        &format!(
            r#"{T},{{"op":"createTable","name":"b","columns":[{{"name":"c0","type":"int","nullable":false}}],"primaryKey":["c0"]}},{{"op":"renameTable","table":"a","to":"b"}}"#
        ),
        &[
            "this renameTable claims",
            "already created a table",
            "one type namespace",
        ],
        "the rename target is a table this envelope just created",
    );
}

// ---------------------------------------------------------------------------
// The controls. A blanket "never mention a name twice" rule passes every test
// above and fails every one of these.
// ---------------------------------------------------------------------------

#[test]
fn dropping_a_table_then_recreating_it_is_still_allowed() {
    verdict(&format!(r#"{T},{{"op":"dropTable","table":"a"}},{T}"#))
        .expect("drop then recreate is how a table is redefined");
}

#[test]
fn dropping_a_column_then_re_adding_it_is_still_allowed() {
    verdict(&format!(
        r#"{T},{{"op":"dropColumn","table":"a","column":"v"}},{{"op":"addColumn","table":"a","column":"v","type":"text","nullable":true}}"#
    ))
    .expect("drop then re-add is how a column's type is changed");
}

#[test]
fn renaming_a_table_away_then_reusing_the_freed_name_is_still_allowed() {
    // The freed name is available again, and the rename walk already pins this.
    verdict(&format!(
        r#"{T},{{"op":"renameTable","table":"a","to":"b"}},{{"op":"createTable","name":"a","columns":[{{"name":"c0","type":"int","nullable":false}}],"primaryKey":["c0"]}}"#
    ))
    .expect("recreating a table under the freed name must remain allowed");
}

#[test]
fn renaming_onto_a_name_that_was_dropped_first_is_still_allowed() {
    verdict(&format!(
        r#"{T},{{"op":"createTable","name":"b","columns":[{{"name":"c0","type":"int","nullable":false}}],"primaryKey":["c0"]}},{{"op":"dropTable","table":"b"}},{{"op":"renameTable","table":"a","to":"b"}}"#
    ))
    .expect("renaming onto a name freed earlier in the envelope is legitimate");
}

#[test]
fn the_same_column_name_on_two_different_tables_is_still_allowed() {
    verdict(&format!(
        r#"{T},{{"op":"createTable","name":"b","columns":[{{"name":"c0","type":"int","nullable":false}},{{"name":"v","type":"int","nullable":true}}],"primaryKey":["c0"]}}"#
    ))
    .expect("column names are scoped to their table");
}

#[test]
fn adding_the_same_column_name_to_two_different_tables_is_still_allowed() {
    verdict(&format!(
        r#"{T},{{"op":"createTable","name":"b","columns":[{{"name":"c0","type":"int","nullable":false}}],"primaryKey":["c0"]}},{{"op":"addColumn","table":"a","column":"n","type":"int","nullable":true}},{{"op":"addColumn","table":"b","column":"n","type":"int","nullable":true}}"#
    ))
    .expect("adding the same column name to two tables is ordinary");
}

#[test]
fn a_check_constraint_name_repeated_across_tables_is_still_allowed() {
    // Pins the measured boundary above: PostgreSQL accepts this, so the engine
    // must not refuse it. Guards against someone "tidying" this fix by adding a
    // blanket constraint-name rule.
    verdict(&format!(
        r#"{T},{{"op":"createTable","name":"b","columns":[{{"name":"c0","type":"int","nullable":false}},{{"name":"v","type":"int","nullable":true}}],"primaryKey":["c0"]}},{{"op":"addConstraint","table":"a","constraint":{{"name":"shared","kind":{{"kind":"check","expr":{{"node":"binOp","op":"gt","lhs":{{"node":"colRef","name":"v"}},"rhs":{{"node":"literal","value":0}}}}}}}}}},{{"op":"addConstraint","table":"b","constraint":{{"name":"shared","kind":{{"kind":"check","expr":{{"node":"binOp","op":"gt","lhs":{{"node":"colRef","name":"v"}},"rhs":{{"node":"literal","value":0}}}}}}}}}}"#
    ))
    .expect("PostgreSQL accepts the same CHECK name on two tables; the engine must too");
}
