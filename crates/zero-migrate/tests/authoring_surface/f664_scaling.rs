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
//!
//! # Why every test here is `#[ignore]`, and where they DO run
//!
//! A ratio of two timings is still a timing, and this file produced false failures
//! repeatedly on a loaded machine: 3.6x/3.6x/3.8x at load average 26 and
//! 3.5x/3.6x/3.9x at load average 30, a DIFFERENT arm each time, each one green on
//! an idle re-run. Nothing was wrong with the code under test on any of those runs.
//!
//! Two things follow, and only one of them is about this file. A false red teaches
//! people to re-run builds instead of reading them, which is corrosive on its own.
//! And a live-database conformance matrix multiplies load on the same runners, so a
//! known flake sitting next to a NEW suite gets the new suite blamed for it.
//!
//! So these run in their own CI job (`scaling` in `.github/workflows/ci.yml`) with no
//! service containers and nothing else on the machine, reached with
//! `cargo test -- --ignored`. `#[ignore]` rather than a cargo feature ON PURPOSE:
//! an ignored test is still COMPILED and still linted by
//! `cargo clippy --all-targets`, so it cannot rot into something that no longer
//! builds while nobody is looking. A `required-features` gate would hide it from
//! both.
//!
//! # The threshold is NOT weakened, and CPU time was measured and rejected
//!
//! Raising the 3.0x ceiling would discard the signal these guards exist for - they
//! caught a 5x, a 4.5x and a 4.53x, all of which sit above a "relaxed" ceiling too.
//!
//! Switching the instrument from wall clock to CPU time is the obvious other idea,
//! and it was MEASURED rather than assumed. On a 16-core machine at load average
//! 20-35, the same fixtures timed simultaneously with `Instant` and with per-thread
//! CPU nanoseconds (`/proc/thread-self/schedstat`) agreed to within about 1% on
//! EVERY arm:
//!
//!   dropColumn   wall 2.232 / cpu 2.230, 2.203 / 2.197, 2.175 / 2.182
//!   renameTable  wall 2.111 / cpu 2.111, 2.121 / 2.114, 2.120 / 2.123
//!   renameColumn wall 2.079 / cpu 2.089, 2.048 / 2.062, 2.030 / 2.037
//!   dropTable    wall 1.744 / cpu 1.748, 2.094 / 2.095, 2.187 / 2.191
//!
//! `best_of` already takes the MINIMUM of five runs, and a single CPU-bound thread on
//! a 16-core box gets a whole core in at least one of five attempts even with a long
//! run queue - so preemption is not what inflated those ratios. What inflates them is
//! that the work itself gets more expensive under memory and I/O pressure (page
//! faults, allocator behaviour, cache pressure), and a CPU clock counts that extra
//! work just as faithfully as a wall clock does. Changing clocks would have added a
//! platform-specific dependency and moved no number.
use std::time::Instant;

