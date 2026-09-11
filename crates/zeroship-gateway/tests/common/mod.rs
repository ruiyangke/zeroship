//! Shared fixture helpers for this crate's live-database tests.
//!
//! Cargo convention: `tests/common/mod.rs` (subdirectory + `mod.rs`) is not
//! compiled as a test target of its own, so each test file that wants it adds
//! `mod common;`. Only the files that need a helper declare it.

#![allow(dead_code)]

use uuid::Uuid;

/// The test database, with the live-database preflight already run.
///
/// EVERY GATE THAT READS THE PLATFORM SCHEMA GOES THROUGH HERE, and the reason
/// is a run that cost real time. (`db_pool_smoke` is the one target that reads
/// no schema; it takes [`require_any_db`], which is the same refusal asking for
/// less.) On 2026-09-07 the database the overlay names held every
/// schema these targets need and had never seen
/// `db/migrations-ts/20260906000100_apps_organization_and_billing_subject.ts`,
/// so `zeroship.apps` had no `organization_id` and this crate's five database
/// targets reported a stale fixture as named tests FAILING. A suite that cannot
/// tell "the code is wrong" from "my database is behind" is not an oracle;
/// `platform_fixture::live_db` makes that a refusal naming
/// `deploy/ops/db-migrate.sh` instead.
///
/// AN UNCONFIGURED DSN IS A REFUSAL, NOT A SKIP. This function used to return
/// `None` there and every caller announced a skip, which cargo counts as a
/// pass: a checkout with no overlay reported this crate's whole browser
/// identity surface green while running none of it. There is no environment
/// variable that turns that back into a skip. The two ways to have no verdict -
/// "nobody said which database" and "the database named cannot serve this
/// suite" - read as one problem to the person hitting them, so they print the
/// same block; [`platform_fixture::live_db::require_configured`] is what joins
/// them, and it names `tests/provision_test_backends.sh` for the first and
/// `deploy/ops/db-migrate.sh` for the second.
///
/// The preflight memoises per process, so calling this from every gate in a
/// file costs one probe -- and it has to be every gate, because
/// `cargo test --exact <one>` makes any of them the first to touch a database.
pub fn require_platform_db() -> String {
    static CHECKED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CHECKED
        .get_or_init(|| {
            platform_fixture::live_db::require_configured(
                zeroship_core::config::test_database_url_opt(),
                platform_fixture::live_db::PLATFORM_SCHEMAS,
            )
        })
        .clone()
}

/// A reachable PostgreSQL, with NO schema requirement.
///
/// `db_pool_smoke` runs `SELECT 1` through the gateway's per-worker pool, so
/// any server that answers is enough and asking for the platform schema would
/// refuse databases that can serve it perfectly. Naming no schema also leaves
/// the migration-journal stage off, which is keyed to the journal schema being
/// asked for.
///
/// It is the same refusal for the same two causes as [`require_platform_db`];
/// only the requirement differs.
pub fn require_any_db() -> String {
    static CHECKED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CHECKED
        .get_or_init(|| {
            platform_fixture::live_db::require_configured(
                zeroship_core::config::test_database_url_opt(),
                &[],
            )
        })
        .clone()
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

#[path = "../../../../tests/fixtures/platform_db/mod.rs"]
mod platform_fixture;
