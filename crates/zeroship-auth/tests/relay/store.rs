use super::fixtures::Alias;
use crate::common::database::Database;
use zeroship_auth::store::relay;

#[compio::test]
async fn probing_is_read_only_and_terminal_commit_is_idempotent() {
    Database::run(async |database| {
        let pg = database.connect_as_auth().await;
        let message_id = "incoming-message";
        assert!(!relay::already_seen(&pg, message_id).await.unwrap());
        assert!(!relay::already_seen(&pg, message_id).await.unwrap());
        relay::commit_seen(&pg, message_id).await.unwrap();
        assert!(relay::already_seen(&pg, message_id).await.unwrap());
        relay::commit_seen(&pg, message_id).await.unwrap();
        assert!(relay::already_seen(&pg, message_id).await.unwrap());
        assert!(!relay::already_seen(&pg, "another-message").await.unwrap());
    })
    .await;
}

#[compio::test]
async fn local_revocation_disables_the_alias_and_preserves_the_grant() {
    Database::run(async |database| {
        let admin = database.connect().await;
        let alias = Alias::seed(&admin).await;
        let pg = database.connect_as_auth().await;
        let grant = alias.granted_scopes(&pg).await;
        let target = relay::resolve_active_alias(&pg, &alias.email)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(target.real_inbox, alias.inbox);
        assert_eq!(target.global_user_id, alias.user_id);
        assert_eq!(target.app_client_id, alias.client_id);

        assert_eq!(
            relay::revoke_local_alias(&pg, &alias.client_id, &alias.user_id)
                .await
                .unwrap(),
            1
        );
        assert!(
            relay::resolve_active_alias(&pg, &alias.email)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            relay::revoke_local_alias(&pg, &alias.client_id, &alias.user_id)
                .await
                .unwrap(),
            0
        );
        assert_eq!(alias.granted_scopes(&pg).await, grant);
    })
    .await;
}
