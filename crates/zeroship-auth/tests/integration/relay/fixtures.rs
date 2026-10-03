use compio_postgres::Client;
use uuid::Uuid;
use zeroship_auth::store::relay;
use zeroship_core::UserId;

pub const RELAY_DOMAIN: &str = "relay.example.test";

/// A per-case real inbox. Each case seeds its own recipient so a shared
/// database cannot collide on `users.email` or leak another case's
/// suppression state into the delivery contract.
pub fn recipient_email(label: &str) -> String {
    format!("{label}-{}@example.test", Uuid::new_v4().simple())
}

pub struct Alias {
    pub user_id: UserId,
    pub client_id: String,
    pub email: String,
    pub inbox: String,
}

impl Alias {
    pub async fn seed(pg: &Client) -> Self {
        let user_id = UserId::mint();
        let client_id = zeroship_core::typed_id::generate("oac");
        let inbox = recipient_email("relay-recipient");
        pg.execute(
            "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
             VALUES ($1, $2::citext, 'Relay recipient', NOW())",
            &[&user_id.as_str(), &inbox],
        )
        .await
        .unwrap();
        pg.execute(
            "INSERT INTO zeroship.oauth_clients \
             (client_id, client_name, redirect_uris, scopes) \
             VALUES ($1, 'Relay application', $2, $3)",
            &[
                &client_id,
                &vec!["https://app.example.test/callback".to_owned()],
                &vec!["email".to_owned()],
            ],
        )
        .await
        .unwrap();
        pg.execute(
            "INSERT INTO zeroship.oauth_grants (user_id, client_id, granted_scopes) \
             VALUES ($1, $2, $3)",
            &[&user_id.as_str(), &client_id, &vec!["email".to_owned()]],
        )
        .await
        .unwrap();
        let subject = zeroship_core::auth::derive_pairwise(
            &zeroship_core::crypto::derive_key("relay-fixture-salt"),
            &user_id,
            "https://app.example.test",
        );
        pg.execute(
            "INSERT INTO zeroship.app_user_identities \
             (app_client_id, global_user_id, pairwise_sub) VALUES ($1, $2, $3)",
            &[&client_id, &user_id.as_str(), &subject],
        )
        .await
        .unwrap();
        let email = relay::mint_alias_at_consent(pg, &client_id, &user_id, RELAY_DOMAIN)
            .await
            .unwrap()
            .expect("mint alias for the established identity");
        Self {
            user_id,
            client_id,
            email,
            inbox,
        }
    }

    pub async fn granted_scopes(&self, pg: &Client) -> Vec<String> {
        pg.query_one(
            "SELECT granted_scopes FROM zeroship.oauth_grants \
             WHERE user_id = $1 AND client_id = $2",
            &[&self.user_id.as_str(), &self.client_id],
        )
        .await
        .unwrap()
        .get(0)
    }
}
