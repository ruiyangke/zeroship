//! PR4 deliverable B2 / test #4 — the CI re-record checksum gate (§8.9.1):
//!
//! For each NOT-YET-APPLIED `.ir.json`, re-record its sibling `.ts` through the
//! canonical recorder and assert the re-recorded TYPED-VALUE checksum equals the
//! committed blob's typed-value checksum. A NEGATIVE test tampers a committed
//! `.ir.json`'s ops so its checksum diverges from what the `.ts` would emit, and
//! asserts the gate FAILS.
//!
//! Faithful: the canonical recorder is the REAL LOCAL sandboxed child.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use zeroship_migrate::frontend::recorder_service::recorder_child_path;
use zeroship_migrate::frontend::{
    build_migrations, recheck_not_yet_applied, RecordVia, ResourceBudget,
};

const OWNER: &str = "app_gate";

const MIG_TS: &str = r#"
import { table, t } from "@zeroship/migrate";
export function up() {
  table("gadgets").create({
    columns: {
      id: t.uuid().notNull().primaryKey().default({ fn: "genRandomUuid" }),
      label: t.text().notNull(),
    },
  });
}
"#;

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

fn write_mig(dir: &Path, stem: &str, ts: &str) {
    fs::write(dir.join(format!("{stem}.ts")), ts.as_bytes()).unwrap();
}

#[test]
fn re_record_gate_passes_on_a_faithfully_committed_artifact() {
    assert_child_built();
    let dir = tempfile::tempdir().unwrap();
    let stem = "20240617130000_gadgets";
    write_mig(dir.path(), stem, MIG_TS);

    // Build (records + commits the `.ir.json`).
    build_migrations(dir.path(), OWNER, &via()).expect("build records the committed artifact");
    assert!(dir.path().join(format!("{stem}.ir.json")).exists());

    // Empty applied set ⇒ all not-yet-applied ⇒ all re-checked. A faithfully
    // recorded artifact re-records to the same typed-value checksum → gate PASSES.
    recheck_not_yet_applied(dir.path(), &BTreeSet::new(), OWNER, &via())
        .expect("the re-record gate must PASS on a faithfully committed artifact");
}

#[test]
fn re_record_gate_fails_on_a_tampered_committed_artifact() {
    assert_child_built();
    let dir = tempfile::tempdir().unwrap();
    let stem = "20240617130000_gadgets";
    write_mig(dir.path(), stem, MIG_TS);
    build_migrations(dir.path(), OWNER, &via()).expect("build");

    // TAMPER the committed `.ir.json` ops so its typed-value checksum diverges from
    // what the `.ts` re-records (rename the created table — a real ops change).
    let ir_path = dir.path().join(format!("{stem}.ir.json"));
    let committed = fs::read_to_string(&ir_path).unwrap();
    let tampered = committed.replace("\"gadgets\"", "\"tampered_gadgets\"");
    assert_ne!(committed, tampered, "the tamper must change the committed ops");
    fs::write(&ir_path, tampered.as_bytes()).unwrap();

    // The gate must FAIL: the committed checksum (over the tampered ops) no longer
    // matches the `.ts`'s re-recorded checksum.
    let err = recheck_not_yet_applied(dir.path(), &BTreeSet::new(), OWNER, &via())
        .expect_err("the re-record gate must FAIL on a tampered committed artifact");
    let msg = format!("{err}");
    assert!(
        msg.contains("checksum") || msg.contains("diverges"),
        "the gate error must name the checksum divergence; got: {msg}"
    );
}

#[test]
fn applied_migrations_are_skipped_by_the_gate() {
    assert_child_built();
    let dir = tempfile::tempdir().unwrap();
    let stem = "20240617130000_gadgets";
    let version = "20240617130000";
    write_mig(dir.path(), stem, MIG_TS);
    build_migrations(dir.path(), OWNER, &via()).expect("build");

    // Tamper the committed artifact, but mark its version as ALREADY APPLIED.
    let ir_path = dir.path().join(format!("{stem}.ir.json"));
    let committed = fs::read_to_string(&ir_path).unwrap();
    fs::write(&ir_path, committed.replace("\"gadgets\"", "\"tampered\"").as_bytes()).unwrap();

    let mut applied = BTreeSet::new();
    applied.insert(version.to_string());
    // An applied migration is frozen — the gate skips it (a tampered-but-applied
    // artifact is the journal's concern, not the re-record gate's).
    recheck_not_yet_applied(dir.path(), &applied, OWNER, &via())
        .expect("an APPLIED migration is skipped by the re-record gate");
}
