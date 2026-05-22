//! Query-build microbench — initial `cargo bench` scaffold for plugin-db.
//!
//! ## Why this bench exists
//!
//! Performance review r9 (cycle 09:30) recommended r10 should NOT run
//! without a `cargo bench` harness landing under `crates/plugin-db/benches/`.
//! This file is the seed of that harness. Future perf cycles will extend it
//! with more paths as findings warrant.
//!
//! ## What this bench targets
//!
//! Originally the highest-leverage candidate was `row_to_json` — it
//! exercises the N4-I3/N9-I1 O(N²) column lookup that is the largest
//! remaining structural cost. However `row_to_json` takes a
//! `&compio_postgres::Row`, and `Row::new` is `pub(crate)` inside
//! `compio-postgres`, so a Row cannot be synthesised from outside the
//! crate without (a) a live Postgres or (b) modifying production code
//! to expose a constructor.
//!
//! Neither option fit this scaffold's scope guards (no production-code
//! edits, no live Postgres). We instead bench `build_find` and
//! `build_insert` — both `pub` — which:
//!
//! 1. exercise `validate_collection` (the byte-prefix check perf r1
//!    closed) transitively on the hot path, so a future regression in
//!    `validate_collection` will show up here as a slowdown;
//! 2. cover the realistic query-build cost the SDK pays on every CRUD
//!    call (filter parsing, identifier quoting, parameter binding).
//!
//! When `row_to_json` becomes externally benchable (either by exposing
//! a `Row` constructor behind `#[cfg(feature = "test-helpers")]` in
//! `compio-postgres`, or by adding a `#[doc(hidden)] pub` wrapper in
//! `plugin-db` that takes a synthetic input), add it as a new bench
//! file alongside this one.
//!
//! ## Running
//!
//! ```
//! cargo bench -p zeroship-plugin-db --bench bench_query_build
//! ```

use std::time::Duration;

use criterion::{
    black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion,
};
use serde_json::json;

use zeroship_plugin_db::query::{build_find, build_insert};

// ---------------------------------------------------------------------------
// Filter shapes — each represents a realistic SDK call site
// ---------------------------------------------------------------------------

/// Trivial: `find()` with no filter. Smallest query the SDK can produce.
fn empty_filter() -> serde_json::Value {
    json!({})
}

/// Median: `find({ status: "active", role: "admin" })`. The most common
/// shape in CRUD-style SDK use (1-3 top-level equalities).
fn small_filter() -> serde_json::Value {
    json!({
        "status": "active",
        "role": "admin",
    })
}

/// Complex: `$and` + `$or` + `$in` + range. Mirrors the harder query
/// shape an analytics page or admin filter would produce.
fn complex_filter() -> serde_json::Value {
    json!({
        "$and": [
            { "status": { "$in": ["active", "pending", "trial"] } },
            { "$or": [
                { "role": "admin" },
                { "createdAt": { "$gte": "2026-01-01" } },
            ]},
            { "score": { "$gte": 50, "$lte": 100 } },
        ]
    })
}

/// Insert doc — typical user record shape.
fn small_insert_doc() -> serde_json::Value {
    json!({
        "id": "usr_01HJQK2A8R000000000000000",
        "email": "alice@example.com",
        "name": "Alice Example",
        "role": "admin",
        "createdAt": "2026-05-22T00:00:00Z",
        "updatedAt": "2026-05-22T00:00:00Z",
    })
}

// ---------------------------------------------------------------------------
// Bench groups
// ---------------------------------------------------------------------------

fn bench_build_find(c: &mut Criterion) {
    let app_id = "app_01HJQK2A8R000000000000000";
    let collection = "users";

    let workloads = vec![
        ("empty", empty_filter()),
        ("small", small_filter()),
        ("complex", complex_filter()),
    ];

    let mut group = c.benchmark_group("build_find");
    group.measurement_time(Duration::from_secs(3));
    group.warm_up_time(Duration::from_secs(1));

    for (name, filter) in &workloads {
        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            filter,
            |b, filter| {
                b.iter_batched_ref(
                    || filter.clone(),
                    |filter| {
                        let built = build_find(
                            app_id,
                            collection,
                            filter,
                            Some(50),
                            Some(0),
                            None,
                            None,
                        )
                        .expect("build_find should succeed on benchmark fixture");
                        black_box(built);
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

fn bench_build_insert(c: &mut Criterion) {
    let app_id = "app_01HJQK2A8R000000000000000";
    let collection = "users";
    let doc = small_insert_doc();

    let mut group = c.benchmark_group("build_insert");
    group.measurement_time(Duration::from_secs(3));
    group.warm_up_time(Duration::from_secs(1));

    group.bench_function("small_doc", |b| {
        b.iter_batched_ref(
            || doc.clone(),
            |doc| {
                let built = build_insert(app_id, collection, doc)
                    .expect("build_insert should succeed on benchmark fixture");
                black_box(built);
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(benches, bench_build_find, bench_build_insert);
criterion_main!(benches);
