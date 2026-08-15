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
