//! `safety.require_rls` resolves PER CONCRETE TABLE, not against one fixed witness.
//!
//! The knob is registered `ObjectModel::PerTable`, so a charter may obligate RLS on
//! `app.users`, on the whole `app` schema, or through a `default_scope` the require
//! rule inherits. Each of those must reach a `CREATE TABLE app.users` that never
//! enables row level security; the `scope = "all"` control differs from them in
//! exactly one variable (the rule's scope) and must reach it too.
//!
//! The arms below all share one base charter and one IR. Only the require rule's
//! scope changes between them.

mod support;

use zero_migrate_guard::guard::{
    check_ir_data_security_policy, data_security_rule, GuardConfig, GuardError,
};
use zero_migrate_ir::dialect::SqlDialect;
use zero_migrate_ir::ir::{MigrationIr, Op};

/// A charter granting the `app` schema everything the data-security walk needs, plus
/// whatever `require`/`default_scope` text an arm supplies.
fn charter(default_scope: &str, require: &str) -> String {
    format!(
        r#"policy_version = 1
{default_scope}
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
{require}"#
    )
}

fn cfg_for(default_scope: &str, require: &str) -> GuardConfig {
    GuardConfig::from_policy(
        support::effective_policy_from_charter_toml(&charter(default_scope, require)),
        SqlDialect::Postgres,
    )
}

fn ir_with(ops: Vec<Op>) -> MigrationIr {
    MigrationIr {
        ir_version: zero_migrate_ir::ir::CURRENT_IR_VERSION,
        name: "require_rls_scope".into(),
        owner_app: "app_corpus".into(),
        ops,
        flags: Default::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    }
}

fn create_table(schema: Option<&str>, name: &str) -> Op {
    Op::CreateTable {
        name: name.to_string(),
        columns: Vec::new(),
        primary_key: None,
        constraints: Vec::new(),
        indexes: Vec::new(),
        partition_by: None,
        runtime_options: None,
        schema: schema.map(str::to_string),
        existence_guard: None,
    }
}

fn set_rls(schema: Option<&str>, table: &str, enabled: Option<bool>, forced: Option<bool>) -> Op {
    Op::SetRls {
        table: table.to_string(),
        schema: schema.map(str::to_string),
        enabled,
        forced,
    }
}

fn raw(sql: &str) -> Op {
    Op::PgRaw {
        sql: sql.to_string(),
        reason: "require_rls scope probe".to_string(),
    }
}

/// The unprotected create every scope arm is measured against.
fn unprotected_create_ir() -> MigrationIr {
    ir_with(vec![create_table(Some("app"), "users")])
}

#[track_caller]
fn assert_require_rls_refused(cfg: &GuardConfig, ir: &MigrationIr, want_op_index: usize) {
    match check_ir_data_security_policy(cfg, ir) {
        Err(err) => {
            assert_eq!(err.op_index, want_op_index, "op index");
            assert!(
                matches!(
                    err.source,
                    GuardError::DataSecurityPolicy {
                        rule: data_security_rule::REQUIRE_RLS,
                        ..
                    }
                ),
                "expected a REQUIRE_RLS refusal, got {:?}",
                err.source
            );
        }
        Ok(()) => panic!("the sealed require_rls obligation admitted the migration"),
    }
}

#[track_caller]
fn assert_admitted(cfg: &GuardConfig, ir: &MigrationIr) {
    if let Err(err) = check_ir_data_security_policy(cfg, ir) {
        panic!("expected admission, got {err:?}");
    }
}

// -- the four arms: one IR, one base charter, one variable (the rule's scope) ----

const REQUIRE_TABLE_SCOPED: &str = r#"
[[require]]
key = "safety.require_rls"
value = true
scope = { include = ["app.users"] }
"#;

const REQUIRE_SCHEMA_SCOPED: &str = r#"
[[require]]
key = "safety.require_rls"
value = true
scope = { include = ["app"] }
"#;

const REQUIRE_SCOPELESS: &str = r#"
[[require]]
key = "safety.require_rls"
value = true
"#;

const REQUIRE_TOP_SCOPED: &str = r#"
[[require]]
key = "safety.require_rls"
value = true
scope = "all"
"#;

const DEFAULT_SCOPE_APP: &str = r#"
[default_scope]
include = ["app"]
"#;

