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
    // Hold each migrated server this area's packages join for the whole run, so
    // its first boot is paid once and every test process joins it rather than
    // booting its own. The migration server's cases join a server of their own
    // scope, because they write cluster roles and execution zones.
    let _platform = zeroship_testkit::postgres::platform();
    let _migrated = zeroship_testkit::postgres::migrate_server_platform();
    let tests = nextest(
        PACKAGES,
        "billing packages with owned PostgreSQL and Redpanda fixtures",
    );
    let docs = doctests(PACKAGES, "billing package doctests");
    tests.and(docs)
}
