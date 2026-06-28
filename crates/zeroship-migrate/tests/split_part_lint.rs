//! PR6b — the JS-side `c.fn.splitPart` GRAMMAR lint (§9), exercised through the
//! REAL V8 `op.*` recorder. The record-time JS recorder is DIALECT-NEUTRAL, so it
//! enforces only the dialect-NEUTRAL grammar (a non-empty string-literal delimiter,
//! a positive-integer literal `n`) — the subset that is broken on BOTH backends.
//!
//! The portability ENVELOPE (single-ASCII delimiter, `1 <= n <= 8`) is dialect-
//! gated and is therefore DEFERRED to the authoritative Rust `validate::
//! check_split_part` (admit on a Postgres target via `dialect_scope=PgOnly`,
//! §2.4.1/§9; reject on a SQLite target). The builder MUST be able to CONSTRUCT an
//! out-of-envelope node so the documented PgOnly escape is reachable end-to-end
//! (the companion to the Rust render-path fix that emits native `split_part` on PG).

use zeroship_migrate::frontend::record_migration_to_json_unsandboxed;

const OWNER: &str = "app_lint";

fn record(src: &str) -> Result<String, String> {
    record_migration_to_json_unsandboxed(src, OWNER, "lint").map_err(|e| e.to_string())
}

/// In-envelope `c.fn.splitPart(c("name"), " ", 1)` records cleanly and emits a
/// `fnSynth(splitPart, …)` node (the §3.1 hero shape).
#[test]
fn in_envelope_split_part_records() {
    let src = r#"
        import { table } from "@zeroship/migrate";
        export const name = "split_name";
        export function up() {
            table("users").backfill({
                set: { first_name: (c) => c.fn.splitPart(c("name"), " ", 1) },
                cursorColumn: "id",
                batchSize: 100,
                name: "split_name_bf",
            });
        }
    "#;
    let json = record(src).expect("in-envelope splitPart records");
    assert!(json.contains("\"splitPart\""), "emits a fnSynth splitPart node: {json}");
    assert!(json.contains("\"fnSynth\""), "node is fnSynth: {json}");
}

/// An OUT-of-envelope (but grammar-valid) splitPart RECORDS at JS time — it is a
/// valid `dialect_scope=PgOnly` node (PG's native split_part is multi-char/any-n
/// capable). The dialect-aware verdict is deferred to the Rust validator; the JS
/// recorder must NOT block construction (else the PgOnly escape is unreachable).
#[test]
fn out_of_envelope_grammar_valid_split_part_records() {
    // (delim, n, label) — grammar-valid but out of the SQLite §9 envelope.
    let cases: &[(&str, &str, &str)] = &[
        (r#"", ""#, "1", "multi-char delimiter"),
        ("\"\u{00B7}\"", "1", "non-ASCII delimiter (U+00B7)"),
        (r#"" ""#, "9", "n = 9 (past the SQLite unroll bound)"),
    ];
    for (delim, n, label) in cases {
        let src = format!(
            r#"
            import {{ table }} from "@zeroship/migrate";
            export const name = "pgonly";
            export function up() {{
                table("users").backfill({{
                    set: {{ x: (c) => c.fn.splitPart(c("name"), {delim}, {n}) }},
                    cursorColumn: "id",
                    batchSize: 100,
                    name: "pgonly_bf",
                }});
            }}
            "#
        );
        let json = record(&src)
            .unwrap_or_else(|e| panic!("out-of-envelope ({label}) is a valid PgOnly node, must record: {e}"));
        assert!(
            json.contains("\"splitPart\"") && json.contains("\"fnSynth\""),
            "({label}) records a fnSynth splitPart node: {json}"
        );
    }
}

/// A GRAMMAR-broken splitPart (broken on BOTH dialects) throws EXPR_NOT_PORTABLE at
/// record time: an empty delimiter, a non-positive `n`, a non-integer `n`. These can
/// never render on any backend, so the JS lint rejects them early.
#[test]
fn grammar_broken_split_part_throws_expr_not_portable() {
    let cases: &[(&str, &str, &str)] = &[
        (r#""""#, "1", "empty delimiter"),
        (r#"" ""#, "0", "n = 0"),
        (r#"" ""#, "-1", "negative n"),
        (r#"" ""#, "1.5", "non-integer n"),
    ];
    for (delim, n, label) in cases {
        let src = format!(
            r#"
            import {{ table }} from "@zeroship/migrate";
            export const name = "bad";
            export function up() {{
                table("users").backfill({{
                    set: {{ x: (c) => c.fn.splitPart(c("name"), {delim}, {n}) }},
                    cursorColumn: "id",
                    batchSize: 100,
                    name: "bad_bf",
                }});
            }}
            "#
        );
        let err = record(&src).expect_err(&format!("grammar-broken ({label}) must throw"));
        assert!(
            err.contains("EXPR_NOT_PORTABLE") || err.to_lowercase().contains("portable") || err.contains("splitPart"),
            "({label}) must surface the EXPR_NOT_PORTABLE lint; got: {err}"
        );
    }
}

/// A non-string delimiter (a column ref passed where the delim is expected) is a
/// grammar violation — the lint rejects a non-string-literal delim on both dialects.
#[test]
fn non_string_delim_throws() {
    let src = r#"
        import { table } from "@zeroship/migrate";
        export const name = "bad";
        export function up() {
            table("users").backfill({
                set: { x: (c) => c.fn.splitPart(c("name"), c("sep"), 1) },
                cursorColumn: "id",
                batchSize: 100,
                name: "bad_bf",
            });
        }
    "#;
    let err = record(src).expect_err("a non-string delim must throw");
    assert!(
        err.contains("EXPR_NOT_PORTABLE") || err.to_lowercase().contains("portable") || err.contains("splitPart"),
        "got: {err}"
    );
}
