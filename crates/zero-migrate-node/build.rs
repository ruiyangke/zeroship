// napi-rs build shim. `napi_build::setup()` emits the cdylib link arguments that
// let the addon reference the Node/Bun N-API symbols (resolved at `.node` load
// time), so the same object loads under Node and Bun. It emits only linker args —
// it needs no Node headers, so it is a no-op-safe call for a plain `cargo test`
// build of the crate's pure-Rust integration suite as well.
//
// It also folds the workspace source digest that `buildInfo()` reports, so a host
// that loaded a `.node` by path can tell WHICH sources produced it. The digest is
// computed from committed bytes ONLY (crate manifests, `Cargo.lock`, and every
// `crates/*/src` file) - no wall clock, no absolute path, no git state, no
// hostname - so rebuilding an unchanged tree yields the same value and a
// generated-file drift gate stays green.
//
// The digest does NOT cover: this build script, the JS packages under `packages/`,
// the `@napi-rs/cli` version, the rustc version, the cargo profile, or the enabled
// feature set. Two builds of the same sources under a different toolchain or
// profile report the same digest. It also hashes bytes as checked out, so a
// CRLF checkout digests differently from an LF one.
//
// NOTHING ELSE COVERS THAT GAP - it is a hole, not a handoff. `BuildInfo` carries
// only `version` (which moves on a release, not on a rebuild), `ir_version` (a
// format floor), and this digest, so no field of the reported identity separates
// two artifacts that differ only in toolchain, profile, or features. Committed JS
// under `packages/` has its own drift gates in CI (`embedded-recorder.js` and
// `index.d.ts` are each diffed against a fresh build), but those gate the
// REPOSITORY, not the identity a loaded `.node` reports about itself.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

fn main() {
    napi_build::setup();

    // Emitting any `rerun-if-changed` disables cargo's default "rerun when any
    // package file changed", so name this script explicitly alongside the hashed
    // inputs.
    println!("cargo:rerun-if-changed=build.rs");
    println!(
        "cargo:rustc-env=ZERO_MIGRATE_SOURCE_DIGEST={}",
        workspace_source_digest()
    );
}

/// sha256 over every committed Rust source and manifest in the workspace, in
/// sorted workspace-relative path order. Each entry folds `path`, a NUL, the
/// little-endian byte length, then the bytes, so no rename or content shift can
/// collide with a different tree by concatenation.
fn workspace_source_digest() -> String {
    let manifest_dir = PathBuf::from(
        env::var("CARGO_MANIFEST_DIR").expect("cargo always sets CARGO_MANIFEST_DIR"),
    );
    let root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("the addon crate sits at <workspace>/crates/zero-migrate-node")
        .to_path_buf();

    let mut inputs = vec![root.join("Cargo.lock"), root.join("Cargo.toml")];

    let crates_dir = root.join("crates");
    let mut crate_dirs: Vec<PathBuf> = fs::read_dir(&crates_dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", crates_dir.display()))
        .map(|entry| entry.expect("readdir entry").path())
        .filter(|path| path.is_dir())
        .collect();
    crate_dirs.sort();

    for crate_dir in &crate_dirs {
        let manifest = crate_dir.join("Cargo.toml");
        if manifest.is_file() {
            inputs.push(manifest);
        }
        let src = crate_dir.join("src");
        if src.is_dir() {
            // Watching the directory itself catches added and removed files, which a
            // per-file watch alone would miss.
            println!("cargo:rerun-if-changed={}", src.display());
            collect_files(&src, &mut inputs);
        }
    }

    inputs.sort();

    let mut hasher = Sha256::new();
    for path in &inputs {
        println!("cargo:rerun-if-changed={}", path.display());
        let relative = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        hasher.update(relative.as_bytes());
        hasher.update([0u8]);
        hasher.update(
            u64::try_from(bytes.len())
                .expect("a source file fits u64")
                .to_le_bytes(),
        );
        hasher.update(&bytes);
    }
    hex::encode(hasher.finalize())
}

/// Push every file under `dir`, recursively. Order here does not matter: the
/// caller sorts the whole input list before hashing.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("readdir entry").path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}
