//! LAYER 2 of the backend conformance kit, ONE observation: `row_order`, and the
//! ORACLE that compares its value ACROSS backends.
//!
//! # What layer 1 gives, and the one thing it does not
//!
//! `dialect_conformance_live.rs` drives every corpus op through the production apply
//! path on PostgreSQL, MySQL and SQLite and records an OUTCOME CLASS: `Applied`,
//! `RefusedByCapability`, `RefusedByPolicy`, `ServerError`, `EngineError`. That is a
//! statement about whether the engine's SQL was accepted. It is not a statement about
//! what the database then CONTAINS, and a defect that applies cleanly and then orders
//! rows wrongly is invisible to every one of those five classes.
//!
//! An observation is the missing half: `(name, args) -> Value`, evaluated against the
//! live database AFTER an op stream applies. `row_order(table, key_column)` returns
//! the sequence `ORDER BY key_column` gives back.
//!
//! # Why this is not `injected_column_collation.rs` again
//!
//! `tests/column_shapes/injected_column_collation.rs` already reads an ordering back
//! from PostgreSQL, from MySQL and from SQLite, on this same four-id fixture, and its
//! header already wins the argument against grepping the emitted DDL. Two things
//! there are genuinely different from what this file does, and both are the point.
//!
//! 1. **It applies DDL by hand.** Its three legs call `IrAuthor::lower` and then hand
//!    the resulting text to `session.batch` (or to `rusqlite::execute_batch`), and
//!    they insert rows with hand-written `INSERT` strings. Neither the guarded load
//!    (`load_and_lower_guarded`), nor the plan, nor the journal, nor the lock, nor
//!    `MigrationEngine::apply_plan` is on that path. Every op stream here crosses the
//!    SAME path layer 1 uses, DDL and DML alike, so what is observed is what an
//!    operator's migration would actually leave behind.
//! 2. **Its three legs never meet.** Each leg compares its own read to the
//!    module-level [`CREATION_ORDER`] constant. Three tests asserting `a == K`,
//!    `b == K`, `c == K` do prove `a == b == c`, so cross-backend agreement is a
//!    THEOREM there - but only for a fixture whose correct value someone already
//!    knew how to write down. The oracle below needs no such constant: it takes the
//!    three values and requires them to be EQUAL TO EACH OTHER. That is what lets a
//!    fixture nobody has an expected value for still be a conformance test, which is
//!    the whole reason the proposal asks for a differential rather than a golden.
//!
//! # The oracle, and its red
//!
//! Default rule: for one fixture and one observation, every backend must produce the
//! same value ([`oracle`]). The rule is only worth having if it can distinguish, so
//! two checks sit under it and neither can be satisfied by a stub:
//!
//! - [`the_oracle_separates_agreement_from_disagreement`] runs the oracle itself over
//!   values, offline. It is the mechanism check.
//! - [`row_order_disagrees_across_the_three_backends_without_the_pin`] is the LIVE
//!   red. The same fixture with the collation removed makes the three servers return
//!   three-way-split answers, and the oracle must SEE that. Without it,
//!   [`row_order_agrees_across_the_three_backends_when_the_column_pins_bytewise`]
//!   could be passing because the harness returns the same thing three times for a
//!   reason that has nothing to do with the servers.
//!
//! Both live tests also refuse to run on a server that cannot distinguish the pinned
//! fixture from the unpinned one: a `C`/`POSIX` PostgreSQL or a `_bin` MySQL makes
//! the two fixtures identical, and every claim here would hold vacuously. They FAIL
//! rather than skip, for the reason `injected_column_collation.rs` records.
//!
//! The red was MEASURED, by mutation, and the first mutation was a false green worth
//! recording. Flipping the agreement test's SQLITE leg to the unpinned fixture left it
//! PASSING - correctly, because SQLite's `BINARY` default is already bytewise, so that
//! leg's observed value does not move between the two fixtures at all. Flipping the
//! POSTGRESQL leg failed it, with the split named:
//! `["postgres"] -> [aaa, AAA, zzz, Zzz]` against `["mysql", "sqlite"] -> creation
//! order`. Only two of the three legs are capable of moving this fixture, and a
//! mutation test that picks the third one measures nothing.
//!
//! Measured on the servers this suite runs against: under the UNPINNED fixture,
//! PostgreSQL (`en_US.utf8`) and MySQL (`utf8mb4_0900_as_cs`) return the SAME wrong
//! sequence as each other, and SQLite returns creation order. The split is
//! `{postgres, mysql}` against `{sqlite}`, not a three-way one. Two backends agreeing
//! on a wrong answer is exactly the case the section below is about.
//!
//! # What this observation CANNOT see
//!
//! The oracle's verdict is "the three backends agree". It is not "the three backends
//! are right". A fixture on which all three are equally wrong passes, and the
//! proposal does not address that. Here that gap is closed by hand, and only because
//! the correct value happens to be knowable: the agreeing value is additionally
//! required to be [`CREATION_ORDER`]. A fixture without a knowable expected value
//! gets the differential and nothing else.

