//! Faithful e2e for the creator IR envelope path on the `SQLite` leg,
//! driven through the REAL fail-closed LOAD GATE + lower (`IrAuthor::load_and_lower`)
//! and APPLIED on a real temp-file `SQLite` backend via the engine.
//!
//! This is the `SQLite` peer of the PG deploy e2e:
//! a valid IR envelope lowers + applies (the table exists, the migration journals),
//! and the SQLite-specific hostile case — an out-of-envelope `.splitPart`
//! against a `SQLite` target — is refused by the gate (`EXPR_NOT_PORTABLE`) before
//! any apply. No shims, no PG-gating: the real `SQLite` runtime.

use crate::support;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::apply::backend::sqlite::Mode;
use zero_migrate::{
    apply::executor::LockMode, resolve_create_table_policy, Approval, DeclarativeApplyError,
    EngineError, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, LoadAndLowerError,
    MigrationEngine, MigrationIr, SqlDialect, SqliteBackend,
};

const PROJECT: &str = "prj_ir";
const APP: &str = "app_ir";

struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn paths(tag: &str) -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join(format!("zs-{tag}.sqlite"));
    let journal = dir.path().join(format!("zs-{tag}.migrations.sqlite"));
    Paths {
        _dir: dir,
        app,
        journal,
    }
}

fn backend(p: &Paths) -> SqliteBackend {
    SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend")
}

fn exec_cfg() -> ExecutorConfig {
    ExecutorConfig::new(PROJECT, PROJECT, support::no_inject(PROJECT))
}

fn registry(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(t, o)| (t.to_string(), o.to_string()))
        .collect()
}

fn resolved_envelope_json(raw: &str) -> String {
    let ir: MigrationIr = serde_json::from_str(raw).expect("test IR parses");
    let resolved = resolve_create_table_policy(&ir, &support::confined_charter(), PROJECT)
        .expect("test IR resolves");
    serde_json::to_string(&resolved).expect("resolved test IR serializes")
}

fn no_inject_envelope_json(raw: &str) -> String {
    let ir: MigrationIr = serde_json::from_str(raw).expect("test IR parses");
    let resolved = resolve_create_table_policy(&ir, &support::no_inject("app"), PROJECT)
        .expect("test IR resolves without platform columns");
    serde_json::to_string(&resolved).expect("resolved test IR serializes")
}

fn assert_exact_uuid(value: &str, version: u8) {
    let bytes = value.as_bytes();
    assert_eq!(bytes.len(), 36, "canonical UUID length: {value}");
    for separator in [8, 13, 18, 23] {
        assert_eq!(bytes[separator], b'-', "canonical UUID separators: {value}");
    }
    assert_eq!(
        bytes[14],
        b'0' + version,
        "UUID version bits must identify version {version}: {value}"
    );
    assert!(
        matches!(bytes[19], b'8' | b'9' | b'a' | b'b'),
        "UUID variant bits must be RFC 9562: {value}"
    );
    assert_eq!(value, value.to_ascii_lowercase(), "UUID must be lowercase");
    assert!(
        bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 8 | 13 | 18 | 23)
                || byte.is_ascii_digit()
                || matches!(byte, b'a'..=b'f')
        }),
        "UUID must contain only lowercase hexadecimal digits and separators: {value}"
    );
}

fn crockford_value(byte: u8, uppercase: bool) -> Option<u8> {
    let alphabet = if uppercase {
        b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".as_slice()
    } else {
        b"0123456789abcdefghjkmnpqrstvwxyz".as_slice()
    };
    alphabet
        .iter()
        .position(|candidate| *candidate == byte)
        .map(|index| index as u8)
}

fn assert_canonical_crockford(value: &str, uppercase: bool) -> u128 {
    let bytes = value.as_bytes();
    assert_eq!(bytes.len(), 26, "canonical Crockford length: {value}");
    let first = crockford_value(bytes[0], uppercase)
        .unwrap_or_else(|| panic!("invalid Crockford character in {value}"));
    assert!(
        first <= 7,
        "128-bit Crockford value must begin at most 7: {value}"
    );

    bytes[1..].iter().fold(first as u128, |decoded, byte| {
        let digit = crockford_value(*byte, uppercase)
            .unwrap_or_else(|| panic!("invalid Crockford character in {value}"));
        (decoded << 5) | u128::from(digit)
    })
}

fn assert_exact_type_id(value: &str, prefix: &str) {
    let suffix = if prefix.is_empty() {
        value
    } else {
        value
            .strip_prefix(&format!("{prefix}_"))
            .unwrap_or_else(|| panic!("TypeID must preserve prefix {prefix:?}: {value}"))
    };
    let decoded = assert_canonical_crockford(suffix, false).to_be_bytes();
    assert_eq!(
        decoded[6] >> 4,
        7,
        "TypeID suffix must encode UUIDv7: {value}"
    );
    assert_eq!(
        decoded[8] & 0xc0,
        0x80,
        "TypeID suffix must encode the RFC UUID variant: {value}"
    );
}

