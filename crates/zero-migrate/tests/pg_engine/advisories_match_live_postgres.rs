//! The lock/rewrite advisories are checked against PostgreSQL, not against a
//! reading of the analyzer.
//!
//! Every advisory in `zero-migrate-guard` makes a claim about what a statement
//! does to a live database — "rewrites the table under an ACCESS EXCLUSIVE lock",
//! "blocks writes for the build", "fails on a non-empty table". An operator picks
//! a deploy window from those claims, so a wrong one is worse than no advisory at
//! all. Nothing checked them against a database until this file.
//!
//! THE ORACLE IS `pg_relation_filenode`. A rewrite changes it; nothing else does.
//! Lock MODE cannot answer this — every `ALTER TABLE` takes ACCESS EXCLUSIVE
//! briefly, so `pg_locks` reads the same for the harmless and the catastrophic.
//! The advisory's real claim is about rewriting and duration, and the filenode is
//! the fact that separates them.
//!
//! THE PROPERTY IS "IS THE OPERATOR TOLD", NOT "WHICH RULE ID FIRED". When this
//! was first measured by hand, keying on the rule id reported `ALTER COLUMN …
//! TYPE` as a mismatch: it rewrites and raises no `TABLE_REWRITE`. It raises
//! `LOSSY_TYPE_CHANGE`, whose message says in plain words that the statement
//! rewrites the table under an ACCESS EXCLUSIVE lock. One statement, one
//! advisory, both risks named. Asserting on rule ids would have written that
//! mistake into the suite permanently, so this matches the MESSAGE.
//!
//! The negative direction matters as much as the positive: `ADD COLUMN DEFAULT
//! 42` does not rewrite on PG11+, and warning there would be a false alarm on the
//! most common migration there is — the kind that teaches operators to ignore the
//! analyzer. Both directions are asserted.
//!
//! GATE: `ZERO_MIGRATE_TEST_PG_URL`.

use crate::support;

use zero_migrate_guard::analysis::analyze::analyze;

/// Does any advisory tell the operator this statement rewrites the table?
///
/// Rule id OR message, deliberately — see the module note.
fn warns_about_a_rewrite(sql: &str) -> bool {
    analyze(sql).iter().any(|advisory| {
        advisory.rule == "TABLE_REWRITE" || advisory.message.to_lowercase().contains("rewrite")
    })
}

fn rule_ids(sql: &str) -> Vec<&'static str> {
    analyze(sql).iter().map(|advisory| advisory.rule).collect()
}

/// Each test owns its OWN table: these run concurrently against one database,
/// and a shared name means one test drops the table another is mid-measurement
/// on. That failed only under the full suite and passed in isolation.
fn seed_rows(table: &str) -> String {
    format!(
        "DROP TABLE IF EXISTS {table}; \
         CREATE TABLE {table} (id int PRIMARY KEY, v int NOT NULL DEFAULT 1); \
         INSERT INTO {table} SELECT g, g FROM generate_series(1, 200) g"
    )
}

/// `(label, statement, rewrites?)` — the truth column is what PostgreSQL does.
const REWRITE_TABLE: &str = "adv_rewrite";
const REWRITE_CASES: &[(&str, &str, bool)] = &[
    (
        "plain add column",
        "ALTER TABLE adv_rewrite ADD COLUMN a1 int",
        false,
    ),
    (
        "constant default",
        "ALTER TABLE adv_rewrite ADD COLUMN a2 int DEFAULT 42",
        false,
    ),
    (
        "volatile default",
        "ALTER TABLE adv_rewrite ADD COLUMN a3 double precision DEFAULT random()",
        true,
    ),
    (
        "alter column type",
        "ALTER TABLE adv_rewrite ALTER COLUMN v TYPE text",
        true,
    ),
    (
        "set not null",
        "ALTER TABLE adv_rewrite ALTER COLUMN v SET NOT NULL",
        false,
    ),
    (
        "add column unique",
        "ALTER TABLE adv_rewrite ADD COLUMN a4 int UNIQUE",
        false,
    ),
];

fn filenode(client: &mut postgres::Client) -> i64 {
    client
        .query_one(
            &format!("SELECT pg_relation_filenode('{REWRITE_TABLE}'::regclass)::text::int8"),
            &[],
        )
        .expect("read the table's filenode")
        .get(0)
}

