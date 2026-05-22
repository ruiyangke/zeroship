//! Row-decoding microbench — measures `row_to_json` against synthesised
//! `compio_postgres::Row` values of varying width.
//!
//! ## Why this bench exists
//!
//! Performance review r12 recommended this harness as the next-cycle
//! forcing function for the [I35] index-lookup fix (commit `251d53b4`),
//! which dropped `row_to_json`'s per-column resolution from O(N²)
//! (linear `position` over `row.columns()` keyed on column NAME) to
//! O(N) (direct `usize` index). The pre-existing `bench_query_build`
//! harness covers SQL construction only — it never enters
//! `v8_bridge::row_to_json` at all, so the I35 win was invisible.
//!
//! This file unblocks measurement by synthesising rows from outside the
//! crate. The blocker was that `compio_postgres::Row::new` is
//! `pub(crate)`; r12 Option A landed alongside this commit as
//! `compio_postgres::test_utils::row_for_test` (gated behind the
//! `test-utils` Cargo feature, which `plugin-db` enables in
//! `[dev-dependencies]`).
//!
//! ## Workloads
//!
//! Three column-count points (narrow / medium / wide) mixing the OID
//! branches `column_to_json` actually decodes in production:
//! `INT4` (23), `INT8` (20), `BOOL` (16), `TEXT` (25), `JSONB` (3802),
//! `TIMESTAMPTZ` (1184). The exact wire-format encoding for each is
//! built once outside the bench loop; the timed work is purely the
//! `Row::columns() → column_to_json` traversal.
//!
//! ## Running
//!
//! ```text
//! cargo bench -p zeroship-plugin-db --bench bench_row_to_json
//! ```

use std::time::Duration;

use compio_postgres::test_utils::{column_for_test, row_for_test};
use compio_postgres::types::Type;
use compio_postgres::Row;
use criterion::{black_box, criterion_group, criterion_main, Criterion};

use zeroship_plugin_db::row_to_json_for_bench;

// ---------------------------------------------------------------------------
// Wire-format encoders for the OID branches we exercise
// ---------------------------------------------------------------------------
//
// All Postgres binary wire encodings; column_to_json's branches consume
// them as-is. Sourced from postgres-types' ToSql impls (verified against
// crates/plugin-db/src/v8_bridge.rs::column_to_json:372-505).

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
/// + name lookup. Sized so the linear-scan / index-scan cost is roughly
/// equal — useful as a baseline.
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

fn bench_row_to_json(c: &mut Criterion) {
    let mut group = c.benchmark_group("row_to_json");
    group.measurement_time(Duration::from_secs(3));
    group.warm_up_time(Duration::from_secs(1));

    let narrow = narrow_row();
    let medium = medium_row();
    let wide = wide_row();

    // Sanity: every fixture decodes to a JSON object with the expected
    // column count. Catches wire-format breakage in `enc_*` before the
    // bench produces nonsense numbers.
    {
        let v = row_to_json_for_bench(&narrow);
        assert_eq!(
            v.as_object().expect("narrow → object").len(),
            3,
            "narrow row should decode 3 columns",
        );
        let v = row_to_json_for_bench(&medium);
        assert_eq!(
            v.as_object().expect("medium → object").len(),
            10,
            "medium row should decode 10 columns",
        );
        let v = row_to_json_for_bench(&wide);
        assert_eq!(
            v.as_object().expect("wide → object").len(),
            50,
            "wide row should decode 50 columns",
        );
    }

    group.bench_function("narrow_3cols", |b| {
        b.iter(|| {
            black_box(row_to_json_for_bench(black_box(&narrow)));
        });
    });
    group.bench_function("medium_10cols", |b| {
        b.iter(|| {
            black_box(row_to_json_for_bench(black_box(&medium)));
        });
    });
    group.bench_function("wide_50cols", |b| {
        b.iter(|| {
            black_box(row_to_json_for_bench(black_box(&wide)));
        });
    });

    group.finish();
}

criterion_group!(benches, bench_row_to_json);
criterion_main!(benches);
