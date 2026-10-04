use crate::support::database::Database;
use crate::support::smtp::{Address, SmtpSink};
use crate::support::{message, recipient};
use zeroship_mailer::inbound::{InboundHeader, InboundMessage, Mailbox};
use zeroship_mailer::{forward, suppressions, Mailer, MailerError};

#[compio::test]
async fn smtp_refusal_is_a_transport_error_and_the_mailer_can_send_again() {
    Database::run(async |database| {
        SmtpSink::run(async |sink| {
            let client = database.connect_as("zeroship_auth").await;
            let mailer = sink.mailer();
            let refused = message(&format!(
                "refused-{}@refused.test",
                uuid::Uuid::new_v4().simple()
            ));
            let error = mailer.send(&client, refused).await.unwrap_err();
            assert!(
                matches!(error, MailerError::Transport(_)),
                "wrong SMTP error: {error:?}"
            );
            assert_eq!(sink.inbox().await.total, 0);
            mailer
                .send(&client, message(&recipient("refusal-recovery")))
                .await
                .unwrap();
            assert_eq!(
                sink.delivered().await.message.subject,
                "Confirm your address"
            );
        })
        .await;
    })
    .await;
}

#[compio::test]
async fn transactional_mail_arrives_with_its_envelope_and_mime_bodies() {
    Database::run(async |database| {
        SmtpSink::run(async |sink| {
            let client = database.connect_as("zeroship_auth").await;
            let to = recipient("transactional");
            let email = message(&to);
            sink.mailer().send(&client, email.clone()).await.unwrap();
            let delivered = sink.delivered().await;
            assert_eq!(
                delivered.message.from,
                Address {
                    name: "zeroship".into(),
                    address: "auth@zeroship.test".into()
                }
            );
            assert_eq!(
                delivered.message.to,
                vec![Address {
                    name: "Reader".into(),
                    address: to.clone()
                }]
            );
            assert!(delivered.message.reply_to.is_empty());
            assert_eq!(delivered.message.return_path, "auth@zeroship.test");
            assert_eq!(delivered.message.subject, email.subject);
            assert_eq!(delivered.message.text, email.text);
            assert_eq!(delivered.message.html, email.html.unwrap());
            assert_eq!(delivered.header("Received").len(), 1);
            assert!(delivered.header("Received")[0].contains(&format!("for <{to}>")));
        })
        .await;
    })
    .await;
}

#[compio::test]
async fn relay_delivers_to_the_real_inbox_with_alias_headers_and_bounce_sender() {
    Database::run(async |database| {
        SmtpSink::run(async |sink| {
            let client = database.connect_as("zeroship_auth").await;
            let tag = uuid::Uuid::new_v4().simple().to_string();
            let real = format!("real.user-{tag}@personal.test");
            let alias = format!("receipt-{tag}@relay.zeroship.test");
            let inbound = InboundMessage {
                from_full: Mailbox {
                    email: "newsletter@shop.test".into(),
                    name: Some("Shop".into()),
                },
                to_full: vec![],
                original_recipient: alias.clone(),
                subject: "Your receipt".into(),
                text_body: "Thanks for your order.".into(),
                html_body: None,
                stripped_text_reply: None,
                headers: [
                    "Delivered-To",
                    "X-Original-To",
                    "Return-Path",
                    "Received",
                    "Sender",
                ]
                .into_iter()
                .map(|name| InboundHeader {
                    name: name.into(),
                    value: real.clone(),
                })
                .collect(),
                message_id: format!("inbound-receipt-{tag}"),
            };
            let email =
                forward::build_forward(&inbound, &alias, &real, "Shop", "relay.zeroship.test", 1);
            sink.mailer().send(&client, email).await.unwrap();
            let delivered = sink.delivered().await;
            assert_eq!(
                delivered.message.from,
                Address {
                    name: "Shop via relay".into(),
                    address: alias.clone()
                }
            );
            assert_eq!(
                delivered.message.to,
                vec![Address {
                    name: "Shop via relay".into(),
                    address: alias.clone()
                }]
            );
            assert_eq!(
                delivered.message.reply_to,
                vec![Address {
                    name: String::new(),
                    address: alias.clone()
                }]
            );
            // Mailpit records MAIL FROM in Return-Path and RCPT TO in Received.
            assert_eq!(
                delivered.message.return_path,
                format!("bounce+receipt-{tag}@relay.zeroship.test")
            );
            assert_eq!(delivered.header("Received").len(), 1);
            assert!(delivered.header("Received")[0].contains(&format!("for <{real}>")));
            assert_eq!(delivered.header("X-ZS-Relay"), &["1"]);
            for name in ["Delivered-To", "X-Original-To", "Sender"] {
                assert!(
                    delivered.header(name).is_empty(),
                    "inbound {name} survived forwarding"
                );
            }
            // Mailpit also synthesizes Bcc for envelope-only recipients. These
            // capture headers describe SMTP delivery, not the submitted MIME.
            for (name, values) in &delivered.headers {
                if name.eq_ignore_ascii_case("Received") || name.eq_ignore_ascii_case("Bcc") {
                    continue;
                }
                assert!(
                    values.iter().all(|value| !value.contains(&real)),
                    "real inbox leaked in {name}: {values:?}"
                );
            }
            assert_eq!(delivered.message.subject, inbound.subject);
            // SMTP terminates the final line of the text-only message.
            assert_eq!(delivered.message.text, format!("{}\r\n", inbound.text_body));
            assert!(delivered.message.html.is_empty());
        })
        .await;
    })
    .await;
}

