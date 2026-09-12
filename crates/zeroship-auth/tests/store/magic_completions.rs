//! Completion reservations and retry ownership in databases owned by each case.

use crate::common::database::Database;
use compio_postgres::{Client, GenericClient};
use zeroship_auth::store::magic_completions::{self, ConsumeError};

const NONCE: &str = "completion-nonce";
const CODE: &str = "123456";
const EMAIL: &str = "magic@example.test";
const TARGET: &str = "completion-handoff";

async fn create(client: &Client) {
    magic_completions::create(client, NONCE, CODE, EMAIL, TARGET, 300)
        .await
        .expect("create completion through the production store");
}

#[derive(Debug, PartialEq, Eq)]
struct State {
    attempts: i16,
    pending: Option<chrono::DateTime<chrono::Utc>>,
    consumed: bool,
}

#[allow(
    clippy::future_not_send,
    reason = "the fixture remains on its owning runtime"
)]
async fn state(client: &(impl GenericClient + ?Sized)) -> State {
    let row = client
        .query_one(
            "SELECT attempts, consumed_pending_at, consumed_at IS NOT NULL AS consumed \
         FROM zeroship.magic_completions WHERE csrf_nonce = $1",
            &[&NONCE],
        )
        .await
        .unwrap();
    State {
        attempts: row.get(0),
        pending: row.get(1),
        consumed: row.get(2),
    }
}

#[compio::test]
async fn wrong_code_does_not_mutate_a_concurrent_reservation() {
    Database::run(async |database| {
        let mut winner = database.connect_as_auth().await;
        create(&winner).await;
        let transaction = winner.transaction().await.unwrap();
        let reserved = magic_completions::consume_pending(&transaction, NONCE, CODE)
            .await
            .unwrap();
        assert_eq!(reserved.email, EMAIL);
        assert_eq!(reserved.target, TARGET);
        let before = state(&transaction).await;
        assert_eq!(before.attempts, 1);
        assert_eq!(before.pending, Some(reserved.reserved_at));
        assert!(!before.consumed);

        // The wrong-code request sees the unreserved committed row, then
        // waits on its UPDATE while the correct reservation is uncommitted.
        let wrong_client = database.connect_as_auth().await;
        let pid = wrong_client
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        let wrong = compio::runtime::spawn(async move {
            magic_completions::consume_pending(&wrong_client, NONCE, "000000").await
        });
        let waiting = database.wait_until_blocked(&[pid]).await;
        transaction.commit().await.unwrap();
        let outcome = wrong.await.unwrap();
        assert!(
            matches!(outcome, Err(ConsumeError::WrongCode)),
            "{outcome:?}"
        );
        assert_eq!(
            state(&winner).await,
            before,
            "wrong code must not mutate the winning reservation"
        );
        assert!(
            magic_completions::finalize_consume(&winner, NONCE, &reserved.reserved_at)
                .await
                .unwrap()
        );
        assert!(
            waiting,
            "wrong-code update must overlap the winning reservation"
        );
    })
    .await;
}

#[compio::test]
async fn wrong_codes_exhaust_the_completion() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;
        create(&client).await;
        for attempt in 1..=5 {
            let outcome = magic_completions::consume_pending(&client, NONCE, "000000").await;
            assert!(
                matches!(outcome, Err(ConsumeError::WrongCode)),
                "{outcome:?}"
            );
            let state = state(&client).await;
            assert_eq!(state.attempts, attempt);
            assert_eq!(state.consumed, attempt == 5);
            assert!(state.pending.is_none());
        }
        let outcome = magic_completions::consume_pending(&client, NONCE, CODE).await;
        assert!(
            matches!(outcome, Err(ConsumeError::WrongCode)),
            "{outcome:?}"
        );
        assert_eq!(state(&client).await.attempts, 5);

        magic_completions::create(&client, "another-nonce", CODE, EMAIL, TARGET, 300)
            .await
            .unwrap();
        let fresh = magic_completions::consume_pending(&client, "another-nonce", CODE)
            .await
            .expect("another completion remains usable");
        assert_eq!(fresh.email, EMAIL);
    })
    .await;
}

