//! Completion reservations and retry ownership in the shared database.

use crate::support::database::Database;
use compio_postgres::{Client, GenericClient};
use uuid::Uuid;
use zeroship_auth::store::magic_completions::{self, ConsumeError};

const CODE: &str = "123456";
const TARGET: &str = "completion-handoff";

fn nonce() -> String {
    format!("completion-{}", Uuid::new_v4().simple())
}

fn email() -> String {
    format!("completion-{}@example.test", Uuid::new_v4().simple())
}

async fn create(client: &Client, nonce: &str, email: &str) {
    magic_completions::create(client, nonce, CODE, email, TARGET, 300)
        .await
        .expect("create completion through the production store");
}

#[derive(Debug, PartialEq, Eq)]
struct State {
    attempts: i32,
    pending: Option<chrono::DateTime<chrono::Utc>>,
    consumed: bool,
}

#[allow(
    clippy::future_not_send,
    reason = "the fixture remains on its owning runtime"
)]
async fn state(client: &(impl GenericClient + ?Sized), nonce: &str) -> State {
    let row = client
        .query_one(
            "SELECT attempts, consumed_pending_at, consumed_at IS NOT NULL AS consumed \
         FROM zeroship.magic_completions WHERE csrf_nonce = $1",
            &[&nonce],
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
        let orm = database.orm().await;
        let nonce = nonce();
        let email = email();
        create(&winner, &nonce, &email).await;
        let transaction = winner.transaction().await.unwrap();
        let reserved = magic_completions::consume_pending(&transaction, &nonce, CODE)
            .await
            .unwrap();
        assert_eq!(reserved.email, email);
        assert_eq!(reserved.target, TARGET);
        let before = state(&transaction, &nonce).await;
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
        let wrong_nonce = nonce.clone();
        let wrong = compio::runtime::spawn(async move {
            magic_completions::consume_pending(&wrong_client, &wrong_nonce, "000000").await
        });
        let waiting = database.wait_until_blocked(&[pid]).await;
        transaction.commit().await.unwrap();
        let outcome = wrong.await.unwrap();
        assert!(
            matches!(outcome, Err(ConsumeError::WrongCode)),
            "{outcome:?}"
        );
        assert_eq!(
            state(&winner, &nonce).await,
            before,
            "wrong code must not mutate the winning reservation"
        );
        assert!(
            magic_completions::finalize_consume(&orm, &nonce, &reserved.reserved_at)
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
        let nonce = nonce();
        let email = email();
        create(&client, &nonce, &email).await;
        for attempt in 1..=5 {
            let outcome = magic_completions::consume_pending(&client, &nonce, "000000").await;
            assert!(
                matches!(outcome, Err(ConsumeError::WrongCode)),
                "{outcome:?}"
            );
            let state = state(&client, &nonce).await;
            assert_eq!(state.attempts, attempt);
            assert_eq!(state.consumed, attempt == 5);
            assert!(state.pending.is_none());
        }
        let outcome = magic_completions::consume_pending(&client, &nonce, CODE).await;
        assert!(
            matches!(outcome, Err(ConsumeError::WrongCode)),
            "{outcome:?}"
        );
        assert_eq!(state(&client, &nonce).await.attempts, 5);

        let fresh_nonce = format!("{nonce}-fresh");
        magic_completions::create(&client, &fresh_nonce, CODE, &email, TARGET, 300)
            .await
            .unwrap();
        let fresh = magic_completions::consume_pending(&client, &fresh_nonce, CODE)
            .await
            .expect("another completion remains usable");
        assert_eq!(fresh.email, email);
    })
    .await;
}

#[compio::test]
async fn concurrent_correct_codes_reserve_once_without_wrong_attempts() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;
        let orm = database.orm().await;
        let nonce = nonce();
        let email = email();
        create(&client, &nonce, &email).await;
        let mut locker = database.connect().await;
        let transaction = locker.transaction().await.unwrap();
        transaction.query_one(
            "SELECT csrf_nonce FROM zeroship.magic_completions WHERE csrf_nonce = $1 FOR UPDATE",
            &[&nonce],
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
            let contender_nonce = nonce.clone();
            handles.push(compio::runtime::spawn(async move {
                magic_completions::consume_pending(&contender, &contender_nonce, CODE).await
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
            state(&client, &nonce).await,
            State {
                attempts: 1,
                pending: Some(accepted[0].reserved_at),
                consumed: false,
            }
        );
        assert!(
            magic_completions::finalize_consume(&orm, &nonce, &accepted[0].reserved_at)
                .await
                .unwrap()
        );
        let replay = magic_completions::consume_pending(&client, &nonce, CODE).await;
        assert!(matches!(replay, Err(ConsumeError::WrongCode)), "{replay:?}");
        assert!(waiting, "all contenders must overlap on the completion row");
    })
    .await;
}

