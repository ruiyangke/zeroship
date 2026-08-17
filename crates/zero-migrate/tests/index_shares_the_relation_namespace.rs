//! An index name and a table/view/sequence name collide where the server says so.
//!
//! Extends `relation_namespace_is_shared.rs`. That fixture put tables, views and
//! sequences into one namespace; PostgreSQL keeps INDEXES in the same one
//! (everything is `pg_class`), so an index was still able to take a live relation
//! name and vice versa. The evidence was already visible in F712's measurements -
//! a UNIQUE constraint collided with `relation "shared_u" already exists` - and
//! was not followed up at the time.
//!
//! MEASURED ON ALL THREE, and the split is real:
//!
//!   PostgreSQL  CREATE INDEX "ix"; CREATE TABLE "ix"     relation "ix" already exists
//!               CREATE INDEX "ix"; CREATE VIEW "ix"      relation "ix" already exists
//!               CREATE INDEX "ix"; CREATE SEQUENCE "ix"  relation "ix" already exists
//!               CREATE TABLE "b";  CREATE INDEX "b"      relation "b" already exists
//!   SQLite      CREATE INDEX a ON a (v)                  there is already a table named a
//!               CREATE TABLE ix, after CREATE INDEX ix   there is already an index named ix
//!   MySQL       both directions                          ACCEPTED - MySQL scopes
//!                                                        index names per TABLE
//!
//! So PostgreSQL and SQLite refuse and MySQL does not. That is the exact INVERSE
//! of the dialect split in `duplicate_constraint_name_on_one_table.rs`, where
//! SQLite is the permissive one. Neither rule could have been guessed from the
//! other, which is why both were measured rather than reasoned about.
//!
//! INDEX-VS-INDEX IS DELIBERATELY NOT HANDLED HERE. `validate_index_names_across_
//! ops` already refuses it with a better message than this check could give - it
//! explains that the render is `CREATE INDEX IF NOT EXISTS`, so the second is
//! silently SKIPPED rather than failing. This check steps aside when the name is
//! already held by an index so that message survives.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn verdict(d: Dialect, ops: &str) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, d).map_err(|e| format!("{}: {}", e.code, e.reason))
}

/// Assert the refusal is the one the test names, not merely that one happened.
///
/// Added by the F769 audit. Every site below had a bare `expect_err`, which any
/// earlier rule in the pipeline would satisfy - the trap that left two claims in
/// the partition fixture measuring a different rule than they named.
fn expect_refusal_mentioning(dialect: Dialect, ops: &str, needle: &str, what: &str) {
    let refusal = verdict(dialect, ops).expect_err(what);
    assert!(
        refusal.contains(needle),
        "the refusal must be the {needle:?} one this test names, not another rule \
         that happens to fire first: {refusal}"
    );
}

const A: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":"int","nullable":true}],"primaryKey":["c0"]}"#;
const IX: &str =
    r#"{"op":"createIndex","name":"ix","table":"a","columns":[{"kind":"column","name":"v"}]}"#;

fn tbl(n: &str) -> String {
    format!(
        r#"{{"op":"createTable","name":"{n}","columns":[{{"name":"c0","type":"int","nullable":false}}],"primaryKey":["c0"]}}"#
    )
}

#[test]
fn a_table_may_not_take_a_live_index_name_on_postgres() {
    let refusal = verdict(Dialect::Postgres, &format!("{A},{IX},{}", tbl("ix")))
        .expect_err("an index and a table share the relation namespace");
    assert!(
        refusal.to_lowercase().contains("already"),
        "the refusal must say the name is already taken: {refusal}"
    );
}

#[test]
fn an_index_may_not_take_a_live_table_name_on_postgres() {
    // The reverse direction, and the one a createTable-only rule would miss.
    expect_refusal_mentioning(
        Dialect::Postgres,
        &format!(
            r#"{A},{{"op":"createIndex","name":"a","table":"a","columns":[{{"kind":"column","name":"v"}}]}}"#
        ),
        "already created a table",
        "an index may not be named after a live table",
    );
}

#[test]
fn an_index_may_not_take_a_live_view_name_on_postgres() {
    expect_refusal_mentioning(
        Dialect::Postgres,
        &format!(
            r#"{A},{{"op":"createView","name":"vw","query":{{"kind":"structured","select":{{"from":{{"name":"a"}},"projection":[{{"kind":"colRef","name":"c0"}}]}}}}}},{{"op":"createIndex","name":"vw","table":"a","columns":[{{"kind":"column","name":"v"}}]}}"#
        ),
        "already created a view",
        "an index may not take a live view name",
    );
}

#[test]
fn an_index_may_not_take_a_live_sequence_name_on_postgres() {
    expect_refusal_mentioning(
        Dialect::Postgres,
        &format!(
            r#"{A},{{"op":"createSequence","name":"sq"}},{{"op":"createIndex","name":"sq","table":"a","columns":[{{"kind":"column","name":"v"}}]}}"#
        ),
        "already created a sequence",
        "an index may not take a live sequence name",
    );
}

#[test]
fn sqlite_refuses_it_too() {
    // MEASURED: `there is already a table named a`.
    expect_refusal_mentioning(
        Dialect::Sqlite,
        &format!(
            r#"{A},{{"op":"createIndex","name":"a","table":"a","columns":[{{"kind":"column","name":"v"}}]}}"#
        ),
        "already created a table",
        "SQLite keeps indexes and tables in one namespace",
    );
}

