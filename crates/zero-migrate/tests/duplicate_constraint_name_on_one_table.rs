//! Two constraints of the same name on ONE table are refused where the server
//! refuses them - and accepted where it does not.
//!
//! The piece `name_claimed_twice_in_one_migration.rs` deliberately left out. That
//! fixture excluded constraint names because the CROSS-TABLE case is kind- and
//! dialect-dependent. The SAME-TABLE case turned out to be dialect-dependent too,
//! so the verdict is per dialect rather than uniform - and every cell below was
//! measured against a real server, not assumed:
//!
//!   PostgreSQL   ADD CONSTRAINT "same" CHECK  x2   ERROR: constraint "same" for
//!                                                  relation "a" already exists
//!                ADD CONSTRAINT "same" CHECK then
//!                ADD CONSTRAINT "same" UNIQUE     same error - the clash is the
//!                                                  NAME, not the kind
//!   MySQL        two CHECKs of one name           ER_CHECK_CONSTRAINT_DUP_NAME
//!                two UNIQUEs of one name          ER_DUP_KEYNAME
//!   SQLite       two CHECKs of one name           ACCEPTED
//!                two UNIQUEs of one name          ACCEPTED
//!
//! SQLite IS THEREFORE NOT REFUSED, and that is the whole reason this check is
//! dialect-aware instead of uniform. Refusing there would reject a migration
//! SQLite runs happily, which is the same line the sibling fixtures hold.
//!
//! CROSS-TABLE REMAINS OUT, still on measured grounds: PostgreSQL ACCEPTS the same
//! CHECK name on two tables and REJECTS the same UNIQUE name, because UNIQUE
//! creates a schema-level index and CHECK does not. `name_claimed_twice_in_one_
//! migration.rs` carries the control that pins it.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn verdict(dialect: Dialect, ops: &str) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, dialect).map_err(|e| format!("{}: {}", e.code, e.reason))
}

const T: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":"int","nullable":true},{"name":"w","type":"int","nullable":true}],"primaryKey":["c0"]}"#;

fn uniq(name: &str, column: &str) -> String {
    format!(
        r#"{{"op":"addConstraint","table":"a","constraint":{{"name":"{name}","kind":{{"kind":"unique","columns":["{column}"]}}}}}}"#
    )
}

fn check(name: &str, column: &str) -> String {
    format!(
        r#"{{"op":"addConstraint","table":"a","constraint":{{"name":"{name}","kind":{{"kind":"check","expr":{{"node":"binOp","op":"gt","lhs":{{"node":"colRef","name":"{column}"}},"rhs":{{"node":"literal","value":0}}}}}}}}}}"#
    )
}

/// Assert WHICH refusal, so no sibling here satisfies the wrong test.
///
/// Three of these four are the cross-op rule ("this addConstraint claims the name
/// ...") and one is the within-one-op rule ("this createTable names constraint ...
/// more than once"). Those are separate code paths and the fixture covers both on
/// purpose, so a guard that could not tell them apart would let the inline case
/// vouch for the cross-op case.
fn expect_constraint_refusal(dialect: Dialect, ops: &str, needle: &str, what: &str) {
    let refusal = verdict(dialect, ops).expect_err(what);
    assert!(
        refusal.contains(needle),
        "{needle:?} is missing, so the other constraint-name rule is satisfying \
         this test: {refusal}"
    );
}

