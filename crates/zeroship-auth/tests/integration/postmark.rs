//! Delivery events traverse the production router and persist under the auth role.

#![allow(
    clippy::future_not_send,
    reason = "fixtures stay on their compio runtime"
)]

use crate::support::{auth_server::AuthServer, database::Database};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::io::Write as _;
use std::sync::Arc;
use uuid::Uuid;
use zeroship_mailer::suppressions;

struct Webhook {
    auth: AuthServer,
    request_id: String,
    _credentials: tempfile::NamedTempFile,
}

impl Webhook {
    async fn start(database: &Database, user: Option<&str>, password: Option<&str>) -> Self {
        let mut credentials = tempfile::NamedTempFile::new().unwrap();
        let mut extra = vec!["--postmark-webhook-user", user.unwrap_or("")];
        if let Some(password) = password {
            credentials.write_all(password.as_bytes()).unwrap();
            extra.extend([
                "--postmark-webhook-password-file",
                credentials.path().to_str().unwrap(),
            ]);
        }
        let auth = AuthServer::configured(database, Arc::new(AuthServer::fixture_issuer()), &extra)
            .await;
        Self {
            auth,
            request_id: format!("postmark-{}", Uuid::new_v4().simple()),
            _credentials: credentials,
        }
    }

    async fn post(&self, payload: &Value, credentials: Option<&str>) -> cyper::Response {
        let request = self
            .auth
            .http
            .request(
                http::Method::POST,
                format!("{}/webhooks/postmark", self.auth.auth_base),
            )
            .unwrap()
            .header("content-type", "application/json")
            .unwrap()
            .header("x-request-id", self.request_id.clone())
            .unwrap()
            .body(serde_json::to_vec(payload).unwrap());
        let request = if let Some(credentials) = credentials {
            request
                .header(
                    "authorization",
                    format!("Basic {}", STANDARD.encode(credentials)),
                )
                .unwrap()
        } else {
            request
        };
        request.send().await.unwrap()
    }

    /// The suppression rows for one address, so a shared database can only
    /// answer about the address this case minted.
    async fn suppressed_address(&self, email: &str) -> Vec<String> {
        self.auth
            .pg
            .query(
                "SELECT email::text FROM zeroship.email_suppressions \
                 WHERE email = $1::citext ORDER BY email",
                &[&email],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect()
    }

    /// The audit rows this case's request produced, filtered by the per-case
    /// request id so another case's events on the shared database are ignored.
    async fn audit_rows(&self, event_type: &str) -> Vec<compio_postgres::Row> {
        self.auth
            .pg
            .query(
                "SELECT outcome, auth_method, detail FROM zeroship.audit_events \
                 WHERE event_type = $1 AND request_id = $2 ORDER BY id",
                &[&event_type, &self.request_id],
            )
            .await
            .unwrap()
    }
}

const CREDENTIALS: &str = "webhook-user:webhook-password";

fn unique_email(label: &str) -> String {
    format!("{label}-{}@example.test", Uuid::new_v4().simple())
}

fn bounce(email: &str, kind: &str) -> Value {
    json!({ "RecordType": "Bounce", "Email": email, "Type": kind, "Description": "Provider delivery failure" })
}

#[ntex::test]
async fn permanent_failures_suppress_recipients_and_complaints_replace_the_reason() {
    Database::run(async |database| {
        let hook = Webhook::start(database, Some("webhook-user"), Some("webhook-password")).await;
        let email = unique_email("postmark-recipient");
        let event = bounce(&email.to_ascii_uppercase(), "HardBounce");
        for _ in 0..2 {
            assert_eq!(hook.post(&event, Some(CREDENTIALS)).await.status().as_u16(), 200);
            assert!(suppressions::is_suppressed(&hook.auth.pg, &email).await.unwrap());
            assert_eq!(hook.suppressed_address(&email).await.len(), 1);
        }
        let row = hook.auth.pg.query_one(
            "SELECT reason, provider_msg FROM zeroship.email_suppressions WHERE email = $1::citext",
            &[&email],
        ).await.unwrap();
        assert_eq!(row.get::<_, String>("reason"), "postmark_HardBounce");
        assert_eq!(row.get::<_, String>("provider_msg"), "Provider delivery failure");

        let complaint = json!({
            "RecordType": "SpamComplaint", "Email": email, "Description": "Reported as spam"
        });
        assert_eq!(hook.post(&complaint, Some(CREDENTIALS)).await.status().as_u16(), 200);
        assert_eq!(hook.suppressed_address(&email).await.len(), 1);
        let row = hook.auth.pg.query_one(
            "SELECT reason, provider_msg FROM zeroship.email_suppressions WHERE email = $1::citext",
            &[&email],
        ).await.unwrap();
        assert_eq!(row.get::<_, String>("reason"), "postmark_complaint");
        assert_eq!(row.get::<_, String>("provider_msg"), "Reported as spam");
        let audits = hook.audit_rows("mailer_complaint").await;
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].get::<_, String>("outcome"), "success");
        assert_eq!(audits[0].get::<_, String>("auth_method"), "postmark");
        assert_eq!(audits[0].get::<_, Value>("detail"), json!({ "email_domain": "example.test" }));
    }).await;
}

