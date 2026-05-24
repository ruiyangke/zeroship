//! `find().first()` lowering microbench — measures the full `&[Row] →
//! JSON-string` path the SDK sees on a single-row read (`find` with
//! `LIMIT 1`, the implementation of `Query.first()`).
//!
//! ## Why this bench exists
//!
//! Performance r12 identified the JSON-string + V8 `JSON.parse` tail in
//! `crud::first_row_or_null` (`crates/plugin-db/src/crud.rs:106-109`) as
//! the next bottleneck after the [I35] index-lookup fix (commit
//! `251d53b4`). With `bench_row_to_json` covering the row-decode half,
//! this harness covers the composed path so deferred [C3] can graduate
//! from "blocked on cross-crate redesign" to "actionable" if the wide-row
//! number crosses the threshold.
//!
//! The measured path (via `first_row_or_null_for_bench`):
//!
//! 1. `v8_bridge::rows_to_json_value` — `Row → serde_json::Value` for
//!    every row (the work `bench_row_to_json` already covers for one row).
//! 2. `crud::first_row_or_null` — pluck the first element (or `Null`)
//!    and `.to_string()` it for `ResolveValue::Json`.
//!
//! The downstream V8 `JSON.parse` cost lives in `zeroship-runtime` and
//! is NOT part of this microbench — it is the structural piece [C3]
//! would have to redesign away.
//!
//! ## Workloads
//!
//! Same three column-count shapes as `bench_row_to_json` (narrow / medium
//! / wide), each wrapped in a single-row `Vec<Row>` — `Query.first()`
//! is by definition a `LIMIT 1` query, so the bench mirrors that
//! workload exactly. The OID mix (INT4 / INT8 / BOOL / TEXT / JSONB /
//! TIMESTAMPTZ)
//! is identical too, for direct comparability.
//!
//! ## Running
//!
//! ```text
//! cargo bench -p zeroship-plugin-db --bench bench_first_row_or_null
//! ```

use std::time::Duration;

use compio_postgres::test_utils::{column_for_test, row_for_test};
use compio_postgres::types::Type;
use compio_postgres::Row;
use criterion::{black_box, criterion_group, criterion_main, Criterion};

use zeroship_plugin_db::first_row_or_null_for_bench;

// ---------------------------------------------------------------------------
// Wire-format encoders for the OID branches we exercise
// ---------------------------------------------------------------------------
//
// Identical to `bench_row_to_json.rs`. Kept duplicated rather than
// factored into a shared module because Criterion bench targets are
// individual `cargo test`-style binaries — sharing a helper would need
// a `mod common;` dance that obscures the OID-coverage intent. The
// duplication is ~30 LOC and the wire formats are immutable Postgres
// contracts, not project code that drifts.

fn enc_int4(v: i32) -> Vec<u8> {
    v.to_be_bytes().to_vec()
}

fn enc_int8(v: i64) -> Vec<u8> {
    v.to_be_bytes().to_vec()
}

fn enc_bool(v: bool) -> Vec<u8> {
    vec![u8::from(v)]
}

fn enc_text(v: &str) -> Vec<u8> {
    v.as_bytes().to_vec()
}

/// JSONB binary format: 1-byte version (0x01) + raw JSON text.
fn enc_jsonb(v: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + v.len());
    out.push(0x01);
    out.extend_from_slice(v.as_bytes());
    out
}

/// TIMESTAMPTZ binary format: 8-byte BE i64 of microseconds since
/// 2000-01-01 00:00:00 UTC.
fn enc_timestamptz_us(pg_usec: i64) -> Vec<u8> {
    pg_usec.to_be_bytes().to_vec()
}

// ---------------------------------------------------------------------------
// Row builders
// ---------------------------------------------------------------------------

/// A single (column, encoded-bytes) tuple for a row fixture.
struct ColFixture {
    name: String,
    ty: Type,
    bytes: Vec<u8>,
}

fn fixture(name: impl Into<String>, ty: Type, bytes: Vec<u8>) -> ColFixture {
    ColFixture {
        name: name.into(),
        ty,
        bytes,
    }
}

/// Build a Row from fixtures. Panics on any mismatch — bench scaffolding,
/// not production code.
fn build_row(fixtures: Vec<ColFixture>) -> Row {
    let columns = fixtures
        .iter()
        .map(|f| column_for_test(&f.name, f.ty.clone()))
        .collect::<Vec<_>>();
    let values = fixtures
        .into_iter()
        .map(|f| Some(f.bytes))
        .collect::<Vec<_>>();
    row_for_test(columns, values).expect("row_for_test synthesises a valid Row")
}

