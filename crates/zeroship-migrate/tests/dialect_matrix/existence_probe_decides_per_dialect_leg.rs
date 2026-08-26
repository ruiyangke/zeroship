//! The existence-guard decider's PER-DIALECT LEGS.
//!
//! These 39 cases were `existence_probe.rs`'s own `#[cfg(test)] mod tests` until the
//! decider moved down to `zeroship-migrate-backend`. They could not go with it: every
//! one of them drives `decide` against a REAL vendor — PostgreSQL's raw
//! `information_schema` compare, SQLite's affinity fold, MySQL's constraint-first
//! catalog resolution — and the contract crate sits BELOW all three, so it cannot
//! name one. They live here, in the crate that links all three, and they reach the
//! decider through the engine's resolving `render::existence_probe::decide`.
//!
//! Nothing about the cases changed. They called ZERO private helpers of that module
//! (measured: all 39 go through the public `decide`), which is why the move cost no
//! visibility — the alternative, publishing `decide_table`, `decide_index`,
//! `decide_constraint` and nine more just to keep their tests, is the shape this
//! avoided.

use std::collections::BTreeMap;
use zeroship_migrate::model::probe::{ExpectColumn, GuardDir, GuardProbe};
use zeroship_migrate::model::snapshot::{
    ColumnSnapshot, ConstraintSnapshot, IndexElementSnapshot, IndexSnapshot, SchemaSnapshot,
    TableSnapshot,
};
use zeroship_migrate::render::existence_probe::{decide, GuardVerdict};
use zeroship_migrate_mysql::DIALECT as MYSQL;
use zeroship_migrate_postgres::DIALECT as POSTGRES;
use zeroship_migrate_sqlite::DIALECT as SQLITE;

/// `decide` on the PG leg (raw `information_schema` compare).
fn decide_pg(probe: &GuardProbe, live: &SchemaSnapshot) -> GuardVerdict {
    decide(zeroship_migrate::shipping_vendors(), probe, live, &POSTGRES)
}

/// `decide` on the SQLite leg (affinity-fold compare — F1).
fn decide_sqlite(probe: &GuardProbe, live: &SchemaSnapshot) -> GuardVerdict {
    decide(zeroship_migrate::shipping_vendors(), probe, live, &SQLITE)
}

/// `decide` on the MySQL leg (constraint-first catalog resolution).
fn decide_mysql(probe: &GuardProbe, live: &SchemaSnapshot) -> GuardVerdict {
    decide(zeroship_migrate::shipping_vendors(), probe, live, &MYSQL)
}

/// PostgreSQL's own catalog normalization, taken from PostgreSQL's own vendor
/// literal. The helper used to reach it through the engine's registry, which was
/// correct while it lived inside the engine and is not available from here — and
/// would be the wrong shape anyway: a caller that already knows which vendor it
/// wants asks that vendor, it does not ask which backend handles a dialect.
fn normalize_pg_constraint_definition(definition: &str) -> String {
    zeroship_migrate_postgres::VENDOR
        .existence_probe
        .normalize_constraint_definition(definition)
}

/// PostgreSQL's DECLARED byte-counted identifier cap, read off its descriptor rather
/// than restating `63`. The engine folds the same descriptor field into a
/// `pub(crate)` budget that an integration test cannot see; reading the source both
/// of them read keeps the two from drifting without publishing an engine internal.
fn pg_identifier_max_bytes() -> usize {
    match zeroship_migrate_postgres::VENDOR.descriptor.limits.identifier {
        zeroship_migrate_ir::backend::IdentifierLimit::Bytes(n) => n,
        other => panic!("PostgreSQL declares a BYTE identifier cap, not {other:?}"),
    }
}

fn col(name: &str, dtype: &str, nullable: bool) -> ColumnSnapshot {
    ColumnSnapshot {
        name: name.to_string(),
        data_type: dtype.to_string(),
        nullable,
        default: None,
        ddl_type_override: None,
        inline_checks: Vec::new(),
        generated: None,
        generated_kind: None,
        identity: None,
        rowid_alias: false,
        value_format: None,
        catalog_uuid_format_check: false,
        id_default: None,
        expression_default: None,
        case_sensitive: None,
        unbounded_text: false,
        type_def: None,
        authored_type: false,
        collation: None,
        text_storage: None,
        vendor: Default::default(),
        encryption_sentinel: None,
        comment_sentinel: None,
        comment: None,
    }
}

