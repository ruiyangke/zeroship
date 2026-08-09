//! `access.rls`, `access.policy` and `schema.create_schema` resolve PER CONCRETE
//! OBJECT on the raw-SQL guard entry, not against one fixed schema witness.
//!
//! All three are registered object-scoped (`access.rls` and `access.policy` are
//! `PerTable`, `schema.create_schema` is `PerSchema`), so a charter may grant them on
//! one table, one schema, or everything. Reading them at a single fixed witness erases
//! that in both directions: a narrow include never covers the witness, so a granted
//! operation is refused; an exclude narrower than a table never carves the witness, so
//! an excluded operation is admitted.
//!
//! Every arm below shares one base charter and one statement. Only the grant's scope
//! changes between them, and the `scope = "all"` arm is the control that must behave
//! identically before and after.

mod support;

use zero_migrate_guard::guard::denylist::rule;
use zero_migrate_guard::guard::{GuardConfig, GuardError, SqlGuard};
use zero_migrate_ir::dialect::SqlDialect;

/// A charter owning the `app` schema, plus whatever grant text an arm supplies.
fn charter(grant: &str) -> String {
    format!(
        r#"policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = {{ include = ["app"] }}

[[grant]]
key = "schema.create_table"
value = true
scope = {{ include = ["app"] }}

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
{grant}"#
    )
}

fn guard_for(grant: &str) -> SqlGuard {
    SqlGuard::new(GuardConfig::from_policy(
        support::effective_policy_from_charter_toml(&charter(grant)),
        SqlDialect::Postgres,
    ))
}

#[track_caller]
fn assert_admitted(guard: &SqlGuard, sql: &str) {
    if let Err(err) = guard.check(sql) {
        panic!("expected the charter's own grant to admit `{sql}`, got {err:?}");
    }
}

#[track_caller]
fn assert_denied_by(guard: &SqlGuard, sql: &str, want: &str) {
    match guard.check(sql) {
        Err(GuardError::Denied { rule, .. }) => assert_eq!(rule, want, "denial rule for `{sql}`"),
        other => panic!("expected `{sql}` to be denied by '{want}', got {other:?}"),
    }
}

// -- the statements each knob decides ---------------------------------------------

const RLS_ON_COVERED: &str = "ALTER TABLE app.users ENABLE ROW LEVEL SECURITY";
const RLS_ON_EXCLUDED: &str = "ALTER TABLE app.secret ENABLE ROW LEVEL SECURITY";

const CREATE_POLICY_COVERED: &str = "CREATE POLICY tenant ON app.users USING (true)";
const CREATE_POLICY_EXCLUDED: &str = "CREATE POLICY tenant ON app.secret USING (true)";
const DROP_POLICY_COVERED: &str = "DROP POLICY tenant ON app.users";
const DROP_POLICY_EXCLUDED: &str = "DROP POLICY tenant ON app.secret";

const CREATE_SCHEMA_APP: &str = "CREATE SCHEMA app";
const DROP_SCHEMA_APP: &str = "DROP SCHEMA app";

// -- access.rls -------------------------------------------------------------------

const RLS_NARROW: &str = r#"
[[grant]]
key = "access.rls"
value = true
scope = { include = ["app.users"] }
"#;

const RLS_WILDCARD_EXCLUDING_SECRET: &str = r#"
[[grant]]
key = "access.rls"
value = true
scope = { include = ["*"], exclude = ["app.secret"] }
"#;

const RLS_ALL: &str = r#"
[[grant]]
key = "access.rls"
value = true
scope = "all"
"#;

/// A grant naming exactly `app.users` must reach `ALTER TABLE app.users ... ROW LEVEL
/// SECURITY`.
#[test]
fn narrow_rls_grant_admits_the_table_it_names() {
    let guard = guard_for(RLS_NARROW);
    assert_admitted(&guard, RLS_ON_COVERED);
}

/// The same narrow grant must NOT reach a table it does not name.
#[test]
fn narrow_rls_grant_refuses_a_table_outside_it() {
    let guard = guard_for(RLS_NARROW);
    assert_denied_by(&guard, RLS_ON_EXCLUDED, rule::UNSAFE_ALTER_TABLE_CMD);
}

/// A wildcard grant carving `app.secret` out must refuse the carved table.
#[test]
fn excluded_table_is_refused_rls() {
    let guard = guard_for(RLS_WILDCARD_EXCLUDING_SECRET);
    assert_admitted(&guard, RLS_ON_COVERED);
    assert_denied_by(&guard, RLS_ON_EXCLUDED, rule::UNSAFE_ALTER_TABLE_CMD);
}

/// CONTROL - same statements, same base charter, scope `all`.
#[test]
fn top_scoped_rls_grant_behaviour_is_unchanged() {
    let guard = guard_for(RLS_ALL);
    assert_admitted(&guard, RLS_ON_COVERED);
    assert_admitted(&guard, RLS_ON_EXCLUDED);
    // Ungranted stays denied at every table.
    let none = guard_for("");
    assert_denied_by(&none, RLS_ON_COVERED, rule::UNSAFE_ALTER_TABLE_CMD);
    assert_denied_by(&none, RLS_ON_EXCLUDED, rule::UNSAFE_ALTER_TABLE_CMD);
}

