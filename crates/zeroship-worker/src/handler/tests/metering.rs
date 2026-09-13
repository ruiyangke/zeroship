use super::*;

#[compio::test]
async fn dispatch_meters_unsupported_upgrade_error_body() {
    let source = br#"
        export default {
          fetch() {
            const pair = new WebSocketPair();
            const [client, server] = Object.values(pair);
            server.accept();
            return new Response(null, { status: 101, webSocket: client });
          }
        };
    "#;
    let result = run_metered_dispatch(source, AppRuntimeLimits::default(), b"sync-upgrade").await;

    assert_generated_error_metering(result, StatusCode::INTERNAL_SERVER_ERROR);
}

#[compio::test]
async fn dispatch_meters_settled_unsupported_upgrade_error_body() {
    let source = br#"
        export default {
          async fetch() {
            await new Promise(resolve => setTimeout(resolve, 0));
            const pair = new WebSocketPair();
            const [client, server] = Object.values(pair);
            server.accept();
            return new Response(null, { status: 101, webSocket: client });
          }
        };
    "#;
    let result =
        run_metered_dispatch(source, AppRuntimeLimits::default(), b"settled-upgrade").await;

    assert_generated_error_metering(result, StatusCode::INTERNAL_SERVER_ERROR);
}

#[compio::test]
async fn dispatch_meters_pending_dispatch_error_body() {
    let source = br#"
        export default {
          async fetch() {
            await new Promise(resolve => setTimeout(resolve, 0));
            return null;
          }
        };
    "#;
    let result = run_metered_dispatch(source, AppRuntimeLimits::default(), b"pending-error").await;

    assert_generated_error_metering(result, StatusCode::INTERNAL_SERVER_ERROR);
}

#[compio::test]
async fn dispatch_meters_timeout_error_body() {
    let source = br#"
        export default {
          fetch() {
            return new Promise(() => {});
          }
        };
    "#;
    let limits = AppRuntimeLimits {
        wall_timeout_ms: Some(1_000),
        ..AppRuntimeLimits::default()
    };
    let result = run_metered_dispatch(source, limits, b"timeout").await;

    assert_generated_error_metering(result, StatusCode::GATEWAY_TIMEOUT);
}

#[compio::test]
async fn dispatch_meters_cpu_burned_on_the_pump_after_an_await() {
    let source = br#"
        export default {
          async fetch() {
            await new Promise(resolve => setTimeout(resolve, 0));
            let total = 0;
            for (let i = 0; i < 20_000_000; i++) total += Math.sqrt(i + 1);
            return new Response(String(total));
          }
        };
    "#;
    let result = run_metered_dispatch(source, AppRuntimeLimits::default(), b"pump-cpu").await;

    assert_eq!(
        result.status,
        StatusCode::OK,
        "the burn handler must complete normally; body was {}",
        String::from_utf8_lossy(&result.body),
    );
    assert_eq!(
        usage_value(&result.events, &result.app_id, "requests"),
        Some(1),
        "exactly one request",
    );

    let total = String::from_utf8(result.body)
        .unwrap()
        .parse::<f64>()
        .unwrap();
    assert!(
        total.is_finite() && total > 0.0,
        "the computation must complete"
    );
    let cpu_us = usage_value(&result.events, &result.app_id, "cpu_us").unwrap_or(0);
    assert!(result.thread_cpu.as_micros() > 0, "observe request CPU");
    assert!(
        u128::from(cpu_us) * 2 >= result.thread_cpu.as_micros(),
        "pump work must dominate the request's metered CPU: metered {cpu_us} us, observed {:?}",
        result.thread_cpu,
    );
}