fn ec(name: &str, dtype: &str, nullable: bool) -> ExpectColumn {
    ExpectColumn {
        name: name.to_string(),
        data_type: dtype.to_string(),
        nullable,
    }
}

fn snapshot_with(table: &str, t: TableSnapshot) -> SchemaSnapshot {
    let mut tables = BTreeMap::new();
    tables.insert(table.to_string(), t);
    SchemaSnapshot {
        tables,
        ..Default::default()
    }
}

fn empty_table() -> TableSnapshot {
    TableSnapshot {
        columns: Vec::new(),
        indexes: Vec::new(),
        constraints: Vec::new(),
        runtime_options: Default::default(),
        attributes: Default::default(),
        partition_by: None,
        comment: None,
        stored_create_sql: None,
    }
}

// -- Column ifNotExists ------------------------------------------------

#[test]
fn column_ifnotexists_absent_runs_bare() {
    let probe = GuardProbe::Column {
        schema: "app".into(),
        table: "users".into(),
        column: "email".into(),
        direction: GuardDir::IfNotExists,
        expect: Some(("text".into(), true)),
    };
    let live = SchemaSnapshot::default();
    assert_eq!(decide_pg(&probe, &live), GuardVerdict::RunBare);
}

#[test]
fn column_ifnotexists_present_matching_is_noop() {
    let probe = GuardProbe::Column {
        schema: "app".into(),
        table: "users".into(),
        column: "email".into(),
        direction: GuardDir::IfNotExists,
        expect: Some(("text".into(), true)),
    };
    let mut t = empty_table();
    t.columns.push(col("email", "text", true));
    let live = snapshot_with("users", t);
    assert_eq!(decide_pg(&probe, &live), GuardVerdict::SatisfiedNoop);
}

#[test]
fn column_ifnotexists_present_divergent_type_fails() {
    let probe = GuardProbe::Column {
        schema: "app".into(),
        table: "users".into(),
        column: "email".into(),
        direction: GuardDir::IfNotExists,
        expect: Some(("text".into(), true)),
    };
    let mut t = empty_table();
    t.columns.push(col("email", "integer", true));
    let live = snapshot_with("users", t);
    match decide_pg(&probe, &live) {
        GuardVerdict::FailDrift(d) => assert_eq!(d.field, "data_type"),
        v => panic!("expected FailDrift(data_type), got {v:?}"),
    }
}

#[test]
fn column_ifnotexists_present_divergent_nullability_fails() {
    let probe = GuardProbe::Column {
        schema: "app".into(),
        table: "users".into(),
        column: "email".into(),
        direction: GuardDir::IfNotExists,
        expect: Some(("text".into(), true)),
    };
    let mut t = empty_table();
    t.columns.push(col("email", "text", false));
    let live = snapshot_with("users", t);
    match decide_pg(&probe, &live) {
        GuardVerdict::FailDrift(d) => assert_eq!(d.field, "nullable"),
        v => panic!("expected FailDrift(nullable), got {v:?}"),
    }
}

// -- F1: SQLite within-text-affinity facet change = noop (differ-consistent) --

#[test]
fn column_ifnotexists_sqlite_within_text_affinity_facet_change_is_noop() {
    // **F1** — on SQLite the live column reads back as the `text` affinity; the
    // guard declares a column whose snapshot is `timestamp with time zone` (a
    // `date` facet) which folds to the same `text` affinity. The within-text-
    // affinity facet blind spot is a documented SQLite divergence the DIFFER also
    // accepts (it compares only the SQLite backend canonicalizer). The guard matches: an
    // affinity-match is a SatisfiedNoop (idempotent re-run), NOT a fail-closed and
    // NOT a false `timestamp with time zone != text` drift. (On SQLite a `ref`/date
    // column is physically a plain `text` column anyway — no provable divergence.)
    let probe = GuardProbe::Column {
        schema: "app".into(),
        table: "users".into(),
        column: "happened".into(),
        direction: GuardDir::IfNotExists,
        expect: Some(("timestamp with time zone".into(), true)),
    };
    let mut t = empty_table();
    // live column → introspected affinity `text`.
    t.columns.push(col("happened", "text", true));
    assert_eq!(
        decide_sqlite(&probe, &snapshot_with("users", t)),
        GuardVerdict::SatisfiedNoop,
        "a within-text-affinity facet on SQLite is an idempotent no-op (differ-consistent)"
    );
}

