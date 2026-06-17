//! Operational analyzer (advisory lint) suite tests (v3 Plan B).
//!
//! Each analyzer has a **fire** test (it emits the right rule on the bad shape)
//! AND a **no-fire** test (it stays silent on the safe shape). A final group
//! asserts the analyzers are **advisory only** — a migration with advisories
//! still passes the guard and is NOT denied (security denials are separate).

use zeroship_migrate::analyze::{analyze, rule, Severity};
use zeroship_migrate::guard::{GuardConfig, SqlGuard};
use zeroship_migrate::{analyze_migration, Advisory};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// All advisories for a SQL string.
fn adv(sql: &str) -> Vec<Advisory> {
    analyze(sql)
}

/// Does any advisory carry `rule`?
fn fires(sql: &str, r: &str) -> bool {
    adv(sql).iter().any(|a| a.rule == r)
}

/// The first advisory with `rule`, if any.
fn first(sql: &str, r: &str) -> Option<Advisory> {
    adv(sql).into_iter().find(|a| a.rule == r)
}

// ---------------------------------------------------------------------------
// GROUP 1 — destructive / backward-incompatible
// ---------------------------------------------------------------------------

#[test]
fn destructive_drop_fires_on_drop_table() {
    let a = first("DROP TABLE orders", rule::DESTRUCTIVE_DROP).expect("DROP TABLE must warn");
    assert_eq!(a.severity, Severity::Warning);
    assert!(a.message.to_lowercase().contains("data loss"));
    assert!(a.suggestion.unwrap().to_lowercase().contains("expand-contract"));
}

#[test]
fn destructive_drop_fires_on_drop_column() {
    let a = first("ALTER TABLE orders DROP COLUMN legacy", rule::DESTRUCTIVE_DROP)
        .expect("DROP COLUMN must warn");
    assert!(a.message.contains("legacy"));
    assert!(a.suggestion.unwrap().to_lowercase().contains("expand-contract"));
}

#[test]
fn destructive_drop_fires_on_drop_constraint() {
    assert!(fires(
        "ALTER TABLE orders DROP CONSTRAINT orders_total_check",
        rule::DESTRUCTIVE_DROP
    ));
}

#[test]
fn destructive_drop_does_not_fire_on_create_table() {
    // A pure additive CREATE TABLE is not destructive.
    assert!(!fires("CREATE TABLE orders(id bigint primary key)", rule::DESTRUCTIVE_DROP));
}

#[test]
fn destructive_drop_does_not_fire_on_drop_index() {
    // Dropping an INDEX is reversible structure, not data loss — no warning.
    assert!(!fires("DROP INDEX idx_orders_name", rule::DESTRUCTIVE_DROP));
}

#[test]
fn backward_incompatible_rename_fires_on_rename_column() {
    let a = first(
        "ALTER TABLE users RENAME COLUMN email TO email_address",
        rule::BACKWARD_INCOMPATIBLE_RENAME,
    )
    .expect("RENAME COLUMN must warn");
    assert_eq!(a.severity, Severity::Warning);
    assert!(a.message.contains("email"));
    assert!(a.suggestion.unwrap().to_lowercase().contains("expand-contract"));
}

#[test]
fn backward_incompatible_rename_fires_on_rename_table() {
    assert!(fires(
        "ALTER TABLE users RENAME TO accounts",
        rule::BACKWARD_INCOMPATIBLE_RENAME
    ));
}

#[test]
fn backward_incompatible_rename_does_not_fire_on_add_column() {
    // ADD COLUMN is not a rename.
    assert!(!fires(
        "ALTER TABLE users ADD COLUMN nickname text",
        rule::BACKWARD_INCOMPATIBLE_RENAME
    ));
}

#[test]
fn lossy_type_change_fires_on_alter_column_type() {
    let a = first(
        "ALTER TABLE users ALTER COLUMN age TYPE smallint",
        rule::LOSSY_TYPE_CHANGE,
    )
    .expect("ALTER COLUMN TYPE must warn");
    assert_eq!(a.severity, Severity::Warning);
    assert!(a.message.contains("age"));
    assert!(a.suggestion.unwrap().to_lowercase().contains("backfill"));
}

