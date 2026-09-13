use super::*;

#[ntex::test]
async fn callback_binds_state_and_client_before_redeeming_the_code() {
    Database::migrated(async |database| {
        let app = App::seed(database, REDIRECT_URI).await;
        let provider = Provider::start(database).await;
        let rp = rp(&provider.base);
        let (authorize, stash) =
            rp.build_authorize_redirect(&app.client, "/some/path", REDIRECT_URI);
        let parsed = url::Url::parse(&authorize).unwrap();
        assert_eq!(
            parsed.origin(),
            url::Url::parse(&provider.base).unwrap().origin()
        );
        assert_eq!(parsed.path(), "/oauth2/authorize");
        let params = query(&parsed);
        assert_eq!(params["client_id"], app.client);
        assert_eq!(params["scope"], "openid offline_access email profile");
        assert_eq!(params["redirect_uri"], REDIRECT_URI);
        assert_eq!(params["code_challenge_method"], "S256");
        let login = login(&provider, &authorize, &app.email, REDIRECT_URI).await;
        let other_client = zeroship_core::typed_id::app_oauth_client_id(&AppId::mint());
        assert!(matches!(
            rp.finish_callback(&login.code, &login.state, &stash, &other_client)
                .await,
            Err(OidcRpError::ClientMismatch)
        ));
        assert!(matches!(
            rp.finish_callback(&login.code, "different-state", &stash, &app.client)
                .await,
            Err(OidcRpError::StateMismatch)
        ));
        let (claims, original_path, scopes) = rp
            .finish_callback(&login.code, &login.state, &stash, &app.client)
            .await
            .expect("binding failures must leave the code available for its real callback");
        assert_eq!(claims.sub, app.user.as_str());
        assert_eq!(claims.email.as_deref(), Some(app.email.as_str()));
        assert_eq!(original_path, "/some/path");
        assert_eq!(
            scopes
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            ["openid", "offline_access", "email", "profile"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
        assert!(
            rp.finish_callback(&login.code, &login.state, &stash, &app.client)
                .await
                .is_err(),
            "a redeemed authorization code must not be reusable"
        );
    })
    .await;
}
