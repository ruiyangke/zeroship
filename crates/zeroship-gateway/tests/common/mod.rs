//! Shared fixture helpers for this crate's live-database tests.
//!
//! Cargo convention: `tests/common/mod.rs` (subdirectory + `mod.rs`) is not
//! compiled as a test target of its own, so each test file that wants it adds
//! `mod common;`. Only the files that need a helper declare it.

#![allow(dead_code)]

use uuid::Uuid;

/// The test database, with the live-database preflight already run.
///
/// EVERY DATABASE GATE IN THIS CRATE GOES THROUGH HERE, and the reason is a run
/// that cost real time. On 2026-09-07 the database the overlay names held every
/// schema these targets need and had never seen
/// `db/migrations-ts/20260906000100_apps_organization_and_billing_subject.ts`,
/// so `zeroship.apps` had no `organization_id` and this crate's five database
/// targets reported a stale fixture as named tests FAILING. A suite that cannot
/// tell "the code is wrong" from "my database is behind" is not an oracle;
/// `zeroship_testkit::live_db` makes that a refusal naming
/// `deploy/ops/db-migrate.sh` instead.
///
/// `None` STAYS A SKIP, and that is deliberate rather than an oversight. A
/// developer running `cargo test -p zeroship-gateway` on a checkout with no
/// overlay should get the announcement `tests/lib/skip_census.sh` counts;
/// `tests/run_auth_suite.sh` already treats a skip HERE as a failure, because
/// it provisions a database before it runs these targets.
///
/// The preflight memoises per process, so calling this from every gate in a
/// file costs one probe -- and it has to be every gate, because
/// `cargo test --exact <one>` makes any of them the first to touch a database.
pub fn platform_db_or_skip() -> Option<String> {
    let dsn = zeroship_core::config::test_database_url_opt()?;
    zeroship_testkit::live_db::require_once(&dsn, zeroship_testkit::live_db::PLATFORM_SCHEMAS);
    Some(dsn)
}

/// The project a fixture's `zeroship.apps` row belongs to.
///
/// `apps.project_id` is NOT NULL against a RESTRICT foreign key and a project
/// needs an organization, so an app row can no longer be seeded on its own.
/// This mints an organization with NO members and a project inside it.
///
/// Member-less is the right shape for every app this crate seeds: these tests
/// are about the gateway's browser identity surface - session anchors, cookies,
/// backchannel logout, the OIDC RP - and an app here needs a place to exist
/// rather than a creator. Nothing in this crate reads an organization seat.
pub async fn unowned_project(pg: &compio_postgres::Client) -> String {
    let organization_id = zeroship_core::typed_id::generate("org");
    let project_id = zeroship_core::typed_id::generate("prj");
    pg.execute(
        "INSERT INTO zeroship.organizations (id, slug, name, billing_email) \
         VALUES ($1, $2, 'Gateway Fixture Organization', 'fixture@zeroship.test')",
        &[
            &organization_id,
            &format!("gateway-fixture-{}", Uuid::new_v4().simple()),
        ],
    )
    .await
    .expect("seed fixture organization");
    pg.execute(
        "INSERT INTO zeroship.projects (id, organization_id, slug, name) \
         VALUES ($1, $2, 'default', 'Default')",
        &[&project_id, &organization_id],
    )
    .await
    .expect("seed fixture project");
    project_id
}
