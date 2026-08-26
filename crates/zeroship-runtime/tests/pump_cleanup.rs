use std::time::Duration;

use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx};

fn never_settling_module() -> Vec<ModuleEntry> {
    vec![ModuleEntry {
        specifier: "index.js".into(),
        source: r#"
            export default {
                async fetch(_request, _env, _ctx) {
                    await new Promise(() => {});
                    return new Response("unreachable");
                },
            };
        "#
        .into(),
    }]
}

#[test]
fn notify_only_wake_cleans_cancelled_pending_request() {
    init_v8();
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let runtime = zeroship_runtime::runtime::Runtime::builder()
            .modules(never_settling_module())
            .idle_gc_after_ms(0)
            .build();
        runtime.start_pump();

        let env = EnvSnapshot::empty();
        let ctx = RequestCtx::new(CancelFlag::new());
        let outcome = runtime.call_fetch_handler(
            "GET",
            "http://localhost/",
            &[],
            "",
            &env,
            ctx,
        );

        let FetchOutcome::Pending { rx, cancel } = outcome else {
            panic!("expected never-settling handler to return Pending");
        };
        assert!(
            runtime.has_pending_requests_for_test(),
            "pending request should be retained until cancellation"
        );

        cancel.cancel();
        runtime.notify_pump();

        let delivered = compio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("cancelled pending request was not delivered promptly");
        let Err(err) = delivered else {
            panic!("cancelled pending request should deliver an error");
        };
        let err = format!("{err:?}");
        assert!(err.contains("Request timed out"), "unexpected cancellation error: {err}");
        assert!(
            !runtime.has_pending_requests_for_test(),
            "notify-only wake must clean cancelled pending requests"
        );
    });
}
