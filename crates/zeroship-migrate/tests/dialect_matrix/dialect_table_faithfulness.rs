//! The dialect-table CORPUS + SIDECAR-DRIFT anchor.
//!
//! The generated single-source dialect table
//! (`tests/dialect_matrix/dialect_table.rs`, emitted from `dialect-support.toml` by
//! `packages/zero-migrate/scripts/gen-dialect-table.mjs`) is the byte-pinned review
//! artifact for the per-op dialect-support decisions. Production asks the selected
//! registered backend's required policy instead; the generated file's lib test
//! compares all 276 reviewed cells with those policies. This file pins the
//! generator-side invariants —
//! see the comment above the test for the full "what guards what".
//!
//! DESIGN — how it enumerates Op × dialect exhaustively:
//!   * A hand-authored REPRESENTATIVE corpus of `(kind, variant, Op)` triples.
//!     Payload-INDEPENDENT ops carry a single `"base"` variant; payload-DEPENDENT
//!     ops (whose support decision turns on a node/option) carry one triple per
//!     distinct support branch, each built to exhibit that branch.
//!   * SHARED VARIANT DERIVATION: each corpus op's `Op::op_variant()` (the same
//!     branch selection production support passes to the backend policy) must
//!     equal its labelled variant — pinning the corpus and the engine against drift.
//!   * EXHAUSTIVENESS over op-KINDS: the corpus's kinds equal the schema's `Op`
//!     `oneOf` discriminants (the 56-op wire contract `op_support_matrix` pins).
//!   * EXHAUSTIVENESS over TABLE ROWS: the corpus's `(kind, variant)` set is a
//!     BIJECTION with the generated `DIALECT_TABLE`'s rows.
//!   * SIDECAR ⟷ TABLE: the generated `DIALECT_TABLE` matches the hand-authored,
//!     human-reviewed `dialect-support.toml` row-for-row (the same drift the TS
//!     `dialect-table-drift` test byte-checks; here checked Rust-side, node-free).
//!   * CENSUS FLOOR over the DIALECT axis: every row declares the same non-empty
//!     set of `DialectId`s, on both sides. See below for why this is not
//!     redundant with the row-for-row comparison.
//!
//! WHY THE CENSUS FLOOR EXISTS. The row used to carry three fields named after
//! vendors, so "every dialect was compared" held BY TYPE. It is now a slice keyed
//! by `DialectId`, and every scan over it iterates a DISCOVERED set — which fails
//! OPEN. Drop a cell from the sidecar and the generator and the table both stop
//! carrying it, the row-for-row comparison compares two rows that agree about the
//! two dialects that remain, and the suite reports clean while the artifact has
//! silently stopped making a claim it used to make. That is not hypothetical: it
//! was MEASURED here by removing one cell from both sides, and the comparison
//! above passed. The floor is what the three fields used to do for free.
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
use crate::dialect_table::{Disposition, DispositionRow, DIALECT_TABLE};
use zeroship_migrate::model::ir::Op;

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
///
/// `dispositions` is keyed by DIALECT ID, matching the generated row. `kind` and
/// `variant` are the only structural keys; every other key in a `[[row]]` is a
/// dialect id, which is what lets a fourth backend add a column without this
/// parser (or the generator) learning its name.
#[derive(Debug, PartialEq, Eq)]
struct SidecarRow {
    kind: String,
    variant: String,
    dispositions: std::collections::BTreeMap<String, String>,
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
        .map(|mut m| {
            let kind = m.remove("kind").expect("row has kind");
            let variant = m.remove("variant").expect("row has variant");
            // Whatever remains is this row's per-dialect dispositions, keyed by id.
            SidecarRow {
                kind,
                variant,
                dispositions: m,
            }
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
// Production support reads the selected registered backend's required policy,
// not this generated table. The generated file's lib test compares all 276 cells
// with those policies, while `op_support_matrix` remains the load-bearing
// behavioural gate (`decision()` == the live validate/lower behaviour). This file
// pins the two generator-side invariants:
//
//   * SHARED VARIANT DERIVATION — every representative corpus op's
//     `Op::op_variant()` equals its labelled variant. Production passes that same
//     branch token to the selected backend policy, so this pins the two against drift.
//   * SIDECAR ⟷ GENERATED TABLE — the committed `dialect_table.rs`'s
//     `DIALECT_TABLE` matches the hand-authored, human-reviewed
//     `dialect-support.toml` (the same sidecar → table drift the TS
//     `dialect-table-drift` test guards with a byte-level regenerate, checked
//     here Rust-side and node-free). Together with the generated 276-cell policy
//     parity test and `op_support_matrix`, this closes the loop: sidecar ⟷ table
//     ⟷ backend policies ⟷ production support ⟷ validate.
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
    //    same derivation production passes to the selected backend policy. This
    //    keeps the corpus and the engine's variant selection from drifting.
    for (kind, variant, op) in &corpus {
        assert_eq!(
            &zeroship_migrate::model::op_support::op_variant(op),
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
            dispositions: row
                .dispositions
                .iter()
                .map(|(id, d)| (id.as_str().to_string(), disposition_token(*d).to_string()))
                .collect(),
        })
        .collect();
    generated.sort_by(|a, b| (a.kind.as_str(), a.variant.as_str()).cmp(&(&b.kind, &b.variant)));
    assert_eq!(
        generated, sidecar,
        "generated dialect_table.rs drifted from dialect-support.toml — regenerate with \
         `pnpm --filter @zeroship/migrate gen:dialect-table`"
    );

    // 4b. CENSUS FLOOR for the dialect axis.
    //
    // The row used to carry three NAMED fields, so "every dialect was compared"
    // was true by TYPE and needed no assertion. It is now a slice keyed by
    // `DialectId`, and every scan over it — the comparison just above, the
    // transparent-degradable sweep just below, `unsupported_reason_is_operator_facing`,
    // and the live conformance suite — iterates a DISCOVERED set. A scan over a
    // discovered set FAILS OPEN: shrink the discovery and it iterates nothing, finds
    // nothing, and reports clean. The comparison above would then be `{}` == `{}`
    // and pass. This floor is what the three fields used to do for free, and it is
    // asserted on BOTH sides, because a cell can go missing on either.
    let census: BTreeSet<String> = DIALECT_TABLE
        .iter()
        .flat_map(|row| row.dialects())
        .map(|id| id.as_str().to_owned())
        .collect();
    assert!(
        census.len() >= 3,
        "the dialect census collapsed to {} ({census:?}); every scan over the table \
         is only as wide as this set",
        census.len()
    );
    assert_eq!(
        census,
        BTreeSet::from([
            "mysql".to_owned(),
            "postgres".to_owned(),
            "sqlite".to_owned()
        ]),
        "the shipping dialect census changed; a backend was added or lost"
    );
    for row in DIALECT_TABLE {
        let row_ids: BTreeSet<String> = row.dialects().map(|id| id.as_str().to_owned()).collect();
        assert_eq!(
            row_ids, census,
            "table row {}/{} declares {:?}, not the table census {census:?} — a row \
             that declares fewer dialects makes no claim where it used to make one",
            row.kind, row.variant, row_ids
        );
    }
    for row in &sidecar {
        let row_ids: BTreeSet<String> = row.dispositions.keys().cloned().collect();
        assert_eq!(
            row_ids, census,
            "sidecar row {}/{} declares {:?}, not the table census {census:?}",
            row.kind, row.variant, row_ids
        );
    }
    // The ids are the CANONICAL `DialectId` spellings, with no aliases. The sidecar
    // said `pg` while every artifact it fed said `postgres`, so `pg → postgres` was
    // an alias in the pipeline — against `DialectId`'s own "no aliases and no
    // display names" rule. Assert the rule, not just today's three names.
    for id in DIALECT_TABLE.iter().flat_map(DispositionRow::dialects) {
        assert!(
            id.is_well_formed(),
            "dialect id {id} in the generated table violates the DialectId rule"
        );
    }

    // TransparentDegradable is not a general escape hatch. It is currently
    // reserved for the explicit partition-collapse affirmation path only.
    let transparent_rows: BTreeSet<(&str, &str)> = DIALECT_TABLE
        .iter()
        .filter(|row| {
            row.dispositions
                .iter()
                .any(|(_, d)| *d == Disposition::TransparentDegradable)
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
