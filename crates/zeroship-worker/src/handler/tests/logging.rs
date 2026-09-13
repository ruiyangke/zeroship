use super::*;

#[compio::test]
async fn dispatch_console_lines_are_queryable_from_logs_endpoint() {
    let source = br#"
    export default {
      fetch(req) {
        console.log("b2-real-log", new URL(req.url).pathname);
        return new Response("ok");
      }
    }
"#;
    let worker = Worker::new();
    let app_id = worker.app_id.clone();
    worker
        .load(source, AppRuntimeLimits::default(), &Manifest::default())
        .await;
    let app = test::init_service(
        web::App::new()
            .configure(worker.configure())
            .service(web::resource("/logs/{app_id}").route(web::get().to(crate::logs::get_logs))),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/dispatch/{}", app_id.as_str()))
        .header("authorization", gateway_authorization())
        .set_payload(dispatch_frame(
            "GET",
            "http://example.test/from-worker-test",
            b"",
        ))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    assert_eq!(&body[..], b"ok");

    let req = test::TestRequest::get()
        .uri(&format!("/logs/{}", app_id.as_str()))
        .header("authorization", control_authorization())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    let lines: Vec<String> = serde_json::from_slice(&body).expect("logs json");
    assert_eq!(lines, vec!["b2-real-log /from-worker-test"]);
}