#[test]
fn column_ifnotexists_no_facet_text_match_is_noop() {
    // A plain `text` == `text` match is a legitimate SatisfiedNoop.
    let probe = GuardProbe::Column {
        schema: "app".into(),
        table: "users".into(),
        column: "email".into(),
        direction: GuardDir::IfNotExists,
        expect: Some(("text".into(), true)),
    };
    let mut t = empty_table();
    t.columns.push(col("email", "text", true));
    assert_eq!(
        decide_pg(&probe, &snapshot_with("users", t)),
        GuardVerdict::SatisfiedNoop
    );
}

#[test]
fn table_ifnotexists_sqlite_text_affinity_reruns_idempotent_noop() {
    // The createTable Table leg is presence + affinity only. A guarded
    // createTable re-run over a table THIS engine made: the declared `timestamp
    // with time zone` (date) / `text` columns fold to the SQLite `text` affinity
    // and MATCH the live `text` affinity → SatisfiedNoop (idempotent re-run), NOT
    // a false `timestamp with time zone != text` drift and NOT a fail-closed.
    let probe = GuardProbe::Table {
        schema: "app".into(),
        table: "events".into(),
        direction: GuardDir::IfNotExists,
        expect_columns: vec![
            // a timestamp snapshot column (PG spelling) over a live SQLite `text`.
            ec("at", "timestamp with time zone", true),
            // a string column (snapshot `text`) over a live `text`.
            ec("name", "text", true),
        ],
    };
    let mut t = empty_table();
    t.columns.push(col("at", "text", true));
    t.columns.push(col("name", "text", true));
    assert_eq!(
        decide_sqlite(&probe, &snapshot_with("events", t)),
        GuardVerdict::SatisfiedNoop,
        "a SQLite createTable re-run over text-affinity columns must be idempotent"
    );
}

// -- Column ifExists (dropColumn) --------------------------------------

#[test]
fn column_ifexists_present_runs_absent_noops() {
    let probe = GuardProbe::Column {
        schema: "app".into(),
        table: "users".into(),
        column: "legacy".into(),
        direction: GuardDir::IfExists,
        expect: None,
    };
    let mut t = empty_table();
    t.columns.push(col("legacy", "text", true));
    assert_eq!(
        decide_pg(&probe, &snapshot_with("users", t)),
        GuardVerdict::RunBare
    );
    assert_eq!(
        decide_pg(&probe, &SchemaSnapshot::default()),
        GuardVerdict::SatisfiedNoop
    );
}

// -- Table ifNotExists -------------------------------------------------

#[test]
fn table_ifnotexists_present_extra_live_column_fails() {
    let probe = GuardProbe::Table {
        schema: "app".into(),
        table: "users".into(),
        direction: GuardDir::IfNotExists,
        expect_columns: vec![ec("id", "integer", false)],
    };
    let mut t = empty_table();
    t.columns.push(col("id", "integer", false));
    t.columns.push(col("sneaky", "text", true)); // extra live column
    match decide_pg(&probe, &snapshot_with("users", t)) {
        GuardVerdict::FailDrift(d) => assert_eq!(d.field, "columns"),
        v => panic!("expected FailDrift(columns) for extra live column, got {v:?}"),
    }
}

#[test]
fn table_ifnotexists_present_matching_is_noop() {
    let probe = GuardProbe::Table {
        schema: "app".into(),
        table: "users".into(),
        direction: GuardDir::IfNotExists,
        expect_columns: vec![ec("id", "integer", false)],
    };
    let mut t = empty_table();
    t.columns.push(col("id", "integer", false));
    assert_eq!(
        decide_pg(&probe, &snapshot_with("users", t)),
        GuardVerdict::SatisfiedNoop
    );
}

// -- Index ifNotExists -------------------------------------------------

#[test]
fn index_ifnotexists_present_unique_flip_fails() {
    let probe = GuardProbe::Index {
        schema: "app".into(),
        table: "users".into(),
        name: "users_email_idx".into(),
        direction: GuardDir::IfNotExists,
        expect: Some((true, vec!["email".into()])),
        ownership_only: false,
    };
    let mut t = empty_table();
    t.indexes.push(IndexSnapshot::btree(
        "users_email_idx".to_string(),
        false,
        vec!["email".to_string()],
    ));
    match decide_pg(&probe, &snapshot_with("users", t)) {
        GuardVerdict::FailDrift(d) => assert_eq!(d.field, "unique"),
        v => panic!("expected FailDrift(unique), got {v:?}"),
    }
}

