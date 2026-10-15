//! Build the workflow-process executables, record that cargo built them, and
//! warm the ORM compile-fail target.
//!
//! `cargo xtask build-test-artifacts` is what `pnpm build:test-artifacts` runs,
//! and what the build-input freshness gate names. It runs
//! `prebuilt::BUILD_ARGS`, stamps every `prebuilt::EXECUTABLES` artifact cargo
//! wrote, and then runs `prebuilt::WARM_ARGS`.
//!
//! The stamp is the gate's remedy. Cargo relinks a binary only when one of its
//! own inputs moved, so a change to the root manifest or the lockfile that
//! touches no source a given executable compiles leaves the executable older
//! than `Cargo.toml` or `Cargo.lock`, and `cargo build` correctly rebuilds
//! nothing. Setting each artifact's modification time after cargo has built
//! exactly the target that produces it records cargo's own verdict: the
//! artifact is current with respect to every input, the manifest and the
//! lockfile included.
//!
//! A failed build stamps nothing. The stamp runs only after the service build
//! exits successfully, so a build that produced no artifact cannot make one
//! look fresh.

use crate::{cargo, checked, prepare, shard, Result};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use zeroship_testkit::prebuilt;

/// Rebuild the workflow-process executables, stamp them, and warm the ORM
/// compile-fail target.
pub fn build_test_artifacts() -> Result<()> {
    let target = target_directory(&shard::workspace_metadata()?)?;
    build_then_stamp(prepare::build_services, || stamp_built(&target))?;
    checked(
        cargo().args(prebuilt::WARM_ARGS),
        "warm the ORM compile-fail target",
    )
}

/// The directory cargo writes build artifacts into, as cargo reports it.
///
/// Reading cargo's own answer honours `CARGO_TARGET_DIR` the way the tests
/// resolve artifacts, so the stamp lands on the executables the same cargo run
/// wrote rather than on a path assumed under the repository root.
fn target_directory(metadata: &serde_json::Value) -> Result<PathBuf> {
    metadata["target_directory"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| "cargo metadata names no target directory".into())
}

/// Run `build`, then run `stamp` only when `build` succeeded.
///
/// The ordering is the contract: a build that failed produced no artifact to
/// record, so nothing is stamped and the gate keeps naming the rebuild.
fn build_then_stamp(
    build: impl FnOnce() -> Result<()>,
    stamp: impl FnOnce() -> Result<()>,
) -> Result<()> {
    build()?;
    stamp()
}

/// Stamp every artifact under `directory` with the time the build finished.
///
/// This is the production time source, shared with the test that drives the
/// stamping path, so an artifact ends up newer than any source the build read.
fn stamp_built(directory: &Path) -> Result<()> {
    stamp(directory, SystemTime::now())
}

/// Set every `prebuilt::EXECUTABLES` artifact under `directory` to `at`.
///
/// Each artifact is opened read-only. Setting a file's times through a handle
/// needs ownership rather than write access, and opening an executable for
/// write fails with `ETXTBSY` while a test fleet is running it, or when the
/// artifact's write permission bit is clear.
///
/// The artifact list is derived from the executable table, so the stamp and the
/// build select the same files.
fn stamp(directory: &Path, at: SystemTime) -> Result<()> {
    for executable in prebuilt::EXECUTABLES {
        let artifact = directory.join(executable.artifact_path());
        std::fs::File::open(&artifact)
            .and_then(|file| file.set_modified(at))
            .map_err(|error| format!("stamp {}: {error}", artifact.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{build_then_stamp, stamp, stamp_built};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};
    use zeroship_testkit::prebuilt::EXECUTABLES;

    /// The time `seconds` after the Unix epoch, so the test sets every
    /// modification time explicitly and never sleeps.
    fn at(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn write(path: &Path, at: SystemTime) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create artifact directory");
        }
        fs::File::options()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .expect("create file")
            .set_modified(at)
            .expect("set modification time");
    }

    fn modified(path: &Path) -> SystemTime {
        fs::metadata(path)
            .expect("metadata")
            .modified()
            .expect("modification time")
    }

    /// Every artifact `EXECUTABLES` names, written at `artifact`, beside a
    /// source they are compiled from written at `source`.
    fn populate(directory: &Path, source: SystemTime, artifact: SystemTime) -> Vec<PathBuf> {
        let paths: Vec<PathBuf> = EXECUTABLES
            .iter()
            .map(|executable| directory.join(executable.artifact_path()))
            .collect();
        for path in &paths {
            write(path, artifact);
        }
        write(&directory.join("a-source.rs"), source);
        paths
    }

    #[test]
    fn a_successful_build_stamps_every_artifact_newer_than_its_sources() {
        let directory = tempfile::tempdir().expect("temp directory");
        let built = at(10);
        let touched = at(20);
        let artifacts = populate(directory.path(), touched, built);
        let source = directory.path().join("a-source.rs");
        assert!(
            modified(&artifacts[0]) < modified(&source),
            "the fixture must start with an artifact older than a source"
        );

        build_then_stamp(|| Ok(()), || stamp_built(directory.path()))
            .expect("a successful build stamps its artifacts");

        assert!(
            artifacts
                .iter()
                .all(|artifact| modified(artifact) > modified(&source)),
            "a stamped artifact is not newer than a source touched before the stamp"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_artifact_without_write_permission_is_stamped() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temp directory");
        let built = at(10);
        let stamped = at(50);
        let artifacts = populate(directory.path(), at(5), built);
        for artifact in &artifacts {
            let mut permissions = fs::metadata(artifact).expect("metadata").permissions();
            permissions.set_mode(0o555);
            fs::set_permissions(artifact, permissions).expect("clear write permission");
        }

        stamp(directory.path(), stamped).expect("stamp read-only artifacts");

        assert!(
            artifacts
                .iter()
                .all(|artifact| modified(artifact) == stamped),
            "an artifact without write permission must still be stamped"
        );
    }

    #[test]
    fn a_failed_build_stamps_nothing() {
        let directory = tempfile::tempdir().expect("temp directory");
        let built = at(20);
        let later = at(40);
        let artifacts = populate(directory.path(), at(10), built);

        let outcome = build_then_stamp(
            || Err("the service build failed".into()),
            || stamp(directory.path(), later),
        );

        assert!(outcome.is_err(), "a failed build is a failed command");
        assert!(
            artifacts
                .iter()
                .all(|artifact| modified(artifact) == built),
            "a failed build must not stamp an artifact"
        );
    }

    /// The xtask subcommand the root build script names is one clap declares,
    /// so the gate's remedy reaches a command that exists.
    #[test]
    fn the_root_build_script_names_a_declared_xtask_subcommand() {
        use clap::Parser;
        let command = zeroship_testkit::prebuilt::BUILD_TEST_ARTIFACTS;
        let tokens: Vec<&str> = command.split_whitespace().collect();
        assert_eq!(
            tokens,
            ["cargo", "xtask", "build-test-artifacts"],
            "the root build script must run the xtask command that owns the service build"
        );
        assert!(
            crate::Args::try_parse_from(["xtask", tokens[2]]).is_ok(),
            "xtask declares no {} subcommand",
            tokens[2]
        );
    }
}
