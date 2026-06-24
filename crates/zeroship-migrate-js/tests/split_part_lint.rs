//! PR6b — the JS-side `c.fn.splitPart` envelope LINT (§9), exercised through the
//! REAL V8 `op.*` recorder. The JS lint is the PEER of the Rust
//! `validate::check_split_part` gate: an in-envelope `splitPart` records cleanly,
//! and EVERY out-of-envelope shape throws a structured `EXPR_NOT_PORTABLE` at
//! record time — the AI loop's early structural feedback, enforced on BOTH sides
//! of the JS↔Rust boundary.

use zeroship_migrate_js::record_migration_to_json;

const OWNER: &str = "app_lint";

fn record(src: &str) -> Result<String, String> {
    record_migration_to_json(src, OWNER, "lint").map_err(|e| e.to_string())
}

/// In-envelope `c.fn.splitPart(c("name"), " ", 1)` records cleanly and emits a
/// `fnSynth(splitPart, …)` node (the §3.1 hero shape).
#[test]
fn in_envelope_split_part_records() {
    let src = r#"
        import { backfill, e } from "@zeroship/migrate";
        export const name = "split_name";
        export function up() {
            backfill("users", "id", 100,
                { first_name: e.splitPart(e.col("name"), " ", 1) },
                "split_name_bf");
        }
    "#;
    let json = record(src).expect("in-envelope splitPart records");
    assert!(json.contains("\"splitPart\""), "emits a fnSynth splitPart node: {json}");
    assert!(json.contains("\"fnSynth\""), "node is fnSynth: {json}");
}

/// EVERY out-of-envelope shape throws EXPR_NOT_PORTABLE at record time (the JS
/// lint). The error MESSAGE carries the code so the AI loop can self-correct.
#[test]
fn out_of_envelope_split_part_throws_expr_not_portable() {
    // (delim, n, label) — each out of the §9 envelope.
    let cases: &[(&str, &str, &str)] = &[
        (r#"", ""#, "1", "multi-char delimiter"),
        ("\"\u{00B7}\"", "1", "non-ASCII delimiter (U+00B7)"),
        (r#""""#, "1", "empty delimiter"),
        (r#"" ""#, "0", "n = 0"),
        (r#"" ""#, "-1", "negative n"),
        (r#"" ""#, "9", "n = 9 (just past the bound)"),
    ];
    for (delim, n, label) in cases {
        let src = format!(
            r#"
            import {{ backfill, e }} from "@zeroship/migrate";
            export const name = "bad";
            export function up() {{
                backfill("users", "id", 100,
                    {{ x: e.splitPart(e.col("name"), {delim}, {n}) }}, "bad_bf");
            }}
            "#
        );
        let err = record(&src).expect_err(&format!("out-of-envelope ({label}) must throw"));
        assert!(
            err.contains("EXPR_NOT_PORTABLE") || err.to_lowercase().contains("portable") || err.contains("splitPart"),
            "({label}) must surface the EXPR_NOT_PORTABLE lint; got: {err}"
        );
    }
}

/// A non-literal delimiter (a column ref passed where the delim is expected) is out
/// of envelope — the lint rejects a non-single-ASCII-string delim.
#[test]
fn non_literal_delim_throws() {
    let src = r#"
        import { backfill, e } from "@zeroship/migrate";
        export const name = "bad";
        export function up() {
            backfill("users", "id", 100,
                { x: e.splitPart(e.col("name"), e.col("sep"), 1) }, "bad_bf");
        }
    "#;
    let err = record(src).expect_err("a non-literal delim must throw");
    assert!(
        err.contains("EXPR_NOT_PORTABLE") || err.to_lowercase().contains("portable") || err.contains("splitPart"),
        "got: {err}"
    );
}

/// The `n=8` boundary is IN envelope (records); `n=9` is OUT (throws) — pins the
/// exact bound on the JS side, matching the Rust `SPLIT_PART_MAX_N`.
#[test]
fn n8_in_envelope_n9_out() {
    let ok = r#"
        import { backfill, e } from "@zeroship/migrate";
        export const name = "ok";
        export function up() {
            backfill("t", "id", 100, { x: e.splitPart(e.col("v"), ",", 8) }, "bf");
        }
    "#;
    record(ok).expect("n=8 is in-envelope");

    let bad = r#"
        import { backfill, e } from "@zeroship/migrate";
        export const name = "bad";
        export function up() {
            backfill("t", "id", 100, { x: e.splitPart(e.col("v"), ",", 9) }, "bf");
        }
    "#;
    record(bad).expect_err("n=9 is out of envelope");
}
