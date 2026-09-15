//! The public key registry is read through auth's native ORM.

#![allow(
    clippy::future_not_send,
    reason = "the ORM belongs to this compio runtime"
)]

use crate::common::{
    auth_server::AuthServer,
    database::{eventually, Database},
};
use compio_postgres::Client;
use serde_json::{json, Value};
use std::collections::HashSet;
use zeroship_auth::oidc::metadata::jwks_document;
use zeroship_data_orm::sql::{MAX_ROW_LIMIT, MAX_ROW_OFFSET};

/// Advisory lock the fixture holds while a registry reader waits in its first page.
const PAGE_GATE: i64 = 41_002;

/// Make the auth role's registry reads wait while the fixture holds the page gate.
///
/// The policy runs while a page scans, after the page took its snapshot. The
/// platform grants the auth role `BYPASSRLS`, so the case withdraws it for the
/// policy to apply; the policy admits every row, so the rows each read sees are
/// unchanged. The superuser fixture connections bypass it.
async fn install_page_gate(admin: &Client) {
    admin
        .batch_execute(&format!(
            "CREATE SCHEMA fixture; \
             CREATE FUNCTION fixture.pass_page_gate() RETURNS boolean \
             LANGUAGE plpgsql VOLATILE AS $$ \
             BEGIN PERFORM pg_advisory_xact_lock_shared({PAGE_GATE}); RETURN true; END $$; \
             ALTER TABLE zeroship.signing_keys ENABLE ROW LEVEL SECURITY; \
             ALTER TABLE zeroship.signing_keys FORCE ROW LEVEL SECURITY; \
             CREATE POLICY page_gate ON zeroship.signing_keys FOR SELECT \
             USING (fixture.pass_page_gate()); \
             ALTER ROLE zeroship_auth NOBYPASSRLS"
        ))
        .await
        .unwrap();
}

/// Insert published keys whose creation times rise with their suffix, starting
/// `offset` seconds into the fixture epoch.
async fn insert_keys(admin: &Client, prefix: &str, status: &str, count: i64, offset: i64) {
    let public_jwk = json!({
        "kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "use": "sig", "x": "public-key"
    });
    admin
        .execute(
            "INSERT INTO zeroship.signing_keys (kid, alg, public_jwk, status, created_at) \
             SELECT $1 || lpad(n::text, 6, '0'), 'EdDSA', $2, $3, \
                    TIMESTAMPTZ '2020-01-01 00:00:00+00' + ($4 + n) * INTERVAL '1 second' \
             FROM generate_series(1::bigint, $5::bigint) AS n",
            &[&prefix, &public_jwk, &status, &offset, &count],
        )
        .await
        .unwrap();
}

/// Published kids in document order, read past the page gate.
async fn published_kids(admin: &Client) -> Vec<String> {
    admin
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
        .collect()
}

fn document_kids(document: &Value) -> Vec<String> {
    document["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|key| key["kid"].as_str().unwrap().to_owned())
        .collect()
}

/// A JWKS read that ran while the fixture committed a registry change.
struct GatedRead {
    document: Value,
    /// Whether the reader was observed waiting at the page gate.
    paused: bool,
    /// Rows the change updated while the reader waited.
    changed_rows: u64,
}

impl GatedRead {
    /// Require that the change committed while the reader waited in its first page.
    #[track_caller]
    fn assert_change_committed_inside_the_read(&self) {
        assert!(self.paused, "the reader must wait inside its first page");
        assert_eq!(self.changed_rows, 1, "the change must update one key");
    }
}

/// Read the JWKS document while `change` commits under a reader that waits
/// inside its first page.
#[allow(
    clippy::future_not_send,
    reason = "the ORM belongs to this compio runtime"
)]
async fn read_across_change(
    database: &Database,
    orm: &zeroship_data_orm::Database,
    change: &str,
    kid: &str,
) -> GatedRead {
    let gate = database.connect().await;
    gate.query_one("SELECT pg_advisory_lock($1)", &[&PAGE_GATE])
        .await
        .unwrap();
    let writer = database.connect().await;
    let change_between_pages = async {
        let paused = eventually(async || {
            writer
                .query_one(
                    "SELECT count(*) = 1 FROM pg_stat_activity \
                     WHERE datname = current_database() AND usename = 'zeroship_auth' \
                       AND wait_event_type = 'Lock' AND wait_event = 'advisory'",
                    &[],
                )
                .await
                .unwrap()
                .get(0)
        })
        .await;
        let changed_rows = if paused {
            writer.execute(change, &[&kid]).await.unwrap()
        } else {
            0
        };
        gate.query_one("SELECT pg_advisory_unlock($1)", &[&PAGE_GATE])
            .await
            .unwrap();
        (paused, changed_rows)
    };
    let (document, (paused, changed_rows)) =
        futures::join!(jwks_document(orm), change_between_pages);
    GatedRead {
        document: document.expect("read the published registry"),
        paused,
        changed_rows,
    }
}

