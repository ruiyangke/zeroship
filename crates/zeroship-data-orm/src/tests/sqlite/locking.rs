//! SQLite locking contracts.
use super::fixtures::*;

use crate::tests::fixtures::Host;

use zeroship_data_orm::backend::{LockManager, LockScope};

use zeroship_data_orm::error::DbError;

use zeroship_data_orm::lock_policy::BoundedLockAcquire;

#[cfg(test)]
use crate::tests::fixtures::DatabaseFixture;

#[test]
fn lock_try_acquire_blocks_second() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            let client = backend
                .fixture_session(&crate::tests::fixtures::harness_alias("default"))
                .await
                .expect("acquire client");
            // First acquisition on a fresh registry succeeds.
            backend
                .acquire_advisory_lock(&client, "key1", "key2")
                .await
                .expect("first acquire_advisory_lock");

            // A try_acquire with the same `(key1, key2)` must observe the
            // slot as held — `Ok(false)` is the contended return. The second
            // handle comes from `autocommit_client()`, which is a handle on the
            // OTHER connection: `fixture_session` is now the exclusive
            // `tx_conn` reservation and a second one is refused, so asking for it
            // here would measure lane admission rather than lock contention.
            let other_client = backend.autocommit_client();
            let got = backend
                .try_acquire_advisory_lock(&other_client, "key1", "key2")
                .await
                .expect("try_acquire_advisory_lock");
            assert!(
                !got,
                "second try_acquire on a held slot must return Ok(false) (got = {got})"
            );
        });
    })
}

#[test]
fn lock_release_unblocks() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            let client = backend
                .fixture_session(&crate::tests::fixtures::harness_alias("default"))
                .await
                .expect("acquire client");
            backend
                .acquire_advisory_lock(&client, "key1", "key2")
                .await
                .expect("first acquire");
            backend
                .release_advisory_lock(&client, "key1", "key2")
                .await
                .expect("release");
            // After release the slot is free — try_acquire flips it back
            // to held and returns Ok(true).
            let got = backend
                .try_acquire_advisory_lock(&client, "key1", "key2")
                .await
                .expect("try_acquire after release");
            assert!(
                got,
                "try_acquire after release must return Ok(true) (got = {got})"
            );
        });
    })
}

#[test]
fn lock_acquire_with_backoff_exhausts_into_contention_error() {
    Host::test(|host| {
        // Holding the same app lock must exhaust the configured backoff and
        // surface the portable lock-contention error.
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            let client = backend
                .fixture_session(&crate::tests::fixtures::harness_alias("default"))
                .await
                .expect("acquire client");
            let scope = LockScope::GlobalApp {
                app_id: "app_demo".to_string(),
                name: "snapshot_restore".to_string(),
            };

            // Hold the slot via the underlying primitive — the typed
            // `try_acquire_with_backoff` will then loop against a
            // permanently-held registry slot.
            let (k1, k2) = scope.to_keys();
            let got = backend
                .try_acquire_advisory_lock(&client, &k1, &k2)
                .await
                .expect("hold slot via try_acquire_advisory_lock");
            assert!(got, "initial hold must succeed");

            // The typed surface exhausts its configured schedule and reports
            // lock contention while the slot remains held.
            let err = backend
                .try_acquire_with_backoff(&client, &scope)
                .await
                .expect_err("backoff loop must exhaust into contention error");
            match err {
                DbError::LockContention { message } => {
                    assert!(
                        message.contains("app_demo"),
                        "contention message should mention the scope's app_id: {message}"
                    );
                    assert!(
                        message.contains("snapshot_restore"),
                        "contention message should mention the scope name: {message}"
                    );
                }
                other => {
                    panic!(
                        "expected DbError::LockContention after backoff exhaustion, got {other:?}"
                    )
                }
            }

            // Sanity: the wire code surfaces as `lock_not_available`
            // through `to_op_error`. We don't reach into `to_op_error`
            // here (it's a private mapping) — the message-shape assertion
            // above is the test-level invariant; the
            // `lock_not_available` mapping is covered by the lib-level
            // `op_code` tests in `crate::error`.
        });
    })
}
