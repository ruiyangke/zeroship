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
    for package in [
        "zeroship-migrate-node",
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
