use compio_postgres::{connect, Client, NoTls};
use std::future::Future;
use uuid::Uuid;
use zeroship_authz::{
    enforce, is_authorized_anywhere, load_platform_policies, policy_hash, Action, AuthzContext,
    AuthzDecision, Condition, Effect, EntityCache, Policy, Resource, Statement,
};

#[test]
fn owner_authorized_token_authorized_returns_allow() {
    run_db_test(|pg| async move {
        let mut fixture = Fixture::new(&pg, "admin_allow", Some("admin"), None).await;
        let token_id = fixture
            .insert_token(
                &pg,
                Policy {
                    name: "deploy token".to_owned(),
                    statements: vec![allow(vec![Action::AppsDeploy], vec![fixture.app()])],
                },
            )
            .await;

        let decision =
            enforce(&pg, &load_platform_policies().unwrap(), &fixture.ctx(Some(token_id)))
                .await
                .unwrap();

        assert_eq!(decision, AuthzDecision::Allow);
        fixture.cleanup(&pg).await;
    });
}

#[test]
fn owner_unauthorized_returns_deny_even_if_token_grants() {
    run_db_test(|pg| async move {
        let mut fixture = Fixture::new(&pg, "viewer_token_grants", None, Some("viewer")).await;
        let token_id = fixture
            .insert_token(
                &pg,
                Policy {
                    name: "overbroad token".to_owned(),
                    statements: vec![allow(vec![Action::AppsDeploy], vec![fixture.app()])],
                },
            )
            .await;

        let decision =
            enforce(&pg, &load_platform_policies().unwrap(), &fixture.ctx(Some(token_id)))
                .await
                .unwrap();

        assert_eq!(decision, AuthzDecision::Deny);
        fixture.cleanup(&pg).await;
    });
}

#[test]
fn token_denies_returns_deny_even_if_owner_allowed() {
    run_db_test(|pg| async move {
        let mut fixture = Fixture::new(&pg, "admin_token_denies", Some("admin"), None).await;
        let token_id = fixture
            .insert_token(
                &pg,
                Policy {
                    name: "read env token".to_owned(),
                    statements: vec![allow(vec![Action::EnvRead], vec![fixture.app()])],
                },
            )
            .await;

        let decision =
            enforce(&pg, &load_platform_policies().unwrap(), &fixture.ctx(Some(token_id)))
                .await
                .unwrap();

        assert_eq!(decision, AuthzDecision::Deny);
        fixture.cleanup(&pg).await;
    });
}

#[test]
fn no_token_uses_owner_policies_only() {
    run_db_test(|pg| async move {
        let fixture = Fixture::new(&pg, "admin_no_token", Some("admin"), None).await;

        let decision = enforce(&pg, &load_platform_policies().unwrap(), &fixture.ctx(None))
            .await
            .unwrap();

        assert_eq!(decision, AuthzDecision::Allow);
        fixture.cleanup(&pg).await;
    });
}

#[test]
fn audit_locked_app_denies_owner_writes() {
    run_db_test(|pg| async move {
        let fixture =
            Fixture::new_registered_app(&pg, "audit-locked", "owner", false, true).await;

        let decision = enforce(&pg, &load_platform_policies().unwrap(), &fixture.ctx(None))
            .await
            .unwrap();

        assert_eq!(decision, AuthzDecision::Deny);
        fixture.cleanup(&pg).await;
    });
}

#[test]
fn suspended_app_denies_owner_writes() {
    run_db_test(|pg| async move {
        let fixture = Fixture::new_registered_app(&pg, "suspended", "owner", true, false).await;

        let decision = enforce(&pg, &load_platform_policies().unwrap(), &fixture.ctx(None))
            .await
            .unwrap();

        assert_eq!(decision, AuthzDecision::Deny);
        fixture.cleanup(&pg).await;
    });
}

#[test]
fn app_owner_is_authorized_anywhere_for_owned_app_action() {
    run_db_test(|pg| async move {
        let fixture =
            Fixture::new_registered_app(&pg, "owner-anywhere", "owner", false, false).await;
        let policies = load_platform_policies().unwrap();
        let ctx = AuthzContext {
            principal_id: fixture.user_id,
            token_id: None,
            token_policy: None,
            action: Action::AppsDeploy,
            resource: Resource::Any,
            now: 12 * 60 * 60,
            request_ip: None,
            mfa_verified: false,
            mfa_age_seconds: None,
            request_id: None,
        };

        assert!(
            is_authorized_anywhere(&pg, &policies, &ctx).await.unwrap(),
            "app owner should be allowed to grant apps:deploy somewhere"
        );

        fixture.cleanup(&pg).await;
    });
}

