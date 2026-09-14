//! Token minting against owned sessions and the migrated platform schema.

mod fixtures;

use super::mint_access_token;
use crate::{advisory_lock, store::users, test_database::Database};
use base64::Engine as _;
use fixtures::MintFixture;
use std::time::Duration;

fn token_iat(token: &str) -> i64 {
    let payload = token.split('.').nth(1).unwrap();
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .unwrap();
    serde_json::from_slice::<serde_json::Value>(&decoded).unwrap()["iat"]
        .as_i64()
        .unwrap()
}

#[compio::test]
async fn identity_claim_projection_uses_the_mint_transaction_and_granted_scopes() {
    Database::run(async |database| {
        let fixture = MintFixture::seed(database).await;
        let observer = database.connect_as_auth().await;
        let mut mint = database.connect_as_auth().await;
        let tx = mint.transaction().await.unwrap();
        let proof = fixture.proof(&tx).await;
        tx.execute(
            "UPDATE zeroship.users SET email = $2, email_verified_at = NOW(), \
             name = $3, avatar_url = $4 WHERE id = $1",
            &[
                &fixture.user_id.as_str(),
                &"pending@example.test",
                &"Pending profile",
                &"https://example.test/pending.png",
            ],
        )
        .await
        .unwrap();
        let observed = observer
            .query_one(
                "SELECT email::text, name FROM zeroship.users WHERE id = $1",
                &[&fixture.user_id.as_str()],
            )
            .await
            .unwrap();
        assert_eq!(observed.get::<_, String>("email"), "recipient@example.test");
        assert_eq!(observed.get::<_, String>("name"), "Mint subject");

        for (scopes, expected) in [
            (
                vec!["openid".to_owned(), "email".to_owned(), "profile".to_owned()],
                serde_json::json!({
                    "email":"pending@example.test", "email_verified":true,
                    "name":"Pending profile", "picture":"https://example.test/pending.png"
                }),
            ),
            (
                vec!["openid".to_owned(), "email".to_owned()],
                serde_json::json!({"email":"pending@example.test", "email_verified":true}),
            ),
            (
                vec!["openid".to_owned(), "profile".to_owned()],
                serde_json::json!({"name":"Pending profile", "picture":"https://example.test/pending.png"}),
            ),
            (vec!["openid".to_owned()], serde_json::json!({})),
        ] {
            let claims = super::transaction_identity_claims(&tx, &proof, &scopes)
                .await
                .unwrap();
            assert_eq!(serde_json::to_value(claims).unwrap(), expected);
        }
        tx.execute(
            "UPDATE zeroship.users SET email_verified_at = NULL, avatar_url = NULL WHERE id = $1",
            &[&fixture.user_id.as_str()],
        )
        .await
        .unwrap();
        let claims = super::transaction_identity_claims(
            &tx,
            &proof,
            &["email".to_owned(), "profile".to_owned()],
        )
        .await
        .unwrap();
        assert_eq!(claims.email_verified, Some(false));
        assert!(claims.picture.is_none());
        tx.rollback().await.unwrap();
        let restored = observer
            .query_one(
                "SELECT email::text, name, avatar_url, email_verified_at IS NOT NULL AS verified \
                 FROM zeroship.users WHERE id = $1",
                &[&fixture.user_id.as_str()],
            )
            .await
            .unwrap();
        assert_eq!(restored.get::<_, String>("email"), "recipient@example.test");
        assert_eq!(restored.get::<_, String>("name"), "Mint subject");
        assert!(restored.get::<_, Option<String>>("avatar_url").is_none());
        assert!(!restored.get::<_, bool>("verified"));
    })
    .await;
}

