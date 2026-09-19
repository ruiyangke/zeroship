//! The policy source reads the migrated Control schema under its service role.
#![expect(
    clippy::future_not_send,
    reason = "platform fixtures use their compio runtime"
)]

#[allow(dead_code, reason = "the platform fixture also supports process tests")]
#[path = "support/platform.rs"]
mod platform;
#[path = "support/policy.rs"]
mod policy_fixture;

use policy_fixture::rollout;
use std::{
    num::NonZeroUsize,
    time::{Duration, Instant},
};
use zeroship_core::{AppId, schema_name::SchemaName, workflow_policy::AppPolicy};
use zeroship_data_orm::{
    ConnectOptions, binding::DbBinding, encryption::ProjectKeySource, orm::Database,
};
use zeroship_workflow_manager::{
    Error,
    policy::{
        PolicySource,
        control::{self, ControlPolicies, ControlPolicyStore, RolloutPolicy},
    },
};

struct Fixture {
    platform: platform::Platform,
    app: AppId,
    plan: String,
    source: ControlPolicyStore,
    operator: ControlPolicyStore,
}

impl Fixture {
    async fn new() -> Self {
        Box::pin(async {
            let platform = platform::Platform::new().await;
            let app = AppId::mint();
            let plan = policy_fixture::seed_app(&platform, &app).await;
            let source = connect_store(&platform.runtime_url).await;
            let operator = policy_fixture::operator(&platform).await;
            Self {
                platform,
                app,
                plan,
                source,
                operator,
            }
        })
        .await
    }

    async fn provision(&self, policy: &AppPolicy) {
        self.operator
            .set_plan_policy(&self.plan, policy)
            .await
            .unwrap();
        self.operator.set_rollout(rollout()).await.unwrap();
    }

    async fn execute(&self, sql: &str) {
        self.platform
            .admin
            .execute(sql, &[&self.app.as_str()])
            .await
            .unwrap();
    }
}

async fn connect_store(url: &str) -> ControlPolicyStore {
    let database = Database::connect(
        DbBinding::new(
            "platform",
            "workflow-policy",
            SchemaName::new("zeroship").unwrap(),
        ),
        ConnectOptions::new(url, ProjectKeySource::unavailable()).connection_authority(),
        control::collections().unwrap(),
    )
    .await
    .unwrap();
    ControlPolicyStore::new(database).unwrap()
}