/// The document must be the registry as it stood when the read began.
#[track_caller]
fn assert_snapshot(document: &[String], snapshot: &[String]) {
    let mut seen = HashSet::new();
    let repeated: Vec<_> = document
        .iter()
        .filter(|kid| !seen.insert(kid.as_str()))
        .collect();
    let dropped: Vec<_> = snapshot
        .iter()
        .filter(|kid| !seen.contains(kid.as_str()))
        .collect();
    assert!(
        repeated.is_empty(),
        "{} keys repeated across pages, first {:?}",
        repeated.len(),
        repeated.first()
    );
    assert!(dropped.is_empty(), "keys dropped across pages: {dropped:?}");
    assert_eq!(document, snapshot);
}

#[compio::test]
async fn native_jwks_keeps_a_key_promoted_while_the_reader_is_between_pages() {
    Database::run(async |database| {
        let admin = database.connect().await;
        let orm = database.orm().await;
        let page = usize::try_from(MAX_ROW_LIMIT).unwrap();
        insert_keys(&admin, "active-", "active", MAX_ROW_LIMIT, 0).await;
        insert_keys(&admin, "retiring-", "retiring", 2, 0).await;
        // The pre-published key is the newest, so promotion sorts it first.
        insert_keys(&admin, "promoted-", "next", 1, MAX_ROW_LIMIT).await;
        let promoted = "promoted-000001";
        let snapshot = published_kids(&admin).await;
        let cursor = snapshot[page - 1].clone();
        assert!(
            snapshot.len() > page,
            "the registry spans more than one page"
        );
        assert!(
            snapshot.iter().position(|kid| kid == promoted).unwrap() >= page,
            "the promoted key starts beyond the first page"
        );
        install_page_gate(&admin).await;

        let read = read_across_change(
            database,
            &orm,
            "UPDATE zeroship.signing_keys SET status = 'active', activated_at = clock_timestamp() \
             WHERE kid = $1",
            promoted,
        )
        .await;

        read.assert_change_committed_inside_the_read();
        let changed = published_kids(&admin).await;
        let position = |kid: &str| changed.iter().position(|key| key == kid).unwrap();
        assert!(
            position(promoted) < position(&cursor),
            "promotion moves the key ahead of the first page's cursor"
        );
        assert_eq!(
            document_kids(&jwks_document(&orm).await.unwrap()),
            changed,
            "a read that starts after the promotion publishes it"
        );
        assert_snapshot(&document_kids(&read.document), &snapshot);
    })
    .await;
}

#[compio::test]
async fn native_jwks_does_not_repeat_keys_when_the_cursor_key_retires_between_pages() {
    Database::run(async |database| {
        let admin = database.connect().await;
        let orm = database.orm().await;
        let page = usize::try_from(MAX_ROW_LIMIT).unwrap();
        insert_keys(&admin, "active-", "active", 1, 0).await;
        insert_keys(&admin, "retiring-", "retiring", MAX_ROW_LIMIT + 1, 0).await;
        let snapshot = published_kids(&admin).await;
        let cursor = snapshot[page - 1].clone();
        assert!(
            snapshot.len() > page,
            "the registry spans more than one page"
        );
        assert!(
            cursor.starts_with("retiring-") && snapshot[page - 2].starts_with("retiring-"),
            "the first page ends inside the retiring keys"
        );
        install_page_gate(&admin).await;

        let read = read_across_change(
            database,
            &orm,
            "UPDATE zeroship.signing_keys SET status = 'retired', retired_at = clock_timestamp() \
             WHERE kid = $1",
            &cursor,
        )
        .await;

        read.assert_change_committed_inside_the_read();
        let changed = published_kids(&admin).await;
        assert!(
            !changed.contains(&cursor),
            "retirement unpublishes the first page's cursor"
        );
        assert_eq!(
            document_kids(&jwks_document(&orm).await.unwrap()),
            changed,
            "a read that starts after the retirement omits the key"
        );
        assert_snapshot(&document_kids(&read.document), &snapshot);
    })
    .await;
}

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