#[test]
fn time_window_policy_enforces_utc_hours() {
    run_db_test(|pg| async move {
        let fixture =
            Fixture::new_registered_app(&pg, "time-window", "owner", false, false).await;
        let token_policy = Policy {
            name: "business hours".to_owned(),
            statements: vec![Statement {
                effect: Effect::Allow,
                actions: vec![Action::AppsRead],
                resources: vec![fixture.app()],
                conditions: vec![Condition::TimeWindow {
                    start: "09:00".to_owned(),
                    end: "17:00".to_owned(),
                    tz: "UTC".to_owned(),
                }],
            }],
        };
        let policies = load_platform_policies().unwrap();

        let denied = enforce(
            &pg,
            &policies,
            &fixture.ctx_with_policy(Action::AppsRead, 3 * 60 * 60, token_policy.clone()),
        )
        .await
        .unwrap();
        let allowed = enforce(
            &pg,
            &policies,
            &fixture.ctx_with_policy(Action::AppsRead, 12 * 60 * 60, token_policy),
        )
        .await
        .unwrap();

        assert_eq!(denied, AuthzDecision::Deny);
        assert_eq!(allowed, AuthzDecision::Allow);
        fixture.cleanup(&pg).await;
    });
}

#[test]
fn entity_cache_invalidation_refreshes_platform_role() {
    run_db_test(|pg| async move {
        let fixture = Fixture::new(&pg, "cache-invalidate", None, None).await;
        let policies = load_platform_policies().unwrap();

        let denied = enforce(&pg, &policies, &fixture.ctx(None)).await.unwrap();
        assert_eq!(denied, AuthzDecision::Deny);

        pg.execute(
            "INSERT INTO zeroship.platform_admin_roles (user_id, role) VALUES ($1, 'admin')",
            &[&fixture.user_id],
        )
        .await
        .expect("insert platform role");
        EntityCache::invalidate(fixture.user_id);

        let allowed = enforce(&pg, &policies, &fixture.ctx(None)).await.unwrap();
        assert_eq!(allowed, AuthzDecision::Allow);

        fixture.cleanup(&pg).await;
    });
}

#[test]
fn audit_decision_recorded() {
    run_db_test(|pg| async move {
        let fixture = Fixture::new(&pg, "audit", Some("admin"), None).await;

        let decision = enforce(&pg, &load_platform_policies().unwrap(), &fixture.ctx(None))
            .await
            .unwrap();
        assert_eq!(decision, AuthzDecision::Allow);

        let rows = pg
            .query(
                "SELECT action, resource_type, resource_id, decision, matched_policies \
             FROM zeroship.authz_decisions \
             WHERE actor_user_id = $1 AND action = $2 AND resource_type = 'app' AND resource_id = $3 \
             ORDER BY occurred_at DESC \
             LIMIT 1",
                &[
                    &fixture.user_id,
                    &Action::AppsDeploy.cedar_id(),
                    &fixture.app_id,
                ],
            )
            .await
            .expect("select audit row");
        let row = rows.first().expect("audit row exists");
        assert_eq!(row.get::<_, String>("action"), "apps:deploy");
        assert_eq!(row.get::<_, String>("resource_type"), "app");
        assert_eq!(
            row.get::<_, Option<String>>("resource_id"),
            Some(fixture.app_id.clone())
        );
        assert_eq!(row.get::<_, String>("decision"), "allow");
        let matched_policies: Vec<String> = row.get("matched_policies");
        assert!(
            !matched_policies.is_empty(),
            "audit row should record Cedar matched policy ids"
        );

        fixture.cleanup(&pg).await;
    });
}

