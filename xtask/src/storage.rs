use crate::{cargo, checked, script, Result};

/// The example apps whose own suites this area runs, as (directory, package name).
const EXAMPLE_SUITES: [(&str, &str); 2] = [
    ("examples/storage-probe", "storage-probe"),
    ("examples/storage-gallery", "storage-gallery"),
];

pub fn run() -> Result<()> {
    checked(
        cargo().args([
            "test",
            "-p",
            "zeroship-storage",
            "-p",
            "zeroship-storage-v8",
            "-p",
            "compio-s3",
            "--all-features",
        ]),
        "Rust storage, V8 binding and S3 driver tests",
    )?;
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
    /// Each example suite runs through Node, and each `test` script it runs
    /// needs no package manager of its own.
    #[test]
    fn the_example_suites_run_without_a_package_manager() {
        for (directory, package) in super::EXAMPLE_SUITES {
            crate::script_contract::assert_no_package_manager(directory, package, "test");
        }
    }
}
