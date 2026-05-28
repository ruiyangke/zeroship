use compio_postgres::{connect, Client, NoTls};
use std::future::Future;
use uuid::Uuid;
use zeroship_authz::{
    enforce, is_authorized_anywhere, load_platform_policies, policy_hash, Action, AuthzContext,
    AuthzDecision, Condition, Effect, Policy, Resource, Statement,
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
fn audit_decision_recorded() {
    run_db_test(|pg| async move {
        let fixture = Fixture::new(&pg, "audit", Some("admin"), None).await;

        let decision = enforce(&pg, &load_platform_policies().unwrap(), &fixture.ctx(None))
            .await
            .unwrap();
        assert_eq!(decision, AuthzDecision::Allow);

        let rows = pg
            .query(
                "SELECT action, resource_type, resource_id, decision \
             FROM control.authz_decisions \
             WHERE user_id = $1 AND action = $2 AND resource_type = 'app' AND resource_id = $3 \
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

        fixture.cleanup(&pg).await;
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
    zeroship_auth::store::migrations::migrate(&client)
        .await
        .expect("migrate auth/control authz tables");
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
        let app_id = format!("blog-{label}-{user_id}");
        let email = format!("{label}-{user_id}@example.com");

        pg.execute(
            "INSERT INTO auth.users (id, email, name) VALUES ($1, $2::citext, $3)",
            &[&user_id, &email, &label],
        )
        .await
        .expect("insert user");

        if let Some(role) = platform_role {
            pg.execute(
                "INSERT INTO platform.roles (user_id, role) VALUES ($1, $2)",
                &[&user_id, &role],
            )
            .await
            .expect("insert platform role");
        }

        if let Some(role) = app_role {
            pg.execute(
                "INSERT INTO control.app_members (app_id, user_id, role) VALUES ($1, $2, $3)",
                &[&app_id, &user_id, &role],
            )
            .await
            .expect("insert app member");
        }

        Self {
            user_id,
            app_id,
            app_db_id: None,
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
            "INSERT INTO auth.users (id, email, name) VALUES ($1, $2::citext, $3)",
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
        pg.execute(
            "INSERT INTO control.app_members (app_id, user_id, role) VALUES ($1, $2, $3)",
            &[&app_id, &user_id, &app_role],
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
            "INSERT INTO control.permission_tokens \
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
                "DELETE FROM control.authz_decisions WHERE user_id = $1",
                &[&self.user_id],
            )
            .await;
        for token_id in &self.token_ids {
            let _ = pg
                .execute("DELETE FROM control.permission_tokens WHERE id = $1", &[token_id])
                .await;
        }
        let _ = pg
            .execute(
                "DELETE FROM control.app_members WHERE user_id = $1",
                &[&self.user_id],
            )
            .await;
        let _ = pg
            .execute("DELETE FROM platform.roles WHERE user_id = $1", &[&self.user_id])
            .await;
        if let Some(app_db_id) = self.app_db_id {
            let _ = pg
                .execute("DELETE FROM apps WHERE id = $1", &[&app_db_id])
                .await;
        }
        let _ = pg
            .execute("DELETE FROM auth.users WHERE id = $1", &[&self.user_id])
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
