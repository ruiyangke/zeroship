#![allow(
    clippy::future_not_send,
    reason = "workflow cases keep V8 and database drivers on their compio runtime"
)]

use super::tests::usage_value;
use crate::identity_fixture::gateway_authorization;
use super::workflow_advance_unsigned;
use fixture::{
    assert_workflow_ack, assert_workflow_nack, workflow_request, workflow_request_for_run, Fixture,
};
use ntex::http::StatusCode;
use ntex::web::{self, test};
mod fixture;
mod journal;

#[compio::test]
async fn workflow_advance_first_frontier_returns_step_completed() {
    Fixture::run(4, async |case| {
        let deploy_hash = case.deploy("A").await;
        case.journal
            .seed_run("run_test", "Checkout", &deploy_hash)
            .await;
        let app = test::init_service(
            web::App::new()
                .state(case.config.clone())
                .state(case.envs.clone())
                .state(case.logs.clone())
                .service(
                    web::resource("/workflow-advance-unsigned/{app_id}")
                        .route(web::post().to(workflow_advance_unsigned)),
                ),
        )
        .await;

        let req = test::TestRequest::post()
            .uri(&format!(
                "/workflow-advance-unsigned/{}",
                case.app_id.as_str()
            ))
            .header("authorization", gateway_authorization())
            .set_payload(serde_json::to_vec(&workflow_request(&case.app_id)).unwrap())
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = test::read_body(resp).await;
        assert_workflow_ack(&body, "run_test");
        assert_eq!(
            case.journal.step_names("run_test").await,
            vec!["first".to_string()]
        );
        let output = case.journal.step_output("run_test", "first").await;
        assert_eq!(output["mark"], "A");
        assert_eq!(output["bodyRuns"], 1);
    })
    .await;
}

#[compio::test]
async fn workflow_advance_claim_lost_nacks_without_replay() {
    Fixture::run(4, async |case| {
        let deploy_hash = case.deploy("CL").await;
        case.journal
            .seed_run("run_claim_lost", "Checkout", &deploy_hash)
            .await;
        case.journal.steal_claim("run_claim_lost").await;
        let app = test::init_service(
            web::App::new()
                .state(case.config.clone())
                .state(case.envs.clone())
                .state(case.logs.clone())
                .service(
                    web::resource("/workflow-advance-unsigned/{app_id}")
                        .route(web::post().to(workflow_advance_unsigned)),
                ),
        )
        .await;

        let req = test::TestRequest::post()
            .uri(&format!(
                "/workflow-advance-unsigned/{}",
                case.app_id.as_str()
            ))
            .header("authorization", gateway_authorization())
            .set_payload(
                serde_json::to_vec(&workflow_request_for_run(&case.app_id, "run_claim_lost"))
                    .unwrap(),
            )
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = test::read_body(resp).await;
        assert_workflow_nack(&body, "run_claim_lost", "claimLost");
        assert!(
            case.journal.step_names("run_claim_lost").await.is_empty(),
            "claim-lost dispatch must not replay or apply"
        );
    })
    .await;
}