#[compio::test]
async fn stale_completion_owner_cannot_finalize_or_clear_a_newer_reservation() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;
        let orm = database.orm().await;
        let nonce = nonce();
        let email = email();
        create(&client, &nonce, &email).await;
        let first = magic_completions::consume_pending(&client, &nonce, CODE).await.unwrap();
        database.connect().await.execute(
            "UPDATE zeroship.magic_completions SET consumed_pending_at = NOW() - INTERVAL '61 seconds' \
             WHERE csrf_nonce = $1", &[&nonce],
        ).await.unwrap();
        let current = magic_completions::consume_pending(&client, &nonce, CODE).await.unwrap();
        assert_ne!(first.reserved_at, current.reserved_at);
        let before = state(&client, &nonce).await;
        assert!(!magic_completions::finalize_consume(&orm, &nonce, &first.reserved_at)
            .await.unwrap());
        assert!(!magic_completions::clear_consume_pending(&orm, &nonce, Some(&first.reserved_at))
            .await.unwrap());
        assert_eq!(state(&client, &nonce).await, before);

        assert!(magic_completions::clear_consume_pending(&orm, &nonce, Some(&current.reserved_at))
            .await.unwrap(), "the current owner can release its reservation");
        let retried = magic_completions::consume_pending(&client, &nonce, CODE).await.unwrap();
        assert_ne!(current.reserved_at, retried.reserved_at);
        assert!(!magic_completions::finalize_consume(&orm, &nonce, &current.reserved_at)
            .await.unwrap());
        assert!(magic_completions::finalize_consume(&orm, &nonce, &retried.reserved_at)
            .await.unwrap(), "the retried owner can finalize");
    }).await;
}

#[compio::test]
async fn expired_completion_cannot_be_reserved() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;
        let nonce = nonce();
        let email = email();
        magic_completions::create(&client, &nonce, CODE, &email, TARGET, -1)
            .await
            .unwrap();
        let before = state(&client, &nonce).await;
        let outcome = magic_completions::consume_pending(&client, &nonce, CODE).await;
        assert!(
            matches!(outcome, Err(ConsumeError::WrongCode)),
            "{outcome:?}"
        );
        assert_eq!(state(&client, &nonce).await, before);
    })
    .await;
}

/// A reservation instant read back from the server binds back to its own row,
/// so the reservation is finalized. This is the whole reason
/// `native::instant_value` exists: equality on a timestamp survives only a
/// lossless conversion.
#[compio::test]
async fn stored_reservation_instant_finalizes_its_own_row() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;
        let orm = database.orm().await;
        let nonce = nonce();
        let email = email();
        create(&client, &nonce, &email).await;
        magic_completions::consume_pending(&client, &nonce, CODE)
            .await
            .unwrap();
        let stored: chrono::DateTime<chrono::Utc> = client
            .query_one(
                "SELECT consumed_pending_at FROM zeroship.magic_completions WHERE csrf_nonce = $1",
                &[&nonce],
            )
            .await
            .unwrap()
            .get(0);
        // The database clock carries a fraction, so a millisecond-floored bind
        // would be a different instant than the one stored.
        assert_ne!(
            stored.timestamp_subsec_micros() % 1_000,
            0,
            "the database clock must supply a sub-millisecond fraction for this case to bind"
        );
        assert!(
            magic_completions::finalize_consume(&orm, &nonce, &stored)
                .await
                .unwrap(),
            "the stored reservation instant must finalize its own row"
        );
    })
    .await;
}

/// Rejection control for the lossless bind: an instant one microsecond away
/// from the stored reservation is a different instant and finalizes nothing,
/// while the untouched reservation is still finalized by its stored instant.
#[compio::test]
async fn finalize_rejects_a_reservation_instant_one_microsecond_off() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;
        let orm = database.orm().await;
        let nonce = nonce();
        let email = email();
        create(&client, &nonce, &email).await;
        magic_completions::consume_pending(&client, &nonce, CODE)
            .await
            .unwrap();
        let stored: chrono::DateTime<chrono::Utc> = client
            .query_one(
                "SELECT consumed_pending_at FROM zeroship.magic_completions WHERE csrf_nonce = $1",
                &[&nonce],
            )
            .await
            .unwrap()
            .get(0);
        let skewed = stored + chrono::Duration::microseconds(1);
        assert!(
            !magic_completions::finalize_consume(&orm, &nonce, &skewed)
                .await
                .unwrap(),
            "an instant one microsecond off must not match the reservation"
        );
        assert!(
            magic_completions::finalize_consume(&orm, &nonce, &stored)
                .await
                .unwrap(),
            "the untouched reservation is still finalized by its stored instant"
        );
    })
    .await;
}