/// The best of `REPEATS` timings of `run`.
///
/// MINIMUM, not mean or a single shot, and the reason is what this file is for.
/// Scheduler noise, page faults and a busy machine only ever ADD time, so the
/// smallest observation is the closest one to the real cost. A single timing of a
/// ~20ms body is noisy enough that a linear pass can measure 3x on a loaded
/// machine, and this suite compares a RATIO of two such numbers - noise in the
/// denominator inflates the result twice over.
///
/// That cuts BOTH ways and both matter: a spurious failure wastes a CI run and
/// teaches people to re-run red builds, while a noise floor that wide can hide a
/// genuine sub-threshold regression. This guard exists to detect the second, so
/// its own precision is load-bearing.
fn best_of(run: impl Fn() -> f64) -> f64 {
    const REPEATS: usize = 5;
    (0..REPEATS).map(|_| run()).fold(f64::INFINITY, f64::min)
}

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn validate_n(n: usize) -> f64 {
    let ops: Vec<String> = (0..n)
        .map(|i| format!(r#"{{"op":"dropTable","table":"t{i}"}}"#))
        .collect();
    let bytes = format!(r#"{{"ir_version":1,"name":"b","ops":[{}]}}"#, ops.join(","));
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("envelope parses");
    let start = Instant::now();
    validate_ir(&ir, Dialect::Postgres).expect("validates");
    start.elapsed().as_secs_f64()
}

#[test]
#[ignore = "wall-clock complexity guard: needs an idle machine. Runs in the `scaling` CI job via `cargo test -- --ignored`; see this file's header"]
fn validate_ir_does_not_scale_quadratically_in_op_count() {
    // Warm up so allocator and cache effects do not land entirely on the first
    // measured run and inflate the ratio.
    validate_n(2_000);

    let small = best_of(|| validate_n(10_000)).max(0.001);
    let large = best_of(|| validate_n(20_000));
    let ratio = large / small;

    // Quadratic would be ~4x for a doubling. Linear-ish is ~2x. The threshold sits
    // between them with room for noise: this caught a 5x before the fix.
    assert!(
        ratio < 3.0,
        "doubling the op count multiplied validate_ir cost by {ratio:.1}x \
         ({small:.3}s -> {large:.3}s), over the 3.0x ceiling this guard holds. \
         Quadratic is ~4x for a doubling and linear-ish is ~2x, so a ratio this \
         high means the per-op cost is growing with N again - the quadratic scan \
         F664 removed. Re-run on an IDLE machine before believing it: these are \
         wall-clock ratios, and heavy background load inflates them"
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
#[ignore = "wall-clock complexity guard: needs an idle machine. Runs in the `scaling` CI job via `cargo test -- --ignored`; see this file's header"]
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
        validate_ir(&ir, Dialect::Postgres).expect("validates");
        start.elapsed().as_secs_f64()
    }

    validate_creates(2_000);

    let small = best_of(|| validate_creates(8_000)).max(0.001);
    let large = best_of(|| validate_creates(16_000));
    let ratio = large / small;

    // Measured ~4.5x before the fix and ~2.2x after, on the same machine.
    assert!(
        ratio < 3.0,
        "doubling the createTable count multiplied validate_ir cost by {ratio:.1}x \
         ({small:.3}s -> {large:.3}s), over the 3.0x ceiling this guard holds. A \
         ratio this high means a pass is scanning the whole declaration map once \
         per op again - the quadratic F666 removed. Re-run on an IDLE machine \
         before believing it: these are wall-clock ratios, and heavy background \
         load inflates them"
    );
}

/// F667: the same property again, across EVERY op kind that mutates the
/// declaration map -- because the two tests above each pinned exactly one.
///
/// `dropColumn` and `renameTable` were still quadratic (4.53x each) after F666
/// was fixed, and nothing failed: the `dropTable` guard was green, the
/// `createTable` guard was green, and the defect sat in the ops neither one
/// exercised. Adding a third single-kind test would have repeated the mistake,
/// so this sweeps the kinds instead and reports EVERY offender in one run rather
/// than stopping at the first.
///
/// Measured before the fix: dropColumn 0.604s -> 2.739s, renameTable 0.933s ->
/// 4.224s. After: 0.037s -> 0.083s and 0.045s -> 0.097s.
///
/// `renameColumn` builds its envelope differently on purpose. The rename
/// isolation rule refuses a `renameColumn` beside any other operation on the same
/// table, so pairing it with a `createTable` the way the other kinds are paired
/// produces OP_INVALID rather than a measurement -- an invalid fixture that would
/// otherwise have been read as "this kind is fine".
#[test]
#[ignore = "wall-clock complexity guard: needs an idle machine. Runs in the `scaling` CI job via `cargo test -- --ignored`; see this file's header"]
fn validate_ir_does_not_scale_quadratically_in_any_op_kind() {
    /// `n` ops of `kind`, half seeding tables where the kind needs one.
    fn envelope(kind: &str, n: usize) -> String {
        let half = n / 2;
        if kind == "renameColumn" {
            let ops: Vec<String> = (0..n)
                .map(|i| {
                    format!(
                        r#"{{"op":"renameColumn","table":"t{i}","from":"c1","to":"d1","type":"bigInt"}}"#
                    )
                })
                .collect();
            return format!(r#"{{"ir_version":1,"name":"k","ops":[{}]}}"#, ops.join(","));
        }
        let mut ops: Vec<String> = (0..half)
            .map(|i| {
                format!(
                    r#"{{"op":"createTable","name":"t{i}","columns":[{{"name":"c0","type":"bigInt","nullable":false}},{{"name":"c1","type":"bigInt","nullable":true}}],"primaryKey":["c0"]}}"#
                )
            })
            .collect();
        for i in 0..half {
            ops.push(match kind {
                "addColumn" => format!(
                    r#"{{"op":"addColumn","table":"t{i}","column":"c2","type":"text","nullable":true}}"#
                ),
                "dropColumn" => format!(r#"{{"op":"dropColumn","table":"t{i}","column":"c1"}}"#),
                "renameTable" => format!(r#"{{"op":"renameTable","table":"t{i}","to":"r{i}"}}"#),
                "createIndex" => format!(
                    r#"{{"op":"createIndex","name":"ix{i}","table":"t{i}","columns":[{{"kind":"column","name":"c0"}}]}}"#
                ),
                "dropTable" => format!(r#"{{"op":"dropTable","table":"t{i}"}}"#),
                other => unreachable!("unhandled op kind {other}"),
            });
        }
        format!(r#"{{"ir_version":1,"name":"k","ops":[{}]}}"#, ops.join(","))
    }

    fn measure(kind: &str, n: usize) -> f64 {
        let ir: MigrationIr = serde_json::from_str(&envelope(kind, n)).expect("envelope parses");
        let start = Instant::now();
        // A fixture the validator REFUSES measures nothing. Fail loudly here
        // rather than silently timing an early return.
        validate_ir(&ir, Dialect::Postgres)
            .unwrap_or_else(|e| panic!("{kind} fixture must be valid to measure anything: {e:?}"));
        start.elapsed().as_secs_f64()
    }

    const KINDS: [&str; 6] = [
        "addColumn",
        "dropColumn",
        "renameTable",
        "renameColumn",
        "createIndex",
        "dropTable",
    ];

    let mut quadratic = Vec::new();
    for kind in KINDS {
        measure(kind, 2_000);
        let small = best_of(|| measure(kind, 8_000)).max(0.001);
        let large = best_of(|| measure(kind, 16_000));
        let ratio = large / small;
        if ratio >= 3.0 {
            quadratic.push(format!("{kind} {ratio:.1}x ({small:.3}s -> {large:.3}s)"));
        }
    }

    assert!(
        quadratic.is_empty(),
        "doubling the op count exceeded the 3.0x ceiling for: {}. That means a pass \
         is scanning the whole declaration map once per op again - the quadratic \
         F667 removed. Re-run on an IDLE machine before believing it: these are \
         wall-clock ratios, and heavy background load inflates them",
        quadratic.join("; ")
    );
}

/// F668: the op-kind sweep above still measured only ONE envelope SHAPE.
///
/// Every fixture in this file builds plain tables with no foreign keys, one
/// schema, and no rows. Foreign-key-bearing envelopes took a different path and
/// were still quadratic (4.48x) after F667: `logical_table_is_declared` and
/// `logical_column_matches` scanned the whole declaration map once per foreign
/// key. Measured 0.362s -> 1.623s before, 0.031s -> 0.065s after.
///
/// The op-kind sweep could not have caught this. Both helpers were converted
/// SPECULATIVELY during F666 and the timings did not move by a microsecond,
/// because that envelope had no foreign keys to reach them with -- a change that
/// measured as worthless against the wrong fixture and was correctly reverted.
/// The fixture, not the code, was what changed here.
#[test]
#[ignore = "wall-clock complexity guard: needs an idle machine. Runs in the `scaling` CI job via `cargo test -- --ignored`; see this file's header"]
fn validate_ir_does_not_scale_quadratically_in_any_envelope_shape() {
    fn envelope(shape: &str, n: usize) -> String {
        let half = n / 2;
        let mut ops: Vec<String> = Vec::new();
        match shape {
            // Each child table carries a foreign key into its own parent.
            "foreignKey" => {
                for i in 0..half {
                    ops.push(format!(
                        r#"{{"op":"createTable","name":"p{i}","columns":[{{"name":"c0","type":"bigInt","nullable":false}}],"primaryKey":["c0"]}}"#
                    ));
                }
                for i in 0..half {
                    ops.push(format!(
                        r#"{{"op":"createTable","name":"k{i}","columns":[{{"name":"c0","type":"bigInt","nullable":false}},{{"name":"f0","type":"bigInt","nullable":false}}],"primaryKey":["c0"],"constraints":[{{"kind":{{"kind":"fk","columns":["f0"],"referencesTable":"p{i}","referencesColumns":["c0"]}}}}]}}"#
                    ));
                }
            }
            // Declarations spread over many schemas, so a table's group is not
            // trivially unique and schema matching does real work.
            "multiSchema" => {
                for i in 0..n {
                    ops.push(format!(
                        r#"{{"op":"createTable","schema":"s{}","name":"t{i}","columns":[{{"name":"c0","type":"bigInt","nullable":false}}],"primaryKey":["c0"]}}"#,
                        i % 50
                    ));
                }
            }
            "perRow" => {
                for i in 0..half {
                    ops.push(format!(
                        r#"{{"op":"createTable","name":"t{i}","columns":[{{"name":"c0","type":"bigInt","nullable":false}}],"primaryKey":["c0"]}}"#
                    ));
                }
                for i in 0..half {
                    ops.push(format!(
                        r#"{{"op":"insert","table":"t{i}","columns":["c0"],"rows":[[1]]}}"#
                    ));
                }
            }
            // F669. Tables are created WITHOUT a primary key so the lifecycle op
            // has something to add.
            "alterPrimaryKey" => {
                for i in 0..half {
                    ops.push(format!(
                        r#"{{"op":"createTable","name":"t{i}","columns":[{{"name":"c0","type":"bigInt","nullable":false}}]}}"#
                    ));
                }
                for i in 0..half {
                    ops.push(format!(
                        r#"{{"op":"alterPrimaryKey","table":"t{i}","action":{{"kind":"add","columns":["c0"]}}}}"#
                    ));
                }
            }
            // F669. The set value MUST be `perRow`; a literal never reaches
            // `validate_per_row_destination` and measures nothing about it.
            "perRowGen" => {
                for i in 0..half {
                    ops.push(format!(
                        r#"{{"op":"createTable","name":"t{i}","columns":[{{"name":"c0","type":"bigInt","nullable":false}},{{"name":"u0","type":"uuid","nullable":true}}],"primaryKey":["c0"]}}"#
                    ));
                }
                for i in 0..half {
                    ops.push(format!(
                        r#"{{"op":"backfill","table":"t{i}","name":"bf{i}","cursorColumns":["c0"],"cursorStability":{{"mode":"guardUpdates"}},"batchSize":100,"set":{{"u0":{{"perRow":"uuidV4"}}}}}}"#
                    ));
                }
            }
            // F669. An INLINE column reference, which is a different type from
            // the table-level foreign key above and takes a different path.
            "inlineRef" => {
                for i in 0..half {
                    ops.push(format!(
                        r#"{{"op":"createTable","name":"p{i}","columns":[{{"name":"c0","type":"bigInt","nullable":false}}],"primaryKey":["c0"]}}"#
                    ));
                }
                for i in 0..half {
                    ops.push(format!(
                        r#"{{"op":"createTable","name":"k{i}","columns":[{{"name":"c0","type":"bigInt","nullable":false}},{{"name":"f0","type":"bigInt","nullable":true,"references":{{"table":"p{i}","column":"c0"}}}}],"primaryKey":["c0"]}}"#
                    ));
                }
            }
            other => unreachable!("unhandled shape {other}"),
        }
        format!(r#"{{"ir_version":1,"name":"s","ops":[{}]}}"#, ops.join(","))
    }

    fn measure(shape: &str, n: usize) -> f64 {
        let ir: MigrationIr = serde_json::from_str(&envelope(shape, n)).expect("envelope parses");
        let start = Instant::now();
        validate_ir(&ir, Dialect::Postgres)
            .unwrap_or_else(|e| panic!("{shape} fixture must be valid to measure anything: {e:?}"));
        start.elapsed().as_secs_f64()
    }

    let mut quadratic = Vec::new();
    for shape in [
        "foreignKey",
        "multiSchema",
        "perRow",
        "alterPrimaryKey",
        "perRowGen",
        "inlineRef",
    ] {
        measure(shape, 1_000);
        let small = best_of(|| measure(shape, 4_000)).max(0.001);
        let large = best_of(|| measure(shape, 8_000));
        let ratio = large / small;
        if ratio >= 3.0 {
            quadratic.push(format!("{shape} {ratio:.1}x ({small:.3}s -> {large:.3}s)"));
        }
    }

    assert!(
        quadratic.is_empty(),
        "doubling the op count exceeded the 3.0x ceiling for: {}. That means a pass \
         is scanning the whole declaration map once per op again. F668 covered \
         foreign keys; F669 added alterPrimaryKey, per-row generation and inline \
         column references. Re-run on an IDLE machine before believing it: these \
         are wall-clock ratios, and heavy background load inflates them",
        quadratic.join("; ")
    );
}