#[compio::test]
async fn access_token_mint_holds_the_user_lock_until_the_transaction_ends() {
    Database::run(async |database| {
        let fixture = MintFixture::seed(database).await;
        let mut mint = database.connect_as_auth().await;
        let contender = database.connect_as_auth().await;
        let setup = mint.transaction().await.unwrap();
        let proof = fixture.proof(&setup).await;
        setup.commit().await.unwrap();
        let available: bool = contender
            .query_one(
                "SELECT pg_try_advisory_xact_lock($1::INT4, hashtext($2::text))",
                &[&advisory_lock::NS_USER, &fixture.user_id.as_str()],
            )
            .await
            .unwrap()
            .get(0);
        assert!(
            available,
            "session setup must leave the mint lock available"
        );
        let tx = mint.transaction().await.unwrap();
        mint_access_token(
            &tx,
            &fixture.issuer,
            &fixture.client,
            &fixture.user_id,
            &["openid".to_owned()],
            &proof,
        )
        .await
        .unwrap();
        let acquired: bool = contender
            .query_one(
                "SELECT pg_try_advisory_xact_lock($1::INT4, hashtext($2::text))",
                &[&advisory_lock::NS_USER, &fixture.user_id.as_str()],
            )
            .await
            .unwrap()
            .get(0);
        assert!(!acquired);
        tx.rollback().await.unwrap();
        let acquired: bool = contender
            .query_one(
                "SELECT pg_try_advisory_xact_lock($1::INT4, hashtext($2::text))",
                &[&advisory_lock::NS_USER, &fixture.user_id.as_str()],
            )
            .await
            .unwrap()
            .get(0);
        assert!(acquired);
        assert!(contender
            .query(
                "SELECT app_client_id FROM zeroship.app_user_identities",
                &[]
            )
            .await
            .unwrap()
            .is_empty());
    })
    .await;
}

#[compio::test]
async fn mint_persists_the_pairwise_identity_reactivates_it_and_refuses_rebinding() {
    Database::run(async |database| {
        let fixture = MintFixture::seed(database).await;
        let mut mint = database.connect_as_auth().await;
        let tx = mint.transaction().await.unwrap();
        let proof = fixture.proof(&tx).await;
        let pairwise = fixture.issuer.pairwise_subject(&fixture.user_id, &fixture.client.sector_identifier);
        for _ in 0..2 {
            mint_access_token(&tx, &fixture.issuer, &fixture.client, &fixture.user_id, &["openid".to_owned()], &proof).await.unwrap();
            let row = tx.query_one(
                "SELECT app_client_id, global_user_id, pairwise_sub, revoked_at IS NULL AS active \
                 FROM zeroship.app_user_identities", &[],
            ).await.unwrap();
            assert_eq!(row.get::<_, String>("app_client_id"), fixture.client.client_id);
            assert_eq!(row.get::<_, String>("global_user_id"), fixture.user_id.as_str());
            assert_eq!(row.get::<_, String>("pairwise_sub"), pairwise);
            assert!(row.get::<_, bool>("active"));
            tx.execute("UPDATE zeroship.app_user_identities SET revoked_at = NOW()", &[]).await.unwrap();
        }
        let changed_issuer = MintFixture::issuer([63; 32]);
        assert_ne!(changed_issuer.pairwise_subject(&fixture.user_id, &fixture.client.sector_identifier), pairwise);
        let error = mint_access_token(&tx, &changed_issuer, &fixture.client, &fixture.user_id, &["openid".to_owned()], &proof).await.unwrap_err();
        assert_eq!(error.error, "server_error");
        let row = tx.query_one("SELECT pairwise_sub, revoked_at IS NOT NULL AS revoked FROM zeroship.app_user_identities", &[]).await.unwrap();
        assert_eq!(row.get::<_, String>("pairwise_sub"), pairwise);
        assert!(row.get::<_, bool>("revoked"));
        tx.commit().await.unwrap();
    }).await;
}

#[compio::test]
async fn inactive_principals_cannot_mint_with_a_previously_established_proof() {
    for transition in [
        "UPDATE zeroship.users SET disabled_at = NOW() WHERE id = $1",
        "UPDATE zeroship.users SET anonymized_at = NOW() WHERE id = $1",
        "UPDATE zeroship.users SET deletion_requested_at = NOW() WHERE id = $1",
        "UPDATE zeroship.users SET deletion_scheduled_for = NOW() WHERE id = $1",
    ] {
        Database::run(async |database| {
            let fixture = MintFixture::seed(database).await;
            let mut mint = database.connect_as_auth().await;
            let tx = mint.transaction().await.unwrap();
            let proof = fixture.proof(&tx).await;
            tx.execute(transition, &[&fixture.user_id.as_str()])
                .await
                .unwrap();
            let error = mint_access_token(
                &tx,
                &fixture.issuer,
                &fixture.client,
                &fixture.user_id,
                &["openid".to_owned()],
                &proof,
            )
            .await
            .unwrap_err();
            assert_eq!(error.error, "invalid_grant", "{transition}");
            assert!(tx
                .query(
                    "SELECT app_client_id FROM zeroship.app_user_identities",
                    &[]
                )
                .await
                .unwrap()
                .is_empty());
            tx.rollback().await.unwrap();
        })
        .await;
    }
}

