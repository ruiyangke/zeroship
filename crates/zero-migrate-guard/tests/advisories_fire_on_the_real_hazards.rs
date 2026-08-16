//! The guard's THIRD duty: operational advisories.
//!
//! Its header names "the deny-list, the cross-schema confinement, and the
//! operational advisories". The sibling fixtures pin the first two against
//! nesting evasion. This one pins the third.
//!
//! ADVISORIES FAIL SILENTLY BY CONSTRUCTION, which is why they need pinning more
//! than the other two do. A deny-list rule that stops firing lets a hostile
//! statement through and the damage is visible. An advisory that stops firing
//! produces NO error, NO refusal and NO diagnostic - the migration simply runs,
//! and the operator is never told it will hold an ACCESS EXCLUSIVE lock over a
//! full table scan. Nothing about the outcome distinguishes "no hazard here" from
//! "the hazard detector broke".
//!
//! MEASURED, each raising a NAMED rule:
//!
//!     SET NOT NULL without a default      SET_NOT_NULL_FULL_SCAN
//!     ADD COLUMN … DEFAULT gen_random_uuid()  TABLE_REWRITE
//!     CREATE INDEX (non-concurrent)       NON_CONCURRENT_INDEX
//!     ALTER COLUMN … TYPE bigint          LOSSY_TYPE_CHANGE
//!     ADD FOREIGN KEY (validated)         CONSTRAINT_NOT_VALIDATED + FK_WITHOUT_INDEX
//!     ADD CHECK (validated)               CONSTRAINT_NOT_VALIDATED
//!     DROP COLUMN                         DESTRUCTIVE_DROP
//!     VACUUM FULL                         LOCK_HEAVY_MAINTENANCE
//!
//! THE TWO SILENT CASES ARE THE POINT OF THE FIXTURE, not an afterthought:
//! `CREATE INDEX CONCURRENTLY` and a plain `CREATE TABLE` raise NOTHING. The
//! concurrent index is the sharp one - it is the same statement KIND as the
//! non-concurrent case and differs only in the property that makes it safe. An
//! analyser keyed on statement type rather than on the hazard would flag both,
//! and advisories that fire on safe work are how operators learn to ignore them.

use zero_migrate_guard::analysis::analyze::analyze;

#[track_caller]
fn advises(sql: &str, expected_rule: &str) {
    let found = analyze(sql);
    assert!(
        found
            .iter()
            .any(|a| format!("{a:?}").contains(expected_rule)),
        "expected advisory {expected_rule:?} for {sql:?}, got {found:?}"
    );
}

#[track_caller]
fn silent(sql: &str) {
    let found = analyze(sql);
    assert!(
        found.is_empty(),
        "this statement carries no operational hazard and must not be flagged - \
         advisories that fire on safe work train operators to ignore them: \
         {sql:?} produced {found:?}"
    );
}

#[test]
fn lock_and_scan_hazards_are_advised() {
    advises(
        "ALTER TABLE app1.t ALTER COLUMN c SET NOT NULL",
        "SET_NOT_NULL_FULL_SCAN",
    );
    advises("CREATE INDEX ix ON app1.t (c)", "NON_CONCURRENT_INDEX");
    advises("VACUUM FULL app1.t", "LOCK_HEAVY_MAINTENANCE");
}

#[test]
fn table_rewrites_are_advised() {
    advises(
        "ALTER TABLE app1.t ADD COLUMN c uuid NOT NULL DEFAULT gen_random_uuid()",
        "TABLE_REWRITE",
    );
    advises(
        "ALTER TABLE app1.t ALTER COLUMN c TYPE bigint",
        "LOSSY_TYPE_CHANGE",
    );
}

#[test]
fn validating_constraint_adds_are_advised() {
    advises(
        "ALTER TABLE app1.t ADD CONSTRAINT fk FOREIGN KEY (p) REFERENCES app1.p(id)",
        "CONSTRAINT_NOT_VALIDATED",
    );
    advises(
        "ALTER TABLE app1.t ADD CONSTRAINT ck CHECK (c > 0)",
        "CONSTRAINT_NOT_VALIDATED",
    );
    // The same statement also earns a second, different advisory - an unindexed
    // FK target. Two hazards in one statement must both be reported, or the
    // operator fixes the one they were told about and ships the other.
    advises(
        "ALTER TABLE app1.t ADD CONSTRAINT fk FOREIGN KEY (p) REFERENCES app1.p(id)",
        "FK_WITHOUT_INDEX",
    );
}

#[test]
fn destructive_ddl_is_advised() {
    advises("ALTER TABLE app1.t DROP COLUMN c", "DESTRUCTIVE_DROP");
}

#[test]
fn safe_statements_are_left_silent() {
    // THE CONTROLS, and the concurrent index is the sharp one: same statement
    // KIND as the flagged case, differing only in the property that makes it
    // safe. An analyser keyed on statement type would flag both.
    silent("CREATE INDEX CONCURRENTLY ix ON app1.t (c)");
    silent("CREATE TABLE app1.t (id int)");
}
