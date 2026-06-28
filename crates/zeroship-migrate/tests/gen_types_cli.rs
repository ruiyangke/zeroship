//! **Migration-first P2b — the `gen-types` load/emit/check round-trip.**
//!
//! Exercises the CLI-facing seam end-to-end OFF DISK: a migrations dir holding
//! committed `<stem>.ts` + `<stem>.ir.json` pairs is loaded ([`load_dir_ops`]),
//! folded + emitted ([`render_artifacts`] / [`write_artifacts`]), and the committed
//! artifacts are diffed ([`check_artifacts`]) — the CI drift gate. A post-hoc edit to
//! a committed artifact MUST trip `--check`.
//!
//! WHY RED pre-P2b: the whole gen-types module (load_dir_ops / render_artifacts /
//! write_artifacts / check_artifacts) is net-new in P2b.
//!
//! FAITHFUL: the `.ir.json` on disk is the BARE `MigrationIr` document (exactly what
//! `record`/`build` commit), loaded through the REAL discovery + parse path.

use std::path::Path;

use zeroship_migrate::model::ir::{ColType, IrColumn, MigrationIr, Op};
use zeroship_migrate::CURRENT_IR_VERSION;
use zeroship_migrate::frontend::{
    check_artifacts, load_dir_ops, render_artifacts, write_artifacts, ENV_DTS_FILE,
    RUNTIME_DESCRIPTOR_FILE,
};

/// Write a committed `<stem>.ts` + `<stem>.ir.json` pair into `dir` (the `.ts` body
/// is a placeholder mirror; gen-types reads the `.ir.json` source of truth).
fn write_migration(dir: &Path, stem: &str, ir: &MigrationIr) {
    std::fs::write(dir.join(format!("{stem}.ts")), b"// committed migration mirror\n").unwrap();
    let mut bytes = serde_json::to_string_pretty(ir).unwrap();
    bytes.push('\n');
    std::fs::write(dir.join(format!("{stem}.ir.json")), bytes.as_bytes()).unwrap();
}

fn ir_for(name: &str, ops: Vec<Op>) -> MigrationIr {
    MigrationIr {
        ir_version: CURRENT_IR_VERSION,
        name: name.into(),
        owner_app: "app_test".into(),
        ops,
        flags: Default::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    }
}

fn create_users() -> Op {
    Op::CreateTable {
        name: "users".into(),
        columns: vec![
            IrColumn {
                name: "id".into(),
                ty: ColType::Uuid,
                nullable: Some(false),
                default: None,
                unique: None,
                id_prefix: Some("usr2".into()),
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            },
            IrColumn {
                name: "email".into(),
                ty: ColType::Text,
                nullable: Some(false),
                default: None,
                unique: Some(true),
                id_prefix: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            },
        ],
        constraints: Vec::new(),
        indexes: Vec::new(),
        runtime_options: None,
            schema: None,
        existence_guard: None,
    }
}

/// A SECOND migration that adds a column — proves `load_dir_ops` concatenates the
/// version-ordered op lists (the folded "current schema" is the WHOLE set).
fn add_bio() -> Op {
    Op::AddColumn {
        table: "users".into(),
        column: "bio".into(),
        ty: ColType::Text,
        nullable: Some(true),
        default: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
        schema: None,
        existence_guard: None,
    }
}

#[test]
fn gen_types_round_trips_load_emit_check() {
    let dir = tempfile::tempdir().unwrap();
    write_migration(dir.path(), "20240101090000_create_users", &ir_for("create_users", vec![create_users()]));
    write_migration(dir.path(), "20240101100000_add_bio", &ir_for("add_bio", vec![add_bio()]));

    // Load the version-ordered op set + render.
    let ops = load_dir_ops(dir.path()).expect("load committed .ir.json set");
    assert_eq!(ops.len(), 2, "both migrations' ops concatenated in version order");
    let artifacts = render_artifacts(&ops, "public").expect("render");

    // The folded "current schema" carries BOTH the createTable columns AND the
    // later-added `bio` (proving the set is folded, not just the first migration).
    assert!(artifacts.env_dts.contains("id: t.id(\"usr2\")"), "id+prefix");
    assert!(artifacts.env_dts.contains("email: t.string().required().unique()"), "email");
    assert!(artifacts.env_dts.contains("bio: t.string()"), "the added column folds in");

    // Write, then `--check` must PASS against the just-written artifacts.
    let out = tempfile::tempdir().unwrap();
    write_artifacts(out.path(), &artifacts).expect("write artifacts");
    assert!(out.path().join(RUNTIME_DESCRIPTOR_FILE).exists());
    assert!(out.path().join(ENV_DTS_FILE).exists());
    check_artifacts(out.path(), &artifacts).expect("freshly-written artifacts are in sync");
}

#[test]
fn gen_types_check_trips_on_drift() {
    let dir = tempfile::tempdir().unwrap();
    write_migration(dir.path(), "20240101090000_create_users", &ir_for("create_users", vec![create_users()]));
    let ops = load_dir_ops(dir.path()).expect("load");
    let artifacts = render_artifacts(&ops, "public").expect("render");

    let out = tempfile::tempdir().unwrap();
    write_artifacts(out.path(), &artifacts).expect("write");

    // Corrupt the committed env.db.ts (simulate a stale, hand-edited artifact).
    let dts_path = out.path().join(ENV_DTS_FILE);
    let mut stale = std::fs::read_to_string(&dts_path).unwrap();
    stale.push_str("\n// drift\n");
    std::fs::write(&dts_path, stale).unwrap();

    // `--check` must now FAIL (the CI drift gate).
    let err = check_artifacts(out.path(), &artifacts).expect_err("drift must trip --check");
    let msg = err.to_string();
    assert!(msg.contains("STALE") || msg.contains("env.db.ts"), "drift names the file: {msg}");
}

#[test]
fn gen_types_check_missing_committed_artifact_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    write_migration(dir.path(), "20240101090000_create_users", &ir_for("create_users", vec![create_users()]));
    let ops = load_dir_ops(dir.path()).expect("load");
    let artifacts = render_artifacts(&ops, "public").expect("render");

    // Never written — `--check` against an empty out dir is a hard error (the gate
    // must not silently pass when the artifact was never generated).
    let out = tempfile::tempdir().unwrap();
    let err = check_artifacts(out.path(), &artifacts).expect_err("missing committed artifact");
    assert!(err.to_string().contains("missing"), "names the missing file: {err}");
}
