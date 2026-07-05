//! PR3 — the FULL `@zeroship/migrate` JS op-builder surface (§3.2/§3.3.1),
//! exercised through the REAL V8 `op.*` recorder. Each test pins one PR3 behavior
//! and would FAIL against the PR1 skeletal builder (which had no `t.*` lexicon, no
//! fluent `(c) => Expr` builder, no `(table, spec)` adders, no determinism lint,
//! and no `OP_OUTSIDE_RECORDER` guard).
//!
//! These complement `op_round_trip.rs` (the golden-corpus value-equality gate over
//! the full fluent fixtures) with targeted, single-behavior regression assertions.

use serde_json::{json, Value};
use zeroship_migrate::frontend::{lint_migration_determinism, record_migration_to_ir_unsandboxed};
use zeroship_migrate::{CanonicalOpList, Checksum, MigrationFlags};

const OWNER: &str = "app_pr3";

/// Record a migration source → its recorded IR as a `serde_json::Value` (so a
/// test can assert on the wire ops without re-deserializing the typed IR).
fn record(src: &str, name: &str) -> Value {
    let ir = record_migration_to_ir_unsandboxed(src, OWNER, name)
        .unwrap_or_else(|e| panic!("record {name}: {e}"));
    serde_json::to_value(&ir).expect("ir -> value")
}

fn record_err(src: &str, name: &str) -> String {
    record_migration_to_ir_unsandboxed(src, OWNER, name)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_else(|| panic!("{name}: expected a recording error, got Ok"))
}

fn ops(ir: &Value) -> &Vec<Value> {
    ir.get("ops").and_then(|o| o.as_array()).expect("ops array")
}

fn column_named<'a>(cols: &'a [Value], name: &str) -> &'a Value {
    cols.iter()
        .find(|c| c.get("name").and_then(|n| n.as_str()) == Some(name))
        .unwrap_or_else(|| panic!("missing column {name}: {cols:#?}"))
}

/// `t.text()` records `nullable: true` (nullable by default) and `t.text()`
/// .notNull() records `nullable: false` — the §3.2 nullable-by-default rule.
#[test]
fn t_text_nullable_by_default_notnull_opts_in() {
    let src = r#"
        import { table, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("u").create({ columns: { a: t.text(), b: t.text().notNull() } });
        }};
    "#;
    let ir = record(src, "nullable");
    let cols = ops(&ir)[0].get("columns").and_then(|c| c.as_array()).unwrap();
    let a = column_named(cols, "a");
    let b = column_named(cols, "b");
    // Nullable-by-default: `a` OMITS `nullable` (its absence is the dialect default),
    // never records `nullable: false`.
    assert!(
        a.get("nullable").is_none(),
        "t.text() is nullable-by-default; `nullable` must be omitted (got {:?})",
        a
    );
    // notNull() records the explicit `nullable: false`.
    assert_eq!(
        b.get("nullable").and_then(|n| n.as_bool()),
        Some(false),
        "t.text().notNull() must record nullable: false"
    );
}

/// `c.fn.concatWs(" ", c("a"), c("b"))` records a `fnSynth(concatWs)` node — the
/// NULL-skipping safe-join helper (§3.3.1) that renders byte-identically on PG/
/// SQLite. Pinning the node shape here (the apply-identity is in the engine's
/// `ir_dml_*` PG/SQLite suites).
#[test]
fn concatws_records_fnsynth_node() {
    let src = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("u").update({ set: { full: (c) => c.fn.concatWs(" ", c("a"), c("b")) } });
        }};
    "#;
    let ir = record(src, "concatws");
    let node = ops(&ir)[0].get("set").unwrap().get("full").unwrap();
    assert_eq!(node.get("node").unwrap(), "fnSynth");
    assert_eq!(node.get("fn").unwrap(), "concatWs");
    let args = node.get("args").and_then(|a| a.as_array()).unwrap();
    assert_eq!(args.len(), 3, "concatWs records [sep, a, b]");
    assert_eq!(args[0], serde_json::json!({ "node": "literal", "value": " " }));
    assert_eq!(args[1], serde_json::json!({ "node": "colRef", "name": "a" }));
    assert_eq!(args[2], serde_json::json!({ "node": "colRef", "name": "b" }));
}

/// An `addForeignKey(table, { columns, references, name })` spec records the same
/// FK op regardless of field order — the named-field (not transposable-positional)
/// guarantee (§3.2 shaping convention 2).
#[test]
fn foreignkey_add_field_order_independent() {
    let src_a = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("orders").foreignKey("orders_customer_fk").add({
                columns: ["customer_id"],
                references: { table: "customers", columns: ["id"] },
            });
        }};
    "#;
    // The SAME spec with the fields written in a different order.
    let src_b = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("orders").foreignKey("orders_customer_fk").add({
                references: { columns: ["id"], table: "customers" },
                columns: ["customer_id"],
            });
        }};
    "#;
    let a = record(src_a, "fk_a");
    let b = record(src_b, "fk_b");
    assert_eq!(
        ops(&a)[0],
        ops(&b)[0],
        "FK spec field order must not affect the recorded op (named fields, not positionals)"
    );
    // And the recorded constraint is the frozen nested-kind FK shape.
    let kind = ops(&a)[0].get("constraint").unwrap().get("kind").unwrap();
    assert_eq!(kind.get("kind").unwrap(), "fk");
    assert_eq!(kind.get("referencesTable").unwrap(), "customers");
    assert_eq!(kind.get("referencesColumns").unwrap(), &serde_json::json!(["id"]));
}

#[test]
fn foreignkey_add_records_composite_non_id_fk() {
    let src = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("billing_line_provider_refs", { schema: "zeroship" }).foreignKey("billing_line_provider_refs_line_fk").add({
                columns: ["invoice_id", "app_id", "segment_no"],
                references: {
                    schema: "zeroship",
                    table: "invoice_lines",
                    columns: ["invoice_id", "app_id", "segment_no"],
                },
                onDelete: "cascade",
            });
        }};
    "#;
    let ir = record(src, "fk_composite");
    let op = &ops(&ir)[0];
    let kind = op.get("constraint").unwrap().get("kind").unwrap();
    assert_eq!(op.get("schema").unwrap(), "zeroship");
    assert_eq!(kind.get("kind").unwrap(), "fk");
    assert_eq!(kind.get("columns").unwrap(), &serde_json::json!(["invoice_id", "app_id", "segment_no"]));
    assert_eq!(kind.get("referencesTable").unwrap(), "invoice_lines");
    assert_eq!(
        kind.get("referencesColumns").unwrap(),
        &serde_json::json!(["invoice_id", "app_id", "segment_no"])
    );
    assert!(kind.get("schema").is_none(), "reference schema is not part of the frozen FK kind");
}

/// A migration that omits `name` records the host-supplied filename-derived label
/// (§3.1). The `export default { up }` shape (no `name`) is the common case.
#[test]
fn name_omitted_records_filename_label() {
    let src = r#"
        import { table } from "@zeroship/migrate";
        export default { up() { table("scratch").drop(); } };
    "#;
    let ir = record(src, "0009_drop_scratch");
    assert_eq!(
        ir.get("name").unwrap(),
        "0009_drop_scratch",
        "an absent module name records the filename-derived label"
    );
}