fn assert_exact_ulid(value: &str) {
    let _ = assert_canonical_crockford(value, true);
}

// Happy path: a valid IR envelope createTable is gated (SQLite dialect), lowered,
// and APPLIED on a real SQLite backend — the table exists + journals.
#[compio::test]
async fn ir_envelope_lowers_and_applies_on_sqlite() {
    let p = paths("ir_apply");
    let be = backend(&p);

    let ir = resolved_envelope_json(
        r#"{"ir_version":1,"name":"create_notes","ops":[
        {"op":"createTable","name":"notes","columns":[
            {"name":"title","type":"text","nullable":false},
            {"name":"body","type":"text"}
        ]}
    ]}"#,
    );

    // The REAL fail-closed gate + lower, SQLite dialect.
    let author = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Sqlite,
        &support::confined_charter(),
    );
    let migrations = author
        .load_and_lower(&ir, APP, &registry(&[]), &LiveSchema::default())
        .expect("a valid IR envelope must lower on SQLite");
    assert!(!migrations.is_empty(), "lowering must yield migration(s)");

    // Apply through the engine on the real SQLite backend (Confined SQLite guard).
    let engine = MigrationEngine::new();
    let guard_cfg = GuardConfig::from_policy(support::no_inject(PROJECT), SqlDialect::Sqlite);
    let plan = engine.plan(&migrations, &guard_cfg);
    assert!(
        plan.denied.is_empty(),
        "no denials on a clean IR set: {:?}",
        plan.denied
    );
    let outcome = engine
        .apply(&plan, Approval::None, &be, &exec_cfg(), "deploy-ir")
        .await
        .expect("apply the lowered IR on SQLite");
    assert!(!outcome.applied.is_empty(), "the IR migration must apply");

    // The table really exists in the SQLite app file.
    let rows = be
        .actor()
        .query("SELECT name FROM sqlite_master WHERE type='table' AND name='notes'")
        .await
        .expect("sqlite_master probe");
    assert_eq!(
        rows.len(),
        1,
        "the IR-created 'notes' table must exist on SQLite"
    );
}

#[compio::test]
async fn per_row_backfill_generates_a_fresh_exact_value_for_every_sqlite_row() {
    let p = paths("per_row_generators");
    let be = backend(&p);
    let schema_ir = no_inject_envelope_json(
        r#"{"ir_version":1,"name":"sqlite_per_row_generators_schema","ops":[
          {"op":"createTable","name":"samples","columns":[
            {"name":"id","type":"bigInt","nullable":false},
            {"name":"uuid4","type":"uuid"},
            {"name":"uuid7","type":"uuid"},
            {"name":"type_id","type":"text","valueFormat":{"typeId":{"prefix":"order"}}},
            {"name":"ulid","type":"text","valueFormat":"ulid"}
          ],"primaryKey":["id"]}
        ]}"#,
    );
    let data_ir = no_inject_envelope_json(
        r#"{"ir_version":1,"name":"sqlite_per_row_generators_data","irreversible":"inserts rows and overwrites generated identifiers without recording the inserted rows or pre-images","ops":[
          {"op":"insert","table":"samples","columns":["id"],
           "rows":[[1],[2],[3],[4],[5],[6],[7],[8]]},
          {"op":"backfill","table":"samples","name":"fill_per_row_ids",
           "cursorColumns":["id"],"cursorStability":{"mode":"guardUpdates"},"batchSize":2,"set":{
             "uuid4":{"perRow":"uuidV4"},
             "uuid7":{"perRow":"uuidV7"},
             "type_id":{"perRow":{"typeId":{"prefix":"order"}}},
             "ulid":{"perRow":"ulid"}
           }}
        ]}"#,
    );
    let charter = support::no_inject("app");
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &charter);
    let guard_cfg = GuardConfig::from_policy(support::no_inject(PROJECT), SqlDialect::Sqlite);
    let schema_artifact = author
        .load_and_lower_guarded(
            &schema_ir,
            APP,
            &registry(&[]),
            &LiveSchema::default(),
            &guard_cfg,
        )
        .expect("the perRow destination schema must lower on SQLite");

    MigrationEngine::new()
        .apply_plan(
            &schema_artifact.plan.steps,
            Approval::None,
            &be,
            &exec_cfg(),
            "sqlite-per-row-generator-schema-test",
            LockMode::Acquire,
        )
        .await
        .expect("the perRow destination schema applies on SQLite");

    let schema_envelope: MigrationIr =
        serde_json::from_str(&schema_ir).expect("resolved schema IR parses");
    let mut live = LiveSchema::from_tables(BTreeSet::from(["samples".to_string()]));
    live.advance_logical_columns(&schema_envelope, SqlDialect::Sqlite, PROJECT, None)
        .expect("the schema records the perRow destination formats");
    let data_artifact = author
        .load_and_lower_guarded(
            &data_ir,
            APP,
            &registry(&[("samples", APP)]),
            &live,
            &guard_cfg,
        )
        .expect("declared perRow destination formats must lower on SQLite");
    MigrationEngine::new()
        .apply_plan(
            &data_artifact.plan.steps,
            Approval::Approved,
            &be,
            &exec_cfg(),
            "sqlite-per-row-generator-test",
            LockMode::Acquire,
        )
        .await
        .expect("perRow generators apply on SQLite");

    be.actor()
        .set_mode(Mode::CreatorUp)
        .await
        .expect("enter read mode");
    let rows = be
        .actor()
        .query("SELECT uuid4, uuid7, type_id, ulid FROM samples ORDER BY id")
        .await
        .expect("read generated values");
    assert_eq!(rows.len(), 8);

    let mut distinct = [
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
    ];
    for row in &rows {
        let uuid4 = row[0].as_deref().expect("uuid4 is populated");
        let uuid7 = row[1].as_deref().expect("uuid7 is populated");
        let type_id = row[2].as_deref().expect("TypeID is populated");
        let ulid = row[3].as_deref().expect("ULID is populated");
        assert_exact_uuid(uuid4, 4);
        assert_exact_uuid(uuid7, 7);
        assert_exact_type_id(type_id, "order");
        assert_exact_ulid(ulid);
        distinct[0].insert(uuid4.to_string());
        distinct[1].insert(uuid7.to_string());
        distinct[2].insert(type_id.to_string());
        distinct[3].insert(ulid.to_string());
    }
    for values in distinct {
        assert_eq!(
            values.len(),
            rows.len(),
            "an apply-engine generator must never reuse one build-time or batch literal"
        );
    }
}

