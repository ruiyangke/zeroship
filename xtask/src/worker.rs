use crate::{doctests, migrations, nextest, Result};

const PACKAGES: &[&str] = &["zeroship-worker"];

pub fn run() -> Result<()> {
    migrations::build_host()?;
    let _platform = zeroship_testkit::postgres::platform();
    let tests = nextest(PACKAGES, "worker package tests with owned backing services");
    let docs = doctests(PACKAGES, "worker package doctests");
    tests.and(docs)
}
