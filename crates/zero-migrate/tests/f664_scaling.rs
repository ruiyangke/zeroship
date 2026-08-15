//! F664: the load gate's rename-isolation pass was quadratic in op count.
//!
//! `validate_online_rename_isolation_op` kept every operation it had seen in one
//! flat `Vec` and scanned all of it per op, so an envelope of N operations paid
//! N^2 comparisons — even with no renames in it at all. Measured before the fix,
//! that pass alone was 3.5s of a 3.6s validate over 50k ops, while every other
//! pass stayed under 2ms.
//!
//! THE ASSERTION IS THE SHAPE OF THE CURVE, not a wall-clock budget. A timing
//! threshold turns into a flake on a loaded machine and says nothing about
//! complexity; the ratio between two sizes is what distinguishes O(n log n) from
//! O(n^2). Doubling N must not quadruple the work.
use std::time::Instant;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn validate_n(n: usize) -> f64 {
    let ops: Vec<String> = (0..n)
        .map(|i| format!(r#"{{"op":"dropTable","table":"t{i}"}}"#))
        .collect();
    let bytes = format!(r#"{{"ir_version":1,"name":"b","ops":[{}]}}"#, ops.join(","));
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("envelope parses");
    let start = Instant::now();
    validate_ir(&ir, Dialect::Postgres, &[]).expect("validates");
    start.elapsed().as_secs_f64()
}

#[test]
fn validate_ir_does_not_scale_quadratically_in_op_count() {
    // Warm up so allocator and cache effects do not land entirely on the first
    // measured run and inflate the ratio.
    validate_n(2_000);

    let small = validate_n(10_000).max(0.001);
    let large = validate_n(20_000);
    let ratio = large / small;

    // Quadratic would be ~4x for a doubling. Linear-ish is ~2x. The threshold sits
    // between them with room for noise: this caught a 5x before the fix.
    assert!(
        ratio < 3.0,
        "doubling the op count multiplied validate_ir cost by {ratio:.1}x \
         ({small:.3}s -> {large:.3}s). Above ~4x means the per-op cost is growing \
         with N again, which is the quadratic scan F664 removed"
    );
}

/// F666: the same property, but for `createTable`.
///
/// The test above asserts a general-sounding claim -- "validate_ir does not scale
/// quadratically in op count" -- while building its envelope entirely from
/// `dropTable`. `createTable` takes a different path through the declaration
/// map, and that path stayed quadratic long after F664 was fixed: 4000 -> 8000
/// ops cost 0.68s -> 3.10s (4.5x). A guard whose name is broader than its
/// fixture reads as covering ground it never touched.
///
/// Three passes each walked every op, and two helpers reached from those walks
/// scanned the whole declaration map per op: `remove_declared_per_row_table`
/// (superseding a table's prior declarations) and `mutate_table_candidate_keys`
/// (re-deriving candidate keys). Under the table-first key order both groups are
/// contiguous and are now taken with `range`.
///
/// NEITHER FIX ALONE CHANGED THE CLASS. Fixing only the candidate-key helper
/// halved the wall clock and left the ratio at ~4.4x, because the other helper
/// still dominated -- which is why the two landed together and why this asserts
/// the ratio rather than a duration.
#[test]
fn validate_ir_does_not_scale_quadratically_in_create_table_count() {
    fn validate_creates(n: usize) -> f64 {
        let ops: Vec<String> = (0..n)
            .map(|i| {
                format!(
                    r#"{{"op":"createTable","name":"t{i}","columns":[{{"name":"c0","type":"bigInt","nullable":false}},{{"name":"c1","type":"bigInt","nullable":false}}],"primaryKey":["c0"]}}"#
                )
            })
            .collect();
        let bytes = format!(r#"{{"ir_version":1,"name":"c","ops":[{}]}}"#, ops.join(","));
        let ir: MigrationIr = serde_json::from_str(&bytes).expect("envelope parses");
        let start = Instant::now();
        validate_ir(&ir, Dialect::Postgres, &[]).expect("validates");
        start.elapsed().as_secs_f64()
    }

    validate_creates(2_000);

    let small = validate_creates(8_000).max(0.001);
    let large = validate_creates(16_000);
    let ratio = large / small;

    // Measured ~4.5x before the fix and ~2.2x after, on the same machine.
    assert!(
        ratio < 3.0,
        "doubling the createTable count multiplied validate_ir cost by {ratio:.1}x \
         ({small:.3}s -> {large:.3}s). Above ~4x means a pass is scanning the whole \
         declaration map once per op again, which is the quadratic F666 removed"
    );
}
