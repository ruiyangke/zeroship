#![cfg(feature = "zsv8")]
//! PR4 code-critic LOW — extend the `generate` autogenerate-parity coverage to ALL
//! portable column types. The existing parity e2e proves re-diff-to-zero + recorded-
//! `.ts` checksum == direct IR checksum only for text/int/unique; this extends the
//! checksum-parity leg to a multi-type schema covering
//! smallInt/bigInt/float/real/boolean/json/timestamp/uuid/inet/textArray/bytes/numeric — so
//! a `render_t_for` name regression on ANY of those types trips the parity
//! assertion.
//!
//! WHY checksum-parity (not re-diff) is the right gate for the wide-type matrix:
//! `render_t_for` (the emitted `.ts` type chain) is the SAME render whether the
//! column round-trips through PG byte-for-byte or widens (e.g. `t.bigInt()` lowers to
//! a PG `integer`). The parity anchor catches a render-NAME regression for EVERY
//! type without depending on PG type-widening fidelity. The PG re-diff-to-zero leg is
//! exercised by `build_new_generate_pg.rs` for the round-trip-safe subset.
//!
//! FAITHFUL: the emitted `.ts` is recorded through the REAL kernel-sandboxed recorder
//! child; the direct checksum is the engine's `Checksum::of_ir` anchor.

#![cfg(target_os = "linux")]

use std::path::Path;

use zeroship_migrate::model::ir::{CanonicalOpList, ColType, Op};
use zeroship_migrate::render::declarative::DesiredSchema;
use zeroship_migrate::{
    Checksum, ColumnSnapshot, MigrationFlags, MigrationIr, SchemaSnapshot, TableSnapshot,
};
use zeroship_migrate::frontend::recorder_service::recorder_child_path;
use zeroship_migrate::frontend::{build_migrations, generate_ops, RecordVia, ResourceBudget};

const APP: &str = "app_alltypes";

fn assert_child_built() {
    let bin = recorder_child_path();
    assert!(
        bin.exists(),
        "recorder child binary missing at {} — run `cargo build -p zeroship-migrate --bins` first",
        bin.display()
    );
}

fn via() -> RecordVia<'static> {
    RecordVia::Local {
        budget: ResourceBudget::default(),
    }
}

/// The directly-derived IR's typed-value checksum (the `Checksum::of_ir` anchor).
fn direct_checksum(ir: &MigrationIr) -> String {
    Checksum::of_ir(
        &CanonicalOpList(&ir.ops),
        &MigrationFlags::default(),
        &ir.owner_app,
        &[],
        &[],
        &ir.preconditions,
    )
    .as_str()
    .to_string()
}

fn col(name: &str, data_type: &str) -> ColumnSnapshot {
    ColumnSnapshot {
        name: name.into(),
        data_type: data_type.into(),
        nullable: false,
        ..Default::default()
    }
}

/// A desired snapshot whose column `data_type`s hit EVERY portable `ColType` branch
/// of `col_type_for_data_type` → `render_t_for`:
/// text/int/smallInt/bigInt/float/real/boolean/json/timestamp/uuid/inet/textArray/bytes/numeric.
fn all_types_desired() -> DesiredSchema {
    let mut cols = vec![
        col("c_text", "text"),
        col("c_int", "integer"),
        col("c_smallint", "smallint"),
        col("c_bigint", "bigint"),
        col("c_float", "double precision"),
        col("c_real", "real"),
        col("c_bool", "boolean"),
        col("c_json", "jsonb"),
        col("c_ts", "timestamp with time zone"),
        col("c_uuid", "uuid"),
        col("c_inet", "inet"),
        col("c_text_array", "text[]"),
        col("c_bytes", "bytea"),
        col("c_num", "numeric"),
    ];
    cols.sort_by(|a, b| a.name.cmp(&b.name));
    let tbl = TableSnapshot {
        columns: cols,
        indexes: vec![],
        constraints: vec![],
        runtime_options: Default::default(),

    partition_by: None,

    comment: None,
        stored_create_sql: None,
    };
    let mut snap = SchemaSnapshot::default();
    snap.tables.insert("gadgets".into(), tbl);
    DesiredSchema {
        snapshot: snap,
        ownership: [("gadgets".to_string(), APP.to_string())].into_iter().collect(),
        sqlite_schemas: Default::default(),
    }
}

#[test]
fn generate_all_column_types_records_to_direct_ir_checksum() {
    assert_child_built();

    let desired = all_types_desired();
    let live = SchemaSnapshot::default(); // empty live → full createTable.
    let gen = generate_ops("create_gadgets", APP, &desired, &live).expect("generate ops");
    assert!(!gen.is_empty, "a fresh multi-type schema must generate ops");

    // The createTable op must carry EVERY one of the ten ColTypes — proving the wide
    // matrix is actually exercised (not silently collapsed).
    let created_types: Vec<&ColType> = gen
        .ir
        .ops
        .iter()
        .filter_map(|o| match o {
            Op::CreateTable { columns, .. } => Some(columns),
            _ => None,
        })
        .flatten()
        .map(|c| &c.ty)
        .collect();
    for expect in [
        ColType::Text,
        ColType::Int,
        ColType::SmallInt,
        ColType::BigInt,
        ColType::Double,
        ColType::Real,
        ColType::Boolean,
        ColType::Json,
        ColType::Timestamp,
        ColType::Uuid,
        ColType::Inet,
        ColType::TextArray,
        ColType::Bytes,
        ColType::Decimal { precision: 38, scale: 9 },
    ] {
        assert!(
            created_types.iter().any(|t| **t == expect),
            "the generated createTable must carry a {expect:?} column; got {created_types:?}"
        );
    }

    // Each `render_t_for` chain must appear in the emitted `.ts` (a name regression
    // on any one would drop / mis-spell it).
    for chain in [
        "t.text()",
        "t.int()",
        "t.smallInt()",
        "t.bigInt()",
        "t.double()",
        "t.real()",
        "t.boolean()",
        "t.json()",
        "t.timestamp()",
        "t.uuid()",
        "t.inet()",
        "t.textArray()",
        "t.bytes()",
        "t.numeric({ precision: 38, scale: 9 })",
    ] {
        assert!(
            gen.ts_body.contains(chain),
            "the emitted .ts must render the {chain} chain; ts:\n{}",
            gen.ts_body
        );
    }

    // RECORD the emitted `.ts` through the REAL recorder and assert its typed-value
    // checksum equals the directly-derived IR's — autogenerate parity across ALL
    // types. A `render_t_for` name regression would make the recorded `.ts` lower to
    // a different ColType → a different checksum → this trips.
    let dir = tempfile::tempdir().unwrap();
    let stem = "20240617160000_create_gadgets";
    std::fs::write(dir.path().join(format!("{stem}.ts")), gen.ts_body.as_bytes()).unwrap();
    let built = build_migrations(&zeroship_migrate_runtime::ZeroshipRuntimeHost, dir.path(), APP, &via()).expect("record the emitted .ts");
    let derived = direct_checksum(&gen.ir);
    assert_eq!(
        built.migrations[0].checksum, derived,
        "the emitted multi-type .ts must record to the SAME checksum as the directly-derived IR \
         (autogenerate parity across all column types — catches a render_t_for name regression)"
    );
}

#[allow(dead_code)]
fn _unused(_: &Path) {}
