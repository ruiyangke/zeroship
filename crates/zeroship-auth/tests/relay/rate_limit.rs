use super::http::RelayServer;
use crate::common::database::Database;
use serde_json::{Value, json};
use zeroship_auth::store::relay;

/// Consume the real HTTP budget. Freeze only the buckets observed after the
/// successful delivery so wall-clock refill cannot change the refusal boundary.
async fn exhaust_budget(server: &RelayServer) -> (Value, Vec<String>) {
    assert_eq!(
        server
            .post(&server.message("warmup"))
            .await
            .status()
            .as_u16(),
        200
    );
    let keys: Vec<String> = server
        .auth
        .pg
        .query(
            "SELECT bucket_key FROM zeroship.rate_limits WHERE tokens > 0",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert!(
        !keys.is_empty(),
        "successful forwarding must create rate-limit state"
    );
    for sequence in 0..256 {
        server
            .auth
            .pg
            .execute(
                "UPDATE zeroship.rate_limits SET updated_at = NOW() + INTERVAL '1 day' \
             WHERE bucket_key = ANY($1)",
                &[&keys],
            )
            .await
            .unwrap();
        let message = server.message(&format!("burst-{sequence}"));
        let delivered_before = server.mailer.delivered().len();
        let response = server.post(&message).await;
        match response.status().as_u16() {
            200 => assert_eq!(server.mailer.delivered().len(), delivered_before + 1),
            503 => {
                assert!(
                    response
                        .headers()
                        .get("retry-after")
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .parse::<u32>()
                        .unwrap()
                        > 0
                );
                assert_eq!(server.mailer.delivered().len(), delivered_before);
                return (message, keys);
            }
            status => panic!("unexpected relay status {status}"),
        }
    }
    panic!("relay accepted the entire bounded flood without throttling");
}

#[ntex::test]
async fn rate_limit_refusal_remains_retryable_after_refill() {
    Database::run(async |database| {
        let server = RelayServer::start(database).await;
        let (message, keys) = exhaust_budget(&server).await;
        let id = message["MessageID"].as_str().unwrap();
        assert!(!relay::already_seen(&server.auth.pg, id).await.unwrap());
        let attempts_before = server.mailer.attempts().len();
        server
            .auth
            .pg
            .execute(
                "UPDATE zeroship.rate_limits SET updated_at = NOW() - INTERVAL '1 year' \
             WHERE bucket_key = ANY($1)",
                &[&keys],
            )
            .await
            .unwrap();

        assert_eq!(server.post(&message).await.status().as_u16(), 200);
        assert_eq!(server.mailer.attempts().len(), attempts_before + 1);
        assert_eq!(server.mailer.delivered().len(), attempts_before + 1);
        assert!(relay::already_seen(&server.auth.pg, id).await.unwrap());
        assert_eq!(server.post(&message).await.status().as_u16(), 200);
        assert_eq!(server.mailer.attempts().len(), attempts_before + 1);
        assert!(server.audit_outcomes("relay_auto_revoke").await.is_empty());
    })
    .await;
}

#[ntex::test]
async fn sustained_abuse_stops_forwarding_and_audits_only_local_revocation() {
    Database::run(async |database| {
        let server = RelayServer::start(database).await;
        let grant = server.alias.granted_scopes(&server.auth.pg).await;
        let (message, _) = exhaust_budget(&server).await;
        let attempts_before = server.mailer.attempts().len();
        let mut revoked = false;
        for _ in 0..64 {
            match server.post(&message).await.status().as_u16() {
                503 => {},
                200 => { revoked = true; break; },
                status => panic!("unexpected abuse response {status}"),
            }
        }
        assert!(revoked, "sustained abuse must reach terminal local revocation");
        assert_eq!(server.mailer.attempts().len(), attempts_before);
        assert!(relay::resolve_active_alias(&server.auth.pg, &server.alias.email).await.unwrap().is_none());
        assert_eq!(server.alias.granted_scopes(&server.auth.pg).await, grant);
        let audit = server.auth.pg.query_one(
            "SELECT outcome, detail FROM zeroship.audit_events WHERE event_type = 'relay_auto_revoke'", &[],
        ).await.unwrap();
        assert_eq!(audit.get::<_, String>("outcome"), "local_revoked_cross_service_pending");
        assert_eq!(audit.get::<_, Value>("detail"), json!({
            "local_alias_disabled": true,
            "already_disabled": false,
            "cross_service_grant_revoke": "not_implemented",
        }));
        assert_eq!(server.post(&message).await.status().as_u16(), 200);
        assert_eq!(server.mailer.attempts().len(), attempts_before);

        let later_message = server.message("after-local-revocation");
        assert_eq!(server.post(&later_message).await.status().as_u16(), 200);
        let deliveries = server.mailer.delivered();
        assert_eq!(deliveries.len(), attempts_before + 1);
        let bounce = deliveries.last().unwrap();
        assert_eq!(bounce.to.email, later_message["FromFull"]["Email"].as_str().unwrap());
        assert_eq!(bounce.tags, ["relay-bounce"]);
        assert!(!bounce.text.contains(&server.alias.inbox));
        assert_eq!(server.post(&later_message).await.status().as_u16(), 200);
        assert_eq!(server.mailer.attempts().len(), attempts_before + 1);
    }).await;
}