#[test]
fn index_ifnotexists_present_expression_index_fails_closed() {
    let probe = GuardProbe::Index {
        schema: "app".into(),
        table: "users".into(),
        name: "users_lower_idx".into(),
        direction: GuardDir::IfNotExists,
        expect: Some((false, vec!["email".into()])),
        ownership_only: false,
    };
    let mut t = empty_table();
    let mut idx = IndexSnapshot::btree("users_lower_idx".to_string(), false, Vec::new());
    idx.elements = vec![IndexElementSnapshot::expr("lower(email)")];
    t.indexes.push(idx);
    match decide_pg(&probe, &snapshot_with("users", t)) {
        GuardVerdict::FailDrift(d) => assert_eq!(d.field, "elements"),
        v => panic!("expected FailDrift(elements) for partial/expression index, got {v:?}"),
    }
}

// -- Index ownership-only (the UNGUARDED create) -----------------------

/// The ownership-only probe an unguarded `createIndex` carries. `expect` is
/// always `None`: there is no declared shape to verify on this path.
fn ownership_probe(table: &str, name: &str) -> GuardProbe {
    GuardProbe::Index {
        schema: "app".into(),
        table: table.into(),
        name: name.into(),
        direction: GuardDir::IfNotExists,
        expect: None,
        ownership_only: true,
    }
}

fn table_owning(index: &str) -> TableSnapshot {
    let mut t = empty_table();
    t.indexes.push(IndexSnapshot::btree(
        index.to_string(),
        false,
        vec!["email".to_string()],
    ));
    t
}

#[test]
fn index_ownership_only_fails_closed_when_another_table_owns_the_name() {
    let mut live = snapshot_with("users", table_owning("idx_shared"));
    live.tables.insert("orders".to_string(), empty_table());
    match decide_pg(&ownership_probe("orders", "idx_shared"), &live) {
        GuardVerdict::FailDrift(d) => {
            assert_eq!(d.field, "table");
            assert_eq!(d.expected, "orders");
            assert!(
                d.actual.starts_with("users"),
                "names the owner: {}",
                d.actual
            );
        }
        v => panic!("expected FailDrift(table) naming the owner, got {v:?}"),
    }
}

#[test]
fn index_ownership_only_runs_bare_when_the_probe_table_already_owns_the_name() {
    // The same-table re-run must stay the `IF NOT EXISTS` no-op that
    // non-transactional crash recovery replays, never a SatisfiedNoop, which
    // would journal the version without the statement ever running.
    let live = snapshot_with("users", table_owning("idx_shared"));
    assert_eq!(
        decide_pg(&ownership_probe("users", "idx_shared"), &live),
        GuardVerdict::RunBare
    );
}

#[test]
fn index_ownership_only_runs_bare_on_a_free_name() {
    let live = snapshot_with("users", table_owning("idx_other"));
    assert_eq!(
        decide_pg(&ownership_probe("users", "idx_shared"), &live),
        GuardVerdict::RunBare
    );
}

#[test]
fn index_ownership_only_ignores_a_foreign_owner_where_names_are_per_table() {
    // MySQL scopes index names per table, so the same name elsewhere is an
    // unrelated object and must not block the create.
    //
    // Pins the DECIDER's per-table ownership contract. The MySQL apply tests
    // cover a free and same-table index name, but no end-to-end arm goes red if
    // `Capability::SchemaWideIndexNames` is flipped for MySQL.
    let mut live = snapshot_with("users", table_owning("idx_shared"));
    live.tables.insert("orders".to_string(), empty_table());
    assert_eq!(
        decide(
            zeroship_migrate::shipping_vendors(),
            &ownership_probe("orders", "idx_shared"),
            &live,
            &MYSQL
        ),
        GuardVerdict::RunBare
    );
}

#[test]
fn index_ownership_only_does_not_refuse_an_over_long_name() {
    // The truncation backstop refuses EVERY over-long `IfNotExists` name. An
    // unguarded create carries no author request to be refused on a name
    // PostgreSQL accepts today, so the ownership path returns before it.
    // Reads PostgreSQL's DECLARED cap off its descriptor rather than restating
    // `63`. A test that hardcodes the number still passes against a stale value
    // if the declared limit ever moves, which is the drift decision 3 closed.
    let long = "i".repeat(pg_identifier_max_bytes() + 8);
    let live = snapshot_with("users", empty_table());
    assert_eq!(
        decide_pg(&ownership_probe("users", &long), &live),
        GuardVerdict::RunBare
    );
}

// -- Constraint ifNotExists ------------------------------

