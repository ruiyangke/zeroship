use uuid::Uuid;
use zeroship_authz::Scope;
use zeroship_control::AppState;

/// A seeded control-plane principal plus the OAuth access token that
/// authenticates it.
///
/// The bearer is a console-client platform token, not a first-party CLI one:
/// control narrows `zeroship-cli` scopes against the principal's live grant
/// rows, and the operator surfaces these fixtures drive are outside the CLI's
/// issuable set. See `common::CONSOLE_CLIENT_ID`.
pub struct AdminPrincipal {
    pub user_id: Uuid,
    pub token: String,
}

impl AdminPrincipal {
    pub fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }

    pub async fn cleanup(&self, state: &AppState) {
        let _ = state
            .control_pg
            .execute(
                "DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1",
                &[&self.user_id],
            )
            .await;
        let _ = state
            .control_pg
            .execute("DELETE FROM zeroship.platform_admin_roles WHERE user_id = $1", &[&self.user_id])
            .await;
        let _ = state
            .control_pg
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[&self.user_id])
            .await;
    }
}

/// A platform admin holding EVERY OAuth scope.
///
/// The scope set is the whole closed vocabulary, so the wrapper policy the
/// bearer path derives never narrows the platform role away. Two Cedar actions
/// have no scope at all - `migrations:approve` and `platform_policies:write` -
/// and no bearer can carry them; see the note on `zeroship_authz::Action`.
pub async fn admin_principal(state: &AppState) -> AdminPrincipal {
    seed_principal(state, true).await
}

/// A creator with NO platform role, holding the same full scope set.
///
/// Same scopes as [`admin_principal`] on purpose: the wrapper policy is a
/// narrowing filter, so an empty one would 403 every request and prove nothing
/// about the role. Holding everything the vocabulary can express makes the
/// Cedar role check the only thing that can refuse - which is what the callers
/// of this fixture assert (a creator who is not a platform admin cannot grant
/// platform roles; a creator may set up their OWN billing but not another's).
///
/// `#[allow(dead_code)]`: `common` is shared across every control test crate;
/// only some of them use this, so the others would warn.
#[allow(dead_code)]
pub async fn non_admin_principal(state: &AppState) -> AdminPrincipal {
    seed_principal(state, false).await
}

fn all_scopes() -> String {
    Scope::ALL
        .iter()
        .map(|scope| scope.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

async fn seed_principal(state: &AppState, admin: bool) -> AdminPrincipal {
    let user_id = Uuid::new_v4();
    let email = format!("bearer-{user_id}@zeroship.test");
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
             VALUES ($1, $2::citext, 'Bearer Test User', NOW())",
            &[&user_id, &email],
        )
        .await
        .expect("insert bearer user");
    if admin {
        state
            .control_pg
            .execute(
                "INSERT INTO zeroship.platform_admin_roles (user_id, role, granted_by) \
                 VALUES ($1, 'admin', $1)",
                &[&user_id],
            )
            .await
            .expect("insert admin role");
    }

    AdminPrincipal {
        user_id,
        token: super::platform_token_for_client(user_id, &all_scopes(), super::CONSOLE_CLIENT_ID),
    }
}
