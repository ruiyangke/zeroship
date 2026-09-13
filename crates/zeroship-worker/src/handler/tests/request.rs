use super::*;

#[compio::test]
async fn a_hyphenated_uuid_is_refused_on_the_dispatch_path() {
    let (status, rejected) = dispatch_path_segment(&Uuid::new_v4().to_string()).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a uuid rendering is not an app id and must not reach dispatch",
    );
    assert!(
        rejected >= 1,
        "the refusal must be attributed to the bad app id, not to the frame",
    );
}

#[compio::test]
async fn a_canonical_app_id_reaches_on_demand_loading() {
    let canonical = AppId::mint();
    let (status, _) = dispatch_path_segment(canonical.as_str()).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "canonical app id reaches on-demand loading"
    );
}

#[compio::test]
async fn oversized_request_body_is_rejected_by_the_handler_not_the_extractor() {
    let body = vec![b'x'; MAX_DISPATCH_BODY_BYTES + 1];
    let frame = dispatch_frame("POST", "http://app.test/", &body);
    let result = run_pre_dispatch_reject(frame).await;
    assert_eq!(
        result.status,
        ntex::http::StatusCode::PAYLOAD_TOO_LARGE,
        "a body one byte over the cap must reach the handler and get our \
         413, not the extractor's 400; body was {}",
        String::from_utf8_lossy(&result.body),
    );
}

#[compio::test]
async fn body_at_the_cap_passes_the_size_check() {
    let body = vec![b'x'; MAX_DISPATCH_BODY_BYTES];
    let frame = dispatch_frame("POST", "http://app.test/", &body);
    let result = run_pre_dispatch_reject(frame).await;
    assert_eq!(
        result.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a body at the cap reaches on-demand loading"
    );
}

#[compio::test]
async fn dispatch_preserves_non_utf8_request_body_bytes() {
    let source = br#"
    export default {
      async fetch(req) {
        const bytes = Array.from(new Uint8Array(await req.arrayBuffer()));
        return Response.json({ bytes });
      }
    }
"#;
    let worker = Worker::new();
    let app_id = worker.app_id.clone();
    worker
        .load(source, AppRuntimeLimits::default(), &Manifest::default())
        .await;
    let app = test::init_service(web::App::new().configure(worker.configure())).await;

    let raw_body = [0xff, 0x00, 0xfe, 0x80];
    let req = test::TestRequest::post()
        .uri(&format!("/dispatch/{}", app_id.as_str()))
        .header("authorization", gateway_authorization())
        .set_payload(dispatch_frame(
            "POST",
            "http://example.test/binary-body",
            &raw_body,
        ))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    let v: serde_json::Value = serde_json::from_slice(&body).expect("handler returned JSON");
    assert_eq!(
        v["bytes"],
        serde_json::json!([255, 0, 254, 128]),
        "request body must be byte-exact, not UTF-8-lossy"
    );
}

#[compio::test]
async fn dispatch_preserves_non_utf8_response_body_bytes() {
    let source = br#"
    export default {
      fetch() {
        return new Response(new Uint8Array([0xFF, 0x00, 0xFE, 0x80]));
      }
    }
"#;
    let worker = Worker::new();
    let app_id = worker.app_id.clone();
    worker
        .load(source, AppRuntimeLimits::default(), &Manifest::default())
        .await;
    let app = test::init_service(web::App::new().configure(worker.configure())).await;

    let req = test::TestRequest::post()
        .uri(&format!("/dispatch/{}", app_id.as_str()))
        .header("authorization", gateway_authorization())
        .set_payload(dispatch_frame(
            "GET",
            "http://example.test/binary-response",
            b"",
        ))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    assert_eq!(
        &body[..],
        &[0xff, 0x00, 0xfe, 0x80],
        "response body must be byte-exact, not UTF-8-lossy"
    );
}

async fn dispatch_path_segment(segment: &str) -> (StatusCode, u64) {
    let worker = Worker::new();
    let app = test::init_service(web::App::new().configure(worker.configure())).await;
    let before = metrics::DISPATCH_REJECTED_BAD_APP_ID.load(std::sync::atomic::Ordering::Relaxed);
    let req = test::TestRequest::post()
        .uri(&format!("/dispatch/{segment}"))
        .header("authorization", gateway_authorization())
        .set_payload(dispatch_frame("GET", "http://app.test/", b""))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let after = metrics::DISPATCH_REJECTED_BAD_APP_ID.load(std::sync::atomic::Ordering::Relaxed);
    (status, after - before)
}
