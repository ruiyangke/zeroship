use crate::{cargo, checked, migrations, Result};

pub fn run() -> Result<()> {
    migrations::build_host()?;
    checked(
        cargo().args([
            "test",
            "--locked",
            "--no-fail-fast",
            "-p",
            "zeroship-worker",
            "--",
            "--test-threads",
            "4",
        ]),
        "worker package tests with owned backing services",
    )
}
