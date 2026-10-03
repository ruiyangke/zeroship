use crate::{doctests, migrations, nextest, Result};

const PACKAGES: &[&str] = &[
    "zeroship-auth",
    "zeroship-authn",
    "zeroship-authz",
    "zeroship-mailer",
    "zeroship-gateway",
];

pub fn run() -> Result<()> {
    migrations::build_host()?;
    // Hold the worktree's migrated server for the whole run, so its first boot
    // is paid once and every test process joins it rather than booting its own.
    let _platform = zeroship_testkit::postgres::platform();
    let tests = nextest(PACKAGES, "auth package tests with owned backing services");
    let docs = doctests(PACKAGES, "auth package doctests");
    tests.and(docs)
}