#[compio::test]
async fn a_suppressed_envelope_recipient_is_not_delivered_even_with_an_allowed_header_to() {
    Database::run(async |database| {
        SmtpSink::run(async |sink| {
            let client = database.connect_as("zeroship_auth").await;
            let mailer = sink.mailer();
            let to = recipient("suppressed-envelope");
            let mut email = message(&to);
            email.header_to = Some(zeroship_mailer::Address {
                email: "alias@relay.zeroship.test".into(),
                name: None,
            });
            email.envelope_from = Some("bounce@relay.zeroship.test".into());
            suppressions::add(&client, &to.to_uppercase(), "complaint", None)
                .await
                .unwrap();
            assert!(matches!(mailer.send(&client, email.clone()).await,
                Err(MailerError::Suppressed(address)) if address == to));
            assert_eq!(sink.inbox().await.total, 0, "suppressed mail reached SMTP");

            let other = recipient("suppressed-other");
            email.to.email = other.clone();
            mailer.send(&client, email).await.unwrap();
            let delivered = sink.delivered().await;
            assert_eq!(delivered.header("Received").len(), 1);
            assert!(delivered.header("Received")[0].contains(&format!("for <{other}>")));
        })
        .await;
    })
    .await;
}

#[compio::test]
async fn failed_suppression_lookup_prevents_delivery_and_a_retry_after_repair_succeeds() {
    // Platform-global: the case renames the shared suppression table, so every
    // reader's lookup fails while it holds.
    Database::run_fresh(async |database| {
        SmtpSink::run(async |sink| {
            let admin = database.connect().await;
            let client = database.connect_as("zeroship_auth").await;
            let mailer = sink.mailer();
            admin
                .batch_execute(
                    "ALTER TABLE zeroship.email_suppressions RENAME TO unavailable_suppressions",
                )
                .await
                .unwrap();
            assert!(matches!(
                mailer
                    .send(&client, message(&recipient("failed-lookup")))
                    .await,
                Err(MailerError::Transport(_))
            ));
            assert_eq!(
                sink.inbox().await.total,
                0,
                "mail escaped a failed suppression lookup"
            );
            admin
                .batch_execute(
                    "ALTER TABLE zeroship.unavailable_suppressions RENAME TO email_suppressions",
                )
                .await
                .unwrap();
            mailer
                .send(&client, message(&recipient("lookup-repair")))
                .await
                .unwrap();
            assert_eq!(
                sink.delivered().await.message.subject,
                "Confirm your address"
            );
        })
        .await;
    })
    .await;
}
