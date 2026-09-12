use zeroship_authz::Scope;
use zeroship_control::AppState;
use zeroship_core::UserId;

/// A seeded control-plane principal plus the OAuth access token that
/// authenticates it.
///
/// The bearer is a console-client platform token, not a first-party CLI one:
/// control narrows `zeroship-cli` scopes against the principal's live grant
/// rows, and not every surface these fixtures drive is in the CLI's issuable
/// set. See `common::CONSOLE_CLIENT_ID`.
pub struct SeededPrincipal {
    pub user_id: UserId,
    pub token: String,
}

impl SeededPrincipal {
    pub fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }

    pub async fn cleanup(&self, state: &AppState) {
        let _ = state
            .control_pg
            .execute(
                "DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1",
                &[&self.user_id.as_str()],
            )
            .await;
        let _ = state
            .control_pg
            .execute(
                "DELETE FROM zeroship.users WHERE id = $1",
                &[&self.user_id.as_str()],
            )
            .await;
    }
}

/// A creator holding EVERY OAuth scope.
///
/// This used to come in two flavours, `admin_principal` and
/// `non_admin_principal`, differing only in whether a `platform_admin_roles`
/// row was written. There is no platform role any more - the staff policies
/// were cross-tenant grants and are deleted - so the two collapsed into one:
/// every principal is a creator, authorized by the self-scoped baseline and by
/// whatever `app_members` rows it holds.
///
/// The scope set is the whole closed vocabulary on purpose. The wrapper policy
/// the bearer path derives is a NARROWING filter, so holding everything the
/// vocabulary can express leaves the Cedar decision as the only thing that can
/// refuse - which is what the callers of this fixture assert. One Cedar action
/// has no scope at all - `migrations:approve` - and no bearer can carry it; see
/// the note on `zeroship_authz::Action::AppsApproveMigration`.
///
/// `#[allow(dead_code)]`: `common` is shared across every control test crate;
/// only some of them use this, so the others would warn.
#[allow(dead_code)]
pub async fn seeded_principal(state: &AppState) -> SeededPrincipal {
    let user_id = UserId::mint();
    let email = format!("bearer-{}@zeroship.test", user_id.as_str());
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
             VALUES ($1, $2::citext, 'Bearer Test User', NOW())",
            &[&user_id.as_str(), &email],
        )
        .await
        .expect("insert bearer user");

    let token = super::platform_token_for_client(&user_id, &all_scopes(), super::CONSOLE_CLIENT_ID);
    SeededPrincipal { user_id, token }
}

fn all_scopes() -> String {
    Scope::ALL
        .iter()
        .map(|scope| scope.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}