/// Calling an op-function OUTSIDE an active recorder (at module top level) throws
/// the structured `OP_OUTSIDE_RECORDER` (§3.1) — the op cannot be silently lost.
///
/// The throw happens during the migration module's own top-level EVALUATION (the
/// op-function runs while `op_recorder.js` imports `__migration__.js`, before the
/// adapter's try/catch can wrap it), so the runtime surfaces it as a hard
/// recording FAILURE. The contract this pins is precisely that: a top-level op is
/// NOT silently lost — it aborts recording. (The structured `OP_OUTSIDE_RECORDER`
/// code + message are thrown by the JS builder and logged by the runtime; the
/// observable Rust-side guarantee is the failed record. See `record_outside.js`
/// for a direct node assertion of the code.)
#[test]
fn op_outside_recorder_aborts_recording() {
    let src = r#"
        import { table, t } from "@zeroship/migrate";
        // Called at MODULE TOP LEVEL — outside any up()/down() recorder.
        table("u").create({ columns: { id: t.id() } });
        export default { up() {} };
    "#;
    let err = record_err(src, "outside");
    // A top-level op must hard-fail recording (the op is not silently dropped).
    assert!(
        err.contains("rejected") || err.contains("evaluate") || err.contains("OP_OUTSIDE_RECORDER"),
        "a top-level op call must abort recording; got: {err}"
    );
    // The well-formed control: the SAME op inside up() records cleanly.
    let ok_src = r#"
        import { table, t } from "@zeroship/migrate";
        export default { up() { table("u").create({ columns: { id: t.id() } }); } };
    "#;
    let ir = record(ok_src, "inside");
    assert_eq!(ops(&ir).len(), 1, "the same op inside up() records fine");
}

/// The fluent insert row-OBJECT form normalizes to the frozen columns + positional
/// rows wire shape (column order from the first row's keys).
#[test]
fn insert_row_object_normalizes_to_columns_and_rows() {
    let src = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").insert({ rows: [ { code: 1, label: "a" }, { code: 2, label: "b" } ] });
        }};
    "#;
    let ir = record(src, "insert_obj");
    let op = &ops(&ir)[0];
    assert_eq!(op.get("columns").unwrap(), &serde_json::json!(["code", "label"]));
    assert_eq!(op.get("rows").unwrap(), &serde_json::json!([[1, "a"], [2, "b"]]));
}

#[test]
fn insert_row_object_rejects_ragged_later_row_keys() {
    let src = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").insert({ rows: [ { a: 1 }, { a: 2, b: 2 } ] });
        }};
    "#;
    let err = record_err(src, "insert_ragged_rows");
    assert!(
        err.contains("ragged insert rows"),
        "ragged insert rows must fail closed, got: {err}"
    );
}

#[test]
fn insert_later_row_function_value_fails_closed() {
    let src = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            const f = () => 1;
            table("t").insert({ rows: [ { a: 1 }, { b: f } ] });
        }};
    "#;
    let err = record_err(src, "insert_later_row_fn");
    assert!(
        err.contains("function values are not valid here"),
        "later-row non-native function insert value must fail closed, got: {err}"
    );
    assert!(
        err.contains("Date.now / Math.random / crypto.randomUUID"),
        "message must steer to supported native symbols, got: {err}"
    );
}

#[test]
fn date_now_symbol_records_as_fnsynth_now() {
    let symbol = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").insert({ rows: [ { at: Date.now } ] });
        }};
    "#;
    let explicit = r#"
        import { table, cFn } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").insert({ rows: [ { at: cFn.now() } ] });
        }};
    "#;
    assert_eq!(ops(&record(symbol, "date_now_symbol"))[0], ops(&record(explicit, "date_now_explicit"))[0]);
}

#[test]
fn math_random_symbol_records_as_fnsynth_gen_random_uuid() {
    let symbol = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").insert({ rows: [ { id: Math.random } ] });
        }};
    "#;
    let explicit = r#"
        import { table, cFn } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").insert({ rows: [ { id: cFn.genRandomUuid() } ] });
        }};
    "#;
    assert_eq!(ops(&record(symbol, "math_random_symbol"))[0], ops(&record(explicit, "math_random_explicit"))[0]);
}

#[test]
fn default_date_now_symbol_equals_default_fn_now() {
    let symbol = r#"
        import { table, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").create({ columns: { at: t.timestamp().default(Date.now) } });
        }};
    "#;
    let explicit = r#"
        import { table, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").create({ columns: { at: t.timestamp().default({ fn: "now" }) } });
        }};
    "#;
    assert_eq!(ops(&record(symbol, "default_date_now_symbol"))[0], ops(&record(explicit, "default_fn_now"))[0]);
}

#[test]
fn non_native_function_value_fails_closed_in_v8_recorder() {
    let insert = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            const f = () => 42;
            table("t").insert({ rows: [ { v: f } ] });
        }};
    "#;
    let err = record_err(insert, "bad_fn_insert");
    assert!(
        err.contains("function values are not valid here"),
        "non-native function insert value must fail closed, got: {err}"
    );
    assert!(
        err.contains("Date.now / Math.random / crypto.randomUUID"),
        "message must steer to supported native symbols, got: {err}"
    );

    let default = r#"
        import { table, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").create({ columns: { v: t.int().default(() => 1) } });
        }};
    "#;
    let err = record_err(default, "bad_fn_default");
    assert!(
        err.contains("function values are not valid here"),
        "non-native function default must fail closed, got: {err}"
    );
}

#[test]
fn nested_function_values_fail_closed_in_v8_recorder() {
    let cases = [
        (
            "nested_fn_insert_object",
            r#"
                import { table } from "@zeroship/migrate";
                export default { name: "n", up() {
                    const f = () => 42;
                    table("t").insert({ rows: [ { doc: { a: f } } ] });
                }};
            "#,
        ),
        (
            "nested_fn_insert_array",
            r#"
                import { table } from "@zeroship/migrate";
                export default { name: "n", up() {
                    const f = () => 42;
                    table("t").insert({ rows: [ { tags: [f] } ] });
                }};
            "#,
        ),
        (
            "nested_native_symbol_insert",
            r#"
                import { table } from "@zeroship/migrate";
                export default { name: "n", up() {
                    table("t").insert({ rows: [ { doc: { a: Date.now } } ] });
                }};
            "#,
        ),
        (
            "nested_fn_default",
            r#"
                import { table, t } from "@zeroship/migrate";
                export default { name: "n", up() {
                    const f = () => 42;
                    table("t").create({ columns: { doc: t.json().default({ a: f }) } });
                }};
            "#,
        ),
        (
            "nested_fn_on_conflict",
            r#"
                import { table } from "@zeroship/migrate";
                export default { name: "n", up() {
                    const f = () => 42;
                    table("t").insert({
                        rows: [ { id: 1 } ],
                        onConflict: { columns: ["id"], doUpdate: { doc: { a: f } } },
                    });
                }};
            "#,
        ),
        (
            "nested_fn_expr_arg",
            r#"
                import { table } from "@zeroship/migrate";
                export default { name: "n", up() {
                    const f = () => 42;
                    table("t").update({ set: { doc: (c) => c.fn.coalesce({ a: f }, "x") } });
                }};
            "#,
        ),
    ];

    for (name, src) in cases {
        let err = record_err(src, name);
        assert!(
            err.contains("function values are not valid here"),
            "{name}: nested function value must fail closed, got: {err}"
        );
        assert!(
            err.contains("Date.now / Math.random / crypto.randomUUID"),
            "{name}: message must steer to supported native symbols, got: {err}"
        );
    }
}

