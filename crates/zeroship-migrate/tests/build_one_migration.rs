#![cfg(feature = "zsv8")]
//! PR4 code-critic LOW regression — `record <file.ts>` (build_one_migration) must
//! record ONLY the requested file, never an unrelated in-progress sibling.
//!
//! Pre-fix the CLI `record` shelled `build_migrations(&dir, …)` over the WHOLE dir
//! and merely filtered the report to the requested file — so a sibling `.ts`
//! was inadvertently recorded. build_one_migration scopes the build to the single
//! discovered file and keeps IR in memory.

use std::fs;

use zeroship_migrate::frontend::recorder_service::recorder_child_path;
use zeroship_migrate::frontend::{build_one_migration, BuildError, RecordVia, ResourceBudget};

const APP: &str = "app_one";

fn assert_child_built() {
    let bin = recorder_child_path();
    assert!(
        bin.exists(),
        "recorder child binary missing at {} — run `cargo build -p zeroship-migrate --bins` first",
        bin.display()
    );
}

const TS: &str = r#"
import { table, t } from "@zeroship/migrate";
export function up() {
  table("t1").create({ columns: { label: t.text().notNull() } });
}
"#;

const SIBLING_TS: &str = r#"
import { table, t } from "@zeroship/migrate";
export function up() {
  table("t2").create({ columns: { label: t.text().notNull() } });
}
"#;

fn via() -> RecordVia<'static> {
    RecordVia::Local {
        budget: ResourceBudget::default(),
    }
}

#[test]
fn record_one_file_does_not_record_siblings() {
    assert_child_built();
    let dir = tempfile::tempdir().unwrap();
    let target = "20240617150000_wanted";
    let sibling = "20240617160000_half_finished_other";
    fs::write(dir.path().join(format!("{target}.ts")), TS.as_bytes()).unwrap();
    fs::write(dir.path().join(format!("{sibling}.ts")), SIBLING_TS.as_bytes()).unwrap();

    let target_path = dir.path().join(format!("{target}.ts"));
    let outcome = build_one_migration(&zeroship_migrate_runtime::ZeroshipRuntimeHost, &target_path, APP, &via()).expect("build one");
    assert_eq!(outcome.migrations.len(), 1, "exactly one migration built");
    assert_eq!(outcome.migrations[0].stem, target);

    // No committed artifact is written for either file.
    assert!(
        !dir.path().join(format!("{target}.ir.json")).exists(),
        "the requested migration's .ir.json must not be written"
    );
    assert!(
        !dir.path().join(format!("{sibling}.ir.json")).exists(),
        "recording one file must NOT record the unrelated sibling"
    );
}

#[test]
fn record_one_file_is_deterministic_across_transient_runs() {
    assert_child_built();
    let dir = tempfile::tempdir().unwrap();
    let stem = "20240617150000_idem";
    let path = dir.path().join(format!("{stem}.ts"));
    fs::write(&path, TS.as_bytes()).unwrap();

    let first = build_one_migration(&zeroship_migrate_runtime::ZeroshipRuntimeHost, &path, APP, &via()).expect("first");
    assert_eq!(first.migrations[0].record_path, zeroship_migrate::frontend::RecordPath::Local);
    let bytes = first.migrations[0].committed_bytes.clone();

    let second = build_one_migration(&zeroship_migrate_runtime::ZeroshipRuntimeHost, &path, APP, &via()).expect("second");
    assert_eq!(
        second.migrations[0].record_path,
        zeroship_migrate::frontend::RecordPath::Local
    );
    assert_eq!(second.migrations[0].committed_bytes, bytes);
    assert!(
        !dir.path().join(format!("{stem}.ir.json")).exists(),
        "transient recording must not write a committed .ir.json"
    );
}

#[test]
fn record_non_ts_path_is_invalid_name() {
    let err = build_one_migration(&zeroship_migrate_runtime::ZeroshipRuntimeHost, std::path::Path::new("/tmp/nota.json"), APP, &via())
        .expect_err("a non-.ts path is rejected");
    assert!(matches!(err, BuildError::InvalidName { .. }), "got {err:?}");
}