#[compio::test]
async fn per_row_destination_mismatches_fail_before_any_sqlite_row_changes() {
    let p = paths("per_row_destination_validation");
    let be = backend(&p);
    be.actor()
        .set_mode(Mode::CreatorUp)
        .await
        .expect("enter creator mode");
    be.actor()
        .exec(
            "CREATE TABLE guarded_items (id INTEGER PRIMARY KEY, generated TEXT); \
             INSERT INTO guarded_items (id, generated) VALUES (1, 'unchanged')",
        )
        .await
        .expect("seed validation sentinel");

    let cases = [
        (
            "TypeID prefix mismatch",
            serde_json::json!({
                "name": "generated",
                "type": "text",
                "valueFormat": { "typeId": { "prefix": "declared" } }
            }),
            serde_json::json!({ "perRow": { "typeId": { "prefix": "requested" } } }),
            "stored prefix \"declared\"",
            "declared stored prefix is exactly \"requested\"",
        ),
        (
            "TypeID on generic text",
            serde_json::json!({ "name": "generated", "type": "text" }),
            serde_json::json!({ "perRow": { "typeId": { "prefix": "order" } } }),
            "perRow.typeId",
            "generic text with no value-format contract",
        ),
        (
            "ULID on generic text",
            serde_json::json!({ "name": "generated", "type": "text" }),
            serde_json::json!({ "perRow": "ulid" }),
            "perRow.ulid",
            "generic text with no value-format contract",
        ),
        (
            "UUIDv4 on generic text",
            serde_json::json!({ "name": "generated", "type": "text" }),
            serde_json::json!({ "perRow": "uuidV4" }),
            "perRow.uuidV4",
            "logical UUID column",
        ),
    ];

    for (label, generated_column, generator, expected_a, expected_b) in cases {
        let raw = serde_json::json!({
            "ir_version": 1,
            "name": format!("reject_{}", label.replace(' ', "_")),
            "ops": [
                {
                    "op": "createTable",
                    "name": "guarded_items",
                    "columns": [
                        { "name": "id", "type": "bigInt", "nullable": false },
                        generated_column
                    ],
                    "primaryKey": ["id"]
                },
                {
                    "op": "backfill",
                    "table": "guarded_items",
                    "name": "invalid_per_row_destination",
                    "cursorColumns": ["id"],
                    "cursorStability": { "mode": "guardUpdates" },
                    "batchSize": 1,
                    "set": { "generated": generator }
                }
            ]
        });
        let ir = no_inject_envelope_json(&raw.to_string());
        let error = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &support::no_inject("app"))
            .load_and_lower_guarded(
                &ir,
                APP,
                &registry(&[]),
                &LiveSchema::default(),
                &GuardConfig::from_policy(support::no_inject(PROJECT), SqlDialect::Sqlite),
            )
            .expect_err(label);
        let message = error.to_string();
        assert!(
            message.contains(expected_a) && message.contains(expected_b),
            "{label} must fail with the declared destination contract, got: {message}"
        );
    }

    let rows = be
        .actor()
        .query("SELECT id, generated FROM guarded_items")
        .await
        .expect("read validation sentinel");
    assert_eq!(
        rows,
        vec![vec![Some("1".into()), Some("unchanged".into())]],
        "destination validation must finish before a first batch can mutate any row"
    );
}

