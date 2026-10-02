//! A hostile name cannot escape its quoting, and a legal-but-hostile one survives
//! a real database unchanged.
//!
//! The engine renders names into SQL text, and the classes of name it renders answer
//! the hostile ones differently:
//!
//!   1. A COLLECTION name (a `createTable`, `createPartition` or `renameTable`
//!      destination) and a declared TABLE COLUMN name must have the portable
//!      identifier shape - ASCII alphanumerics and underscores - on every dialect. The
//!      load gate refuses anything else before a renderer sees it
//!      (`validate_collection` and `validate_field_name`, called from
//!      `validate_op_authorized`). So `a"b`, `a;b` and `café` never reach a renderer
//!      as a table or column name, and the refusal is the property.
//!
//!   2. An INDEX name is bounded in length only, so it can still carry the character
//!      that closes the dialect's own identifier quoting. Every dialect escapes it by
//!      DOUBLING that character, which is the standard and correct escape.
//!
//!      THE DANGEROUS CHARACTER IS DIALECT-SPECIFIC. A `"` is inert inside MySQL's
//!      backticks and a backtick is inert inside PostgreSQL's and SQLite's double
//!      quotes, so each dialect is attacked with the character that closes ITS OWN
//!      quoting. Counting `"` on every dialect would report MySQL as broken for
//!      rendering a perfectly safe `` `a"b` ``.
//!
//!   3. A string LITERAL - a column default - carries `'`, the character that closes
//!      a SQL string, and is escaped by doubling it.
//!
//! Names that are merely awkward - `;`, `--`, a space, non-ASCII - are legal inside a
//! quoted index name and must survive intact. That half catches downstream damage
//! rather than injection: a statement splitter that breaks on `;`, a comment stripper
//! that eats `--`, an ASCII-only path that mangles `café`. Asserting on the rendered
//! SQL alone would not see it, so those arms apply to a REAL SQLite database and read
//! the name back out of `sqlite_master`.

use crate::support;

use std::collections::BTreeMap;

use serde_json::{json, Value};
use zeroship_migrate::apply::executor::LockMode;
use zeroship_migrate::render::step::PlanStep;
use zeroship_migrate::{
    Approval, DialectId, ExecutorConfig, GuardConfig, IrAuthor, IrLoadError, LiveSchema,
    LoadAndLowerGuardedError, MigrationEngine,
};
use zeroship_migrate_ir::validate::CODE_OP_INVALID;
use zeroship_migrate_sqlite::backend::Mode;
use zeroship_migrate_sqlite::SqliteBackend;

const PROJECT: &str = "prj_ir";
const APP: &str = "app_ir";

/// The three shipping dialects, each with the character that closes its identifier
/// quoting.
fn dialects() -> [(DialectId, char); 3] {
    [
        (zeroship_migrate_postgres::DIALECT, '"'),
        (zeroship_migrate_sqlite::DIALECT, '"'),
        (zeroship_migrate_mysql::DIALECT, '`'),
    ]
}

/// Names carrying a placeholder `Q` for the character that terminates a quoted
/// identifier. Each dialect substitutes its own quote, so each is attacked with the
/// character that threatens it.
const QUOTE_BEARING: &[(&str, &str)] = &[
    ("bare quote", "aQb"),
    ("statement injection", "xQ); DROP TABLE victim; --"),
];

/// Legal-but-awkward names, each a different way for a downstream text pass to
/// corrupt a name without any injection.
const AWKWARD: &[(&str, &str)] = &[
    ("semicolon", "a;b"),
    ("sql comment", "a--b"),
    ("space", "a b"),
    ("non-ascii", "café"),
];

/// String literals carrying `'`, the character that closes a SQL string.
const LITERAL_BEARING: &[(&str, &str)] = &[
    ("bare apostrophe", "a'b"),
    ("statement injection", "x'); DROP TABLE victim; --"),
];

fn envelope(ops: &[Value]) -> String {
    json!({ "ir_version": 1, "name": "hostile", "ops": ops }).to_string()
}

/// `createTable` with a bigint key `c0` and one bounded string column. Bounded so a
/// MySQL index over it is legal.
fn create_table(table: &str, column: &str, default: Option<&str>) -> Value {
    let mut string_column = json!({
        "name": column,
        "type": { "string": { "length": 64 } },
        "nullable": true,
    });
    if let Some(value) = default {
        string_column["default"] = json!({ "literal": { "value": value } });
    }
    json!({
        "op": "createTable",
        "name": table,
        "columns": [
            { "name": "c0", "type": "bigInt", "nullable": false },
            string_column,
        ],
        "primaryKey": ["c0"],
    })
}

