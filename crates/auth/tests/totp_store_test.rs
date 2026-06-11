//! TOTP 2FA store + crypto lifecycle — live PG (ISS-11).
//!
//! Skipped unless `AUTH_DB_URL` (or `PG_TEST_URL`) is set. These drive the REAL
//! `store::totp` transactions and the REAL `identity::totp` crypto/verify — no
//! shims — so a green run exercises the same code the `/me/2fa/*` handlers and
//! the login challenge call in production. Run with `--test-threads=1`.
//!
//! Each test scopes itself with a random tag and cleans up the rows it inserted.

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;

use zeroship_auth::identity::totp;
use zeroship_auth::store::{totp as totp_store, users};

#[allow(clippy::future_not_send)]
async fn pg() -> Option<Client> {
    let dsn = std::env::var("AUTH_DB_URL")
        .ok()
        .or_else(|| std::env::var("PG_TEST_URL").ok())?;
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("totp_store_test pg connection error: {e}");
        }
    })
    .detach();
    Some(client)
}

fn key() -> [u8; 32] {
    zeroship_core::crypto::derive_key("totp-store-test-key")
}

#[allow(clippy::future_not_send)]
async fn cleanup(db: &Client, ids: &[Uuid]) {
    for id in ids {
        let _ = db
            .execute("DELETE FROM zeroship.totp_backup_codes WHERE user_id = $1", &[id])
            .await;
        let _ = db
            .execute("DELETE FROM zeroship.totp_credentials WHERE user_id = $1", &[id])
            .await;
        let _ = db
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[id])
            .await;
    }
}

#[compio::test]
async fn enroll_stores_encrypted_and_unconfirmed() {
    let Some(db) = pg().await else {
        eprintln!("skipping totp_store_test (no AUTH_DB_URL/PG_TEST_URL)");
        return;
    };
    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(&db, &format!("totp-enroll-{tag}@zeroship.test"), "Enroll", Some("phc"))
        .await
        .unwrap();

    let secret = totp::generate_secret();
    let ct = totp::encrypt_secret(&key(), user.id, &secret).unwrap();
    totp_store::enroll(&db, user.id, &ct).await.expect("enroll");

    // Pending: row exists but confirmed_at is NULL, so it does NOT gate login.
    let cred = totp_store::find(&db, user.id).await.unwrap().expect("row exists");
    assert!(cred.confirmed_at.is_none(), "fresh enrollment is unconfirmed");
    assert!(
        !totp_store::is_enabled(&db, user.id).await.unwrap(),
        "an unconfirmed credential must not be enabled"
    );

    // The stored secret is ENCRYPTED — the raw 20-byte secret never appears.
    let stored = db
        .query_one(
            "SELECT encrypted_secret FROM zeroship.totp_credentials WHERE user_id = $1",
            &[&user.id],
        )
        .await
        .unwrap();
    let blob: Vec<u8> = stored.get("encrypted_secret");
    assert!(
        !blob.windows(secret.len()).any(|w| w == secret.as_slice()),
        "plaintext secret must not be stored"
    );
    // ...but it decrypts back to the original secret (bound to this user).
    assert_eq!(totp::decrypt_secret(&key(), user.id, &blob).unwrap(), secret);

    cleanup(&db, &[user.id]).await;
}

