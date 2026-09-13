//! Shared fixture helpers for this crate's live-database tests.
//!
//! Cargo convention: `tests/common/mod.rs` (subdirectory + `mod.rs`) is not
//! compiled as a test target of its own, so each test file that wants it adds
//! `mod common;`. Only the files that need a helper declare it.

#![allow(dead_code)]

use uuid::Uuid;

/// Require the shared migrated database still used by the identity fixtures.
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
