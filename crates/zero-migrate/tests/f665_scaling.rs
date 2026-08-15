//! F665: the load gate was quadratic in TOTAL COLUMN COUNT, not just op count.
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
//! changed. An op-count sweep (f664_scaling.rs) moves both together and would
//! have reported the fix as a modest constant-factor win.
//!
//! Measured on the fix commit, 4000 ops: doubling C went from ~3.9x to ~2.0x,
//! and 16 columns per table went 54.3s -> 5.8s.
//!
//! THE ASSERTION IS THE SHAPE OF THE CURVE, not a wall-clock budget, for the
//! reason spelled out in f664_scaling.rs: a timing threshold flakes on a loaded
//! machine and says nothing about complexity.
use std::time::Instant;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

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
    validate_ir(&ir, Dialect::Postgres, &[]).expect("validates");
    start.elapsed().as_secs_f64()
}

#[test]
fn validate_ir_does_not_scale_quadratically_in_column_count() {
    const OPS: usize = 3_000;

    // Warm up so allocator and cache effects do not land entirely on the first
    // measured run and inflate the ratio.
    validate_shape(500, 4);

    let narrow = validate_shape(OPS, 4).max(0.001);
    let wide = validate_shape(OPS, 8);
    let ratio = wide / narrow;

    // The op count is IDENTICAL in both runs, so a well-behaved gate does at most
    // ~2x the work for 2x the columns. Quadratic-in-columns is ~4x; this measured
    // 3.4x before the fix and ~2.0x after. The threshold sits between them.
    assert!(
        ratio < 3.0,
        "doubling the columns per table at a FIXED op count multiplied validate_ir \
         cost by {ratio:.1}x ({narrow:.3}s -> {wide:.3}s). Above ~4x means a pass is \
         scanning the whole declaration map once per column again, which is the \
         quadratic F665 removed"
    );
}
