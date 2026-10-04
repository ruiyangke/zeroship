//! Anchor controls and independent observations for browser revocation cases.

use super::*;
use crate::anchors;

/// The anchors this case owns, scoped to the app ids it minted so cases
/// sharing the migrated database never observe each other's rows.
pub async fn stored_anchor_ids(admin: &compio_postgres::Client, apps: &[AppId]) -> Vec<Uuid> {
    let apps: Vec<String> = apps.iter().map(|app| app.as_str().to_owned()).collect();
    admin
        .query(
            "SELECT id FROM zeroship.app_session_anchors WHERE app_id = ANY($1::text[]) ORDER BY id",
            &[&apps],
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
