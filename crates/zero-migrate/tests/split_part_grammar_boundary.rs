//! The two-list separation: `instr` is added to the `SQLite` AUTHORIZER
//! allow-list ONLY (for the engine's pinned `splitPart` lowering), NOT to the
//! portable-expression grammar a creator authors against. So a creator-authored
//! raw `instr` / `substr` / `split_part` named function is REJECTED at IR load
//! (the closed `ScalarFn` enum has no such variant — there is no raw escape,
//! property A), while the engine-synthesized in-envelope `.splitPart`
//! (`FnSynth(splitPart)`) is accepted. The boundary is precise: the authorizer
//! gates which builtins the migrator role may EXECUTE; the grammar gates which
//! functions an author may BUILD; they are distinct lists.

use std::collections::BTreeMap;
use zero_migrate::model::expr::{Expr, SynthFn};
use zero_migrate::model::ir::{IrScalar, IrValue};
use zero_migrate::model::load::load_ir_document;
use zero_migrate::model::validate::Dialect;
use zero_migrate::render::dml::assemble_backfill_clauses;
use zero_migrate::SqlDialect;

const APP: &str = "app_grammar";

/// A `.splitPart` synth node for the render-path probes.
fn split(col: &str, delim: &str, n: i64) -> Expr {
    Expr::FnSynth {
        r#fn: SynthFn::SplitPart,
        args: vec![
            Expr::ColRef {
                name: col.into(),
                table: None,
            },
            Expr::Literal {
                value: IrScalar::Str(delim.into()),
            },
            Expr::Literal {
                value: IrScalar::Int(n),
            },
        ],
    }
}

fn registry() -> std::collections::BTreeMap<String, String> {
    [("t".to_string(), APP.to_string())].into_iter().collect()
}

/// A raw author-named `instr` / `split_part` `fnCall` is NOT in the
/// portable-expression grammar (the closed `ScalarFn` enum) — it fails to load on
/// EITHER dialect. There is no raw escape: an author cannot name `instr` even
/// though the engine's own lowering uses it (the two lists are distinct). NB:
/// `substr`/`replace` are NO LONGER in this list — they are now first-class
/// portable `ScalarFn`s, so a `fnCall` naming them loads fine.
#[test]
fn raw_split_funcs_rejected_at_load_both_dialects() {
    for raw_fn in ["instr", "split_part"] {
        let ir = format!(
            r#"{{"ir_version":1,"name":"raw","ops":[
                {{"op":"update","table":"t",
                  "set":{{"x":{{"node":"fnCall","fn":"{raw_fn}","args":[
                      {{"node":"colRef","name":"v"}},{{"node":"literal","value":","}}]}}}}}}
            ]}}"#
        );
        for dialect in [Dialect::Postgres, Dialect::Sqlite] {
            let err = load_ir_document(&ir, APP, dialect, &registry(), None, None).expect_err(
                &format!("raw `{raw_fn}` must be rejected at load on {dialect:?}"),
            );
            // The closed ScalarFn enum has no such variant → a deserialize/contract
            // rejection. (Never a silent acceptance that would later mis-apply.)
            let msg = err.to_string();
            assert!(
                !msg.is_empty(),
                "raw `{raw_fn}` on {dialect:?} must surface a structured load error"
            );
        }
    }
}

/// The engine-synthesized in-envelope `.splitPart` (`FnSynth(splitPart)`) IS
/// accepted — it loads on both dialects (the `SQLite` leg is PG-renderable + in the
/// pinned envelope). This is the ONLY split surface; it contrasts with the raw
/// rejection above.
#[test]
fn in_envelope_split_part_helper_accepted() {
    let ir = r#"{"ir_version":1,"name":"ok","ops":[
        {"op":"update","table":"t",
         "set":{"x":{"node":"fnSynth","fn":"splitPart","args":[
             {"node":"colRef","name":"v"},{"node":"literal","value":","},{"node":"literal","value":1}]}}}
    ]}"#;
    for dialect in [Dialect::Postgres, Dialect::Sqlite] {
        load_ir_document(ir, APP, dialect, &registry(), None, None)
            .unwrap_or_else(|e| panic!("in-envelope .splitPart must load on {dialect:?}: {e}"));
    }
}

/// An OUT-of-envelope `FnSynth(splitPart)` is rejected on the `SQLite` leg
/// (`EXPR_NOT_PORTABLE`) but loads on PG — the dialect-gated verdict, now at
/// its expression level for splitPart (multi-char delim).
#[test]
fn out_of_envelope_split_part_pg_loads_sqlite_rejected() {
    let ir = r#"{"ir_version":1,"name":"oob","ops":[
        {"op":"update","table":"t",
         "set":{"x":{"node":"fnSynth","fn":"splitPart","args":[
             {"node":"colRef","name":"v"},{"node":"literal","value":", "},{"node":"literal","value":1}]}}}
    ]}"#;
    load_ir_document(ir, APP, Dialect::Postgres, &registry(), None, None)
        .expect("out-of-envelope splitPart is PG-renderable → loads on PG");
    let err = load_ir_document(ir, APP, Dialect::Sqlite, &registry(), None, None)
        .expect_err("out-of-envelope splitPart must reject on SQLite");
    assert!(
        err.to_string().contains("EXPR_NOT_PORTABLE")
            || err.to_string().to_lowercase().contains("portable"),
        "the SQLite leg must reject with EXPR_NOT_PORTABLE; got: {err}"
    );
}

/// The RENDER-path companion to the load-only test above.
/// The grammar test proved an out-of-envelope splitPart LOADS on PG; this proves it
/// actually LOWERS to native `split_part(col, 'delim', n)` on a Postgres target
/// (the `dialect_scope=PgOnly` escape) instead of hard-erroring at
/// render — and that the SAME node still rejects at lower on a `SQLite` target.
#[test]
fn out_of_envelope_split_part_lowers_native_on_pg_rejects_on_sqlite() {
    // multi-char delimiter, the grammar-boundary example.
    let set = BTreeMap::from([("x".to_string(), IrValue::Expr(split("v", ", ", 1)))]);
    let c = assemble_backfill_clauses(SqlDialect::Postgres, "t", &set, None)
        .expect("out-of-envelope splitPart must LOWER to native split_part on PG");
    assert_eq!(c.set_clause, "\"x\" = split_part(\"v\", ', ', 1)");

    // the same node is unrenderable on the SQLite leg (out of the byte-wise envelope).
    let err = assemble_backfill_clauses(SqlDialect::Sqlite, "t", &set, None)
        .expect_err("out-of-envelope splitPart must reject at lower on SQLite");
    assert!(
        err.to_string().to_lowercase().contains("sqlite")
            || err.to_string().to_lowercase().contains("ascii"),
        "the SQLite leg rejects out-of-envelope; got: {err}"
    );
}
