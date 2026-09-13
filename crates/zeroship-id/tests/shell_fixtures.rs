use std::path::Path;
use std::process::Command;

use zeroship_id::{typed_id, AppId, OrganizationId, ProjectId};

fn organization_fixture(slug: &str) -> (OrganizationId, ProjectId) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new("bash")
        .current_dir(root)
        .args([
            "-euc",
            r#"source tests/lib/organization_fixture.sh
organization_fixture_ids "$1"
printf '%s\n%s\n' "$ZS_FIXTURE_ORGANIZATION_ID" "$ZS_FIXTURE_PROJECT_ID""#,
            "organization-fixture",
            slug,
        ])
        .output()
        .expect("run the organization fixture helper");
    assert!(
        output.status.success(),
        "fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("fixture output is UTF-8");
    let mut ids = stdout.lines();
    let organization = ids.next().expect("organization id was emitted");
    let project = ids.next().expect("project id was emitted");
    assert!(ids.next().is_none(), "fixture emitted unexpected output");
    assert!(OrganizationId::parse(project).is_err());
    assert!(ProjectId::parse(organization).is_err());
    (
        OrganizationId::parse(organization).expect("canonical organization fixture id"),
        ProjectId::parse(project).expect("canonical project fixture id"),
    )
}

#[test]
fn organization_fixture_ids_are_canonical_stable_and_distinct() {
    let first = organization_fixture("billing-e2e");
    assert_eq!(first, organization_fixture("billing-e2e"));
    let other = organization_fixture("auth-ui-consent");
    assert_ne!(first.0, other.0);
    assert_ne!(first.1, other.1);
}

#[test]
fn auth_ui_client_identifies_its_canonical_app() {
    #[derive(serde::Deserialize)]
    struct Fixture {
        app_id: AppId,
        client_id: String,
    }

    let fixture: Fixture =
        serde_json::from_str(include_str!("../../../tests/fixtures/auth_ui_ids.json"))
            .expect("auth UI identity fixture is canonical");
    assert_eq!(
        typed_id::app_id_from_oauth_client_id(&fixture.client_id),
        Some(fixture.app_id.clone())
    );
    assert_eq!(
        fixture.client_id,
        typed_id::app_oauth_client_id(&fixture.app_id)
    );
    assert_eq!(
        typed_id::app_id_from_oauth_client_id(fixture.app_id.as_str()),
        None
    );
}
