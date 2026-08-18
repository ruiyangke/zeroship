//! The two column accessors are exhaustive over `Op`, and closing them found two
//! more defects.
//!
//! F761 through F764 were four consecutive defects with ONE cause: a rule whose
//! accessor could not see every op. `expression_column_references` and
//! `plain_column_references` both ended in `_ => Vec::new()`, so an op absent
//! from the match was silently exempt - a new variant's SILENCE read as "this op
//! names no columns".
//!
//! Deleting both catch-alls and listing all 56 variants is what this fixture
//! protects. The point is not tidiness: the next op that names a column is now a
//! COMPILE ERROR in those matches instead of an accepted migration the server
//! rejects at apply. A test cannot assert "the match has no catch-all", so the
//! two defects the change surfaced stand in for it - each was accepted before and
//! is refused now.
//!
//! MEASURED AGAINST LIVE POSTGRESQL:
//!
//!     ALTER TABLE a DROP COLUMN v;
//!     SELECT pg_get_serial_sequence('a','v')
//!         ERROR: column "v" of relation "a" does not exist
//!
//! That is the lookup a `synchronizeIdentity` must perform: the op is
//! runtime-resolved rather than a fixed statement, so the measurable claim is the
//! catalog lookup it depends on, and that lookup fails.
//!
//!     ALTER TABLE a DROP COLUMN v; CREATE INDEX ix ON a (v)
//!         ERROR: column "v" does not exist
//!
//! that one reached through a `dialectal` container, whose nested ops no walk in
//! this file had ever descended into.
//!
//! ONLY THE LEG THAT RUNS IS DESCENDED INTO. A `dialectal` op carries a sequence
//! per dialect plus a `default`; refusing on a reference in a leg the target
//! dialect never emits would reject a migration the server never sees. Controls
//! below hold that line from both sides.
//!
//! STILL OPEN, MEASURED, AND NAMED RATHER THAN IMPLIED: the TABLE-level walk has
//! the same container blindness. A `dropTable` nested in a `dialectal` leg does
//! not register as vacated, so a later op targeting that table is accepted:
//!
//!     DROP TABLE a; ALTER TABLE a ADD COLUMN z int
//!         ERROR: relation "f766.a" does not exist
//!
//! Fixing that means threading nested ops through a state machine that mutates
//! `vacated`, not just reading names out of them, so it is separate work rather
//! than something bundled in here.

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn verdict_on(dialect: Dialect, ops: &str) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, dialect).map_err(|e| format!("{}: {}", e.code, e.reason))
}

fn verdict(ops: &str) -> Result<(), String> {
    verdict_on(Dialect::Postgres, ops)
}

/// Assert WHICH op named the column, not merely that a column was mentioned.
///
/// The first version asserted `contains("column")`, which the whole
/// dropped-column family satisfies, so the three tests below were mutually
/// satisfiable. Written before the audit established that a guard every sibling
/// passes is not a guard.
fn expect_column_refusal(ops: &str, needle: &str, what: &str) {
    let refusal = verdict(ops).expect_err(what);
    assert!(
        refusal.contains(needle),
        "{needle:?} is missing, so a different op is satisfying this test: {refusal}"
    );
}

const A: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":"int","nullable":true}],"primaryKey":["c0"]}"#;
const DROP_V: &str = r#"{"op":"dropColumn","table":"a","column":"v"}"#;
const SYNC_V: &str = r#"{"op":"synchronizeIdentity","table":"a","column":"v","writesQuiesced":"maintenance window"}"#;
const INDEX_V: &str =
    r#"{"op":"createIndex","name":"ix","table":"a","columns":[{"kind":"column","name":"v"}]}"#;

// ---------------------------------------------------------------------------
// The two defects the exhaustiveness change surfaced.
// ---------------------------------------------------------------------------

#[test]
fn synchronizing_identity_on_a_dropped_column_is_refused() {
    expect_column_refusal(
        &format!("{A},{DROP_V},{SYNC_V}"),
        "this synchronizeIdentity names column",
        "the identity lookup names a column that is gone",
    );
}

#[test]
fn a_dialectal_leg_naming_a_dropped_column_is_refused() {
    expect_column_refusal(
        &format!(r#"{A},{DROP_V},{{"op":"dialectal","pg":[{INDEX_V}]}}"#),
        "this createIndex names column",
        "a nested op names a column that is gone",
    );
}

#[test]
fn a_dialectal_default_leg_naming_a_dropped_column_is_refused() {
    // The fallback leg runs when the dialect has no leg of its own, so it must
    // be descended into as well. Reading only the dialect-specific legs passes
    // the test above and misses this.
    expect_column_refusal(
        &format!(r#"{A},{DROP_V},{{"op":"dialectal","default":[{INDEX_V}]}}"#),
        "this createIndex names column",
        "the default leg runs on PostgreSQL when pg is absent",
    );
}

// ---------------------------------------------------------------------------
// Controls.
// ---------------------------------------------------------------------------

#[test]
fn synchronizing_identity_on_a_live_column_is_still_allowed() {
    verdict(&format!("{A},{SYNC_V}")).expect("synchronizing a live column is the ordinary case");
}

#[test]
fn a_dialectal_leg_naming_a_live_column_is_still_allowed() {
    verdict(&format!(r#"{A},{{"op":"dialectal","pg":[{INDEX_V}]}}"#))
        .expect("a nested op on a live column is ordinary");
}

#[test]
fn a_leg_for_another_dialect_is_not_descended_into() {
    // THE BOUNDARY. Under PostgreSQL the `sqlite` leg never runs, so a reference
    // inside it cannot fail at apply. Descending into every leg regardless -
    // the obvious implementation, and the one that looks more thorough - would
    // refuse a migration the server never sees.
    verdict(&format!(
        r#"{A},{DROP_V},{{"op":"dialectal","sqlite":[{INDEX_V}]}}"#
    ))
    .expect("the sqlite leg is not emitted on PostgreSQL");
}

#[test]
fn the_dialect_specific_leg_wins_over_the_default() {
    // The same envelope, read two ways. Under SQLite the `sqlite` leg runs and
    // the reference is refused; the `default` leg it shadows names a live
    // column, so a reader that took `default` whenever it was present would
    // accept this.
    let ops = format!(
        r#"{A},{DROP_V},{{"op":"dialectal","sqlite":[{INDEX_V}],"default":[{{"op":"createIndex","name":"ix","table":"a","columns":[{{"kind":"column","name":"c0"}}]}}]}}"#
    );
    let sqlite = verdict_on(Dialect::Sqlite, &ops)
        .expect_err("the sqlite leg runs and names a dropped column");
    assert!(
        sqlite.contains("this createIndex names column"),
        "the SQLite leg must be refused by the same rule as its PG twin: {sqlite}"
    );
    verdict_on(Dialect::Postgres, &ops)
        .expect("PostgreSQL falls back to the default leg, which names a live column");
}

#[test]
fn a_leg_less_container_never_reaches_the_accessor() {
    // Written first as "an empty container is harmless", which FAILED: a
    // pre-existing rule refuses a dialectal op with no legs at all. The control
    // was asserting something about an envelope that never gets this far, so it
    // now pins the real reason instead - `dialectal_leg` returning an empty
    // slice is unreachable in practice, and that is why, rather than an
    // accident.
    let refusal = verdict(&format!(r#"{A},{DROP_V},{{"op":"dialectal"}}"#))
        .expect_err("a dialectal op must carry at least one leg");
    assert!(
        refusal.contains("carries no legs"),
        "the refusal must be the leg-less one, not anything this change added: {refusal}"
    );
}
