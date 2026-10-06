//! The load gate must stay linear in TOTAL COLUMN COUNT, not just op count.
//!
//! `declare_logical_column` ran a full-map `retain` once per column declared, to
//! supersede any prior declaration of that same logical column. Only entries
//! sharing the exact table and column can ever match, but `LogicalColumnKey`
//! ordered `schema` first, so those siblings were scattered across the map and
//! every declaration paid a traversal of all of it. For N tables of C columns
//! that is (N*C) calls over an (N*C) map.
//!
//! WHY THIS TEST HOLDS THE OP COUNT FIXED. Three sibling passes also scan the
//! whole map, but once per OP -- N*(N*C), which is linear in C. The per-column
//! pass was N^2*C^2, quadratic in C. Varying C with N pinned is therefore the
//! only measurement that tells the two apart: it isolates the axis this fix
//! changed. An op-count sweep (load_gate_op_count_scaling.rs) moves both together and would
//! have reported the fix as a modest constant-factor win.
//!
//! The per-column pass is linear in C, so doubling C does not quadruple the work.
//!
//! THE ASSERTION IS THE SHAPE OF THE CURVE, not a wall-clock budget, for the
//! reason spelled out in load_gate_op_count_scaling.rs: a timing threshold flakes on a loaded
//! machine and says nothing about complexity.
//!
//! `#[ignore]`d and run in the `scaling` CI job, for the reasons load_gate_op_count_scaling.rs
//! sets out in full, including why the 3.0x ceiling is not relaxed and why CPU time
//! was measured and rejected as an instrument.
//!
//! # Why this file now takes the best of five, like its sibling
//!
//! It takes the best of five timings of each shape, like its sibling. The minimum
//! of five cuts BOTH failure modes: a false red, and a real sub-threshold regression
//! hidden inside a wide noise floor. The 3.0x ceiling is untouched.
use std::time::Instant;

/// The best of `REPEATS` timings of `run` - the same instrument, and the same
/// argument for it, as `load_gate_op_count_scaling.rs::best_of`. Scheduler noise, page faults and
/// a busy machine only ever ADD time, so the smallest observation is the closest one
/// to the real cost, and this file compares a RATIO of two such numbers where noise
/// in the denominator inflates the result twice over.
fn best_of(run: impl Fn() -> f64) -> f64 {
    const REPEATS: usize = 5;
    (0..REPEATS).map(|_| run()).fold(f64::INFINITY, f64::min)
}

use zeroship_migrate::model::ir::MigrationIr;
use zeroship_migrate::model::validate::validate_ir;

/// Validate `n` `createTable` ops of `c` columns each, returning seconds.
fn validate_shape(n: usize, c: usize) -> f64 {
    let ops: Vec<String> = (0..n)
        .map(|i| {
            let columns: Vec<String> = (0..c)
                .map(|j| format!(r#"{{"name":"c{j}","type":"bigInt","nullable":false}}"#))
                .collect();
            format!(
                r#"{{"op":"createTable","name":"t{i}","columns":[{}],"primaryKey":["c0"]}}"#,
                columns.join(",")
            )
        })
        .collect();
    let bytes = format!(
        r#"{{"ir_version":1,"name":"scaling","ops":[{}]}}"#,
        ops.join(",")
    );
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("envelope parses");
    let start = Instant::now();
    validate_ir(
        zeroship_migrate::shipping_vendors(),
        &ir,
        &zeroship_migrate_postgres::DIALECT,
    )
    .expect("validates");
    start.elapsed().as_secs_f64()
}

#[test]
#[ignore = "wall-clock complexity guard: needs an idle machine. Runs in the `scaling` CI job via `cargo test -- --ignored`; see this file's header"]
fn validate_ir_does_not_scale_quadratically_in_column_count() {
    const OPS: usize = 3_000;

    // Warm up so allocator and cache effects do not land entirely on the first
    // measured run and inflate the ratio.
    validate_shape(500, 4);

    let narrow = best_of(|| validate_shape(OPS, 4)).max(0.001);
    let wide = best_of(|| validate_shape(OPS, 8));
    let ratio = wide / narrow;

    // The op count is IDENTICAL in both runs, so a well-behaved gate does at most
    // ~2x the work for 2x the columns. Quadratic-in-columns is ~4x; this measured
    // 3.4x before the fix and ~2.0x after. The threshold sits between them.
    assert!(
        ratio < 3.0,
        "doubling the columns per table at a FIXED op count multiplied validate_ir \
         cost by {ratio:.1}x ({narrow:.3}s -> {wide:.3}s). Above ~4x means a pass is \
         scanning the whole declaration map once per column again, which is the \
         quadratic scan the fix removed"
    );
}
