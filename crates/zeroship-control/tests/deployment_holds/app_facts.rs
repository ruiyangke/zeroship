//! Control's app-facts endpoint: the policy inputs and the deletion marker the
//! workflow service used to read out of `zeroship.apps` and `zeroship.plans`
//! through its own database binding.

use super::*;
use zeroship_core::workflow_app_facts::MAX_APPS_PER_REQUEST;
use zeroship_workflow_client::ControlAppFacts;

/// The real client against the real handler: every fact the policy ledger and
/// the closing lane consume, and the watermark that orders one answer against
/// the next.
///
/// The watermark is checked for the two properties the consumer's fence needs
/// and nothing more: it is a real position rather than a constant, and it does
/// not go backwards across reads of the same source. Asserting a particular
/// value would bind this test to the fixture's write volume.
#[ntex::test]
async fn app_facts_answer_the_policy_inputs_and_the_deletion_marker() {
    let fixture = Fixture::new().await;
    let live = zeroship_core::AppId::mint();
    let gone = zeroship_core::AppId::mint();
    let plan = fixture.platform.seed_app(&live).await;
    let gone_plan = fixture.platform.seed_app(&gone).await;
    let control_server = fixture.control("http://127.0.0.1:1/".into()).await;
    let client = ControlAppFacts::new(
        &origin(&control_server),
        fixture.workflow_role.clone(),
        Options::default(),
    )
    .unwrap();

    let first = client.observe(&[live.clone(), gone.clone()]).await.unwrap();
    assert_eq!(first.apps.len(), 2, "both apps have rows");
    let facts = |response: &zeroship_core::workflow_app_facts::AppFactsResponse,
                 app: &zeroship_core::AppId| {
        response
            .apps
            .iter()
            .find(|facts| facts.app_id == *app)
            .cloned()
            .expect("the requested app is in the answer")
    };
    let seeded = facts(&first, &live);
    assert_eq!(seeded.plan_id, plan);
    assert!(!seeded.workflows_enabled, "apps start with workflows off");
    assert!(!seeded.archived);
    assert!(!seeded.deleted);
    assert!(seeded.plan.workflows_allowed, "the seeded plan allows them");
    assert!(!seeded.plan.archived);
    assert_eq!(
        seeded.plan.workflow_policy, None,
        "an unprovisioned plan carries no policy, which the consumer refuses"
    );
    assert_eq!(facts(&first, &gone).plan_id, gone_plan);

    // Every input the ledger reduces, driven through the wire.
    let policy = zeroship_core::workflow_policy::AppPolicy::default();
    fixture
        .platform
        .admin
        .execute(
            "UPDATE zeroship.apps SET workflows_enabled = true, archived_at = now() WHERE id = $1",
            &[&live.as_str()],
        )
        .await
        .unwrap();
    fixture
        .platform
        .admin
        .execute(
            "UPDATE zeroship.plans SET archived = true, workflows_allowed = false, \
             workflow_policy_json = $2 WHERE id = $1",
            &[&plan, &serde_json::to_value(&policy).unwrap()],
        )
        .await
        .unwrap();
    fixture
        .platform
        .admin
        .execute(
            "UPDATE zeroship.apps SET archived_at = now(), deleted_at = now(), project_id = NULL \
             WHERE id = $1",
            &[&gone.as_str()],
        )
        .await
        .unwrap();
    let second = client.observe(&[live.clone(), gone.clone()]).await.unwrap();
    let changed = facts(&second, &live);
    assert!(changed.workflows_enabled);
    assert!(changed.archived);
    assert!(!changed.deleted);
    assert!(!changed.plan.workflows_allowed);
    assert!(changed.plan.archived);
    assert_eq!(
        changed.plan.workflow_policy,
        Some(serde_json::to_value(&policy).unwrap()),
        "the plan policy crosses as raw JSON for the consumer to decode"
    );
    assert!(facts(&second, &gone).deleted, "the deletion marker crosses");

    // The watermark advanced across those writes and never goes backwards.
    assert!(
        second.watermark > first.watermark,
        "a source that wrote between the reads reports a later position"
    );
    let third = client.observe(std::slice::from_ref(&live)).await.unwrap();
    assert!(third.watermark >= second.watermark);
    // Rejection control on the watermark being real rather than a constant:
    // two positions from the same source differ once the source has written.
    assert_ne!(first.watermark, second.watermark);

    // An app Control has no row for is ABSENT, not reported. A caller must not
    // be able to tell "unknown" from "deleted" by presence alone, because the
    // closing lane abandons on the latter.
    let unknown = zeroship_core::AppId::mint();
    let partial = client.observe(&[live.clone(), unknown.clone()]).await.unwrap();
    assert_eq!(partial.apps.len(), 1);
    assert_eq!(partial.apps[0].app_id, live);
    let none = client.observe(&[unknown]).await.unwrap();
    assert!(none.apps.is_empty());
    assert!(
        none.watermark >= third.watermark,
        "an empty answer still carries an orderable position"
    );

    // The handler's join to the plan cannot drop a live app: `apps_plan_fk` is
    // `ON DELETE RESTRICT`, so an app always names a plan row that exists. That
    // is asserted here as the constraint refusing the write, because the state
    // the join would have to handle is unreachable and cannot be arranged.
    let orphan = fixture
        .platform
        .admin
        .execute(
            "UPDATE zeroship.apps SET plan_id = 'no-such-plan' WHERE id = $1",
            &[&gone.as_str()],
        )
        .await
        .expect_err("an app cannot name a plan that does not exist");
    assert_eq!(
        orphan.as_db_error().unwrap().constraint(),
        Some("apps_plan_fk")
    );
    let retained = fixture
        .platform
        .admin
        .execute("DELETE FROM zeroship.plans WHERE id = $1", &[&gone_plan])
        .await
        .expect_err("a plan an app still names cannot be deleted");
    assert_eq!(
        retained.as_db_error().unwrap().constraint(),
        Some("apps_plan_fk")
    );
}