#[compio::test]
async fn soft_password_lockout_does_not_invalidate_an_established_proof() {
    Database::run(async |database| {
        let fixture = MintFixture::seed(database).await;
        let mut mint = database.connect_as_auth().await;
        let tx = mint.transaction().await.unwrap();
        let proof = fixture.proof(&tx).await;
        tx.execute(
            "UPDATE zeroship.users SET locked_until = NOW() + INTERVAL '1 day' WHERE id = $1",
            &[&fixture.user_id.as_str()],
        )
        .await
        .unwrap();
        let token = mint_access_token(
            &tx,
            &fixture.issuer,
            &fixture.client,
            &fixture.user_id,
            &["openid".to_owned()],
            &proof,
        )
        .await
        .unwrap();
        assert!(token_iat(&token) > 0);
        tx.commit().await.unwrap();
    })
    .await;
}

#[compio::test]
async fn a_deleted_principal_cannot_establish_a_session() {
    Database::run(async |database| {
        let fixture = MintFixture::seed(database).await;
        let mut setup = database.connect_as_auth().await;
        users::request_deletion(
            &mut setup,
            &fixture.user_id,
            crate::cron::account_reaper::GRACE_DAYS,
        )
        .await
        .unwrap()
        .unwrap();
        let tx = setup.transaction().await.unwrap();
        let result = super::refresh::establish_session(
            &tx,
            &fixture.issuer,
            &fixture.keys,
            &super::refresh::Establish {
                client: &fixture.client,
                user_id: &fixture.user_id,
                granted_scopes: &["openid".to_owned()],
                auth_credential_version: 0,
                kind: super::SessionKind::Browser,
                with_secret: false,
            },
        )
        .await;
        assert!(result.is_err());
        assert!(tx
            .query("SELECT id FROM zeroship.sessions", &[])
            .await
            .unwrap()
            .is_empty());
        tx.rollback().await.unwrap();
    })
    .await;
}

#[compio::test]
async fn deletion_marker_uses_a_post_lock_timestamp() {
    Database::run(async |database| {
        let fixture = MintFixture::seed(database).await;
        let observer = database.connect().await;
        let mut mint = database.connect_as_auth().await;
        let mut deletion = database.connect_as_auth().await;
        let deletion_pid: i32 = deletion.query_one("SELECT pg_backend_pid()", &[]).await.unwrap().get(0);
        let tx = mint.transaction().await.unwrap();
        advisory_lock::lock_refresh_user_xact(&tx, &fixture.user_id).await.unwrap();
        let (deleted, issued_at) = futures::join!(
            users::request_deletion(&mut deletion, &fixture.user_id, crate::cron::account_reaper::GRACE_DAYS),
            async {
                assert!(database.wait_until_blocked(&[deletion_pid]).await);
                let started: i64 = observer.query_one(
                    "SELECT floor(extract(epoch FROM xact_start))::bigint FROM pg_stat_activity WHERE pid = $1", &[&deletion_pid],
                ).await.unwrap().get(0);
                // Wait until the issuer clock distinguishes the later mint
                // from the deletion transaction that is already waiting.
                compio::time::timeout(Duration::from_secs(5), async {
                    while chrono::Utc::now().timestamp() <= started {
                        compio::time::sleep(Duration::from_millis(25)).await;
                    }
                }).await.expect("issuer clock must advance beyond the waiting transaction");
                let proof = fixture.proof(&tx).await;
                let token = mint_access_token(&tx, &fixture.issuer, &fixture.client, &fixture.user_id, &["openid".to_owned()], &proof).await.unwrap();
                let issued_at = token_iat(&token);
                assert!(issued_at > started);
                tx.commit().await.unwrap();
                issued_at
            },
        );
        deleted.unwrap().unwrap();
        let pairwise = fixture.issuer.pairwise_subject(&fixture.user_id, &fixture.client.sector_identifier);
        let newer: bool = observer.query_one(
            "SELECT revoked_after > TIMESTAMPTZ 'epoch' + ($3::bigint * INTERVAL '1 second') \
             FROM zeroship.token_revocations WHERE client_id = $1 AND sub = $2",
            &[&fixture.client.client_id, &pairwise, &issued_at],
        ).await.unwrap().get(0);
        assert!(newer, "deletion must revoke the token minted while it waited");
    }).await;
}
