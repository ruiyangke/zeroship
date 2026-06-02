use zeroship_core::config::{AuthSection, FileConfig};
use zeroship_control::{resolve_trusted_oauth_clients, AppState};

#[test]
fn file_overlay_clients_drive_trust_resolution() {
    // A deployment names the console's oac_ client id explicitly; only that
    // client is trusted. Nothing else (including the retired builder id) is.
    let file = FileConfig {
        auth: AuthSection {
            trusted_oauth_clients: Some(vec!["oac_console".to_string()]),
            ..AuthSection::default()
        },
        ..FileConfig::default()
    };

    let trusted = resolve_trusted_oauth_clients(&file.auth);

    assert!(AppState::is_trusted_client_id(&trusted, "oac_console"));
    assert!(!AppState::is_trusted_client_id(&trusted, "acme-ci"));
    assert!(!AppState::is_trusted_client_id(&trusted, "zeroship-builder"));
}

#[test]
fn omitted_file_list_is_empty_fail_closed() {
    // The compiled default is EMPTY (fail-closed): with no overlay, NO client
    // is trusted — not the retired `zeroship-builder` id, not anything. A
    // deployment MUST name the console's oac_ client id in
    // `[auth].trusted_oauth_clients` to skip Hydra consent for the console (so
    // the immersive framed login auto-accepts identity consent).
    let file = FileConfig::default();

    let trusted = resolve_trusted_oauth_clients(&file.auth);

    assert!(trusted.is_empty(), "default trusted set must be empty");
    assert!(!AppState::is_trusted_client_id(&trusted, "zeroship-builder"));
    assert!(!AppState::is_trusted_client_id(&trusted, ""));
}