#[compio::test]
async fn insert_on_conflict_updates_and_does_nothing_on_real_sqlite() {
    let p = paths("ir_on_conflict");
    let be = backend(&p);
    be.actor()
        .set_mode(Mode::CreatorUp)
        .await
        .expect("enter creator mode");
    be.actor()
        .exec("CREATE TABLE status_codes (code INTEGER PRIMARY KEY, label TEXT NOT NULL)")
        .await
        .expect("create conflict target");
    be.actor()
        .exec("INSERT INTO status_codes (code, label) VALUES (200, 'seed')")
        .await
        .expect("seed conflict target");

    let ir = resolved_envelope_json(
        r#"{"ir_version":1,"name":"sqlite_on_conflict","irreversible":"updates a conflicting label without recording its pre-image","ops":[
          {"op":"insert","table":"status_codes","columns":["code","label"],
           "rows":[[200,"incoming"]],
           "onConflict":{"columns":["code"],"doUpdate":{"label":"updated"}}},
          {"op":"insert","table":"status_codes","columns":["code","label"],
           "rows":[[200,"must-not-apply"]],
           "onConflict":{"columns":["code"]}}
        ]}"#,
    );
    let author = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Sqlite,
        &support::confined_charter(),
    );
    let guard_cfg = GuardConfig::from_policy(support::no_inject(PROJECT), SqlDialect::Sqlite);
    let artifact = author
        .load_and_lower_guarded(
            &ir,
            APP,
            &registry(&[("status_codes", APP)]),
            &LiveSchema::default(),
            &guard_cfg,
        )
        .expect("both exact SQLite conflict forms lower");

    let engine = MigrationEngine::new();
    let outcome = engine
        .apply_plan(
            &artifact.plan.steps,
            Approval::None,
            &be,
            &exec_cfg(),
            "sqlite-on-conflict-test",
            LockMode::Acquire,
        )
        .await
        .expect("both exact SQLite conflict forms apply");
    assert_eq!(outcome.applied.applied.len(), 2);

    be.actor()
        .set_mode(Mode::CreatorUp)
        .await
        .expect("enter read mode");
    let rows = be
        .actor()
        .query("SELECT label FROM status_codes WHERE code = 200")
        .await
        .expect("read conflict result");
    assert_eq!(rows[0][0].as_deref(), Some("updated"));
}

#[compio::test]
async fn portable_scalar_and_date_functions_apply_on_hardened_sqlite() {
    let p = paths("ir_portable_functions");
    let be = backend(&p);
    be.actor()
        .set_mode(Mode::CreatorUp)
        .await
        .expect("enter creator mode");
    be.actor()
        .exec(
            "CREATE TABLE metrics (\
                id INTEGER PRIMARY KEY, x REAL NOT NULL, source TEXT NOT NULL, \
                happened_at TEXT NOT NULL, rounded REAL, floored REAL, ceiled REAL, \
                replaced TEXT, event_year INTEGER); \
             INSERT INTO metrics (id, x, source, happened_at) \
             VALUES (1, 12.75, 'a-b-a', '2026-07-15T12:30:00Z'), \
                    (2, -12.25, 'a-b-a', '2026-07-15T12:30:00Z'), \
                    (3, 1e20, 'a-b-a', '2026-07-15T12:30:00Z'), \
                    (4, -1e20, 'a-b-a', '2026-07-15T12:30:00Z')",
        )
        .await
        .expect("seed function target");

    let ir = resolved_envelope_json(
        r#"{"ir_version":1,"name":"sqlite_portable_functions","irreversible":"overwrites derived metric columns without recording their pre-images","ops":[
          {"op":"update","table":"metrics","set":{
            "rounded":{"node":"fnCall","fn":"round","args":[{"node":"colRef","name":"x"}]},
            "floored":{"node":"fnCall","fn":"floor","args":[{"node":"colRef","name":"x"}]},
            "ceiled":{"node":"fnCall","fn":"ceil","args":[{"node":"colRef","name":"x"}]},
            "replaced":{"node":"fnCall","fn":"replace","args":[
              {"node":"colRef","name":"source"},{"node":"literal","value":"a"},{"node":"literal","value":"z"}]},
            "event_year":{"node":"extract","field":"year","from":{"node":"colRef","name":"happened_at"}}
          }}
        ]}"#,
    );
    let artifact = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Sqlite,
        &support::confined_charter(),
    )
    .load_and_lower_guarded(
        &ir,
        APP,
        &registry(&[("metrics", APP)]),
        &LiveSchema::default(),
        &GuardConfig::from_policy(support::no_inject(PROJECT), SqlDialect::Sqlite),
    )
    .expect("portable function update lowers");

    MigrationEngine::new()
        .apply_plan(
            &artifact.plan.steps,
            Approval::None,
            &be,
            &exec_cfg(),
            "sqlite-function-test",
            LockMode::Acquire,
        )
        .await
        .expect("portable functions apply through the hardened connection");

    be.actor().set_mode(Mode::CreatorUp).await.unwrap();
    let rows = be
        .actor()
        .query(
            "SELECT rounded, floored, ceiled, replaced, event_year \
             FROM metrics WHERE id <= 2 ORDER BY id",
        )
        .await
        .expect("read function results");
    assert_eq!(
        rows,
        vec![
            vec![
                Some("13".into()),
                Some("12".into()),
                Some("13".into()),
                Some("z-b-z".into()),
                Some("2026".into()),
            ],
            vec![
                Some("-12".into()),
                Some("-13".into()),
                Some("-12".into()),
                Some("z-b-z".into()),
                Some("2026".into()),
            ]
        ]
    );

    let large_rows = be
        .actor()
        .query(
            "SELECT floored = x, ceiled = x \
             FROM metrics WHERE id > 2 ORDER BY id",
        )
        .await
        .expect("read large-number floor and ceil results");
    assert_eq!(
        large_rows,
        vec![
            vec![Some("1".into()), Some("1".into())],
            vec![Some("1".into()), Some("1".into())],
        ],
        "floor and ceil must not clamp finite SQLite REAL values outside i64"
    );
}