fn create_index(name: &str) -> Value {
    json!({
        "op": "createIndex",
        "name": name,
        "table": "t",
        "columns": [{ "kind": "column", "name": "c1" }],
    })
}

/// Lower `ops` through the shipped guarded load-and-lower path, returning the
/// rendered DDL statements in plan order.
fn lower(ops: &[Value], dialect: &DialectId) -> Result<Vec<String>, LoadAndLowerGuardedError> {
    let artifact = IrAuthor::new(
        zeroship_migrate::shipping_vendors(),
        PROJECT,
        APP,
        dialect,
        &support::no_inject(PROJECT),
    )
    .load_and_lower_guarded(
        &envelope(ops),
        APP,
        &BTreeMap::new(),
        &LiveSchema::default(),
        &GuardConfig::from_policy(support::no_inject(PROJECT), (*dialect).clone(), PROJECT),
    )?;
    Ok(artifact
        .plan
        .steps
        .iter()
        .filter_map(|step| match step {
            PlanStep::Ddl(migration) => Some(migration.up.clone()),
            _ => None,
        })
        .collect())
}

/// The refusal must be the load gate's structural refusal of op 0, carrying the reason
/// the engine's own identifier validator gives for this name. Comparing against the
/// validator's answer, rather than accepting any error, is what stops a broken fixture
/// or an unrelated refusal from passing as the identifier being refused.
fn assert_refused_by_the_identifier_gate(
    label: &str,
    dialect: &DialectId,
    outcome: Result<Vec<String>, LoadAndLowerGuardedError>,
    expected_reason: &str,
) {
    match outcome {
        Err(LoadAndLowerGuardedError::Load(IrLoadError::Validate(error))) => {
            assert_eq!(
                (error.code.as_str(), error.op_index, error.reason.as_str()),
                (CODE_OP_INVALID, 0, expected_reason),
                "{label} on {dialect:?}: refused, but not by the identifier gate: {error:?}"
            );
        }
        Err(other) => panic!(
            "{label} on {dialect:?}: refused at the wrong layer. The identifier gate runs at \
             load, before any renderer or guard sees the name: {other:?}"
        ),
        Ok(statements) => panic!(
            "{label} on {dialect:?}: a name outside the portable identifier shape was \
             RENDERED instead of refused: {statements:?}"
        ),
    }
}

/// Every hostile name the identifier gate must refuse, decoded, with its label.
fn non_portable_names() -> Vec<(String, String)> {
    let mut names = Vec::new();
    for (label, raw) in QUOTE_BEARING {
        for quote in ['"', '`'] {
            names.push((format!("{label} ({quote})"), raw.replace('Q', &quote.to_string())));
        }
    }
    for (label, raw) in AWKWARD {
        names.push(((*label).to_string(), (*raw).to_string()));
    }
    names
}

#[test]
fn a_collection_name_outside_the_portable_shape_is_refused_on_every_dialect() {
    let vendors = zeroship_migrate::shipping_vendors();
    let names = non_portable_names();
    assert!(!names.is_empty(), "no hostile names were exercised");
    for (dialect, _) in dialects() {
        let dialect = &dialect;
        // CONTROL. The same envelope with a portable name lowers, so a refusal below
        // is about the name rather than the fixture.
        let control = lower(&[create_table("plain_t", "c1", None)], dialect)
            .unwrap_or_else(|e| panic!("{dialect:?}: the portable control must lower: {e:?}"));
        assert!(
            control.iter().any(|sql| sql.contains("plain_t")),
            "{dialect:?}: the control rendered no statement naming its table: {control:?}"
        );

        for (label, name) in &names {
            let expected = zeroship_migrate::schema::query::validate_collection(vendors, name)
                .expect_err("every hostile name is outside the portable collection shape")
                .to_string();
            assert_refused_by_the_identifier_gate(
                label,
                dialect,
                lower(&[create_table(name, "c1", None)], dialect),
                &expected,
            );
        }
    }
}

#[test]
fn a_column_name_outside_the_portable_shape_is_refused_on_every_dialect() {
    let vendors = zeroship_migrate::shipping_vendors();
    let names = non_portable_names();
    assert!(!names.is_empty(), "no hostile names were exercised");
    for (dialect, _) in dialects() {
        let dialect = &dialect;
        let control = lower(&[create_table("t", "plain_c", None)], dialect)
            .unwrap_or_else(|e| panic!("{dialect:?}: the portable control must lower: {e:?}"));
        assert!(
            control.iter().any(|sql| sql.contains("plain_c")),
            "{dialect:?}: the control rendered no statement naming its column: {control:?}"
        );

        for (label, name) in &names {
            let expected = zeroship_migrate::schema::query::validate_field_name(vendors, name)
                .expect_err("every hostile name is outside the portable field-name shape")
                .to_string();
            assert_refused_by_the_identifier_gate(
                label,
                dialect,
                lower(&[create_table("t", name, None)], dialect),
                &expected,
            );
        }
    }
}