#[compio::test]
async fn concurrent_correct_codes_reserve_once_without_wrong_attempts() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;
        create(&client).await;
        let mut locker = database.connect().await;
        let transaction = locker.transaction().await.unwrap();
        transaction.query_one(
            "SELECT csrf_nonce FROM zeroship.magic_completions WHERE csrf_nonce = $1 FOR UPDATE",
            &[&NONCE],
        ).await.unwrap();
        let mut pids = Vec::new();
        let mut handles = Vec::new();
        for _ in 0..5 {
            let contender = database.connect_as_auth().await;
            pids.push(
                contender
                    .query_one("SELECT pg_backend_pid()", &[])
                    .await
                    .unwrap()
                    .get(0),
            );
            handles.push(compio::runtime::spawn(async move {
                magic_completions::consume_pending(&contender, NONCE, CODE).await
            }));
        }
        let waiting = database.wait_until_blocked(&pids).await;
        transaction.commit().await.unwrap();
        let outcomes = futures::future::join_all(handles).await;
        let mut accepted = Vec::new();
        let mut in_flight = 0;
        for outcome in outcomes {
            match outcome.unwrap() {
                Ok(completion) => accepted.push(completion),
                Err(ConsumeError::InFlight) => in_flight += 1,
                Err(error) => panic!("correct code must not count as a wrong attempt: {error:?}"),
            }
        }
        assert_eq!(accepted.len(), 1);
        assert_eq!(in_flight, pids.len() - accepted.len());
        assert_eq!(
            state(&client).await,
            State {
                attempts: 1,
                pending: Some(accepted[0].reserved_at),
                consumed: false,
            }
        );
        assert!(
            magic_completions::finalize_consume(&client, NONCE, &accepted[0].reserved_at)
                .await
                .unwrap()
        );
        let replay = magic_completions::consume_pending(&client, NONCE, CODE).await;
        assert!(matches!(replay, Err(ConsumeError::WrongCode)), "{replay:?}");
        assert!(waiting, "all contenders must overlap on the completion row");
    })
    .await;
}

#[compio::test]
async fn stale_completion_owner_cannot_finalize_or_clear_a_newer_reservation() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;
        create(&client).await;
        let first = magic_completions::consume_pending(&client, NONCE, CODE).await.unwrap();
        database.connect().await.execute(
            "UPDATE zeroship.magic_completions SET consumed_pending_at = NOW() - INTERVAL '61 seconds' \
             WHERE csrf_nonce = $1", &[&NONCE],
        ).await.unwrap();
        let current = magic_completions::consume_pending(&client, NONCE, CODE).await.unwrap();
        assert_ne!(first.reserved_at, current.reserved_at);
        let before = state(&client).await;
        assert!(!magic_completions::finalize_consume(&client, NONCE, &first.reserved_at)
            .await.unwrap());
        assert!(!magic_completions::clear_consume_pending(&client, NONCE, Some(&first.reserved_at))
            .await.unwrap());
        assert_eq!(state(&client).await, before);

        assert!(magic_completions::clear_consume_pending(&client, NONCE, Some(&current.reserved_at))
            .await.unwrap(), "the current owner can release its reservation");
        let retried = magic_completions::consume_pending(&client, NONCE, CODE).await.unwrap();
        assert_ne!(current.reserved_at, retried.reserved_at);
        assert!(!magic_completions::finalize_consume(&client, NONCE, &current.reserved_at)
            .await.unwrap());
        assert!(magic_completions::finalize_consume(&client, NONCE, &retried.reserved_at)
            .await.unwrap(), "the retried owner can finalize");
    }).await;
}

#[compio::test]
async fn expired_completion_cannot_be_reserved() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;
        magic_completions::create(&client, NONCE, CODE, EMAIL, TARGET, -1)
            .await
            .unwrap();
        let before = state(&client).await;
        let outcome = magic_completions::consume_pending(&client, NONCE, CODE).await;
        assert!(
            matches!(outcome, Err(ConsumeError::WrongCode)),
            "{outcome:?}"
        );
        assert_eq!(state(&client).await, before);
    })
    .await;
}