#[compio::test]
async fn byte_value_insert_persists_exact_blob_and_completed_journal_on_real_sqlite() {
    let p = paths("ir_byte_value");
    let be = backend(&p);
    be.actor()
        .set_mode(Mode::CreatorUp)
        .await
        .expect("enter creator mode");
    be.actor()
        .exec("CREATE TABLE files (payload BLOB NOT NULL)")
        .await
        .expect("create binary DML target");

    let ir = resolved_envelope_json(
        r#"{"ir_version":1,"name":"sqlite_byte_value","irreversible":"inserts payload bytes without recording an identifier for the inserted row","ops":[
          {"op":"insert","table":"files","columns":["payload"],
           "rows":[[{"bytes":"AAF/gP8="}]]}
        ]}"#,
    );
    let author = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Sqlite,
        &support::confined_charter(),
    );
    let guard_cfg = GuardConfig::from_policy(support::no_inject(PROJECT), SqlDialect::Sqlite);
    let artifact = author
        .load_and_lower_guarded(
            &ir,
            APP,
            &registry(&[("files", APP)]),
            &LiveSchema::default(),
            &guard_cfg,
        )
        .expect("a byteValue insert lowers for SQLite");

    let engine = MigrationEngine::new();
    let outcome = engine
        .apply_plan(
            &artifact.plan.steps,
            Approval::None,
            &be,
            &exec_cfg(),
            "sqlite-byte-value-test",
            LockMode::Acquire,
        )
        .await
        .expect("the byteValue insert applies on SQLite");
    assert_eq!(outcome.applied.applied.len(), 1);
    let version = &outcome.applied.applied[0];

    be.actor()
        .set_mode(Mode::CreatorUp)
        .await
        .expect("enter read mode");
    let rows = be
        .actor()
        .query("SELECT typeof(payload), hex(payload), length(payload) FROM files")
        .await
        .expect("read the stored binary value");
    assert_eq!(
        rows,
        vec![vec![
            Some("blob".to_string()),
            Some("00017F80FF".to_string()),
            Some("5".to_string()),
        ]],
        "SQLite must store the decoded bytes as a five-byte BLOB"
    );

    let journal = be.applied_sqlite().await.expect("read the SQLite journal");
    let entry = journal
        .iter()
        .find(|entry| &entry.version == version)
        .expect("the byteValue DML step is journaled");
    assert_eq!(
        entry.phase,
        zero_migrate::apply::journal::Phase::Completed,
        "the exact BLOB write and its completed journal event commit together"
    );
}