// -- access.policy ----------------------------------------------------------------

const POLICY_NARROW: &str = r#"
[[grant]]
key = "access.policy"
value = true
scope = { include = ["app.users"] }
"#;

const POLICY_WILDCARD_EXCLUDING_SECRET: &str = r#"
[[grant]]
key = "access.policy"
value = true
scope = { include = ["*"], exclude = ["app.secret"] }
"#;

const POLICY_ALL: &str = r#"
[[grant]]
key = "access.policy"
value = true
scope = "all"
"#;

/// A grant naming exactly `app.users` must reach both spellings on that table.
#[test]
fn narrow_policy_grant_admits_the_table_it_names() {
    let guard = guard_for(POLICY_NARROW);
    assert_admitted(&guard, CREATE_POLICY_COVERED);
    assert_admitted(&guard, DROP_POLICY_COVERED);
}

#[test]
fn narrow_policy_grant_refuses_a_table_outside_it() {
    let guard = guard_for(POLICY_NARROW);
    assert_denied_by(&guard, CREATE_POLICY_EXCLUDED, rule::UNRECOGNIZED_DANGEROUS);
    assert_denied_by(&guard, DROP_POLICY_EXCLUDED, rule::UNRECOGNIZED_DANGEROUS);
}

#[test]
fn excluded_table_is_refused_policy() {
    let guard = guard_for(POLICY_WILDCARD_EXCLUDING_SECRET);
    assert_admitted(&guard, CREATE_POLICY_COVERED);
    assert_admitted(&guard, DROP_POLICY_COVERED);
    assert_denied_by(&guard, CREATE_POLICY_EXCLUDED, rule::UNRECOGNIZED_DANGEROUS);
    assert_denied_by(&guard, DROP_POLICY_EXCLUDED, rule::UNRECOGNIZED_DANGEROUS);
}

/// CONTROL - same statements, same base charter, scope `all`.
#[test]
fn top_scoped_policy_grant_behaviour_is_unchanged() {
    let guard = guard_for(POLICY_ALL);
    for sql in [
        CREATE_POLICY_COVERED,
        CREATE_POLICY_EXCLUDED,
        DROP_POLICY_COVERED,
        DROP_POLICY_EXCLUDED,
    ] {
        assert_admitted(&guard, sql);
    }
    let none = guard_for("");
    for sql in [
        CREATE_POLICY_COVERED,
        CREATE_POLICY_EXCLUDED,
        DROP_POLICY_COVERED,
        DROP_POLICY_EXCLUDED,
    ] {
        assert_denied_by(&none, sql, rule::UNRECOGNIZED_DANGEROUS);
    }
}

// -- schema.create_schema ---------------------------------------------------------

const CREATE_SCHEMA_NARROW: &str = r#"
[[grant]]
key = "schema.create_schema"
value = true
scope = { include = ["app"] }
"#;

const CREATE_SCHEMA_WILDCARD_EXCLUDING_APP: &str = r#"
[[grant]]
key = "schema.create_schema"
value = true
scope = { include = ["*"], exclude = ["app"] }
"#;

const CREATE_SCHEMA_ALL: &str = r#"
[[grant]]
key = "schema.create_schema"
value = true
scope = "all"
"#;

/// A grant naming exactly `app` must reach `CREATE SCHEMA app` and `DROP SCHEMA app`.
#[test]
fn narrow_create_schema_grant_admits_the_schema_it_names() {
    let guard = guard_for(CREATE_SCHEMA_NARROW);
    assert_admitted(&guard, CREATE_SCHEMA_APP);
    assert_admitted(&guard, DROP_SCHEMA_APP);
}

/// The wildcard-with-exclude arm for `CREATE SCHEMA` is deliberately absent: the
/// create path already resolves per object in `check_namespace_structural`, which
/// refuses an excluded schema with `CreateSchemaNotGranted`, so that direction is
/// already closed and would not be a RED. `DROP SCHEMA` has no such resolver - it is
/// admitted today on the witness read alone, and that is what this pins.
#[test]
fn excluded_schema_is_refused_drop_schema() {
    let guard = guard_for(CREATE_SCHEMA_WILDCARD_EXCLUDING_APP);
    assert_denied_by(&guard, DROP_SCHEMA_APP, rule::UNRECOGNIZED_DANGEROUS);
}

/// CONTROL - same statements, same base charter, scope `all`.
#[test]
fn top_scoped_create_schema_grant_behaviour_is_unchanged() {
    let guard = guard_for(CREATE_SCHEMA_ALL);
    assert_admitted(&guard, CREATE_SCHEMA_APP);
    assert_admitted(&guard, DROP_SCHEMA_APP);
    let none = guard_for("");
    assert_denied_by(&none, CREATE_SCHEMA_APP, rule::UNRECOGNIZED_DANGEROUS);
    assert_denied_by(&none, DROP_SCHEMA_APP, rule::UNRECOGNIZED_DANGEROUS);
}