#[test]
fn lossy_type_change_does_not_fire_on_set_default() {
    // Changing a column DEFAULT is not a type change.
    assert!(!fires(
        "ALTER TABLE users ALTER COLUMN age SET DEFAULT 0",
        rule::LOSSY_TYPE_CHANGE
    ));
}

// ---------------------------------------------------------------------------
// GROUP 2 — constraint-on-existing-data / lock-heavy
// ---------------------------------------------------------------------------

#[test]
fn add_not_null_no_default_fires() {
    let a = first(
        "ALTER TABLE users ADD COLUMN status text NOT NULL",
        rule::ADD_NOT_NULL_NO_DEFAULT,
    )
    .expect("ADD COLUMN NOT NULL with no default must warn");
    assert_eq!(a.severity, Severity::Warning);
    assert!(a.message.to_lowercase().contains("non-empty"));
    let s = a.suggestion.unwrap().to_lowercase();
    assert!(s.contains("nullable") && s.contains("backfill"));
}

#[test]
fn add_not_null_with_constant_default_does_not_fire() {
    // Constant default is the PG11+ metadata-only fast path — no NOT_NULL_NO_DEFAULT.
    assert!(!fires(
        "ALTER TABLE users ADD COLUMN status text NOT NULL DEFAULT 'active'",
        rule::ADD_NOT_NULL_NO_DEFAULT
    ));
    // And it does NOT trigger a TABLE_REWRITE either (constant default).
    assert!(!fires(
        "ALTER TABLE users ADD COLUMN status text NOT NULL DEFAULT 'active'",
        rule::TABLE_REWRITE
    ));
}

#[test]
fn add_nullable_column_does_not_fire_not_null() {
    assert!(!fires(
        "ALTER TABLE users ADD COLUMN nickname text",
        rule::ADD_NOT_NULL_NO_DEFAULT
    ));
}

#[test]
fn set_not_null_full_scan_fires() {
    let a = first(
        "ALTER TABLE users ALTER COLUMN email SET NOT NULL",
        rule::SET_NOT_NULL_FULL_SCAN,
    )
    .expect("SET NOT NULL must warn");
    assert_eq!(a.severity, Severity::Warning);
    assert!(a.message.contains("email"));
    let s = a.suggestion.unwrap().to_lowercase();
    assert!(s.contains("not valid") && s.contains("validate"));
}

#[test]
fn drop_not_null_does_not_fire_set_not_null() {
    // DROP NOT NULL is the safe direction — no full-scan warning.
    assert!(!fires(
        "ALTER TABLE users ALTER COLUMN email DROP NOT NULL",
        rule::SET_NOT_NULL_FULL_SCAN
    ));
}

#[test]
fn constraint_not_validated_fires_on_plain_fk() {
    let a = first(
        "ALTER TABLE orders ADD CONSTRAINT fk_user FOREIGN KEY (user_id) REFERENCES users(id)",
        rule::CONSTRAINT_NOT_VALIDATED,
    )
    .expect("plain FK add must warn");
    let s = a.suggestion.unwrap().to_lowercase();
    assert!(s.contains("not valid") && s.contains("validate"));
}

#[test]
fn constraint_not_validated_fires_on_plain_check() {
    assert!(fires(
        "ALTER TABLE orders ADD CONSTRAINT chk_total CHECK (total >= 0)",
        rule::CONSTRAINT_NOT_VALIDATED
    ));
}

#[test]
fn constraint_not_validated_does_not_fire_when_not_valid() {
    // The explicit NOT VALID form is the safe path — no warning.
    assert!(!fires(
        "ALTER TABLE orders ADD CONSTRAINT fk_user FOREIGN KEY (user_id) \
         REFERENCES users(id) NOT VALID",
        rule::CONSTRAINT_NOT_VALIDATED
    ));
    assert!(!fires(
        "ALTER TABLE orders ADD CONSTRAINT chk_total CHECK (total >= 0) NOT VALID",
        rule::CONSTRAINT_NOT_VALIDATED
    ));
}

