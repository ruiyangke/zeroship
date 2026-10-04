//! The workspace executables the control workflow process suites run.
//!
//! The process contracts - `workflow_e2e`, `worker_retirement_e2e` and the
//! boot suite in `main` - start the real Control, worker, gateway, CDC relay
//! and workflow manager binaries, and the `workflow-test-environment` example
//! that owns their migrated databases. The build chain produces those
//! executables with [`BUILD_ARGS`] before the suites run, so a test process
//! only locates an artifact cargo already wrote rather than starting a cargo
//! of its own.
//!
//! A test process locates an executable beside the profile directory holding
//! its own binary, and [`resolve`] refuses - naming [`REBUILD`] - when the
//! artifact is absent, has no build record, or is older than a source that
//! record lists. Cargo writes that record as a Makefile-style dep-info file
//! next to the artifact, so the freshness question is cargo's own dependency
//! list rather than a second copy of it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Where a cargo target lands relative to the profile directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Location {
    /// A `[[bin]]` target: `<profile>/<name>`.
    Bin,
    /// An `[[example]]` target: `<profile>/examples/<name>`.
    Example,
}

/// One executable the workflow process suites start.
#[derive(Clone, Copy, Debug)]
pub struct Executable {
    /// The cargo target name, and the key a suite looks the executable up by.
    pub name: &'static str,
    /// The package whose build produces it.
    pub package: &'static str,
    /// Where cargo leaves it.
    pub location: Location,
}

/// The example that owns the platform and creator servers the fleets share.
pub const WORKFLOW_ENVIRONMENT: Executable = Executable {
    name: "workflow-test-environment",
    package: "zeroship-control",
    location: Location::Example,
};

/// Every executable the workflow process suites start.
pub const EXECUTABLES: &[Executable] = &[
    Executable {
        name: "zeroship-control",
        package: "zeroship-control",
        location: Location::Bin,
    },
    Executable {
        name: "zeroship-worker",
        package: "zeroship-worker",
        location: Location::Bin,
    },
    Executable {
        name: "zeroship-gate",
        package: "zeroship-gateway",
        location: Location::Bin,
    },
    Executable {
        name: "zeroship-data-cdc-server",
        package: "zeroship-data-cdc-server",
        location: Location::Bin,
    },
    Executable {
        name: "zeroship-workflow-server",
        package: "zeroship-workflow-server",
        location: Location::Bin,
    },
    Executable {
        name: "dev-provision",
        package: "zeroship-control",
        location: Location::Bin,
    },
    WORKFLOW_ENVIRONMENT,
];

/// The cargo arguments the service build runs to produce every
/// [`EXECUTABLES`] entry.
pub const BUILD_ARGS: &[&str] = &[
    "build",
    "--locked",
    "--bins",
    "-p",
    "zeroship-control",
    "-p",
    "zeroship-worker",
    "-p",
    "zeroship-gateway",
    "-p",
    "zeroship-data-cdc-server",
    "-p",
    "zeroship-workflow-server",
    "--example",
    "workflow-test-environment",
];

/// The repository-relative paths [`BUILD_ARGS`] writes, for the build-input
/// gate that compares a generated artifact against the sources it is built
/// from.
pub const ARTIFACTS: &[&str] = &[
    "target/debug/zeroship-control",
    "target/debug/zeroship-worker",
    "target/debug/zeroship-gate",
    "target/debug/zeroship-data-cdc-server",
    "target/debug/zeroship-workflow-server",
    "target/debug/dev-provision",
    "target/debug/examples/workflow-test-environment",
];

/// The command that rebuilds every executable above and warms the ORM
/// compile-fail project's trybuild target.
pub const REBUILD: &str = "pnpm build:test-artifacts";

/// The `cargo test` invocation that warms the ORM compile-fail project's
/// trybuild target directory.
///
/// `trybuild` compiles its generated project's whole dependency graph inside
/// the test process, into a target directory of its own, on the first run of a
/// cold checkout. The ordered build chain runs this once before the suite, so
/// the test only compiles its fixture bins rather than the ORM's closure.
pub const WARM_ARGS: &[&str] = &[
    "test",
    "-p",
    "zeroship-data-orm",
    "--test",
    "main",
    "integration::derive_contract",
];