use crate::support;

use std::collections::BTreeMap;
use std::path::PathBuf;

use tempfile::TempDir;

use crate::support::mysql::{DatabaseGuard, MysqlDevSession};
use crate::support::PgDevSession;
use zero_migrate::apply::backend::sqlite::Mode;
use zero_migrate::apply::backend::{MigrationBackend, MysqlBackend};
use zero_migrate::apply::executor::LockMode;
use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::Op;
use zero_migrate::render::fold::single_fold;
use zero_migrate::{
    resolve_create_table_policy, Approval, EffectivePolicy, ExecutorConfig, GuardConfig, IrAuthor,
    LiveSchema, MigrationEngine, MigrationIr, PostgresBackend, SqlDialect, SqliteBackend,
};

const OWNER: &str = "app_row_order";

/// The observed table, and the key column `ORDER BY` is taken over.
const TABLE: &str = "t";
const KEY: &str = "id";

/// The observation's third argument, `inserted_rows`, and simultaneously the value a
/// bytewise column must give back.
///
/// Four ids in a consumer's shape - a prefix plus base62 - listed in CREATION order,
/// which for base62 of a monotonic UUIDv7 is BYTE order. The suffixes vary only in
/// the case runs `A`, `Z`, `a`, `z`, whose byte values ascend
/// (0x41 < 0x5A < 0x61 < 0x7A). A locale-aware collation interleaves those runs; a
/// bytewise one does not.
///
/// Deliberately the same four ids `injected_column_collation.rs` uses and the same
/// four the proposal's own worked example names
/// (`observe = "row_order(t, id, [aaa, AAA, zzz, Zzz])"`). A second set of ids would
/// be a second fixture pretending to be the same one.
const CREATION_ORDER: [&str; 4] = [
    "note_0000000000000000000AAA",
    "note_0000000000000000000Zzz",
    "note_0000000000000000000aaa",
    "note_0000000000000000000zzz",
];

/// Whether the fixture's key column pins a bytewise collation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pin {
    /// `collation: "bytewise"` on the key column.
    Bytewise,
    /// No collation facet at all - the column takes whatever the dialect's default is.
    None,
}

impl Pin {
    const fn label(self) -> &'static str {
        match self {
            Self::Bytewise => "bytewise",
            Self::None => "unpinned",
        }
    }
}

// ---------------------------------------------------------------------------
// The fixture, authored through the PUBLIC path
// ---------------------------------------------------------------------------

