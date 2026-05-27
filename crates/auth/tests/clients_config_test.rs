use zeroship_auth::bootstrap::clients_config::ClientsConfig;

#[test]
fn parses_minimal_first_party_client() {
    let toml = r#"
        [[client]]
        client_id = "console.zeroship.ai"
        redirect_uris = ["https://console.zeroship.ai/auth/callback"]
        scope = "openid offline_access email profile"
        first_party = true
    "#;
    let cfg: ClientsConfig = toml::from_str(toml).expect("parse");
    assert_eq!(cfg.clients.len(), 1);
    let oc = cfg.clients[0].to_oauth2_client();
    assert_eq!(oc.client_id, "console.zeroship.ai");
    assert!(oc.skip_consent);
    assert!(!oc.require_consent);
    assert_eq!(oc.grant_types, vec!["authorization_code", "refresh_token"]);
    assert_eq!(oc.response_types, vec!["code"]);
}
