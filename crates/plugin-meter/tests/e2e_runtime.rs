//! End-to-end test for `env.meter` — drives JS through the REAL V8 runtime
//! down to the shared per-worker [`Meter`], then asserts on the Rust side
//! that the increments actually landed (and that the per-app scoping +
//! validation hold).
//!
//! This closes the gap nothing else covers: the real path
//!
//!     JS `env.meter.increment("x", n)`
//!       → V8 arg marshalling
//!       → MeterHandle::increment (synchronous)
//!       → shared Arc<Meter>
//!       → (Rust) Meter::drain sees the totals
//!
//! Mirrors `plugin-kv/tests/e2e_runtime.rs`'s harness. Because increment is
//! synchronous, the fetch handler returns a `Response` directly (no Pending
//! pump round trip needed), but we still build a real Runtime + plugin.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch,
};

use zeroship_plugin_meter::{Meter, MeterPlugin};

/// The JS app — exercises `env.meter.increment` and self-asserts the return
/// values (the new running totals). The Rust side then drains the shared
/// meter and asserts the persisted totals match.
const METER_E2E_APP: &str = r#"
export default {
    async fetch(request, env, ctx) {
        const m = env.meter;
        const trace = [];
        function fail(step, got, want) {
            const e = new Error("step failed: " + step);
            e.zsStep = step; e.zsGot = got; e.zsWant = want; throw e;
        }
        function eq(step, got, want) {
            trace.push(step);
            if (got !== want) fail(step, got, want);
        }

        try {
            // increment returns the new running total
            eq("inc.default", m.increment("requests"), 1);   // n defaults to 1
            eq("inc.by3", m.increment("requests", 3), 4);
            eq("inc.custom.1", m.increment("emails_sent", 2), 2);
            eq("inc.custom.2", m.increment("emails_sent", 5), 7);
            eq("inc.cpu", m.increment("cpu_us", 1000), 1000);

            // validation: bad count throws synchronously
            let threwNeg = false;
            try { m.increment("requests", -1); } catch (e) { threwNeg = true; }
            eq("inc.neg.throws", threwNeg, true);

            let threwFrac = false;
            try { m.increment("requests", 1.5); } catch (e) { threwFrac = true; }
            eq("inc.frac.throws", threwFrac, true);

            let threwEmpty = false;
            try { m.increment("", 1); } catch (e) { threwEmpty = true; }
            eq("inc.emptymetric.throws", threwEmpty, true);

            let threwType = false;
            try { m.increment("requests", "x"); } catch (e) { threwType = true; }
            eq("inc.counttype.throws", threwType, true);

            return Response.json({ ok: true, trace });
        } catch (e) {
            if (e && e.zsStep) {
                return Response.json({
                    ok: false, step: e.zsStep,
                    got: e.zsGot === undefined ? null : e.zsGot,
                    want: e.zsWant === undefined ? null : e.zsWant,
                    trace,
                }, { status: 500 });
            }
            return Response.json({
                ok: false,
                step: trace.length ? trace[trace.length - 1] : "<none>",
                error: String(e && e.message ? e.message : e),
                trace,
            }, { status: 500 });
        }
    }
};
"#;

/// Illegal-constructor app — `new env.meter.constructor()` must throw.
const METER_CTOR_APP: &str = r#"
export default {
    async fetch(request, env, ctx) {
        let threw = false;
        let msg = "";
        try { new env.meter.constructor(); }
        catch (e) { threw = true; msg = String(e && e.message ? e.message : e); }
        if (!threw) return Response.json({ ok: false, detail: "did not throw" }, { status: 500 });
        if (!msg.includes("Illegal constructor"))
            return Response.json({ ok: false, detail: "wrong message: " + msg }, { status: 500 });
        return Response.json({ ok: true });
    }
};
"#;

fn module(source: &str) -> Vec<ModuleEntry> {
    vec![ModuleEntry { specifier: "index.js".into(), source: source.into() }]
}

