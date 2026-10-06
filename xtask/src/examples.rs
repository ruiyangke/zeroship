//! The example apps' own Vitest and Playwright suites.
//!
//! These start a live multi-process platform and real browsers, so they run by
//! hand rather than in CI. The Rust crates the examples exercise are tested by
//! their shards.

use crate::{checked, memlock, prepare, script, Result};

/// The example apps whose own suites this area runs, as (directory, package name).
const SUITES: [(&str, &str); 4] = [
    ("examples/storage-probe", "storage-probe"),
    ("examples/storage-gallery", "storage-gallery"),
    ("examples/workflow-probe", "workflow-probe"),
    (
        "examples/workflows-order",
        "zeroship-workflows-order-example",
    ),
];

pub fn run() -> Result<()> {
    memlock::require()?;
    prepare::build_host()?;
    prepare::build_services()?;
    for (directory, _) in SUITES {
        checked(
            &mut script(directory, "test"),
            &format!("{directory} Vitest and Playwright tests"),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Each example suite runs through Node, and each `test` script it runs
    /// needs no package manager of its own.
    #[test]
    fn the_example_suites_run_without_a_package_manager() {
        for (directory, package) in super::SUITES {
            crate::script_contract::assert_no_package_manager(directory, package, "test");
        }
    }
}
