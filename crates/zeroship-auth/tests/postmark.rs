//! Delivery events traverse the production router and persist under the auth role.

#![allow(
    clippy::future_not_send,
    reason = "fixtures stay on their compio runtime"
)]

use crate::common::{auth_server::AuthServer, database::Database};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::io::Write as _;
use zeroship_mailer::suppressions;

struct Webhook {
    auth: AuthServer,
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
        let auth = AuthServer::configured(database, None, &extra).await;
        Self {
            auth,
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

    async fn suppressed_addresses(&self) -> Vec<String> {
        self.auth
            .pg
            .query(
                "SELECT email::text FROM zeroship.email_suppressions ORDER BY email",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect()
    }
}

const CREDENTIALS: &str = "webhook-user:webhook-password";

fn bounce(email: &str, kind: &str) -> Value {
    json!({ "RecordType": "Bounce", "Email": email, "Type": kind, "Description": "Provider delivery failure" })
}

#[ntex::test]
async fn permanent_failures_suppress_recipients_and_complaints_replace_the_reason() {
    Database::run(async |database| {
        let hook = Webhook::start(database, Some("webhook-user"), Some("webhook-password")).await;
        let event = bounce("Recipient@Example.Test", "HardBounce");
        for _ in 0..2 {
            assert_eq!(hook.post(&event, Some(CREDENTIALS)).await.status().as_u16(), 200);
            assert!(suppressions::is_suppressed(&hook.auth.pg, "recipient@example.test").await.unwrap());
            assert_eq!(hook.suppressed_addresses().await.len(), 1);
        }
        let row = hook.auth.pg.query_one(
            "SELECT reason, provider_msg FROM zeroship.email_suppressions", &[],
        ).await.unwrap();
        assert_eq!(row.get::<_, String>("reason"), "postmark_HardBounce");
        assert_eq!(row.get::<_, String>("provider_msg"), "Provider delivery failure");

        let complaint = json!({
            "RecordType": "SpamComplaint", "Email": "recipient@example.test", "Description": "Reported as spam"
        });
        assert_eq!(hook.post(&complaint, Some(CREDENTIALS)).await.status().as_u16(), 200);
        assert_eq!(hook.suppressed_addresses().await.len(), 1);
        let row = hook.auth.pg.query_one(
            "SELECT reason, provider_msg FROM zeroship.email_suppressions", &[],
        ).await.unwrap();
        assert_eq!(row.get::<_, String>("reason"), "postmark_complaint");
        assert_eq!(row.get::<_, String>("provider_msg"), "Reported as spam");
        let audit = hook.auth.pg.query_one(
            "SELECT outcome, auth_method, detail FROM zeroship.audit_events WHERE event_type = 'mailer_complaint'", &[],
        ).await.unwrap();
        assert_eq!(audit.get::<_, String>("outcome"), "success");
        assert_eq!(audit.get::<_, String>("auth_method"), "postmark");
        assert_eq!(audit.get::<_, Value>("detail"), json!({ "email_domain": "example.test" }));
    }).await;
}

#[ntex::test]
async fn transient_and_unrelated_events_leave_delivery_enabled() {
    Database::run(async |database| {
        let hook = Webhook::start(database, Some("webhook-user"), Some("webhook-password")).await;
        for event in [
            bounce("recipient@example.test", "SoftBounce"),
            json!({ "RecordType": "Delivery", "Email": "recipient@example.test" }),
        ] {
            assert_eq!(
                hook.post(&event, Some(CREDENTIALS)).await.status().as_u16(),
                200
            );
            assert!(hook.suppressed_addresses().await.is_empty());
        }
        assert!(
            hook.auth
                .pg
                .query("SELECT id FROM zeroship.audit_events", &[])
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
        let event = bounce("recipient@example.test", "HardBounce");
        for credentials in [
            None,
            Some("webhook-user:wrong-password"),
            Some("wrong-user:webhook-password"),
        ] {
            assert_eq!(hook.post(&event, credentials).await.status().as_u16(), 401);
            assert!(hook.suppressed_addresses().await.is_empty());
        }
        assert!(
            hook.auth
                .pg
                .query("SELECT id FROM zeroship.audit_events", &[])
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            hook.post(&event, Some(CREDENTIALS)).await.status().as_u16(),
            200
        );
        assert_eq!(
            hook.suppressed_addresses().await,
            ["recipient@example.test"]
        );
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
            let hook = Webhook::start(database, user, password).await;
            assert_eq!(
                hook.post(
                    &bounce("recipient@example.test", "HardBounce"),
                    Some(CREDENTIALS)
                )
                .await
                .status()
                .as_u16(),
                401
            );
            assert!(hook.suppressed_addresses().await.is_empty());
        }
    })
    .await;
}

#[ntex::test]
async fn database_failure_is_retryable_and_never_audited_as_success() {
    Database::run(async |database| {
        let hook = Webhook::start(database, Some("webhook-user"), Some("webhook-password")).await;
        let admin = database.connect().await;
        admin.batch_execute(
            "ALTER TABLE zeroship.email_suppressions ADD CONSTRAINT refuse_delivery_event \
             CHECK (email <> 'recipient@example.test'::citext)",
        ).await.unwrap();
        let event = bounce("recipient@example.test", "HardBounce");
        assert_eq!(hook.post(&event, Some(CREDENTIALS)).await.status().as_u16(), 500);
        assert!(hook.suppressed_addresses().await.is_empty());
        assert!(hook.auth.pg.query("SELECT id FROM zeroship.audit_events", &[]).await.unwrap().is_empty());

        admin.batch_execute("ALTER TABLE zeroship.email_suppressions DROP CONSTRAINT refuse_delivery_event").await.unwrap();
        assert_eq!(hook.post(&event, Some(CREDENTIALS)).await.status().as_u16(), 200);
        assert_eq!(hook.suppressed_addresses().await, ["recipient@example.test"]);
        let audit = hook.auth.pg.query_one(
            "SELECT outcome, detail FROM zeroship.audit_events WHERE event_type = 'mailer_bounce'", &[],
        ).await.unwrap();
        assert_eq!(audit.get::<_, String>("outcome"), "success");
        assert_eq!(audit.get::<_, Value>("detail"), json!({
            "email_domain": "example.test", "bounce_type": "HardBounce"
        }));
    }).await;
}