#[compio::test]
async fn confirm_activates_and_issues_backup_codes() {
    let Some(db) = pg().await else {
        return;
    };
    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(&db, &format!("totp-confirm-{tag}@zeroship.test"), "Confirm", Some("phc"))
        .await
        .unwrap();

    let secret = totp::generate_secret();
    let ct = totp::encrypt_secret(&key(), user.id, &secret).unwrap();
    totp_store::enroll(&db, user.id, &ct).await.unwrap();

    let (plain, hashes) = totp::generate_backup_codes().unwrap();
    let confirmed = totp_store::confirm(&db, user.id, &hashes).await.expect("confirm");
    assert!(confirmed, "confirm must succeed for a pending credential");

    // Now active.
    assert!(totp_store::is_enabled(&db, user.id).await.unwrap(), "confirmed → enabled");
    let cred = totp_store::find_confirmed(&db, user.id).await.unwrap().expect("confirmed");
    assert!(cred.confirmed_at.is_some());

    // Backup codes landed (count, all unused, hashes not plaintext).
    let unused = totp_store::unused_backup_codes(&db, user.id).await.unwrap();
    assert_eq!(unused.len(), totp::BACKUP_CODE_COUNT, "all codes minted, all unused");
    for (code, row) in plain.iter().zip(&unused) {
        assert!(row.code_hash.starts_with("$argon2"), "stored as PHC hash");
        assert!(!row.code_hash.contains(code), "hash must not be the plaintext code");
    }

    cleanup(&db, &[user.id]).await;
}

#[compio::test]
async fn confirm_without_enrollment_is_a_noop() {
    let Some(db) = pg().await else {
        return;
    };
    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(&db, &format!("totp-noconf-{tag}@zeroship.test"), "NoConf", Some("phc"))
        .await
        .unwrap();

    let (_, hashes) = totp::generate_backup_codes().unwrap();
    let confirmed = totp_store::confirm(&db, user.id, &hashes).await.expect("confirm");
    assert!(!confirmed, "confirm with no pending credential writes nothing");
    let unused = totp_store::unused_backup_codes(&db, user.id).await.unwrap();
    assert!(unused.is_empty(), "no codes inserted without a credential");

    cleanup(&db, &[user.id]).await;
}

#[compio::test]
async fn current_code_verifies_against_stored_secret() {
    let Some(db) = pg().await else {
        return;
    };
    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(&db, &format!("totp-verify-{tag}@zeroship.test"), "Verify", Some("phc"))
        .await
        .unwrap();

    let secret = totp::generate_secret();
    let ct = totp::encrypt_secret(&key(), user.id, &secret).unwrap();
    totp_store::enroll(&db, user.id, &ct).await.unwrap();

    // Round-trip the stored ciphertext, then verify a fresh current code.
    let cred = totp_store::find(&db, user.id).await.unwrap().unwrap();
    let recovered = totp::decrypt_secret(&key(), user.id, &cred.encrypted_secret).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let code = totp::code_at(&recovered, now);
    assert!(totp::verify_code(&recovered, &code), "current code verifies");
    assert!(!totp::verify_code(&recovered, "000001"), "an arbitrary wrong code is rejected");

    cleanup(&db, &[user.id]).await;
}

#[compio::test]
async fn backup_code_works_once_then_is_rejected() {
    let Some(db) = pg().await else {
        return;
    };
    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(&db, &format!("totp-backup-{tag}@zeroship.test"), "Backup", Some("phc"))
        .await
        .unwrap();

    let secret = totp::generate_secret();
    let ct = totp::encrypt_secret(&key(), user.id, &secret).unwrap();
    totp_store::enroll(&db, user.id, &ct).await.unwrap();
    let (plain, hashes) = totp::generate_backup_codes().unwrap();
    totp_store::confirm(&db, user.id, &hashes).await.unwrap();

    // Redeem the first code: find the matching unused row, verify, mark used.
    let target = &plain[0];
    let unused = totp_store::unused_backup_codes(&db, user.id).await.unwrap();
    let mut matched_id = None;
    for row in &unused {
        if totp::verify_backup_code(target, &row.code_hash).unwrap() {
            matched_id = Some(row.id);
            break;
        }
    }
    let id = matched_id.expect("a backup code must match");
    assert!(totp_store::mark_backup_code_used(&db, id).await.unwrap(), "first redeem succeeds");

    // Second redeem of the SAME row loses the single-use race.
    assert!(
        !totp_store::mark_backup_code_used(&db, id).await.unwrap(),
        "a used backup code can't be redeemed again"
    );
    // And it no longer appears in the unused set.
    let after = totp_store::unused_backup_codes(&db, user.id).await.unwrap();
    assert_eq!(after.len(), totp::BACKUP_CODE_COUNT - 1, "one code consumed");
    assert!(after.iter().all(|r| r.id != id), "the used code is gone from unused");

    cleanup(&db, &[user.id]).await;
}

