//! Cross-schema confinement must hold wherever the foreign name is HIDDEN.
//!
//! The sibling fixture `denylist_resists_nesting_evasion.rs` does this for the
//! deny-list rules. Confinement is the guard's OTHER security axis - its header
//! names "the deny-list, the cross-schema confinement, and the operational
//! advisories" as three separate duties - and `guard_smoke.rs` covers only the
//! direct case, `CREATE TABLE other.t`.
//!
//! The failure mode is the same one nesting always produces: a check that reads
//! the statement's obvious target and misses a foreign schema reached one level
//! down. Confinement failing that way is worse than a deny-list rule failing,
//! because the payload is a read or a write against another tenant's data rather
//! than a refused verb.
//!
//! MEASURED - every one denied, naming the foreign schema:
//!
//!     CREATE TABLE other.t                          direct
//!     WITH x AS (SELECT * FROM other.secrets) ...   CTE
//!     CREATE VIEW v AS SELECT (SELECT ... other.…)  scalar subquery
//!     ... JOIN other.secrets ON true                join target
//!     CREATE TABLE app1.t (id int REFERENCES
//!       other.parent(id))                           foreign-key target
//!     ... DEFAULT (SELECT max(id) FROM other.…)     column default
//!     CREATE FUNCTION app1.f() ... other.secrets    function body
//!     DO $$ EXECUTE 'DROP TABLE other.secrets' $$   string-literal payload
//!     SET search_path = other                       reached via search_path
//!
//! The last is denied by `forbidden_set_param` rather than by confinement, which
//! is the right layering: rather than trusting confinement to notice every
//! unqualified name that a changed `search_path` would redirect, the guard
//! refuses to let the path change at all.
//!
//! TWO CONTROLS, because a guard that denied every nested statement, or every
//! statement naming any schema, would satisfy all nine assertions above.

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
fn denied_naming_the_foreign_schema(sql: &str) {
    let error = confined()
        .check(sql)
        .err()
        .unwrap_or_else(|| panic!("confinement must deny this, it was ALLOWED: {sql}"));
    let rendered = format!("{error}");
    assert!(
        rendered.contains("other"),
        "the refusal must name the foreign schema so an operator can see what was \
         reached for, got: {rendered}"
    );
}

#[test]
fn a_foreign_schema_hidden_by_syntax_is_still_denied() {
    for sql in [
        "CREATE TABLE other.t (id int)",
        "WITH x AS (SELECT * FROM other.secrets) SELECT * FROM x",
        "CREATE VIEW v AS SELECT (SELECT count(*) FROM other.secrets)",
        "CREATE VIEW v AS SELECT 1 FROM app1.t JOIN other.secrets ON true",
        "CREATE TABLE app1.t (id int REFERENCES other.parent(id))",
        "CREATE TABLE app1.t (id int DEFAULT (SELECT max(id) FROM other.secrets))",
    ] {
        denied_naming_the_foreign_schema(sql);
    }
}

#[test]
fn a_foreign_schema_inside_a_body_or_a_literal_is_still_denied() {
    denied_naming_the_foreign_schema(
        "CREATE FUNCTION app1.f() RETURNS int AS $$ SELECT count(*)::int FROM other.secrets $$ LANGUAGE sql",
    );
    denied_naming_the_foreign_schema("DO $$ BEGIN EXECUTE 'DROP TABLE other.secrets'; END $$");
}

#[test]
fn the_search_path_route_is_closed_at_its_source() {
    // Not a confinement refusal, deliberately: instead of trusting confinement to
    // catch every unqualified name a redirected search_path would resolve
    // elsewhere, the guard refuses the redirect itself.
    let error = confined()
        .check("SET search_path = other")
        .expect_err("changing search_path must be refused");
    let rendered = format!("{error}");
    assert!(
        rendered.contains("forbidden_set_param"),
        "the search_path route must be closed by the set-param rule, got: {rendered}"
    );
}

#[test]
fn own_schema_work_is_still_allowed() {
    // THE CONTROLS. A guard that denied every nested statement - or every
    // statement mentioning a schema at all - would pass every assertion above.
    let g = confined();
    g.check("CREATE TABLE app1.t (id int)")
        .expect("ordinary own-schema DDL must pass");
    g.check("WITH x AS (SELECT * FROM app1.t) SELECT * FROM x")
        .expect("an own-schema CTE must pass");
}
