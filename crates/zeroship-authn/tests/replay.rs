//! Replay decisions and migrated privileges across independent database sessions.

#![allow(
    clippy::future_not_send,
    reason = "fixtures belong to their compio runtime"
)]

mod fixtures;

use crate::common::database::Database;
use compio_postgres::Client;
use fixtures::{race, Assertion, ReadThenWriteStore};
use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};
use zeroship_authn::service_replay::{PostgresReplayStore, SharedClientReplayStore};
use zeroship_core::service_assertion::{ReplayClaim, ReplayStore};
use zeroship_core::service_identity::AuthError;
use zeroship_core::UserId;

/// A key token no other case in the shared server holds.
fn scope() -> String {
    UserId::mint().as_str().to_owned()
}

async fn expiry_after_database_now(client: &Client) -> SystemTime {
    let seconds: f64 = client
        .query_one(
            "SELECT extract(epoch FROM clock_timestamp())::double precision",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    SystemTime::UNIX_EPOCH + Duration::from_secs_f64(seconds) + Duration::from_secs(86_400)
}

async fn expire(client: &Client, key: &str) {
    assert_eq!(client.execute(
        "UPDATE service_authn.service_assertion_replay SET expires_at = NOW() - INTERVAL '1 day' WHERE replay_key = $1",
        &[&key],
    ).await.unwrap(), 1);
}

/// The replay keys beginning with `prefix`, in order.
async fn keys(client: &Client, prefix: &str) -> Vec<String> {
    client
        .query(
            "SELECT replay_key FROM service_authn.service_assertion_replay \
             WHERE substring(replay_key FROM 1 FOR length($1)) = $1 \
             ORDER BY replay_key",
            &[&prefix],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get(0))
        .collect()
}

#[compio::test]
async fn concurrent_replicas_admit_one_assertion_and_reject_its_replay() {
    Database::run(async |database| {
        let request = Assertion::new();
        let outcomes = race(database, &request, |client| {
            Arc::new(PostgresReplayStore::new(client))
        })
        .await;
        match outcomes {
            [Ok(_), Err(AuthError::CredentialRejected)]
            | [Err(AuthError::CredentialRejected), Ok(_)] => {}
            other => panic!("expected acceptance and replay refusal, got {other:?}"),
        }
        let key = request.replay_key();
        assert_eq!(keys(&database.connect().await, &key).await, [key]);
    })
    .await;
}

#[compio::test]
async fn a_read_then_write_store_admits_the_replay_under_the_same_interleaving() {
    Database::run(async |database| {
        let request = Assertion::new();
        let outcomes =
            race(database, &request, |client| Arc::new(ReadThenWriteStore { client })).await;
        for outcome in outcomes {
            outcome.expect("the non-atomic control must incorrectly admit the replay");
        }
        let key = request.replay_key();
        assert_eq!(keys(&database.connect().await, &key).await, [key]);
    })
    .await;
}

#[compio::test]
async fn a_claim_is_single_use_until_expiry_and_sweeping_preserves_live_claims() {
    Database::run(async |database| {
        let prefix = scope();
        let live = format!("{prefix}:live");
        let reclaimed = format!("{prefix}:reclaimed");
        let swept = format!("{prefix}:swept");
        let probe = database.connect().await;
        let store = PostgresReplayStore::new(database.connect_as("zeroship_control").await);
        let future = expiry_after_database_now(&probe).await;
        assert_eq!(
            store.claim(&live, future).await.unwrap(),
            ReplayClaim::Accepted
        );
        assert_eq!(
            store.claim(&live, future).await.unwrap(),
            ReplayClaim::AlreadyUsed
        );
        assert_eq!(
            store.claim(&reclaimed, future).await.unwrap(),
            ReplayClaim::Accepted
        );
        expire(&probe, &reclaimed).await;
        assert_eq!(
            store.claim(&reclaimed, future).await.unwrap(),
            ReplayClaim::Accepted
        );
        assert_eq!(
            store.claim(&reclaimed, future).await.unwrap(),
            ReplayClaim::AlreadyUsed
        );
        assert_eq!(
            store.claim(&swept, future).await.unwrap(),
            ReplayClaim::Accepted
        );
        expire(&probe, &swept).await;
        assert_eq!(keys(&probe, &swept).await, std::slice::from_ref(&swept));
        store.purge_expired().await.unwrap();
        assert!(keys(&probe, &swept).await.is_empty());
        assert_eq!(keys(&probe, &prefix).await, [live, reclaimed]);
        store.purge_expired().await.unwrap();
        assert_eq!(
            keys(&probe, &prefix).await.len(),
            2,
            "a repeated sweep must preserve this case's live claims"
        );
    })
    .await;
}

#[compio::test]
async fn a_shared_client_claim_is_visible_to_an_independent_replica() {
    Database::run(async |database| {
        let request = Assertion::new();
        let first = request.verifier(Arc::new(SharedClientReplayStore::new(Arc::new(
            database.connect_as("zeroship_control").await,
        ))));
        let second = request.verifier(Arc::new(PostgresReplayStore::new(
            database.connect_as("zeroship_control").await,
        )));
        request.verify(&first).await.unwrap();
        assert_eq!(
            request.verify(&second).await,
            Err(AuthError::CredentialRejected)
        );
        let key = request.replay_key();
        assert_eq!(keys(&database.connect().await, &key).await, [key]);
    })
    .await;
}

#[compio::test]
async fn platform_service_roles_can_claim_reclaim_and_sweep_but_cannot_create_tables() {
    Database::run(async |database| {
        let prefix = scope();
        let admin = database.connect().await;
        let future = expiry_after_database_now(&admin).await;
        // These callers are the contract under test, independent of the
        // migration's current grants and of the catalog's current contents.
        for role in [
            "zeroship_auth",
            "zeroship_control",
            "zeroship_gateway",
            "zeroship_worker",
        ] {
            let key = format!("{prefix}:{role}");
            let client = database.connect_as(role).await;
            assert_eq!(
                client
                    .query_one("SELECT current_user::text", &[])
                    .await
                    .unwrap()
                    .get::<_, String>(0),
                role
            );
            let denied = client
                .batch_execute("CREATE TABLE service_authn.fixture_unpermitted (id integer)")
                .await
                .unwrap_err();
            assert_eq!(
                denied.as_db_error().unwrap().code().code(),
                "42501",
                "{role}"
            );
            let store = PostgresReplayStore::new(client);
            assert_eq!(
                store.claim(&key, future).await.unwrap(),
                ReplayClaim::Accepted,
                "{role}"
            );
            assert_eq!(
                store.claim(&key, future).await.unwrap(),
                ReplayClaim::AlreadyUsed,
                "{role}"
            );
            expire(&admin, &key).await;
            assert_eq!(
                store.claim(&key, future).await.unwrap(),
                ReplayClaim::Accepted,
                "{role}"
            );
            expire(&admin, &key).await;
            store.purge_expired().await.unwrap();
            assert!(keys(&admin, &key).await.is_empty(), "{role}");
        }
    })
    .await;
}

#[compio::test]
async fn an_app_role_cannot_claim_or_authenticate_a_service_assertion() {
    Database::run(async |database| {
        let prefix = scope();
        let key = format!("{prefix}:denied");
        let admin = database.connect().await;
        let store = PostgresReplayStore::new(database.connect_as("zeroship_app").await);
        let error = store
            .claim(&key, expiry_after_database_now(&admin).await)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("permission denied"));
        assert!(error.to_string().contains("42501"));
        let request = Assertion::new();
        let verifier = request.verifier(Arc::new(store));
        assert_eq!(
            request.verify(&verifier).await,
            Err(AuthError::StoreUnavailable)
        );
        assert!(keys(&admin, &prefix).await.is_empty());
    })
    .await;
}

