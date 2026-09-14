use crate::{cargo, checked, migrations, Result};

pub fn run() -> Result<()> {
    migrations::build_host()?;
    checked(
        cargo().args([
            "test",
            "--locked",
            "--no-fail-fast",
            "-p",
            "zeroship-control",
            "-p",
            "zeroship-migrate-server",
            "-p",
            "zeroship-metering",
            "-p",
            "zeroship-stream",
        ]),
        "billing packages with owned PostgreSQL and Redpanda fixtures",
    )
}