#[compio::test]
async fn disable_removes_credential_and_codes() {
    let Some(db) = pg().await else {
        return;
    };
    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(&db, &format!("totp-disable-{tag}@zeroship.test"), "Disable", Some("phc"))
        .await
        .unwrap();

    let secret = totp::generate_secret();
    let ct = totp::encrypt_secret(&key(), user.id, &secret).unwrap();
    totp_store::enroll(&db, user.id, &ct).await.unwrap();
    let (_, hashes) = totp::generate_backup_codes().unwrap();
    totp_store::confirm(&db, user.id, &hashes).await.unwrap();
    assert!(totp_store::is_enabled(&db, user.id).await.unwrap());

    let removed = totp_store::disable(&db, user.id).await.expect("disable");
    assert!(removed, "disable removes an existing credential");
    assert!(totp_store::find(&db, user.id).await.unwrap().is_none(), "credential gone");
    assert!(
        totp_store::unused_backup_codes(&db, user.id).await.unwrap().is_empty(),
        "backup codes gone"
    );
    // Idempotent: disabling again is a clean no-op.
    assert!(!totp_store::disable(&db, user.id).await.unwrap(), "second disable is a no-op");

    cleanup(&db, &[user.id]).await;
}

#[compio::test]
async fn re_enroll_resets_to_pending() {
    let Some(db) = pg().await else {
        return;
    };
    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(&db, &format!("totp-reenroll-{tag}@zeroship.test"), "ReEnroll", Some("phc"))
        .await
        .unwrap();

    let s1 = totp::generate_secret();
    totp_store::enroll(&db, user.id, &totp::encrypt_secret(&key(), user.id, &s1).unwrap())
        .await
        .unwrap();
    let (_, hashes) = totp::generate_backup_codes().unwrap();
    totp_store::confirm(&db, user.id, &hashes).await.unwrap();
    assert!(totp_store::is_enabled(&db, user.id).await.unwrap());

    // Re-enroll with a new secret resets to PENDING (login no longer gated until
    // a fresh confirm) and changes the stored secret.
    let s2 = totp::generate_secret();
    assert_ne!(s1, s2);
    totp_store::enroll(&db, user.id, &totp::encrypt_secret(&key(), user.id, &s2).unwrap())
        .await
        .unwrap();
    assert!(
        !totp_store::is_enabled(&db, user.id).await.unwrap(),
        "re-enroll resets to pending (unconfirmed)"
    );
    let cred = totp_store::find(&db, user.id).await.unwrap().unwrap();
    assert_eq!(totp::decrypt_secret(&key(), user.id, &cred.encrypted_secret).unwrap(), s2);

    cleanup(&db, &[user.id]).await;
}

#[compio::test]
async fn credential_cascades_on_user_delete() {
    let Some(db) = pg().await else {
        return;
    };
    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(&db, &format!("totp-cascade-{tag}@zeroship.test"), "Cascade", Some("phc"))
        .await
        .unwrap();
    let secret = totp::generate_secret();
    totp_store::enroll(&db, user.id, &totp::encrypt_secret(&key(), user.id, &secret).unwrap())
        .await
        .unwrap();
    let (_, hashes) = totp::generate_backup_codes().unwrap();
    totp_store::confirm(&db, user.id, &hashes).await.unwrap();

    // Hard-delete the user → CASCADE tears down credential + codes (ISS-11/ISS-12).
    db.execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .unwrap();
    assert!(totp_store::find(&db, user.id).await.unwrap().is_none(), "credential cascaded");
    assert!(
        totp_store::unused_backup_codes(&db, user.id).await.unwrap().is_empty(),
        "codes cascaded"
    );
}