fn constraint(name: &str, kind: &str, definition: &str) -> ConstraintSnapshot {
    ConstraintSnapshot {
        name: name.to_string(),
        kind: kind.to_string(),
        definition: definition.to_string(),
        comment: None,
        cascade_columns: None,
    }
}

fn constraint_probe(name: &str, direction: GuardDir, expect_kind: Option<&str>) -> GuardProbe {
    GuardProbe::Constraint {
        schema: "app".into(),
        table: "users".into(),
        name: name.into(),
        direction,
        expect_kind: expect_kind.map(str::to_owned),
        expect_definition: None,
    }
}

fn mysql_primary_table() -> TableSnapshot {
    let mut table = empty_table();
    table
        .constraints
        .push(constraint("users_pkey", "PRIMARY KEY", "PRIMARY KEY (id)"));
    table.indexes.push(IndexSnapshot::btree(
        "users_pkey".to_string(),
        true,
        vec!["id".to_string()],
    ));
    table
}

#[test]
fn constraint_mysql_ifexists_unique_index_runs_bare() {
    let probe = constraint_probe("users_email_key", GuardDir::IfExists, None);
    let mut table = empty_table();
    table.indexes.push(IndexSnapshot::btree(
        "users_email_key".to_string(),
        true,
        vec!["email".to_string()],
    ));
    assert_eq!(
        decide_mysql(&probe, &snapshot_with("users", table)),
        GuardVerdict::RunBare
    );
}

#[test]
fn constraint_mysql_ifexists_unresolved_refuses() {
    let probe = constraint_probe("users_age_check", GuardDir::IfExists, None);
    match decide_mysql(&probe, &snapshot_with("users", empty_table())) {
        GuardVerdict::FailDrift(divergence) => {
            assert_eq!(divergence.object, "constraint users_age_check");
            assert_eq!(divergence.field, "presence");
            assert_eq!(divergence.expected, "<absent>");
            assert_eq!(
                divergence.actual,
                "<unknown: the MySQL snapshot's constraint scope excludes arbitrary CHECK identities, so not found does not prove absent>"
            );
        }
        verdict => panic!("expected FailDrift(presence), got {verdict:?}"),
    }
}

#[test]
fn constraint_mysql_ifexists_primary_constraint_runs_bare() {
    let probe = constraint_probe("users_pkey", GuardDir::IfExists, None);
    assert_eq!(
        decide_mysql(&probe, &snapshot_with("users", mysql_primary_table())),
        GuardVerdict::RunBare
    );
}

#[test]
fn constraint_mysql_ifexists_foreign_key_runs_bare() {
    let probe = constraint_probe("users_account_fkey", GuardDir::IfExists, None);
    let mut table = empty_table();
    table.constraints.push(constraint(
        "users_account_fkey",
        "FOREIGN KEY",
        "FOREIGN KEY (account_id) REFERENCES accounts(id)",
    ));
    assert_eq!(
        decide_mysql(&probe, &snapshot_with("users", table)),
        GuardVerdict::RunBare
    );
}

#[test]
fn constraint_mysql_ifnotexists_unresolved_runs_bare() {
    let probe = constraint_probe("users_email_key", GuardDir::IfNotExists, Some("UNIQUE"));
    assert_eq!(
        decide_mysql(&probe, &snapshot_with("users", empty_table())),
        GuardVerdict::RunBare
    );
}

#[test]
fn constraint_mysql_ifnotexists_primary_constraint_beats_unique_index_fallback() {
    let probe = constraint_probe("users_pkey", GuardDir::IfNotExists, Some("UNIQUE"));
    match decide_mysql(&probe, &snapshot_with("users", mysql_primary_table())) {
        GuardVerdict::FailDrift(divergence) => {
            assert_eq!(divergence.field, "kind");
            assert_eq!(divergence.expected, "UNIQUE");
            assert_eq!(divergence.actual, "PRIMARY KEY");
        }
        verdict => panic!("expected FailDrift(kind), got {verdict:?}"),
    }
}

#[test]
fn constraint_postgres_ifexists_unresolved_noops() {
    let probe = constraint_probe("users_age_check", GuardDir::IfExists, None);
    assert_eq!(
        decide_pg(&probe, &snapshot_with("users", empty_table())),
        GuardVerdict::SatisfiedNoop
    );
}

#[test]
fn constraint_sqlite_ifexists_unresolved_noops() {
    let probe = constraint_probe("users_age_check", GuardDir::IfExists, None);
    assert_eq!(
        decide_sqlite(&probe, &snapshot_with("users", empty_table())),
        GuardVerdict::SatisfiedNoop
    );
}

