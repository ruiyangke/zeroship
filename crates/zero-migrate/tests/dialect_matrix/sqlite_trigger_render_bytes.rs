//! The bytes `render_sqlite_trigger_op` emits, pinned, because NOTHING pinned them.
//!
//! # Why this file exists: a coverage hole found by a control that came out wrong
//!
//! `sqlite_trigger_quoting_reaches_postgres.rs` next door measured that all six
//! identifier quotes in the SQLite trigger render path resolved to
//! `PostgresDmlRenderer::quote_ident`. Fixing that had to be proven byte-neutral, and
//! the tree's own established technique for proving a render dependency is NEUTERING:
//! replace a renderer method with a marker and watch which suites go red.
//! `backends/sqlite.rs` records exactly that argument for the earlier half of the same
//! fix ("the SQLite-only `sqlite_engine` binary went from 148 passed / 7 failed to
//! 155 / 0 over the same 155 tests, so the dependency is gone rather than merely
//! re-covered").
//!
//! Run against the trigger path, that instrument was BLIND, and it took a two-sided
//! control to see it. Neutering `PostgresDmlRenderer::quote_ident` and running
//! `sqlite_engine`:
//!
//! ```text
//!   BEFORE the fix, PostgreSQL renderer neutered    156 passed / 0 failed
//!   AFTER  the fix, PostgreSQL renderer neutered    156 passed / 0 failed
//! ```
//!
//! The first line is the one that matters. Before the fix the SQLite trigger path
//! DEMONSTRABLY reached the PostgreSQL renderer, so a suite that exercised it had to
//! go red — and `sqlite_engine` did not move. It never renders a trigger: its five
//! `CREATE TRIGGER` occurrences are raw SQL strings handed straight to SQLite, not
//! `Op::CreateTrigger` lowered through `render_sqlite_trigger_op`. A green there was
//! evidence of nothing, and had the control been left out it would have been read as
//! proof the fix was safe.
//!
//! Widening the search: no offline test in this crate asserted the emitted bytes of a
//! rendered SQLite trigger at all. The path is reached by the live conformance sweep,
//! which needs a database and cannot say WHICH renderer spelled a quote. So the
//! coupling was not merely hard to see - the bytes it produced were unpinned, which is
//! the more useful finding and the one this file closes.
//!
//! # What is pinned, and why these particular ops
//!
//! One `createTrigger` and one `dropTrigger` chosen to touch EVERY identifier quote in
//! `render_sqlite_trigger_op` and its two helpers - all six of the calls the census
//! next door counts:
//!
//! | quote                          | reached by                                  |
//! |--------------------------------|---------------------------------------------|
//! | the trigger name               | `CREATE TRIGGER "t_audit"` and its `down`   |
//! | the trigger's target table     | `ON "orders"`                               |
//! | a body statement's table       | `sqlite_trigger_table_ref`, INSERT + UPDATE |
//! | a body INSERT's column list    | `("order_id", "at")`                        |
//! | a body UPDATE's assignment LHS | `SET "seen" = 1`                            |
//! | the DROP TRIGGER name          | `DROP TRIGGER IF EXISTS "t_audit"`          |
//!
//! Asserted as EXACT strings rather than `contains`, because the defect class this
//! guards is a changed QUOTING RULE - brackets, backticks, case folding, a dropped
//! quote - and every one of those survives a `contains` check on an unquoted
//! substring. The whole point is the bytes.
//!
//! # What this file does NOT prove
//!
//! It cannot, on its own, tell you WHICH renderer produced these bytes: PostgreSQL and
//! SQLite spell an identifier identically, which is the fact the census file's sibling
//! test pins and the reason the coupling survived. This file is the byte half; that
//! one is the provenance half. Neither replaces the other.

use zero_migrate::render::sql_preview::{render_ir_envelope_sql_statements, PreviewOpts};
use zero_migrate::schema::query::SqlDialect;

/// A trigger whose body reaches every identifier quote in the SQLite trigger path.
const TRIGGER_IR: &str = r#"{
  "ir_version": 1,
  "name": "sqlite_trigger_render_bytes",
  "ops": [
    {"op":"createTrigger","name":"t_audit","table":"orders",
     "timing":"after","events":["insert"],"forEach":"row",
     "action":{"kind":"body","statements":[
       {"stmt":"insert","table":"order_audit","columns":["order_id","at"],
        "rows":[[1,"2026-01-01T00:00:00Z"]]},
       {"stmt":"update","table":"order_audit","set":{"seen":1}}
     ]}},
    {"op":"dropTrigger","name":"t_audit","table":"orders","ifExists":true}
  ]
}"#;