#[test]
fn two_constraints_of_one_name_on_one_table_are_refused_on_postgres() {
    let refusal = verdict(
        Dialect::Postgres,
        &format!("{T},{},{}", check("same", "v"), check("same", "w")),
    )
    .expect_err("PostgreSQL rejects this with `constraint \"same\" ... already exists`");
    assert!(
        refusal.contains(r#"this addConstraint claims the name "same""#),
        "must be the cross-op constraint-name rule: {refusal}"
    );
    assert!(
        refusal.to_lowercase().contains("already"),
        "the refusal must say the name is already taken: {refusal}"
    );
}

#[test]
fn the_clash_is_the_name_not_the_kind() {
    // PostgreSQL gives the SAME error for CHECK-then-UNIQUE, so a check keyed on
    // (kind, name) rather than name alone would miss this.
    expect_constraint_refusal(
        Dialect::Postgres,
        &format!(
            r#"{T},{},{{"op":"addConstraint","table":"a","constraint":{{"name":"same","kind":{{"kind":"unique","columns":["w"]}}}}}}"#,
            check("same", "v")
        ),
        r#"this addConstraint claims the name "same""#,
        "a UNIQUE may not take a name a CHECK on the same table already holds",
    );
}

#[test]
fn two_constraints_of_one_name_on_one_table_are_refused_on_mysql() {
    // UNIQUE, not CHECK, and that is not incidental: `addConstraint(check)`
    // expression rendering is PostgreSQL-only in this engine, so a CHECK-based
    // MySQL test passes whatever this rule does. It would be green for the wrong
    // reason - the first draft of this fixture was.
    expect_constraint_refusal(
        Dialect::Mysql,
        &format!("{T},{},{}", uniq("same", "v"), uniq("same", "w")),
        r#"this addConstraint claims the name "same""#,
        "MySQL rejects a repeated constraint name with ER_DUP_KEYNAME",
    );
}

#[test]
fn a_create_table_naming_one_constraint_twice_is_refused() {
    expect_constraint_refusal(
        Dialect::Postgres,
        r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":"int","nullable":true}],"primaryKey":["c0"],"constraints":[{"name":"same","kind":{"kind":"unique","columns":["v"]}},{"name":"same","kind":{"kind":"unique","columns":["c0"]}}]}"#,
        r#"names constraint "same" more than once"#,
        "the same mistake authored inline in a createTable",
    );
}

// ---------------------------------------------------------------------------
// The controls, including the one that makes this dialect-aware at all.
// ---------------------------------------------------------------------------

#[test]
fn sqlite_still_accepts_what_sqlite_accepts() {
    // MEASURED: SQLite accepts two constraints of one name on one table. The
    // engine must not refuse it. Without this the natural "tidy" fix is a
    // dialect-uniform rule, which would reject a migration SQLite runs.
    //
    // UNIQUE for the same reason as the MySQL case: a CHECK here fails on
    // "expression rendering is PostgreSQL-only" and measures nothing about names.
    verdict(
        Dialect::Sqlite,
        &format!("{T},{},{}", uniq("same", "v"), uniq("same", "w")),
    )
    .expect("SQLite accepts duplicate constraint names; the engine must too");
}

#[test]
fn two_differently_named_constraints_are_still_allowed() {
    verdict(
        Dialect::Postgres,
        &format!("{T},{},{}", check("one", "v"), check("two", "w")),
    )
    .expect("distinct names on one table are ordinary");
}

#[test]
fn dropping_a_constraint_frees_its_name() {
    verdict(
        Dialect::Postgres,
        &format!(
            r#"{T},{},{{"op":"dropConstraint","table":"a","name":"same"}},{}"#,
            check("same", "v"),
            check("same", "w")
        ),
    )
    .expect("drop then re-add under the same name is a real migration pattern");
}

#[test]
fn dropping_the_table_frees_its_constraint_names() {
    verdict(
        Dialect::Postgres,
        &format!(
            r#"{T},{},{{"op":"dropTable","table":"a"}},{T},{}"#,
            check("same", "v"),
            check("same", "w")
        ),
    )
    .expect("a recreated table starts with a clean constraint namespace");
}

#[test]
fn the_same_constraint_name_on_two_tables_is_still_allowed_on_postgres() {
    // The cross-table boundary, measured: PostgreSQL accepts this for CHECK.
    verdict(
        Dialect::Postgres,
        &format!(
            r#"{T},{{"op":"createTable","name":"b","columns":[{{"name":"c0","type":"int","nullable":false}},{{"name":"v","type":"int","nullable":true}}],"primaryKey":["c0"]}},{},{{"op":"addConstraint","table":"b","constraint":{{"name":"same","kind":{{"kind":"check","expr":{{"node":"binOp","op":"gt","lhs":{{"node":"colRef","name":"v"}},"rhs":{{"node":"literal","value":0}}}}}}}}}}"#,
            check("same", "v")
        ),
    )
    .expect("PostgreSQL accepts the same CHECK name on two tables");
}

#[test]
fn unnamed_constraints_do_not_collide() {
    // A constraint with no explicit name has its name derived later. Treating
    // absent names as equal would refuse two ordinary anonymous constraints.
    verdict(
        Dialect::Postgres,
        &format!(
            r#"{T},{{"op":"addConstraint","table":"a","constraint":{{"kind":{{"kind":"unique","columns":["v"]}}}}}},{{"op":"addConstraint","table":"a","constraint":{{"kind":{{"kind":"unique","columns":["w"]}}}}}}"#
        ),
    )
    .expect("two anonymous constraints must not read as one repeated name");
}
