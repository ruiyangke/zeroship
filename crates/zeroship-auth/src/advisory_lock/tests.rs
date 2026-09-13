use super::{
    release_advisory_lock, try_acquire_advisory_lock, with_advisory_lock, ONE_ARGUMENT_KEYS,
};
use crate::{error::AuthError, test_database::Database};
use futures::channel::oneshot;

#[test]
fn all_one_argument_keys_are_distinct() {
    let mut seen = std::collections::HashMap::<i64, &str>::new();
    for (key, name) in ONE_ARGUMENT_KEYS {
        if let Some(previous) = seen.insert(key, name) {
            panic!("advisory key {key:#x} is taken by both '{previous}' and '{name}'");
        }
    }
    assert_eq!(seen.len(), ONE_ARGUMENT_KEYS.len());
}

#[test]
fn every_declared_one_argument_key_is_registered() {
    for key in [
        super::OP_SIGNING_KEY_BOOTSTRAP_LOCK,
        super::ACCOUNT_REAPER_SWEEP_LOCK,
    ] {
        assert!(ONE_ARGUMENT_KEYS
            .iter()
            .any(|(registered, _)| *registered == key));
    }
}

#[compio::test]
async fn advisory_lock_serializes_same_key_across_sessions() {
    Database::run(async |database| {
        let holder = database.connect_as_auth().await;
        let peer = database.connect_as_auth().await;
        let peer_pid: i32 = peer
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        let key = 0x0042_B007_A071_2001_i64;
        let (entered, entered_rx) = oneshot::channel();
        let (start_peer, start_peer_rx) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();

        // Keep both sessions alive after the operations return: disconnecting
        // the holder must not substitute for the wrapper's explicit unlock.
        let (held, contended, blocked) = futures::join!(
            with_advisory_lock(&holder, key, || async {
                entered.send(()).unwrap();
                release_rx.await.unwrap();
                Ok("holder completed")
            }),
            async {
                start_peer_rx.await.unwrap();
                with_advisory_lock(&peer, key, || async { Ok("peer completed") }).await
            },
            async {
                entered_rx.await.unwrap();
                start_peer.send(()).unwrap();
                let blocked = database.wait_until_blocked(&[peer_pid]).await;
                release.send(()).unwrap();
                blocked
            },
        );
        assert!(blocked, "the peer must actually wait for the held lock");
        assert_eq!(held.unwrap(), "holder completed");
        assert_eq!(contended.unwrap(), "peer completed");
        assert!(try_acquire_advisory_lock(&holder, key).await.unwrap());
        release_advisory_lock(&holder, key).await.unwrap();
    })
    .await;
}

#[compio::test]
async fn an_operation_error_is_preserved_and_releases_the_lock() {
    Database::run(async |database| {
        let holder = database.connect_as_auth().await;
        let peer = database.connect_as_auth().await;
        let key = 0x0042_B007_A071_2001_i64;
        let result = with_advisory_lock(&holder, key, || async {
            Err::<(), _>(AuthError::Internal("operation refused".to_owned()))
        })
        .await;
        let AuthError::Internal(reason) = result.unwrap_err() else {
            panic!("the wrapper must preserve the operation's error");
        };
        assert_eq!(reason, "operation refused");
        assert!(try_acquire_advisory_lock(&peer, key).await.unwrap());
        release_advisory_lock(&peer, key).await.unwrap();
    })
    .await;
}

#[compio::test]
async fn a_second_session_is_refused_the_sweep_key_until_the_holder_releases() {
    Database::run(async |database| {
        let holder = database.connect_as_auth().await;
        let peer = database.connect_as_auth().await;
        let key = super::ACCOUNT_REAPER_SWEEP_LOCK;
        assert!(try_acquire_advisory_lock(&holder, key).await.unwrap());
        assert!(!try_acquire_advisory_lock(&peer, key).await.unwrap());
        release_advisory_lock(&holder, key).await.unwrap();
        assert!(try_acquire_advisory_lock(&peer, key).await.unwrap());
        release_advisory_lock(&peer, key).await.unwrap();
        assert!(try_acquire_advisory_lock(&holder, key).await.unwrap());
        release_advisory_lock(&holder, key).await.unwrap();
    })
    .await;
}
