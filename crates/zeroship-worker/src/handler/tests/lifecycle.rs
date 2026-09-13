use super::*;

#[compio::test]
async fn in_flight_dispatch_pins_its_isolate_against_eviction() {
    let worker = Worker::with_kernel(1, empty_kernel(Arc::new(zeroship_metering::Meter::new())));
    let source = br#"
        export default {
          async fetch(req) {
            if (new URL(req.url).pathname === "/release") {
              setTimeout(globalThis.finishDispatch, 0);
              return new Response("released");
            }
            await new Promise(resolve => { globalThis.finishDispatch = resolve; });
            return new Response("a-finished");
          }
        }
    "#;
    worker
        .load(source, AppRuntimeLimits::default(), &Manifest::default())
        .await;
    let service = test::init_service(web::App::new().configure(worker.configure())).await;
    let request = |path: &str| {
        test::TestRequest::post()
            .uri(&format!("/dispatch/{}", worker.app_id.as_str()))
            .header("authorization", gateway_authorization())
            .set_payload(dispatch_frame(
                "GET",
                &format!("http://app.test{path}"),
                b"",
            ))
            .to_request()
    };

    let dispatch = async {
        let response = test::call_service(&service, request("/hold")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(test::read_body(response).await.as_ref(), b"a-finished");
    };
    let competing_load = async {
        compio::time::timeout(Duration::from_secs(10), async {
            while !crate::cache::get_runtime(&worker.app_id).is_some_and(|r| r.is_isolate_leased())
            {
                compio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("dispatch takes an isolate lease");

        let error = crate::cache::load_app(
            AppId::mint(),
            br#"export default { fetch() { return new Response("b"); } }"#,
            AppRuntimeLimits::default(),
            Default::default(),
            None,
            None,
            &Manifest::default(),
            &EnvSnapshot::empty(),
        )
        .await
        .expect_err("a running isolate cannot be evicted to make room");
        assert!(error.contains("leased"), "load must be deferred: {error}");
        assert!(crate::cache::get_runtime(&worker.app_id).is_some());

        let response = test::call_service(&service, request("/release")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(test::read_body(response).await.as_ref(), b"released");
    };

    // Both futures borrow the fixture and are dropped together on failure.
    compio::time::timeout(Duration::from_secs(15), async {
        futures::join!(dispatch, competing_load);
    })
    .await
    .expect("released dispatch completes");
    assert!(crate::cache::get_runtime(&worker.app_id).is_some_and(|r| !r.is_isolate_leased()));
}

#[compio::test]
async fn unwinding_releases_worker_resources_before_the_next_case() {
    let mut observed = None;
    let failure = std::panic::AssertUnwindSafe(async {
        let worker = Worker::new();
        observed = Some((
            worker.app_id.clone(),
            worker.storage.path().to_owned(),
            Arc::downgrade(&worker.config),
        ));
        worker
            .load(
                br#"export default { fetch() { return new Response("ok"); } }"#,
                AppRuntimeLimits::default(),
                &Manifest::default(),
            )
            .await;
        assert!(crate::cache::get_runtime(&worker.app_id).is_some());
        panic!("intentional worker fixture failure");
    })
    .catch_unwind()
    .await
    .expect_err("preserve case failure");
    assert_eq!(
        failure.downcast_ref::<&str>(),
        Some(&"intentional worker fixture failure")
    );
    let (app_id, storage, config) = observed.unwrap();
    assert!(crate::cache::get_runtime(&app_id).is_none());
    assert!(
        !storage.exists(),
        "temporary storage is removed while unwinding"
    );
    assert!(
        config.upgrade().is_none(),
        "service configuration is released"
    );
    let next = Worker::new();
    assert!(next.storage.path().exists());
    next.load(
        br#"export default { fetch() { return new Response("next-worker"); } }"#,
        AppRuntimeLimits::default(),
        &Manifest::default(),
    )
    .await;
    let response = next
        .dispatch(dispatch_frame("GET", "http://app.test/", b""))
        .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, b"next-worker");
}
