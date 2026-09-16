use crate::{cargo, checked, root, Result};
use std::process::Command;

pub fn run() -> Result<()> {
    for schema in [
        "crates/zeroship-workflow-schema/schema",
        "crates/zeroship-workflow-manager/schema",
        "crates/zeroship-workflow-manager/schema/deployments",
    ] {
        checked(
            Command::new("node")
                .current_dir(root())
                .arg(format!("{schema}/generate.mjs"))
                .arg("--check"),
            "workflow migration compiler artifacts",
        )?;
    }
    checked(
        cargo().args([
            "test",
            "--manifest-path",
            "xtask/Cargo.toml",
            "--test",
            "workflow_architecture",
        ]),
        "workflow crate boundaries",
    )?;
    checked(
        cargo().args([
            "test",
            "-p",
            "zeroship-workflow",
            "-p",
            "zeroship-workflow-calendar",
            "-p",
            "zeroship-workflow-client",
            "-p",
            "zeroship-workflow-manager",
            "-p",
            "zeroship-workflow-schema",
            "-p",
            "zeroship-workflow-v8",
        ]),
        "workflow engine, schema artifacts, metadata client, manager and binding tests",
    )?;
    checked(
        cargo().args(["test", "-p", "zeroship-workflow-server"]),
        "workflow coordinator store, HTTP and platform authority contracts",
    )?;
    checked(
        cargo().args([
            "test",
            "-p",
            "zeroship-cli",
            "--bin",
            "zeroship",
            "workflow::",
        ]),
        "local workflow identity, retained code and background worker contracts",
    )?;
    checked(
        cargo().args(["test", "-p", "zeroship-cli", "--test", "workflow_local"]),
        "CLI workflow binding and process recovery",
    )?;
    checked(
        cargo().args([
            "test",
            "-p",
            "zeroship-runtime",
            "--test",
            "workflow_dispatch",
        ]),
        "workflow runtime dispatch tests",
    )?;
    checked(
        cargo().args([
            "test",
            "-p",
            "zeroship-control",
            "--test",
            "workflow_e2e",
        ]),
        "workflow API, persistence and deployed acceptance tests",
    )?;
    for package in ["packages/workflows", "packages/eslint-plugin-workflow"] {
        checked(
            Command::new("pnpm")
                .current_dir(root().join(package))
                .arg("test"),
            &format!("{package} tests"),
        )?;
    }
    for example in ["workflow-probe", "workflows-order"] {
        checked(
            Command::new("pnpm")
                .current_dir(root().join("examples").join(example))
                .arg("test"),
            &format!("{example} Vitest and Playwright tests"),
        )?;
    }
    Ok(())
}
