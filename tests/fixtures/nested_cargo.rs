//! A cargo started by a process that cargo itself started.
//!
//! Cargo hands every binary it runs - a test under `cargo test`, xtask under
//! `cargo run` - variables that describe that binary's own package:
//! `CARGO_PKG_*`, `CARGO_MANIFEST_*`, `CARGO_BIN_EXE_*` and `OUT_DIR`. A cargo
//! started with them inherited judges each build script's
//! `rerun-if-env-changed` against them, because that check reads the
//! environment of the cargo running the build rather than the one the script
//! is given. `ring` declares it on the manifest directory, the package name and
//! the version parts, so such a cargo re-runs `ring`'s build script and
//! rebuilds every crate above it, and the next cargo started from a shell
//! rebuilds them back.
use std::process::Command;

/// The prefixes of the variables cargo sets to describe the package whose
/// binary it runs.
const PACKAGE_PREFIXES: [&str; 3] = ["CARGO_PKG_", "CARGO_MANIFEST_", "CARGO_BIN_EXE_"];

/// The cargo that compiled the caller, with this process's environment less
/// every variable that describes the caller's package.
#[must_use]
#[allow(
    clippy::disallowed_methods,
    reason = "remove the parent package's variables from an owned cargo child"
)]
pub fn cargo() -> Command {
    let mut command = Command::new(env!("CARGO"));
    for (name, _) in std::env::vars_os() {
        let text = name.to_string_lossy();
        if text == "OUT_DIR" || PACKAGE_PREFIXES.iter().any(|prefix| text.starts_with(prefix)) {
            command.env_remove(name);
        }
    }
    command
}
