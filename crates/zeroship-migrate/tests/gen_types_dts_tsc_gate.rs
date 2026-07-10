#![cfg(feature = "zsv8")]
//! **Migration-first P2b — the REAL `tsc` gate over the emitted `env.db` module
//! (CRITICAL-1).**
//!
//! The prior P2b suite only asserted `dts.contains(substring)` — it never ran `tsc`,
//! so it never noticed the headline artifact DID NOT COMPILE: the emitter wrote the
//! `const schema = { … t.string() … } as const` builder-call body under a `.d.ts`
//! extension, which tsc treats as an AMBIENT declaration context where those runtime
//! value expressions are illegal (`TS1046` / `TS1254`). The fix renames the artifact
//! to a real `.ts` MODULE ([`zeroship_migrate::frontend::ENV_DTS_FILE`] = `env.db.ts`).
//!
//! This test is the regression net: it
//!   1. emits the keystone artifact through the REAL `render_artifacts` path,
//!   2. writes it as `env.db.ts` next to a consumer that exercises the inferred types,
//!   3. runs the repo's `tsc --noEmit` against it — asserting it TYPE-CHECKS CLEAN,
//!   4. proves the rename is LOAD-BEARING: the SAME bytes under a `.d.ts` extension
//!      FAIL with `TS1046` (so a regression back to `.d.ts` re-trips this test).
//!
//! It is a NO-OP (graceful skip) when the JS toolchain isn't present (no `tsc`, or the
//! `@zeroship/db` dist hasn't been built) — but when `tsc` IS available it fails HARD,
//! so `pnpm build && cargo test` is the gate. FAITHFUL: real emitter, real SDK `.d.ts`,
//! real compiler; no `dts.contains` proxy.

use std::path::{Path, PathBuf};
use std::process::Command;

use zeroship_migrate::descriptors_to_create_ops;
use zeroship_migrate::frontend::{eval_schema_to_ir, render_artifacts};

const KEYSTONE: &str = include_str!("fixtures/keystone_schema.js");

/// The repo root — two levels up from this crate's `CARGO_MANIFEST_DIR`
/// (`<root>/crates/zeroship-migrate`).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate is <root>/crates/zeroship-migrate")
        .to_path_buf()
}

/// Locate a usable `tsc` binary + the SDK `.d.ts` paths the emitted module imports.
/// Returns `None` (⇒ skip) if any piece is absent.
fn toolchain(root: &Path) -> Option<(PathBuf, PathBuf, PathBuf)> {
    let tsc = root.join("sdks/db/node_modules/.bin/tsc");
    let db_dts = root.join("sdks/db/dist/index.d.ts");
    let zeroship_dts = root.join("sdks/types/zeroship.d.ts");
    if tsc.exists() && db_dts.exists() && zeroship_dts.exists() {
        Some((tsc, db_dts, zeroship_dts))
    } else {
        None
    }
}

/// Emit the keystone `env.db.ts` via the REAL producer + emitter.
fn emit_keystone_env_db_ts() -> String {
    let descriptors = eval_schema_to_ir(KEYSTONE, "app_keystone").expect("eval keystone");
    let ops = descriptors_to_create_ops(&descriptors).expect("producer");
    let artifacts = render_artifacts(&ops, "public").expect("render");
    artifacts.env_dts
}

/// Write a `tsconfig.json` into `dir` mapping `@zeroship/db` + `zeroship` to the repo
/// SDK `.d.ts` files and compiling exactly `files`.
///
/// `skip_lib_check` MUST be `false` when the file-under-test is itself a `.d.ts` —
/// otherwise tsc skips checking it (it's a "lib") and never reports its ambient-context
/// errors. For a real `.ts` module-under-test, `true` keeps the SDK's own `.d.ts`
/// internals out of scope (we test OUR artifact, not the SDK).
fn write_tsconfig(
    dir: &Path,
    db_dts: &Path,
    zeroship_dts: &Path,
    files: &[&str],
    skip_lib_check: bool,
) {
    let files_json = files
        .iter()
        .map(|f| format!("{:?}", f))
        .collect::<Vec<_>>()
        .join(", ");
    let tsconfig = format!(
        r#"{{
  "compilerOptions": {{
    "strict": true,
    "noEmit": true,
    "skipLibCheck": {skip_lib_check},
    "module": "esnext",
    "moduleResolution": "bundler",
    "target": "es2022",
    "types": [],
    "paths": {{
      "@zeroship/db": [{db:?}],
      "zeroship": [{zs:?}]
    }}
  }},
  "files": [{files_json}]
}}"#,
        db = db_dts.to_string_lossy(),
        zs = zeroship_dts.to_string_lossy(),
    );
    std::fs::write(dir.join("tsconfig.json"), tsconfig).unwrap();
}

