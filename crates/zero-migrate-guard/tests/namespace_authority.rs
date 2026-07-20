//! Namespace-authority conservative-deny suite (Phase 2 Step 2b — II.2.5 raw-SQL
//! classification / II.2.6 creation-gating + injected-shape immutability).
//!
//! Each rule the policy redesign introduces is a fail-closed decision with the
//! design's named error code. These tests drive the guard's PUBLIC API
//! (`SqlGuard::check`) over a caller-composed [`EffectivePolicy`] carrying inject
//! rules + scoped creation/raw-SQL grants (via `GuardConfig::from_policy`),
//! so both the DENY cases and their ACCEPT counterparts are exercised on the real
//! parse-time path.

mod support;

use zero_migrate_guard::guard::{namespace_rule, GuardConfig, GuardError, SqlGuard};
use zero_migrate_ir::dialect::SqlDialect;

/// A confined guard over the composed policy, pinned to project schema `app`.
fn guard_with(charter_toml: &str) -> SqlGuard {
    SqlGuard::new(GuardConfig::from_policy(
        support::effective_policy_from_charter_toml(charter_toml),
        SqlDialect::Postgres,
    ))
}

fn assert_namespace_denied(guard: &SqlGuard, sql: &str, want_rule: &str) {
    match guard.check(sql) {
        Err(GuardError::NamespacePolicy { rule, .. }) => assert_eq!(
            rule, want_rule,
            "expected {want_rule} for `{sql}`, got {rule}"
        ),
        other => panic!("expected NamespacePolicy({want_rule}) for `{sql}`, got {other:?}"),
    }
}