#[compio::test]
async fn readiness_checks_source_join_and_ledger_identity_grants_with_an_empty_catalog() {
    let platform = platform::Platform::new().await;
    let source = connect_store(&platform.runtime_url).await;
    let apps: i64 = platform
        .admin
        .query_one("SELECT count(*) FROM zeroship.apps", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(apps, 0, "readiness must not require a configured app");
    source.ready().await.unwrap();

    platform
        .admin
        .batch_execute("REVOKE SELECT (plan_id) ON zeroship.apps FROM zeroship_workflow")
        .await
        .unwrap();
    assert_eq!(source.ready().await, Err(Error::Unavailable));
    platform
        .admin
        .batch_execute("GRANT SELECT (plan_id) ON zeroship.apps TO zeroship_workflow")
        .await
        .unwrap();
    source.ready().await.unwrap();

    platform
        .admin
        .batch_execute(
            "REVOKE SELECT ON zeroship.workflow_policy_ledger FROM zeroship_workflow; \
             GRANT SELECT (revision, policy_json, source_validity_ms) \
                ON zeroship.workflow_policy_ledger TO zeroship_workflow",
        )
        .await
        .unwrap();
    assert_eq!(source.ready().await, Err(Error::Unavailable));
    platform
        .admin
        .batch_execute("GRANT SELECT ON zeroship.workflow_policy_ledger TO zeroship_workflow")
        .await
        .unwrap();
    source.ready().await.unwrap();
}

#[compio::test]
async fn authoritative_policy_requires_complete_inputs_and_preserves_publication_order() {
    let fixture = Fixture::new().await;
    let policy = AppPolicy::default();
    fixture.source.ready().await.unwrap();
    assert!(matches!(
        fixture.source.observe(&fixture.app).await,
        Err(Error::Unavailable)
    ));
    fixture
        .operator
        .set_plan_policy(&fixture.plan, &policy)
        .await
        .unwrap();
    assert!(matches!(
        fixture.source.observe(&fixture.app).await,
        Err(Error::Unavailable)
    ));
    fixture.operator.set_rollout(rollout()).await.unwrap();
    let original = fixture.source.observe(&fixture.app).await.unwrap();
    assert_eq!(original.policy(), &policy);
    let unchanged = connect_store(&fixture.platform.runtime_url)
        .await
        .observe(&fixture.app)
        .await
        .unwrap();
    assert_eq!(unchanged.revision(), original.revision());
    assert!(!unchanged.same_observation(&original));

    let mut previous = unchanged;
    for sql in [
        "UPDATE zeroship.apps SET workflows_enabled=false WHERE id=$1",
        "UPDATE zeroship.apps SET workflows_enabled=true WHERE id=$1",
        "UPDATE zeroship.apps SET archived_at=now() WHERE id=$1",
        "UPDATE zeroship.apps SET archived_at=NULL WHERE id=$1",
        "UPDATE zeroship.plans SET workflows_allowed=false WHERE id=(SELECT plan_id FROM zeroship.apps WHERE id=$1)",
        "UPDATE zeroship.plans SET workflows_allowed=true WHERE id=(SELECT plan_id FROM zeroship.apps WHERE id=$1)",
        "UPDATE zeroship.plans SET archived=true WHERE id=(SELECT plan_id FROM zeroship.apps WHERE id=$1)",
        "UPDATE zeroship.plans SET archived=false WHERE id=(SELECT plan_id FROM zeroship.apps WHERE id=$1)",
    ] {
        fixture.execute(sql).await;
        let next = fixture.source.observe(&fixture.app).await.unwrap();
        assert_eq!(next.revision().get(), previous.revision().get() + 1);
        assert_ne!(next.policy().admission, previous.policy().admission);
        assert_eq!(next.policy().admission, next.policy().dispatch);
        assert_eq!(next.policy().admission, next.policy().ingress);
        previous = next;
    }
    let alternate_plan = zeroship_core::typed_id::new_plan_id();
    fixture.platform.admin.execute(
        "INSERT INTO zeroship.plans(id,name,runtime_limits_json,workflows_allowed) VALUES($1,'alternate-policy','{}',true)",
        &[&alternate_plan],
    ).await.unwrap();
    let alternate = AppPolicy {
        max_running: policy.max_running + 1,
        ..policy.clone()
    };
    fixture
        .operator
        .set_plan_policy(&alternate_plan, &alternate)
        .await
        .unwrap();
    for (plan, expected) in [(&alternate_plan, &alternate), (&fixture.plan, &policy)] {
        fixture
            .platform
            .admin
            .execute(
                "UPDATE zeroship.apps SET plan_id=$1 WHERE id=$2",
                &[plan, &fixture.app.as_str()],
            )
            .await
            .unwrap();
        let next = fixture.source.observe(&fixture.app).await.unwrap();
        assert!(next.revision() > previous.revision());
        assert_eq!(next.policy(), expected);
        previous = next;
    }
    let paused = RolloutPolicy {
        dispatch_paused: true,
        ..rollout()
    };
    fixture.operator.set_rollout(paused).await.unwrap();
    let next = fixture.source.observe(&fixture.app).await.unwrap();
    assert!(next.policy().admission && next.policy().ingress && !next.policy().dispatch);
    assert!(next.revision() > previous.revision());
    fixture
        .operator
        .set_rollout(RolloutPolicy {
            ingress_disabled: true,
            ..paused
        })
        .await
        .unwrap();
    let next = fixture.source.observe(&fixture.app).await.unwrap();
    assert!(next.policy().admission && !next.policy().ingress && !next.policy().dispatch);
    fixture
        .operator
        .set_rollout(RolloutPolicy {
            source_validity_ms: 10_000,
            ingress_disabled: true,
            ..paused
        })
        .await
        .unwrap();
    let shortened = fixture.source.observe(&fixture.app).await.unwrap();
    assert!(shortened.revision() > next.revision());
    assert_eq!(shortened.policy(), next.policy());

    invalid_inputs_do_not_publish(&fixture).await;
    source_role_cannot_write_inputs_or_read_customer_storage(&fixture).await;
}

async fn invalid_inputs_do_not_publish(fixture: &Fixture) {
    let before: i64 = fixture
        .platform
        .admin
        .query_one(
            "SELECT revision FROM zeroship.workflow_policy_ledger WHERE id=$1",
            &[&fixture.app.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    for corrupt in [
        serde_json::json!({}),
        serde_json::json!(null),
        serde_json::json!("not a policy"),
    ] {
        fixture
            .platform
            .admin
            .execute(
                "UPDATE zeroship.plans SET workflow_policy_json=$1 WHERE id=$2",
                &[&corrupt, &fixture.plan],
            )
            .await
            .unwrap();
        assert!(matches!(
            fixture.source.observe(&fixture.app).await,
            Err(Error::Unavailable)
        ));
    }
    fixture.platform.admin.execute("UPDATE zeroship.plans SET workflow_policy_json=NULL, workflows_allowed=false WHERE id=$1", &[&fixture.plan]).await.unwrap();
    assert!(
        matches!(
            fixture.source.observe(&fixture.app).await,
            Err(Error::Unavailable)
        ),
        "disabled policy still requires complete limits"
    );
    let after: i64 = fixture
        .platform
        .admin
        .query_one(
            "SELECT revision FROM zeroship.workflow_policy_ledger WHERE id=$1",
            &[&fixture.app.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(before, after);
    let invalid = AppPolicy {
        lease_ms: 0,
        ..AppPolicy::default()
    };
    assert_eq!(
        fixture
            .operator
            .set_plan_policy(&fixture.plan, &invalid)
            .await,
        Err(Error::Invalid)
    );
    assert_eq!(
        fixture
            .operator
            .set_rollout(RolloutPolicy {
                source_validity_ms: 0,
                ..rollout()
            })
            .await,
        Err(Error::Invalid)
    );
    let absent = AppId::mint();
    assert!(matches!(
        fixture.source.observe(&absent).await,
        Err(Error::Unavailable)
    ));
    let inserted: i64 = fixture
        .platform
        .admin
        .query_one(
            "SELECT count(*) FROM zeroship.workflow_policy_ledger WHERE id=$1",
            &[&absent.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        inserted, 0,
        "failed first observation rolls back its unpublished row"
    );
}

async fn source_role_cannot_write_inputs_or_read_customer_storage(fixture: &Fixture) {
    assert!(
        fixture
            .source
            .set_plan_policy(&fixture.plan, &AppPolicy::default())
            .await
            .is_err()
    );
    assert!(fixture.source.set_rollout(rollout()).await.is_err());
    fixture.platform.admin.batch_execute("CREATE SCHEMA customer; CREATE TABLE customer.__zeroship_workflow_history(id text PRIMARY KEY, payload text);").await.unwrap();
    let runtime = platform::connect(&fixture.platform.runtime_url).await;
    for sql in [
        "SELECT * FROM customer.__zeroship_workflow_history",
        "UPDATE zeroship.apps SET workflows_enabled=true",
        "DELETE FROM zeroship.workflow_policy_ledger",
    ] {
        let error = runtime.batch_execute(sql).await.unwrap_err();
        assert_eq!(error.as_db_error().unwrap().code().code(), "42501");
    }
}

#[compio::test]
async fn publication_reads_after_lock_wait_and_charges_that_wait_to_source_validity() {
    let fixture = Fixture::new().await;
    fixture.provision(&AppPolicy::default()).await;
    let original = fixture.source.observe(&fixture.app).await.unwrap();
    fixture.platform.admin.batch_execute("BEGIN").await.unwrap();
    fixture
        .execute("UPDATE zeroship.workflow_policy_ledger SET id=id WHERE id=$1")
        .await;
    let source = fixture.source.clone();
    let app = fixture.app.clone();
    let pending = compio::runtime::spawn(async move { source.observe(&app).await });
    wait_for_publication_lock(&fixture.platform.admin).await;
    let blocked_at = Instant::now();
    let changed = AppPolicy {
        max_live_runs: AppPolicy::default().max_live_runs + 1,
        ..AppPolicy::default()
    };
    fixture
        .operator
        .set_plan_policy(&fixture.plan, &changed)
        .await
        .unwrap();
    fixture
        .platform
        .admin
        .batch_execute("COMMIT")
        .await
        .unwrap();
    let observed = pending.await.unwrap().unwrap();
    assert_eq!(observed.policy(), &changed);
    assert_eq!(observed.revision().get(), original.revision().get() + 1);
    assert!(
        observed.expires_at()
            <= blocked_at
                + Duration::from_millis(rollout().source_validity_ms.try_into().unwrap())
    );
    let peer = connect_store(&fixture.platform.runtime_url).await;
    let (left, right) = futures::join!(
        fixture.source.observe(&fixture.app),
        peer.observe(&fixture.app)
    );
    assert_eq!(left.unwrap().revision(), right.unwrap().revision());

    fixture
        .operator
        .set_rollout(RolloutPolicy {
            source_validity_ms: 100,
            ..rollout()
        })
        .await
        .unwrap();
    fixture.platform.admin.batch_execute("BEGIN").await.unwrap();
    fixture
        .execute("UPDATE zeroship.workflow_policy_ledger SET id=id WHERE id=$1")
        .await;
    let source = fixture.source.clone();
    let app = fixture.app.clone();
    let started = Instant::now();
    let pending = compio::runtime::spawn(async move { source.observe(&app).await });
    wait_for_publication_lock(&fixture.platform.admin).await;
    compio::time::sleep_until(started + Duration::from_millis(150)).await;
    fixture
        .platform
        .admin
        .batch_execute("COMMIT")
        .await
        .unwrap();
    assert!(matches!(pending.await.unwrap(), Err(Error::Unavailable)));
    let revision: i64 = fixture
        .platform
        .admin
        .query_one(
            "SELECT revision FROM zeroship.workflow_policy_ledger WHERE id=$1",
            &[&fixture.app.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        revision,
        observed.revision().get(),
        "expired attempt rolls back its publication"
    );
}

async fn wait_for_publication_lock(admin: &compio_postgres::Client) {
    compio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: i64 = admin.query_one("SELECT count(*) FROM pg_stat_activity WHERE usename='zeroship_workflow' AND wait_event_type='Lock'", &[]).await.unwrap().get(0);
            if waiting > 0 { break; }
            compio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("source must wait for the stored publication lock");
}

#[compio::test]
async fn cached_authority_cannot_be_renewed_from_its_own_ledger() {
    let fixture = Fixture::new().await;
    fixture.provision(&AppPolicy::default()).await;
    let source = ControlPolicies::new(
        fixture.source.clone(),
        NonZeroUsize::new(2).unwrap(),
        Duration::from_secs(5),
    )
    .unwrap();
    let original = source.observe(&fixture.app).await.unwrap();
    fixture
        .platform
        .admin
        .batch_execute("DELETE FROM zeroship.workflow_rollout_config")
        .await
        .unwrap();
    let cached = source.observe(&fixture.app).await.unwrap();
    assert!(cached.same_observation(&original));
    assert_eq!(cached.expires_at(), original.expires_at());
    source.invalidate(&fixture.app);
    assert!(source.revalidate(&original).is_err());
    assert!(matches!(
        source.observe(&fixture.app).await,
        Err(Error::Unavailable)
    ));
    fixture.operator.set_rollout(rollout()).await.unwrap();
    let restored = source.observe(&fixture.app).await.unwrap();
    assert_eq!(restored.revision(), original.revision());
    assert!(!restored.same_observation(&original));
    assert!(source.revalidate(&original).is_err());
    assert!(source.revalidate(&restored).is_ok());
}