#[test]
fn date_now_symbol_is_deterministic_across_records() {
    let src = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").insert({ rows: [ { at: Date.now } ] });
        }};
    "#;
    let a = record_migration_to_ir_unsandboxed(src, OWNER, "date_now_symbol_a").unwrap();
    let b = record_migration_to_ir_unsandboxed(src, OWNER, "date_now_symbol_a").unwrap();
    let checksum = |ir: &zeroship_migrate::MigrationIr| {
        Checksum::of_ir(
            &CanonicalOpList(&ir.ops),
            &MigrationFlags::default(),
            &ir.owner_app,
            &[],
            &[],
            &ir.preconditions,
        )
        .as_str()
        .to_string()
    };
    assert_eq!(checksum(&a), checksum(&b));
}

#[test]
fn date_now_call_just_evaluates_no_error() {
    use zeroship_migrate::frontend::record_migration_to_ir_with_warnings_unsandboxed;

    let src = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").insert({ rows: [ { at: Date.now() } ] });
        }};
    "#;
    let outcome = record_migration_to_ir_with_warnings_unsandboxed(src, OWNER, "date_now_call")
        .expect("Date.now() call should evaluate and record");
    let value = serde_json::to_value(&outcome.ir).expect("ir -> value");
    let cell = &ops(&value)[0]["rows"][0][0];
    assert!(cell.as_i64().is_some(), "Date.now() call records its evaluated number: {cell}");
    assert!(
        outcome.warnings.iter().any(|f| f.accessor.contains("Date.now")),
        "Date.now() call should produce only a soft advisory warning: {:?}",
        outcome.warnings
    );
}

/// The §4.3 determinism lint flags `Date.now()` in an op argument and steers the
/// author to `c.fn.now()`; a clean migration produces NO findings.
#[test]
fn determinism_lint_flags_date_now_in_op_arg() {
    let dirty = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").insert({ rows: [ { created_at: Date.now() } ] });
        }};
    "#;
    let findings = lint_migration_determinism(dirty).expect("lint runs");
    assert!(
        !findings.is_empty(),
        "Date.now() in an op argument must be flagged by the determinism lint"
    );
    let f = &findings[0];
    assert_eq!(f.code, "NONDETERMINISTIC_OP_ARG");
    assert!(f.accessor.contains("Date.now"), "accessor names Date.now(): {}", f.accessor);
    assert!(f.suggested_fix.contains("Date.now symbol"), "steer names symbol form: {}", f.suggested_fix);
    assert!(f.suggested_fix.contains("c.fn.now()"), "steer names c.fn.now(): {}", f.suggested_fix);

    let clean = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").insert({ rows: [ { created_at: Date.now } ] });
        }};
    "#;
    assert!(
        lint_migration_determinism(clean).expect("lint runs").is_empty(),
        "the Date.now symbol form must produce NO determinism findings"
    );
}

/// The determinism lint also flags the RNG accessor + the clock/Date constructor
/// (§4.3 mechanism (a)): `Math.random()`, `crypto.randomUUID()`, `new Date()`.
#[test]
fn determinism_lint_flags_rng_and_clock_constructor() {
    for (src_frag, needle) in [
        ("Math.random()", "Math.random"),
        ("crypto.randomUUID()", "crypto.randomUUID"),
        ("new Date()", "new Date"),
    ] {
        let src = format!(
            r#"
            import {{ table }} from "@zeroship/migrate";
            export default {{ name: "n", up() {{
                table("t").insert({{ rows: [ {{ v: {src_frag} }} ] }});
            }}}};
            "#
        );
        let findings = lint_migration_determinism(&src).expect("lint runs");
        assert!(
            findings.iter().any(|f| f.accessor.contains(needle)),
            "{src_frag} must be flagged; findings: {findings:?}"
        );
    }
}

/// Same source → same recorded JSON (determinism — §4.3): recording the same
/// migration twice yields byte-identical IR.
#[test]
fn same_source_records_same_json() {
    let src = r#"
        import { table, t } from "@zeroship/migrate";
        export default { name: "det", up() {
            const u = table("u");
            u.create({ columns: { id: t.id(), email: t.text().notNull() } });
            u.column("status").add({ type: t.text().default("new") });
            u.update({ set: { email: (c) => c.fn.lower(c("email")) }, where: (c) => c("id").isNotNull() });
        }};
    "#;
    let a =
        zeroship_migrate::frontend::record_migration_to_json_unsandboxed(src, OWNER, "det").unwrap();
    let b =
        zeroship_migrate::frontend::record_migration_to_json_unsandboxed(src, OWNER, "det").unwrap();
    assert_eq!(a, b, "the same source must record byte-identical .ir.json");
}

/// The fluent `(c) => Expr` builder constructs the closed AST for every operator
/// family (comparison/boolean/arithmetic/cast/unary), proving the headline §3.3.1
/// surface records the same closed-AST nodes the Rust validator/lowerer expect.
#[test]
fn fluent_expr_builder_constructs_closed_ast() {
    let src = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").update({
                set: {
                    a: (c) => c("x").add(1).cast("integer"),
                    b: (c) => c("y").isNull().not(),
                },
                where: (c) => c("x").gt(0).and(c("y").le(10)),
            });
        }};
    "#;
    let ir = record(src, "fluent_expr");
    let set = ops(&ir)[0].get("set").unwrap();
    // a: cast(add(colRef x, lit 1), integer)
    let a = set.get("a").unwrap();
    assert_eq!(a.get("node").unwrap(), "cast");
    assert_eq!(a.get("target").unwrap(), "integer");
    assert_eq!(a.get("operand").unwrap().get("op").unwrap(), "add");
    // b: not(isNull(colRef y))
    let b = set.get("b").unwrap();
    assert_eq!(b.get("node").unwrap(), "unaryOp");
    assert_eq!(b.get("op").unwrap(), "not");
    assert_eq!(b.get("operand").unwrap().get("op").unwrap(), "isNull");
    // where: and(gt(...), le(...))
    let w = ops(&ir)[0].get("where").unwrap();
    assert_eq!(w.get("op").unwrap(), "and");
    assert_eq!(w.get("lhs").unwrap().get("op").unwrap(), "gt");
    assert_eq!(w.get("rhs").unwrap().get("op").unwrap(), "le");
}

