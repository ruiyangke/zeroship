//! Revocation decisions through the router, using the migrated gateway role.

#![allow(
    clippy::future_not_send,
    reason = "router fixtures stay on their compio runtime"
)]

use super::*;
use crate::db::tests::postgres::Database;
use std::sync::Arc;
use std::time::{Duration, Instant};
use zeroship_authz::wrapper_revocation::{revoke_family, RevocationCache};

const CACHE_TTL: u64 = 600;

struct CookieFixture {
    state: Arc<crate::GateState>,
    request: ntex::web::HttpRequest,
    client_id: String,
    subject: String,
}

impl CookieFixture {
    fn new(database: &Database) -> Self {
        let mut state = build_state_with_session_and_auth_ui_url_and_db(
            ed25519_dalek::SigningKey::from_bytes(&[7; 32]),
            "https://auth.zeroship.test",
            Some(database.config_as("zeroship_gateway", 4)),
        );
        Arc::get_mut(&mut state).unwrap().revocation_cache =
            Arc::new(RevocationCache::with_ttl_and_capacity(CACHE_TTL, 16));
        let (client_id, _) = op_app_binding(OP_APP_A);
        let subject = zeroship_core::auth::derive_pairwise(
            &state.pairwise_salt,
            &zeroship_core::user_id::UserId::mint(),
            "https://myapp.zeroship.ai",
        );
        let token = issue_signed_session_cookie(&state, &client_id, &subject, "", &[]);
        let request = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, "myapp.zeroship.ai")
            .header(
                "cookie",
                format!("{}={token}", oidc_rp::app_session_cookie_name()),
            )
            .to_http_request();
        Self {
            state,
            request,
            client_id,
            subject,
        }
    }

    async fn resolve(&self) -> CookieOutcome {
        resolve_app_session_user_header_inner(
            &self.request,
            &self.state,
            &Uuid::new_v4(),
            Some(&self.client_id),
        )
        .await
    }

    async fn revoke(&self) {
        let pool = crate::db::checkout(self.state.db.as_ref().unwrap())
            .await
            .unwrap();
        let connection = pool.acquire().await.unwrap();
        revoke_family(&connection, &self.client_id, &self.subject)
            .await
            .unwrap();
    }

    fn expire_negative_entry(&self) {
        self.state.revocation_cache.store(
            &self.client_id,
            &self.subject,
            None,
            Instant::now()
                .checked_sub(Duration::from_secs(CACHE_TTL + 1))
                .expect("clock supports an expired fixture entry"),
        );
    }

    fn invalidate(&self) {
        self.state
            .revocation_cache
            .invalidate(&self.client_id, &self.subject);
    }
}

#[compio::test]
async fn cookie_arm_rejects_revoked_family_statelessly() {
    Database::migrated(async |database| {
        let fixture = CookieFixture::new(database);
        assert!(matches!(fixture.resolve().await, CookieOutcome::Allowed(_)));
        fixture.revoke().await;
        fixture.invalidate();
        assert!(matches!(fixture.resolve().await, CookieOutcome::None));
        let unrelated = CookieFixture::new(database);
        assert!(
            matches!(unrelated.resolve().await, CookieOutcome::Allowed(_)),
            "another principal was revoked"
        );
    })
    .await;
}

#[compio::test]
async fn revocation_cache_honors_revocation_after_ttl_expiry() {
    Database::migrated(async |database| {
        let fixture = CookieFixture::new(database);
        assert!(matches!(fixture.resolve().await, CookieOutcome::Allowed(_)));
        fixture.revoke().await;
        assert!(
            matches!(fixture.resolve().await, CookieOutcome::Allowed(_)),
            "a fresh cache entry was bypassed"
        );
        fixture.expire_negative_entry();
        assert!(
            matches!(fixture.resolve().await, CookieOutcome::None),
            "the expired cache hid a revocation"
        );
    })
    .await;
}

#[compio::test]
async fn revocation_cache_negative_entry_serves_without_db_within_ttl() {
    Database::migrated(async |database| {
        let fixture = CookieFixture::new(database);
        assert!(matches!(fixture.resolve().await, CookieOutcome::Allowed(_)));
        assert_eq!(
            fixture.state.revocation_cache.get(
                &fixture.client_id,
                &fixture.subject,
                Instant::now()
            ),
            Some(None)
        );
        database
            .admin
            .batch_execute(
                "ALTER TABLE zeroship.token_revocations RENAME TO unavailable_revocations",
            )
            .await
            .unwrap();
        assert!(
            matches!(fixture.resolve().await, CookieOutcome::Allowed(_)),
            "fresh negative caching still contacted the database"
        );
        fixture.expire_negative_entry();
        assert!(
            matches!(fixture.resolve().await, CookieOutcome::None),
            "an expired entry hid a database failure"
        );
        database
            .admin
            .batch_execute(
                "ALTER TABLE zeroship.unavailable_revocations RENAME TO token_revocations",
            )
            .await
            .unwrap();
        assert!(
            matches!(fixture.resolve().await, CookieOutcome::Allowed(_)),
            "a failed lookup poisoned later reads"
        );
    })
    .await;
}

#[compio::test]
async fn revocation_cache_same_node_bust_takes_effect_immediately() {
    Database::migrated(async |database| {
        let fixture = CookieFixture::new(database);
        assert!(matches!(fixture.resolve().await, CookieOutcome::Allowed(_)));
        fixture.revoke().await;
        assert!(
            matches!(fixture.resolve().await, CookieOutcome::Allowed(_)),
            "the negative entry must remain fresh before invalidation"
        );
        fixture.invalidate();
        assert!(
            matches!(fixture.resolve().await, CookieOutcome::None),
            "invalidation did not reload the revocation"
        );
    })
    .await;
}