// ---------------------------------------------------------------------------
// Controls.
// ---------------------------------------------------------------------------

#[test]
fn mysql_still_accepts_what_mysql_accepts() {
    // MEASURED: MySQL scopes index names per TABLE, so this is legal there and
    // the engine must not refuse it. Without this control the natural "tidy" fix
    // is a dialect-uniform rule that rejects migrations MySQL runs.
    verdict(
        Dialect::Mysql,
        &format!(
            r#"{A},{{"op":"createIndex","name":"a","table":"a","columns":[{{"kind":"column","name":"v"}}]}}"#
        ),
    )
    .expect("MySQL scopes index names per table; the engine must not refuse this");
}

#[test]
fn index_vs_index_keeps_its_own_better_message() {
    // `validate_index_names_across_ops` explains that the second render is
    // `CREATE INDEX IF NOT EXISTS` and is silently SKIPPED. This check must not
    // preempt that with a vaguer one.
    let refusal = verdict(Dialect::Postgres, &format!("{A},{IX},{IX}"))
        .expect_err("creating one index name twice is still refused");
    assert!(
        refusal.contains("IF NOT EXISTS") || refusal.contains("skipped"),
        "the index-specific message must survive: {refusal}"
    );
}

#[test]
fn distinct_names_are_still_allowed() {
    verdict(Dialect::Postgres, &format!("{A},{IX}"))
        .expect("an index whose name collides with nothing is ordinary");
}

#[test]
fn dropping_an_index_frees_its_name() {
    verdict(
        Dialect::Postgres,
        &format!(
            r#"{A},{IX},{{"op":"dropIndex","name":"ix","table":"a"}},{}"#,
            tbl("ix")
        ),
    )
    .expect("the relation name is free once the index is dropped");
}

#[test]
fn dropping_a_table_frees_the_name_for_an_index() {
    verdict(
        Dialect::Postgres,
        &format!(
            r#"{A},{},{{"op":"dropTable","table":"z"}},{{"op":"createIndex","name":"z","table":"a","columns":[{{"kind":"column","name":"v"}}]}}"#,
            tbl("z")
        ),
    )
    .expect("an index may take a name a dropped table released");
}

/// Dropping a table frees the names of the indexes ON it.
///
/// The same dependent-object shape as F753's partitions, found by continuing the
/// interaction probe that produced it: an index is dropped with its table, so its
/// name comes free. Measured against live PostgreSQL:
///
///     CREATE INDEX ix ON idrop.a (v);
///     DROP TABLE idrop.a;
///     -- pg_indexes reports 0 rows for ix
///     CREATE TABLE idrop.ix (c0 int);       -- SUCCEEDS
///
/// Before this, the relation map released the table and kept the index, refusing
/// a name the database had freed.
///
/// The probe that found it also cleared CONSTRAINT and TRIGGER names: both are
/// keyed per table, so dropping the table removes their entry with it and they
/// were already correct. Indexes are the exception because they live in the
/// SCHEMA-WIDE relation namespace, which is exactly what F715 established.
#[test]
fn dropping_a_table_frees_the_names_of_its_indexes() {
    verdict(
        Dialect::Postgres,
        &format!(r#"{A},{IX},{{"op":"dropTable","table":"a"}},{}"#, tbl("ix")),
    )
    .expect("the index went with its table, so the name is free");
}

#[test]
fn dropping_an_unrelated_table_does_not_free_an_index_name() {
    // THE CONTROL. Releasing every index on any drop would pass the test above
    // and lose what F715 added.
    expect_refusal_mentioning(
        Dialect::Postgres,
        &format!(
            r#"{A},{IX},{},{{"op":"dropTable","table":"other"}},{}"#,
            tbl("other"),
            tbl("ix")
        ),
        "already created a index",
        "an unrelated drop must not release a live index name",
    );
}

/// A RENAME must carry the parentage the previous two commits introduced.
///
/// F753 and F754 added a map from container to dependents so a drop releases
/// both. That map is keyed on the container's NAME - and `renameTable` moves the
/// relation, type, column, constraint and trigger entries without moving it, so
/// renaming a table STRANDED its dependents. A later drop of the new name then
/// released nothing:
///
///     createIndex ix on a; renameTable a -> b; dropTable b; createTable ix
///         -> REFUSED, though PostgreSQL frees the name
///
/// Measured live: after the rename and drop, `CREATE TABLE rdep.ix` succeeds.
///
/// This is a defect introduced BY the fix for the previous one - the second-order
/// cost of adding state. Every other per-container map in this walk is moved on
/// rename; the new one was not, because it was added after that arm was written
/// and nothing forced the author to revisit it.
#[test]
fn a_rename_carries_the_index_parentage_so_a_later_drop_still_frees_it() {
    verdict(
        Dialect::Postgres,
        &format!(
            r#"{A},{IX},{{"op":"renameTable","table":"a","to":"b"}},{{"op":"dropTable","table":"b"}},{}"#,
            tbl("ix")
        ),
    )
    .expect("the index went with the renamed table when it was dropped");
}