/// A spec-blessed `decimal()` / `Uint8Array` author value passed through the FLUENT
/// insert + column default records the closed `IrScalar` WIRE carriers
/// (`{decimal}` / `{bytes:base64}`), so the RECORD path produces a shape Rust
/// accepts value-equal — the previously promised-but-broken §3.2/§2.3.2 path. A
/// pre-fix recorder either cannot expose the decimal value constructor or emits
/// the `{"0":…}` array-index spelling Rust HARD-REJECTS, so `record` would fail.
#[test]
fn fluent_insert_normalizes_decimal_and_bytes_scalars() {
    let src = r#"
        import { table, t, decimal } from "@zeroship/migrate";
        export default { name: "n", up() {
            const tbl = table("t");
            tbl.create({
                columns: {
                    id: t.id(),
                    seq: t.numeric(38, 0).notNull().default(decimal("9007199254740993")),
                    salt: t.bytes().default(new Uint8Array([1, 2, 3, 255])),
                },
            });
            tbl.insert({ rows: [ { seq: decimal("9007199254740993"), salt: new Uint8Array([0, 255]) } ] });
        }};
    "#;
    // Recording succeeds (the typed `MigrationIr` deserialize is the gate) — the
    // scalars came through as the accepted carriers.
    let ir = record(src, "scalars");
    let cols = ops(&ir)[0].get("columns").and_then(|c| c.as_array()).unwrap();
    // seq default -> {literal:{value:{decimal:"9007199254740993"}}}
    let seq_default = column_named(cols, "seq")
        .get("default")
        .unwrap()
        .get("literal")
        .unwrap()
        .get("value")
        .unwrap();
    assert_eq!(seq_default.get("decimal").unwrap(), "9007199254740993");
    // salt default -> {literal:{value:{bytes:"AQID/w=="}}}
    let salt_default = column_named(cols, "salt")
        .get("default")
        .unwrap()
        .get("literal")
        .unwrap()
        .get("value")
        .unwrap();
    assert_eq!(salt_default.get("bytes").unwrap(), "AQID/w==");
    // insert row carriers
    let row = &ops(&ir)[1].get("rows").unwrap().as_array().unwrap()[0];
    assert_eq!(row[0].get("decimal").unwrap(), "9007199254740993");
    assert_eq!(row[1].get("bytes").unwrap(), "AP8="); // base64([0,255])
}

/// `update { batch }` is authorable through the engine recorder AND deserializes
/// into `Op::Update.batch` (parity with the npm DSL, which now also exposes
/// `batch`). The two JS impls expose ONE surface.
#[test]
fn update_carries_a_batch_knob() {
    let src = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").update({
                set: { x: (c) => c.fn.now() },
                where: (c) => c("id").isNotNull(),
                batch: { cursorColumn: "id", batchSize: 500 },
            });
        }};
    "#;
    let ir = record(src, "ubatch");
    let batch = ops(&ir)[0].get("batch").expect("update records the batch knob");
    assert_eq!(batch.get("cursorColumn").unwrap(), "id");
    assert_eq!(batch.get("batchSize").unwrap(), 500);
}

/// The determinism lint is WIRED into the record path as a soft advisory only:
/// calls evaluate normally, the recorder persists the produced value, and the
/// source lint steers authors toward DB-evaluated symbols / `c.fn.*`.
#[test]
fn record_path_allows_date_now_call_with_soft_warning() {
    use zeroship_migrate::frontend::record_migration_to_ir_with_warnings_unsandboxed;

    let src = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").insert({ rows: [ { created_at: Date.now() } ] });
        }};
    "#;
    let outcome = record_migration_to_ir_with_warnings_unsandboxed(src, OWNER, "date_call")
        .expect("Date.now() call evaluates and records");
    assert_eq!(outcome.ir.ops.len(), 1);
    let created_at = match &outcome.ir.ops[0] {
        zeroship_migrate::Op::Insert { rows, .. } => &rows[0][0],
        other => panic!("expected insert, got {other:?}"),
    };
    assert!(
        matches!(created_at, zeroship_migrate::IrValue::Scalar(zeroship_migrate::IrScalar::Int(v)) if *v > 0),
        "Date.now() call must record its evaluated integer value, got {created_at:?}"
    );
    assert!(
        outcome
            .warnings
            .iter()
            .any(|f| f.code == "NONDETERMINISTIC_OP_ARG" && f.accessor.contains("Date.now")),
        "Date.now() call should surface advisory warning: {:?}",
        outcome.warnings
    );

    // A normal deterministic migration still records cleanly with no advisory.
    let clean = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").insert({ rows: [ { v: 1 } ] });
        }};
    "#;
    let clean_outcome = record_migration_to_ir_with_warnings_unsandboxed(clean, OWNER, "clean")
        .expect("clean migration records");
    assert!(
        clean_outcome.warnings.is_empty(),
        "a clean migration surfaces no determinism warnings: {:?}",
        clean_outcome.warnings
    );
}

#[test]
fn record_path_allows_math_random_calls_with_soft_warning() {
    use zeroship_migrate::frontend::record_migration_to_ir_with_warnings_unsandboxed;

    let src = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            const collapsed = Math.random() - Math.random();
            table("t").insert({ rows: [ { sample: collapsed } ] });
        }};
    "#;
    let outcome = record_migration_to_ir_with_warnings_unsandboxed(src, OWNER, "random_difference")
        .expect("Math.random() calls evaluate and record");
    assert_eq!(outcome.ir.ops.len(), 1);
    let sample = match &outcome.ir.ops[0] {
        zeroship_migrate::Op::Insert { rows, .. } => &rows[0][0],
        other => panic!("expected insert, got {other:?}"),
    };
    assert!(
        matches!(sample, zeroship_migrate::IrValue::Scalar(zeroship_migrate::IrScalar::Decimal(_))),
        "Math.random() arithmetic should record the evaluated finite number carrier, got {sample:?}"
    );
    assert!(
        outcome
            .warnings
            .iter()
            .any(|f| f.code == "NONDETERMINISTIC_OP_ARG" && f.accessor.contains("Math.random")),
        "Math.random() call should surface advisory warning: {:?}",
        outcome.warnings
    );
}

#[test]
fn record_path_allows_argless_new_date_call_with_soft_warning() {
    use zeroship_migrate::frontend::record_migration_to_ir_with_warnings_unsandboxed;

    let src = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").insert({ rows: [ { year: new Date().getUTCFullYear() } ] });
        }};
    "#;
    let outcome = record_migration_to_ir_with_warnings_unsandboxed(src, OWNER, "argless_date")
        .expect("argless new Date() call evaluates and records");
    assert_eq!(outcome.ir.ops.len(), 1);
    let year = match &outcome.ir.ops[0] {
        zeroship_migrate::Op::Insert { rows, .. } => &rows[0][0],
        other => panic!("expected insert, got {other:?}"),
    };
    assert!(
        matches!(year, zeroship_migrate::IrValue::Scalar(zeroship_migrate::IrScalar::Int(v)) if *v >= 2026),
        "new Date().getUTCFullYear() should record the evaluated year, got {year:?}"
    );
    assert!(
        outcome
            .warnings
            .iter()
            .any(|f| f.code == "NONDETERMINISTIC_OP_ARG" && f.accessor.contains("new Date")),
        "argless new Date() should surface advisory warning: {:?}",
        outcome.warnings
    );
}

#[test]
fn record_path_allows_explicit_new_date_argument() {
    use zeroship_migrate::frontend::record_migration_to_ir_with_warnings_unsandboxed;

    let clean = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").insert({ rows: [ { ms: new Date(0).getTime() } ] });
        }};
    "#;
    let outcome = record_migration_to_ir_with_warnings_unsandboxed(clean, OWNER, "explicit_date")
        .expect("new Date(<explicit ms>) records normally");
    assert_eq!(outcome.ir.ops.len(), 1);
}