fn run_tsc(tsc: &Path, dir: &Path) -> (bool, String) {
    let out = Command::new(tsc)
        .arg("-p")
        .arg(dir.join("tsconfig.json"))
        .current_dir(dir)
        .output()
        .expect("spawn tsc");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), combined)
}

#[test]
fn emitted_env_db_ts_typechecks_against_the_real_sdk() {
    let root = repo_root();
    let Some((tsc, db_dts, zeroship_dts)) = toolchain(&root) else {
        eprintln!("SKIP: tsc / @zeroship/db dist not built — run `pnpm build` to enable the tsc gate");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("env.db.ts"), emit_keystone_env_db_ts()).unwrap();

    // A consumer that PROVES the inferred types are real: optional fields stay
    // optional, required fields are plain strings, and a wrong assignment is rejected
    // (`@ts-expect-error`). If the augmentation didn't resolve, these lines would
    // fail tsc — so a green compile is a real proof.
    let consumer = r#"import { env } from "zeroship";
import "./env.db";
async function probe() {
  const res = await env.db.users.get("acct_x");
  if (res.error === null && res.data !== null) {
    const u = res.data;
    const r: string | undefined = u.role;
    const e: string = u.email; // .required() → plain string
    // @ts-expect-error email is a string, never a number
    const bad: number = u.email;
    return [r, e, bad];
  }
}
probe();
"#;
    std::fs::write(dir.path().join("consumer.ts"), consumer).unwrap();

    // The ambient `zeroship` module decl must be in the program; include it explicitly.
    let zeroship_in_dir = dir.path().join("zeroship.d.ts");
    std::fs::copy(&zeroship_dts, &zeroship_in_dir).unwrap();
    write_tsconfig(
        dir.path(),
        &db_dts,
        &zeroship_dts,
        &["zeroship.d.ts", "env.db.ts", "consumer.ts"],
        true,
    );

    let (ok, output) = run_tsc(&tsc, dir.path());
    assert!(
        ok,
        "the emitted env.db.ts MUST type-check against the real @zeroship/db (CRITICAL-1).\n\
         tsc output:\n{output}"
    );
}

#[test]
fn the_dts_extension_is_load_bearing_same_bytes_as_dts_fail_ts1046() {
    // The companion proof: the rename to `.ts` is not cosmetic. The IDENTICAL bytes
    // under a `.d.ts` extension are an ambient context and FAIL `TS1046`/`TS1254`. If
    // someone regresses ENV_DTS_FILE back to `env.db.d.ts`, the positive test above
    // flips RED; this test documents WHY by reproducing the exact ambient-context error.
    let root = repo_root();
    let Some((tsc, db_dts, zeroship_dts)) = toolchain(&root) else {
        eprintln!("SKIP: tsc / @zeroship/db dist not built");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    // Write the SAME emitted body under a `.d.ts` name.
    std::fs::write(dir.path().join("env.db.d.ts"), emit_keystone_env_db_ts()).unwrap();
    // skipLibCheck:false so tsc actually CHECKS the `.d.ts` (and reports TS1046).
    write_tsconfig(dir.path(), &db_dts, &zeroship_dts, &["env.db.d.ts"], false);

    let (ok, output) = run_tsc(&tsc, dir.path());
    assert!(
        !ok,
        "the .d.ts form MUST fail (it's why we emit `.ts`); tsc unexpectedly passed:\n{output}"
    );
    assert!(
        output.contains("TS1046"),
        "the .d.ts failure must be the ambient-context error TS1046 (proving the \
         `.ts` rename is load-bearing); got:\n{output}"
    );
}
