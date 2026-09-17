use zeroship_id::{typed_id, AppId};

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