#[test]
fn record_path_allows_date_now_inside_comment_or_string() {
    use zeroship_migrate::frontend::record_migration_to_ir_with_warnings_unsandboxed;

    let clean = r#"
        import { table } from "@zeroship/migrate";
        // Human note only: Date.now() and Math.random() are not executed.
        export default { name: "n", up() {
            table("t").insert({
                rows: [ { note: "literal mentions Date.now() and Math.random()" } ],
            });
        }};
    "#;
    let outcome = record_migration_to_ir_with_warnings_unsandboxed(clean, OWNER, "comment_string")
        .expect("inert Date.now/Math.random text records normally");
    assert_eq!(outcome.ir.ops.len(), 1);
}

// ───────────────────────────────────────────────────────────────────────────
// PR10 review F1 (HIGH) — twin-fidelity round-trip.
//
// The engine-embedded V8 recorder is the compiled artifact
// (`sdks/migrate/dist/embedded-recorder.js`) built from `sdks/migrate/src/ops.ts`
// (S0.5 collapsed the former hand-kept `migrate_ops.js` twin). Before this fix the
// recorder DROPPED the `schema`
// qualifier and `existenceGuard` token on 10 op variants at RECORD time — a
// silently-dropped `ifNotExists` turned a guarded create into a bare
// unconditional create (fail-OPEN over a divergent object), and a dropped schema
// silently re-pinned the op to the project schema. These tests author EVERY
// schema-targeting / guardable op through the REAL V8 recorder WITH a schema
// qualifier + (where legal) an existence guard and assert the recorded IR
// carries them — RED before the twin emits `schema`/`existenceGuard` on these ops.
// ───────────────────────────────────────────────────────────────────────────

/// Find the first recorded op with the given `op` discriminant.
fn op_named<'a>(ir: &'a Value, name: &str) -> &'a Value {
    ops(ir)
        .iter()
        .find(|o| o.get("op").and_then(|v| v.as_str()) == Some(name))
        .unwrap_or_else(|| panic!("no recorded `{name}` op in {ir:#}"))
}

fn assert_schema(op: &Value, want: &str) {
    assert_eq!(
        op.get("schema").and_then(|v| v.as_str()),
        Some(want),
        "op `{}` must carry schema:{want:?}; got {op:#}",
        op.get("op").and_then(|v| v.as_str()).unwrap_or("?"),
    );
}

fn assert_guard(op: &Value, want: &str) {
    assert_eq!(
        op.get("existenceGuard").and_then(|v| v.as_str()),
        Some(want),
        "op `{}` must carry existenceGuard:{want:?}; got {op:#}",
        op.get("op").and_then(|v| v.as_str()).unwrap_or("?"),
    );
}

/// `table(name).create({ columns, schema, ifNotExists })` records BOTH the schema
/// qualifier and the `ifNotExists` create-family guard. RED before the twin fix
/// (the bare `createTable` dropped both — a fail-OPEN unconditional CREATE).
#[test]
fn twin_create_table_carries_schema_and_guard() {
    let src = r#"
        import { table, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").create({ columns: { qty: t.int() }, schema: "app2", ifNotExists: true });
        }};
    "#;
    let ir = record(src, "create_schema_guard");
    let op = op_named(&ir, "createTable");
    assert_schema(op, "app2");
    assert_guard(op, "ifNotExists");
}

/// `table(name).column(from).rename({ to, type, schema })` records the schema
/// qualifier. RED before the twin fix.
#[test]
fn twin_rename_column_carries_schema() {
    let src = r#"
        import { table, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").column("a").rename({ to: "b", type: t.text(), schema: "app2" });
        }};
    "#;
    let ir = record(src, "rename_schema_guard");
    let op = op_named(&ir, "renameColumn");
    assert_schema(op, "app2");
}

/// `table(name).column(col).setType({ to, schema })` records `setColumnType`
/// carrying the schema qualifier. RED before the twin fix.
#[test]
fn twin_alter_column_type_carries_schema() {
    let src = r#"
        import { table, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").column("a").setType({ to: t.bigInt(), schema: "app2" });
        }};
    "#;
    let ir = record(src, "alter_type_schema_guard");
    let op = op_named(&ir, "setColumnType");
    assert_schema(op, "app2");
}

/// `table(name).column(col).setNotNull({ schema })` records `setColumnNotNull`
/// carrying the schema qualifier. RED before the twin fix.
#[test]
fn twin_alter_column_nullability_carries_schema() {
    let src = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").column("a").setNotNull({ schema: "app2" });
        }};
    "#;
    let ir = record(src, "alter_null_schema_guard");
    let op = op_named(&ir, "setColumnNotNull");
    assert_schema(op, "app2");
}

#[test]
fn column_alter_bag_is_not_on_embedded_recorder_surface() {
    let src = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            const col = table("t").column("a");
            if ("alter" in col) throw new Error("alter should not exist");
            col.setNotNull();
        }};
    "#;
    let ir = record(src, "column_alter_absent");
    assert_eq!(ops(&ir).len(), 1);
    assert_eq!(ops(&ir)[0].get("op").and_then(|v| v.as_str()), Some("setColumnNotNull"));
}

/// `addForeignKey` / `addUnique` / `addCheck` all record an `addConstraint` op
/// carrying the schema qualifier + the `ifNotExists` add-family guard. RED before
/// the twin fix.
#[test]
fn twin_add_constraint_family_carries_schema_and_guard() {
    let fk = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").foreignKey("t_o_fk").add({
                columns: ["o"],
                references: { table: "o", columns: ["id"] },
                schema: "app2",
                ifNotExists: true,
            });
        }};
    "#;
    let fk_ir = record(fk, "fk_schema_guard");
    let op = op_named(&fk_ir, "addConstraint");
    assert_schema(op, "app2");
    assert_guard(op, "ifNotExists");

    let uq = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").unique("t_a_uq").add({ columns: ["a"], schema: "app2", ifNotExists: true });
        }};
    "#;
    let uq_ir = record(uq, "uq_schema_guard");
    let op = op_named(&uq_ir, "addConstraint");
    assert_schema(op, "app2");
    assert_guard(op, "ifNotExists");

    let ck = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").check("t_a_chk").add({ expr: (c) => c("a").gt(0), schema: "app2", ifNotExists: true });
        }};
    "#;
    let ck_ir = record(ck, "ck_schema_guard");
    let op = op_named(&ck_ir, "addConstraint");
    assert_schema(op, "app2");
    assert_guard(op, "ifNotExists");
}

