use super::http::RelayServer;
use crate::common::database::Database;
use zeroship_auth::store::relay;

#[ntex::test]
async fn rejected_credentials_cannot_consume_budget_or_deliver_mail() {
    Database::run(async |database| {
        let server = RelayServer::start(database).await;
        let message = server.message("credential-refusal");
        for (body, credentials) in [
            (b"{".to_vec(), None),
            (
                serde_json::to_vec(&message).unwrap(),
                Some("webhook-user:wrong-password"),
            ),
            (
                serde_json::to_vec(&message).unwrap(),
                Some("wrong-user:webhook-password"),
            ),
        ] {
            assert_eq!(
                server.post_raw(body, credentials).await.status().as_u16(),
                401
            );
            assert!(server.mailer.attempts().is_empty());
            assert!(
                server
                    .auth
                    .pg
                    .query("SELECT bucket_key FROM zeroship.rate_limits", &[])
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        assert_eq!(server.post(&message).await.status().as_u16(), 200);
        assert_eq!(server.mailer.delivered().len(), 1);
    })
    .await;
}

#[ntex::test]
async fn transport_failure_retries_the_same_message_and_deduplicates_after_delivery() {
    Database::run(async |database| {
        let server = RelayServer::start(database).await;
        let message = server.message("retryable-delivery");
        server.mailer.set_unavailable(true);
        assert_eq!(server.post(&message).await.status().as_u16(), 503);
        assert_eq!(server.mailer.attempts().len(), 1);
        assert!(server.mailer.delivered().is_empty());
        assert!(server.audit_outcomes("relay_forward").await.is_empty());
        assert!(
            !relay::already_seen(&server.auth.pg, "retryable-delivery")
                .await
                .unwrap()
        );

        server.mailer.set_unavailable(false);
        assert_eq!(server.post(&message).await.status().as_u16(), 200);
        let delivered = server.mailer.delivered();
        assert_eq!(delivered.len(), 1);
        let forward = &delivered[0];
        assert_eq!(forward.to.email, server.alias.inbox);
        assert_eq!(
            forward.header_to.as_ref().unwrap().email,
            server.alias.email
        );
        assert_eq!(forward.from.email, server.alias.email);
        assert_eq!(forward.reply_to.as_ref().unwrap().email, server.alias.email);
        assert_eq!(forward.subject, message["Subject"].as_str().unwrap());
        assert_eq!(forward.text, message["TextBody"].as_str().unwrap());
        assert_eq!(forward.html.as_deref(), message["HtmlBody"].as_str());
        assert_eq!(forward.headers, [("X-ZS-Relay".to_owned(), "1".to_owned())]);
        let (token, domain) = server.alias.email.split_once('@').unwrap();
        assert_eq!(
            forward.envelope_from.as_deref(),
            Some(format!("bounce+{token}@{domain}").as_str())
        );
        assert_eq!(server.audit_outcomes("relay_forward").await, ["success"]);
        assert!(
            relay::already_seen(&server.auth.pg, "retryable-delivery")
                .await
                .unwrap()
        );

        assert_eq!(server.post(&message).await.status().as_u16(), 200);
        assert_eq!(server.mailer.attempts().len(), 2);
        assert_eq!(server.mailer.delivered().len(), 1);
        assert_eq!(server.audit_outcomes("relay_forward").await, ["success"]);

        assert_eq!(
            server
                .post(&server.message("independent-delivery"))
                .await
                .status()
                .as_u16(),
            200
        );
        assert_eq!(server.mailer.delivered().len(), 2);
    })
    .await;
}