/// The client refuses a request it should never send, before any exchange.
#[ntex::test]
async fn the_client_refuses_an_empty_or_oversized_request() {
    let fixture = Fixture::new().await;
    let control_server = fixture.control("http://127.0.0.1:1/".into()).await;
    let client = ControlAppFacts::new(
        &origin(&control_server),
        fixture.workflow_role.clone(),
        Options::default(),
    )
    .unwrap();
    // An empty answer to an empty request reads as "none of these apps exists",
    // which is the one thing absence must never mean.
    assert!(matches!(
        client.observe(&[]).await,
        Err(zeroship_workflow_client::Error::InvalidConfig)
    ));
    let oversized: Vec<zeroship_core::AppId> = (0..=MAX_APPS_PER_REQUEST)
        .map(|_| zeroship_core::AppId::mint())
        .collect();
    assert!(matches!(
        client.observe(&oversized).await,
        Err(zeroship_workflow_client::Error::InvalidConfig)
    ));
    // Rejection control: one app under the bound reaches the handler and is
    // answered, so the two refusals above are the bound and not the transport.
    assert!(
        client
            .observe(&oversized[..MAX_APPS_PER_REQUEST])
            .await
            .unwrap()
            .apps
            .is_empty()
    );
}

/// Only the verified `svc/workflow` ROLE reaches the facts, and the check runs
/// before the body is decoded.
#[ntex::test]
async fn app_facts_authenticate_before_decoding() {
    let fixture = Fixture::new().await;
    let app = zeroship_core::AppId::mint();
    fixture.platform.seed_app(&app).await;
    let control_server = fixture.control("http://127.0.0.1:1/".into()).await;
    let origin = origin(&control_server);
    let http = Client::new().await;
    let (worker, worker_auth) = fixture.joined_worker(&http, &origin).await;
    let workflow_instance = signer(
        ServiceIssuer::parse(&format!(
            "spiffe://zeroship.ai/svc/workflow/{}",
            worker.as_str()
        ))
        .unwrap(),
        ServiceSigningKey::generate(),
    );
    let wrong_key = signer(
        service_issuer(WORKFLOW_SERVICE_NAME).unwrap(),
        ServiceSigningKey::generate(),
    );
    for token in [
        None,
        // The stale shared worker role, refused at role arity.
        Some(control_header(&fixture.worker_role)),
        // A joined worker instance: it authenticates, and not for this.
        Some(control_header(&worker_auth)),
        // Control's own credential.
        Some(control_header(&fixture.state.service_auth)),
        // Instance arity of the workflow role: this endpoint is role arity.
        Some(control_header(&workflow_instance)),
        // Correct issuer, key Control does not trust.
        Some(control_header(&wrong_key)),
        // Correctly signed, addressed to the WORKFLOW service instead.
        Some(
            fixture
                .workflow_role
                .authorization_for(&ServiceIssuer::parse(AUDIENCE).unwrap())
                .unwrap(),
        ),
    ] {
        // The body is deliberately undecodable. A 400 here would mean the body
        // was parsed before the caller was checked.
        let (status, failure) = post(
            &http,
            &origin,
            endpoints::CONTROL_APP_FACTS,
            token.as_deref(),
            &json!({"appIds": "not-a-list"}),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(failure, json!({"code":"unauthenticated"}));
    }

    // Rejection control: the verified workflow role DOES reach decoding, so
    // the refusals above are the gate and not a broken route.
    let token = control_header(&fixture.workflow_role);
    let (status, failure) = post(
        &http,
        &origin,
        endpoints::CONTROL_APP_FACTS,
        Some(&token),
        &json!({"appIds": "not-a-list"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(failure, json!({"code":"invalid"}));

    // An unknown field is refused, so a caller cannot smuggle a selector the
    // handler would ignore.
    let token = control_header(&fixture.workflow_role);
    let (status, failure) = post(
        &http,
        &origin,
        endpoints::CONTROL_APP_FACTS,
        Some(&token),
        &json!({"appIds": [app.as_str()], "watermark": 0}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(failure, json!({"code":"invalid"}));

    // An empty list is refused by the handler too, not only by the client.
    let token = control_header(&fixture.workflow_role);
    let (status, failure) = post(
        &http,
        &origin,
        endpoints::CONTROL_APP_FACTS,
        Some(&token),
        &json!({"appIds": []}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(failure, json!({"code":"invalid"}));

    // A single-use assertion is single use here as well.
    let token = control_header(&fixture.workflow_role);
    let body = json!({"appIds": [app.as_str()]});
    let (status, _) = post(
        &http,
        &origin,
        endpoints::CONTROL_APP_FACTS,
        Some(&token),
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, failure) = post(
        &http,
        &origin,
        endpoints::CONTROL_APP_FACTS,
        Some(&token),
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(failure, json!({"code":"unauthenticated"}));
}