#[test]
fn self_rescheduling_zero_delay_timer_does_not_wedge_the_pump() {
    const WATCHDOG: Duration = Duration::from_secs(30);

    let source = br#"
        export default {
          async fetch() {
            function spin() {
              const until = Date.now() + 4;
              while (Date.now() < until) {}
              setTimeout(spin, 0);
            }
            setTimeout(spin, 0);
            await new Promise(resolve => setTimeout(resolve, 5));
            return new Response("pump-alive");
          }
        };
    "#;

    let (tx, rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");
        let _ = tx.send(runtime.block_on(run_metered_dispatch(
            source,
            AppRuntimeLimits::default(),
            b"pump-spin",
        )));
    });

    let Ok(result) = rx.recv_timeout(WATCHDOG) else {
        panic!(
            "the pump never came back: a self-rescheduling setTimeout(fn, 0) \
             chain held the PHASE 1 drain for {WATCHDOG:?}, so the handler's \
             5 ms timer was never polled and the request could not complete. \
             The zero-delay drain must be bounded per pass.",
        );
    };
    thread.join().expect("dispatch thread completes");

    assert_eq!(
        result.status,
        StatusCode::OK,
        "the handler must complete normally once the pump yields between \
         passes; body was {}",
        String::from_utf8_lossy(&result.body),
    );
    assert_eq!(result.body, b"pump-alive");

    let cpu_us = usage_value(&result.events, &result.app_id, "cpu_us").unwrap_or(0);
    assert!(result.thread_cpu.as_micros() > 0, "observe request CPU");
    assert!(
        u128::from(cpu_us) * 2 >= result.thread_cpu.as_micros(),
        "self-rescheduling pump CPU must be billed: metered {cpu_us}, observed {:?}",
        result.thread_cpu
    );
}

#[compio::test]
async fn pre_dispatch_bad_envelope_reject_records_platform_counters() {
    let payload = b"not-a-dispatch-frame".to_vec();
    let result = run_pre_dispatch_reject(payload.clone()).await;

    assert_eq!(result.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        usage_value(&result.events, &result.app_id, "requests"),
        Some(1),
        "a rejected envelope is still one request the platform served"
    );
    assert_eq!(
        usage_value(&result.events, &result.app_id, "ingress_bytes"),
        Some(payload.len() as u64),
        "ingress must be the raw frame bytes received, since the frame did not decode"
    );
    assert_eq!(
        usage_value(&result.events, &result.app_id, "egress_bytes"),
        Some(result.body.len() as u64),
        "egress must be the error body actually sent"
    );
}

#[compio::test]
async fn pre_dispatch_on_demand_load_failure_records_platform_counters() {
    let request_body = b"load-failure-request-body";
    let payload = dispatch_frame("POST", "http://example.test/load-fail", request_body);
    let result = run_pre_dispatch_reject(payload).await;

    assert_eq!(result.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        usage_value(&result.events, &result.app_id, "requests"),
        Some(1),
        "a failed on-demand load is still one request the platform handled"
    );
    assert_eq!(
        usage_value(&result.events, &result.app_id, "ingress_bytes"),
        Some(request_body.len() as u64),
        "ingress must be the request body the worker received"
    );
}

#[compio::test]
async fn dispatch_cached_runtime_missing_env_records_all_five_platform_counters() {
    let source = br#"
        export default {
          fetch() {
            return new Response("unreachable");
          }
        };
    "#;
    let request_body = b"missing-env-request-body";
    let worker = Worker::new();
    worker
        .load(source, AppRuntimeLimits::default(), &Manifest::default())
        .await;
    assert!(worker
        .envs
        .write()
        .unwrap()
        .remove(&worker.app_id)
        .is_some());
    let result = worker
        .dispatch(dispatch_frame(
            "POST",
            "http://example.test/missing-env",
            request_body,
        ))
        .await;

    assert_eq!(result.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!result.body.is_empty(), "503 body must be non-empty");
    assert_eq!(
        usage_value(&result.events, &result.app_id, "requests"),
        Some(1),
        "missing env must count exactly one request"
    );
    assert_eq!(
        usage_value(&result.events, &result.app_id, "ingress_bytes"),
        Some(request_body.len() as u64),
        "missing env ingress must equal the request body length"
    );
    assert!(
        usage_value(&result.events, &result.app_id, "wall_us").unwrap_or(0) > 0,
        "missing env wall time must be recorded"
    );
    assert_eq!(
        usage_value(&result.events, &result.app_id, "cpu_us").unwrap_or(0),
        0,
        "missing env does not enter V8 and must record zero V8 CPU time"
    );
    assert_eq!(
        usage_value(&result.events, &result.app_id, "egress_bytes"),
        Some(result.body.len() as u64),
        "missing env egress must equal the 503 body length"
    );
}

