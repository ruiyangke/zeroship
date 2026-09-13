#![allow(clippy::future_not_send)]

use super::fixtures::*;
use crate::common::{self, auth_server::AuthServer, database::Database};
use zeroship_auth::{
    identity::linker::{self, LinkOutcome, LinkResume, ResolvedProfile},
    store::identities,
};

#[ntex::test]
async fn password_confirmation_defers_identity_creation_until_the_second_factor_succeeds() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        let user = account(&server, "creator@example.test").await;
        let device = Authenticator::confirmed(&server, &user.id).await;
        let return_to = AuthServer::fresh_challenge();
        let subject = "123456789";
        let result = linker::resolve_or_link(
            &server.pg,
            &ResolvedProfile {
                provider: "github",
                subject,
                email: &user.email,
                name: None,
                avatar_url: None,
                provider_trusted_for_email: true,
                raw_profile: None,
            },
            LinkResume::ReturnTo(&return_to),
            server
                .config
                .settings
                .stash_signing_key
                .expose_str()
                .as_bytes(),
        )
        .await
        .unwrap();
        let LinkOutcome::NeedsConfirmation { pending_token, .. } = result else {
            panic!("password account requires confirmation")
        };
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("token", &pending_token)
            .finish();
        let form = server
            .http
            .get(format!("{}/link?{query}", server.auth_base))
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(form.status().as_u16(), 200);
        let csrf = common::read_set_cookie(&form, "__Host-zsidp_csrf").unwrap();
        let response = post(
            &server,
            "/link",
            &format!("__Host-zsidp_csrf={csrf}"),
            &[
                ("csrf", &csrf),
                ("token", &pending_token),
                ("password", PASSWORD),
            ],
        )
        .await;
        let mut challenge = Challenge::from_response(response, &return_to).await;
        assert_counts(&server, 0, 0).await;
        challenge.reject_wrong_code(&server, &device).await;
        assert_counts(&server, 0, 0).await;
        let response = challenge.submit(&server, &device.code()).await;
        assert_session(
            &server,
            &response,
            &user.id,
            &return_to,
            "github",
            &["oauth", "pwd", "otp"],
        )
        .await;
        assert_counts(&server, 1, 1).await;
        let identity = identities::find_by_provider_subject(&server.pg, "github", subject)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(identity.user_id, user.id);
        assert_eq!(identity.email_at_link.as_deref(), Some(user.email.as_str()));
    })
    .await;
}
