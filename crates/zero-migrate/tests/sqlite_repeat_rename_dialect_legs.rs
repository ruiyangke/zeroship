//! **The SQLite repeat-rename refusal sees inside the selected dialect leg.**
//!
//! SQLite has no `ALTER TABLE ... RENAME COLUMN` that this engine can compose freely:
//! a rename is lowered as a table REBUILD from the stored `CREATE` SQL. Two renames of
//! the same table in one migration would run the second rebuild against SQL captured
//! before the first, so `refuse_repeat_sqlite_rename_target` refuses the pair up front
//! rather than emitting a rebuild from a stale definition.
//!
//! That refusal scanned only the top-level ops and matched `Op::RenameColumn`
//! directly, so a rename authored inside `dialect({ sqlite: ... })` was invisible to
//! it. The rename still LOWERS - the lowering path expands legs correctly - so the
//! wrapper did not avoid the hazard, only the guard against it. A safety check that a
//! wrapper walks past is worse than one that was never written, because the refusal
//! message is what tells an author to split the migration.
//!
//! SELECTED leg, not every leg, and the direction is the opposite of the ownership and
//! checksum walkers: this refusal is about what SQLite will actually RUN. A rename
//! sitting in the PostgreSQL leg is never executed here and never rebuilds anything,
//! so refusing on it would reject a migration that is correct on this target.
//!
//! No server needed: the refusal happens during lowering.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::{IrAuthor, LiveSchema, SqlDialect};

const PROJECT: &str = "app";
const APP: &str = "app";

/// Lower under SQLite and report the error text, or `None` when it lowered cleanly.
fn lower_error(ops_json: &str) -> Option<String> {
    let raw = format!(r#"{{"ir_version":1,"name":"renames","ops":{ops_json}}}"#);
    let ir: MigrationIr = serde_json::from_str(&raw).expect("the rename test IR parses");
    IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &support::no_inject("app"))
        .lower_steps(&ir, &LiveSchema::default())
        .err()
        .map(|error| error.to_string())
}

/// The control: two top-level renames of one table are already refused, so the arms
/// below are measuring the wrapper and not a refusal that never worked.
#[test]
fn two_top_level_renames_of_one_table_are_refused() {
    let error = lower_error(
        r#"[{"op":"renameColumn","table":"notes","from":"a","to":"b","type":"text"},
            {"op":"renameColumn","table":"notes","from":"b","to":"c","type":"text"}]"#,
    )
    .expect("two renames of one table must be refused on SQLite");
    assert!(
        error.contains("renamed twice in one migration"),
        "the specific repeat-rename refusal must fire, not some other error: {error}"
    );
}

/// The reported defect: the same pair, with the second rename authored inside the
/// SELECTED SQLite leg. It lowers to the same two rebuilds and must be refused the
/// same way.
#[test]
fn a_repeat_rename_inside_the_selected_leg_is_refused() {
    let error = lower_error(
        r#"[{"op":"renameColumn","table":"notes","from":"a","to":"b","type":"text"},
            {"op":"dialectal","sqlite":[
              {"op":"renameColumn","table":"notes","from":"b","to":"c","type":"text"}]}]"#,
    )
    .expect("a wrapper must not hide the second rename from the refusal");
    assert!(
        error.contains("renamed twice in one migration"),
        "the specific repeat-rename refusal must fire whether or not a leg wrapped the second rename: {error}"
    );
}

/// Both renames inside one leg, which is the shape an author writing a SQLite-specific
/// migration would actually produce.
#[test]
fn two_renames_inside_one_leg_are_refused() {
    let error = lower_error(
        r#"[{"op":"dialectal","sqlite":[
              {"op":"renameColumn","table":"notes","from":"a","to":"b","type":"text"},
              {"op":"renameColumn","table":"notes","from":"b","to":"c","type":"text"}]}]"#,
    )
    .expect("two renames inside one leg are still two rebuilds of one table");
    assert!(
        error.contains("renamed twice in one migration"),
        "the specific repeat-rename refusal must fire: {error}"
    );
}

/// The control that keeps this SELECTED-leg rather than every-leg. The second rename
/// lives in the PostgreSQL leg, so SQLite never runs it and never rebuilds twice -
/// refusing here would reject a migration that is correct on this target.
///
/// Asserts the ABSENCE OF THIS REFUSAL rather than a clean lowering, deliberately. A
/// rename needs the live column's type to lower at all, and these fixtures carry an
/// empty [`LiveSchema`], so every arm here ends in some error; the subject under test
/// is which one. Asserting success would mean building a live catalog that has nothing
/// to do with the property, and asserting "some error occurred" is what made the first
/// draft of the other three arms pass vacuously - they matched the table name inside
/// the live-schema error and never exercised the refusal at all.
#[test]
fn a_rename_in_an_unselected_leg_does_not_trigger_the_refusal() {
    let error = lower_error(
        r#"[{"op":"renameColumn","table":"notes","from":"a","to":"b","type":"text"},
            {"op":"dialectal","pg":[
              {"op":"renameColumn","table":"notes","from":"b","to":"c","type":"text"}]}]"#,
    )
    .unwrap_or_default();
    assert!(
        !error.contains("renamed twice in one migration"),
        "SQLite selects no leg here, so there is exactly one rebuild and the \
         repeat-rename refusal must not fire: {error}"
    );
}
