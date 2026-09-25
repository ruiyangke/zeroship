//! The policy source publishes into its own migrated schema under its service
//! role, from inputs that arrive over Control's app-facts capability.
#![expect(
    clippy::future_not_send,
    reason = "platform fixtures use their compio runtime"
)]

#[path = "support/app_facts.rs"]
mod app_facts;
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
        control::{self, ControlPolicies, ControlPolicyStore, PolicyObservations, RolloutPolicy},
    },
};

struct Fixture {
    platform: platform::Platform,
    app: AppId,
    plan: String,
    source: ControlPolicyStore,
    operator: ControlPolicyStore,
    plans: zeroship_workflow_manager::policy::control::PlanPolicyStore,
}

impl Fixture {
    async fn new() -> Self {
        Box::pin(async {
            let platform = platform::Platform::new().await;
            let app = AppId::mint();
            let plan = policy_fixture::seed_app(&platform, &app).await;
            let source = connect_store(&platform.runtime_url).await;
            let operator = policy_fixture::operator(&platform).await;
            let plans = policy_fixture::plan_admin(&platform).await;
            Self {
                platform,
                app,
                plan,
                source,
                operator,
                plans,
            }
        })
        .await
    }

    async fn provision(&self, policy: &AppPolicy) {
        Box::pin(self.plans.set_plan_policy(&self.plan, policy))
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

/// The publication binding takes the SERVICE role, because the ledger is the
/// service's own. The facts source takes an administrative credential, because
/// it stands in for Control: the workflow role holds no grant on the policy
/// inputs at all now, which
/// `source_role_cannot_write_inputs_or_read_customer_storage` asserts directly.
async fn connect_store(url: &str) -> ControlPolicyStore {
    let control = url.replacen("zeroship_workflow@", "postgres@", 1);
    connect_store_with(url, app_facts::DatabaseAppFacts::connect(&control).await).await
}

async fn connect_store_with(
    url: &str,
    facts: std::rc::Rc<dyn zeroship_workflow_manager::app_facts::AppFactsSource>,
) -> ControlPolicyStore {
    let publication = Database::connect(
        DbBinding::platform(
            "workflow-policy-ledger",
            "workflow-policy-ledger",
            SchemaName::new("workflow_manager").unwrap(),
        ),
        ConnectOptions::new(url, ProjectKeySource::unavailable()).connection_authority(),
        control::publication_collections().unwrap(),
    )
    .await
    .unwrap();
    ControlPolicyStore::new(facts, publication).unwrap()
}

/// Readiness now covers the publication binding ALONE. The policy inputs are
/// Control's and arrive over its endpoint, so there is no Control grant left
/// for this probe to check - and no Control database binding for it to hold.
#[compio::test]
async fn readiness_checks_publication_grants_alone_with_an_empty_catalog() {
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

    // Narrowed to everything BUT `source_watermark`, so this arm binds the
    // watermark column into the projection: a ledger provisioned without it
    // fails readiness rather than failing later at the publication fence,
    // where the error would name an app instead of a missing column.
    platform
        .admin
        .batch_execute(
            "REVOKE SELECT ON workflow_manager.workflow_policy_ledger FROM zeroship_workflow; \
             GRANT SELECT (revision, policy_json, source_validity_ms) \
                ON workflow_manager.workflow_policy_ledger TO zeroship_workflow",
        )
        .await
        .unwrap();
    assert_eq!(source.ready().await, Err(Error::Unavailable));
    platform
        .admin
        .batch_execute(
            "GRANT SELECT ON workflow_manager.workflow_policy_ledger TO zeroship_workflow",
        )
        .await
        .unwrap();
    source.ready().await.unwrap();

    // The switches need their own arm: the ledger probe passes without them.
    platform
        .admin
        .batch_execute(
            "REVOKE SELECT ON workflow_manager.workflow_rollout_config FROM zeroship_workflow",
        )
        .await
        .unwrap();
    assert_eq!(source.ready().await, Err(Error::Unavailable));
    platform
        .admin
        .batch_execute(
            "GRANT SELECT ON workflow_manager.workflow_rollout_config TO zeroship_workflow",
        )
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
        .plans
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
        .plans
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
            "SELECT revision FROM workflow_manager.workflow_policy_ledger WHERE id=$1",
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
            "SELECT revision FROM workflow_manager.workflow_policy_ledger WHERE id=$1",
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
            .plans
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
            "SELECT count(*) FROM workflow_manager.workflow_policy_ledger WHERE id=$1",
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
    assert!(fixture.source.set_rollout(rollout()).await.is_err());
    fixture.platform.admin.batch_execute("CREATE SCHEMA customer; CREATE TABLE customer.__zeroship_workflow_history(id text PRIMARY KEY, payload text);").await.unwrap();
    let runtime = platform::connect(&fixture.platform.runtime_url).await;
    for sql in [
        "SELECT * FROM customer.__zeroship_workflow_history",
        "UPDATE zeroship.apps SET workflows_enabled=true",
        "DELETE FROM workflow_manager.workflow_policy_ledger",
        // The policy inputs are out of reach: the service role holds no grant
        // on the plan catalog and none on the app columns that carry policy,
        // because it reads both over Control's endpoint.
        "SELECT workflow_policy_json FROM zeroship.plans",
        "SELECT workflows_enabled FROM zeroship.apps",
        "SELECT plan_id FROM zeroship.apps",
        "SELECT archived_at FROM zeroship.apps",
        // So is Control's deployment catalog. A management command names its
        // deployment on the wire and this process decides nothing about which
        // one is current, so it reads neither the app's pointer nor the
        // catalog row.
        "SELECT deploy_hash FROM zeroship.apps",
        "SELECT id FROM zeroship.app_deploys",
    ] {
        let error = runtime.batch_execute(sql).await.unwrap_err();
        assert_eq!(
            error.as_db_error().unwrap().code().code(),
            "42501",
            "{sql} must be refused for want of a grant"
        );
    }
    // Rejection control: the columns placement reads ARE granted, so the
    // refusals above are a column-scoped grant rather than the whole table,
    // or the whole schema, having become unreadable to this role.
    let granted = "SELECT id, deleted_at, execution_zone_id FROM zeroship.apps";
    runtime
        .batch_execute(granted)
        .await
        .unwrap_or_else(|error| panic!("{granted} must still be granted: {error}"));
}

#[compio::test]
async fn publication_reads_after_lock_wait_and_charges_that_wait_to_source_validity() {
    let fixture = Fixture::new().await;
    fixture.provision(&AppPolicy::default()).await;
    let original = fixture.source.observe(&fixture.app).await.unwrap();
    fixture.platform.admin.batch_execute("BEGIN").await.unwrap();
    fixture
        .execute("UPDATE workflow_manager.workflow_policy_ledger SET id=id WHERE id=$1")
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
        .plans
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
        .execute("UPDATE workflow_manager.workflow_policy_ledger SET id=id WHERE id=$1")
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
            "SELECT revision FROM workflow_manager.workflow_policy_ledger WHERE id=$1",
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
        PolicyObservations::new(NonZeroUsize::new(2).unwrap()),
        Duration::from_secs(5),
    )
    .unwrap();
    let original = source.observe(&fixture.app).await.unwrap();
    fixture
        .platform
        .admin
        .batch_execute("DELETE FROM workflow_manager.workflow_rollout_config")
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

/// Two policy sources built the way two HTTP threads build theirs grant from
/// one observation, so the deadline a worker receives does not depend on which
/// thread accepted its connection.
#[compio::test]
async fn threads_of_one_manager_grant_from_one_observation() {
    let fixture = Fixture::new().await;
    fixture.provision(&AppPolicy::default()).await;
    let observations = PolicyObservations::new(NonZeroUsize::new(2).unwrap());
    let first = ControlPolicies::new(
        fixture.source.clone(),
        observations.clone(),
        Duration::from_secs(5),
    )
    .unwrap();
    let second =
        ControlPolicies::new(fixture.source.clone(), observations, Duration::from_secs(5)).unwrap();

    let observed = first.observe(&fixture.app).await.unwrap();
    let shared = second.observe(&fixture.app).await.unwrap();
    assert!(shared.same_observation(&observed));
    assert_eq!(shared.expires_at(), observed.expires_at());
    assert_eq!(second.revalidate(&observed).unwrap(), observed.expires_at());

    // The control: a source holding observations of its own reads Control for
    // itself and opens a window of its own, which neither source will recheck
    // the other's grant against.
    let separate = ControlPolicies::new(
        fixture.source.clone(),
        PolicyObservations::new(NonZeroUsize::new(2).unwrap()),
        Duration::from_secs(5),
    )
    .unwrap();
    let independent = separate.observe(&fixture.app).await.unwrap();
    assert!(!independent.same_observation(&observed));
    assert_ne!(independent.expires_at(), observed.expires_at());
    assert!(first.revalidate(&independent).is_err());
    assert!(separate.revalidate(&observed).is_err());
}

/// AN OBSERVATION OLDER THAN THE ONE THE LEDGER ALREADY PUBLISHED FROM IS
/// REFUSED, AND THE REFUSAL PUBLISHES NOTHING.
///
/// This is the property the publication bracket used to get free from a single
/// PostgreSQL instance: a read issued after a commit could not return state
/// older than that commit saw. The inputs now arrive over Control's endpoint,
/// where a lagging replica or a cache in front of the route can return exactly
/// that, so the ledger carries a watermark and refuses a regression instead.
///
/// The hazard is a stale PERMISSIVE policy: `admission`, `dispatch` and
/// `ingress` are ANDed down when an app is disabled, so the stale answer is the
/// one that still admits work, and `PolicyRefresh::install` accepts whatever
/// policy a HIGHER revision carries. This case drives that exact shape - the
/// regressed answer is the permissive one, and the fresh one denies.
///
/// WHAT IT BINDS: delete the comparison in `store::publish` and the regressed
/// answer publishes, bumping the revision and putting the permissive policy on
/// top. The `Unavailable` assertion and the unchanged-revision assertion both
/// fail. It is driven through `ScriptedAppFacts` rather than a real Control,
/// because neither a replica nor a proxy cache can be arranged in a fixture and
/// the fence is precisely what a deployment relies on when one appears.
#[compio::test]
async fn a_regressed_watermark_refuses_and_publishes_nothing() {
    let fixture = Fixture::new().await;
    fixture.provision(&AppPolicy::default()).await;
    let scripted = app_facts::ScriptedAppFacts::new();
    let store = connect_store_with(&fixture.platform.runtime_url, scripted.clone()).await;

    let permissive = AppPolicy::default();
    let restricted = AppPolicy {
        max_live_runs: AppPolicy::default().max_live_runs + 1,
        ..AppPolicy::default()
    };
    let answer = |watermark: i64, policy: &AppPolicy, enabled: bool| {
        zeroship_core::workflow_app_facts::AppFactsResponse {
            watermark: zeroship_core::workflow_app_facts::SourceWatermark::new(watermark).unwrap(),
            apps: vec![zeroship_core::workflow_app_facts::AppSourceFacts {
                app_id: fixture.app.clone(),
                plan_id: fixture.plan.clone(),
                workflows_enabled: enabled,
                archived: false,
                deleted: false,
                plan: zeroship_core::workflow_app_facts::PlanSourceFacts {
                    workflows_allowed: true,
                    archived: false,
                    workflow_policy: Some(serde_json::to_value(policy).unwrap()),
                },
            }],
        }
    };

    // A first publication from a fresh source establishes the held watermark.
    scripted.set(answer(1_000, &permissive, true));
    let published = store.observe(&fixture.app).await.unwrap();
    assert!(published.policy().admission, "the held policy admits work");
    assert_eq!(
        held(&fixture.platform.admin, &fixture.app).await,
        (published.revision().get(), Some(1_000)),
        "the publication records the position it was computed from"
    );

    // An answer from BEHIND that position, carrying a policy that differs, is
    // refused. Without the fence it would publish at a higher revision.
    for stale in [999, 500, 0] {
        scripted.set(answer(stale, &restricted, true));
        assert!(
            matches!(store.observe(&fixture.app).await, Err(Error::Unavailable)),
            "an answer at {stale} is older than the published 1000"
        );
        assert_eq!(
            held(&fixture.platform.admin, &fixture.app).await,
            (published.revision().get(), Some(1_000)),
            "a refused observation leaves the ledger where it was"
        );
    }

    // Rejection control, and the direction that matters: the SAME differing
    // policy at or above the held position publishes. Without this arm the
    // assertions above would also pass if `observe` had simply stopped working.
    scripted.set(answer(1_000, &restricted, true));
    let equal = store.observe(&fixture.app).await.unwrap();
    assert_eq!(equal.revision().get(), published.revision().get() + 1);
    assert_eq!(equal.policy(), &restricted);
    assert_eq!(
        held(&fixture.platform.admin, &fixture.app).await,
        (equal.revision().get(), Some(1_000))
    );
    scripted.set(answer(2_000, &permissive, true));
    let ahead = store.observe(&fixture.app).await.unwrap();
    assert_eq!(ahead.revision().get(), equal.revision().get() + 1);
    assert_eq!(
        held(&fixture.platform.admin, &fixture.app).await,
        (ahead.revision().get(), Some(2_000))
    );

    // The hazard in its own shape: the app is disabled at a NEWER position, so
    // the fresh answer denies. A stale answer that still admits cannot climb
    // over it.
    scripted.set(answer(3_000, &permissive, false));
    let denied = store.observe(&fixture.app).await.unwrap();
    assert!(!denied.policy().admission, "the fresh answer denies");
    scripted.set(answer(2_500, &permissive, true));
    assert!(
        matches!(store.observe(&fixture.app).await, Err(Error::Unavailable)),
        "a stale answer cannot restore admission over a newer denial"
    );
    assert_eq!(
        held(&fixture.platform.admin, &fixture.app).await,
        (denied.revision().get(), Some(3_000))
    );
}

/// The revision and the source position the ledger currently holds for an app.
async fn held(ledger: &compio_postgres::Client, app: &AppId) -> (i64, Option<i64>) {
    let row = ledger
        .query_one(
            "SELECT revision, source_watermark \
               FROM workflow_manager.workflow_policy_ledger WHERE id=$1",
            &[&app.as_str()],
        )
        .await
        .unwrap();
    (row.get::<_, i64>(0), row.get::<_, Option<i64>>(1))
}
