//! PR3 — the FULL `@zeroship/migrate` JS op-builder surface (§3.2/§3.3.1),
//! exercised through the REAL V8 `op.*` recorder. Each test pins one PR3 behavior
//! and would FAIL against the PR1 skeletal builder (which had no `t.*` lexicon, no
//! fluent `(c) => Expr` builder, no `(table, spec)` adders, no determinism lint,
//! and no `OP_OUTSIDE_RECORDER` guard).
//!
//! These complement `op_round_trip.rs` (the golden-corpus value-equality gate over
//! the full fluent fixtures) with targeted, single-behavior regression assertions.

use serde_json::Value;
use zeroship_migrate_js::{lint_migration_determinism, record_migration_to_ir};

const OWNER: &str = "app_pr3";

/// Record a migration source → its recorded IR as a `serde_json::Value` (so a
/// test can assert on the wire ops without re-deserializing the typed IR).
fn record(src: &str, name: &str) -> Value {
    let ir = record_migration_to_ir(src, OWNER, name)
        .unwrap_or_else(|e| panic!("record {name}: {e}"));
    serde_json::to_value(&ir).expect("ir -> value")
}

fn record_err(src: &str, name: &str) -> String {
    record_migration_to_ir(src, OWNER, name)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_else(|| panic!("{name}: expected a recording error, got Ok"))
}

fn ops(ir: &Value) -> &Vec<Value> {
    ir.get("ops").and_then(|o| o.as_array()).expect("ops array")
}

/// `t.text()` records `nullable: true` (nullable by default) and `t.text()`
/// .notNull() records `nullable: false` — the §3.2 nullable-by-default rule.
#[test]
fn t_text_nullable_by_default_notnull_opts_in() {
    let src = r#"
        import { createTable, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            createTable("u", { a: t.text(), b: t.text().notNull() });
        }};
    "#;
    let ir = record(src, "nullable");
    let cols = ops(&ir)[0].get("columns").and_then(|c| c.as_array()).unwrap();
    // Nullable-by-default: `a` OMITS `nullable` (its absence is the dialect default),
    // never records `nullable: false`.
    assert_eq!(cols[0].get("name").unwrap(), "a");
    assert!(
        cols[0].get("nullable").is_none(),
        "t.text() is nullable-by-default; `nullable` must be omitted (got {:?})",
        cols[0]
    );
    // notNull() records the explicit `nullable: false`.
    assert_eq!(cols[1].get("name").unwrap(), "b");
    assert_eq!(
        cols[1].get("nullable").and_then(|n| n.as_bool()),
        Some(false),
        "t.text().notNull() must record nullable: false"
    );
}

/// A `createTable(name, { … })` object-literal map and the `(b) => …` scoped-
/// builder overload both record the same `createTable` op — proving the two
/// table-builder shapes read alike and produce the identical wire op.
#[test]
fn createtable_object_literal_and_builder_overload_record_same_op() {
    let src_obj = r#"
        import { createTable, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            createTable("u", { id: t.uuid().notNull(), email: t.text() });
        }};
    "#;
    // The (b) => … overload with the SAME columns + no extra constraints/indexes
    // must yield a byte-identical createTable op.
    let src_builder = r#"
        import { createTable, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            createTable("u", { id: t.uuid().notNull(), email: t.text() }, (b) => {});
        }};
    "#;
    let a = record(src_obj, "ct_obj");
    let b = record(src_builder, "ct_builder");
    assert_eq!(
        ops(&a)[0],
        ops(&b)[0],
        "the object-literal and (b) => … overloads must record the same createTable op"
    );
}