#[test]
fn constraint_not_validated_fires_on_unique_with_concurrently_suggestion() {
    // UNIQUE cannot be NOT VALID; the advisory suggests the CONCURRENTLY index path.
    let a = first(
        "ALTER TABLE users ADD CONSTRAINT uq_email UNIQUE (email)",
        rule::CONSTRAINT_NOT_VALIDATED,
    )
    .expect("UNIQUE add must warn");
    let s = a.suggestion.unwrap().to_lowercase();
    assert!(s.contains("concurrently") && s.contains("using index"));
}

#[test]
fn non_concurrent_index_fires() {
    let a = first("CREATE INDEX idx_users_name ON users(name)", rule::NON_CONCURRENT_INDEX)
        .expect("plain CREATE INDEX must warn");
    assert_eq!(a.severity, Severity::Warning);
    assert!(a.message.to_lowercase().contains("blocks writes"));
    assert!(a.suggestion.unwrap().to_lowercase().contains("concurrently"));
}

#[test]
fn non_concurrent_index_does_not_fire_when_concurrent() {
    assert!(!fires(
        "CREATE INDEX CONCURRENTLY idx_users_name ON users(name)",
        rule::NON_CONCURRENT_INDEX
    ));
}

#[test]
fn table_rewrite_fires_on_volatile_default_add_column() {
    let a = first(
        "ALTER TABLE users ADD COLUMN created_at timestamptz NOT NULL DEFAULT now()",
        rule::TABLE_REWRITE,
    )
    .expect("volatile-default ADD COLUMN must warn rewrite");
    assert!(a.message.to_lowercase().contains("rewrite"));
    assert!(a.message.to_lowercase().contains("access exclusive"));
}

#[test]
fn table_rewrite_fires_on_nullable_volatile_default() {
    // Even a NULLABLE column with a volatile default rewrites the table.
    assert!(fires(
        "ALTER TABLE users ADD COLUMN token uuid DEFAULT gen_random_uuid()",
        rule::TABLE_REWRITE
    ));
}

#[test]
fn table_rewrite_does_not_fire_on_constant_default() {
    assert!(!fires(
        "ALTER TABLE users ADD COLUMN active boolean NOT NULL DEFAULT true",
        rule::TABLE_REWRITE
    ));
}

// ---------------------------------------------------------------------------
// GROUP 3 — perf advisories (Notice)
// ---------------------------------------------------------------------------

#[test]
fn fk_without_index_fires_as_notice() {
    let a = first(
        "ALTER TABLE orders ADD CONSTRAINT fk_user FOREIGN KEY (user_id) REFERENCES users(id)",
        rule::FK_WITHOUT_INDEX,
    )
    .expect("FK with no supporting index must emit a notice");
    assert_eq!(a.severity, Severity::Notice);
    assert!(a.message.contains("user_id"));
    assert!(a.suggestion.unwrap().to_lowercase().contains("index"));
}

#[test]
fn fk_with_supporting_index_in_same_statement_does_not_fire() {
    // FK column also gets a UNIQUE constraint in the same ALTER → supported.
    assert!(!fires(
        "ALTER TABLE orders ADD CONSTRAINT fk_user FOREIGN KEY (user_id) REFERENCES users(id), \
         ADD CONSTRAINT uq_user UNIQUE (user_id)",
        rule::FK_WITHOUT_INDEX
    ));
}

#[test]
fn non_fk_constraint_does_not_fire_fk_without_index() {
    assert!(!fires(
        "ALTER TABLE orders ADD CONSTRAINT chk_total CHECK (total >= 0)",
        rule::FK_WITHOUT_INDEX
    ));
}

// ---------------------------------------------------------------------------
// advisory-only invariant — analyzers NEVER deny or gate
// ---------------------------------------------------------------------------

fn guard_cfg() -> GuardConfig {
    GuardConfig {
        project_schema: "proj_acme".into(),
        extension_allowlist: Vec::new(),
    }
}