#[test]
fn table_scoped_require_rls_refuses_unprotected_create() {
    let cfg = cfg_for("", REQUIRE_TABLE_SCOPED);
    assert_require_rls_refused(&cfg, &unprotected_create_ir(), 0);
}

#[test]
fn schema_scoped_require_rls_refuses_unprotected_create() {
    let cfg = cfg_for("", REQUIRE_SCHEMA_SCOPED);
    assert_require_rls_refused(&cfg, &unprotected_create_ir(), 0);
}

#[test]
fn default_scope_inherited_require_rls_refuses_unprotected_create() {
    let cfg = cfg_for(DEFAULT_SCOPE_APP, REQUIRE_SCOPELESS);
    assert_require_rls_refused(&cfg, &unprotected_create_ir(), 0);
}

/// CONTROL - the same IR and the same base charter, differing only in the rule's
/// scope. This arm passes today; the three above must join it.
#[test]
fn top_scoped_require_rls_refuses_unprotected_create() {
    let cfg = cfg_for("", REQUIRE_TOP_SCOPED);
    assert_require_rls_refused(&cfg, &unprotected_create_ir(), 0);
}

// -- the object is built through the composer's identifier fold ------------------

/// `CREATE TABLE "Users"` under a scope of `app.users`: the composer PG-folds both
/// sides, so raw table bytes would miss the covering rule.
#[test]
fn table_scoped_require_rls_folds_the_created_identifier() {
    let cfg = cfg_for("", REQUIRE_TABLE_SCOPED);
    assert_require_rls_refused(&cfg, &ir_with(vec![create_table(Some("app"), "Users")]), 0);
}

// -- a table the obligation does not cover is admitted --------------------------

#[test]
fn table_scoped_require_rls_admits_a_table_outside_its_scope() {
    let cfg = cfg_for("", REQUIRE_TABLE_SCOPED);
    assert_admitted(&cfg, &ir_with(vec![create_table(Some("app"), "audit")]));
}

// -- the RLS toggles resolve at their own op's table -----------------------------

#[test]
fn scoped_require_rls_refuses_disable_at_a_covered_table_only() {
    let cfg = cfg_for("", REQUIRE_TABLE_SCOPED);
    for op in [
        set_rls(Some("app"), "users", Some(false), None),
        set_rls(Some("app"), "users", None, Some(false)),
    ] {
        assert_require_rls_refused(&cfg, &ir_with(vec![op]), 0);
    }
    for op in [
        set_rls(Some("app"), "audit", Some(false), None),
        set_rls(Some("app"), "audit", None, Some(false)),
    ] {
        assert_admitted(&cfg, &ir_with(vec![op]));
    }
}

// -- rename: the FINAL surviving identity decides --------------------------------

#[test]
fn scoped_require_rls_judges_the_renamed_identity() {
    let cfg = cfg_for("", REQUIRE_TABLE_SCOPED);
    // Created outside the scope, renamed INTO it, never RLS-enabled -> refused at the
    // final identity `app.users`.
    assert_require_rls_refused(
        &cfg,
        &ir_with(vec![
            create_table(Some("app"), "audit"),
            Op::RenameTable {
                table: "audit".to_string(),
                to: "users".to_string(),
                schema: Some("app".to_string()),
                existence_guard: None,
            },
        ]),
        1,
    );
    // Created inside the scope, renamed OUT of it -> the surviving identity is
    // uncovered, so nothing is obligated.
    assert_admitted(
        &cfg,
        &ir_with(vec![
            create_table(Some("app"), "users"),
            Op::RenameTable {
                table: "users".to_string(),
                to: "audit".to_string(),
                schema: Some("app".to_string()),
                existence_guard: None,
            },
        ]),
    );
}

// -- an unqualified table under a multi-schema charter fails CLOSED --------------