#[test]
fn constraint_ifnotexists_absent_runs_bare() {
    let probe = GuardProbe::Constraint {
        schema: "app".into(),
        table: "users".into(),
        name: "users_email_key".into(),
        direction: GuardDir::IfNotExists,
        expect_kind: Some("UNIQUE".into()),
        expect_definition: None,
    };
    assert_eq!(
        decide_pg(&probe, &SchemaSnapshot::default()),
        GuardVerdict::RunBare
    );
}

#[test]
fn constraint_ifnotexists_present_different_kind_fails_kind() {
    let probe = GuardProbe::Constraint {
        schema: "app".into(),
        table: "users".into(),
        name: "users_pk".into(),
        direction: GuardDir::IfNotExists,
        expect_kind: Some("UNIQUE".into()),
        expect_definition: None,
    };
    let mut t = empty_table();
    t.constraints
        .push(constraint("users_pk", "PRIMARY KEY", "PRIMARY KEY (id)"));
    match decide_pg(&probe, &snapshot_with("users", t)) {
        GuardVerdict::FailDrift(d) => assert_eq!(d.field, "kind"),
        v => panic!("expected FailDrift(kind), got {v:?}"),
    }
}

#[test]
fn constraint_ifnotexists_present_same_kind_fails_definition_not_noop() {
    // A same-name + same-kind constraint must NOT be SatisfiedNoop
    // — the live pg_get_constraintdef body cannot be proven equal to the IR's
    // un-normalized constraint, so a possibly-divergent CHECK/FK is refused.
    let probe = GuardProbe::Constraint {
        schema: "app".into(),
        table: "users".into(),
        name: "users_age_chk".into(),
        direction: GuardDir::IfNotExists,
        expect_kind: Some("CHECK".into()),
        expect_definition: None,
    };
    let mut t = empty_table();
    t.constraints
        .push(constraint("users_age_chk", "CHECK", "CHECK ((age > 18))"));
    match decide_pg(&probe, &snapshot_with("users", t)) {
        GuardVerdict::FailDrift(d) => assert_eq!(d.field, "definition"),
        v => panic!("expected FailDrift(definition) on same-name+same-kind, got {v:?}"),
    }
}

// -- F1: SQLite affinity-fold data_type compare ------------------------

#[test]
fn sqlite_timestamp_snapshot_vs_text_live_is_noop_not_false_drift() {
    // **F1 root-cause unit** — the PG-spelled snapshot data_type for a timestamp
    // column is `timestamp with time zone`, but a REAL SQLite catalog reports the
    // `text` affinity. The OLD raw `expect != live` compare false-FailDrifted on
    // EVERY re-run (every table has created_at/updated_at/deleted_at timestamps).
    // After the affinity fold both sides canonicalize to `text` → SatisfiedNoop.
    let probe = GuardProbe::Column {
        schema: "app".into(),
        table: "events".into(),
        column: "created_at".into(),
        direction: GuardDir::IfNotExists,
        // snapshot spelling (PG) vs the live SQLite `text` affinity.
        expect: Some(("timestamp with time zone".into(), false)),
        // a timestamp is NOT a TEXT-affinity SDK-facet blind spot from the
        // addColumn leg's perspective only when no facet flag is set; here we
        // assert the fold alone removes the false drift (facet None).
    };
    let mut t = empty_table();
    t.columns.push(col("created_at", "text", false));
    assert_eq!(
        decide_sqlite(&probe, &snapshot_with("events", t)),
        GuardVerdict::SatisfiedNoop,
        "a timestamp-snapshot vs text-affinity live must NOT false-drift on SQLite"
    );
}

#[test]
fn sqlite_jsonb_snapshot_vs_text_live_is_noop() {
    // **F1** — a `json` column: snapshot `jsonb`, live SQLite `text` affinity. The
    // affinity fold makes them MATCH (both `text`) → SatisfiedNoop. NOT a false
    // `jsonb != text` drift; the within-text-affinity facet blind spot is accepted
    // (differ-consistent), so a guarded re-run over a json column is idempotent.
    let probe = GuardProbe::Column {
        schema: "app".into(),
        table: "docs".into(),
        column: "body".into(),
        direction: GuardDir::IfNotExists,
        expect: Some(("jsonb".into(), true)),
    };
    let mut t = empty_table();
    t.columns.push(col("body", "text", true));
    assert_eq!(
        decide_sqlite(&probe, &snapshot_with("docs", t)),
        GuardVerdict::SatisfiedNoop,
        "a jsonb-snapshot vs text-affinity live must fold-match to a no-op on SQLite"
    );
}