/// The op stream, as the two IR envelopes a real migration would be.
///
/// DDL and DML are separate envelopes because the DML one has to carry
/// `irreversible`: an `insert` has no recorded inverse, and the loader refuses a
/// reversible envelope containing one. That split is the shape
/// `pg_scenarios.rs` and `scalar_precision_boundary_pg.rs` already author in.
///
/// The collation rides the AUTHORED column, not a charter injection. `IrColumn`
/// carries `collation: Option<ColumnCollation>` as a closed one-member lexicon
/// (`bytewise`), so this fixture is exactly what a user could write, which is the
/// proposal's requirement that fixtures be authored through the public path.
///
/// The key column is a BOUNDED `string`, not `text`, and that was measured rather
/// than chosen. The first version of this fixture keyed an unbounded `text` column,
/// and the engine refused it on MySQL before any server was touched:
/// `DIALECT_UNSUPPORTED` / "createTable.primaryKey keys t.id, which renders as MySQL
/// TEXT storage; MySQL refuses a key over a TEXT or BLOB column with no prefix
/// length". PostgreSQL and SQLite both accepted the same op. That is a legitimate
/// capability difference, it is layer 1's business rather than layer 2's - an
/// observation needs a fixture that APPLIES on every backend before it can compare
/// anything - and a fixture that trips it would have measured the refusal, not the
/// ordering.
fn ddl_envelope(pin: Pin) -> String {
    let collation = match pin {
        Pin::Bytewise => r#","collation":"bytewise""#,
        Pin::None => "",
    };
    format!(
        r#"{{"ir_version":1,"name":"row_order_ddl","ops":[
             {{"op":"createTable","name":"{TABLE}",
               "columns":[{{"name":"{KEY}","type":{{"string":{{"length":64}}}},
                            "nullable":false{collation}}}],
               "primaryKey":["{KEY}"],"indexes":[]}}
           ]}}"#
    )
}

/// The rows, inserted in [`CREATION_ORDER`] so a wrong read order cannot be an
/// artefact of the insert order.
fn dml_envelope() -> String {
    let rows = CREATION_ORDER
        .iter()
        .map(|id| format!(r#"["{id}"]"#))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"{{"ir_version":1,"name":"row_order_dml",
             "irreversible":"row_order fixture: DML has no recorded inverse",
             "ops":[{{"op":"insert","table":"{TABLE}","columns":["{KEY}"],"rows":[{rows}]}}]}}"#
    )
}

fn registry() -> BTreeMap<String, String> {
    BTreeMap::from([(TABLE.to_string(), OWNER.to_string())])
}

/// Author + lower one envelope through the production guarded path, then apply it
/// through `MigrationEngine`.
///
/// This is the whole difference from `injected_column_collation.rs`'s legs, and it is
/// the same sequence `dialect_conformance_live.rs::run_row` uses: resolve the
/// charter's create-table policy, `load_and_lower_guarded`, `apply_plan`.
async fn apply_envelope<B: MigrationBackend>(
    envelope: &str,
    tag: &str,
    backend: &B,
    cfg: &ExecutorConfig,
    policy: &EffectivePolicy,
    dialect: SqlDialect,
    live: &LiveSchema,
) -> Result<(), String> {
    let authored: MigrationIr = serde_json::from_str(envelope)
        .map_err(|error| format!("{tag}: the fixture envelope did not parse: {error}"))?;
    let resolved = resolve_create_table_policy(&authored, policy, &cfg.project_schema)
        .map_err(|error| format!("{tag}: resolve create-table policy: {error}"))?;
    let source = serde_json::to_string(&resolved)
        .map_err(|error| format!("{tag}: re-serialize the resolved envelope: {error}"))?;
    let artifact = IrAuthor::new(&cfg.project_schema, OWNER, dialect, policy)
        .load_and_lower_guarded(
            &source,
            OWNER,
            &registry(),
            live,
            &GuardConfig::from_policy(policy.clone(), dialect),
        )
        .map_err(|error| format!("{tag}: guarded lower: {error:?}"))?;
    MigrationEngine::new()
        .apply_plan(
            &artifact.plan.steps,
            Approval::Approved,
            backend,
            cfg,
            "row-order-observation",
            LockMode::Acquire,
        )
        .await
        .map_err(|error| format!("{tag}: apply: {error}"))?;
    Ok(())
}

