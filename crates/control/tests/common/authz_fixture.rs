use chrono::{Duration, Utc};
use uuid::Uuid;
use zeroship_authz::{policy_hash, Action, Effect, Policy, Resource, Statement};
use zeroship_control::AppState;

pub struct AdminPat {
    pub user_id: Uuid,
    pub token_id: Uuid,
    pub token: String,
}

impl AdminPat {
    pub fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }

    pub async fn cleanup(&self, state: &AppState) {
        let _ = state
            .auth_pg
            .execute(
                "DELETE FROM zeroship.authz_decisions WHERE token_id = $1 OR actor_user_id = $2",
                &[&self.token_id, &self.user_id],
            )
            .await;
        let _ = state
            .auth_pg
            .execute(
                "DELETE FROM zeroship.permission_tokens WHERE id = $1",
                &[&self.token_id],
            )
            .await;
        let _ = state
            .auth_pg
            .execute("DELETE FROM zeroship.platform_admin_roles WHERE user_id = $1", &[&self.user_id])
            .await;
        let _ = state
            .auth_pg
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[&self.user_id])
            .await;
    }
}

pub async fn admin_pat(state: &AppState) -> AdminPat {
    issue_pat(state, true, admin_policy()).await
}

/// A PAT owned by a NON-admin user with an empty wrapper policy. Used to
/// assert that the bearer principal path rejects an unauthorized actor — e.g.
/// a creator who is not a platform admin cannot grant platform roles. (Under
/// the R5 cutover the bespoke console-session principal path is gone; bearer is
/// the only principal, so the "non-admin actor" case is a non-admin PAT.)
///
/// `#[allow(dead_code)]`: `common` is shared across every control test crate;
/// only `admin_handlers_test` uses this, so the others would warn.
#[allow(dead_code)]
pub async fn non_admin_pat(state: &AppState) -> AdminPat {
    issue_pat(state, false, empty_policy()).await
}

async fn issue_pat(state: &AppState, admin: bool, policy: Policy) -> AdminPat {
    let user_id = Uuid::new_v4();
    let email = format!("pat-{user_id}@zeroship.test");
    state
        .auth_pg
        .execute(
            "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
             VALUES ($1, $2::citext, 'PAT Test User', NOW())",
            &[&user_id, &email],
        )
        .await
        .expect("insert PAT user");
    if admin {
        state
            .auth_pg
            .execute(
                "INSERT INTO zeroship.platform_admin_roles (user_id, role, granted_by) \
                 VALUES ($1, 'admin', $1)",
                &[&user_id],
            )
            .await
            .expect("insert admin PAT role");
    }

    let token_id = Uuid::new_v4();
    let policies = policy.to_json_value();
    let hash = policy_hash(&policies);
    let expires_at = Utc::now() + Duration::days(1);
    let token = state
        .pat_issuer
        .issue(token_id, user_id, hash.clone(), expires_at)
        .expect("issue PAT");

    state
        .auth_pg
        .execute(
            "INSERT INTO zeroship.permission_tokens \
                (id, owner_id, kind, name, policies, policy_hash, expires_at) \
             VALUES ($1, $2, 'pat', 'integration PAT', $3, $4, $5)",
            &[&token_id, &user_id, &policies, &hash, &expires_at],
        )
        .await
        .expect("insert PAT row");

    AdminPat {
        user_id,
        token_id,
        token,
    }
}

#[allow(dead_code)]
fn empty_policy() -> Policy {
    Policy {
        name: "integration non-admin".to_owned(),
        statements: Vec::new(),
    }
}

fn admin_policy() -> Policy {
    Policy {
        name: "integration admin".to_owned(),
        statements: vec![Statement {
            effect: Effect::Allow,
            actions: vec![
                Action::AppsRead,
                Action::AppsWrite,
                Action::AppsDeploy,
                Action::AppsDelete,
                Action::DeploymentsRead,
                Action::DeploymentsRollback,
                Action::EnvRead,
                Action::EnvWrite,
                Action::SecretsRead,
                Action::SecretsWrite,
                Action::BillingRead,
                Action::BillingWrite,
                Action::TeamRead,
                Action::TeamWrite,
                Action::AccountRead,
                Action::AccountWrite,
                Action::PlatformPoliciesWrite,
            ],
            resources: vec![Resource::Any],
            conditions: Vec::new(),
        }],
    }
}
