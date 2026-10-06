use crate::{cargo, checked, script, Result};
use xtask::build_chain::BUILD_CHAIN;
use zeroship_testkit::prebuilt;

/// Run the ordered package chain that produces the generated build inputs.
pub fn build_host() -> Result<()> {
    for step in BUILD_CHAIN {
        checked(
            &mut script(step.directory, step.script),
            &format!("build {}", step.package),
        )?;
    }
    Ok(())
}

/// Build the workspace executables the control workflow process suites run.
///
/// This is its own step rather than an entry in `BUILD_CHAIN`: the package
/// chain is JavaScript, and a shard that runs none of Control's workflow suites
/// does not need the service binaries.
pub fn build_services() -> Result<()> {
    checked(
        cargo().args(prebuilt::BUILD_ARGS),
        "build the workflow process executables",
    )
}

/// Build the real CDC relay the data suites start.
pub fn build_relay() -> Result<()> {
    checked(
        cargo().args(["build", "--locked", "-p", "zeroship-data-cdc-server"]),
        "build the real CDC relay",
    )
}

#[cfg(test)]
mod tests {
    /// Every build in the host chain reaches its compiler through Node, and each
    /// script it runs needs no package manager of its own.
    #[test]
    fn the_host_chain_builds_without_a_package_manager() {
        for step in super::BUILD_CHAIN {
            crate::script_contract::assert_no_package_manager(
                step.directory,
                step.package,
                step.script,
            );
        }
    }

    /// The service step names a runnable cargo build, so a test that cannot
    /// find an executable points at a command that produces one.
    #[test]
    fn the_service_step_builds_workspace_targets() {
        let args = super::prebuilt::BUILD_ARGS;
        assert_eq!(
            args.first(),
            Some(&"build"),
            "the service step must run `cargo build`"
        );
        assert!(
            args.contains(&"--locked"),
            "the service step must build against the lockfile"
        );
        assert!(
            !super::prebuilt::ARTIFACTS.is_empty(),
            "the service step declares no artifacts"
        );
    }
}