#[test]
fn sqlite_real_snapshot_matches_real_live_is_noop() {
    // A non-text affinity (`number` → snapshot `double precision`, live `real`):
    // unambiguous, no facet flag. The fold maps both to `real` → SatisfiedNoop.
    let probe = GuardProbe::Column {
        schema: "app".into(),
        table: "m".into(),
        column: "amount".into(),
        direction: GuardDir::IfNotExists,
        expect: Some(("double precision".into(), true)),
    };
    let mut t = empty_table();
    t.columns.push(col("amount", "real", true));
    assert_eq!(
        decide_sqlite(&probe, &snapshot_with("m", t)),
        GuardVerdict::SatisfiedNoop
    );
}

#[test]
fn sqlite_real_text_genuine_change_still_diverges() {
    // A REAL affinity change (string→number): snapshot `text` vs live `real` fold
    // to DIFFERENT canonical tokens → FailDrift even on SQLite.
    let probe = GuardProbe::Column {
        schema: "app".into(),
        table: "m".into(),
        column: "x".into(),
        direction: GuardDir::IfNotExists,
        expect: Some(("text".into(), true)),
    };
    let mut t = empty_table();
    t.columns.push(col("x", "real", true));
    match decide_sqlite(&probe, &snapshot_with("m", t)) {
        GuardVerdict::FailDrift(d) => assert_eq!(d.field, "data_type"),
        v => panic!("expected FailDrift(data_type) on a genuine text→real change, got {v:?}"),
    }
}

#[test]
fn pg_leg_does_not_fold_affinities() {
    // The PG leg compares raw `information_schema` spellings. A `jsonb` declared
    // over a `text` live column is a real PG divergence (NOT folded to match).
    let probe = GuardProbe::Column {
        schema: "app".into(),
        table: "docs".into(),
        column: "body".into(),
        direction: GuardDir::IfNotExists,
        expect: Some(("jsonb".into(), true)),
    };
    let mut t = empty_table();
    t.columns.push(col("body", "text", true));
    match decide_pg(&probe, &snapshot_with("docs", t)) {
        GuardVerdict::FailDrift(d) => assert_eq!(d.field, "data_type"),
        v => panic!("PG leg must NOT fold jsonb→text, got {v:?}"),
    }
}

// -- F2: createTable deferred-FK structural compare --------------------

#[test]
fn constraint_ifnotexists_with_definition_schema_qualifier_normalized_is_noop() {
    // **F2** — the createTable deferred-FK probe carries the declared
    // `pg_get_constraintdef` body, which is ALWAYS schema-qualified
    // (`REFERENCES app.people(id)`), but the LIVE `pg_get_constraintdef` OMITS the
    // schema when the referenced table is in the search_path (`REFERENCES
    // people(id)`). After normalizing the qualifier out of both sides they MATCH →
    // SatisfiedNoop (the real-world re-deploy case — a hard FailDrift here was the
    // F2 bug).
    let declared = "FOREIGN KEY (owner) REFERENCES app.people(id) DEFERRABLE INITIALLY DEFERRED";
    let live = "FOREIGN KEY (owner) REFERENCES people(id) DEFERRABLE INITIALLY DEFERRED";
    let probe = GuardProbe::Constraint {
        schema: "app".into(),
        table: "pets".into(),
        name: "pets_owner_fkey".into(),
        direction: GuardDir::IfNotExists,
        expect_kind: Some("FOREIGN KEY".into()),
        expect_definition: Some(declared.into()),
    };
    let mut t = empty_table();
    t.constraints
        .push(constraint("pets_owner_fkey", "FOREIGN KEY", live));
    assert_eq!(
        decide_pg(&probe, &snapshot_with("pets", t)),
        GuardVerdict::SatisfiedNoop,
        "schema-qualifier-only difference must normalize to an idempotent no-op"
    );
}

#[test]
fn normalize_fk_definition_strips_referenced_schema_qualifier() {
    // Quoted, bare, and already-unqualified targets all normalize to the same form.
    let a = normalize_pg_constraint_definition(
        "FOREIGN KEY (owner) REFERENCES \"app\".people(id) ON DELETE RESTRICT",
    );
    let b = normalize_pg_constraint_definition(
        "FOREIGN KEY (owner) REFERENCES app.people(id) ON DELETE RESTRICT",
    );
    let c = normalize_pg_constraint_definition(
        "FOREIGN KEY (owner) REFERENCES people(id) ON DELETE RESTRICT",
    );
    assert_eq!(a, c);
    assert_eq!(b, c);
    assert!(
        c.contains("REFERENCES people(id)"),
        "table + columns preserved: {c}"
    );
    // A re-pointed target survives normalization → still DIFFERENT.
    let d = normalize_pg_constraint_definition(
        "FOREIGN KEY (owner) REFERENCES app.companies(id) ON DELETE RESTRICT",
    );
    assert_ne!(
        c, d,
        "a different referenced TABLE must not be normalized away"
    );
}

