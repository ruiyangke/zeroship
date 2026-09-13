use super::fixtures::{Alias, RELAY_DOMAIN};
use crate::common::{auth_server::AuthServer, database::Database};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use compio_postgres::Client;
use serde_json::{Value, json};
use std::{
    io::Write as _,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use zeroship_mailer::{Email, MailerError, MessageId};

pub struct RelayServer {
    pub auth: AuthServer,
    pub alias: Alias,
    pub mailer: Arc<DeliveryMailbox>,
    _credentials: tempfile::NamedTempFile,
}

impl RelayServer {
    pub async fn start(database: &Database) -> Self {
        let admin = database.connect().await;
        let alias = Alias::seed(&admin).await;
        let mut credentials = tempfile::NamedTempFile::new().unwrap();
        credentials.write_all(b"webhook-password").unwrap();
        let mailer = Arc::new(DeliveryMailbox::default());
        let auth = AuthServer::with_relay_mailer(
            database,
            mailer.clone(),
            &[
                "--relay-domain",
                RELAY_DOMAIN,
                "--relay-inbound-user",
                "webhook-user",
                "--relay-inbound-password-file",
                credentials.path().to_str().unwrap(),
            ],
        )
        .await;
        Self {
            auth,
            alias,
            mailer,
            _credentials: credentials,
        }
    }

    pub fn message(&self, message_id: &str) -> Value {
        let (token, domain) = self.alias.email.split_once('@').unwrap();
        json!({
            "FromFull": { "Email": "sender@shop.example.test", "Name": "Shop" },
            "OriginalRecipient": format!("{}+receipt@{}", token.to_uppercase(), domain.to_uppercase()),
            "Subject": "Your receipt",
            "TextBody": "Order received.",
            "HtmlBody": "<p>Order received.</p>",
            "MessageID": message_id,
            "Headers": [
                { "Name": "Authentication-Results", "Value": "mx.example.test; dmarc=pass" },
                { "Name": "X-Spam-Status", "Value": "No" },
                { "Name": "X-Private-Sender-Header", "Value": "discard this" }
            ]
        })
    }

    pub async fn post(&self, message: &Value) -> cyper::Response {
        self.post_raw(
            serde_json::to_vec(message).unwrap(),
            Some("webhook-user:webhook-password"),
        )
        .await
    }

    pub async fn post_raw(&self, body: Vec<u8>, credentials: Option<&str>) -> cyper::Response {
        let request = self
            .auth
            .http
            .request(
                http::Method::POST,
                format!("{}/webhooks/relay-inbound", self.auth.auth_base),
            )
            .unwrap()
            .header("content-type", "application/json")
            .unwrap()
            .body(body);
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

    pub async fn audit_outcomes(&self, event_type: &str) -> Vec<String> {
        self.auth.pg.query(
            "SELECT outcome FROM zeroship.audit_events WHERE event_type = $1 ORDER BY occurred_at, id",
            &[&event_type],
        ).await.unwrap().iter().map(|row| row.get(0)).collect()
    }
}

/// A controllable delivery boundary; the real handler still performs its gates
/// and builds the forwarded message. Suppression follows the mailer contract.
#[derive(Debug, Default)]
pub struct DeliveryMailbox {
    unavailable: AtomicBool,
    attempts: Mutex<Vec<Email>>,
    delivered: Mutex<Vec<Email>>,
}

impl DeliveryMailbox {
    pub fn set_unavailable(&self, unavailable: bool) {
        self.unavailable.store(unavailable, Ordering::SeqCst);
    }

    pub fn attempts(&self) -> Vec<Email> {
        self.attempts.lock().unwrap().clone()
    }

    pub fn delivered(&self) -> Vec<Email> {
        self.delivered.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl zeroship_mailer::Mailer for DeliveryMailbox {
    async fn send(&self, db: &Client, email: Email) -> Result<MessageId, MailerError> {
        self.attempts.lock().unwrap().push(email.clone());
        zeroship_mailer::check_suppression(db, &email.to.email).await?;
        if self.unavailable.load(Ordering::SeqCst) {
            return Err(MailerError::Transport(
                "fixture delivery unavailable".to_owned(),
            ));
        }
        self.delivered.lock().unwrap().push(email);
        Ok(MessageId("fixture-delivery".to_owned()))
    }
}
