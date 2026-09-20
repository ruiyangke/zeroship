use crate::{cargo, checked, root, Result};
use std::process::Command;

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
    // Dependency order, and the whole chain: `@zeroship/migrate` bundles
    // `@zeroship/schema`, so in a checkout whose `dist` directories are absent
    // esbuild cannot resolve the import and the host never gets built.
    for package in [
        "zeroship-migrate-node",
        "@zeroship/schema",
        "@zeroship/migrate",
        "zero-migrate-cli",
    ] {
        checked(
            Command::new("pnpm")
                .current_dir(root())
                .args(["--filter", package, "build"]),
            &format!("build {package}"),
        )?;
    }
    Ok(())
}