/// The rendered statement that creates the index, which is the only statement whose
/// text carries the hostile name.
fn create_index_statement(label: &str, dialect: &DialectId, statements: &[String]) -> String {
    let found: Vec<&String> = statements
        .iter()
        .filter(|sql| sql.contains("CREATE INDEX"))
        .collect();
    assert_eq!(
        found.len(),
        1,
        "{label} on {dialect:?}: expected exactly one CREATE INDEX statement: {statements:?}"
    );
    found[0].clone()
}

#[test]
fn an_index_name_carrying_the_dialect_quote_is_escaped_on_every_dialect() {
    // Every dialect ESCAPES here; none refuses. A refusal would be a broken escaper
    // rendering malformed SQL that the fragment guard then rejected, which looks
    // exactly like a deliberate refusal - so a refusal fails this test rather than
    // passing as the safe outcome.
    for (dialect, quote) in dialects() {
        let dialect = &dialect;
        for (label, raw) in QUOTE_BEARING {
            let name = raw.replace('Q', &quote.to_string());
            let statements =
                lower(&[create_table("t", "c1", None), create_index(&name)], dialect)
                    .unwrap_or_else(|e| {
                        panic!(
                            "{label} on {dialect:?}: an index name is bounded in length only, \
                             so a {quote:?}-bearing one must be escaped and rendered: {e:?}"
                        )
                    });
            let sql = create_index_statement(label, dialect, &statements);
            let doubled = format!("{quote}{quote}");
            let escaped_name = name.replace(quote, &doubled);
            assert!(
                sql.contains(&format!("{quote}{escaped_name}{quote}")),
                "{label} on {dialect:?}: the index name must appear quoted with every \
                 {quote:?} doubled. Anything else is either stripped or sitting bare and \
                 closing the identifier: {sql}"
            );
            assert_eq!(
                sql.matches(quote).count() % 2,
                0,
                "{label} on {dialect:?}: an odd number of {quote:?} means one of them closes \
                 the identifier and the rest of the name becomes syntax: {sql}"
            );
        }
    }
}

/// Open a fresh hardened SQLite database holding the table a payload tries to drop.
async fn sqlite_with_victim(dir: &tempfile::TempDir, file: &str) -> SqliteBackend {
    let backend = SqliteBackend::open(&dir.path().join(file))
        .expect("open the hardened sqlite backend");
    backend
        .actor()
        .query("CREATE TABLE victim (id integer primary key)")
        .await
        .expect("seed the table the payload tries to drop");
    backend
}

