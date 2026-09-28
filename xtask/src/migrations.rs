use crate::{cargo, checked, script, Result};

/// The Node host chain, in dependency order, as (directory, package name).
///
/// The whole chain, not just the addon: `@zeroship/migrate` bundles
/// `@zeroship/schema`, so in a checkout whose `dist` directories are absent
/// esbuild cannot resolve the import and the host never gets built.
const HOST_CHAIN: [(&str, &str); 4] = [
    ("crates/zeroship-migrate-node", "zeroship-migrate-node"),
    ("packages/schema", "@zeroship/schema"),
    ("packages/zero-migrate", "@zeroship/migrate"),
    ("packages/zero-migrate-cli", "zero-migrate-cli"),
];

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
    for (directory, package) in HOST_CHAIN {
        checked(&mut script(directory, "build"), &format!("build {package}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Every build in the host chain reaches its compiler through Node, and each
    /// `build` script it runs needs no package manager of its own.
    #[test]
    fn the_host_chain_builds_without_a_package_manager() {
        for (directory, package) in super::HOST_CHAIN {
            crate::script_contract::assert_no_package_manager(directory, package, "build");
        }
    }
}
