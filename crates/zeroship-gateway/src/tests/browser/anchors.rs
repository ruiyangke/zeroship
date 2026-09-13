//! Anchor controls and independent observations for browser revocation cases.

use super::*;
use crate::anchors;

pub async fn stored_anchor_ids(admin: &compio_postgres::Client) -> Vec<Uuid> {
    admin
        .query(
            "SELECT id FROM zeroship.app_session_anchors ORDER BY id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect()
}

pub async fn control_anchor(
    state: &GateState,
    app: &AppId,
    client: &str,
    user: &UserId,
    refresh: &str,
) -> Uuid {
    let encrypted = zeroship_core::crypto::encrypt(
        &state.anchor_enc_key,
        format!("zs-anchor-refresh:{client}:{}", user.as_str()).as_bytes(),
        refresh.as_bytes(),
    )
    .unwrap();
    let pool = crate::db::checkout(state.db.as_ref().unwrap())
        .await
        .unwrap();
    let mut connection = pool.acquire().await.unwrap();
    anchors::create(
        &mut connection,
        &anchors::NewAnchor {
            app_id: app,
            client_id: client,
            global_user_id: user,
            refresh_token_enc: &encrypted,
            refresh_family_id: refresh,
            granted_scopes: &["openid".to_owned()],
        },
    )
    .await
    .unwrap()
    .id
}
