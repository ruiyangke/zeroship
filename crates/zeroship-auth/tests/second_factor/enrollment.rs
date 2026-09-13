//! Enrollment, rotation and removal through authenticated account routes.

#![allow(clippy::future_not_send)]

use super::fixtures::*;
use crate::common::{self, CapturingMailer, auth_server::AuthServer, database::Database};
use serde_json::{Value, json};
use std::sync::Arc;
use zeroship_auth::{
    identity::totp,
    store::{sessions, totp as totp_store, users},
};

struct AccountSession {
    server: AuthServer,
    mailer: Arc<CapturingMailer>,
    user: users::UserRow,
    session: uuid::Uuid,
    csrf: String,
}

impl AccountSession {
    async fn new(database: &Database) -> Self {
        let mailer = Arc::new(CapturingMailer::default());
        let server = AuthServer::with_mailer(database, mailer.clone()).await;
        let user = account(&server, "creator@example.test").await;
        let response = password_login(&server, &user, PASSWORD, "/me").await;
        assert_session(&server, &response, &user.id, "/me", "pwd", &["pwd"]).await;
        let session = common::read_set_cookie(&response, "__Host-zsidp_session")
            .unwrap()
            .parse()
            .unwrap();
        let page = server
            .http
            .get(format!("{}/me", server.auth_base))
            .unwrap()
            .header("cookie", format!("__Host-zsidp_session={session}"))
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(page.status().as_u16(), 200);
        let csrf = common::read_set_cookie(&page, "__Host-zsidp_csrf").unwrap();
        Self {
            server,
            mailer,
            user,
            session,
            csrf,
        }
    }

    fn cookies(&self) -> String {
        format!(
            "__Host-zsidp_session={}; __Host-zsidp_csrf={}",
            self.session, self.csrf
        )
    }

    async fn submit(&self, route: &str, fields: &[(&str, &str)]) -> cyper::Response {
        let mut fields = fields.to_vec();
        fields.push(("csrf", &self.csrf));
        post(
            &self.server,
            &format!("/me/2fa/{route}"),
            &self.cookies(),
            &fields,
        )
        .await
    }

    async fn stored_secret(&self) -> Option<Vec<u8>> {
        let credential = totp_store::find(&self.server.pg, &self.user.id)
            .await
            .unwrap()?;
        let key =
            totp::key_from_config(self.server.config.settings.totp_enc_key.expose_str()).unwrap();
        Some(totp::decrypt_secret(&key, &self.user.id, &credential.encrypted_secret).unwrap())
    }

    async fn assert_session_survives(&self) {
        let session = sessions::validate(&self.server.pg, self.session)
            .await
            .unwrap()
            .expect("factor management preserves the caller's session");
        assert_eq!(session.user_id, self.user.id);
    }

    async fn assert_unchanged(&self, device: &Authenticator) {
        assert_eq!(
            self.stored_secret().await.as_deref(),
            Some(device.secret.as_slice())
        );
        assert!(
            totp_store::is_enabled(&self.server.pg, &self.user.id)
                .await
                .unwrap()
        );
        assert_eq!(
            unused_backups(&self.server, &self.user.id).await,
            device.backups.len()
        );
        assert!(self.mailer.sent().is_empty());
        self.assert_session_survives().await;
    }