#[test]
fn a_rewrite_is_advised_exactly_when_postgresql_performs_one() {
    let Some(url) = support::pg_url() else {
        support::announce_live_db_skip(support::PG_URL_ENV);
        return;
    };
    let mut client = postgres::Client::connect(&url, postgres::NoTls).expect("connect to live PG");

    for (label, sql, expected_rewrite) in REWRITE_CASES {
        client
            .batch_execute(&seed_rows(REWRITE_TABLE))
            .expect("seed the table");
        let before = filenode(&mut client);
        client
            .batch_execute(sql)
            .unwrap_or_else(|e| panic!("{label}: the statement must run to be measured: {e}"));
        let after = filenode(&mut client);

        let rewrote = before != after;
        assert_eq!(
            rewrote,
            *expected_rewrite,
            "{label}: PostgreSQL's own behaviour changed. The filenode {} — this test's \
             expectation is a recorded measurement of `{sql}`, so a change here means the \
             server version behaves differently, not that the analyzer is wrong",
            if rewrote { "changed" } else { "held" }
        );

        assert_eq!(
            warns_about_a_rewrite(sql),
            rewrote,
            "{label}: the analyzer and the database disagree about `{sql}`. \
             PostgreSQL {} the table; the advisories were {:?}. Warning about a rewrite \
             that does not happen is the worse direction: a false alarm on a routine \
             migration teaches operators to ignore every advisory after it",
            if rewrote {
                "REWROTE"
            } else {
                "did NOT rewrite"
            },
            rule_ids(sql)
        );
    }

    client
        .batch_execute(&format!("DROP TABLE IF EXISTS {REWRITE_TABLE}"))
        .ok();
}

#[test]
fn not_valid_is_advised_exactly_when_it_validates() {
    const TBL: &str = "adv_notvalid";
    let Some(url) = support::pg_url() else {
        support::announce_live_db_skip(support::PG_URL_ENV);
        return;
    };
    let mut client = postgres::Client::connect(&url, postgres::NoTls).expect("connect to live PG");

    // One row violating the constraint, so validation is observable: the plain
    // form fails on it, the NOT VALID form does not look.
    client
        .batch_execute(&seed_rows(TBL))
        .expect("seed the table");
    client
        .batch_execute("UPDATE adv_notvalid SET v = -1 WHERE id = 1")
        .expect("dirty one row");

    let plain = "ALTER TABLE adv_notvalid ADD CONSTRAINT ck_plain CHECK (v > 0)";
    let not_valid = "ALTER TABLE adv_notvalid ADD CONSTRAINT ck_lazy CHECK (v > 0) NOT VALID";

    assert!(
        client.batch_execute(plain).is_err(),
        "the plain CHECK must validate every row and fail on the dirty one, or this test \
         is not measuring validation"
    );
    assert!(
        rule_ids(plain).contains(&"CONSTRAINT_NOT_VALIDATED"),
        "a validating constraint must be advised: {:?}",
        rule_ids(plain)
    );

    assert!(
        client.batch_execute(not_valid).is_ok(),
        "NOT VALID must skip the existing rows and succeed against the same dirty data"
    );
    assert!(
        rule_ids(not_valid).is_empty(),
        "NOT VALID skips the validation the advisory warns about, so there is nothing to \
         warn about. Matching on ADD CONSTRAINT and warning on both forms is the failure \
         mode that trains operators to skip the output: {:?}",
        rule_ids(not_valid)
    );

    client
        .batch_execute("DROP TABLE IF EXISTS adv_notvalid")
        .ok();
}

#[test]
fn a_plain_create_index_takes_the_lock_the_advisory_names() {
    const TBL: &str = "adv_index";
    let Some(url) = support::pg_url() else {
        support::announce_live_db_skip(support::PG_URL_ENV);
        return;
    };
    let mut client = postgres::Client::connect(&url, postgres::NoTls).expect("connect to live PG");
    client
        .batch_execute(&seed_rows(TBL))
        .expect("seed the table");

    let plain = "CREATE INDEX adv_index_v_ix ON adv_index (v)";
    let mut tx = client.transaction().expect("open a transaction");
    tx.batch_execute(plain).expect("build the index");
    let modes: Vec<String> = tx
        .query(
            "SELECT mode FROM pg_locks WHERE relation = 'adv_index'::regclass \
             AND pid = pg_backend_pid()",
            &[],
        )
        .expect("read our own locks")
        .iter()
        .map(|row| row.get::<_, String>(0))
        .collect();

    // ShareLock is precisely "blocks writes": INSERT/UPDATE/DELETE take
    // RowExclusiveLock, which conflicts with it. Readers are unaffected.
    assert!(
        modes.iter().any(|m| m == "ShareLock"),
        "a plain CREATE INDEX must hold ShareLock, which is what NON_CONCURRENT_INDEX's \
         \"blocks writes for the build\" means. Held: {modes:?}"
    );
    assert!(
        rule_ids(plain).contains(&"NON_CONCURRENT_INDEX"),
        "a write-blocking index build must be advised: {:?}",
        rule_ids(plain)
    );
    tx.rollback().expect("roll the index back");

    // CONCURRENTLY does not block writes and must stay silent.
    assert!(
        rule_ids("CREATE INDEX CONCURRENTLY adv_index_v_ix ON adv_index (v)").is_empty(),
        "CONCURRENTLY is the fix the advisory recommends; advising against it would be \
         recommending a change and then warning about it"
    );

    client.batch_execute("DROP TABLE IF EXISTS adv_index").ok();
}