#[compio::test]
async fn byte_value_backfill_persists_exact_blob_on_real_sqlite() {
    let p = paths("ir_byte_backfill");
    let be = backend(&p);
    be.actor()
        .set_mode(Mode::CreatorUp)
        .await
        .expect("enter creator mode");
    be.actor()
        .exec(
            "CREATE TABLE files (id INTEGER PRIMARY KEY, payload BLOB); \
             INSERT INTO files (id, payload) VALUES (1, NULL), (2, NULL)",
        )
        .await
        .expect("create binary backfill target");

    let ir = resolved_envelope_json(
        r#"{"ir_version":1,"name":"sqlite_byte_backfill","irreversible":"overwrites payload bytes; the pre-image is not recorded","ops":[
          {"op":"backfill","table":"files","name":"fill_payload",
           "cursorColumns":["id"],"cursorStability":{"mode":"guardUpdates"},"batchSize":1,
           "set":{"payload":{"node":"literal","value":{"bytes":"AAF/gP8="}}}}
        ]}"#,
    );
    let artifact = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Sqlite,
        &support::confined_charter(),
    )
    .load_and_lower_guarded(
        &ir,
        APP,
        &registry(&[("files", APP)]),
        &LiveSchema::default(),
        &GuardConfig::from_policy(support::no_inject(PROJECT), SqlDialect::Sqlite),
    )
    .expect("a byteValue backfill lowers for SQLite");

    MigrationEngine::new()
        .apply_plan(
            &artifact.plan.steps,
            Approval::Approved,
            &be,
            &exec_cfg(),
            "sqlite-byte-backfill-test",
            LockMode::Acquire,
        )
        .await
        .expect("the byteValue backfill applies on SQLite");

    be.actor().set_mode(Mode::CreatorUp).await.unwrap();
    let rows = be
        .actor()
        .query("SELECT typeof(payload), hex(payload) FROM files ORDER BY id")
        .await
        .expect("read binary backfill results");
    assert_eq!(
        rows,
        vec![
            vec![Some("blob".into()), Some("00017F80FF".into())],
            vec![Some("blob".into()), Some("00017F80FF".into())],
        ]
    );
}

#[compio::test]
async fn fixed_decimal_create_and_insert_preserve_exact_text_on_real_sqlite() {
    let p = paths("ir_fixed_decimal");
    let be = backend(&p);
    let schema_ir = no_inject_envelope_json(
        r#"{"ir_version":1,"name":"sqlite_fixed_decimal_schema","ops":[
          {"op":"createTable","name":"ledger","columns":[
            {"name":"amount","type":{"decimal":{"precision":30,"scale":10}},"nullable":false}
          ]}
        ]}"#,
    );
    let data_ir = no_inject_envelope_json(
        r#"{"ir_version":1,"name":"sqlite_fixed_decimal_data","irreversible":"inserts a decimal row without recording an identifier for deletion","ops":[
          {"op":"insert","table":"ledger","columns":["amount"],
           "rows":[[{"decimal":"12345678901234567890.1234567890"}]]}
        ]}"#,
    );
    let guard_cfg = GuardConfig::from_policy(support::no_inject(PROJECT), SqlDialect::Sqlite);
    let charter = support::no_inject("app");
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &charter);
    let schema_artifact = author
        .load_and_lower_guarded(
            &schema_ir,
            APP,
            &registry(&[]),
            &LiveSchema::default(),
            &guard_cfg,
        )
        .expect("a fixed decimal table creation lowers on SQLite");

    MigrationEngine::new()
        .apply_plan(
            &schema_artifact.plan.steps,
            Approval::None,
            &be,
            &exec_cfg(),
            "sqlite-fixed-decimal-schema-test",
            LockMode::Acquire,
        )
        .await
        .expect("the fixed decimal schema plan applies on SQLite");

    let data_artifact = author
        .load_and_lower_guarded(
            &data_ir,
            APP,
            &registry(&[("ledger", APP)]),
            &LiveSchema::default(),
            &guard_cfg,
        )
        .expect("a fixed decimal insert lowers on SQLite");
    MigrationEngine::new()
        .apply_plan(
            &data_artifact.plan.steps,
            Approval::None,
            &be,
            &exec_cfg(),
            "sqlite-fixed-decimal-data-test",
            LockMode::Acquire,
        )
        .await
        .expect("the fixed decimal data plan applies on SQLite");

    be.actor()
        .set_mode(Mode::CreatorUp)
        .await
        .expect("enter read mode");
    let table_sql = be
        .actor()
        .query("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'ledger'")
        .await
        .expect("read the created table definition");
    assert!(
        table_sql
            .first()
            .and_then(|row| row.first())
            .and_then(Option::as_deref)
            .is_some_and(|sql| sql.contains("\"amount\" TEXT")),
        "the fixed decimal column must use SQLite TEXT storage: {table_sql:?}"
    );
    let rows = be
        .actor()
        .query("SELECT typeof(amount), amount FROM ledger")
        .await
        .expect("read the stored decimal text");
    assert_eq!(
        rows,
        vec![vec![
            Some("text".to_string()),
            Some("12345678901234567890.1234567890".to_string()),
        ]],
        "SQLite must not coerce a wide fixed decimal through REAL affinity"
    );
}