#[test]
fn wall_limit_passes_through_unlimited_none() {
    use zeroship_runtime::runtime::{Runtime, RuntimeLimits};
    zeroship_runtime::init::init_v8();

    let unlimited = Runtime::builder()
        .limits(RuntimeLimits {
            cpu_limit: None,
            wall_timeout: None,
            heap_limit_bytes: None,
        })
        .build();
    assert_eq!(
        wall_limit(&unlimited),
        None,
        "unlimited plan must have no dispatch wall cap"
    );

    let bounded = Runtime::builder()
        .limits(RuntimeLimits {
            cpu_limit: None,
            wall_timeout: Some(std::time::Duration::from_secs(5)),
            heap_limit_bytes: None,
        })
        .build();
    assert_eq!(
        wall_limit(&bounded),
        Some(std::time::Duration::from_secs(5)),
        "bounded plan keeps its configured wall cap"
    );
}

#[compio::test]
async fn dispatch_feeds_all_five_platform_counters() {
    let source = br#"
    export default {
      fetch(req, env, ctx) {
        let acc = 0;
        for (let i = 0; i < 200000; i++) { acc += i % 7; }
        return new Response("echo:" + acc.toString().slice(0, 0) + req.url);
      }
    }
"#;
    let worker = Worker::new();
    let app_id = worker.app_id.clone();
    let meter = worker.meter.clone();
    worker
        .load(source, AppRuntimeLimits::default(), &Manifest::default())
        .await;
    let app = test::init_service(web::App::new().configure(worker.configure())).await;

    let req_body = "the-end-user-request-body-payload";
    let url = "http://example.test/counters-probe";
    let req = test::TestRequest::post()
        .uri(&format!("/dispatch/{}", app_id.as_str()))
        .header("authorization", gateway_authorization())
        .set_payload(dispatch_frame("POST", url, req_body.as_bytes()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    let resp_body_len = body.len() as u64;
    assert!(resp_body_len > 0, "handler returned a non-empty body");

    let events = meter.drain();

    assert_eq!(
        usage_value(&events, &app_id, "requests"),
        Some(1),
        "requests counter unchanged"
    );
    assert_eq!(
        usage_value(&events, &app_id, "ingress_bytes"),
        Some(req_body.len() as u64),
        "ingress_bytes must equal the request body length"
    );
    assert_eq!(
        usage_value(&events, &app_id, "egress_bytes"),
        Some(resp_body_len),
        "egress_bytes must equal the response body length"
    );
    assert!(
        usage_value(&events, &app_id, "wall_us").unwrap_or(0) > 0,
        "wall_us must be a positive elapsed-time measurement"
    );
    assert!(
        usage_value(&events, &app_id, "cpu_us").unwrap_or(0) > 0,
        "cpu_us must be a positive CPU-time measurement"
    );
}

fn assert_generated_error_metering(result: crate::worker_fixture::Response, status: StatusCode) {
    assert_eq!(result.status, status);
    assert!(
        !result.body.is_empty(),
        "generated error body must be non-empty"
    );
    assert_eq!(
        usage_value(&result.events, &result.app_id, "requests"),
        Some(1),
        "generated error must count exactly one request"
    );
    assert_eq!(
        usage_value(&result.events, &result.app_id, "egress_bytes"),
        Some(result.body.len() as u64),
        "generated error egress must equal the response body length"
    );
}