/// `c.fn.concatWs(" ", c("a"), c("b"))` records a `fnSynth(concatWs)` node — the
/// NULL-skipping safe-join helper (§3.3.1) that renders byte-identically on PG/
/// SQLite. Pinning the node shape here (the apply-identity is in the engine's
/// `ir_dml_*` PG/SQLite suites).
#[test]
fn concatws_records_fnsynth_node() {
    let src = r#"
        import { update } from "@zeroship/migrate";
        export default { name: "n", up() {
            update("u", { set: { full: (c) => c.fn.concatWs(" ", c("a"), c("b")) } });
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
fn addforeignkey_field_order_independent() {
    let src_a = r#"
        import { addForeignKey } from "@zeroship/migrate";
        export default { name: "n", up() {
            addForeignKey("orders", {
                columns: ["customer_id"],
                references: { table: "customers", columns: ["id"] },
                name: "orders_customer_fk",
            });
        }};
    "#;
    // The SAME spec with the fields written in a different order.
    let src_b = r#"
        import { addForeignKey } from "@zeroship/migrate";
        export default { name: "n", up() {
            addForeignKey("orders", {
                name: "orders_customer_fk",
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

/// A migration that omits `name` records the host-supplied filename-derived label
/// (§3.1). The `export default { up }` shape (no `name`) is the common case.
#[test]
fn name_omitted_records_filename_label() {
    let src = r#"
        import { dropTable } from "@zeroship/migrate";
        export default { up() { dropTable("scratch"); } };
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
        import { dropTable } from "@zeroship/migrate";
        // Called at MODULE TOP LEVEL — outside any up()/down() recorder.
        dropTable("oops");
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
        import { dropTable } from "@zeroship/migrate";
        export default { up() { dropTable("oops"); } };
    "#;
    let ir = record(ok_src, "inside");
    assert_eq!(ops(&ir).len(), 1, "the same op inside up() records fine");
}

/// The fluent insert row-OBJECT form normalizes to the frozen columns + positional
/// rows wire shape (column order from the first row's keys).
#[test]
fn insert_row_object_normalizes_to_columns_and_rows() {
    let src = r#"
        import { insert } from "@zeroship/migrate";
        export default { name: "n", up() {
            insert("t", { rows: [ { code: 1, label: "a" }, { code: 2, label: "b" } ] });
        }};
    "#;
    let ir = record(src, "insert_obj");
    let op = &ops(&ir)[0];
    assert_eq!(op.get("columns").unwrap(), &serde_json::json!(["code", "label"]));
    assert_eq!(op.get("rows").unwrap(), &serde_json::json!([[1, "a"], [2, "b"]]));
}

/// The §4.3 determinism lint flags `Date.now()` in an op argument and steers the
/// author to `c.fn.now()`; a clean migration produces NO findings.
#[test]
fn determinism_lint_flags_date_now_in_op_arg() {
    let dirty = r#"
        import { insert } from "@zeroship/migrate";
        export default { name: "n", up() {
            insert("t", { rows: [ { created_at: Date.now() } ] });
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
    assert!(
        f.suggested_fix.contains("c.fn.now()"),
        "steer the author to c.fn.now(): {}",
        f.suggested_fix
    );

    let clean = r#"
        import { insert } from "@zeroship/migrate";
        export default { name: "n", up() {
            insert("t", { rows: [ { created_at: (c) => c.fn.now() } ] });
        }};
    "#;
    assert!(
        lint_migration_determinism(clean).expect("lint runs").is_empty(),
        "the c.fn.now() form must produce NO determinism findings"
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
            import {{ insert }} from "@zeroship/migrate";
            export default {{ name: "n", up() {{
                insert("t", {{ rows: [ {{ v: {src_frag} }} ] }});
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
        import { createTable, addColumn, update, t } from "@zeroship/migrate";
        export default { name: "det", up() {
            createTable("u", { id: t.id(), email: t.text().notNull() });
            addColumn("u", "status", t.text().default("new"));
            update("u", { set: { email: (c) => c.fn.lower(c("email")) }, where: (c) => c("id").isNotNull() });
        }};
    "#;
    let a = zeroship_migrate_js::record_migration_to_json(src, OWNER, "det").unwrap();
    let b = zeroship_migrate_js::record_migration_to_json(src, OWNER, "det").unwrap();
    assert_eq!(a, b, "the same source must record byte-identical .ir.json");
}

/// The fluent `(c) => Expr` builder constructs the closed AST for every operator
/// family (comparison/boolean/arithmetic/cast/unary), proving the headline §3.3.1
/// surface records the same closed-AST nodes the Rust validator/lowerer expect.
#[test]
fn fluent_expr_builder_constructs_closed_ast() {
    let src = r#"
        import { update } from "@zeroship/migrate";
        export default { name: "n", up() {
            update("t", {
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

/// A spec-blessed `bigint` / `Uint8Array` author value passed through the FLUENT
/// insert + column default records the closed `IrScalar` WIRE carriers
/// (`{decimal}` / `{bytes:base64}`), so the RECORD path produces a shape Rust
/// accepts value-equal — the previously promised-but-broken §3.2/§2.3.2 path. A
/// pre-fix recorder either THROWS on the bigint (JSON.stringify) or emits the
/// `{"0":…}` array-index spelling Rust HARD-REJECTS, so `record` would fail.
#[test]
fn fluent_insert_normalizes_bigint_and_bytes_scalars() {
    let src = r#"
        import { createTable, insert, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            createTable("t", {
                id: t.id(),
                seq: t.numeric(38, 0).notNull().default(9007199254740993n),
                salt: t.bytes().default(new Uint8Array([1, 2, 3, 255])),
            });
            insert("t", { rows: [ { seq: 9007199254740993n, salt: new Uint8Array([0, 255]) } ] });
        }};
    "#;
    // Recording succeeds (the typed `MigrationIr` deserialize is the gate) — the
    // scalars came through as the accepted carriers.
    let ir = record(src, "scalars");
    let cols = ops(&ir)[0].get("columns").and_then(|c| c.as_array()).unwrap();
    // seq default -> {literal:{value:{decimal:"9007199254740993"}}}
    let seq_default = cols[1].get("default").unwrap().get("literal").unwrap().get("value").unwrap();
    assert_eq!(seq_default.get("decimal").unwrap(), "9007199254740993");
    // salt default -> {literal:{value:{bytes:"AQID/w=="}}}
    let salt_default = cols[2].get("default").unwrap().get("literal").unwrap().get("value").unwrap();
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
        import { update } from "@zeroship/migrate";
        export default { name: "n", up() {
            update("t", {
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

/// The §4.3 determinism lint is WIRED into the record/build path (not just an inert
/// standalone function): recording a migration whose op argument carries a
/// non-deterministic accessor SURFACES the finding on the record outcome's
/// `warnings` — the pre-commit catch the AI loop self-corrects on (§8.8). Per §4.3
/// it is a WARNING, not a hard reject (the build-once committed artifact already
/// neutralizes post-deploy non-determinism, so the IR is still produced). A
/// pre-wiring recorder would record the `Date.now()` migration with ZERO warnings
/// (the lint never fired on the record path).
#[test]
fn record_path_surfaces_determinism_warnings() {
    use zeroship_migrate_js::record_migration_to_ir_with_warnings;

    let dirty = r#"
        import { insert } from "@zeroship/migrate";
        export default { name: "n", up() {
            insert("t", { rows: [ { created_at: Date.now() } ] });
        }};
    "#;
    // Recording still SUCCEEDS (warn, don't fail-closed) — the IR is produced …
    let outcome = record_migration_to_ir_with_warnings(dirty, OWNER, "dirty")
        .expect("recording a non-deterministic migration still produces an IR (warn, not reject)");
    // … and the wired lint surfaces the structured finding the AI loop steers on.
    assert!(
        !outcome.warnings.is_empty(),
        "the wired record path must surface a determinism warning (pre-wiring it was empty)"
    );
    assert!(outcome.warnings.iter().any(|f| f.accessor.contains("Date.now")));
    assert!(outcome.warnings.iter().all(|f| f.code == "NONDETERMINISTIC_OP_ARG"));
    // The op is actually recorded — recording is not blocked.
    let ir = serde_json::to_value(&outcome.ir).unwrap();
    assert_eq!(ops(&ir)[0].get("op").unwrap(), "insert");

    // The structured `c.fn.now()` replacement records cleanly — NO warnings.
    let clean = r#"
        import { insert } from "@zeroship/migrate";
        export default { name: "n", up() {
            insert("t", { rows: [ { v: 1 } ] });
        }};
    "#;
    let clean_outcome = record_migration_to_ir_with_warnings(clean, OWNER, "clean")
        .expect("clean migration records");
    assert!(
        clean_outcome.warnings.is_empty(),
        "a clean migration surfaces no determinism warnings: {:?}",
        clean_outcome.warnings
    );
}

// ───────────────────────────────────────────────────────────────────────────
// PR10 review F1 (HIGH) — twin-fidelity round-trip.
//
// The engine-embedded V8 recorder (`migrate_ops.js`) is the byte-for-byte twin
// of `sdks/migrate/src/ops.ts`. Before this fix the twin DROPPED the `schema`
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

/// `createTable(name, cols, { schema, ifNotExists })` records BOTH the schema
/// qualifier and the `ifNotExists` create-family guard. RED before the twin fix
/// (the bare `createTable` dropped both — a fail-OPEN unconditional CREATE).
#[test]
fn twin_create_table_carries_schema_and_guard() {
    let src = r#"
        import { createTable, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            createTable("t", { id: t.int() }, { schema: "app2", ifNotExists: true });
        }};
    "#;
    let ir = record(src, "create_schema_guard");
    let op = op_named(&ir, "createTable");
    assert_schema(op, "app2");
    assert_guard(op, "ifNotExists");
}

/// `renameColumn(table, from, to, type, { schema, ifExists })` records the schema
/// qualifier + the `ifExists` alter-family guard. RED before the twin fix.
#[test]
fn twin_rename_column_carries_schema_and_guard() {
    let src = r#"
        import { renameColumn, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            renameColumn("t", "a", "b", t.text(), { schema: "app2", ifExists: true });
        }};
    "#;
    let ir = record(src, "rename_schema_guard");
    let op = op_named(&ir, "renameColumn");
    assert_schema(op, "app2");
    assert_guard(op, "ifExists");
}

/// `alterColumn` with a `type` change records `alterColumnType` carrying the
/// schema qualifier + the `ifExists` guard. RED before the twin fix.
#[test]
fn twin_alter_column_type_carries_schema_and_guard() {
    let src = r#"
        import { alterColumn, t } from "@zeroship/migrate";
        export default { name: "n", up() {
            alterColumn("t", "a", { type: t.bigInt() }, { schema: "app2", ifExists: true });
        }};
    "#;
    let ir = record(src, "alter_type_schema_guard");
    let op = op_named(&ir, "alterColumnType");
    assert_schema(op, "app2");
    assert_guard(op, "ifExists");
}

/// `alterColumn` with a `nullable` change records `alterColumnNullability`
/// carrying the schema qualifier + the `ifExists` guard. RED before the twin fix.
#[test]
fn twin_alter_column_nullability_carries_schema_and_guard() {
    let src = r#"
        import { alterColumn } from "@zeroship/migrate";
        export default { name: "n", up() {
            alterColumn("t", "a", { nullable: false }, { schema: "app2", ifExists: true });
        }};
    "#;
    let ir = record(src, "alter_null_schema_guard");
    let op = op_named(&ir, "alterColumnNullability");
    assert_schema(op, "app2");
    assert_guard(op, "ifExists");
}

/// `addForeignKey` / `addUnique` / `addCheck` all record an `addConstraint` op
/// carrying the schema qualifier + the `ifNotExists` add-family guard. RED before
/// the twin fix.
#[test]
fn twin_add_constraint_family_carries_schema_and_guard() {
    let fk = r#"
        import { addForeignKey } from "@zeroship/migrate";
        export default { name: "n", up() {
            addForeignKey("t", { columns: ["o"], references: { table: "o", columns: ["id"] } },
                { schema: "app2", ifNotExists: true });
        }};
    "#;
    let fk_ir = record(fk, "fk_schema_guard");
    let op = op_named(&fk_ir, "addConstraint");
    assert_schema(op, "app2");
    assert_guard(op, "ifNotExists");

    let uq = r#"
        import { addUnique } from "@zeroship/migrate";
        export default { name: "n", up() {
            addUnique("t", { columns: ["a"] }, { schema: "app2", ifNotExists: true });
        }};
    "#;
    let uq_ir = record(uq, "uq_schema_guard");
    let op = op_named(&uq_ir, "addConstraint");
    assert_schema(op, "app2");
    assert_guard(op, "ifNotExists");

    let ck = r#"
        import { addCheck } from "@zeroship/migrate";
        export default { name: "n", up() {
            addCheck("t", { expr: (c) => c("a").gt(0) }, { schema: "app2", ifNotExists: true });
        }};
    "#;
    let ck_ir = record(ck, "ck_schema_guard");
    let op = op_named(&ck_ir, "addConstraint");
    assert_schema(op, "app2");
    assert_guard(op, "ifNotExists");
}

/// `dropConstraint(table, name, { schema, ifExists })` records the schema
/// qualifier + the `ifExists` drop-family guard. RED before the twin fix.
#[test]
fn twin_drop_constraint_carries_schema_and_guard() {
    let src = r#"
        import { dropConstraint } from "@zeroship/migrate";
        export default { name: "n", up() {
            dropConstraint("t", "t_a_key", { schema: "app2", ifExists: true });
        }};
    "#;
    let dc_ir = record(src, "drop_constraint_schema_guard");
    let op = op_named(&dc_ir, "dropConstraint");
    assert_schema(op, "app2");
    assert_guard(op, "ifExists");
}

/// The DML ops `insert` / `update` / `delete` / `backfill` carry the schema
/// qualifier (no existence guard — DML is not guardable). RED before the twin fix
/// (the schema was silently dropped, re-pinning the op to the project schema).
#[test]
fn twin_dml_ops_carry_schema() {
    let ins = r#"
        import { insert } from "@zeroship/migrate";
        export default { name: "n", up() {
            insert("t", { rows: [{ a: 1 }], schema: "app2" });
        }};
    "#;
    assert_schema(&op_named(&record(ins, "insert_schema"), "insert"), "app2");

    let upd = r#"
        import { update } from "@zeroship/migrate";
        export default { name: "n", up() {
            update("t", { set: { a: (c) => c("a") }, schema: "app2" });
        }};
    "#;
    assert_schema(&op_named(&record(upd, "update_schema"), "update"), "app2");

    let del = r#"
        import { del } from "@zeroship/migrate";
        export default { name: "n", up() {
            del("t", { where: (c) => c("a").gt(0), schema: "app2" });
        }};
    "#;
    assert_schema(&op_named(&record(del, "delete_schema"), "delete"), "app2");

    let bf = r#"
        import { backfill } from "@zeroship/migrate";
        export default { name: "n", up() {
            backfill("t", { set: { a: (c) => c("a") }, schema: "app2" });
        }};
    "#;
    assert_schema(&op_named(&record(bf, "backfill_schema"), "backfill"), "app2");
}
