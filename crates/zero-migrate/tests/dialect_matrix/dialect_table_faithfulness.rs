//! The dialect-table CORPUS + SIDECAR-DRIFT anchor.
//!
//! The generated single-source dialect table
//! (`src/model/dialect_table.rs`, emitted from `dialect-support.toml` by
//! `packages/zero-migrate/scripts/gen-dialect-table.mjs`) is the single source of the
//! per-op dialect-support decisions, and `Op::support` READS the
//! table (via [`Op::op_variant`]). This file pins the invariants that stay
//! meaningful once the table is authoritative —
//! see the comment above the test for the full "what guards what".
//!
//! DESIGN — how it enumerates Op × dialect exhaustively:
//!   * A hand-authored REPRESENTATIVE corpus of `(kind, variant, Op)` triples.
//!     Payload-INDEPENDENT ops carry a single `"base"` variant; payload-DEPENDENT
//!     ops (whose support decision turns on a node/option) carry one triple per
//!     distinct support branch, each built to exhibit that branch.
//!   * SHARED VARIANT DERIVATION: each corpus op's `Op::op_variant()` (the ONE
//!     branch-selection `Op::support` keys the table lookup on) must equal its
//!     labelled variant — pinning the corpus and the engine against drift.
//!   * EXHAUSTIVENESS over op-KINDS: the corpus's kinds equal the schema's `Op`
//!     `oneOf` discriminants (the 56-op wire contract `op_support_matrix` pins).
//!   * EXHAUSTIVENESS over TABLE ROWS: the corpus's `(kind, variant)` set is a
//!     BIJECTION with the generated `DIALECT_TABLE`'s rows.
//!   * SIDECAR ⟷ TABLE: the generated `DIALECT_TABLE` matches the hand-authored,
//!     human-reviewed `dialect-support.toml` row-for-row (the same drift the TS
//!     `dialect-table-drift` test byte-checks; here checked Rust-side, node-free).
//!
//! The disposition vocabulary is portable / vendor (both supported cells),
//! transparentDegradable, and unsupported. `transparentDegradable` is LIVE, not
//! reserved: two rows carry it on both non-PostgreSQL dialects
//! (`createPartition/base` and `createTable/partitionedCollapse`), `op_support.rs`
//! groups it with portable and vendor as SUPPORTED, and lowering collapses a
//! partition child into its parent behind a mirror guard rather than creating a
//! relation. The sidecar's own legend claimed the opposite until it was measured.
//!
//! The CORPUS itself now lives in `tests/dialect_corpus/mod.rs`, unchanged, so the
//! live conformance layer (`dialect_conformance_live.rs`) drives the SAME
//! representative ops this file proves the bijection over. Two corpora could
//! drift; one cannot. The guarantees below are exactly the ones this file always
//! made.

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::dialect_corpus::corpus;
use zero_migrate::model::dialect_table::{Disposition, DIALECT_TABLE};
use zero_migrate::model::ir::Op;

/// The `op` wire tag (op-kind discriminant) of a concrete op, via its serde image.
fn op_tag(op: &Op) -> String {
    serde_json::to_value(op)
        .expect("op serializes")
        .get("op")
        .and_then(|v| v.as_str())
        .expect("op tag is present")
        .to_string()
}

fn sidecar_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("dialect-support.toml")
}

/// A parsed `[[row]]` of the hand-authored sidecar (kind, variant, per-dialect
/// disposition), read with the SAME restricted grammar the generator
/// (`gen-dialect-table.mjs`) enforces: blank lines, `#` comments, `[[row]]`
/// headers, and `key = "string"` assignments only.
#[derive(Debug, PartialEq, Eq)]
struct SidecarRow {
    kind: String,
    variant: String,
    pg: String,
    sqlite: String,
    mysql: String,
}

