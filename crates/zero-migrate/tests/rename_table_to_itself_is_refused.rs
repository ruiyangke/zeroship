//! `renameTable` from a name to itself is refused at the gate.
//!
//! Measured before the fix: `{"op":"renameTable","table":"a","to":"a"}` passed
//! `validate_ir`, passed the load gate, lowered to
//!
//!     ALTER TABLE "prj_ir"."a" RENAME TO "a"
//!
//! and passed the fragment guard. PostgreSQL then rejects it at APPLY with
//!
//!     ERROR: relation "a" already exists
//!
//! which is the wrong story told at the worst moment. The operator is mid-deploy,
//! holding locks, reading an error that describes a name COLLISION — the usual
//! cause of which is another table already occupying the target name — when what
//! actually happened is that the rename names its own source.
//!
//! This engine refuses far subtler authoring mistakes at the gate: an empty
//! `primaryKey`, a `primaryKey` naming an absent column, a foreign key whose
//! local column does not exist. A self-rename is more trivially detectable than
//! any of them — it is a string comparison on two fields of one op — and it is
//! the only one that reached a live server.
//!
//! THE CONTROL MATTERS: a rename that only changes CASE is a real rename, because
//! quoted identifiers are case-sensitive on PostgreSQL, and refusing it would
//! break a legitimate migration. Only exact equality is a no-op.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn verdict(op: &str, dialect: Dialect) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{op}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, dialect, &[]).map_err(|e| format!("{}: {}", e.code, e.reason))
}

#[test]
fn renaming_a_table_to_its_own_name_is_refused_on_every_dialect() {
    for dialect in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
        let refusal =
            verdict(r#"{"op":"renameTable","table":"a","to":"a"}"#, dialect).expect_err(&format!(
                "{dialect:?}: a rename to the same name lowers to `ALTER TABLE a RENAME \
                 TO a`, which the server rejects with `relation \"a\" already exists` \
                 during apply. That error describes a collision with some OTHER table, \
                 so it sends the operator looking for the wrong problem while the \
                 deploy is holding locks"
            ));
        assert!(
            refusal.to_lowercase().contains("rename"),
            "{dialect:?}: the refusal must name the rename as the problem: {refusal}"
        );
    }
}

#[test]
fn renaming_a_table_to_a_different_name_is_still_allowed() {
    // The control. Without it, refusing every renameTable would satisfy the test
    // above while breaking the operation entirely.
    for dialect in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
        verdict(r#"{"op":"renameTable","table":"a","to":"b"}"#, dialect)
            .unwrap_or_else(|e| panic!("{dialect:?}: an ordinary rename must pass: {e}"));
    }
}

#[test]
fn a_rename_that_only_changes_case_is_still_allowed() {
    // Quoted identifiers are case-sensitive on PostgreSQL, so `a` -> `A` renames
    // a table to a genuinely different name. Comparing case-insensitively would
    // refuse a legitimate migration, which is why the fix compares exactly.
    verdict(
        r#"{"op":"renameTable","table":"a","to":"A"}"#,
        Dialect::Postgres,
    )
    .expect("a case-only rename is a real rename on PostgreSQL and must pass");
}

// ---------------------------------------------------------------------------
// The column sibling. Same defect, same reasoning, different op.
// ---------------------------------------------------------------------------

#[test]
fn renaming_a_column_to_its_own_name_is_refused_on_every_dialect() {
    // PostgreSQL rejects `ALTER TABLE a RENAME COLUMN c TO c` with
    //
    //     ERROR: column "c" of relation "a" already exists
    //
    // verified against the server - the same misleading shape as the table case.
    // It reads as a collision with a DIFFERENT column that already occupies the
    // target name, so the operator goes looking for that column.
    //
    // Reaching it needs a live schema carrying `c`, which is the ordinary case on
    // the CLI path: introspection supplies the column, the rename lowers, and the
    // server supplies the wrong story. The gate accepted this before the fix.
    // MySQL is EXCLUDED, not forgotten: it refuses every `renameColumn` with
    // "renameColumn is render-only for MySQL, not live-rendered", so it cannot
    // distinguish this mistake from the op itself and a loop over it would be
    // asserting the wrong refusal.
    for dialect in [Dialect::Postgres, Dialect::Sqlite] {
        let refusal = verdict(
            r#"{"op":"renameColumn","table":"a","from":"c","to":"c","type":"text"}"#,
            dialect,
        )
        .expect_err(&format!(
            "{dialect:?}: a column rename to the same name reaches the server, which \
             reports a collision with an existing column rather than the actual \
             mistake"
        ));
        assert!(
            refusal.to_lowercase().contains("rename"),
            "{dialect:?}: the refusal must name the rename as the problem: {refusal}"
        );
    }
}

#[test]
fn renaming_a_column_to_a_different_name_is_still_allowed() {
    for dialect in [Dialect::Postgres, Dialect::Sqlite] {
        verdict(
            r#"{"op":"renameColumn","table":"a","from":"c","to":"d","type":"text"}"#,
            dialect,
        )
        .unwrap_or_else(|e| panic!("{dialect:?}: an ordinary column rename must pass: {e}"));
    }
}

#[test]
fn a_column_rename_that_only_changes_case_is_still_allowed() {
    // Same reasoning as the table control: quoted identifiers are case-sensitive
    // on PostgreSQL, so `c` -> `C` renames the column to a genuinely different
    // name and must not be swept up by an over-eager equality check.
    verdict(
        r#"{"op":"renameColumn","table":"a","from":"c","to":"C","type":"text"}"#,
        Dialect::Postgres,
    )
    .expect("a case-only column rename is a real rename on PostgreSQL and must pass");
}