fn statements(ir: &str, dialect: SqlDialect) -> Vec<String> {
    let opts = PreviewOpts {
        default_schema: "public".to_string(),
        owner_app: "app_sqlite_trigger_render_bytes".to_string(),
        effective_policy: crate::support::confined_charter(),
    };
    let (_name, statements) = render_ir_envelope_sql_statements(ir, dialect, &opts)
        .unwrap_or_else(|e| panic!("rendering the trigger IR on {dialect:?}: {e}"));
    statements
}

/// The exact SQLite trigger bytes. Every identifier is double-quoted, and every one of
/// them now comes from `SqliteDmlRenderer::quote_ident` by name rather than from
/// `PostgresDmlRenderer::quote_ident` by accident.
///
/// # If this goes red
///
/// Read the diff before touching this file. A changed QUOTE character or a lost quote
/// is the defect class this exists for and means a render path picked up the wrong
/// dialect's speller. A changed keyword, spacing, or statement order is a deliberate
/// render change and the constants below should be updated to match - but say so in
/// the commit, because these bytes go into `Checksum::of_ir`-adjacent migration text
/// that is already deployed.
#[test]
fn a_sqlite_trigger_renders_these_exact_bytes() {
    let rendered = statements(TRIGGER_IR, SqlDialect::Sqlite);

    let create = rendered
        .iter()
        .find(|s| s.contains("CREATE TRIGGER"))
        .unwrap_or_else(|| {
            panic!("no CREATE TRIGGER in the rendered SQLite output: {rendered:#?}")
        });
    assert_eq!(
        create,
        "CREATE TRIGGER \"t_audit\" AFTER INSERT ON \"orders\" FOR EACH ROW BEGIN \
         INSERT INTO \"order_audit\" (\"order_id\", \"at\") VALUES (1, '2026-01-01T00:00:00Z'); \
         UPDATE \"order_audit\" SET \"seen\" = 1; END;",
        "the rendered SQLite CREATE TRIGGER bytes moved.\n\nfull output: {rendered:#?}"
    );

    let drop = rendered
        .iter()
        .find(|s| s.contains("DROP TRIGGER"))
        .unwrap_or_else(|| panic!("no DROP TRIGGER in the rendered SQLite output: {rendered:#?}"));
    assert_eq!(
        drop, "DROP TRIGGER IF EXISTS \"t_audit\";",
        "the rendered SQLite DROP TRIGGER bytes moved.\n\nfull output: {rendered:#?}"
    );
}

/// Every identifier in the rendered trigger is double-quoted — stated as a RULE rather
/// than as the six literals above, so a seventh identifier slot added to the trigger
/// renderer and left unquoted fails here even if someone updates the exact-bytes
/// constants to match their new output.
///
/// This is the half that survives a legitimate render change. The test above pins
/// today's bytes and will be edited when the bytes legitimately move; this one pins
/// the invariant that no such edit is allowed to drop.
#[test]
fn no_identifier_in_a_rendered_sqlite_trigger_is_left_bare() {
    let rendered = statements(TRIGGER_IR, SqlDialect::Sqlite);
    let create = rendered
        .iter()
        .find(|s| s.contains("CREATE TRIGGER"))
        .expect("a CREATE TRIGGER statement");

    for ident in ["t_audit", "orders", "order_audit", "order_id", "at", "seen"] {
        let quoted = format!("\"{ident}\"");
        assert!(
            create.contains(quoted.as_str()),
            "the identifier `{ident}` is not double-quoted in the rendered SQLite \
             trigger. A backend whose `quote_ident` spells identifiers some other way \
             (brackets, backticks, bare) has been reached, or the quote was dropped \
             entirely.\n\n{create}"
        );
        assert!(
            !create.contains(format!(" {ident} ").as_str()),
            "the identifier `{ident}` also appears BARE in the rendered SQLite \
             trigger, so something emits it through a path that does not quote.\n\n\
             {create}"
        );
    }
}