/// Apply `ops` to `backend` through the shipped engine. The apply must succeed: broken
/// quoting that yields invalid SQL would fail here, leave `victim` standing, and pass a
/// survival-only assertion vacuously.
async fn apply_on_sqlite(label: &str, backend: &SqliteBackend, ops: &[Value]) {
    let artifact = IrAuthor::new(
        zeroship_migrate::shipping_vendors(),
        PROJECT,
        APP,
        &zeroship_migrate_sqlite::DIALECT,
        &support::no_inject(PROJECT),
    )
    .load_and_lower_guarded(
        &envelope(ops),
        APP,
        &BTreeMap::new(),
        &LiveSchema::default(),
        &GuardConfig::from_policy(
            support::no_inject(PROJECT),
            zeroship_migrate_sqlite::DIALECT,
            PROJECT,
        ),
    )
    .unwrap_or_else(|e| panic!("{label}: the envelope must lower on SQLite: {e:?}"));
    MigrationEngine::new(zeroship_migrate::shipping_vendors())
        .apply_plan(
            &artifact.plan.steps,
            Approval::Approved,
            backend,
            &ExecutorConfig::new(PROJECT, PROJECT, support::no_inject(PROJECT)),
            "hostile-name",
            LockMode::Acquire,
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: the migration must apply: {e:?}"));
}

/// Every creator-visible object of `kind` in the app file. The engine's own journal
/// tables carry the platform-reserved `__zeroship` prefix, which no creator
/// collection may claim, so leaving them out cannot hide a payload's table.
async fn names_of(backend: &SqliteBackend, kind: &str) -> Vec<String> {
    backend
        .actor()
        .query(&format!(
            "SELECT name FROM sqlite_master WHERE type='{kind}' \
             AND name NOT LIKE 'sqlite_%' AND substr(name, 1, 10) <> '__zeroship' \
             ORDER BY name"
        ))
        .await
        .expect("read sqlite_master")
        .iter()
        .filter_map(|row| row.first().cloned().flatten())
        .collect()
}

#[compio::test]
async fn an_injecting_index_name_cannot_execute_a_second_statement() {
    // The decisive arm, and the only one that can actually fail open: apply the
    // payload against a real database holding a table it tries to drop. Reading the
    // rendered SQL cannot answer this; executing it can.
    for (label, raw) in QUOTE_BEARING {
        let name = raw.replace('Q', "\"");
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = sqlite_with_victim(&dir, "inject.sqlite").await;

        apply_on_sqlite(
            label,
            &backend,
            &[create_table("t", "c1", None), create_index(&name)],
        )
        .await;

        // BOTH halves are load-bearing. `victim` proves no second statement ran; the
        // index under its exact name proves the first one did, which is what stops a
        // failed or mangled apply from passing this as a clean result.
        assert_eq!(
            names_of(&backend, "table").await,
            vec!["t".to_string(), "victim".to_string()],
            "{label}: the payload's second statement EXECUTED or the payload table is \
             missing. The name escaped its quoting and the rest of it ran as SQL"
        );
        assert_eq!(
            names_of(&backend, "index").await,
            vec![name.clone()],
            "{label}: the database must hold the index under exactly the authored name"
        );
    }
}

#[test]
fn an_awkward_index_name_is_quoted_verbatim_on_every_dialect() {
    for (dialect, quote) in dialects() {
        let dialect = &dialect;
        for (label, raw) in AWKWARD {
            let statements = lower(&[create_table("t", "c1", None), create_index(raw)], dialect)
                .unwrap_or_else(|e| {
                    panic!("{label} on {dialect:?}: a legal index name must lower: {e:?}")
                });
            let sql = create_index_statement(label, dialect, &statements);
            assert!(
                sql.contains(&format!("{quote}{raw}{quote}")),
                "{label} on {dialect:?}: the index name must appear quoted verbatim, so that \
                 `;` and `--` are inert text rather than syntax: {sql}"
            );
        }
    }
}

#[compio::test]
async fn an_awkward_index_name_survives_a_real_database_unchanged() {
    for (label, raw) in AWKWARD {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = SqliteBackend::open(&dir.path().join("awkward.sqlite"))
            .expect("open the hardened sqlite backend");

        apply_on_sqlite(
            label,
            &backend,
            &[create_table("t", "c1", None), create_index(raw)],
        )
        .await;

        assert_eq!(
            names_of(&backend, "index").await,
            vec![(*raw).to_string()],
            "{label}: the database must hold the index name EXACTLY as authored. A \
             difference here is a downstream text pass corrupting the name - a splitter \
             breaking on `;`, a stripper eating `--`, an ASCII-only path mangling \
             non-ASCII - none of which is visible in the rendered SQL alone"
        );
    }
}

#[test]
fn a_default_literal_carrying_an_apostrophe_is_escaped_on_every_dialect() {
    for (dialect, _) in dialects() {
        let dialect = &dialect;
        for (label, raw) in LITERAL_BEARING {
            let statements = lower(&[create_table("t", "c1", Some(raw))], dialect)
                .unwrap_or_else(|e| {
                    panic!("{label} on {dialect:?}: a string default must lower: {e:?}")
                });
            let sql = statements.join("\n");
            let escaped = raw.replace('\'', "''");
            assert!(
                sql.contains(&format!("'{escaped}'")),
                "{label} on {dialect:?}: the default must appear as one SQL string with \
                 every apostrophe doubled: {sql}"
            );
        }
    }
}

#[compio::test]
async fn an_injecting_default_literal_is_stored_verbatim_and_runs_nothing() {
    for (label, raw) in LITERAL_BEARING {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = sqlite_with_victim(&dir, "literal.sqlite").await;

        apply_on_sqlite(label, &backend, &[create_table("t", "c1", Some(raw))]).await;

        assert_eq!(
            names_of(&backend, "table").await,
            vec!["t".to_string(), "victim".to_string()],
            "{label}: the literal escaped its quoting and the rest of it ran as SQL"
        );
        backend
            .actor()
            .set_mode(Mode::CreatorUp)
            .await
            .expect("switch to creator mode for the probe row");
        backend
            .actor()
            .query("INSERT INTO t (c0) VALUES (1)")
            .await
            .expect("insert a row that takes the default");
        let stored = backend
            .actor()
            .query("SELECT c1 FROM t WHERE c0 = 1")
            .await
            .expect("read the defaulted value back");
        assert_eq!(
            stored,
            vec![vec![Some((*raw).to_string())]],
            "{label}: the row must store the authored default byte for byte"
        );
    }
}
