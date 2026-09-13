use super::super::{load_client, refresh, Issuer, OAuthClient, SessionKind, ValidatedSession};
use crate::{session_store::SessionSecretKeys, store::users, test_database::Database};
use compio_postgres::Transaction;
use zeroship_core::UserId;

pub struct MintFixture {
    pub user_id: UserId,
    pub client: OAuthClient,
    pub issuer: Issuer,
    pub keys: SessionSecretKeys,
    _key_directory: tempfile::TempDir,
}

impl MintFixture {
    pub async fn seed(database: &Database) -> Self {
        let setup = database.connect().await;
        let user = users::create(&setup, "recipient@example.test", "Mint subject", None)
            .await
            .unwrap();
        let client_id = zeroship_core::typed_id::generate("oac");
        setup
            .execute(
                "INSERT INTO zeroship.oauth_clients \
             (client_id, client_name, redirect_uris, scopes, token_endpoint_auth_method) \
             VALUES ($1, 'Mint application', $2, $3, 'none')",
                &[
                    &client_id,
                    &vec!["https://app.example.test/callback".to_owned()],
                    &vec!["openid".to_owned()],
                ],
            )
            .await
            .unwrap();
        let client = load_client(&setup, &client_id).await.unwrap();
        let issuer = Self::issuer([62; 32]);
        let auth = database.connect_as_auth().await;
        issuer.publish_active_key(&auth).await.unwrap();
        let key_directory = tempfile::tempdir().unwrap();
        let hash_path = key_directory.path().join("hash");
        let idem_path = key_directory.path().join("idem");
        for (path, body) in [
            (
                &hash_path,
                "1:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff\n",
            ),
            (&idem_path, "mint-fixture-idempotency-secret"),
        ] {
            std::fs::write(path, body).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
        }
        let keys = SessionSecretKeys::from_files(&hash_path, &idem_path).unwrap();
        Self {
            user_id: user.id,
            client,
            issuer,
            keys,
            _key_directory: key_directory,
        }
    }

    pub fn issuer(pairwise_salt: [u8; 32]) -> Issuer {
        Issuer::from_signing_key(
            &ed25519_dalek::SigningKey::from_bytes(&[61; 32]),
            pairwise_salt,
            "https://auth.example.test".to_owned(),
        )
        .unwrap()
    }

    pub async fn proof(&self, tx: &Transaction<'_>) -> ValidatedSession {
        refresh::establish_session(
            tx,
            &self.issuer,
            &self.keys,
            &refresh::Establish {
                client: &self.client,
                user_id: &self.user_id,
                granted_scopes: &["openid".to_owned()],
                auth_credential_version: 0,
                kind: SessionKind::Browser,
                with_secret: false,
            },
        )
        .await
        .unwrap()
        .proof
    }
}
