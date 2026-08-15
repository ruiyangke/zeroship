//! Two indexes sharing a name in one `createTable` are refused at the gate.
//!
//! This is a FAIL-OPEN, not a late verdict, which makes it worse than the
//! self-rename and duplicate-column findings that preceded it. Those reached the
//! server and were rejected loudly. This one SUCCEEDS.
//!
//! Measured. A `createTable` declaring two indexes both named `ix`, one on `(c)`
//! and one on `(d)`, lowers to
//!
//!     CREATE INDEX IF NOT EXISTS "ix" ON "prj_ir"."a" ("c")
//!     CREATE INDEX IF NOT EXISTS "ix" ON "prj_ir"."a" ("d")
//!
//! and PostgreSQL answers the second with
//!
//!     NOTICE:  relation "ix" already exists, skipping
//!
//! which is a NOTICE, not an error. The apply succeeds. The final schema carries
//! ONE index, on `(c)`. The index the author declared on `(d)` does not exist,
//! nothing failed, and nothing anywhere says so.
//!
//! `IF NOT EXISTS` is right for idempotent re-application — it is what makes a
//! re-run a no-op instead of an error — and it is exactly what turns this
//! authoring mistake into silence.
//!
//! It is decidable from the operation alone: two entries of one `indexes` list
//! share a name. One pass over one list, the same shape as the duplicate-column
//! check on UNIQUE constraints.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn verdict(op: &str, dialect: Dialect) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{op}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, dialect, &[]).map_err(|e| format!("{}: {}", e.code, e.reason))
}

const TWO_INDEXES_ONE_NAME: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c","type":"int","nullable":false},{"name":"d","type":"int","nullable":true}],"primaryKey":["c"],"indexes":[{"name":"ix","columns":[{"kind":"column","name":"c"}]},{"name":"ix","columns":[{"kind":"column","name":"d"}]}]}"#;

#[test]
fn two_indexes_sharing_a_name_are_refused_on_every_dialect() {
    for dialect in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
        let refusal = verdict(TWO_INDEXES_ONE_NAME, dialect).expect_err(&format!(
            "{dialect:?}: both indexes lower to CREATE INDEX IF NOT EXISTS under the \
             same name, so the second is SKIPPED with a notice and the apply \
             succeeds. The author declared an index that does not exist afterwards \
             and nothing reports it"
        ));
        assert!(
            refusal.to_lowercase().contains("index"),
            "{dialect:?}: the refusal must name the index as the problem: {refusal}"
        );
    }
}

#[test]
fn two_indexes_with_distinct_names_are_still_allowed() {
    // The control. Refusing every multi-index createTable would satisfy the test
    // above while breaking ordinary migrations.
    for dialect in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
        verdict(
            r#"{"op":"createTable","name":"a","columns":[{"name":"c","type":"int","nullable":false},{"name":"d","type":"int","nullable":true}],"primaryKey":["c"],"indexes":[{"name":"ix_c","columns":[{"kind":"column","name":"c"}]},{"name":"ix_d","columns":[{"kind":"column","name":"d"}]}]}"#,
            dialect,
        )
        .unwrap_or_else(|e| panic!("{dialect:?}: two distinctly named indexes must pass: {e}"));
    }
}

#[test]
fn one_index_is_still_allowed() {
    // The narrower control: a single index must not be caught by an
    // over-eager duplicate scan.
    verdict(
        r#"{"op":"createTable","name":"a","columns":[{"name":"c","type":"int","nullable":false}],"primaryKey":["c"],"indexes":[{"name":"ix","columns":[{"kind":"column","name":"c"}]}]}"#,
        Dialect::Postgres,
    )
    .expect("a single index must pass");
}
