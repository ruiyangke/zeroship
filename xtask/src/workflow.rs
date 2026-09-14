use crate::{cargo, checked, root, Result};
use std::process::Command;

pub fn run() -> Result<()> {
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
            "zeroship-workflow-v8",
            "-p",
            "zeroship-workflow-scheduler",
        ]),
        "workflow engine, binding and scheduler tests",
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
            "workflow_api",
            "--test",
            "workflow_engine_test",
            "--test",
            "workflow_e2e",
        ]),
        "workflow API, persistence and deployed acceptance tests",
    )?;
    for package in ["sdks/workflows", "sdks/eslint-plugin-workflow"] {
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