#[test]
fn a_migration_with_advisories_still_passes_the_guard() {
    // A lock-heavy, destructive, rename-y batch — every analyzer fires — but the
    // guard does NOT deny it (no security violation). Advisories are NOT denials.
    let sql = "ALTER TABLE \"proj_acme\".\"users\" ADD COLUMN status text NOT NULL; \
               ALTER TABLE \"proj_acme\".\"users\" RENAME COLUMN email TO email_address; \
               CREATE INDEX idx_users_name ON \"proj_acme\".\"users\"(name); \
               DROP TABLE \"proj_acme\".\"legacy\"";
    let report = SqlGuard::new(guard_cfg())
        .check(sql)
        .expect("operational footguns must NOT be denied by the guard");
    // The guard surfaced advisories...
    assert!(!report.advisories.is_empty(), "advisories should be present");
    assert!(report.advisories.iter().any(|a| a.rule == rule::ADD_NOT_NULL_NO_DEFAULT));
    assert!(report
        .advisories
        .iter()
        .any(|a| a.rule == rule::BACKWARD_INCOMPATIBLE_RENAME));
    assert!(report.advisories.iter().any(|a| a.rule == rule::NON_CONCURRENT_INDEX));
    assert!(report.advisories.iter().any(|a| a.rule == rule::DESTRUCTIVE_DROP));
    // ...and it still flagged the DROP as destructive (the gate's signal, not the
    // analyzer's — the analyzer only ENRICHES, it does not drive the gate).
    assert!(report.destructive);
}

#[test]
fn a_security_violation_is_still_denied_regardless_of_advisories() {
    // A cross-tenant read carries no operational advisory but is HARD-DENIED by
    // the guard — proving advisories and security are separate machinery.
    let err = SqlGuard::new(guard_cfg())
        .check("DROP TABLE control.users")
        .expect_err("cross-schema DROP must be denied");
    // It's a denial (security), not an advisory.
    let _ = err;
}

#[test]
fn analyze_runs_independently_of_the_guard() {
    // The analyzer engine is callable with no guard/config — it is pure parse
    // analysis, so it can enrich a generated migration before any guard check.
    let advisories = analyze("DROP TABLE anything");
    assert!(advisories.iter().any(|a| a.rule == rule::DESTRUCTIVE_DROP));
}

// ---------------------------------------------------------------------------
// the analyze_migration seam (for the declarative differ / plan UI)
// ---------------------------------------------------------------------------

#[test]
fn analyze_migration_attaches_advisories_to_a_generated_migration() {
    use zeroship_migrate::{Column, MigrationAuthor, RawSqlAuthor};
    // Author a destructive drop the way the differ / RawSqlAuthor would, then
    // run the analyzer seam over it.
    let drop = RawSqlAuthor::new("proj_acme", "app_acme")
        .wrap(
            "drop_legacy",
            "DROP TABLE \"proj_acme\".\"legacy\"",
            None,
        )
        .unwrap();
    let advisories = analyze_migration(&drop);
    assert!(
        advisories.iter().any(|a| a.rule == rule::DESTRUCTIVE_DROP),
        "the seam must surface the destructive-drop advisory, got: {advisories:?}"
    );
    // The suggestion points at the expand-contract path.
    let a = advisories
        .iter()
        .find(|a| a.rule == rule::DESTRUCTIVE_DROP)
        .unwrap();
    assert!(a.suggestion.as_deref().unwrap().to_lowercase().contains("expand-contract"));
    // sanity: a benign additive migration gets no advisories.
    let add = zeroship_migrate::DeterministicAuthor::new("proj_acme", "app_acme")
        .author(&zeroship_migrate::AuthorRequest::CreateTable {
            name: "orders".into(),
            columns: vec![Column {
                name: "id".into(),
                ty: "bigint".into(),
                nullable: false,
            }],
        })
        .unwrap();
    for m in &add {
        assert!(
            analyze_migration(m).is_empty(),
            "a plain additive CREATE TABLE should carry no advisories, got: {:?}",
            analyze_migration(m)
        );
    }
}

// ---------------------------------------------------------------------------
// robustness — unparseable SQL yields no advisories (no panic)
// ---------------------------------------------------------------------------

#[test]
fn unparseable_sql_yields_no_advisories() {
    assert!(analyze("this is not sql ;;;").is_empty());
}
