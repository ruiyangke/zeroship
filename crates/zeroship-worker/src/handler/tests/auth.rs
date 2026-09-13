use super::*;

#[compio::test]
async fn dispatch_refuses_an_unauthenticated_request_to_a_user_route() {
    let manifest = manifest_declaring_a_user_route();
    let (status, lines) = dispatch_unauthenticated(&manifest, "/api/private").await;

    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a `user` route reached with no identity must be refused by the \
     worker; got {status} instead, which means the request was served"
    );
    assert!(
        lines.is_empty(),
        "the refusal must happen BEFORE creator code runs, so the app \
     must have logged nothing; got {lines:?}"
    );
}

#[compio::test]
async fn dispatch_refuses_a_caller_with_no_service_credential() {
    let worker = Worker::new();
    let app_id = worker.app_id.clone();
    let app = test::init_service(web::App::new().configure(worker.configure())).await;

    for header in [None, Some("Bearer "), Some("Bearer not-an-assertion")] {
        let mut req = test::TestRequest::post()
            .uri(&format!("/dispatch/{}", app_id.as_str()))
            .set_payload(dispatch_frame("GET", "http://example.test/", b""));
        if let Some(value) = header {
            req = req.header("authorization", value);
        }
        let resp = test::call_service(&app, req.to_request()).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "header {header:?} must not reach the runtime"
        );
    }

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/dispatch/{}", app_id.as_str()))
            .header("authorization", gateway_authorization())
            .set_payload(dispatch_frame("GET", "http://example.test/", b""))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[compio::test]
async fn an_unconfigured_worker_refuses_dispatch() {
    let mut worker = Worker::new();
    Arc::get_mut(&mut worker.config)
        .expect("sole config owner")
        .service_auth = Arc::new(zeroship_core::service_peers::ServiceAuth::unconfigured());
    let app_id = worker.app_id.clone();
    let app = test::init_service(web::App::new().configure(worker.configure())).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/dispatch/{}", app_id.as_str()))
            .header("authorization", gateway_authorization())
            .set_payload(dispatch_frame("GET", "http://example.test/", b""))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[compio::test]
async fn an_identity_envelope_signed_by_the_wrong_key_is_refused() {
    let worker = Worker::new();
    let app_id = worker.app_id.clone();
    let app = test::init_service(web::App::new().configure(worker.configure())).await;

    const USER: &[u8] = br#"{"id":"pws_forged","email":"a@b.test","name":"A","avatar":null,"email_verified":true,"scopes":[]}"#;
    let request_id = Uuid::new_v4();
    let identity = identity();
    let forged = identity
        .impostor
        .user_envelope_signer()
        .sign(USER, request_id);

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/dispatch/{}", app_id.as_str()))
            .header("authorization", gateway_authorization())
            .header("x-request-id", request_id.to_string().as_str())
            .header("zeroship-user", forged.as_str())
            .set_payload(dispatch_frame("GET", "http://example.test/", b""))
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "an identity envelope the gateway did not sign must be refused, \
     even when the TRANSPORT credential is the gateway's genuine one"
    );

    let mut parts: Vec<&str> = forged.split('.').collect();
    parts[3] = identity.gateway.user_envelope_signer().key_id();
    let relabelled = parts.join(".");
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/dispatch/{}", app_id.as_str()))
            .header("authorization", gateway_authorization())
            .header("x-request-id", request_id.to_string().as_str())
            .header("zeroship-user", relabelled.as_str())
            .set_payload(dispatch_frame("GET", "http://example.test/", b""))
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "naming a trusted kid must not admit an envelope signed by another key"
    );

    let genuine = identity
        .gateway
        .user_envelope_signer()
        .sign(USER, request_id);
    assert_ne!(genuine, forged, "the two signers must differ");
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/dispatch/{}", app_id.as_str()))
            .header("authorization", gateway_authorization())
            .header("x-request-id", request_id.to_string().as_str())
            .header("zeroship-user", genuine.as_str())
            .set_payload(dispatch_frame("GET", "http://example.test/", b""))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[test]
fn the_worker_own_service_key_cannot_sign_an_identity_it_accepts() {
    let identity = identity();
    const USER: &[u8] = br#"{"id":"pws_self","email":"a@b.test","name":"A","avatar":null,"email_verified":true,"scopes":[]}"#;
    let request_id = Uuid::new_v4();
    let verifier = identity
        .service_auth
        .user_envelope_verifier()
        .expect("the test worker verifies envelopes");

    let self_signed = identity
        .service_auth
        .user_envelope_signer()
        .expect("the worker holds its own key")
        .sign(USER, request_id);
    assert_eq!(
        verifier.verify_for_request(&self_signed, request_id),
        None,
        "the worker must not be able to mint an identity it would accept"
    );

    assert!(verifier
        .verify_for_request(
            &identity
                .gateway
                .user_envelope_signer()
                .sign(USER, request_id),
            request_id
        )
        .is_some());
}

#[compio::test]
async fn dispatch_still_serves_an_unauthenticated_request_to_an_anonymous_route() {
    let manifest = manifest_declaring_a_user_route();
    let (status, lines) = dispatch_unauthenticated(&manifest, "/api/public").await;

    assert_eq!(
        status,
        StatusCode::OK,
        "an `anonymous` route must stay reachable without identity"
    );
    assert_eq!(
        lines,
        vec!["creator-code-ran /api/public".to_string()],
        "the app's handler must have run on the anonymous route"
    );
}

fn manifest_declaring_a_user_route() -> zeroship_bundle::Manifest {
    let manifest: zeroship_bundle::Manifest = serde_json::from_value(serde_json::json!({
        "version": 1,
        "resources": {
            "/api/private": { "auth": "user" },
            "/api/public": { "auth": "anonymous", "publicly_accessible": true },
        }
    }))
    .expect("fixture manifest parses as the real wire shape");

    assert_eq!(
        manifest
            .resources
            .get("/api/private")
            .and_then(|entry| entry.auth),
        Some(zeroship_bundle::RequiredPrincipal::User),
        "fixture must declare /api/private as a user route"
    );
    assert_eq!(
        manifest
            .resources
            .get("/api/public")
            .and_then(|entry| entry.auth),
        Some(zeroship_bundle::RequiredPrincipal::Anonymous),
        "fixture must declare /api/public as an anonymous route"
    );
    manifest
}

const AUTH_OBLIVIOUS_APP: &[u8] = br#"
    export default {
      fetch(req) {
        console.log("creator-code-ran", new URL(req.url).pathname);
        return new Response("served");
      }
    }
"#;

async fn dispatch_unauthenticated(
    manifest: &zeroship_bundle::Manifest,
    path: &str,
) -> (StatusCode, Vec<String>) {
    let worker = Worker::new();
    let app_id = worker.app_id.clone();
    worker
        .load(AUTH_OBLIVIOUS_APP, AppRuntimeLimits::default(), manifest)
        .await;
    let app = test::init_service(web::App::new().configure(worker.configure())).await;
    let req = test::TestRequest::post()
        .uri(&format!("/dispatch/{}", app_id.as_str()))
        .header("authorization", gateway_authorization())
        .set_payload(dispatch_frame(
            "GET",
            &format!("http://app.test{path}"),
            b"",
        ))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let _ = test::read_body(resp).await;
    let lines = crate::logs::get(&worker.logs, &app_id);
    (status, lines)
}
