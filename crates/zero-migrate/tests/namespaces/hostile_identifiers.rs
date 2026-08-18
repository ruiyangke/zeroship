//! An identifier cannot escape its quoting, and a legal-but-hostile one survives
//! a real database unchanged.
//!
//! The engine renders identifiers into SQL text. That makes two properties
//! load-bearing, and neither had a test:
//!
//!   1. A name carrying the character that closes the dialect's own quoting must
//!      never leave a rendered statement able to run a second statement. There
//!      are TWO safe answers and the dialects give different ones: PostgreSQL
//!      refuses these outright, while SQLite and MySQL escape by DOUBLING the
//!      quote, which is the standard and correct escape. Asserting refusal
//!      everywhere would pin a mechanism rather than the property.
//!
//!      THE DANGEROUS CHARACTER IS DIALECT-SPECIFIC. A `"` is inert inside
//!      MySQL's backticks and a backtick is inert inside PostgreSQL's quotes, so
//!      each dialect must be attacked with the character that closes ITS OWN
//!      quoting. Counting `"` on every dialect — the first thing I wrote — reports
//!      MySQL as broken for rendering a perfectly safe `` `a"b` ``.
//!
//!   2. Names that are merely awkward — `;`, `--`, a space, non-ASCII — are legal
//!      inside a quoted identifier and must survive intact. This is the half that
//!      catches downstream damage rather than injection: a statement splitter that
//!      breaks on `;`, a comment stripper that eats `--`, an ASCII-only path that
//!      mangles `café`. Each would corrupt a table name without any injection
//!      being involved, and asserting on the rendered SQL alone would not see it.
//!      So this half applies to a REAL SQLite database and reads the name back out
//!      of `sqlite_master`.
//!
//! Measured before writing. PostgreSQL refuses `a"b`; SQLite renders `"a""b"`;
//! MySQL renders `` `a``b` `` for the backtick payload. `a;b`, `a--b`, `a b` and
//! `café` all apply and round-trip out of `sqlite_master` byte-identical, and the
//! injection payload leaves `victim` standing.

use crate::support;

use std::collections::BTreeMap;

use zero_migrate::apply::executor::LockMode;
use zero_migrate::render::step::PlanStep;
use zero_migrate::{
    Approval, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, MigrationEngine, SqlDialect,
    SqliteBackend,
};

const PROJECT: &str = "prj_ir";
const APP: &str = "app_ir";

