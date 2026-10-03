use crate::{cargo, checked, root, script, Result};
use std::process::Command;

/// The SDK packages whose own suites this area runs, as (directory, package name).
const SDK_SUITES: [(&str, &str); 2] = [
    ("packages/workflows", "@zeroship/workflows"),
    (
        "packages/eslint-plugin-workflow",
        "@zeroship/eslint-plugin-workflow",
    ),
];

/// The example apps whose own suites this area runs, as (directory, package name).
const EXAMPLE_SUITES: [(&str, &str); 2] = [
    ("examples/workflow-probe", "workflow-probe"),
    (
        "examples/workflows-order",
        "zeroship-workflows-order-example",
    ),
];

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
            "main",
            "workflow::",
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
            "zeroship-workflow-runner",
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
        cargo().args([
            "test",
            "-p",
            "zeroship-cli",
            "--test",
            "e2e",
            "workflow_local::",
        ]),
        "CLI workflow binding and process recovery",
    )?;
    checked(
        cargo().args([
            "test",
            "-p",
            "zeroship-control",
            "--test",
            "workflow_e2e",
            "--test",
            "control_boot_test",
        ]),
        "workflow API, persistence, boot and deployed acceptance tests",
    )?;
    for (directory, _) in SDK_SUITES {
        checked(
            &mut script(directory, "test"),
            &format!("{directory} tests"),
        )?;
    }
    for (directory, _) in EXAMPLE_SUITES {
        checked(
            &mut script(directory, "test"),
            &format!("{directory} Vitest and Playwright tests"),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Each SDK and example suite runs through Node, and each `test` script it
    /// runs needs no package manager of its own.
    #[test]
    fn the_javascript_suites_run_without_a_package_manager() {
        for (directory, package) in super::SDK_SUITES.into_iter().chain(super::EXAMPLE_SUITES) {
            crate::script_contract::assert_no_package_manager(directory, package, "test");
        }
    }
}