/// Sequences are standalone top-level ops; exclusion constraints are table
/// constraints with a CLOSED operator token set. The V8 recorder must emit the
/// exact nested IR shape the Rust loader deserializes.
#[test]
fn sequences_and_exclusion_constraints_record_canonical_ir() {
    let src = r#"
        import { table, t } from "@zeroship/migrate";
        import { sequence } from "@zeroship/migrate/pg";
        export default { name: "n", up() {
            sequence("invoice_seq").create({
                as: t.bigInt(),
                increment: 5,
                start: 100,
                cache: 10,
                cycle: true,
                ownedBy: { table: "invoices", column: "id" },
                schema: "app2",
            });
            sequence("invoice_seq").alter({
                increment: 7,
                restart: 200,
                minValue: 1,
                maxValue: 999,
                cache: 20,
                cycle: false,
                ownedBy: null,
                schema: "app2",
            });
            sequence("invoice_seq").drop({ schema: "app2", ifExists: true });
            table("bookings", { schema: "app2" }).exclusion("bookings_no_overlap").add({
                using: "gist",
                elements: [
                    { target: "room", operator: "=" },
                    { target: "during", operator: "&&" },
                ],
                where: (c) => c("cancelled").eq(false),
                deferrable: true,
                ifNotExists: true,
            });
        }};
    "#;
    let ir = record(src, "seq_excl");
    assert_eq!(
        ops(&ir)[0],
        json!({
            "op": "createSequence",
            "name": "invoice_seq",
            "schema": "app2",
            "as": "bigInt",
            "increment": 5,
            "start": 100,
            "cache": 10,
            "cycle": true,
            "ownedBy": { "table": "invoices", "column": "id" }
        })
    );
    assert_eq!(ops(&ir)[1].get("ownedBy"), Some(&Value::Null));
    assert_eq!(ops(&ir)[2].get("op").and_then(Value::as_str), Some("dropSequence"));
    assert_guard(&ops(&ir)[2], "ifExists");

    let exclusion = &ops(&ir)[3];
    assert_eq!(exclusion.get("op").and_then(Value::as_str), Some("addConstraint"));
    assert_guard(exclusion, "ifNotExists");
    assert_eq!(
        exclusion.get("constraint").and_then(|c| c.get("kind")),
        Some(&json!({
            "kind": "exclusion",
            "usingMethod": "gist",
            "elements": [
                { "target": { "kind": "column", "name": "room" }, "operator": "=" },
                { "target": { "kind": "column", "name": "during" }, "operator": "&&" }
            ],
            "wherePredicate": {
                "node": "binOp",
                "op": "eq",
                "lhs": { "node": "colRef", "name": "cancelled" },
                "rhs": { "node": "literal", "value": false }
            },
            "deferrable": true
        }))
    );
}

/// `dropConstraint(table, name, { schema, ifExists })` records the schema
/// qualifier + the `ifExists` drop-family guard. RED before the twin fix.
#[test]
fn twin_drop_constraint_carries_schema_and_guard() {
    let src = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").constraint("t_a_key").drop({ schema: "app2", ifExists: true });
        }};
    "#;
    let dc_ir = record(src, "drop_constraint_schema_guard");
    let op = op_named(&dc_ir, "dropConstraint");
    assert_schema(op, "app2");
    assert_guard(op, "ifExists");
}

/// `addColumn(table, name, type, { schema, ifNotExists })` records the schema
/// qualifier + the `ifNotExists` add-family guard. Completes the twin proof for
/// the add-column op (PR10 review LOW — closes the full_surface coverage hole so a
/// future recorder edit that drops the guard here is caught RED).
#[test]
fn twin_add_column_carries_schema_and_guard() {
    let src = r#"
        import { table, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").column("c").add({ type: t.int(), schema: "app2", ifNotExists: true });
        }};
    "#;
    let ir = record(src, "add_column_schema_guard");
    let op = op_named(&ir, "addColumn");
    assert_schema(op, "app2");
    assert_guard(op, "ifNotExists");
}

/// `dropTable(table, { schema, ifExists })` records the schema qualifier + the
/// `ifExists` drop-family guard. PR10 review LOW — coverage-hole closure.
#[test]
fn twin_drop_table_carries_schema_and_guard() {
    let src = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").drop({ schema: "app2", ifExists: true });
        }};
    "#;
    let ir = record(src, "drop_table_schema_guard");
    let op = op_named(&ir, "dropTable");
    assert_schema(op, "app2");
    assert_guard(op, "ifExists");
}

/// `view(name, { schema, columns })` mirrors the table handle's config pattern:
/// the handle schema stamps create/drop, handle columns stamp create, and
/// `drop({ ifExists })` records the CORE existenceGuard token.
#[test]
fn twin_view_handle_config_and_drop_guard() {
    let src = r#"
        import { view } from "@zeroship/migrate";
        export default { name: "n", up() {
            const v = view("active_users", { schema: "zeroship", columns: ["id", "email"] });
            v.create({ as: (q) => q.from("users").select(["id", "email"]) });
            v.drop({ ifExists: true });
        }};
    "#;
    let ir = record(src, "view_handle_config_guard");
    let ops = ops(&ir);
    let create = &ops[0];
    assert_eq!(create.get("op").and_then(|v| v.as_str()), Some("createView"));
    assert_schema(create, "zeroship");
    assert_eq!(
        create.get("columns"),
        Some(&serde_json::json!(["id", "email"])),
        "handle columns must stamp createView unless inline columns override; got {create:#}",
    );

    let drop = &ops[1];
    assert_eq!(drop.get("op").and_then(|v| v.as_str()), Some("dropView"));
    assert_schema(drop, "zeroship");
    assert_guard(drop, "ifExists");
    assert!(
        drop.get("ifExists").is_none(),
        "legacy native ifExists bool must not be recorded on dropView: {drop:#}"
    );
}

/// `dropColumn(table, column, { schema, ifExists })` records the schema qualifier
/// + the `ifExists` drop-family guard. PR10 review LOW — coverage-hole closure.
#[test]
fn twin_drop_column_carries_schema_and_guard() {
    let src = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").column("c").drop({ schema: "app2", ifExists: true });
        }};
    "#;
    let ir = record(src, "drop_column_schema_guard");
    let op = op_named(&ir, "dropColumn");
    assert_schema(op, "app2");
    assert_guard(op, "ifExists");
}

/// `createIndex(table, { columns, schema, ifNotExists })` records the schema
/// qualifier + the `ifNotExists` create-family guard. PR10 review LOW —
/// coverage-hole closure (a dropped guard here would turn a guarded index create
/// into a bare unconditional CREATE INDEX — fail-OPEN).
#[test]
fn twin_create_index_carries_schema_and_guard() {
    let src = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").index("idx_t_a").add({ on: ["a"], schema: "app2", ifNotExists: true });
        }};
    "#;
    let ir = record(src, "create_index_schema_guard");
    let op = op_named(&ir, "createIndex");
    assert_schema(op, "app2");
    assert_guard(op, "ifNotExists");
}

/// `dropIndex(name, { table, schema, ifExists })` records the schema qualifier +
/// the `ifExists` drop-family guard. PR10 review LOW — coverage-hole closure.
#[test]
fn twin_drop_index_carries_schema_and_guard() {
    let src = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").index("idx_t_a").drop({ schema: "app2", ifExists: true });
        }};
    "#;
    let ir = record(src, "drop_index_schema_guard");
    let op = op_named(&ir, "dropIndex");
    assert_schema(op, "app2");
    assert_guard(op, "ifExists");
}