fn parse_sidecar() -> Vec<SidecarRow> {
    let text = std::fs::read_to_string(sidecar_path()).expect("read dialect-support.toml");
    let mut rows: Vec<std::collections::BTreeMap<String, String>> = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[row]]" {
            rows.push(std::collections::BTreeMap::new());
            continue;
        }
        let (key, rest) = line.split_once('=').unwrap_or_else(|| {
            panic!(
                "dialect-support.toml:{}: not a key = value line: {raw:?}",
                i + 1
            )
        });
        let key = key.trim().to_string();
        // strip an optional trailing `# comment`, then the surrounding quotes.
        let value_part = rest.split('#').next().unwrap_or("").trim();
        let value = value_part
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or_else(|| {
                panic!(
                    "dialect-support.toml:{}: value is not a quoted string: {raw:?}",
                    i + 1
                )
            })
            .to_string();
        let cur = rows.last_mut().unwrap_or_else(|| {
            panic!(
                "dialect-support.toml:{}: key before any [[row]] header",
                i + 1
            )
        });
        cur.insert(key, value);
    }
    rows.into_iter()
        .map(|mut m| SidecarRow {
            kind: m.remove("kind").expect("row has kind"),
            variant: m.remove("variant").expect("row has variant"),
            pg: m.remove("pg").expect("row has pg"),
            sqlite: m.remove("sqlite").expect("row has sqlite"),
            mysql: m.remove("mysql").expect("row has mysql"),
        })
        .collect()
}

const fn disposition_token(disposition: Disposition) -> &'static str {
    match disposition {
        Disposition::Portable => "portable",
        Disposition::TransparentDegradable => "transparentDegradable",
        Disposition::Vendor => "vendor",
        Disposition::Unsupported => "unsupported",
    }
}

fn schema_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ir-envelope.schema.json")
}

/// The `Op` discriminant tokens the schema declares (the 56-op wire contract).
fn schema_op_tags() -> BTreeSet<String> {
    let schema: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(schema_path()).expect("read ir-envelope.schema.json"),
    )
    .expect("parse ir-envelope.schema.json");
    schema
        .get("$defs")
        .and_then(|d| d.get("Op"))
        .and_then(|o| o.get("oneOf"))
        .and_then(|o| o.as_array())
        .expect("Op oneOf branches")
        .iter()
        .filter_map(|branch| {
            branch
                .get("properties")
                .and_then(|p| p.get("op"))
                .and_then(|t| t.get("const"))
                .and_then(|c| c.as_str())
                .map(str::to_string)
        })
        .collect()
}

