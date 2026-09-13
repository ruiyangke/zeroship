//! A private Mailpit inbox accepting personal.test recipients on mapped ports.

#![allow(
    clippy::future_not_send,
    reason = "fixtures belong to their compio runtime"
)]

use futures::FutureExt;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::time::Duration;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::{runners::SyncRunner, Container, GenericImage, ImageExt};
use zeroship_mailer::{SmtpConfig, SmtpMailer, SmtpTls};

pub struct SmtpSink {
    container: Container<GenericImage>,
    api: url::Url,
    client: cyper::Client,
}

impl SmtpSink {
    pub async fn run(test: impl AsyncFnOnce(&Self)) {
        let container = GenericImage::new(
            "axllent/mailpit",
            "latest@sha256:98b916bd3c8d61f7633a52d3ea2f58d00620cb01ca57ab59edde68c347a95365",
        )
        .with_exposed_port(1025.tcp())
        .with_exposed_port(8025.tcp())
        .with_wait_for(WaitFor::message_on_stdout("accessible via"))
        .with_env_var("MP_SMTP_DISABLE_RDNS", "true")
        .with_env_var("MP_SMTP_ALLOWED_RECIPIENTS", r"^[^@]+@personal\.test$")
        .with_env_var("MP_DISABLE_VERSION_CHECK", "true")
        .with_startup_timeout(Duration::from_secs(60))
        .start()
        .expect("mailer tests require Docker and Mailpit");
        let mut api = url::Url::parse("http://localhost/").unwrap();
        api.set_host(Some(&container.get_host().expect("SMTP host").to_string()))
            .unwrap();
        api.set_port(Some(
            container.get_host_port_ipv4(8025).expect("SMTP API port"),
        ))
        .unwrap();
        let sink = Self {
            container,
            api,
            client: cyper::Client::new(),
        };
        let outcome = AssertUnwindSafe(async {
            compio::time::timeout(Duration::from_secs(45), Box::pin(test(&sink)))
                .await
                .expect("SMTP case timed out");
        })
        .catch_unwind()
        .await;
        sink.container.rm().expect("remove the case's SMTP server");
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    pub fn mailer(&self) -> SmtpMailer {
        SmtpMailer::new(&SmtpConfig {
            host: self.container.get_host().expect("SMTP host").to_string(),
            port: self.container.get_host_port_ipv4(1025).expect("SMTP port"),
            username: None,
            password: None,
            tls: SmtpTls::Plaintext,
        })
        .expect("build plaintext SMTP mailer")
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> T {
        compio::time::timeout(Duration::from_secs(10), async {
            let response = self
                .client
                .get(self.api.join(path).unwrap())
                .expect("build Mailpit request")
                .send()
                .await
                .expect("read Mailpit API");
            assert!(
                response.status().is_success(),
                "Mailpit API: {}",
                response.status()
            );
            response.json().await.expect("decode Mailpit response")
        })
        .await
        .expect("Mailpit API timed out")
    }

    pub async fn inbox(&self) -> Inbox {
        self.get("api/v1/messages").await
    }

    pub async fn delivered(&self) -> Delivered {
        // SMTP acknowledges after storage; a successful send is a read barrier.
        let inbox = self.inbox().await;
        assert_eq!(inbox.total, 1, "unexpected inbox: {inbox:?}");
        assert_eq!(inbox.messages.len(), 1, "unexpected inbox: {inbox:?}");
        let id = &inbox.messages[0].id;
        let message = self.get(&format!("api/v1/message/{id}")).await;
        let headers = self.get(&format!("api/v1/message/{id}/headers")).await;
        Delivered { message, headers }
    }
}

#[derive(Debug, Deserialize)]
pub struct Inbox {
    pub total: usize,
    pub messages: Vec<MessageSummary>,
}

#[derive(Debug, Deserialize)]
pub struct MessageSummary {
    #[serde(rename = "ID")]
    pub id: String,
}

pub struct Delivered {
    pub message: Message,
    pub headers: BTreeMap<String, Vec<String>>,
}

impl Delivered {
    pub fn header(&self, name: &str) -> &[String] {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, values)| values.as_slice())
            .unwrap_or_default()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Message {
    pub from: Address,
    pub to: Vec<Address>,
    pub reply_to: Vec<Address>,
    pub return_path: String,
    pub subject: String,
    pub text: String,
    #[serde(rename = "HTML")]
    pub html: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct Address {
    pub name: String,
    pub address: String,
}

#[compio::test]
async fn assertion_failure_removes_the_owned_smtp_server() {
    use super::database::container_ids;
    use std::cell::RefCell;

    let failed_id = RefCell::new(String::new());
    let failed = AssertUnwindSafe(SmtpSink::run(async |sink| {
        *failed_id.borrow_mut() = sink.container.id().to_owned();
        assert_eq!(sink.inbox().await.total, 0);
        panic!("intentional SMTP fixture failure");
    }))
    .catch_unwind()
    .await
    .expect_err("case must propagate its assertion failure");
    assert_eq!(
        failed.downcast_ref::<&str>(),
        Some(&"intentional SMTP fixture failure")
    );
    assert!(
        !container_ids().contains(&*failed_id.borrow()),
        "failed case leaked its SMTP server"
    );

    let successful_id = RefCell::new(String::new());
    SmtpSink::run(async |sink| {
        *successful_id.borrow_mut() = sink.container.id().to_owned();
        assert_ne!(*successful_id.borrow(), *failed_id.borrow());
        assert_eq!(sink.inbox().await.total, 0);
    })
    .await;
    assert!(
        !container_ids().contains(&*successful_id.borrow()),
        "successful case leaked its SMTP server"
    );
}
