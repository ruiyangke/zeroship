//! PR4 code-critic LOW regression — `record <file.ts>` (build_one_migration) must
//! record ONLY the requested file, never an unrelated in-progress sibling.
//!
//! Pre-fix the CLI `record` shelled `build_migrations(&dir, …)` over the WHOLE dir
//! and merely filtered the report to the requested file — so a sibling `.ts`
//! lacking a committed `.ir.json` was inadvertently recorded (its `.ir.json` written
//! to disk). build_one_migration scopes the build to the single discovered file.

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
    let outcome = build_one_migration(&target_path, APP, &via()).expect("build one");
    assert_eq!(outcome.migrations.len(), 1, "exactly one migration built");
    assert_eq!(outcome.migrations[0].stem, target);

    // The requested file's committed artifact exists.
    assert!(
        dir.path().join(format!("{target}.ir.json")).exists(),
        "the requested migration's .ir.json must be written"
    );
    // The SIBLING's committed artifact must NOT exist — it was never recorded.
    assert!(
        !dir.path().join(format!("{sibling}.ir.json")).exists(),
        "recording one file must NOT record the unrelated sibling"
    );
}

#[test]
fn record_one_file_is_idempotent_verbatim() {
    assert_child_built();
    let dir = tempfile::tempdir().unwrap();
    let stem = "20240617150000_idem";
    let path = dir.path().join(format!("{stem}.ts"));
    fs::write(&path, TS.as_bytes()).unwrap();

    let first = build_one_migration(&path, APP, &via()).expect("first");
    assert_eq!(first.migrations[0].record_path, zeroship_migrate::frontend::RecordPath::Local);
    let bytes = first.migrations[0].committed_bytes.clone();

    let second = build_one_migration(&path, APP, &via()).expect("second");
    assert_eq!(
        second.migrations[0].record_path,
        zeroship_migrate::frontend::RecordPath::CommittedVerbatim
    );
    assert_eq!(second.migrations[0].committed_bytes, bytes);
}

#[test]
fn record_non_ts_path_is_invalid_name() {
    let err = build_one_migration(std::path::Path::new("/tmp/nota.json"), APP, &via())
        .expect_err("a non-.ts path is rejected");
    assert!(matches!(err, BuildError::InvalidName { .. }), "got {err:?}");
}