    async fn provisioning(&self, response: cyper::Response) -> Authenticator {
        assert_eq!(response.status().as_u16(), 200);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["confirmed"], false);
        let encoded = body["secret"]
            .as_str()
            .expect("enrollment supplies a secret");
        let uri = url::Url::parse(body["otpauth_uri"].as_str().unwrap()).unwrap();
        assert_eq!(uri.scheme(), "otpauth");
        assert_eq!(uri.host_str(), Some("totp"));
        assert_eq!(
            uri.query_pairs()
                .find(|(key, _)| key == "secret")
                .unwrap()
                .1,
            encoded
        );
        let secret = totp_rs::Secret::Encoded(encoded.into()).to_bytes().unwrap();
        assert_eq!(
            self.stored_secret().await.as_deref(),
            Some(secret.as_slice())
        );
        assert!(
            !totp_store::is_enabled(&self.server.pg, &self.user.id)
                .await
                .unwrap()
        );
        self.assert_session_survives().await;
        Authenticator {
            secret,
            backups: Vec::new(),
        }
    }

    async fn confirm(&self, device: &mut Authenticator) {
        let response = self.submit("confirm", &[("code", &device.code())]).await;
        assert_eq!(response.status().as_u16(), 200);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["confirmed"], true);
        device.backups = body["backup_codes"]
            .as_array()
            .expect("confirmation supplies backup codes")
            .iter()
            .map(|value| value.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(device.backups.len(), totp::BACKUP_CODE_COUNT);
        assert_eq!(
            unused_backups(&self.server, &self.user.id).await,
            device.backups.len()
        );
        assert!(
            totp_store::is_enabled(&self.server.pg, &self.user.id)
                .await
                .unwrap()
        );
    }

    fn assert_notice(&self) {
        let sent = self.mailer.sent();
        let [message] = sent.as_slice() else {
            panic!("factor removal sends the account holder a notice: {sent:?}")
        };
        assert_eq!(message.to.email, self.user.email);
        assert!(message.subject.to_lowercase().contains("two-factor"));
        for body in [
            message.text.as_str(),
            message.html.as_deref().expect("HTML notice"),
        ] {
            assert!(body.contains("removed"));
            assert!(body.contains(&format!("{}/forgot", self.server.auth_base)));
        }
    }
}

async fn assert_json_refusal(response: cyper::Response, status: u16, error: &str) {
    assert_eq!(response.status().as_u16(), status);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({"error": error})
    );
}

#[ntex::test]
async fn initial_and_pending_enrollment_need_no_reauth_or_removal_notice() {
    Database::run(async |database| {
        let session = AccountSession::new(database).await;
        assert!(session.stored_secret().await.is_none());
        let first = session
            .provisioning(session.submit("enroll", &[]).await)
            .await;
        assert!(session.mailer.sent().is_empty());
        let mut replacement = session
            .provisioning(session.submit("enroll", &[]).await)
            .await;
        assert_ne!(replacement.secret, first.secret);
        assert!(session.mailer.sent().is_empty());
        let (wrong, deadline) = replacement.rejected_code();
        let response = session.submit("confirm", &[("code", &wrong)]).await;
        assert!(unix_now() <= deadline);
        assert_json_refusal(response, 401, "invalid_code").await;
        assert!(
            !totp_store::is_enabled(&session.server.pg, &session.user.id)
                .await
                .unwrap()
        );
        assert_eq!(unused_backups(&session.server, &session.user.id).await, 0);
        session.confirm(&mut replacement).await;
        assert!(session.mailer.sent().is_empty());
        let mut challenge = Challenge::password(&session.server, &session.user, PASSWORD).await;
        let response = challenge.submit(&session.server, &replacement.code()).await;
        assert_session(
            &session.server,
            &response,
            &session.user.id,
            &challenge.return_to,
            "pwd",
            &["pwd", "otp"],
        )
        .await;
    })
    .await;
}

enum RejectedProof {
    Missing,
    Password,
    Code,
}

async fn password_rotation_after_refusal(proof: RejectedProof) {
    Database::run(async |database| {
        let session = AccountSession::new(database).await;
        let device = Authenticator::confirmed(&session.server, &session.user.id).await;
        let (wrong, deadline) = device.rejected_code();
        let fields = match proof {
            RejectedProof::Missing => vec![],
            RejectedProof::Password => vec![("password", "incorrect account password")],
            RejectedProof::Code => vec![("code", wrong.as_str())],
        };
        let response = session.submit("enroll", &fields).await;
        assert!(unix_now() <= deadline);
        assert_json_refusal(response, 401, "reauth_required").await;
        session.assert_unchanged(&device).await;
        let mut replacement = session
            .provisioning(session.submit("enroll", &[("password", PASSWORD)]).await)
            .await;
        assert_ne!(replacement.secret, device.secret);
        session.assert_notice();
        session.confirm(&mut replacement).await;
        let mut challenge = Challenge::password(&session.server, &session.user, PASSWORD).await;
        let response = challenge.submit(&session.server, &replacement.code()).await;
        assert_session(
            &session.server,
            &response,
            &session.user.id,
            &challenge.return_to,
            "pwd",
            &["pwd", "otp"],
        )
        .await;
        session.assert_session_survives().await;
    })
    .await;
}