/// Regression for finding C1 (cross-tenant read IDOR), exercised end-to-end
/// through the real `enforce` path (which runs the `COALESCE(r.role, ...)`
/// default-role SQL in `load_user`).
///
/// An ordinary creator with NO `platform_admin_roles` row and NO membership of
/// the target app must be DENIED reads on that app. Before the fix the default
/// resolved to `"readonly"`, whose Cedar policy permits reads on an
/// unconstrained resource, so this returned Allow — a fleet-wide cross-tenant
/// read of app metadata / env-var names / secret names / billing / deploy
/// history. Requires AUTH_DB_URL (live PG).
#[test]
fn unroled_creator_denied_cross_tenant_reads() {
    run_db_test(|pg| async move {
        // Victim's app, owned by someone else. `victim` is the owner.
        let victim = Fixture::new(&pg, "c1-victim", None, Some("owner")).await;

        // Attacker: an ordinary creator, no platform role, member of nothing.
        let attacker_id = Uuid::new_v4();
        pg.execute(
            "INSERT INTO zeroship.users (id, email, name) VALUES ($1, $2::citext, $3)",
            &[
                &attacker_id,
                &format!("c1-attacker-{attacker_id}@example.com"),
                &"c1-attacker",
            ],
        )
        .await
        .expect("insert attacker");

        // Force a fresh entity-cache read so the attacker's (lack of) role is
        // evaluated by load_user rather than a stale cache entry.
        EntityCache::invalidate(attacker_id);

        let policies = load_platform_policies().unwrap();
        for action in [
            Action::AppsRead,
            Action::EnvRead,
            Action::SecretsRead,
            Action::BillingRead,
            Action::DeploymentsRead,
        ] {
            let ctx = AuthzContext {
                principal_id: attacker_id,
                token_id: None,
                token_policy: None,
                action,
                resource: Resource::App {
                    id: victim.app_id.clone(),
                },
                now: 12 * 60 * 60,
                request_ip: None,
                mfa_verified: false,
                mfa_age_seconds: None,
                request_id: None,
            };
            let decision = enforce(&pg, &policies, &ctx).await.unwrap();
            assert_eq!(
                decision,
                AuthzDecision::Deny,
                "un-roled creator must NOT read {action:?} on a non-member app (cross-tenant IDOR)",
            );
        }

        // Same-tenant access MUST still work: the victim/owner reads their own
        // app. This guards against over-restricting the fix.
        EntityCache::invalidate(victim.user_id);
        let owner_ctx = AuthzContext {
            principal_id: victim.user_id,
            token_id: None,
            token_policy: None,
            action: Action::SecretsRead,
            resource: Resource::App {
                id: victim.app_id.clone(),
            },
            now: 12 * 60 * 60,
            request_ip: None,
            mfa_verified: false,
            mfa_age_seconds: None,
            request_id: None,
        };
        assert_eq!(
            enforce(&pg, &policies, &owner_ctx).await.unwrap(),
            AuthzDecision::Allow,
            "owner must still read secrets on their OWN app",
        );

        // Cleanup attacker rows.
        let _ = pg
            .execute(
                "DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1",
                &[&attacker_id],
            )
            .await;
        let _ = pg
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[&attacker_id])
            .await;
        victim.cleanup(&pg).await;
    });
}

fn run_db_test<F, Fut>(test: F)
where
    F: FnOnce(Client) -> Fut,
    Fut: Future<Output = ()>,
{
    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skipping (no AUTH_DB_URL)");
        return;
    };
    compio::runtime::Runtime::new()
        .expect("create compio runtime")
        .block_on(async move {
            let pg = pg(&dsn).await;
            test(pg).await;
        });
}

async fn pg(dsn: &str) -> Client {
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}

struct Fixture {
    user_id: Uuid,
    app_id: String,
    app_db_id: Option<Uuid>,
    token_ids: Vec<Uuid>,
}

impl Fixture {
    async fn new(
        pg: &Client,
        label: &str,
        platform_role: Option<&str>,
        app_role: Option<&str>,
    ) -> Self {
        let user_id = Uuid::new_v4();
        // `app_members.app_id` and `apps.id` are `UUID` columns; seed a real
        // UUID and bind it as `Uuid` (binding the String panics ToSql, which
        // is exactly the bug that hid the eval.rs H2 read-as-String defect).
        let app_db_id = Uuid::new_v4();
        let app_id = app_db_id.to_string();
        let email = format!("{label}-{user_id}@example.com");

        pg.execute(
            "INSERT INTO zeroship.users (id, email, name) VALUES ($1, $2::citext, $3)",
            &[&user_id, &email, &label],
        )
        .await
        .expect("insert user");

        if let Some(role) = platform_role {
            pg.execute(
                "INSERT INTO zeroship.platform_admin_roles (user_id, role) VALUES ($1, $2)",
                &[&user_id, &role],
            )
            .await
            .expect("insert platform role");
        }

        let app_db_id = if let Some(role) = app_role {
            // A membership row requires the FK-referenced `apps` row to exist.
            let app_name = format!("authz-{label}-{}", Uuid::new_v4().simple());
            pg.execute(
                "INSERT INTO zeroship.apps (id, name, api_key, api_key_hash) \
                 VALUES ($1, $2, $3, $4)",
                &[&app_db_id, &app_name, &"test-api-key", &"test-api-key-hash"],
            )
            .await
            .expect("insert app");
            pg.execute(
                "INSERT INTO zeroship.app_members (app_id, user_id, role) VALUES ($1, $2, $3)",
                &[&app_db_id, &user_id, &role],
            )
            .await
            .expect("insert app member");
            Some(app_db_id)
        } else {
            None
        };

        Self {
            user_id,
            app_id,
            app_db_id,
            token_ids: Vec::new(),
        }
    }