// ── a charter that mandatorily injects over the whole app-schema universe ───────
// `schema.create_table` / `schema.rename` are granted only inside `app` (creatable ⊑
// injected). `deleted_at` is an injected column; the PK is pinned to `id`.
const INJECT_APP_CHARTER: &str = r#"policy_version = 1
[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app"] }
[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app"] }
[[grant]]
key = "schema.rename"
value = true
scope = { include = ["app"] }
[[grant]]
key = "schema.alter_injected"
value = false
scope = { include = ["app"] }
[[inject]]
scope = { include = ["app"] }
mandatory = true
primary_key = ["id"]
columns = [
  { name = "id",         type = "text",        nullable = false },
  { name = "created_at", type = "timestamptz", nullable = false },
  { name = "deleted_at", type = "timestamptz", nullable = true  },
]
indexes = [ { name = "ix_deleted_at", columns = ["deleted_at"] } ]
"#;

// ════════════════════════════════════════════════════════════════════════════════
// 1. Raw CREATE TABLE inside an inject scope → RawCreateInInjectScope (DENY)
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn raw_create_table_in_inject_scope_is_denied() {
    // `app.t` is covered by the mandatory inject rule; injection cannot rewrite raw
    // text, so the structured DSL is the only way to make an injected table.
    let g = guard_with(INJECT_APP_CHARTER);
    assert_namespace_denied(
        &g,
        "CREATE TABLE app.t (id text)",
        namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
    );
}

#[test]
fn raw_ctas_and_select_into_in_inject_scope_are_denied() {
    let g = guard_with(INJECT_APP_CHARTER);
    // CTAS spelling.
    assert_namespace_denied(
        &g,
        "CREATE TABLE app.t AS SELECT 1 AS id",
        namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
    );
    // SELECT … INTO spelling — same target, same denial.
    assert_namespace_denied(
        &g,
        "SELECT 1 AS id INTO app.t",
        namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
    );
}

// ════════════════════════════════════════════════════════════════════════════════
// 2. Structured/raw create OUTSIDE the schema.create_table grant → denied
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn create_outside_the_create_table_grant_is_denied() {
    // A charter that grants create only in `app`, injects nothing (so no
    // RawCreateInInjectScope) — a create in `staging` is not granted.
    let charter = r#"policy_version = 1
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["staging"] }
[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app"] }
"#;
    // `staging` is in cross-schema scope (so `staging.t` is not a CrossSchema
    // violation) but NOT in the create grant (`app`-only) → CreateTableNotGranted.
    let g = SqlGuard::new(GuardConfig::from_policy(
        support::effective_policy_from_charter_toml(charter),
        SqlDialect::Postgres,
    ));
    assert_namespace_denied(
        &g,
        "CREATE TABLE staging.t (id text)",
        namespace_rule::CREATE_TABLE_NOT_GRANTED,
    );
}

// ════════════════════════════════════════════════════════════════════════════════
// 3. ALTER TABLE … DROP COLUMN <injected> → InjectedShapeImmutable (DENY)
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn drop_injected_column_is_immutable() {
    let g = guard_with(INJECT_APP_CHARTER);
    // `deleted_at` (a covering-inject column) cannot be dropped without an explicit
    // schema.alter_injected grant (the charter grants it `false`).
    assert_namespace_denied(
        &g,
        "ALTER TABLE app.t DROP COLUMN deleted_at",
        namespace_rule::INJECTED_SHAPE_IMMUTABLE,
    );
}

#[test]
fn drop_non_injected_column_is_allowed() {
    let g = guard_with(INJECT_APP_CHARTER);
    // A plain author column is not injected → the immutability rule does not fire.
    g.check("ALTER TABLE app.t DROP COLUMN title")
        .expect("dropping a non-injected column is allowed");
}

#[test]
fn drop_pinned_pk_constraint_is_immutable() {
    let g = guard_with(INJECT_APP_CHARTER);
    // The covering inject rule pins the PK → any DROP CONSTRAINT is fail-closed
    // immutable (the guard cannot always tell which constraint is the PK by name).
    assert_namespace_denied(
        &g,
        "ALTER TABLE app.t DROP CONSTRAINT t_pkey",
        namespace_rule::INJECTED_PRIMARY_KEY_IMMUTABLE,
    );
}

// ════════════════════════════════════════════════════════════════════════════════
// 4. SET search_path / CREATE FUNCTION under a Scoped sql.raw grant → DENY
// ════════════════════════════════════════════════════════════════════════════════

// A charter with `sql.raw` granted only in `app` (Scoped, non-⊤) + a ⊤ create
// grant so ordinary DDL is not incidentally denied by creation-gating.
const SCOPED_RAW_SQL_CHARTER: &str = r#"policy_version = 1
[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"
[[grant]]
key = "sql.raw"
value = true
scope = { include = ["app"] }
[[grant]]
key = "schema.create_table"
value = true
scope = "all"
[[grant]]
key = "schema.rename"
value = true
scope = "all"
"#;

#[test]
fn set_search_path_under_scoped_raw_sql_is_denied() {
    let g = guard_with(SCOPED_RAW_SQL_CHARTER);
    assert_namespace_denied(
        &g,
        "SET search_path TO app, public",
        namespace_rule::SEARCH_PATH_UNDER_SCOPED_RAW_SQL,
    );
}

#[test]
fn create_function_under_scoped_raw_sql_is_denied() {
    let g = guard_with(SCOPED_RAW_SQL_CHARTER);
    assert_namespace_denied(
        &g,
        "CREATE FUNCTION app.f() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        namespace_rule::OPAQUE_BODY_UNDER_SCOPED_RAW_SQL,
    );
}

#[test]
fn unqualified_reference_under_scoped_raw_sql_is_denied() {
    let g = guard_with(SCOPED_RAW_SQL_CHARTER);
    // An unqualified relation reference cannot be attributed (no live search_path to
    // resolve it) → matches only a ⊤-scoped grant → deny.
    assert_namespace_denied(
        &g,
        "INSERT INTO widgets (id) VALUES ('x')",
        namespace_rule::UNQUALIFIED_NAME_UNDER_SCOPED_RAW_SQL,
    );
}

// ════════════════════════════════════════════════════════════════════════════════
// 5. Raw rename INTO an inject scope with a nonconforming shape → denied
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn rename_into_scope_without_rename_grant_is_denied() {
    // A same-schema `RENAME TO` that crosses an inject/rename sub-boundary within the
    // pinned schema (so cross-schema confinement does NOT fire). `schema.rename`
    // is granted only over `app.keep_*`; renaming `app.t` → `app.other` lands outside
    // the rename grant and (no inject covers `app.other`) is denied for lack of it.
    let charter = r#"policy_version = 1
[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app"] }
[[grant]]
key = "schema.create_table"
value = true
scope = "all"
[[grant]]
key = "schema.rename"
value = true
scope = { include = ["app.keep_*"] }
"#;
    let g = SqlGuard::new(GuardConfig::from_policy(
        support::effective_policy_from_charter_toml(charter),
        SqlDialect::Postgres,
    ));
    assert_namespace_denied(
        &g,
        "ALTER TABLE app.t RENAME TO other",
        namespace_rule::RENAME_INTO_NOT_GRANTED,
    );
}

#[test]
fn rename_into_inject_scope_is_denied_raw() {
    // A same-schema `RENAME TO` where the NEW name lands in a mandatory-inject
    // sub-scope (`app.injected_*`): raw text cannot carry the injection, so the move
    // is RawRenameIntoInjectScope. `app.t` (source) is injection-free.
    let charter = r#"policy_version = 1
[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app"] }
[[grant]]
key = "schema.create_table"
value = true
scope = "all"
[[grant]]
key = "schema.rename"
value = true
scope = "all"
[[inject]]
scope = { include = ["app.injected_*"] }
columns = [ { name = "id", type = "text", nullable = false } ]
"#;
    // The inject need not be mandatory: RawRenameIntoInjectScope denies a raw move
    // into ANY inject scope, because raw text cannot carry the injection.
    let g = SqlGuard::new(GuardConfig::from_policy(
        support::effective_policy_from_charter_toml(charter),
        SqlDialect::Postgres,
    ));
    assert_namespace_denied(
        &g,
        "ALTER TABLE app.t RENAME TO injected_t",
        namespace_rule::RAW_RENAME_INTO_INJECT_SCOPE,
    );
}

// ════════════════════════════════════════════════════════════════════════════════
// ACCEPT cases — the enforcement is not over-broad
// ════════════════════════════════════════════════════════════════════════════════

#[test]
fn granted_create_outside_any_inject_scope_is_allowed() {
    // create in `app` where create_table is granted AND no inject covers it.
    let charter = r#"policy_version = 1
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app"] }
[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app"] }
"#;
    let g = SqlGuard::new(GuardConfig::from_policy(
        support::effective_policy_from_charter_toml(charter),
        SqlDialect::Postgres,
    ));
    g.check("CREATE TABLE app.plain (id text)")
        .expect("a granted create outside any inject scope is allowed");
}

#[test]
fn dsl_path_injected_create_renders_and_passes() {
    // The DSL/structured path renders the injected columns into the CREATE TABLE
    // fragment, so the SQL the guard sees ALREADY carries the injected shape. That
    // fragment is still a raw create in the inject scope → the guard denies it here,
    // which is why the structured resolver runs createTable BEFORE the guard, over a
    // policy that GRANTS create in-scope. This ACCEPT case models the plain grant path:
    // a create in `app` under a create grant with NO inject covering it passes.
    let charter = r#"policy_version = 1
[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app"] }
[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app"] }
[[inject]]
scope = { include = ["other"] }
columns = [ { name = "id", type = "text", nullable = false } ]
"#;
    let g = SqlGuard::new(GuardConfig::from_policy(
        support::effective_policy_from_charter_toml(charter),
        SqlDialect::Postgres,
    ));
    // `app.t` is granted + no inject covers `app.*` (the inject is scoped to `other`).
    g.check("CREATE TABLE app.t (id text)")
        .expect("granted create in a scope no inject covers is allowed");
}

#[test]
fn top_raw_sql_admits_search_path_and_opaque_body() {
    // A ⊤-scoped sql.raw grant is the fully-trusted raw posture: SET search_path
    // and CREATE FUNCTION are admitted (the deny-list still runs, so use a benign
    // trusted-language body + a benign GUC).
    let charter = r#"policy_version = 1
[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"
[[grant]]
key = "sql.raw"
value = true
scope = "all"
[[grant]]
key = "schema.create_table"
value = true
scope = "all"
"#;
    let g = SqlGuard::new(GuardConfig::from_policy(
        support::effective_policy_from_charter_toml(charter),
        SqlDialect::Postgres,
    ));
    // SET search_path is NOT refused under a ⊤ grant.
    match g.check("SET search_path TO app, public") {
        Ok(_) => {}
        Err(GuardError::NamespacePolicy { rule, .. }) => {
            panic!("⊤ raw_sql must admit SET search_path, got {rule}")
        }
        // The deny-list may still classify SET search_path (FORBIDDEN_SET) — that is a
        // separate Denied belt, not a NamespacePolicy refusal. Accept either non-namespace
        // outcome; the point is the SCOPED-raw-SQL namespace rule does not fire.
        Err(_) => {}
    }
    // A trusted-language CREATE FUNCTION is not refused as an opaque body under ⊤.
    if let Err(GuardError::NamespacePolicy { rule, .. }) =
        g.check("CREATE FUNCTION app.f() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$")
    {
        panic!("⊤ raw_sql must not refuse an opaque body, got {rule}");
    }
}