// What now guards what.
//
// `Op::support` READS the dialect table (looking the disposition up by
// `Op::op_variant`), so a direct `generated DIALECT_TABLE == decision_to_disposition(
// Op::support())` check would be TAUTOLOGICAL. The load-bearing behavioural gate
// lives in `op_support_matrix` (`decision()` == the live validate/lower behaviour).
// This file pins the TWO invariants that remain meaningful once the table
// is the consumer's source of truth:
//
//   * SHARED VARIANT DERIVATION — every representative corpus op's
//     `Op::op_variant()` equals its labelled variant. `op_variant` is the single
//     branch-selection shared by `Op::support` and this corpus, so this pins the
//     two against drift.
//   * SIDECAR ⟷ GENERATED TABLE — the committed `dialect_table.rs`'s
//     `DIALECT_TABLE` matches the hand-authored, human-reviewed
//     `dialect-support.toml` (the same sidecar → table drift the TS
//     `dialect-table-drift` test guards with a byte-level regenerate, checked
//     here Rust-side and node-free). Together with `op_support_matrix` this closes
//     the loop: sidecar ⟷ table ⟷ (op_variant∘table) `Op::support` ⟷ validate.
#[test]
fn op_variant_matches_the_corpus_and_the_generated_table_matches_the_sidecar() {
    let corpus = corpus();

    // 1. Exhaustiveness over op-KINDS: the corpus covers exactly the schema's Op
    //    discriminants (the 56-op wire contract). No op silently uncovered.
    let corpus_kinds: BTreeSet<String> = corpus.iter().map(|(k, _, _)| (*k).to_string()).collect();
    let schema_kinds = schema_op_tags();
    assert_eq!(
        schema_kinds.len(),
        56,
        "the wire contract must still carry the closed 56-op discriminant set"
    );
    assert_eq!(
        corpus_kinds, schema_kinds,
        "faithfulness corpus op-kinds must equal the schema's Op discriminants"
    );

    // 2. SHARED VARIANT DERIVATION: each representative op reports the labelled
    //    variant AND kind through the crate's `Op::op_variant` / serde tag — the
    //    same derivation `Op::support` uses to key the table lookup. This is what
    //    keeps the corpus and the engine's variant selection from drifting.
    for (kind, variant, op) in &corpus {
        assert_eq!(
            &zero_migrate::model::op_support::op_variant(op),
            variant,
            "corpus labels {kind}/{variant} but op_variant() disagrees"
        );
        assert_eq!(
            &op_tag(op).as_str(),
            kind,
            "corpus labels kind {kind} but the op's serde tag disagrees"
        );
    }

    // 3. Exhaustiveness over TABLE ROWS: the corpus's (kind, variant) pairs are a
    //    BIJECTION with the generated DIALECT_TABLE rows. Every table row is
    //    exercised by a representative op; every corpus case has a row.
    let corpus_pairs: BTreeSet<(String, String)> = corpus
        .iter()
        .map(|(k, v, _)| ((*k).to_string(), (*v).to_string()))
        .collect();
    assert_eq!(
        corpus_pairs.len(),
        corpus.len(),
        "faithfulness corpus must not contain duplicate (kind, variant) entries"
    );
    let table_pairs: BTreeSet<(String, String)> = DIALECT_TABLE
        .iter()
        .map(|row| (row.kind.to_string(), row.variant.to_string()))
        .collect();
    assert_eq!(
        corpus_pairs, table_pairs,
        "generated DIALECT_TABLE rows must be a bijection with the faithfulness corpus"
    );

    // 4. SIDECAR ⟷ TABLE: the generated const table matches the human-reviewed
    //    sidecar row-for-row (kind, variant, and each dialect's disposition token),
    //    so the committed `dialect_table.rs` cannot be hand-edited to diverge from
    //    its single source.
    let mut sidecar: Vec<SidecarRow> = parse_sidecar();
    sidecar.sort_by(|a, b| (a.kind.as_str(), a.variant.as_str()).cmp(&(&b.kind, &b.variant)));
    let mut generated: Vec<SidecarRow> = DIALECT_TABLE
        .iter()
        .map(|row| SidecarRow {
            kind: row.kind.to_string(),
            variant: row.variant.to_string(),
            pg: disposition_token(row.postgres).to_string(),
            sqlite: disposition_token(row.sqlite).to_string(),
            mysql: disposition_token(row.mysql).to_string(),
        })
        .collect();
    generated.sort_by(|a, b| (a.kind.as_str(), a.variant.as_str()).cmp(&(&b.kind, &b.variant)));
    assert_eq!(
        generated, sidecar,
        "generated dialect_table.rs drifted from dialect-support.toml — regenerate with \
         `pnpm --filter zero-migrate gen:dialect-table`"
    );

    // TransparentDegradable is not a general escape hatch. It is currently
    // reserved for the explicit partition-collapse affirmation path only.
    let transparent_rows: BTreeSet<(&str, &str)> = DIALECT_TABLE
        .iter()
        .filter(|row| {
            row.postgres == Disposition::TransparentDegradable
                || row.sqlite == Disposition::TransparentDegradable
                || row.mysql == Disposition::TransparentDegradable
        })
        .map(|row| (row.kind, row.variant))
        .collect();
    assert_eq!(
        transparent_rows,
        BTreeSet::from([
            ("createPartition", "base"),
            ("createTable", "partitionedCollapse"),
        ]),
        "transparent-degradable rows must stay limited to explicit partition collapse"
    );
}