/// One "unit" of the realistic column mix — 6 OIDs covering every
/// branch in `column_to_json` we exercise here.
fn unit_fixtures(prefix: &str) -> Vec<ColFixture> {
    vec![
        fixture(format!("{prefix}_id"), Type::INT8, enc_int8(42)),
        fixture(format!("{prefix}_count"), Type::INT4, enc_int4(7)),
        fixture(format!("{prefix}_active"), Type::BOOL, enc_bool(true)),
        fixture(
            format!("{prefix}_name"),
            Type::TEXT,
            enc_text("alice example"),
        ),
        fixture(
            format!("{prefix}_payload"),
            Type::JSONB,
            enc_jsonb(r#"{"tier":"gold","seats":5}"#),
        ),
        fixture(
            format!("{prefix}_created_at"),
            Type::TIMESTAMPTZ,
            // 2026-05-22 00:00:00 UTC ≈ pg-µs from 2000-01-01
            enc_timestamptz_us(806_198_400_000_000),
        ),
    ]
}

/// Narrow row: 3 columns (INT4 + TEXT + INT8). Mirrors a primary-key
/// + name lookup.
fn narrow_row() -> Row {
    build_row(vec![
        fixture("id", Type::INT8, enc_int8(1)),
        fixture("name", Type::TEXT, enc_text("alice")),
        fixture("count", Type::INT4, enc_int4(42)),
    ])
}

/// Medium row: 10 columns — one full unit (6) + 4 extra mixed.
fn medium_row() -> Row {
    let mut f = unit_fixtures("a");
    f.push(fixture("extra_count", Type::INT4, enc_int4(13)));
    f.push(fixture("extra_flag", Type::BOOL, enc_bool(false)));
    f.push(fixture(
        "extra_text",
        Type::TEXT,
        enc_text("medium-row-extra"),
    ));
    f.push(fixture("extra_id", Type::INT8, enc_int8(9_999)));
    assert_eq!(f.len(), 10, "medium_row should have 10 columns");
    build_row(f)
}

/// Wide row: 50 columns — repeats the 6-column unit 8 times (48) + 2
/// extras. The shape an analytics page or a row with many JSON
/// expansions would produce.
fn wide_row() -> Row {
    let mut f: Vec<ColFixture> = (0..8).flat_map(|i| unit_fixtures(&format!("u{i}"))).collect();
    f.push(fixture("tail_id", Type::INT8, enc_int8(7)));
    f.push(fixture("tail_flag", Type::BOOL, enc_bool(true)));
    assert_eq!(f.len(), 50, "wide_row should have 50 columns");
    build_row(f)
}

// ---------------------------------------------------------------------------
// Bench groups
// ---------------------------------------------------------------------------

fn bench_first_row_or_null(c: &mut Criterion) {
    let mut group = c.benchmark_group("first_row_or_null");
    group.measurement_time(Duration::from_secs(3));
    group.warm_up_time(Duration::from_secs(1));

    // `Query.first()` always materialises exactly one row at the SDK
    // boundary (the SQL builder appends `LIMIT 1`), so each input is a
    // 1-element slice. Keeping the slice constructed once outside the
    // loop holds
    // allocation out of the timed work — only the decode + serialise
    // path is measured.
    let narrow: [Row; 1] = [narrow_row()];
    let medium: [Row; 1] = [medium_row()];
    let wide: [Row; 1] = [wide_row()];

    // Sanity: every fixture lowers to a JSON string whose top-level
    // object has the expected column count. Catches wire-format
    // breakage in `enc_*` before the bench produces nonsense numbers.
    {
        let s = first_row_or_null_for_bench(&narrow);
        let v: serde_json::Value =
            serde_json::from_str(&s).expect("narrow output should parse as JSON");
        assert_eq!(
            v.as_object().expect("narrow → object").len(),
            3,
            "narrow row should serialise 3 columns",
        );
        let s = first_row_or_null_for_bench(&medium);
        let v: serde_json::Value =
            serde_json::from_str(&s).expect("medium output should parse as JSON");
        assert_eq!(
            v.as_object().expect("medium → object").len(),
            10,
            "medium row should serialise 10 columns",
        );
        let s = first_row_or_null_for_bench(&wide);
        let v: serde_json::Value =
            serde_json::from_str(&s).expect("wide output should parse as JSON");
        assert_eq!(
            v.as_object().expect("wide → object").len(),
            50,
            "wide row should serialise 50 columns",
        );
    }

    group.bench_function("narrow_3cols", |b| {
        b.iter(|| {
            black_box(first_row_or_null_for_bench(black_box(&narrow)));
        });
    });
    group.bench_function("medium_10cols", |b| {
        b.iter(|| {
            black_box(first_row_or_null_for_bench(black_box(&medium)));
        });
    });
    group.bench_function("wide_50cols", |b| {
        b.iter(|| {
            black_box(first_row_or_null_for_bench(black_box(&wide)));
        });
    });

    group.finish();
}

criterion_group!(benches, bench_first_row_or_null);
criterion_main!(benches);