#[compio::test]
async fn workflow_advance_concurrent_frontier_returns_outcomes_batch() {
    Fixture::run(4, async |case| {
        let deploy_hash = case.deploy("C").await;
        case.journal
            .seed_run("run_test", "ConcurrentWorkflow", &deploy_hash)
            .await;
        let app = test::init_service(
            web::App::new()
                .state(case.config.clone())
                .state(case.envs.clone())
                .state(case.logs.clone())
                .service(
                    web::resource("/workflow-advance-unsigned/{app_id}")
                        .route(web::post().to(workflow_advance_unsigned)),
                ),
        )
        .await;

        let req = test::TestRequest::post()
            .uri(&format!(
                "/workflow-advance-unsigned/{}",
                case.app_id.as_str()
            ))
            .header("authorization", gateway_authorization())
            .set_payload(serde_json::to_vec(&workflow_request(&case.app_id)).unwrap())
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = test::read_body(resp).await;
        assert_workflow_ack(&body, "run_test");
        assert_eq!(
            case.journal.step_names("run_test").await,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    })
    .await;
}

#[compio::test]
async fn workflow_advance_feeds_platform_counters_and_workflow_steps_metric() {
    Fixture::run(4, async |case| {
        let deploy_hash = case.deploy("M").await;
        case.journal
            .seed_run("run_test", "Checkout", &deploy_hash)
            .await;
        let app = test::init_service(
            web::App::new()
                .state(case.config.clone())
                .state(case.envs.clone())
                .state(case.logs.clone())
                .service(
                    web::resource("/workflow-advance-unsigned/{app_id}")
                        .route(web::post().to(workflow_advance_unsigned)),
                ),
        )
        .await;

        let payload = serde_json::to_vec(&workflow_request(&case.app_id)).unwrap();
        let req = test::TestRequest::post()
            .uri(&format!(
                "/workflow-advance-unsigned/{}",
                case.app_id.as_str()
            ))
            .header("authorization", gateway_authorization())
            .set_payload(payload.clone())
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = test::read_body(resp).await;
        assert_workflow_ack(&body, "run_test");

        let events = case.meter.drain();
        assert_eq!(
            usage_value(&events, &case.app_id, "requests"),
            Some(1),
            "workflow advance is one metered request"
        );
        assert_eq!(
            usage_value(&events, &case.app_id, "ingress_bytes"),
            Some(payload.len() as u64),
            "workflow advance ingress is the StepRequest JSON body"
        );
        assert_eq!(
            usage_value(&events, &case.app_id, "egress_bytes"),
            Some(body.len() as u64),
            "workflow advance egress is the ack JSON body"
        );
        assert!(
            usage_value(&events, &case.app_id, "wall_us").unwrap_or(0) > 0,
            "workflow advance records wall_us"
        );
        assert!(
            usage_value(&events, &case.app_id, "cpu_us").unwrap_or(0) > 0,
            "workflow advance records cpu_us"
        );
        assert_eq!(
            usage_value(&events, &case.app_id, "workflow_steps"),
            Some(1),
            "workflow advance records observability workflow_steps"
        );
    })
    .await;
}

#[compio::test]
async fn workflow_advance_replays_journal_hit_without_rerunning_body() {
    Fixture::run(4, async |case| {
        let deploy_hash = case.deploy("A").await;
        case.journal
            .seed_run("run_test", "Checkout", &deploy_hash)
            .await;
        let app = test::init_service(
            web::App::new()
                .state(case.config.clone())
                .state(case.envs.clone())
                .state(case.logs.clone())
                .service(
                    web::resource("/workflow-advance-unsigned/{app_id}")
                        .route(web::post().to(workflow_advance_unsigned)),
                ),
        )
        .await;

        let first_req = test::TestRequest::post()
            .uri(&format!(
                "/workflow-advance-unsigned/{}",
                case.app_id.as_str()
            ))
            .header("authorization", gateway_authorization())
            .set_payload(serde_json::to_vec(&workflow_request(&case.app_id)).unwrap())
            .to_request();
        let first_resp = test::call_service(&app, first_req).await;
        assert_eq!(first_resp.status(), StatusCode::OK);
        let first_body = test::read_body(first_resp).await;
        assert_workflow_ack(&first_body, "run_test");
        case.journal.reclaim("run_test").await;
        let second_req = test::TestRequest::post()
            .uri(&format!(
                "/workflow-advance-unsigned/{}",
                case.app_id.as_str()
            ))
            .header("authorization", gateway_authorization())
            .set_payload(serde_json::to_vec(&workflow_request(&case.app_id)).unwrap())
            .to_request();
        let second_resp = test::call_service(&app, second_req).await;
        assert_eq!(second_resp.status(), StatusCode::OK);
        let second_body = test::read_body(second_resp).await;
        assert_workflow_ack(&second_body, "run_test");
        assert_eq!(
            case.journal.step_names("run_test").await,
            vec!["first".to_string(), "second".to_string()]
        );
        let second_output = case.journal.step_output("run_test", "second").await;
        assert_eq!(
            second_output["bodyRuns"], 2,
            "the completed first step must be replayed from journal, not re-run"
        );
    })
    .await;
}

#[compio::test]
async fn workflow_advance_keeps_in_flight_run_on_pinned_deploy_after_redeploy() {
    Fixture::run(4, async |case| {
        let deploy_a = case.deploy("A").await;
        let deploy_b = case.deploy("B").await;
        assert_ne!(deploy_a, deploy_b);
        let app = test::init_service(
            web::App::new()
                .state(case.config.clone())
                .state(case.envs.clone())
                .state(case.logs.clone())
                .service(
                    web::resource("/workflow-advance-unsigned/{app_id}")
                        .route(web::post().to(workflow_advance_unsigned)),
                ),
        )
        .await;

        for (run_id, deploy_hash, expected_mark) in [
            ("run_pinned_a", &deploy_a, "A"),
            ("run_pinned_b", &deploy_b, "B"),
        ] {
            case.journal.seed_run(run_id, "Checkout", deploy_hash).await;
            let req = test::TestRequest::post()
                .uri(&format!(
                    "/workflow-advance-unsigned/{}",
                    case.app_id.as_str()
                ))
                .header("authorization", gateway_authorization())
                .set_payload(
                    serde_json::to_vec(&workflow_request_for_run(&case.app_id, run_id)).unwrap(),
                )
                .to_request();
            let resp = test::call_service(&app, req).await;
            assert_eq!(resp.status(), StatusCode::OK);
            let body = test::read_body(resp).await;
            assert_workflow_ack(&body, run_id);
            let output = case.journal.step_output(run_id, "first").await;
            assert_eq!(output["mark"], expected_mark);
        }
        case.journal.reclaim("run_pinned_a").await;
        let resumed = test::TestRequest::post()
            .uri(&format!(
                "/workflow-advance-unsigned/{}",
                case.app_id.as_str()
            ))
            .header("authorization", gateway_authorization())
            .set_payload(
                serde_json::to_vec(&workflow_request_for_run(&case.app_id, "run_pinned_a"))
                    .unwrap(),
            )
            .to_request();
        let response = test::call_service(&app, resumed).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_workflow_ack(&test::read_body(response).await, "run_pinned_a");
        let output = case.journal.step_output("run_pinned_a", "second").await;
        assert_eq!(
            output["mark"], "A",
            "resume the original run on its original deploy"
        );
        assert_eq!(output["first"]["mark"], "A");
        assert_eq!(output["bodyRuns"], 2, "replay the original journal prefix");
        assert!(crate::cache::has_pinned_workflow_app(
            &case.app_id,
            &deploy_a
        ));
        assert!(crate::cache::has_pinned_workflow_app(
            &case.app_id,
            &deploy_b
        ));
    })
    .await;
}

#[compio::test]
async fn workflow_advance_pinned_isolate_budget_lru_evicts_per_app() {
    Fixture::run(1, async |case| {
        let deploy_a = case.deploy("A").await;
        let deploy_b = case.deploy("B").await;
        let app = test::init_service(
            web::App::new()
                .state(case.config.clone())
                .state(case.envs.clone())
                .state(case.logs.clone())
                .service(
                    web::resource("/workflow-advance-unsigned/{app_id}")
                        .route(web::post().to(workflow_advance_unsigned)),
                ),
        )
        .await;

        case.journal
            .seed_run("run_test_lru_a", "Checkout", &deploy_a)
            .await;
        let req_a = test::TestRequest::post()
            .uri(&format!(
                "/workflow-advance-unsigned/{}",
                case.app_id.as_str()
            ))
            .header("authorization", gateway_authorization())
            .set_payload(
                serde_json::to_vec(&workflow_request_for_run(&case.app_id, "run_test_lru_a"))
                    .unwrap(),
            )
            .to_request();
        let resp_a = test::call_service(&app, req_a).await;
        assert_eq!(resp_a.status(), StatusCode::OK);
        assert!(crate::cache::has_pinned_workflow_app(
            &case.app_id,
            &deploy_a
        ));

        case.journal
            .seed_run("run_test_lru_b", "Checkout", &deploy_b)
            .await;
        let req_b = test::TestRequest::post()
            .uri(&format!(
                "/workflow-advance-unsigned/{}",
                case.app_id.as_str()
            ))
            .header("authorization", gateway_authorization())
            .set_payload(
                serde_json::to_vec(&workflow_request_for_run(&case.app_id, "run_test_lru_b"))
                    .unwrap(),
            )
            .to_request();
        let resp_b = test::call_service(&app, req_b).await;
        assert_eq!(resp_b.status(), StatusCode::OK);
        assert!(!crate::cache::has_pinned_workflow_app(
            &case.app_id,
            &deploy_a
        ));
        assert!(crate::cache::has_pinned_workflow_app(
            &case.app_id,
            &deploy_b
        ));
    })
    .await;
}
