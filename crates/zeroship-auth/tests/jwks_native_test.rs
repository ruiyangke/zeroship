//! The public key registry is read through auth's native ORM.

#![allow(
    clippy::future_not_send,
    reason = "the ORM belongs to this compio runtime"
)]

use crate::common::{auth_server::AuthServer, database::Database};
use serde_json::{json, Value};
use zeroship_auth::oidc::metadata::jwks_document;
use zeroship_data_orm::sql::{MAX_ROW_LIMIT, MAX_ROW_OFFSET};

#[compio::test]
async fn native_jwks_preserves_complete_sql_order_and_public_fields() {
    Database::run(async |database| {
        let db = database.connect_as_auth().await;
        let orm = database.orm().await;
        assert_eq!(jwks_document(&orm).await.unwrap(), json!({ "keys": [] }));

        let registry_size = MAX_ROW_OFFSET + MAX_ROW_LIMIT + 1;
        let public_jwk = json!({
            "kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "use": "sig",
            "kid": "untrusted-embedded-id", "x": "public-key", "d": "private-key",
            "private_key": "must-not-be-published"
        });
        db.execute(
            "INSERT INTO zeroship.signing_keys (kid, alg, public_jwk, status, created_at) \
             SELECT 'key-' || lpad(n::text, 8, '0'), 'EdDSA', $1, \
                    CASE n % 3 WHEN 0 THEN 'active' WHEN 1 THEN 'next' ELSE 'retiring' END, \
                    TIMESTAMPTZ '2020-01-01 00:00:00+00' + (n % 7) * INTERVAL '1 microsecond' \
             FROM generate_series(1::bigint, $2::bigint) AS n",
            &[&public_jwk, &registry_size],
        )
        .await
        .unwrap();
        db.execute(
            "INSERT INTO zeroship.signing_keys (kid, alg, public_jwk, status) \
             VALUES ('retired-invalid', 'EdDSA', '[]', 'retired')",
            &[],
        )
        .await
        .unwrap();
        let expected: Vec<String> = db
            .query(
                "SELECT kid FROM zeroship.signing_keys \
                 WHERE status IN ('active', 'next', 'retiring') \
                 ORDER BY CASE status WHEN 'active' THEN 0 WHEN 'next' THEN 1 ELSE 2 END, \
                          created_at DESC, kid ASC",
                &[],
            )
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.get("kid"))
            .collect();
        assert_eq!(expected.len(), usize::try_from(registry_size).unwrap());

        let document = jwks_document(&orm).await.unwrap();
        let keys = document["keys"].as_array().unwrap();
        let actual: Vec<_> = keys
            .iter()
            .map(|key| key["kid"].as_str().unwrap())
            .collect();
        assert_eq!(actual, expected);
        for key in keys {
            assert_eq!(key["x"], "public-key");
            assert!(key.get("d").is_none());
            assert!(key.get("private_key").is_none());
            assert_eq!(key.as_object().unwrap().len(), 6);
        }
    })
    .await;
}

#[compio::test]
async fn native_jwks_rejects_malformed_published_keys() {
    Database::run(async |database| {
        let db = database.connect_as_auth().await;
        let orm = database.orm().await;
        db.execute(
            "INSERT INTO zeroship.signing_keys (kid, alg, public_jwk, status) \
             VALUES ('invalid', 'EdDSA', '[]', 'active')",
            &[],
        )
        .await
        .unwrap();
        assert!(jwks_document(&orm).await.is_err());
        for invalid in [json!({}), Value::Null] {
            db.execute(
                "UPDATE zeroship.signing_keys SET public_jwk = $1 WHERE kid = 'invalid'",
                &[&invalid],
            )
            .await
            .unwrap();
            assert!(jwks_document(&orm).await.is_err());
        }
        db.execute(
            "UPDATE zeroship.signing_keys SET status = 'retired' WHERE kid = 'invalid'",
            &[],
        )
        .await
        .unwrap();
        assert_eq!(jwks_document(&orm).await.unwrap(), json!({ "keys": [] }));
    })
    .await;
}

#[ntex::test]
async fn native_jwks_failure_is_uncacheable() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        server
            .pg
            .execute(
                "INSERT INTO zeroship.signing_keys (kid, alg, public_jwk, status) \
                 VALUES ('malformed', 'EdDSA', '{}', 'active')",
                &[],
            )
            .await
            .unwrap();
        let response = server
            .http
            .get(format!("{}/oauth2/.well-known/jwks.json", server.auth_base))
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 503);
        assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
    })
    .await;
}