/// Apply the whole fixture - DDL then DML - the way a deployment would.
///
/// The DML lowers against a live schema read back from the CATALOG, not against
/// `LiveSchema::default()`, for the reason layer 1 records: an op that lowers against
/// a schema the server does not have is answering a different question. SQLite
/// additionally needs `sqlite_schemas`, folded from the DDL that just applied, which
/// IS this stream's history.
async fn apply_fixture<B: MigrationBackend>(
    pin: Pin,
    backend: &B,
    cfg: &ExecutorConfig,
    policy: &EffectivePolicy,
    dialect: SqlDialect,
) -> Result<(), String> {
    let ddl = ddl_envelope(pin);
    apply_envelope(
        &ddl,
        "ddl",
        backend,
        cfg,
        policy,
        dialect,
        &LiveSchema::default(),
    )
    .await?;

    let snapshot = backend
        .snapshot_schema(cfg)
        .await
        .map_err(|error| format!("read the applied schema back: {error}"))?;
    let mut live = LiveSchema::from_catalog_snapshot(snapshot, OWNER);
    if dialect == SqlDialect::Sqlite {
        let authored: MigrationIr =
            serde_json::from_str(&ddl).map_err(|error| format!("re-parse the DDL: {error}"))?;
        let history: Vec<Op> = authored.ops;
        if let Ok(defs) = single_fold::fold(&history, dialect, &cfg.project_schema, policy)
            .map(|folded| folded.project_field_defs())
        {
            live.sqlite_schemas = defs;
        }
    }

    apply_envelope(&dml_envelope(), "dml", backend, cfg, policy, dialect, &live).await
}

// ---------------------------------------------------------------------------
// The observation
// ---------------------------------------------------------------------------

/// One backend's answer to one observation on one fixture.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Observed {
    backend: &'static str,
    value: Vec<String>,
}

