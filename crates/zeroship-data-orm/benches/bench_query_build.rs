//! Benchmark descriptor-aware SQL compilation and native parameter binding.
//! Run with `cargo bench -p zeroship-data-orm --bench bench_query_build`.

use std::time::Duration;

use criterion::{BatchSize, BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use zeroship_data_orm::value;

use zeroship_data_orm::sql::compile::{build_find_with_schema, build_insert};

/// The descriptor entry the benchmarked read is projected through.
///
/// `build_find` took no schema and expanded to `SELECT *`; it is deleted, and
/// the read path now always builds an explicit projection from the descriptor.
/// This map declares the fields the filter fixtures below name, so the builder
/// does the same identifier validation and projection expansion it does at
/// runtime — a benchmark against a `SELECT *` builder would be measuring work
/// production no longer performs.
fn users_schema() -> zeroship_data_orm::value::Value {
    value!({
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
fn empty_filter() -> zeroship_data_orm::value::Value {
    value!({})
}

/// Median: `find({ status: "active", role: "admin" })`. The most common
/// shape in CRUD-style SDK use (1-3 top-level equalities).
fn small_filter() -> zeroship_data_orm::value::Value {
    value!({
        "status": "active",
        "role": "admin",
    })
}

/// Complex: `$and` + `$or` + `$in` + range. Mirrors the harder query
/// shape an analytics page or admin filter would produce.
fn complex_filter() -> zeroship_data_orm::value::Value {
    value!({
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
fn small_insert_doc() -> zeroship_data_orm::value::Value {
    value!({
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
    let schema_name = zeroship_data_orm::sql::SchemaName::new("app_01HJQK2A8R000000000000000")
        .expect("benchmark schema");
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
        group.bench_with_input(BenchmarkId::from_parameter(name), filter, |b, filter| {
            b.iter_batched_ref(
                || filter.clone(),
                |filter| {
                    let built = build_find_with_schema(
                        &schema_name,
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
        });
    }
    group.finish();
}

fn bench_build_insert(c: &mut Criterion) {
    let schema_name = zeroship_data_orm::sql::SchemaName::new("app_01HJQK2A8R000000000000000")
        .expect("benchmark schema");
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
                let built = build_insert(&schema_name, collection, &schema, doc)
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
