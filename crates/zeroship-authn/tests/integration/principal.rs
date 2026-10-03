//! Principal eligibility through the public verifier and migrated control role.

#![allow(
    clippy::future_not_send,
    reason = "fixtures belong to their compio runtime"
)]

use crate::support::database::Database;
use compio_postgres::Client;
use std::{collections::HashSet, sync::Arc};
use zeroship_authn::{BearerVerifier, HttpRejection};
use zeroship_core::{
    auth_provider::{AuthProvider, PlatformConfig, PlatformProvider},
    UserId,
};

struct Principal {
    admin: Client,
    verifier: BearerVerifier,
    id: UserId,
}

impl Principal {
    async fn seed(database: &Database) -> Self {
        Self::seed_as(database, "zeroship_control").await
    }

    async fn seed_as(database: &Database, role: &str) -> Self {
        let admin = database.connect().await;
        let id = UserId::mint();
        admin
            .execute(
                "INSERT INTO zeroship.users (id, email, name) VALUES ($1, $2, 'Principal fixture')",
                &[&id.as_str(), &format!("{}@example.test", id.as_str())],
            )
            .await
            .unwrap();
        let provider = AuthProvider::platform(PlatformProvider::new(
            PlatformConfig::new("https://auth.example.test", None).unwrap(),
        ));
        let verifier = BearerVerifier::new(
            Arc::new(database.connect_as(role).await),
            Arc::new(provider),
            HashSet::new(),
            "zeroship".to_owned(),
        );
        Self {
            admin,
            verifier,
            id,
        }
    }
}

fn assert_refusal(error: &HttpRejection, status: u16, code: &str) {
    assert_eq!(error.as_response_error().status_code().as_u16(), status);
    assert_eq!(error.to_string(), code);
}

#[compio::test]
async fn hard_lifecycle_states_refuse_a_principal_and_recovery_restores_eligibility() {
    Database::run(async |database| {
        let fixture = Principal::seed(database).await;
        fixture
            .verifier
            .require_active_principal(&fixture.id)
            .await
            .unwrap();
        for column in [
            "disabled_at",
            "anonymized_at",
            "deletion_requested_at",
            "deletion_scheduled_for",
        ] {
            fixture
                .admin
                .execute(
                    &format!("UPDATE zeroship.users SET {column} = NOW() WHERE id = $1"),
                    &[&fixture.id.as_str()],
                )
                .await
                .unwrap();
            let error = fixture
                .verifier
                .require_active_principal(&fixture.id)
                .await
                .unwrap_err();
            assert_refusal(&error, 401, "principal_inactive");
            fixture
                .admin
                .execute(
                    &format!("UPDATE zeroship.users SET {column} = NULL WHERE id = $1"),
                    &[&fixture.id.as_str()],
                )
                .await
                .unwrap();
            fixture
                .verifier
                .require_active_principal(&fixture.id)
                .await
                .unwrap();
        }
    })
    .await;
}

#[compio::test]
async fn a_soft_password_lockout_preserves_eligibility_but_a_missing_principal_is_refused() {
    Database::run(async |database| {
        let fixture = Principal::seed(database).await;
        fixture
            .admin
            .execute(
                "UPDATE zeroship.users SET locked_until = NOW() + INTERVAL '1 day' WHERE id = $1",
                &[&fixture.id.as_str()],
            )
            .await
            .unwrap();
        fixture
            .verifier
            .require_active_principal(&fixture.id)
            .await
            .unwrap();
        assert_refusal(
            &fixture
                .verifier
                .require_active_principal(&UserId::mint())
                .await
                .unwrap_err(),
            401,
            "principal_inactive",
        );
        fixture
            .admin
            .execute(
                "DELETE FROM zeroship.users WHERE id = $1",
                &[&fixture.id.as_str()],
            )
            .await
            .unwrap();
        assert_refusal(
            &fixture
                .verifier
                .require_active_principal(&fixture.id)
                .await
                .unwrap_err(),
            401,
            "principal_inactive",
        );
    })
    .await;
}

#[compio::test]
async fn a_lookup_failure_is_a_server_error_and_the_same_principal_can_retry() {
    Database::run(async |database| {
        let admin = database.connect().await;
        // A role private to this case holds the grant the case revokes, so
        // revoking it cannot deny the lookups of every other case in the shared
        // server.
        let role = format!("authn_lookup_{}", UserId::mint().as_str());
        admin
            .batch_execute(&format!(
                "CREATE ROLE \"{role}\" LOGIN PASSWORD '{role}'"
            ))
            .await
            .unwrap();
        admin
            .batch_execute(&format!("GRANT USAGE ON SCHEMA zeroship TO \"{role}\""))
            .await
            .unwrap();
        admin
            .batch_execute(&format!("GRANT SELECT ON zeroship.users TO \"{role}\""))
            .await
            .unwrap();
        let fixture = Principal::seed_as(database, &role).await;
        fixture
            .verifier
            .require_active_principal(&fixture.id)
            .await
            .unwrap();
        fixture
            .admin
            .batch_execute(&format!(
                "REVOKE SELECT ON zeroship.users FROM \"{role}\""
            ))
            .await
            .unwrap();
        assert_refusal(
            &fixture
                .verifier
                .require_active_principal(&fixture.id)
                .await
                .unwrap_err(),
            500,
            "principal_eligibility_lookup_failed",
        );
        fixture
            .admin
            .batch_execute(&format!("GRANT SELECT ON zeroship.users TO \"{role}\""))
            .await
            .unwrap();
        fixture
            .verifier
            .require_active_principal(&fixture.id)
            .await
            .unwrap();
    })
    .await;
}