/// The DML ops `insert` / `update` / `delete` / `backfill` carry the schema
/// qualifier (no existence guard — DML is not guardable). RED before the twin fix
/// (the schema was silently dropped, re-pinning the op to the project schema).
#[test]
fn twin_dml_ops_carry_schema() {
    let ins = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").insert({ rows: [{ a: 1 }], schema: "app2" });
        }};
    "#;
    assert_schema(&op_named(&record(ins, "insert_schema"), "insert"), "app2");

    let upd = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").update({ set: { a: (c) => c("a") }, schema: "app2" });
        }};
    "#;
    assert_schema(&op_named(&record(upd, "update_schema"), "update"), "app2");

    let del = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").delete({ where: (c) => c("a").gt(0), schema: "app2" });
        }};
    "#;
    assert_schema(&op_named(&record(del, "delete_schema"), "delete"), "app2");

    let bf = r#"
        import { table } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("t").backfill({ set: { a: (c) => c("a") }, schema: "app2" });
        }};
    "#;
    assert_schema(&op_named(&record(bf, "backfill_schema"), "backfill"), "app2");
}

// ───────────────────────────────────────────────────────────────────────────
// PR11 — the eager fluent `table()` surface. The engine-embedded V8 recorder's
// `table()` records the canonical byte-stable op objects. These target the fluent
// terminals directly through the REAL V8 recorder.
// ───────────────────────────────────────────────────────────────────────────

/// The full DDL + DML + schema + guard surface authored via `table()` records the
/// expected canonical op sequence. RED if a fluent terminal drops schema/guard
/// propagation or records the wrong op kind.
#[test]
fn twin_table_surface_records_full_expected_op_sequence() {
    let src = r#"
        import { table, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            const u = table("users", { schema: "app2" });
            u.create({ columns: { id: t.id(), email: t.text().notNull() }, ifNotExists: true });
            u.column("status").add({ type: t.text().notNull().default("new") });
            u.column("legacy").drop({ ifExists: true });
            u.column("label").rename({ to: "display_label", type: t.text() });
            u.column("status").setNotNull();
            u.foreignKey("u_team_fk").add({ columns: ["team"], references: { table: "teams", columns: ["name"] } });
            u.unique("u_email_uq").add({ columns: ["email"] });
            u.check("u_status_chk").add({ expr: (c) => c("status").isNotNull() });
            u.constraint("u_legacy_chk").drop({ ifExists: true });
            u.index("u_email_idx").add({ on: ["email"], unique: true });
            u.index("u_old_idx").drop({ ifExists: true });
            u.insert({ rows: [{ email: "a@b.c", status: "new" }] });
            u.update({ set: { status: (c) => c.fn.lower(c("status")) }, where: (c) => c("id").isNotNull() });
            u.delete({ where: (c) => c("status").isNull(), limit: 10 });
            u.backfill({ set: { status: (c) => c.fn.coalesce(c("status"), "new") }, cursorColumn: "id", batchSize: 500, name: "bf_status" });
        }};
    "#;
    let ir = record(src, "fluent_full");
    let names: Vec<&str> = ops(&ir)
        .iter()
        .map(|o| o.get("op").and_then(|v| v.as_str()).unwrap())
        .collect();
    assert_eq!(
        names,
        vec![
            "createTable",
            "addColumn",
            "dropColumn",
            "renameColumn",
            "setColumnNotNull",
            "addConstraint",
            "addConstraint",
            "addConstraint",
            "dropConstraint",
            "createIndex",
            "dropIndex",
            "insert",
            "update",
            "delete",
            "backfill",
        ]
    );
    for op in ops(&ir) {
        assert_schema(op, "app2");
    }
    assert_guard(&ops(&ir)[0], "ifNotExists");
    assert_guard(&ops(&ir)[2], "ifExists");
    assert_guard(&ops(&ir)[8], "ifExists");
    assert_guard(&ops(&ir)[10], "ifExists");
}

/// `table().create({ columns, uniques, indexes, ifNotExists })`
/// records the table-level constraints/indexes AND the schema/guard on the one
/// `createTable` op.
#[test]
fn twin_table_create_with_table_level_specs_carries_schema_and_guard() {
    let src = r#"
        import { table, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("memberships", { schema: "app2" }).create({
                columns: {
                    account_id: t.uuid().notNull(),
                    team: t.text().notNull(),
                },
                uniques: [{ name: "m_team_uq", columns: ["team"] }],
                indexes: [{ name: "m_account_idx", on: ["account_id"] }],
                ifNotExists: true,
            });
        }};
    "#;
    let ir = record(src, "facade_ct");
    let op = &ops(&ir)[0];
    assert_eq!(op.get("op").unwrap(), "createTable");
    assert_schema(op, "app2");
    assert_guard(op, "ifNotExists");
    assert!(op.get("constraints").and_then(|c| c.as_array()).map(|a| !a.is_empty()).unwrap_or(false));
    assert!(op.get("indexes").and_then(|i| i.as_array()).map(|a| !a.is_empty()).unwrap_or(false));
}

/// A per-method `schema` OVERRIDES the `table()` default through the V8 recorder
/// (precedence: per-call key-present wins; an opts bag without a `schema` key keeps
/// the table default).
#[test]
fn twin_table_per_method_schema_overrides_default() {
    let src = r#"
        import { table, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            const u = table("users", { schema: "app2" });
            u.column("a").add({ type: t.int() });                       // table default
            u.column("b").add({ type: t.int(), ifNotExists: true });     // guard-only → keeps default
            u.column("c").add({ type: t.int(), schema: "other" });       // override
        }};
    "#;
    let ir = record(src, "override");
    let cols: Vec<(&str, Option<&str>)> = ops(&ir)
        .iter()
        .map(|o| {
            (
                o.get("column").and_then(|v| v.as_str()).unwrap(),
                o.get("schema").and_then(|v| v.as_str()),
            )
        })
        .collect();
    assert_eq!(cols[0], ("a", Some("app2")));
    assert_eq!(cols[1], ("b", Some("app2")));
    assert_eq!(cols[2], ("c", Some("other")));
}

// ---------------------------------------------------------------------------
// Migration-first P2a — declared-only facets are CREATE-ONLY + closed-set metric
// ---------------------------------------------------------------------------

/// **HIGH-2** — `t.id({ prefix })` on an `addColumn` (`.column(x).add({type})`) is
/// REFUSED fail-closed, not silently dropped. `Op::AddColumn` has no facet slot, so
/// carrying the prefix through would silently lose the typed-id brand — the one
/// outcome the closed-contract discipline forbids. RED pre-fix: `__toAddColumnTail`
/// emitted `{type,nullable,default}` and dropped `_idPrefix` with no error.
#[test]
fn add_column_with_id_prefix_is_refused_not_dropped() {
    let err = record_err(
        r#"
        import { table, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("posts").column("pid").add({ type: t.id({ prefix: "post" }) });
        }};
    "#,
        "addcol_idprefix",
    );
    assert!(
        err.contains("prefix") && err.contains("create()"),
        "an addColumn carrying a t.id({{prefix}}) must be refused with a create-only \
         OP_INVALID, got: {err}"
    );
}