/// Build a Runtime around `app` JS + a `MeterPlugin` over `meter`, scoped to
/// `app_id`, call the fetch handler, and return `(status, body)`.
fn run_app(meter: Arc<Meter>, app_id: &str, app: &'static str) -> (u16, String) {
    let app_id = app_id.to_string();
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        init_v8();

        let mut env_vars = HashMap::new();
        env_vars.insert("APP_ID".to_string(), app_id);

        let plugin: Arc<dyn NativePlugin> = Arc::new(MeterPlugin::with_meter(meter));

        let runtime = Runtime::builder()
            .modules(module(app))
            .env_vars(env_vars)
            .plugins(vec![plugin])
            .build();
        runtime.start_pump();

        let env = EnvSnapshot::empty();
        let ctx = RequestCtx::new(CancelFlag::new());
        let outcome = runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx);

        match outcome {
            FetchOutcome::Response { status, body, .. } => (status, body),
            FetchOutcome::Pending { rx, cancel: _ } => {
                let settled = compio::time::timeout(Duration::from_secs(30), rx.recv())
                    .await
                    .expect("meter e2e: fetch pending timed out")
                    .expect("meter e2e: pending delivered DispatchError");
                match settled {
                    SettledFetch::Response { status, body, .. } => (status, body),
                    _ => panic!("meter e2e: expected SettledFetch::Response"),
                }
            }
            FetchOutcome::Stream { .. } => panic!("meter e2e: unexpected Stream outcome"),
            FetchOutcome::WebSocketUpgrade { .. } => {
                panic!("meter e2e: unexpected WebSocketUpgrade outcome")
            }
        }
    })
}

fn assert_ok(status: u16, body: &str) {
    assert_eq!(status, 200, "meter e2e handler returned non-200; body: {body}");
    assert!(body.contains(r#""ok":true"#), "meter e2e handler reported failure; body: {body}");
}

#[test]
fn e2e_increment_reaches_meter() {
    let app_id = Uuid::new_v4();
    let meter = Arc::new(Meter::new());

    let (status, body) = run_app(Arc::clone(&meter), &app_id.to_string(), METER_E2E_APP);
    assert_ok(status, &body);

    // The JS asserted the return values; now assert the Rust-side meter
    // actually accumulated the totals for THIS app.
    let snap = meter.drain();
    let usage = snap.get(&app_id).expect("app present in meter after increments");
    assert_eq!(usage.requests, 4, "requests = 1 + 3");
    assert_eq!(usage.cpu_us, 1000, "fixed-name increment routes to fixed field");
    assert_eq!(usage.custom.get("emails_sent").copied(), Some(7), "custom = 2 + 5");
    assert!(
        !usage.custom.contains_key("requests") && !usage.custom.contains_key("cpu_us"),
        "fixed metric names must not leak into custom"
    );
}

#[test]
fn e2e_per_app_scoping_is_structural() {
    // Two isolates over the SAME shared meter but DIFFERENT app ids: each
    // app's increments must land only under its own id. User code never
    // names the app — it is stamped at mint time from APP_ID — so an app
    // cannot meter another.
    let app_a = Uuid::new_v4();
    let app_b = Uuid::new_v4();
    let meter = Arc::new(Meter::new());

    let (sa, ba) = run_app(Arc::clone(&meter), &app_a.to_string(), METER_E2E_APP);
    assert_ok(sa, &ba);
    let (sb, bb) = run_app(Arc::clone(&meter), &app_b.to_string(), METER_E2E_APP);
    assert_ok(sb, &bb);

    let snap = meter.drain();
    // Each app independently accumulated requests = 4.
    assert_eq!(snap.get(&app_a).unwrap().requests, 4);
    assert_eq!(snap.get(&app_b).unwrap().requests, 4);
    assert_eq!(snap.len(), 2, "exactly the two distinct apps present");
}

#[test]
fn e2e_illegal_constructor() {
    let app_id = Uuid::new_v4();
    let meter = Arc::new(Meter::new());
    let (status, body) = run_app(meter, &app_id.to_string(), METER_CTOR_APP);
    assert_ok(status, &body);
}