#[compio::test]
async fn missing_table_privileges_fail_closed_and_do_not_burn_a_retryable_assertion() {
    Database::run(async |database| {
        let prefix = scope();
        let known_live = format!("{prefix}:known-live");
        let denied = format!("{prefix}:denied");
        let admin = database.connect().await;
        // A role private to this case holds the table grant the case revokes, so
        // revoking it cannot deny the claims of every other case in the shared
        // server.
        let role = format!("authn_replay_{}", UserId::mint().as_str());
        admin
            .batch_execute(&format!(
                "CREATE ROLE \"{role}\" LOGIN PASSWORD '{role}'"
            ))
            .await
            .unwrap();
        admin
            .batch_execute(&format!("GRANT USAGE ON SCHEMA service_authn TO \"{role}\""))
            .await
            .unwrap();
        admin
            .batch_execute(&format!(
                "GRANT SELECT, INSERT, UPDATE, DELETE ON service_authn.service_assertion_replay TO \"{role}\""
            ))
            .await
            .unwrap();
        let store = PostgresReplayStore::new(database.connect_as(&role).await);
        let request = Assertion::new();
        let future = expiry_after_database_now(&admin).await;
        assert_eq!(
            store.claim(&known_live, future).await.unwrap(),
            ReplayClaim::Accepted
        );
        admin
            .batch_execute(&format!(
                "REVOKE SELECT ON service_authn.service_assertion_replay FROM \"{role}\""
            ))
            .await
            .unwrap();
        let error = store.claim(&denied, future).await.unwrap_err();
        assert!(error
            .to_string()
            .contains("permission denied for table service_assertion_replay"));
        assert!(error.to_string().contains("42501"));
        let verifier = request.verifier(Arc::new(store));
        assert_eq!(
            request.verify(&verifier).await,
            Err(AuthError::StoreUnavailable)
        );
        assert_eq!(keys(&admin, &prefix).await, [known_live]);
        admin
            .batch_execute(&format!(
                "GRANT SELECT ON service_authn.service_assertion_replay TO \"{role}\""
            ))
            .await
            .unwrap();
        let key = request.replay_key();
        request.verify(&verifier).await.unwrap();
        assert_eq!(
            request.verify(&verifier).await,
            Err(AuthError::CredentialRejected)
        );
        assert_eq!(keys(&admin, &key).await, [key]);
    })
    .await;
}
