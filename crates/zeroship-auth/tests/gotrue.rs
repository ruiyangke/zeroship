//! Signed email requests traverse the production router and the mailer contract.

#![allow(
    clippy::future_not_send,
    reason = "fixtures stay on their compio runtime"
)]

use crate::common::{CapturingMailer, auth_server::AuthServer, database::Database};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::json;
use std::{io::Write as _, sync::Arc};
use zeroship_mailer::suppressions;

const KEY: [u8; 32] = [7; 32];
const MESSAGE_ID: &str = "provider-email-request";

struct Hook {
    auth: AuthServer,
    mailer: Arc<CapturingMailer>,
    _credentials: tempfile::NamedTempFile,
}

impl Hook {
    async fn start(database: &Database, configured: bool) -> Self {
        let mut credentials = tempfile::NamedTempFile::new().unwrap();
        write!(credentials, "v1,whsec_{}", STANDARD.encode(KEY)).unwrap();
        let mut extra = vec![
            "--provider",
            "supabase",
            "--supabase-url",
            "https://provider.example.test",
            "--supabase-anon-key",
            "fixture-public-key",
        ];
        if configured {
            extra.extend([
                "--gotrue-email-hook-secret-file",
                credentials.path().to_str().unwrap(),
            ]);
        }
        let mailer = Arc::new(CapturingMailer::default());
        let auth = AuthServer::configured_with_mailer(database, mailer.clone(), &extra).await;
        Self {
            auth,
            mailer,
            _credentials: credentials,
        }
    }

    async fn post(&self, body: &[u8], timestamp: i64, signature: Option<&str>) -> cyper::Response {
        let request = self
            .auth
            .http
            .request(
                http::Method::POST,
                format!("{}/hooks/gotrue/send-email", self.auth.auth_base),
            )
            .unwrap()
            .header("content-type", "application/json")
            .unwrap()
            .header("webhook-id", MESSAGE_ID)
            .unwrap()
            .header("webhook-timestamp", timestamp.to_string())
            .unwrap()
            .body(body.to_vec());
        let request = if let Some(signature) = signature {
            request.header("webhook-signature", signature).unwrap()
        } else {
            request
        };
        request.send().await.unwrap()
    }

    async fn signed_post(&self, body: &[u8]) -> cyper::Response {
        let timestamp = chrono::Utc::now().timestamp();
        self.post(body, timestamp, Some(&sign(body, timestamp)))
            .await
    }
}

fn sign(body: &[u8], timestamp: i64) -> String {
    let mut signed = format!("{MESSAGE_ID}.{timestamp}.").into_bytes();
    signed.extend_from_slice(body);
    format!(
        "v1,{}",
        STANDARD.encode(zeroship_core::auth::hmac_sha256(&KEY, &signed))
    )
}

fn payload(email: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "user": { "email": email, "user_metadata": { "name": "Recipient" } },
        "email_data": {
            "token_hash": "verification-token-hash",
            "email_action_type": "signup",
            "redirect_to": "https://app.example.test/welcome",
            "site_url": "https://app.example.test"
        }
    }))
    .unwrap()
}

#[ntex::test]
async fn signature_refusals_cannot_send_mail_and_the_authentic_request_succeeds() {
    Database::run(async |database| {
        let hook = Hook::start(database, true).await;
        let body = payload("recipient@example.test");
        let timestamp = chrono::Utc::now().timestamp();
        let signature = sign(&body, timestamp);
        let mut tampered = body.clone();
        tampered.push(b' ');
        assert_eq!(
            hook.post(&body, timestamp, None).await.status().as_u16(),
            401
        );
        assert_eq!(
            hook.post(&tampered, timestamp, Some(&signature))
                .await
                .status()
                .as_u16(),
            401
        );
        let stale = timestamp - 86_400;
        assert_eq!(
            hook.post(&body, stale, Some(&sign(&body, stale)))
                .await
                .status()
                .as_u16(),
            401
        );
        assert!(hook.mailer.sent().is_empty());
        assert_eq!(hook.signed_post(&body).await.status().as_u16(), 200);
        let sent = hook.mailer.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].to.email, "recipient@example.test");
        assert!(sent[0].text.contains("verification-token-hash"));
        assert!(
            sent[0]
                .text
                .contains("https://provider.example.test/auth/v1/verify")
        );
    })
    .await;
}

#[ntex::test]
async fn an_unconfigured_hook_cannot_send_even_a_correctly_signed_request() {
    Database::run(async |database| {
        let hook = Hook::start(database, false).await;
        assert_eq!(
            hook.signed_post(&payload("recipient@example.test"))
                .await
                .status()
                .as_u16(),
            401
        );
        assert!(hook.mailer.sent().is_empty());
    })
    .await;
}

#[ntex::test]
async fn suppression_acknowledges_without_delivery_and_leaves_other_recipients_usable() {
    Database::run(async |database| {
        let hook = Hook::start(database, true).await;
        suppressions::add(&hook.auth.pg, "recipient@example.test", "complaint", None)
            .await
            .unwrap();
        let suppressed = hook.signed_post(&payload("Recipient@Example.Test")).await;
        assert_eq!(suppressed.status().as_u16(), 200);
        let confirmation = suppressed.text().await.unwrap();
        assert!(hook.mailer.sent().is_empty());
        let allowed = hook.signed_post(&payload("other@example.test")).await;
        assert_eq!(allowed.status().as_u16(), 200);
        assert_eq!(allowed.text().await.unwrap(), confirmation);
        let sent = hook.mailer.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].to.email, "other@example.test");
    })
    .await;
}

#[ntex::test]
async fn an_unavailable_suppression_lookup_refuses_delivery_and_a_retry_can_recover() {
    Database::run(async |database| {
        let hook = Hook::start(database, true).await;
        let admin = database.connect().await;
        admin.batch_execute("ALTER TABLE zeroship.email_suppressions RENAME TO fixture_unavailable_suppressions").await.unwrap();
        let body = payload("recipient@example.test");
        assert_eq!(hook.signed_post(&body).await.status().as_u16(), 500);
        assert!(hook.mailer.sent().is_empty());
        admin.batch_execute("ALTER TABLE zeroship.fixture_unavailable_suppressions RENAME TO email_suppressions").await.unwrap();
        assert_eq!(hook.signed_post(&body).await.status().as_u16(), 200);
        assert_eq!(hook.mailer.sent().len(), 1);
    }).await;
}
