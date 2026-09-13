use crate::{cargo, checked, migrations, Result};

pub fn run() -> Result<()> {
    migrations::build_host()?;
    checked(
        cargo().args([
            "test",
            "--locked",
            "--no-fail-fast",
            "-p",
            "zeroship-auth",
            "-p",
            "zeroship-authn",
            "-p",
            "zeroship-authz",
            "-p",
            "zeroship-mailer",
            "-p",
            "zeroship-gateway",
            "--",
            "--test-threads",
            "4",
        ]),
        "auth package tests with owned backing services",
    )
}