/// [`BUILD_ARGS`] and [`WARM_ARGS`] as one command line, so a package script
/// and the build-input gate name the same invocation.
pub fn build_command() -> String {
    let invocation = |args: &[&str]| {
        std::iter::once("cargo")
            .chain(args.iter().copied())
            .collect::<Vec<_>>()
            .join(" ")
    };
    format!("{} && {}", invocation(BUILD_ARGS), invocation(WARM_ARGS))
}

/// The cargo profile directory the running test binary was built into.
///
/// A test binary runs from `<profile>/deps`, so its parent's parent is the
/// directory cargo hardlinks the service executables into.
pub fn profile_directory() -> PathBuf {
    let executable = std::env::current_exe().expect("the running test executable");
    executable
        .parent()
        .and_then(Path::parent)
        .expect("a test executable runs from the profile's deps directory")
        .to_owned()
}

/// Where cargo leaves `executable`, under [`profile_directory`].
pub fn path(executable: &Executable) -> PathBuf {
    let profile = profile_directory();
    match executable.location {
        Location::Bin => profile.join(executable.name),
        Location::Example => profile.join("examples").join(executable.name),
    }
}

/// Why an executable cannot be run.
#[derive(Debug)]
pub enum Unusable {
    /// The artifact is absent.
    Missing(PathBuf),
    /// The artifact exists, but its dep-info record does not, so freshness
    /// cannot be established.
    Unrecorded { executable: PathBuf, dep_info: PathBuf },
    /// A source in the dep-info record is newer than the artifact.
    Stale { executable: PathBuf, source: PathBuf },
}

/// Resolve `executable` under [`profile_directory`], refusing with a message
/// naming [`REBUILD`] when it cannot be run.
pub fn resolve(executable: &Executable) -> PathBuf {
    let resolved = path(executable);
    match inspect_at(&resolved) {
        Ok(()) => resolved,
        Err(Unusable::Missing(path)) => panic!(
            "the {} executable is not built: {}\n\
             build the executables the workflow process suites run with:\n    {REBUILD}",
            executable.name,
            path.display()
        ),
        Err(Unusable::Unrecorded { dep_info, .. }) => panic!(
            "the {} executable has no usable build record: {}\n\
             build the executables the workflow process suites run with:\n    {REBUILD}",
            executable.name,
            dep_info.display()
        ),
        Err(Unusable::Stale {
            executable: path,
            source,
        }) => panic!(
            "the {} executable ({}) is older than the source {}\n\
             rebuild the executables the workflow process suites run with:\n    {REBUILD}",
            executable.name,
            path.display(),
            source.display()
        ),
    }
}

/// Every executable in [`EXECUTABLES`], resolved by target name.
pub fn resolve_all() -> BTreeMap<String, PathBuf> {
    EXECUTABLES
        .iter()
        .map(|executable| (executable.name.to_owned(), resolve(executable)))
        .collect()
}

/// Whether `executable` is present and at least as new as every source cargo
/// recorded when it built the artifact.
pub fn inspect_at(executable: &Path) -> Result<(), Unusable> {
    if !executable.is_file() {
        return Err(Unusable::Missing(executable.to_owned()));
    }
    let dep_info = executable.with_extension("d");
    let Some(text) = std::fs::read_to_string(&dep_info).ok() else {
        return Err(Unusable::Unrecorded {
            executable: executable.to_owned(),
            dep_info,
        });
    };
    let Ok(built) = std::fs::metadata(executable).and_then(|metadata| metadata.modified()) else {
        return Err(Unusable::Unrecorded {
            executable: executable.to_owned(),
            dep_info,
        });
    };
    let Some((source, modified)) = newest_prerequisite(&text) else {
        return Err(Unusable::Unrecorded {
            executable: executable.to_owned(),
            dep_info,
        });
    };
    if modified > built {
        return Err(Unusable::Stale {
            executable: executable.to_owned(),
            source,
        });
    }
    Ok(())
}