#[ntex::test]
async fn transient_and_unrelated_events_leave_delivery_enabled() {
    Database::run(async |database| {
        let hook = Webhook::start(database, Some("webhook-user"), Some("webhook-password")).await;
        let email = unique_email("postmark-transient");
        for event in [
            bounce(&email, "SoftBounce"),
            json!({ "RecordType": "Delivery", "Email": email }),
        ] {
            assert_eq!(
                hook.post(&event, Some(CREDENTIALS)).await.status().as_u16(),
                200
            );
            assert!(hook.suppressed_address(&email).await.is_empty());
        }
        assert!(
            hook.auth
                .pg
                .query(
                    "SELECT id FROM zeroship.audit_events WHERE request_id = $1",
                    &[&hook.request_id],
                )
                .await
                .unwrap()
                .is_empty()
        );
    })
    .await;
}

#[ntex::test]
async fn rejected_credentials_cannot_suppress_an_address() {
    Database::run(async |database| {
        let hook = Webhook::start(database, Some("webhook-user"), Some("webhook-password")).await;
        let email = unique_email("postmark-rejected");
        let event = bounce(&email, "HardBounce");
        for credentials in [
            None,
            Some("webhook-user:wrong-password"),
            Some("wrong-user:webhook-password"),
        ] {
            assert_eq!(hook.post(&event, credentials).await.status().as_u16(), 401);
            assert!(hook.suppressed_address(&email).await.is_empty());
        }
        assert!(
            hook.auth
                .pg
                .query(
                    "SELECT id FROM zeroship.audit_events WHERE request_id = $1",
                    &[&hook.request_id],
                )
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            hook.post(&event, Some(CREDENTIALS)).await.status().as_u16(),
            200
        );
        assert_eq!(hook.suppressed_address(&email).await, [email]);
    })
    .await;
}

#[ntex::test]
async fn incomplete_configuration_rejects_even_matching_credentials() {
    Database::run(async |database| {
        for (user, password) in [
            (None, None),
            (Some("webhook-user"), None),
            (None, Some("webhook-password")),
        ] {
            let email = unique_email("postmark-incomplete");
            let hook = Webhook::start(database, user, password).await;
            assert_eq!(
                hook.post(&bounce(&email, "HardBounce"), Some(CREDENTIALS))
                    .await
                    .status()
                    .as_u16(),
                401
            );
            assert!(hook.suppressed_address(&email).await.is_empty());
        }
    })
    .await;
}

#[ntex::test]
async fn database_failure_is_retryable_and_never_audited_as_success() {
    // Platform-global: ALTER TABLE zeroship.email_suppressions ADD/DROP CONSTRAINT.
    Database::run_fresh(async |database| {
        let hook = Webhook::start(database, Some("webhook-user"), Some("webhook-password")).await;
        let admin = database.connect().await;
        admin.batch_execute(
            "ALTER TABLE zeroship.email_suppressions ADD CONSTRAINT refuse_delivery_event \
             CHECK (email <> 'recipient@example.test'::citext)",
        ).await.unwrap();
        let event = bounce("recipient@example.test", "HardBounce");
        assert_eq!(hook.post(&event, Some(CREDENTIALS)).await.status().as_u16(), 500);
        assert!(hook.suppressed_address("recipient@example.test").await.is_empty());
        assert!(hook.auth.pg.query("SELECT id FROM zeroship.audit_events", &[]).await.unwrap().is_empty());

        admin.batch_execute("ALTER TABLE zeroship.email_suppressions DROP CONSTRAINT refuse_delivery_event").await.unwrap();
        assert_eq!(hook.post(&event, Some(CREDENTIALS)).await.status().as_u16(), 200);
        assert_eq!(hook.suppressed_address("recipient@example.test").await, ["recipient@example.test"]);
        let audit = hook.auth.pg.query_one(
            "SELECT outcome, detail FROM zeroship.audit_events WHERE event_type = 'mailer_bounce'", &[],
        ).await.unwrap();
        assert_eq!(audit.get::<_, String>("outcome"), "success");
        assert_eq!(audit.get::<_, Value>("detail"), json!({
            "email_domain": "example.test", "bounce_type": "HardBounce"
        }));
    }).await;
}
