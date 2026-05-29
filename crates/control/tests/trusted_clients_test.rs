use zeroship_core::config::{AuthSection, FileConfig};
use zeroship_control::{resolve_trusted_oauth_clients, AppState};

#[test]
fn file_overlay_clients_drive_trust_resolution() {
    let file = FileConfig {
        auth: AuthSection {
            trusted_oauth_clients: vec!["zeroship-console".to_string()],
            ..AuthSection::default()
        },
        ..FileConfig::default()
    };

    let trusted = resolve_trusted_oauth_clients(&file.auth);

    assert!(AppState::is_trusted_client_id(
        &trusted,
        "zeroship-console"
    ));
    assert!(!AppState::is_trusted_client_id(&trusted, "acme-ci"));
    assert!(!AppState::is_trusted_client_id(
        &trusted,
        "zeroship-builder"
    ));
}

#[test]
fn omitted_file_list_uses_builder_compiled_default() {
    let file = FileConfig::default();

    let trusted = resolve_trusted_oauth_clients(&file.auth);

    assert!(AppState::is_trusted_client_id(
        &trusted,
        "zeroship-builder"
    ));
    assert!(!AppState::is_trusted_client_id(&trusted, ""));
    assert!(!AppState::is_trusted_client_id(
        &trusted,
        "zeroship-builder-malicious-suffix"
    ));
}
