use crate::{cargo, checked, script, Result};
use xtask::build_chain::BUILD_CHAIN;

pub fn run() -> Result<()> {
    build_host()?;
    checked(
        cargo().args([
            "test",
            "-p",
            "zeroship-migrate-node",
            "--test",
            "platform_corpus",
        ]),
        "platform migration corpus",
    )
}

pub fn build_host() -> Result<()> {
    for step in BUILD_CHAIN {
        checked(
            &mut script(step.directory, step.script),
            &format!("build {}", step.package),
        )?;
    }
    Ok(())
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
}
