//! The replay bridge is a host module, not creator glue.
//!
//! Two properties this file binds, neither of which the compiler can see:
//! creator code cannot reach `zeroship:workflows/dispatch`, and the very first
//! dispatch in a fresh isolate instantiates it. Nothing injects the bridge into
//! the creator's module graph any more, so a runtime that never instantiates it
//! would compile and would only fail at the first replay.

use serde_json::{json, Value};
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::{EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime};

use super::{build_runtime, build_runtime_with, dispatch_workflow};

const COUNTER_WORKFLOW: &str = r"
    let dispatches = 0;
    export class Counter {
        run() { dispatches += 1; return dispatches; }
    }
";

fn envelope(name: &str, nonce: &str) -> String {
    json!({
        "runId": "wfr_host_module", "nonce": nonce, "workflowName": name,
        "phase": "running", "trigger": { "input": null }, "journal": [],
    })
    .to_string()
}

fn fetch(runtime: &Runtime) -> (u16, String) {
    let outcome = runtime.call_fetch_handler(
        "GET",
        "http://localhost/",
        &[],
        "",
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
    );
    match outcome {
        FetchOutcome::Response { status, body, .. } => {
            (status, String::from_utf8(body).unwrap())
        }
        _ => panic!("fixture must settle without external I/O"),
    }
}

/// The very first thing this isolate does is replay a workflow. Nothing else
/// has imported or evaluated the bridge, so the dispatch path has to
/// instantiate the host module itself.
#[test]
fn first_dispatch_in_a_fresh_isolate_instantiates_the_bridge() {
    let runtime = build_runtime(COUNTER_WORKFLOW);
    let result = dispatch_workflow(&runtime, &envelope("Counter", "first"));
    assert_eq!(result["kind"], "RunCompleted", "{result}");
    assert_eq!(result["output"], json!(1), "{result}");
}

/// Replay reads the creator entry module the host entry imported, not a second
/// evaluation of it: module-scope state a request mutated is visible to the
/// workflow, and the workflow's own mutation survives into the next dispatch.
#[test]
fn dispatch_and_fetch_share_one_creator_module_instance() {
    let runtime = build_runtime(
        r"
        let dispatches = 0;
        export class Counter {
            run() { dispatches += 1; return { dispatches, fetched: globalThis.__fetched ?? 0 }; }
        }
        export default { fetch() {
            globalThis.__fetched = (globalThis.__fetched ?? 0) + 1;
            return Response.json({ dispatches });
        } };
    ",
    );
    let (status, body) = fetch(&runtime);
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({ "dispatches": 0 })
    );

    let first = dispatch_workflow(&runtime, &envelope("Counter", "one"));
    assert_eq!(first["kind"], "RunCompleted", "{first}");
    assert_eq!(first["output"], json!({ "dispatches": 1, "fetched": 1 }));

    let second = dispatch_workflow(&runtime, &envelope("Counter", "two"));
    assert_eq!(second["kind"], "RunCompleted", "{second}");
    assert_eq!(second["output"], json!({ "dispatches": 2, "fetched": 1 }));

    // The request path observes the counter the replays advanced, so both read
    // one module instance rather than two copies of its top-level state.
    let (status, body) = fetch(&runtime);
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({ "dispatches": 2 })
    );
}

/// A static creator import of the bridge must not resolve. The refusal is the
/// reason the bridge needs no unguessable specifier: `zeroship:` host modules
/// are unreachable from the creator graph by construction.
#[test]
fn creator_static_import_cannot_resolve_the_dispatch_module() {
    let runtime = build_runtime_with(vec![ModuleEntry {
        specifier: "index.js".into(),
        source: r#"
            import "zeroship:workflows/dispatch";
            export default { fetch() { return new Response("unexpected import"); } };
        "#
        .into(),
    }]);
    let (status, body) = fetch(&runtime);
    assert_eq!(status, 500, "{body}");
    assert!(body.contains("cannot import host-only module"), "{body}");
}

/// The dynamic form is refused as a missing module, the same way every other
/// host-only adapter is.
#[test]
fn creator_dynamic_import_cannot_resolve_the_dispatch_module() {
    let runtime = build_runtime_with(vec![ModuleEntry {
        specifier: "index.js".into(),
        source: r#"
            export default { async fetch() {
                try { await import("zeroship:workflows/dispatch"); }
                catch (error) { return new Response(error.message); }
                return new Response("unexpected import");
            } };
        "#
        .into(),
    }]);
    let (status, body) = fetch(&runtime);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, "Cannot find module 'zeroship:workflows/dispatch'");
}

/// Creator artifacts cannot supply the bridge either: a module entry under the
/// host specifier is refused before anything evaluates.
#[test]
fn creator_artifact_cannot_supply_the_dispatch_module() {
    let runtime = build_runtime_with(vec![
        ModuleEntry {
            specifier: "index.js".into(),
            source: COUNTER_WORKFLOW.into(),
        },
        ModuleEntry {
            specifier: "zeroship:workflows/dispatch".into(),
            source: "export function dispatch() { return { kind: 'RunCompleted' }; }".into(),
        },
    ]);
    let (status, body) = fetch(&runtime);
    assert_eq!(status, 500, "{body}");
    assert!(body.contains("reserved host specifier"), "{body}");
}