/// **#173** — `t.vector(n, { metric })` on an `addColumn` is now CARRIED on the op
/// tail (`Op::AddColumn` gained a `vectorMetric` slot), not refused/dropped — a vector
/// ADD COLUMN renders the metric opclass. RED pre-#173: `__toAddColumnTail` THREW a
/// create-only `OP_INVALID` on `_vectorMetric` (the P2a fail-closed this lift removes).
#[test]
fn add_column_with_vector_metric_is_carried_not_refused() {
    let ir = record(
        r#"
        import { table, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("docs").column("emb").add({ type: t.vector(8, { metric: "cosine" }) });
        }};
    "#,
        "addcol_metric",
    );
    let add = &ops(&ir)[0];
    assert_eq!(add.get("op").unwrap(), "addColumn");
    assert_eq!(
        add.get("vectorMetric").and_then(|v| v.as_str()),
        Some("cosine"),
        "an addColumn t.vector(n, {{ metric }}) must CARRY the metric on the op tail \
         (the #173 lift of the P2a fail-closed), got: {add:#}"
    );
}

/// **#174** — a standalone `.mask({ kind, classification })` on an `addColumn` is
/// CARRIED on the op tail (`Op::AddColumn` gained a `mask` slot) so a masked ADD COLUMN
/// emits the `__zsmask` sentinel + `_masked` sibling and keeps the `MaskedValue<T>`
/// brand. RED pre-#174: `ColumnDef` had no `.mask()` method at all (and the op tail had
/// no mask slot), so the facet could not be authored on an added column.
#[test]
fn add_column_with_standalone_mask_is_carried() {
    let ir = record(
        r#"
        import { table, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("people").column("ssn").add({ type: t.text().mask({ kind: "last4", classification: "spi" }) });
        }};
    "#,
        "addcol_mask",
    );
    let add = &ops(&ir)[0];
    assert_eq!(add.get("op").unwrap(), "addColumn");
    let mask = add.get("mask").unwrap_or_else(|| panic!("addColumn must carry mask: {add:#}"));
    assert_eq!(mask.get("kind").and_then(|v| v.as_str()), Some("last4"));
    assert_eq!(mask.get("classification").and_then(|v| v.as_str()), Some("spi"));
}

#[test]
fn generated_and_identity_column_facets_are_recorded_on_create_and_add_column() {
    let ir = record(
        r#"
        import { table, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("line_items").create({ columns: {
                id: t.bigInt().identity({ always: true }).primaryKey(),
                qty: t.int(),
                unit_cents: t.int(),
                total_cents: t.int().generated((c) => c.col("qty").mul(c.col("unit_cents"))),
                virtual_total: t.int().generated((c) => c.col("qty").mul(c.col("unit_cents")), { virtual: true }),
            }});
            table("line_items").column("added_total").add({
                type: t.int().generated((c) => c.col("qty").mul(c.col("unit_cents"))),
            });
            table("line_items").column("seq").add({ type: t.bigInt().identity() });
        }};
    "#,
        "generated_identity_facets",
    );
    let create = &ops(&ir)[0];
    assert_eq!(create.get("op").unwrap(), "createTable");
    let cols = create.get("columns").and_then(|c| c.as_array()).unwrap();
    let by_name = |name: &str| {
        cols.iter()
            .find(|c| c.get("name").and_then(|n| n.as_str()) == Some(name))
            .unwrap_or_else(|| panic!("missing column {name}: {cols:#?}"))
    };
    assert_eq!(by_name("id").get("identity").unwrap(), &serde_json::json!({ "always": true }));
    assert_eq!(
        by_name("total_cents").get("generated").unwrap(),
        &serde_json::json!({
            "expr": {
                "node": "binOp",
                "op": "mul",
                "lhs": { "node": "colRef", "name": "qty" },
                "rhs": { "node": "colRef", "name": "unit_cents" }
            },
            "stored": true
        })
    );
    assert_eq!(
        by_name("virtual_total")
            .get("generated")
            .and_then(|g| g.get("stored"))
            .and_then(|s| s.as_bool()),
        Some(false),
        "generated(expr, {{ virtual: true }}) records stored:false",
    );

    assert_eq!(
        create.get("primaryKey").unwrap(),
        &serde_json::json!(["id"]),
        "primaryKey() records the top-level createTable primaryKey"
    );
    assert!(
        !create
            .get("constraints")
            .and_then(|c| c.as_array())
            .unwrap()
            .iter()
            .any(|c| c.get("kind").and_then(|k| k.get("kind")).and_then(|k| k.as_str()) == Some("pk")),
        "primaryKey() must not hoist a pk constraint"
    );

    let add_generated = &ops(&ir)[1];
    assert_eq!(add_generated.get("op").unwrap(), "addColumn");
    assert!(add_generated.get("generated").is_some(), "addColumn must carry generated");

    let add_identity = &ops(&ir)[2];
    assert_eq!(add_identity.get("op").unwrap(), "addColumn");
    assert_eq!(
        add_identity.get("identity").unwrap(),
        &serde_json::json!({ "always": false }),
        "identity() default records BY DEFAULT"
    );
}

/// A `t.vector(n)` (no metric) on an addColumn is STILL allowed — only the declared
/// metric facet is create-only. Pins that the HIGH-2 reject is scoped to the facet,
/// not the vector column type.
#[test]
fn add_column_plain_vector_is_allowed() {
    let ir = record(
        r#"
        import { table, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("docs").column("emb").add({ type: t.vector(8) });
        }};
    "#,
        "addcol_plain_vector",
    );
    let add = &ops(&ir)[0];
    assert_eq!(add.get("op").unwrap(), "addColumn");
    assert!(add.get("vectorMetric").is_none(), "a metric-less vector carries no facet");
}

/// **LOW-1** — an out-of-set metric is rejected CLIENT-SIDE with a friendly
/// `OP_INVALID` naming the closed set, not deferred to a cryptic serde "unknown
/// variant" at the Rust deserialize seam. RED pre-fix: `t.vector` only `requireString`d
/// the metric and recorded it verbatim.
#[test]
fn vector_metric_out_of_set_is_rejected_client_side() {
    let err = record_err(
        r#"
        import { table, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            table("docs").create({ columns: { emb: t.vector(8, { metric: "euclidean" }) } });
        }};
    "#,
        "bad_metric",
    );
    // The CLIENT-SIDE guard fires at record time with a friendly OP_INVALID naming
    // the call site + closed set ("must be one of …"). It must NOT fall through to
    // the Rust deserialize seam's cryptic "unknown variant" error — that distinction
    // is what makes this RED pre-fix (the serde error ALSO names the variants, so a
    // token-only assertion would falsely pass; we pin the client-side wording).
    assert!(
        err.contains("t.vector(n, { metric })") && err.contains("must be one of"),
        "an out-of-set vector metric must be rejected CLIENT-SIDE with a friendly \
         OP_INVALID naming the closed set, got: {err}"
    );
    assert!(
        !err.contains("unknown variant"),
        "the metric must be caught client-side, NOT deferred to the serde \
         'unknown variant' deserialize error: {err}"
    );
}

/// `table()` with NO schema records ops carrying NO `schema` key.
#[test]
fn twin_table_no_schema_omits_key() {
    let ir = record(
        r#"
        import { table, t } from "@zeroship/migrate";
        export default { name: "n", up() { table("users").column("a").add({ type: t.int() }); }};
    "#,
        "facade_noschema",
    );
    assert!(
        ops(&ir)[0].get("schema").is_none(),
        "no table default ⇒ schema key omitted"
    );
}