/// Two owned schemas means no unique pinned schema, so the guard's table key carries
/// an EMPTY schema and no `ObjectName` can be built from it. That table is not
/// provably outside the obligation, so it must be refused.
#[test]
fn unqualified_table_under_a_multi_schema_charter_fails_closed() {
    let charter = r#"policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app", "other"] }

[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app", "other"] }

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"

[[require]]
key = "safety.require_rls"
value = true
scope = { include = ["app"] }
"#;
    let cfg = GuardConfig::from_policy(
        support::effective_policy_from_charter_toml(charter),
        SqlDialect::Postgres,
    );
    assert_require_rls_refused(&cfg, &ir_with(vec![create_table(None, "users")]), 0);
    // Same charter, same missing pin, reached through raw SQL instead.
    assert_require_rls_refused(
        &cfg,
        &ir_with(vec![raw("INSERT INTO ledger (id) VALUES (1)")]),
        0,
    );
}

// -- raw islands: refused where the obligation reaches, admitted where it does not -

#[test]
fn scoped_require_rls_refuses_a_raw_island_it_covers() {
    let cfg = cfg_for("", REQUIRE_SCHEMA_SCOPED);
    assert_require_rls_refused(
        &cfg,
        &ir_with(vec![raw("CREATE TABLE app.raw_users (id int)")]),
        0,
    );
}

#[test]
fn scoped_require_rls_admits_a_raw_island_outside_its_reach() {
    let cfg = cfg_for("", REQUIRE_SCHEMA_SCOPED);
    assert_admitted(
        &cfg,
        &ir_with(vec![raw("INSERT INTO other.ledger (id) VALUES (1)")]),
    );
}

/// An unqualified raw relation pins to the charter's sole owned schema, exactly as a
/// raw create target does, so the obligation is decided at the pinned identity.
#[test]
fn scoped_require_rls_pins_an_unqualified_raw_relation() {
    let cfg = cfg_for("", REQUIRE_TABLE_SCOPED);
    assert_require_rls_refused(
        &cfg,
        &ir_with(vec![raw("INSERT INTO users (id) VALUES (1)")]),
        0,
    );
    assert_admitted(
        &cfg,
        &ir_with(vec![raw("INSERT INTO ledger (id) VALUES (1)")]),
    );
}

#[test]
fn require_rls_admits_every_raw_island_when_no_rule_authors_it() {
    // No `safety.require_rls` rule at all: the obligation reaches nothing, so even an
    // island the guard cannot attribute is admitted.
    let cfg = cfg_for("", "");
    assert_admitted(&cfg, &ir_with(vec![raw("SET LOCAL work_mem = '64MB'")]));
    assert_admitted(
        &cfg,
        &ir_with(vec![raw("CREATE TABLE app.raw_users (id int)")]),
    );
}

// -- scope = "all" behaviour is unchanged ----------------------------------------

#[test]
fn top_scoped_require_rls_behaviour_is_unchanged() {
    let cfg = cfg_for("", REQUIRE_TOP_SCOPED);

    // A create that enables RLS in the same migration is admitted.
    assert_admitted(
        &cfg,
        &ir_with(vec![
            create_table(Some("app"), "users"),
            set_rls(Some("app"), "users", Some(true), None),
        ]),
    );
    // Enabling then disabling is a net-off create -> refused at the disable.
    assert_require_rls_refused(
        &cfg,
        &ir_with(vec![
            create_table(Some("app"), "users"),
            set_rls(Some("app"), "users", Some(true), None),
            set_rls(Some("app"), "users", Some(false), None),
        ]),
        2,
    );
    // A standalone disable / no-force is refused at any table.
    assert_require_rls_refused(
        &cfg,
        &ir_with(vec![set_rls(None, "users", Some(false), None)]),
        0,
    );
    assert_require_rls_refused(
        &cfg,
        &ir_with(vec![set_rls(None, "users", None, Some(false))]),
        0,
    );
    // Every raw island stays refused: a create, a statement naming an unrelated
    // relation, and a statement naming none at all.
    for sql in [
        "CREATE TABLE app.raw_users AS SELECT 1 AS id",
        "INSERT INTO other.ledger (id) VALUES (1)",
        "SET LOCAL work_mem = '64MB'",
    ] {
        assert_require_rls_refused(&cfg, &ir_with(vec![raw(sql)]), 0);
    }
    // A dropped table carries no end-state obligation.
    assert_admitted(
        &cfg,
        &ir_with(vec![
            create_table(Some("app"), "tmp"),
            Op::DropTable {
                table: "tmp".to_string(),
                cascade: None,
                schema: Some("app".to_string()),
                existence_guard: None,
            },
        ]),
    );
}