#[compio::test]
async fn mixed_data_plan_is_refused_before_insert_when_delete_and_backfill_are_unapproved() {
    let p = paths("mixed_data_approval_preflight");
    let be = backend(&p);
    be.actor()
        .set_mode(Mode::CreatorUp)
        .await
        .expect("enter creator mode");
    be.actor()
        .exec(
            "CREATE TABLE users (\
                id INTEGER PRIMARY KEY, \
                ready INTEGER NOT NULL\
            )",
        )
        .await
        .expect("create mixed-plan target");

    let ir = resolved_envelope_json(
        r#"{"ir_version":1,"name":"mixed_data_approval_preflight","irreversible":"deletes and overwrites rows without recording the deleted rows or prior values","ops":[
          {"op":"insert","table":"users","columns":["id","ready"],"rows":[[1,false]]},
          {"op":"delete","table":"users","where":{"node":"binOp","op":"eq",
            "lhs":{"node":"colRef","name":"id"},"rhs":{"node":"literal","value":999}}},
          {"op":"backfill","table":"users","name":"mark_users_ready",
            "cursorColumns":["id"],"cursorStability":{"mode":"guardUpdates"},"batchSize":100,
            "set":{"ready":{"node":"literal","value":true}}}
        ]}"#,
    );
    let author = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Sqlite,
        &support::confined_charter(),
    );
    let artifact = author
        .load_and_lower_guarded(
            &ir,
            APP,
            &registry(&[("users", APP)]),
            &LiveSchema::default(),
            &GuardConfig::from_policy(support::no_inject(PROJECT), SqlDialect::Sqlite),
        )
        .expect("the mixed data plan lowers");

    let result = MigrationEngine::new()
        .apply_plan(
            &artifact.plan.steps,
            Approval::None,
            &be,
            &exec_cfg(),
            "sqlite-approval-preflight-test",
            LockMode::Acquire,
        )
        .await;
    assert!(matches!(
        result,
        Err(DeclarativeApplyError::Plain(EngineError::ApprovalRequired))
    ));

    be.actor()
        .set_mode(Mode::CreatorUp)
        .await
        .expect("enter read mode");
    let rows = be
        .actor()
        .query("SELECT COUNT(*) FROM users")
        .await
        .expect("read target after refusal");
    assert_eq!(
        rows,
        vec![vec![Some("0".to_string())]],
        "the leading insert must not commit before a later approval gate"
    );
    assert!(
        be.applied_sqlite()
            .await
            .expect("read journal after refusal")
            .is_empty(),
        "no plan step may be journaled on approval refusal"
    );
}

// L7/M15: `date` is an honest portable column type. The SQLite leg stores it as
// TEXT affinity and must accept/apply it through the real IR load gate.
#[compio::test]
async fn ir_envelope_date_column_lowers_and_applies_on_sqlite() {
    let p = paths("ir_date_apply");
    let be = backend(&p);

    let ir = resolved_envelope_json(
        r#"{"ir_version":1,"name":"create_events","ops":[
        {"op":"createTable","name":"events","columns":[
            {"name":"happened_on","type":"date","nullable":false}
        ]}
    ]}"#,
    );

    let author = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Sqlite,
        &support::confined_charter(),
    );
    let migrations = author
        .load_and_lower(&ir, APP, &registry(&[]), &LiveSchema::default())
        .expect("date columns must validate and lower on SQLite");
    assert!(
        migrations
            .iter()
            .any(|m| m.up.contains("\"happened_on\" TEXT NOT NULL")),
        "SQLite date column must render with TEXT affinity: {migrations:#?}"
    );

    let engine = MigrationEngine::new();
    let guard_cfg = GuardConfig::from_policy(support::no_inject(PROJECT), SqlDialect::Sqlite);
    let plan = engine.plan(&migrations, &guard_cfg);
    assert!(
        plan.denied.is_empty(),
        "no denials on a date-column IR set: {:?}",
        plan.denied
    );
    engine
        .apply(&plan, Approval::None, &be, &exec_cfg(), "deploy-ir")
        .await
        .expect("apply the date-column IR on SQLite");

    let rows = be
        .actor()
        .query("SELECT type FROM pragma_table_info('events') WHERE name = 'happened_on'")
        .await
        .expect("pragma_table_info probe");
    assert_eq!(rows.len(), 1, "the date column must exist");
    assert_eq!(rows[0][0].as_deref(), Some("TEXT"));
}