/// The newest file a target's dep-info records as an input, or `None` when the
/// record resolves no input.
///
/// The dep-info is `cargo`'s Makefile-style record beside the artifact,
/// `<target>: <source> <source> ...`, with backslash-newline continuations. A
/// prerequisite that does not resolve is skipped, and the target before the
/// colon is not itself a prerequisite.
fn newest_prerequisite(dep_info: &str) -> Option<(PathBuf, SystemTime)> {
    let joined = dep_info.replace("\\\n", " ");
    let (_, prerequisites) = joined.split_once(':')?;
    prerequisites
        .split_whitespace()
        .filter_map(|prerequisite| {
            let path = Path::new(prerequisite);
            let modified = std::fs::metadata(path).ok()?.modified().ok()?;
            Some((path.to_owned(), modified))
        })
        .max_by_key(|(_, modified)| *modified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    fn stamp(path: &Path, seconds: u64) {
        fs::File::options()
            .write(true)
            .open(path)
            .expect("open to stamp")
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds))
            .expect("set modification time");
    }

    #[test]
    fn an_absent_executable_is_unusable() {
        let directory = tempfile::tempdir().expect("temp directory");
        let executable = directory.path().join("zeroship-gate");
        assert!(matches!(
            inspect_at(&executable),
            Err(Unusable::Missing(_))
        ));
    }

    #[test]
    fn an_executable_without_a_build_record_is_unusable() {
        let directory = tempfile::tempdir().expect("temp directory");
        let executable = directory.path().join("zeroship-gate");
        fs::write(&executable, b"").expect("write executable");
        assert!(matches!(
            inspect_at(&executable),
            Err(Unusable::Unrecorded { .. })
        ));
    }

    #[test]
    fn a_record_that_resolves_no_source_is_unusable() {
        let directory = tempfile::tempdir().expect("temp directory");
        let executable = directory.path().join("zeroship-gate");
        fs::write(&executable, b"").expect("write executable");
        fs::write(
            directory.path().join("zeroship-gate.d"),
            format!("{}:\n", executable.display()),
        )
        .expect("write build record");
        assert!(matches!(
            inspect_at(&executable),
            Err(Unusable::Unrecorded { .. })
        ));
    }

    #[test]
    fn an_executable_older_than_a_recorded_source_is_unusable() {
        let directory = tempfile::tempdir().expect("temp directory");
        let executable = directory.path().join("zeroship-gate");
        let source = directory.path().join("gate.rs");
        fs::write(&executable, b"").expect("write executable");
        fs::write(&source, b"").expect("write source");
        fs::write(
            directory.path().join("zeroship-gate.d"),
            format!("{}: {}\n", executable.display(), source.display()),
        )
        .expect("write build record");
        stamp(&source, 20);
        stamp(&executable, 10);
        assert!(matches!(
            inspect_at(&executable),
            Err(Unusable::Stale { .. })
        ));

        stamp(&executable, 20);
        assert!(
            inspect_at(&executable).is_ok(),
            "an artifact built after its source is usable"
        );
    }

    #[test]
    fn the_newest_resolvable_prerequisite_is_the_build_record() {
        let directory = tempfile::tempdir().expect("temp directory");
        let older = directory.path().join("older.rs");
        let newer = directory.path().join("newer.rs");
        fs::write(&older, b"").expect("write older source");
        fs::write(&newer, b"").expect("write newer source");
        stamp(&older, 10);
        stamp(&newer, 20);
        let record = format!(
            "target: {}\\\n {} {}\n",
            directory.path().join("gone.rs").display(),
            older.display(),
            newer.display()
        );
        let (path, modified) = newest_prerequisite(&record).expect("a resolvable prerequisite");
        assert_eq!(path, newer);
        assert_eq!(modified, SystemTime::UNIX_EPOCH + Duration::from_secs(20));
    }

    #[test]
    fn the_build_selection_covers_every_executable() {
        for executable in EXECUTABLES {
            let artifact = match executable.location {
                Location::Bin => format!("target/debug/{}", executable.name),
                Location::Example => format!("target/debug/examples/{}", executable.name),
            };
            assert!(
                ARTIFACTS.contains(&artifact.as_str()),
                "{} names no artifact",
                executable.name
            );
            let selected = BUILD_ARGS
                .windows(2)
                .any(|window| window == ["-p", executable.package]);
            assert!(
                selected,
                "{} names package {} the build does not select",
                executable.name, executable.package
            );
            if executable.location == Location::Example {
                let named = BUILD_ARGS
                    .windows(2)
                    .any(|window| window == ["--example", executable.name]);
                assert!(named, "{} names no example build", executable.name);
            }
        }
        assert!(
            EXECUTABLES.len() >= 6,
            "the executable table lost its entries"
        );
    }
}
