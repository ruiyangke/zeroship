use crate::tests::{database::Database, membership::Membership};
use crate::{enforce, load_platform_policies, Action, AuthzDecision};
use crate::{Condition, Effect, Policy, Resource, Statement};

#[compio::test]
async fn principal_authorized_token_authorized_returns_allow() {
    Database::run(async |database| {
        let pg = &database.service;
        let fixture = Membership::seated(&database.admin, "developer-allow", "developer").await;
        let wrapper = Policy {
            name: "deploy token".to_owned(),
            statements: vec![allow(vec![Action::AppsDeploy], vec![fixture.app()])],
        };

        let decision = enforce(
            pg,
            &load_platform_policies().unwrap(),
            &fixture.ctx_with_policy(Action::AppsDeploy, fixture.app(), 12 * 60 * 60, wrapper),
        )
        .await
        .unwrap();

        assert_eq!(decision, AuthzDecision::Allow);
    })
    .await;
}

#[compio::test]
async fn principal_unauthorized_returns_deny_even_if_token_grants() {
    Database::run(async |database| {
        let pg = &database.service;
        let fixture = Membership::seated(&database.admin, "viewer-token-grants", "viewer").await;
        let wrapper = Policy {
            name: "overbroad token".to_owned(),
            statements: vec![allow(vec![Action::AppsDeploy], vec![fixture.app()])],
        };

        let decision = enforce(
            pg,
            &load_platform_policies().unwrap(),
            &fixture.ctx_with_policy(Action::AppsDeploy, fixture.app(), 12 * 60 * 60, wrapper),
        )
        .await
        .unwrap();

        assert_eq!(decision, AuthzDecision::Deny);
    })
    .await;
}

#[compio::test]
async fn token_denies_returns_deny_even_if_principal_allowed() {
    Database::run(async |database| {
        let pg = &database.service;
        let fixture =
            Membership::seated(&database.admin, "developer-token-denies", "developer").await;
        let wrapper = Policy {
            name: "read env token".to_owned(),
            statements: vec![allow(vec![Action::EnvRead], vec![fixture.app()])],
        };

        let decision = enforce(
            pg,
            &load_platform_policies().unwrap(),
            &fixture.ctx_with_policy(Action::AppsDeploy, fixture.app(), 12 * 60 * 60, wrapper),
        )
        .await
        .unwrap();

        assert_eq!(decision, AuthzDecision::Deny);
    })
    .await;
}

#[compio::test]
async fn no_token_uses_the_static_bands_only() {
    Database::run(async |database| {
        let pg = &database.service;
        let fixture = Membership::seated(&database.admin, "developer-no-token", "developer").await;

        let decision = enforce(
            pg,
            &load_platform_policies().unwrap(),
            &fixture.ctx(Action::AppsDeploy, fixture.app()),
        )
        .await
        .unwrap();

        assert_eq!(decision, AuthzDecision::Allow);
    })
    .await;
}

#[compio::test]
async fn time_window_policy_enforces_utc_hours() {
    Database::run(async |database| {
        let pg = &database.service;
        let fixture = Membership::seated(&database.admin, "time-window", "developer").await;
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
            pg,
            &policies,
            &fixture.ctx_with_policy(
                Action::AppsRead,
                fixture.app(),
                3 * 60 * 60,
                token_policy.clone(),
            ),
        )
        .await
        .unwrap();
        let allowed = enforce(
            pg,
            &policies,
            &fixture.ctx_with_policy(Action::AppsRead, fixture.app(), 12 * 60 * 60, token_policy),
        )
        .await
        .unwrap();

        assert_eq!(denied, AuthzDecision::Deny);
        assert_eq!(allowed, AuthzDecision::Allow);
    })
    .await;
}

const fn allow(actions: Vec<Action>, resources: Vec<Resource>) -> Statement {
    Statement {
        effect: Effect::Allow,
        actions,
        resources,
        conditions: Vec::new(),
    }
}