#[compio::test]
async fn revocation_cache_miss_plus_db_error_fails_closed() {
    Database::migrated(async |database| {
        let fixture = CookieFixture::new(database);
        database
            .admin
            .batch_execute(
                "ALTER TABLE zeroship.token_revocations RENAME TO unavailable_revocations",
            )
            .await
            .unwrap();
        assert!(matches!(fixture.resolve().await, CookieOutcome::None));
        assert_eq!(
            fixture.state.revocation_cache.get(
                &fixture.client_id,
                &fixture.subject,
                Instant::now()
            ),
            None
        );
        database
            .admin
            .batch_execute(
                "ALTER TABLE zeroship.unavailable_revocations RENAME TO token_revocations",
            )
            .await
            .unwrap();
        assert!(matches!(fixture.resolve().await, CookieOutcome::Allowed(_)));
    })
    .await;
}

struct BearerFixture {
    state: Arc<crate::GateState>,
    signing: ed25519_dalek::SigningKey,
    user: zeroship_core::user_id::UserId,
    _server: ntex::web::test::TestServer,
}

struct AppBearer {
    client_id: String,
    subject: String,
    sector: String,
    request: ntex::web::HttpRequest,
}

impl BearerFixture {
    async fn new(database: &Database) -> Self {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[55; 32]);
        let server = start_jwks_server(op_jwks_doc(&signing)).await;
        let base = server.url("").trim_end_matches('/').to_string();
        let oidc_rp = crate::oidc_rp::OidcRp::new(
            &base,
            test_broker_secret(),
            b"test-stash-key-32-bytes-long----".to_vec(),
        )
        .with_issuer(OP_ISS);
        let state = build_state_with_session_and_oidc_and_db(
            ed25519_dalek::SigningKey::from_bytes(&[7; 32]),
            oidc_rp,
            Some(database.config_as("zeroship_gateway", 4)),
        );
        Self {
            state,
            signing,
            user: zeroship_core::user_id::UserId::mint(),
            _server: server,
        }
    }

    fn token(&self, app: &str, host: &str) -> AppBearer {
        let (client_id, audience) = op_app_binding(app);
        let sector = format!("https://{host}");
        let subject =
            zeroship_core::auth::derive_pairwise(&self.state.pairwise_salt, &self.user, &sector);
        let token = sign_op_access_jwt(
            &self.signing,
            &subject,
            Some(&client_id),
            serde_json::json!([audience]),
            "user@example.com",
            "OP User",
            3600,
        );
        AppBearer {
            request: bearer_req(&token, host),
            client_id,
            subject,
            sector,
        }
    }

    async fn resolve(&self, bearer: &AppBearer) -> BearerOutcome {
        resolve_bearer_user_header(
            &bearer.request,
            &self.state,
            &Uuid::new_v4(),
            Some(&bearer.client_id),
            Some(&bearer.sector),
        )
        .await
    }

    async fn revoke(&self, bearer: &AppBearer) {
        let pool = crate::db::checkout(self.state.db.as_ref().unwrap())
            .await
            .unwrap();
        let connection = pool.acquire().await.unwrap();
        revoke_family(&connection, &bearer.client_id, &bearer.subject)
            .await
            .unwrap();
        self.state
            .revocation_cache
            .invalidate(&bearer.client_id, &bearer.subject);
    }
}

#[ntex::test]
async fn bearer_raw_op_revocation_is_per_app_not_global() {
    Database::migrated(async |database| {
        let fixture = BearerFixture::new(database).await;
        let app_a = fixture.token(OP_APP_A, "app-a.zeroship.ai");
        let app_b = fixture.token(OP_APP_B, "app-b.zeroship.ai");
        assert_ne!(app_a.client_id, app_b.client_id);
        assert_ne!(app_a.subject, app_b.subject);
        assert!(matches!(
            fixture.resolve(&app_a).await,
            BearerOutcome::Allowed(_)
        ));
        assert!(matches!(
            fixture.resolve(&app_b).await,
            BearerOutcome::Allowed(_)
        ));
        fixture.revoke(&app_a).await;
        assert!(matches!(
            fixture.resolve(&app_a).await,
            BearerOutcome::Invalid
        ));
        assert!(
            matches!(fixture.resolve(&app_b).await, BearerOutcome::Allowed(_)),
            "another app's family was revoked"
        );
    })
    .await;
}

#[ntex::test]
async fn revocation_keyed_on_pws_rejects_raw_op_bearer() {
    Database::migrated(async |database| {
        let fixture = BearerFixture::new(database).await;
        let bearer = fixture.token(OP_APP_A, "myapp.zeroship.ai");
        assert!(
            matches!(fixture.resolve(&bearer).await, BearerOutcome::Allowed(_)),
            "the token must be valid before its family is revoked"
        );
        fixture.revoke(&bearer).await;
        assert!(matches!(
            fixture.resolve(&bearer).await,
            BearerOutcome::Invalid
        ));
        database
            .admin
            .execute(
                "DELETE FROM zeroship.token_revocations WHERE client_id = $1 AND sub = $2",
                &[&bearer.client_id, &bearer.subject],
            )
            .await
            .unwrap();
        fixture
            .state
            .revocation_cache
            .invalidate(&bearer.client_id, &bearer.subject);
        assert!(
            matches!(fixture.resolve(&bearer).await, BearerOutcome::Allowed(_)),
            "removing the marker must restore the same token"
        );
    })
    .await;
}
