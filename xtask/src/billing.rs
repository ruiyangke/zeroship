use crate::{doctests, migrations, nextest, Result};

const PACKAGES: &[&str] = &[
    "zeroship-control",
    "zeroship-migrate-server",
    "zeroship-metering",
    "zeroship-stream",
];

pub fn run() -> Result<()> {
    migrations::build_host()?;
    migrations::build_services()?;
    let _platform = zeroship_testkit::postgres::platform();
    let tests = nextest(
        PACKAGES,
        "billing packages with owned PostgreSQL and Redpanda fixtures",
    );
    let docs = doctests(PACKAGES, "billing package doctests");
    tests.and(docs)
}