    async fn new_registered_app(
        pg: &Client,
        label: &str,
        app_role: &str,
        suspended: bool,
        audit_locked: bool,
    ) -> Self {
        let user_id = Uuid::new_v4();
        let app_db_id = Uuid::new_v4();
        let app_id = app_db_id.to_string();
        let email = format!("{label}-{user_id}@example.com");
        let app_name = format!("authz-{label}-{}", Uuid::new_v4().simple());

        pg.execute(
            "INSERT INTO zeroship.users (id, email, name) VALUES ($1, $2::citext, $3)",
            &[&user_id, &email, &label],
        )
        .await
        .expect("insert user");
        pg.execute(
            "INSERT INTO apps (id, name, api_key, api_key_hash, suspended, audit_locked) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            &[
                &app_db_id,
                &app_name,
                &"test-api-key",
                &"test-api-key-hash",
                &suspended,
                &audit_locked,
            ],
        )
        .await
        .expect("insert app");
        // `app_members.app_id` is a `UUID` column — bind the `Uuid`, not the
        // stringified form (binding the String panics ToSql WrongType).
        pg.execute(
            "INSERT INTO zeroship.app_members (app_id, user_id, role) VALUES ($1, $2, $3)",
            &[&app_db_id, &user_id, &app_role],
        )
        .await
        .expect("insert app member");

        Self {
            user_id,
            app_id,
            app_db_id: Some(app_db_id),
            token_ids: Vec::new(),
        }
    }

    async fn insert_token(&mut self, pg: &Client, policy: Policy) -> Uuid {
        let token_id = Uuid::new_v4();
        let policies = policy.to_json_value();
        let hash = policy_hash(&policies);
        pg.execute(
            "INSERT INTO zeroship.permission_tokens \
                (id, owner_id, kind, name, policies, policy_hash) \
             VALUES ($1, $2, 'pat', $3, $4, $5)",
            &[&token_id, &self.user_id, &"test token", &policies, &hash],
        )
        .await
        .expect("insert permission token");
        self.token_ids.push(token_id);
        token_id
    }

    fn app(&self) -> Resource {
        Resource::App {
            id: self.app_id.clone(),
        }
    }

    fn ctx(&self, token_id: Option<Uuid>) -> AuthzContext<'_> {
        AuthzContext {
            principal_id: self.user_id,
            token_id,
            token_policy: None,
            action: Action::AppsDeploy,
            resource: self.app(),
            now: 12 * 60 * 60,
            request_ip: None,
            mfa_verified: false,
            mfa_age_seconds: None,
            request_id: None,
        }
    }

    fn ctx_with_policy(&self, action: Action, now: i64, policy: Policy) -> AuthzContext<'_> {
        AuthzContext {
            principal_id: self.user_id,
            token_id: None,
            token_policy: Some(policy),
            action,
            resource: self.app(),
            now,
            request_ip: None,
            mfa_verified: false,
            mfa_age_seconds: None,
            request_id: None,
        }
    }

    async fn cleanup(&self, pg: &Client) {
        let _ = pg
            .execute(
                "DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1",
                &[&self.user_id],
            )
            .await;
        for token_id in &self.token_ids {
            let _ = pg
                .execute("DELETE FROM zeroship.permission_tokens WHERE id = $1", &[token_id])
                .await;
        }
        let _ = pg
            .execute(
                "DELETE FROM zeroship.app_members WHERE user_id = $1",
                &[&self.user_id],
            )
            .await;
        let _ = pg
            .execute("DELETE FROM zeroship.platform_admin_roles WHERE user_id = $1", &[&self.user_id])
            .await;
        if let Some(app_db_id) = self.app_db_id {
            let _ = pg
                .execute("DELETE FROM apps WHERE id = $1", &[&app_db_id])
                .await;
        }
        let _ = pg
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[&self.user_id])
            .await;
    }
}

fn allow(actions: Vec<Action>, resources: Vec<Resource>) -> Statement {
    Statement {
        effect: Effect::Allow,
        actions,
        resources,
        conditions: Vec::new(),
    }
}
