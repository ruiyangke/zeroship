//! TOTP login-challenge gate — the second-factor decision + stash binding
//! exercised against the REAL store/crypto (ISS-11).
//!
//! This test pins the store/crypto parts that are the security core of the
//! challenge: (1) a confirmed-2FA user is gated (`is_enabled`), a pending one is
//! NOT; (2) the signed factor-1 stash round-trips bound to the user +
//! credential_version, and a version bump (password change / forced logout)
//! invalidates it — the exact check `post_2fa` runs before accepting factor 2;
//! (3) the second-factor evaluation the handler performs (valid TOTP code OR a
//! single-use backup code) accepts/rejects correctly against the real store.
//!
//! Skipped unless `AUTH_DB_URL` (or `PG_TEST_URL`) is set. Run `--test-threads=1`.

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;

use zeroship_auth::identity::totp;
use zeroship_auth::sessions::totp_challenge::TotpChallenge;
use zeroship_auth::store::{totp as totp_store, users};

#[allow(clippy::future_not_send)]
async fn pg() -> Option<Client> {
    let dsn = std::env::var("AUTH_DB_URL")
        .ok()
        .or_else(|| std::env::var("PG_TEST_URL").ok())?;
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("totp_login_challenge_test pg connection error: {e}");
        }
    })
    .detach();
    Some(client)
}

fn key() -> [u8; 32] {
    zeroship_core::crypto::derive_key("totp-challenge-test-key")
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
        let _ = db.execute("DELETE FROM zeroship.users WHERE id = $1", &[id]).await;
    }
}

/// (1) Only a CONFIRMED credential gates login; a pending enrollment does not —
/// this is the `is_enabled` branch in the `/login` POST success path.
#[compio::test]
async fn only_confirmed_credential_gates_login() {
    let Some(db) = pg().await else {
        eprintln!("skipping totp_login_challenge_test (no AUTH_DB_URL/PG_TEST_URL)");
        return;
    };
    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(&db, &format!("totp-gate-{tag}@zeroship.test"), "Gate", Some("phc"))
        .await
        .unwrap();

    // No credential → not gated.
    assert!(!totp_store::is_enabled(&db, user.id).await.unwrap());

    // Pending enrollment → still not gated (login must NOT be blocked yet).
    let secret = totp::generate_secret();
    totp_store::enroll(&db, user.id, &totp::encrypt_secret(&key(), user.id, &secret).unwrap())
        .await
        .unwrap();
    assert!(
        !totp_store::is_enabled(&db, user.id).await.unwrap(),
        "a pending (unconfirmed) credential must not gate login"
    );

    // Confirm → now gated.
    let (_, hashes) = totp::generate_backup_codes().unwrap();
    totp_store::confirm(&db, user.id, &hashes).await.unwrap();
    assert!(
        totp_store::is_enabled(&db, user.id).await.unwrap(),
        "a confirmed credential gates login (second factor required)"
    );

    cleanup(&db, &[user.id]).await;
}

/// (2) The factor-1 stash round-trips bound to user_id + credential_version, and
/// a version mismatch (the `post_2fa` re-check) invalidates it.
#[compio::test]
async fn challenge_stash_binds_user_and_credential_version() {
    let Some(db) = pg().await else {
        return;
    };
    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(&db, &format!("totp-stash-{tag}@zeroship.test"), "Stash", Some("phc"))
        .await
        .unwrap();
    let cv = user.credential_version;
    let signing_key = b"test-stash-key-not-for-prod-32bytes!";

    let stash = TotpChallenge::new(user.id, cv, "lc-xyz".into());
    let cookie = stash.encode(signing_key);
    let decoded = TotpChallenge::decode(&cookie, signing_key).expect("decode");
    assert_eq!(decoded.user_id, user.id);
    assert_eq!(decoded.credential_version, cv);
    assert_eq!(decoded.return_to, "lc-xyz");

    // Bump credential_version (a password reset does this). The cookie's
    // captured version no longer matches the live row → `post_2fa` rejects it.
    let fresh = users::find_by_id(&db, &user.id.to_string())
        .await
        .unwrap()
        .unwrap();
    // Simulate a password change which bumps credential_version.
    users::update_password_hash(&db, user.id, "phc2").await.unwrap();
    let after = users::find_by_id(&db, &user.id.to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fresh.credential_version, decoded.credential_version);
    assert_ne!(
        after.credential_version, decoded.credential_version,
        "a password change bumps credential_version, invalidating the in-flight 2FA challenge"
    );

    cleanup(&db, &[user.id]).await;
}

/// (3) The second-factor evaluation the handler runs: a current TOTP code is
/// accepted; a wrong code is rejected; a backup code works exactly once.
#[compio::test]
async fn second_factor_accepts_totp_then_backup_code_once() {
    let Some(db) = pg().await else {
        return;
    };
    let tag = Uuid::new_v4().simple().to_string();
    let user = users::create(&db, &format!("totp-2f-{tag}@zeroship.test"), "TwoF", Some("phc"))
        .await
        .unwrap();

    let secret = totp::generate_secret();
    totp_store::enroll(&db, user.id, &totp::encrypt_secret(&key(), user.id, &secret).unwrap())
        .await
        .unwrap();
    let (plain, hashes) = totp::generate_backup_codes().unwrap();
    totp_store::confirm(&db, user.id, &hashes).await.unwrap();

    // --- TOTP code path (what post_2fa step 5a checks) ---
    let cred = totp_store::find_confirmed(&db, user.id).await.unwrap().unwrap();
    let recovered = totp::decrypt_secret(&key(), user.id, &cred.encrypted_secret).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let code = totp::code_at(&recovered, now);
    assert!(totp::verify_code(&recovered, &code), "valid TOTP code accepted");
    assert!(!totp::verify_code(&recovered, "111111"), "an arbitrary code rejected");

    // --- Backup-code path (post_2fa step 5b): find match, mark used, single-use ---
    let target = &plain[0];
    let codes = totp_store::unused_backup_codes(&db, user.id).await.unwrap();
    let matched = codes
        .iter()
        .find(|c| totp::verify_backup_code(target, &c.code_hash).unwrap())
        .expect("a backup code must match");
    assert!(totp_store::mark_backup_code_used(&db, matched.id).await.unwrap(), "first use ok");
    assert!(
        !totp_store::mark_backup_code_used(&db, matched.id).await.unwrap(),
        "a redeemed backup code can't be reused (single-use)"
    );

    cleanup(&db, &[user.id]).await;
}