/// The ORACLE. Default rule: for one fixture and one observation, every backend must
/// produce the same value.
///
/// `Ok(value)` is the agreed value; `Err(report)` NAMES the split, grouped by value,
/// so the failure says which backends disagreed with which - not merely that they
/// did. A differential whose failure message is "not equal" costs a debugging session
/// per occurrence.
///
/// There is deliberately no exception channel here. The proposal's `[[case.differs]]`
/// belongs at the fixture, and this spike has one fixture that needs none; a
/// suppression seam built before a case that needs it is a seam nobody has measured.
/// See this file's report for why `ALLOWANCES` is the wrong host for it.
fn oracle(observations: &[Observed]) -> Result<Vec<String>, String> {
    assert!(
        observations.len() >= 2,
        "a differential over fewer than two backends is not a differential"
    );
    let mut by_value: Vec<(Vec<String>, Vec<&'static str>)> = Vec::new();
    for observed in observations {
        if let Some(entry) = by_value.iter_mut().find(|(v, _)| *v == observed.value) {
            entry.1.push(observed.backend);
        } else {
            by_value.push((observed.value.clone(), vec![observed.backend]));
        }
    }
    if by_value.len() == 1 {
        return Ok(by_value.remove(0).0);
    }
    let mut report = format!(
        "{} backends produced {} different values for this observation:",
        observations.len(),
        by_value.len()
    );
    for (value, backends) in &by_value {
        report.push_str(&format!("\n  {backends:?} -> {value:?}"));
    }
    Err(report)
}

// ---------------------------------------------------------------------------
// Per-backend drivers: isolate, apply the fixture, observe
// ---------------------------------------------------------------------------

/// A per-process-unique name. `zmrowobs_` rather than layer 1's `zmconf_`, because
/// that suite's leak census claims every `zmconf_%` name it can attribute to a dead
/// pid, and these are not its to judge.
fn token(suffix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "zmrowobs_{}_{}_{suffix}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

async fn pg_row_order(url: &str, pin: Pin) -> Result<Vec<String>, String> {
    let session = PgDevSession::connect(url);
    let schema = token(pin.label());
    let policy = support::operator_charter(&schema);
    let cfg = ExecutorConfig::new(format!("prj_{schema}"), &schema, policy.clone());
    let _guard = support::SchemaGuard::arm(
        &session,
        [
            cfg.project_schema.clone(),
            cfg.confinement.meta_schema.clone(),
        ],
    );
    session
        .batch(&format!("CREATE SCHEMA \"{}\"", cfg.project_schema))
        .await
        .map_err(|error| format!("create the probe schema: {error}"))?;
    let backend = PostgresBackend::new_generic(&session);
    backend
        .ensure_journal(&cfg)
        .await
        .map_err(|error| format!("create the journal: {error}"))?;
    apply_fixture(pin, &backend, &cfg, &policy, SqlDialect::Postgres).await?;

    let rows = session
        .query(
            &format!("SELECT {KEY} FROM \"{schema}\".\"{TABLE}\" ORDER BY {KEY}"),
            &[],
        )
        .await
        .map_err(|error| format!("observe row_order: {error}"))?;
    rows.iter()
        .map(|row| {
            row.try_get::<_, String>(0)
                .map_err(|error| format!("the key column did not decode as text: {error}"))
        })
        .collect()
}

async fn mysql_row_order(url: &str, pin: Pin) -> Result<Vec<String>, String> {
    let session = MysqlDevSession::connect(url);
    let database = token(pin.label());
    let policy = support::operator_charter(&database);
    let cfg = ExecutorConfig::new(format!("prj_{database}"), &database, policy.clone());
    let _guard = DatabaseGuard::arm(
        &session,
        [
            cfg.project_schema.clone(),
            cfg.confinement.meta_schema.clone(),
        ],
    );
    session
        .batch(&format!(
            "CREATE DATABASE {}",
            support::mysql::quote_ident(&cfg.project_schema)
        ))
        .await
        .map_err(|error| format!("create the probe database: {error}"))?;
    let backend = MysqlBackend::new_generic(&session);
    backend
        .ensure_journal(&cfg)
        .await
        .map_err(|error| format!("create the journal: {error}"))?;
    apply_fixture(pin, &backend, &cfg, &policy, SqlDialect::Mysql).await?;

    let rows = session
        .query(
            &format!(
                "SELECT {KEY} FROM {}.`{TABLE}` ORDER BY {KEY}",
                support::mysql::quote_ident(&database)
            ),
            &[],
        )
        .await
        .map_err(|error| format!("observe row_order: {error}"))?;
    rows.iter()
        .map(|row| {
            row.try_get::<_, String>(KEY)
                .map_err(|error| format!("the key column did not decode as text: {error}"))
        })
        .collect()
}

const SQLITE_PROJECT: &str = "prj_row_order";

async fn sqlite_row_order(pin: Pin) -> Result<Vec<String>, String> {
    let dir: TempDir =
        tempfile::tempdir().map_err(|error| format!("create a temp dir: {error}"))?;
    let app: PathBuf = dir.path().join("probe.sqlite");
    let journal: PathBuf = dir.path().join("probe.migrations.sqlite");
    let backend = SqliteBackend::open(&app, &journal)
        .map_err(|error| format!("open the probe database: {error}"))?;
    let policy = support::operator_charter(SQLITE_PROJECT);
    let cfg = ExecutorConfig::new(SQLITE_PROJECT, SQLITE_PROJECT, policy.clone());
    apply_fixture(pin, &backend, &cfg, &policy, SqlDialect::Sqlite).await?;

    backend
        .actor()
        .set_mode(Mode::EngineJournal)
        .await
        .map_err(|error| format!("switch the actor to the journal mode: {error}"))?;
    let rows = backend
        .actor()
        .query(&format!(
            "SELECT {KEY} FROM main.\"{TABLE}\" ORDER BY {KEY}"
        ))
        .await
        .map_err(|error| format!("observe row_order: {error}"))?;
    rows.iter()
        .map(|row| {
            row.first()
                .cloned()
                .flatten()
                .ok_or_else(|| "the key column came back NULL".to_string())
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The instrument checks
// ---------------------------------------------------------------------------

/// Every ordering claim below is void on a database whose default collation is
/// already bytewise, because then the pinned and the unpinned fixtures agree and the
/// pair proves nothing.
async fn pg_default_collation(session: &PgDevSession) -> Result<String, String> {
    let row = session
        .query_one(
            "SELECT datcollate FROM pg_database WHERE datname = current_database()",
            &[],
        )
        .await
        .map_err(|error| format!("read the database collation: {error}"))?;
    row.try_get::<_, String>(0)
        .map_err(|error| format!("datcollate did not decode as text: {error}"))
}

async fn mysql_default_collation(session: &MysqlDevSession) -> Result<String, String> {
    let row = session
        .query_one("SELECT @@collation_server AS collation_server", &[])
        .await
        .map_err(|error| format!("read the server collation: {error}"))?;
    row.try_get::<_, String>("collation_server")
        .map_err(|error| format!("@@collation_server did not decode as text: {error}"))
}

/// FAIL, never skip: a server that cannot distinguish the two fixtures makes the
/// whole file vacuous, and a vacuous green is the failure mode this layer exists to
/// stop.
async fn refuse_a_saturated_instrument(pg: &PgDevSession, mysql: &MysqlDevSession) {
    let collate = pg_default_collation(pg)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        collate != "C" && collate != "POSIX",
        "this PostgreSQL database's default collation is {collate}, so the pinned and \
         the unpinned fixtures order identically and no ordering claim here can \
         distinguish them"
    );
    let collation = mysql_default_collation(mysql)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        !collation.ends_with("_bin") && collation != "binary",
        "this MySQL server's default collation is {collation}, so the pinned and the \
         unpinned fixtures order identically and no ordering claim here can \
         distinguish them"
    );
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// The oracle's mechanism check, offline. It has to separate the two cases and it has
/// to name the split, or the live red below would be indistinguishable from a stub
/// that always returns `Err`.
#[test]
fn the_oracle_separates_agreement_from_disagreement() {
    let creation: Vec<String> = CREATION_ORDER.iter().map(|s| (*s).to_string()).collect();
    let interleaved: Vec<String> = vec![
        CREATION_ORDER[2].to_string(),
        CREATION_ORDER[0].to_string(),
        CREATION_ORDER[3].to_string(),
        CREATION_ORDER[1].to_string(),
    ];

    let agreed = oracle(&[
        Observed {
            backend: "postgres",
            value: creation.clone(),
        },
        Observed {
            backend: "mysql",
            value: creation.clone(),
        },
        Observed {
            backend: "sqlite",
            value: creation.clone(),
        },
    ])
    .expect("three equal values agree");
    assert_eq!(agreed, creation, "the agreed value is the value observed");

    let split = oracle(&[
        Observed {
            backend: "postgres",
            value: interleaved.clone(),
        },
        Observed {
            backend: "mysql",
            value: interleaved,
        },
        Observed {
            backend: "sqlite",
            value: creation,
        },
    ])
    .expect_err("a two-way split is a disagreement");
    assert!(
        split.contains("\"postgres\", \"mysql\"") && split.contains("\"sqlite\""),
        "the report must name WHICH backends took which value; got:\n{split}"
    );
    assert!(
        split.contains("2 different values"),
        "the report must count the distinct values; got:\n{split}"
    );
}

/// The claim. One fixture, one observation, three live backends, and the three values
/// are required to be equal TO EACH OTHER.
#[compio::test]
async fn row_order_agrees_across_the_three_backends_when_the_column_pins_bytewise() {
    let Some(pg_url) = support::pg_url() else {
        support::announce_live_db_skip(support::PG_URL_ENV);
        return;
    };
    let Some(mysql_url) = support::mysql::mysql_url() else {
        support::announce_live_db_skip(support::mysql::MYSQL_URL_ENV);
        return;
    };
    let pg_session = PgDevSession::connect(&pg_url);
    let mysql_session = MysqlDevSession::connect(&mysql_url);
    refuse_a_saturated_instrument(&pg_session, &mysql_session).await;

    let observations = vec![
        Observed {
            backend: "postgres",
            value: pg_row_order(&pg_url, Pin::Bytewise)
                .await
                .unwrap_or_else(|error| panic!("postgres: {error}")),
        },
        Observed {
            backend: "mysql",
            value: mysql_row_order(&mysql_url, Pin::Bytewise)
                .await
                .unwrap_or_else(|error| panic!("mysql: {error}")),
        },
        Observed {
            backend: "sqlite",
            value: sqlite_row_order(Pin::Bytewise)
                .await
                .unwrap_or_else(|error| panic!("sqlite: {error}")),
        },
    ];
    println!("LEDGER row_order pin=bytewise {observations:?}");

    let agreed = oracle(&observations).unwrap_or_else(|report| {
        panic!(
            "row_order must not depend on the backend under a pinned bytewise collation:\n{report}"
        )
    });

    // The differential proves AGREEMENT, never CORRECTNESS. Three backends equally
    // wrong would pass the line above. This fixture is one of the ones whose right
    // answer is knowable, so it is also asserted - see the module header.
    assert_eq!(
        agreed,
        CREATION_ORDER.to_vec(),
        "the three backends agree, but on the wrong sequence: a bytewise key column \
         must give creation order back"
    );
}

/// The LIVE red, and the saturation check for the test above.
///
/// The same fixture with the collation facet removed. PostgreSQL under a locale-aware
/// default interleaves the case runs, MySQL's `utf8mb4_0900_as_cs` weights case as a
/// UCA tertiary difference and orders differently again, and SQLite's `BINARY`
/// default is already bytewise and keeps creation order. The oracle must SEE that
/// split. If it cannot, the harness is returning one value three times and the
/// agreement test above is measuring the harness rather than the servers.
#[compio::test]
async fn row_order_disagrees_across_the_three_backends_without_the_pin() {
    let Some(pg_url) = support::pg_url() else {
        support::announce_live_db_skip(support::PG_URL_ENV);
        return;
    };
    let Some(mysql_url) = support::mysql::mysql_url() else {
        support::announce_live_db_skip(support::mysql::MYSQL_URL_ENV);
        return;
    };
    let pg_session = PgDevSession::connect(&pg_url);
    let mysql_session = MysqlDevSession::connect(&mysql_url);
    refuse_a_saturated_instrument(&pg_session, &mysql_session).await;

    let observations = vec![
        Observed {
            backend: "postgres",
            value: pg_row_order(&pg_url, Pin::None)
                .await
                .unwrap_or_else(|error| panic!("postgres: {error}")),
        },
        Observed {
            backend: "mysql",
            value: mysql_row_order(&mysql_url, Pin::None)
                .await
                .unwrap_or_else(|error| panic!("mysql: {error}")),
        },
        Observed {
            backend: "sqlite",
            value: sqlite_row_order(Pin::None)
                .await
                .unwrap_or_else(|error| panic!("sqlite: {error}")),
        },
    ];
    println!("LEDGER row_order pin=none {observations:?}");

    let sqlite = observations
        .iter()
        .find(|o| o.backend == "sqlite")
        .expect("the sqlite observation is in the list");
    assert_eq!(
        sqlite.value,
        CREATION_ORDER.to_vec(),
        "SQLite's BINARY default is already bytewise, so the unpinned fixture must \
         still give creation order there; if this moved, the split below is no longer \
         the one this test names"
    );

    let report = oracle(&observations).err().unwrap_or_else(|| {
        panic!(
            "the unpinned fixture came back identical on all three backends, so the \
             agreement test cannot be measuring the servers. Observations: \
             {observations:?}"
        )
    });
    assert!(
        report.contains("\"sqlite\""),
        "SQLite must be on one side of the split; got:\n{report}"
    );
}