#[test]
fn normalize_fk_definition_quoted_dotted_schema_keeps_dotfree_table() {
    // Latent-invariant pin — the rsplit('.') table extraction is safe
    // ONLY because the referenced TABLE segment is dot-free post-validation
    // (`validate_ident` rejects dots; `reject_cross_app_ref` rejects dotted FK
    // targets). The qualifier ahead of it may itself be quoted, but the FINAL
    // segment is the dot-free table. A quoted reserved-word SCHEMA qualifier must
    // still strip cleanly to the dot-free table, MATCHING the bare-schema and
    // already-unqualified forms (the debug_assert must NOT fire on this path).
    let quoted_schema =
        normalize_pg_constraint_definition("FOREIGN KEY (owner) REFERENCES \"order\".people(id)");
    let bare = normalize_pg_constraint_definition("FOREIGN KEY (owner) REFERENCES people(id)");
    assert_eq!(
        quoted_schema, bare,
        "a quoted schema qualifier over a dot-free table normalizes to the bare form"
    );
    assert!(
        quoted_schema.contains("REFERENCES people(id)"),
        "the dot-free table survives: {quoted_schema}"
    );
}

#[test]
fn constraint_ifnotexists_with_definition_divergent_fails_closed() {
    // A re-pointed FK (different live definition) still fails CLOSED naming
    // `definition` even with the structural-compare path.
    let probe = GuardProbe::Constraint {
        schema: "app".into(),
        table: "pets".into(),
        name: "pets_owner_fkey".into(),
        direction: GuardDir::IfNotExists,
        expect_kind: Some("FOREIGN KEY".into()),
        expect_definition: Some(
            "FOREIGN KEY (owner) REFERENCES app.people(id) DEFERRABLE INITIALLY DEFERRED".into(),
        ),
    };
    let mut t = empty_table();
    // live FK points at a DIFFERENT table.
    t.constraints.push(constraint(
        "pets_owner_fkey",
        "FOREIGN KEY",
        "FOREIGN KEY (owner) REFERENCES app.companies(id) DEFERRABLE INITIALLY DEFERRED",
    ));
    match decide_pg(&probe, &snapshot_with("pets", t)) {
        GuardVerdict::FailDrift(d) => assert_eq!(d.field, "definition"),
        v => panic!("expected FailDrift(definition) on a re-pointed FK, got {v:?}"),
    }
}

#[test]
fn constraint_ifnotexists_with_definition_kind_clash_still_fails_kind() {
    // A kind clash takes precedence over the structural definition compare.
    let probe = GuardProbe::Constraint {
        schema: "app".into(),
        table: "pets".into(),
        name: "pets_owner_fkey".into(),
        direction: GuardDir::IfNotExists,
        expect_kind: Some("FOREIGN KEY".into()),
        expect_definition: Some("FOREIGN KEY (owner) REFERENCES app.people(id)".into()),
    };
    let mut t = empty_table();
    t.constraints
        .push(constraint("pets_owner_fkey", "UNIQUE", "UNIQUE (owner)"));
    match decide_pg(&probe, &snapshot_with("pets", t)) {
        GuardVerdict::FailDrift(d) => assert_eq!(d.field, "kind"),
        v => panic!("expected FailDrift(kind) on a kind clash, got {v:?}"),
    }
}

// -- ColumnPresence (alter/rename ifExists) ----------------------------

#[test]
fn column_presence_present_runs_absent_noops() {
    let probe = GuardProbe::ColumnPresence {
        schema: "app".into(),
        table: "users".into(),
        column: "name".into(),
        direction: GuardDir::IfExists,
    };
    let mut t = empty_table();
    t.columns.push(col("name", "text", true));
    assert_eq!(
        decide_pg(&probe, &snapshot_with("users", t)),
        GuardVerdict::RunBare
    );
    assert_eq!(
        decide_pg(&probe, &SchemaSnapshot::default()),
        GuardVerdict::SatisfiedNoop
    );
}