#[ntex::test]
async fn active_enrollment_refuses_missing_proof_then_allows_password_rotation() {
    password_rotation_after_refusal(RejectedProof::Missing).await;
}

#[ntex::test]
async fn active_enrollment_refuses_wrong_password_then_allows_password_rotation() {
    password_rotation_after_refusal(RejectedProof::Password).await;
}

#[ntex::test]
async fn active_enrollment_refuses_wrong_code_then_allows_password_rotation() {
    password_rotation_after_refusal(RejectedProof::Code).await;
}

#[ntex::test]
async fn active_enrollment_accepts_the_current_authenticator_as_reauth_proof() {
    Database::run(async |database| {
        let session = AccountSession::new(database).await;
        let device = Authenticator::confirmed(&session.server, &session.user.id).await;
        let mut replacement = session
            .provisioning(session.submit("enroll", &[("code", &device.code())]).await)
            .await;
        assert_ne!(replacement.secret, device.secret);
        session.assert_notice();
        session.confirm(&mut replacement).await;
        session.assert_session_survives().await;
    })
    .await;
}

#[ntex::test]
async fn disable_refuses_unproven_requests_then_notifies_without_revoking_the_session() {
    Database::run(async |database| {
        let session = AccountSession::new(database).await;
        let device = Authenticator::confirmed(&session.server, &session.user.id).await;
        for fields in [vec![], vec![("password", "incorrect account password")]] {
            assert_json_refusal(
                session.submit("disable", &fields).await,
                401,
                "reauth_required",
            )
            .await;
            session.assert_unchanged(&device).await;
        }
        let response = session.submit("disable", &[("password", PASSWORD)]).await;
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(
            response.json::<Value>().await.unwrap(),
            json!({"disabled": true})
        );
        assert!(session.stored_secret().await.is_none());
        assert_eq!(unused_backups(&session.server, &session.user.id).await, 0);
        session.assert_notice();
        session.assert_session_survives().await;
        let repeated = session.submit("disable", &[]).await;
        assert_eq!(repeated.status().as_u16(), 200);
        session.assert_notice();
        let response = password_login(&session.server, &session.user, PASSWORD, "/me").await;
        assert_session(
            &session.server,
            &response,
            &session.user.id,
            "/me",
            "pwd",
            &["pwd"],
        )
        .await;
    })
    .await;
}

#[ntex::test]
async fn disable_accepts_the_current_authenticator_as_reauth_proof() {
    Database::run(async |database| {
        let session = AccountSession::new(database).await;
        let device = Authenticator::confirmed(&session.server, &session.user.id).await;
        let response = session.submit("disable", &[("code", &device.code())]).await;
        assert_eq!(response.status().as_u16(), 200);
        assert!(session.stored_secret().await.is_none());
        assert_eq!(unused_backups(&session.server, &session.user.id).await, 0);
        session.assert_notice();
        session.assert_session_survives().await;
    })
    .await;
}

#[ntex::test]
async fn factor_management_requires_a_session_and_matching_csrf() {
    Database::run(async |database| {
        let session = AccountSession::new(database).await;
        let device = Authenticator::confirmed(&session.server, &session.user.id).await;
        for route in ["enroll", "confirm", "disable"] {
            let path = format!("/me/2fa/{route}");
            let response = post(
                &session.server,
                &path,
                &format!("__Host-zsidp_csrf={}", session.csrf),
                &[
                    ("csrf", &session.csrf),
                    ("password", PASSWORD),
                    ("code", &device.code()),
                ],
            )
            .await;
            assert_json_refusal(response, 401, "unauthenticated").await;
            let response = post(
                &session.server,
                &path,
                &session.cookies(),
                &[
                    ("csrf", "unrelated-csrf"),
                    ("password", PASSWORD),
                    ("code", &device.code()),
                ],
            )
            .await;
            assert_json_refusal(response, 403, "invalid_request").await;
            session.assert_unchanged(&device).await;
        }
        let response = session.submit("disable", &[("password", PASSWORD)]).await;
        assert_eq!(response.status().as_u16(), 200);
        assert!(session.stored_secret().await.is_none());
        session.assert_notice();
    })
    .await;
}
