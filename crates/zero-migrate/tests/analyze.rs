//! Operational analyzer (advisory lint) suite tests (v3 Plan B).
//!
//! Each analyzer has a **fire** test (it emits the right rule on the bad shape)
//! AND a **no-fire** test (it stays silent on the safe shape). A final group
//! asserts the analyzers are **advisory only** — a migration with advisories
//! still passes the guard and is NOT denied (security denials are separate).

use zero_migrate::analyze::{analyze, rule, Severity};
use zero_migrate::guard::{GuardConfig, SqlGuard};
use zero_migrate::{analyze_migration, Advisory};

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
    assert!(a
        .suggestion
        .unwrap()
        .to_lowercase()
        .contains("expand-contract"));
}

#[test]
fn destructive_drop_fires_on_drop_column() {
    let a = first(
        "ALTER TABLE orders DROP COLUMN legacy",
        rule::DESTRUCTIVE_DROP,
    )
    .expect("DROP COLUMN must warn");
    assert!(a.message.contains("legacy"));
    assert!(a
        .suggestion
        .unwrap()
        .to_lowercase()
        .contains("expand-contract"));
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
    assert!(!fires(
        "CREATE TABLE orders(id bigint primary key)",
        rule::DESTRUCTIVE_DROP
    ));
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
    assert!(a
        .suggestion
        .unwrap()
        .to_lowercase()
        .contains("expand-contract"));
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
    let a = first(
        "CREATE INDEX idx_users_name ON users(name)",
        rule::NON_CONCURRENT_INDEX,
    )
    .expect("plain CREATE INDEX must warn");
    assert_eq!(a.severity, Severity::Warning);
    assert!(a.message.to_lowercase().contains("blocks writes"));
    assert!(a
        .suggestion
        .unwrap()
        .to_lowercase()
        .contains("concurrently"));
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
// GROUP 1 (review) — inline PRIMARY KEY / UNIQUE on ADD COLUMN, and the
// ADD CONSTRAINT PRIMARY KEY form. Previously these emitted ZERO advisories.
// ---------------------------------------------------------------------------

#[test]
fn add_column_inline_primary_key_fires_not_null_and_lock() {
    // PRIMARY KEY implies NOT NULL with no default → fails on a non-empty table,
    // AND builds a unique index under ACCESS EXCLUSIVE → a lock advisory.
    let sql = "ALTER TABLE t ADD COLUMN x int PRIMARY KEY";
    assert!(
        fires(sql, rule::ADD_NOT_NULL_NO_DEFAULT),
        "inline PRIMARY KEY implies NOT NULL with no default"
    );
    assert!(
        fires(sql, rule::CONSTRAINT_NOT_VALIDATED),
        "inline PRIMARY KEY builds a unique index under ACCESS EXCLUSIVE"
    );
}

#[test]
fn add_column_inline_unique_not_null_fires_lock() {
    // UNIQUE builds a validating index under ACCESS EXCLUSIVE → a lock advisory.
    // (NOT NULL with no default is also a footgun, but UNIQUE is the new arm.)
    let sql = "ALTER TABLE t ADD COLUMN x int UNIQUE NOT NULL";
    assert!(
        fires(sql, rule::CONSTRAINT_NOT_VALIDATED),
        "inline UNIQUE builds a validating index under ACCESS EXCLUSIVE"
    );
    assert!(
        fires(sql, rule::ADD_NOT_NULL_NO_DEFAULT),
        "explicit NOT NULL with no default still fires"
    );
}

#[test]
fn add_column_bigserial_primary_key_fires() {
    // bigserial gets an implicit sequence default, so it is NOT a NOT_NULL_NO_DEFAULT,
    // but PRIMARY KEY still builds a unique index under ACCESS EXCLUSIVE.
    let sql = "ALTER TABLE t ADD COLUMN id bigserial PRIMARY KEY";
    assert!(
        fires(sql, rule::CONSTRAINT_NOT_VALIDATED),
        "bigserial PRIMARY KEY still builds a unique index under lock"
    );
}

#[test]
fn add_column_plain_nullable_int_stays_clean() {
    // A plain nullable column with no constraints emits nothing.
    let sql = "ALTER TABLE t ADD COLUMN x int";
    assert!(
        adv(sql).is_empty(),
        "plain nullable ADD COLUMN must be clean, got: {:?}",
        adv(sql)
    );
}

#[test]
fn add_constraint_primary_key_fires() {
    // ALTER TABLE … ADD CONSTRAINT pk PRIMARY KEY (id) builds a validating unique
    // index under ACCESS EXCLUSIVE — previously fell through with zero advisories.
    let a = first(
        "ALTER TABLE t ADD CONSTRAINT pk PRIMARY KEY (id)",
        rule::CONSTRAINT_NOT_VALIDATED,
    )
    .expect("ADD CONSTRAINT PRIMARY KEY must warn");
    let s = a.suggestion.unwrap().to_lowercase();
    assert!(s.contains("concurrently") && s.contains("using index"));
}

#[test]
fn add_constraint_bare_primary_key_fires() {
    assert!(fires(
        "ALTER TABLE t ADD PRIMARY KEY (id)",
        rule::CONSTRAINT_NOT_VALIDATED
    ));
}

// ---------------------------------------------------------------------------
// GROUP 2 (review) — new statement arms: TRUNCATE, STORED generated columns,
// and the lock-heavy maintenance ops (CLUSTER / VACUUM FULL / REINDEX).
// ---------------------------------------------------------------------------

#[test]
fn truncate_fires_as_warning_data_loss() {
    let a = first("TRUNCATE orders", rule::TRUNCATE_DATA_LOSS).expect("TRUNCATE must warn");
    assert_eq!(a.severity, Severity::Warning);
    assert!(
        a.message.to_lowercase().contains("data loss")
            || a.message.to_lowercase().contains("all rows")
    );
}

#[test]
fn add_column_generated_stored_fires_table_rewrite() {
    let a = first(
        "ALTER TABLE t ADD COLUMN g int GENERATED ALWAYS AS (x+1) STORED",
        rule::TABLE_REWRITE,
    )
    .expect("STORED generated ADD COLUMN must warn rewrite");
    assert!(a.message.to_lowercase().contains("rewrite"));
}

#[test]
fn cluster_fires_lock_heavy() {
    assert!(fires(
        "CLUSTER orders USING idx_orders",
        rule::LOCK_HEAVY_MAINTENANCE
    ));
}

#[test]
fn vacuum_full_fires_lock_heavy() {
    assert!(fires("VACUUM FULL orders", rule::LOCK_HEAVY_MAINTENANCE));
}

#[test]
fn plain_vacuum_does_not_fire() {
    assert!(!fires("VACUUM orders", rule::LOCK_HEAVY_MAINTENANCE));
}

#[test]
fn reindex_non_concurrent_fires_with_concurrently_suggestion() {
    let a = first("REINDEX TABLE orders", rule::LOCK_HEAVY_MAINTENANCE)
        .expect("non-concurrent REINDEX must warn");
    assert!(a
        .suggestion
        .unwrap()
        .to_lowercase()
        .contains("concurrently"));
}

#[test]
fn reindex_concurrently_does_not_fire() {
    assert!(!fires(
        "REINDEX TABLE CONCURRENTLY orders",
        rule::LOCK_HEAVY_MAINTENANCE
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
// GROUP 3 (review) — #6 USING INDEX false-positive + #7 CONCURRENTLY message.
// ---------------------------------------------------------------------------

#[test]
fn add_constraint_unique_using_index_is_clean() {
    // Attaching a pre-built index is the SAFE path the advisory itself recommends
    // (metadata-only) — it must NOT fire CONSTRAINT_NOT_VALIDATED.
    assert!(
        !fires(
            "ALTER TABLE t ADD CONSTRAINT uq UNIQUE USING INDEX uq_idx",
            rule::CONSTRAINT_NOT_VALIDATED
        ),
        "USING INDEX (pre-built index) is the safe metadata-only path"
    );
}

#[test]
fn add_constraint_plain_unique_still_fires() {
    // The plain UNIQUE (no USING INDEX) still fires — guards against over-suppression.
    assert!(fires(
        "ALTER TABLE t ADD CONSTRAINT uq UNIQUE (email)",
        rule::CONSTRAINT_NOT_VALIDATED
    ));
}

#[test]
fn non_concurrent_index_suggestion_notes_own_nontransactional_migration() {
    // CONCURRENTLY can't run inside a txn block, so it must be its OWN migration —
    // the suggestion must say so (it is unactionable inside a multi-statement txn).
    let a = first(
        "CREATE INDEX idx_users_name ON users(name)",
        rule::NON_CONCURRENT_INDEX,
    )
    .expect("plain CREATE INDEX must warn");
    let s = a.suggestion.unwrap().to_lowercase();
    assert!(s.contains("concurrently"));
    assert!(
        s.contains("own") && s.contains("transaction"),
        "suggestion must note authoring CONCURRENTLY as its own non-transactional migration, got: {s}"
    );
}

// ---------------------------------------------------------------------------
// advisory-only invariant — analyzers NEVER deny or gate
// ---------------------------------------------------------------------------

fn guard_cfg() -> GuardConfig {
    GuardConfig::confined("proj_acme")
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
    assert!(
        !report.advisories.is_empty(),
        "advisories should be present"
    );
    assert!(report
        .advisories
        .iter()
        .any(|a| a.rule == rule::ADD_NOT_NULL_NO_DEFAULT));
    assert!(report
        .advisories
        .iter()
        .any(|a| a.rule == rule::BACKWARD_INCOMPATIBLE_RENAME));
    assert!(report
        .advisories
        .iter()
        .any(|a| a.rule == rule::NON_CONCURRENT_INDEX));
    assert!(report
        .advisories
        .iter()
        .any(|a| a.rule == rule::DESTRUCTIVE_DROP));
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
    use zero_migrate::{Column, MigrationAuthor, RawSqlAuthor};
    // Author a destructive drop the way the differ / RawSqlAuthor would, then
    // run the analyzer seam over it.
    let drop = RawSqlAuthor::new("proj_acme", "app_acme")
        .wrap("drop_legacy", "DROP TABLE \"proj_acme\".\"legacy\"", None)
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
    assert!(a
        .suggestion
        .as_deref()
        .unwrap()
        .to_lowercase()
        .contains("expand-contract"));
    // sanity: a benign additive migration gets no advisories.
    let add = zero_migrate::DeterministicAuthor::new("proj_acme", "app_acme")
        .author(&zero_migrate::AuthorRequest::CreateTable {
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