// A LEGITIMATE portable string-literal column DEFAULT whose
// value contains the substring `;\n` (and a bare `;`) must lower CLEANLY through
// the PRODUCTION guarded path (`load_and_lower_guarded`) and APPLY on a real
// SQLite backend — the renderer's interior `;\n` (from `DEFAULT 'a;\nb'`) must NOT
// split the single CREATE statement. Pre-fix the textual fragment split broke the
// CREATE on the literal's `;\n`, tripping a guard denial / ReassemblyMismatch on
// a valid default. Post-fix the structural per-statement fragments keep the
// literal whole; the table materialises and a default-driven INSERT round-trips
// the embedded `;\n`. Driven through `apply_plan` (the shared orchestrator) over
// the guarded artifact's plan steps — the real deploy shape.
#[compio::test]
async fn ir_envelope_string_default_with_embedded_semicolon_newline_applies_on_sqlite() {
    let p = paths("ir_semicolon_default");
    let be = backend(&p);

    // The JSON `\n` escape yields the literal three-byte run `a ; \n b ; c`.
    let ir = resolved_envelope_json(
        r#"{"ir_version":1,"name":"create_docs","ops":[
        {"op":"createTable","name":"docs","columns":[
            {"name":"note","type":"text","nullable":false,
             "default":{"literal":{"value":"a;\nb;c"}}}
        ]}
    ]}"#,
    );

    // The REAL fail-closed gate + GUARDED lower (the production deploy entry).
    let author = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Sqlite,
        &support::confined_charter(),
    );
    let guard_cfg = GuardConfig::from_policy(support::no_inject(PROJECT), SqlDialect::Sqlite);
    let artifact = author
        .load_and_lower_guarded(&ir, APP, &registry(&[]), &LiveSchema::default(), &guard_cfg)
        .expect("a portable ;\\n string default must lower through the guarded path on SQLite");

    // Per-statement attribution survived: the createTable's CREATE is ONE fragment
    // carrying the whole default (the interior `;\n` did NOT split it).
    let create_frag = artifact
        .fragments
        .iter()
        .find(|f| f.op_index == 0 && f.sql.contains("CREATE TABLE"))
        .expect("a CREATE TABLE fragment for op #0");
    assert_eq!(create_frag.op_kind, "createTable");
    assert!(
        create_frag.sql.contains("DEFAULT 'a;\nb;c'"),
        "the whole string default (incl. its ;\\n) stays inside one fragment; got {:?}",
        create_frag.sql
    );

    // Apply the guarded artifact's plan on the real SQLite backend.
    let engine = MigrationEngine::new();
    let outcome = engine
        .apply_plan(
            &artifact.plan.steps,
            Approval::None,
            &be,
            &exec_cfg(),
            "deploy-ir",
            LockMode::Acquire,
        )
        .await
        .expect("apply the guarded ;\\n-default IR on SQLite");
    assert!(
        !outcome.applied.applied.is_empty(),
        "the IR migration must apply"
    );

    // The table exists and the stored CREATE SQL preserved the embedded `;\n`.
    let create_sql = be
        .actor()
        .query("SELECT sql FROM sqlite_master WHERE type='table' AND name='docs'")
        .await
        .expect("sqlite_master probe");
    assert_eq!(
        create_sql.len(),
        1,
        "the IR-created 'docs' table must exist on SQLite"
    );

    // The default really drives an INSERT: a row that omits `note` gets `a;\nb;c`.
    be.actor()
        .exec(
            "INSERT INTO docs (id, created_at, updated_at, version) \
             VALUES ('doc_1', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 1)",
        )
        .await
        .expect("insert a row relying on the column default");
    let rows = be
        .actor()
        .query("SELECT note FROM docs WHERE id = 'doc_1'")
        .await
        .expect("read back the defaulted row");
    assert_eq!(rows.len(), 1, "one row inserted");
    let note = rows[0][0].as_deref().expect("note is non-null").to_string();
    assert_eq!(
        note, "a;\nb;c",
        "the column default with its embedded ;\\n must apply verbatim"
    );
}

// HOSTILE (SQLite-specific) — an out-of-envelope `.splitPart` in a backfill
// SET against a SQLite target is refused by the gate (EXPR_NOT_PORTABLE) BEFORE
// any apply. `splitPart` is PG-expressible but out-of-envelope on SQLite, so
// the dialect-parameterized validate refuses it fail-closed.
#[compio::test]
async fn out_of_envelope_splitpart_refused_on_sqlite() {
    // An update whose SET applies a MULTI-CHAR-delim splitPart — in-envelope on PG,
    // out-of-envelope on SQLite. The gate is dialect-parameterized, so the SAME
    // artifact loads on PG and is REFUSED on SQLite (the gate runs BEFORE lower, so
    // the DML-not-yet-lowerable arm is never reached here).
    let ir = r#"{"ir_version":1,"name":"split_update","ops":[
        {"op":"update","table":"users","set":{
            "name":{"node":"fnSynth","fn":"splitPart","args":[
                {"node":"colRef","name":"full_name"},
                {"node":"literal","value":", "},
                {"node":"literal","value":1}
            ]}
        }}
    ]}"#;

    let mut live = BTreeSet::new();
    live.insert("users".to_string());
    let author = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &support::no_inject("app"));
    let err = author
        .load_and_lower(ir, APP, &registry(&[("users", APP)]), &(&live).into())
        .expect_err("an out-of-envelope splitPart must be refused on SQLite");
    match err {
        LoadAndLowerError::Load(zero_migrate::IrLoadError::Validate(ae)) => {
            assert_eq!(
                ae.code,
                zero_migrate::model::validate::CODE_EXPR_NOT_PORTABLE,
                "the SQLite-out-of-envelope splitPart is EXPR_NOT_PORTABLE; got {:?}",
                ae.code
            );
        }
        other => panic!("expected a fail-closed Load(Validate EXPR_NOT_PORTABLE), got: {other}"),
    }
}