/// Lower a single `createTable` whose table name is `raw`.
fn lower_create_table(raw: &str, dialect: SqlDialect) -> Result<Vec<String>, String> {
    let bytes = format!(
        r#"{{"ir_version":1,"name":"hostile","ops":[{{"op":"createTable","name":"{raw}","columns":[{{"name":"c0","type":"bigInt","nullable":false}}],"primaryKey":["c0"]}}]}}"#
    );
    let artifact = IrAuthor::new(PROJECT, APP, dialect, &support::confined_charter())
        .load_and_lower_guarded(
            &bytes,
            APP,
            &BTreeMap::new(),
            &LiveSchema::default(),
            &GuardConfig::from_policy(support::no_inject(PROJECT), dialect),
        )
        .map_err(|e| format!("{e:?}"))?;
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

/// Identifiers carrying the one character that can terminate a quoted identifier.
/// Escaped here for the JSON envelope; the identifier itself contains `"`.
const QUOTE_BEARING: &[(&str, &str)] = &[
    ("bare double quote", r#"a\"b"#),
    ("statement injection", r#"x\"); DROP TABLE victim; --"#),
];

#[test]
fn a_double_quote_is_refused_or_escaped_but_never_left_bare() {
    // TWO SAFE ANSWERS, and the dialects give different ones: PostgreSQL REFUSES
    // these outright, SQLite and MySQL ESCAPE by doubling the quote, which is the
    // standard and correct escape. Demanding refusal everywhere would assert a
    // MECHANISM; the property is that no rendered statement ever carries a bare
    // quote that could close the identifier.
    //
    // THE DANGEROUS CHARACTER IS DIALECT-SPECIFIC, which is the whole reason this
    // is a loop and not one assertion. A `"` is inert inside MySQL's backticks and
    // a backtick is inert inside PostgreSQL's quotes; each dialect is only at risk
    // from the character that closes ITS OWN quoting. Counting `"` everywhere —
    // which is what I first wrote — reports MySQL as broken for rendering a
    // perfectly safe `` `a"b` ``.
    //
    // EACH DIALECT'S OUTCOME IS PINNED, and that is load-bearing. Accepting
    // "refused OR escaped" from every dialect made this test unable to fail:
    // breaking the escaper turns the rendered SQL into something the fragment
    // guard rejects, which the permissive form scored as the refusal branch and
    // passed. A test that reads a BROKEN ESCAPER as a safe refusal is worse than
    // no test, so the dialect that is known to escape must still escape.
    for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite, SqlDialect::Mysql] {
        let (quote, must_escape) = match dialect {
            SqlDialect::Mysql => ('`', true),
            SqlDialect::Sqlite => ('"', true),
            SqlDialect::Postgres => ('"', false),
        };
        for (label, raw) in QUOTE_BEARING {
            // Reuse the payloads with the dialect's own quote substituted in, so
            // each dialect is attacked with the character that threatens it.
            // Substitute the dialect's quote in its JSON-ESCAPED form. Splicing a
            // bare `"` into the envelope makes the JSON itself invalid, and the
            // resulting parse error is indistinguishable from the engine refusing
            // the identifier — a fixture failure wearing the costume of a result.
            let json_form = if quote == '"' { r#"\""# } else { "`" };
            let hostile = raw.replace(r#"\""#, json_form);
            let outcome = lower_create_table(&hostile, dialect);
            let statements = match outcome {
                Ok(statements) => {
                    assert!(
                        must_escape,
                        "{label} on {dialect:?}: this dialect is recorded as REFUSING a \
                         {quote:?}-bearing identifier and it rendered one instead. Either \
                         the refusal was lost or the recorded behaviour is stale: {statements:?}"
                    );
                    statements
                }
                Err(refusal) => {
                    assert!(
                        !must_escape,
                        "{label} on {dialect:?}: this dialect is recorded as ESCAPING a \
                         {quote:?}-bearing identifier, and it refused instead. A broken \
                         escaper renders malformed SQL that the fragment guard rejects, \
                         which looks exactly like a deliberate refusal — that is why the \
                         outcome is pinned per dialect rather than accepting either: {refusal}"
                    );
                    continue;
                }
            };
            for sql in &statements {
                let doubled = format!("{quote}{quote}");
                let single_occurrences = sql.matches(quote).count();
                let doubled_occurrences = sql.matches(&doubled).count();
                assert!(
                    doubled_occurrences > 0,
                    "{label} on {dialect:?}: the identifier carries {quote:?}, the character \
                     that closes this dialect's quoting, and the rendered statement contains \
                     no doubled form of it. Either it was stripped or it is sitting bare and \
                     closing the identifier: {sql}"
                );
                assert_eq!(
                    single_occurrences % 2,
                    0,
                    "{label} on {dialect:?}: an odd number of {quote:?} means one of them \
                     closes the identifier and the rest of the name becomes syntax: {sql}"
                );
            }
        }
    }
}

#[compio::test]
async fn an_injecting_identifier_cannot_execute_a_second_statement() {
    // The decisive test, and the only one that can actually fail open: apply the
    // payload against a real database holding a table it tries to drop. Reading
    // the rendered SQL cannot answer this; executing it can.
    for (label, raw) in QUOTE_BEARING {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = SqliteBackend::open(
            &dir.path().join("inject.sqlite"),
            &dir.path().join("inject.migrations.sqlite"),
        )
        .expect("open the hardened sqlite backend");

        backend
            .actor()
            .query("CREATE TABLE victim (id integer primary key)")
            .await
            .expect("seed the table the payload tries to drop");

        let artifact = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &support::confined_charter())
            .load_and_lower_guarded(
                &format!(
                    r#"{{"ir_version":1,"name":"hostile","ops":[{{"op":"createTable","name":"{raw}","columns":[{{"name":"c0","type":"bigInt","nullable":false}}],"primaryKey":["c0"]}}]}}"#
                ),
                APP,
                &BTreeMap::new(),
                &LiveSchema::default(),
                &GuardConfig::from_policy(support::no_inject(PROJECT), SqlDialect::Sqlite),
            )
        .unwrap_or_else(|e| {
            // NOT a `continue`. SQLite is the dialect that ESCAPES these, so a
            // refusal here means the escaper broke and rendered something the
            // guard rejected. Skipping on refusal would absorb exactly the
            // regression this test exists to catch — the same way the sibling
            // test above once did.
            panic!(
                "{label}: SQLite must escape a quote-bearing identifier and lower it. \
                 A refusal here is a broken escaper producing malformed SQL, not a \
                 deliberate rejection: {e:?}"
            )
        });

        MigrationEngine::new()
            .apply_plan(
                &artifact.plan.steps,
                Approval::Approved,
                &backend,
                &ExecutorConfig::new(PROJECT, PROJECT, support::no_inject(PROJECT)),
                "identifier-injection",
                LockMode::Acquire,
            )
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "{label}: the statement must APPLY for this test to mean anything. \
                     Broken quoting that yields invalid SQL would fail here, leave \
                     `victim` standing, and pass a survival-only assertion vacuously: {e:?}"
                )
            });

        let tables = backend
            .actor()
            .query(
                "SELECT name FROM sqlite_master WHERE type='table' \
                 AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .await
            .expect("read the live table list");

        // BOTH halves are load-bearing. `victim` proves no second statement ran;
        // the payload table proves the first one did, which is what stops a
        // failed apply from passing this as a clean result.
        let names: Vec<String> = tables
            .iter()
            .filter_map(|row| row.first().cloned().flatten())
            .collect();
        assert!(
            names.iter().any(|n| n == "victim"),
            "{label}: the payload's second statement EXECUTED — `victim` is gone. The \
             identifier escaped its quoting and the rest of the name ran as SQL. \
             Tables: {names:?}"
        );
        assert!(
            names.iter().any(|n| n != "victim"),
            "{label}: the payload table was never created, so nothing was actually \
             tested. Tables: {names:?}"
        );
    }
}

/// Legal-but-awkward names, each a different way for a downstream text pass to
/// corrupt an identifier without any injection.
const AWKWARD: &[(&str, &str)] = &[
    ("semicolon", "a;b"),
    ("sql comment", "a--b"),
    ("space", "a b"),
    ("non-ascii", "café"),
];

#[test]
fn an_awkward_identifier_is_quoted_rather_than_refused() {
    for (label, raw) in AWKWARD {
        let statements = lower_create_table(raw, SqlDialect::Postgres)
            .unwrap_or_else(|e| panic!("{label}: a legal identifier must lower: {e}"));
        let sql = statements.join(" ");
        assert!(
            sql.contains(&format!("\"{raw}\"")),
            "{label}: the identifier must appear quoted verbatim in the rendered SQL, \
             so that `;` and `--` are inert text rather than syntax: {sql}"
        );
    }
}

#[compio::test]
async fn an_awkward_identifier_survives_a_real_database_unchanged() {
    for (label, raw) in AWKWARD {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = SqliteBackend::open(
            &dir.path().join("hostile.sqlite"),
            &dir.path().join("hostile.migrations.sqlite"),
        )
        .expect("open the hardened sqlite backend");

        let statements = lower_create_table(raw, SqlDialect::Sqlite)
            .unwrap_or_else(|e| panic!("{label}: a legal identifier must lower: {e}"));
        assert!(!statements.is_empty(), "{label}: nothing was rendered");

        let artifact = IrAuthor::new(PROJECT, APP, SqlDialect::Sqlite, &support::confined_charter())
            .load_and_lower_guarded(
                &format!(
                    r#"{{"ir_version":1,"name":"hostile","ops":[{{"op":"createTable","name":"{raw}","columns":[{{"name":"c0","type":"bigInt","nullable":false}}],"primaryKey":["c0"]}}]}}"#
                ),
                APP,
                &BTreeMap::new(),
                &LiveSchema::default(),
                &GuardConfig::from_policy(support::no_inject(PROJECT), SqlDialect::Sqlite),
            )
            .expect("lower for apply");

        MigrationEngine::new()
            .apply_plan(
                &artifact.plan.steps,
                Approval::Approved,
                &backend,
                &ExecutorConfig::new(PROJECT, PROJECT, support::no_inject(PROJECT)),
                "hostile-identifier",
                LockMode::Acquire,
            )
            .await
            .unwrap_or_else(|e| panic!("{label}: the migration must apply: {e:?}"));

        let rows = backend
            .actor()
            .query(
                "SELECT name FROM sqlite_master WHERE type='table' \
                 AND name NOT LIKE 'sqlite_%'",
            )
            .await
            .expect("read the live table list");

        assert_eq!(
            rows,
            vec![vec![Some((*raw).to_string())]],
            "{label}: the database must hold the identifier EXACTLY as authored. A \
             difference here is a downstream text pass corrupting the name — a splitter \
             breaking on `;`, a stripper eating `--`, an ASCII-only path mangling \
             non-ASCII — none of which is visible in the rendered SQL alone"
        );
    }
}
