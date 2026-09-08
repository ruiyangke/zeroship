//! Query-build microbench — initial `cargo bench` scaffold for plugin-db.
//!
//! ## Why this bench exists
//!
//! A `cargo bench` harness is needed under `crates/zeroship-plugin-db/benches/`
//! before further performance work proceeds. This file is the seed of
//! that harness; future benches can extend it with more paths.
//!
//! ## What this bench targets
//!
//! The highest-leverage candidate is `row_to_json` — it exercises the
//! O(N²) column lookup that is the largest remaining structural cost.
//! However `row_to_json` takes a
//! `&compio_postgres::Row`, and `Row::new` is `pub(crate)` inside
//! `compio-postgres`, so a Row cannot be synthesised from outside the
//! crate without (a) a live Postgres or (b) modifying production code
//! to expose a constructor.
//!
//! Neither option fit this scaffold's scope guards (no production-code
//! edits, no live Postgres). We instead bench `build_find_with_schema` and
//! `build_insert` — both `pub` — which:
//!
//! 1. exercise `validate_collection` (the byte-prefix check) transitively
//!    on the hot path, so a future regression in
//!    `validate_collection` will show up here as a slowdown;
//! 2. cover the realistic query-build cost the SDK pays on every CRUD
//!    call (filter parsing, identifier quoting, parameter binding).
//!
//! That is no longer the state of things: `row_to_json` IS externally
//! benchable now, and `bench_row_to_json.rs` sits next to this file. The
//! route taken was the first of the two this paragraph used to offer -
//! `compio-postgres` exposes `Row` / `Statement` / `Column` constructors
//! from `test_utils`, a doc-hidden module there that is always compiled. It
//! used to sit behind a `test-utils` feature; that flag stopped
//! `serialized_loop.rs` from building under the plain test command, so it was
//! removed.
//!
//! An earlier version of this paragraph named `test-helpers`, which is THIS
//! crate's feature and never existed in `compio-postgres` at all - so a reader
//! following it would look for a feature that is not there and conclude
//! the work was still undone.
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

use zeroship_plugin_db::query::{build_find_with_schema, build_insert};

/// The descriptor entry the benchmarked read is projected through.
///
/// `build_find` took no schema and expanded to `SELECT *`; it is deleted, and
/// the read path now always builds an explicit projection from the descriptor.
/// This map declares the fields the filter fixtures below name, so the builder
/// does the same identifier validation and projection expansion it does at
/// runtime — a benchmark against a `SELECT *` builder would be measuring work
/// production no longer performs.
fn users_schema() -> serde_json::Value {
    json!({
        "status": { "type": "string" },
        "role": { "type": "string" },
        "score": { "type": "int" },
        "email": { "type": "string" },
        "name": { "type": "string" },
        "createdAt": { "type": "date" },
        "updatedAt": { "type": "date" },
    })
}

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
    let app_id = zeroship_schema::SchemaName::new("app_01HJQK2A8R000000000000000")
        .expect("the benchmark fixture names a valid schema");
    let collection = "users";
    let schema = users_schema();

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
                        let built = build_find_with_schema(
                            &app_id,
                            collection,
                            filter,
                            Some(50),
                            Some(0),
                            None,
                            None,
                            &schema,
                        )
                        .expect("build_find_with_schema should succeed on benchmark fixture");
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
    let app_id = zeroship_schema::SchemaName::new("app_01HJQK2A8R000000000000000")
        .expect("the benchmark fixture names a valid schema");
    let collection = "users";
    let doc = small_insert_doc();
    // The write builder now projects its `RETURNING` list from the descriptor,
    // so the benchmark measures the same work production does.
    let schema = users_schema();

    let mut group = c.benchmark_group("build_insert");
    group.measurement_time(Duration::from_secs(3));
    group.warm_up_time(Duration::from_secs(1));

    group.bench_function("small_doc", |b| {
        b.iter_batched_ref(
            || doc.clone(),
            |doc| {
                let built = build_insert(&app_id, collection, &schema, doc)
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
