// compio-postgres gained per-connection deadline state, which deepens the
// generated future here past rustc's default layout-query depth. The depth
// is in the async body, not in anything this file can restructure.
#![recursion_limit = "256"]

//! Distributed `db.live` regression against PostgreSQL and a real relay process.
//!
//! The target keeps V8 runtimes alive on separate OS threads:
//!
//! - isolate A opens an anchor subscription through the ORM relay client;
//! - isolate B serves a stream RPC backed by the installed `env.db.live`
//!   facade;
//! - isolate C performs the native insert.
//!
//! Keeping A alive until B has received the write is load-bearing. It forces
//! decoded invalidation to cross the shared ORM broker seam from A to B. The
//! writer's local fast path is suppressed while the relay client is active,
//! so C cannot make this test pass without the relay decoding the commit.
//!
//! Run with:
//!
//! ```text
//! docker compose -f deploy/compose/docker-compose.yml up -d postgres
//! cargo build -p zeroship-data-cdc-server
//! cargo test -p zeroship-data-v8 \
//!   --test distributed_live -- --test-threads=1
//! ```

use std::collections::HashMap;
#[path = "../../../tests/fixtures/postgres/mod.rs"]
mod postgres;
mod relay_fixture;
#[path = "../../../tests/fixtures/data/roles.rs"]
mod roles;
#[path = "../../../tests/fixtures/data/tracing.rs"]
mod test_tracing;
use zeroship_data_orm::cdc::relay::RelayConfig;

use std::sync::Arc;
use std::thread::{self, JoinHandle, ThreadId};
use std::time::{Duration, Instant};

use compio_postgres::{NoTls, Pool};
use zeroship_core::{AppId, BindingId, DatabaseId};
use zeroship_data_orm::binding::{DbBinding, COLD_START_DEPLOY_TOKEN};
use zeroship_data_orm::resolved_bindings::{ResolvedBinding, SuppliedAppBindings};
use zeroship_data_v8::service::{DbService, DbServiceConfig};
use zeroship_runtime::channel::{CancelFlag, StreamReader};
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, SettledFetch};

const PROBE: &str = "distributed-live-cross-isolate-probe";

/// Runtime field map matching `EVENTS_DDL`, including its assignment generators.
const RUNTIME_DESCRIPTOR: &str = r#"{
  "version": 2,
  "collections": {
    "events": {
      "fields": {
        "id": {
          "type": "string",
          "maxLength": 255,
          "required": true,
          "readable": true,
          "filterable": true,
          "sortable": true,
          "projectable": true,
          "storage": {
            "valueColumn": "id"
          },
          "assign": {
            "by": "typedId",
            "on": "insert"
          },
          "writable": false,
          "primaryKey": true
        },
        "created_at": {
          "type": "timestamp",
          "required": true,
          "readable": true,
          "filterable": true,
          "sortable": true,
          "projectable": true,
          "storage": {
            "valueColumn": "created_at"
          },
          "assign": {
            "by": "now",
            "on": "insert"
          },
          "writable": false
        },
        "updated_at": {
          "type": "timestamp",
          "required": true,
          "readable": true,
          "filterable": true,
          "sortable": true,
          "projectable": true,
          "storage": {
            "valueColumn": "updated_at"
          },
          "assign": {
            "by": "now",
            "on": "write"
          },
          "writable": false
        },
        "created_by": {
          "type": "string",
          "maxLength": 255,
          "readable": true,
          "filterable": true,
          "sortable": true,
          "projectable": true,
          "storage": {
            "valueColumn": "created_by"
          },
          "assign": {
            "by": "actor",
            "on": "insert"
          },
          "writable": false
        },
        "updated_by": {
          "type": "string",
          "maxLength": 255,
          "readable": true,
          "filterable": true,
          "sortable": true,
          "projectable": true,
          "storage": {
            "valueColumn": "updated_by"
          },
          "assign": {
            "by": "actor",
            "on": "write"
          },
          "writable": false
        },
        "version": {
          "type": "int",
          "required": true,
          "default": 1,
          "readable": true,
          "filterable": true,
          "sortable": true,
          "projectable": true,
          "storage": {
            "valueColumn": "version"
          },
          "assign": {
            "by": "increment(1)",
            "on": "write"
          },
          "writable": false
        },
        "deleted_at": {
          "type": "timestamp",
          "readable": true,
          "filterable": true,
          "sortable": true,
          "projectable": true,
          "storage": {
            "valueColumn": "deleted_at"
          },
          "assign": {
            "by": "now",
            "on": "delete"
          },
          "writable": false
        },
        "title": {
          "type": "string",
          "required": true,
          "readable": true,
          "filterable": true,
          "sortable": true,
          "projectable": true,
          "storage": {
            "valueColumn": "title"
          }
        }
      },
      "options": {
        "softDelete": false,
        "versioning": false,
        "strictness": "strict"
      },
      "indexes": [
        {
          "name": "events_deleted_at_idx",
          "fields": [
            "deleted_at"
          ]
        },
        {
          "name": "events_updated_at_idx",
          "fields": [
            "updated_at"
          ]
        },
        {
          "name": "events_created_by_idx",
          "fields": [
            "created_by"
          ]
        }
      ]
    }
  }
}"#;

/// The table [`RUNTIME_DESCRIPTOR`] describes, as `zeroship-migrate-server` would have
/// created it.
///
/// The pairing with the descriptor is the point: eight columns for eight
/// declared fields, in the same order, carrying the types the platform charter
/// injects - `varchar(255)` for the three id-bearing system columns (which is
/// what the `maxLength: 255` on each of them means), `timestamptz` for the three
/// timestamps, `integer` for `version`, and unbounded `text` for `title`, the
/// one field the descriptor gives no `maxLength`. The three indexes are the
/// charter's `ix_deleted_at` / `ix_updated_at` / `ix_created_by` under the
/// per-table names the engine emits.
///
/// The column list is checked against the live table by
/// [`assert_descriptor_matches_table`] before the exercise starts; the types are
/// not, and nothing in the tree checks them.
///
/// [`SCHEMA_SLOT`] is the only placeholder; the caller substitutes the
/// binding's schema name.
const EVENTS_DDL: &str = r#"CREATE TABLE "DB_SCHEMA"."events" (
    id VARCHAR(255) PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_by VARCHAR(255) NULL,
    updated_by VARCHAR(255) NULL,
    version INTEGER NOT NULL DEFAULT 1,
    deleted_at TIMESTAMPTZ NULL,
    title TEXT NOT NULL
)"#;

/// The three system indexes [`RUNTIME_DESCRIPTOR`] declares, by the names it
/// declares them under.
const EVENTS_INDEX_DDL: [&str; 3] = [
    r#"CREATE INDEX "events_deleted_at_idx" ON "DB_SCHEMA"."events" (deleted_at)"#,
    r#"CREATE INDEX "events_updated_at_idx" ON "DB_SCHEMA"."events" (updated_at)"#,
    r#"CREATE INDEX "events_created_by_idx" ON "DB_SCHEMA"."events" (created_by)"#,
];

/// The token [`EVENTS_DDL`] and [`EVENTS_INDEX_DDL`] carry where the binding's
/// schema name goes.
///
/// Not `{schema}`: a brace-delimited placeholder inside a plain string literal
/// is what `clippy::literal_string_with_formatting_args` is looking for, and the
/// two `.replace` call sites would each raise it.
const SCHEMA_SLOT: &str = "DB_SCHEMA";

/// One isolate's runtime, reading its binding out of the store the harness
/// resolved.
///
/// `app_bindings` is composed once, on the test's own thread, and cloned into
/// every spawn. An isolate composes no part of a binding, so handing the same
/// store to all three threads is what keeps them on one schema and one role.
fn runtime_for(
    url: &str,
    runtime_app_id: AppId,
    app_id: &str,
    app_bindings: Arc<SuppliedAppBindings>,
    relay: &RelayConfig,
    modules: Vec<ModuleEntry>,
) -> Runtime {
    let mut env_vars = HashMap::new();
    env_vars.insert("APP_ID".to_string(), app_id.to_string());
    // The ENVELOPE, not the bare descriptor. `RUNTIME_DESCRIPTOR` is the v2
    // SCHEMA descriptor and its `"version": 2` is the schema's; the document the
    // runtime validates carries its own `"version": 1` from
    // `databases::DOCUMENT_VERSION` and wraps the schema under `databases`. Two
    // different numbers share the field name, so passing the schema where the
    // document belongs fails as "must be version 1" against a 2.
    //
    // Read BEFORE `app_bindings` moves into the service config below.
    let descriptor_document = {
        let bound = app_bindings.bindings_for(app_id, COLD_START_DEPLOY_TOKEN);
        let database = bound
            .first()
            .and_then(DbBinding::database)
            .expect("the harness binding addresses a database");
        zeroship_runtime::databases::RuntimeDatabases::single(
            "main",
            database.as_str(),
            RUNTIME_DESCRIPTOR,
        )
        .expect("the harness descriptor is valid JSON")
    };
    let plugins: Vec<Arc<dyn NativePlugin>> = vec![DbService::new(DbServiceConfig {
        app_bindings,
        project_keys: Default::default(),
        connection: zeroship_data_orm::connection::ConnectionFactory::for_app_url(url)
            .expect("valid database configuration"),
        cdc_relay: Some(relay.clone()),
        meter: None,
    })
    .expect("db service")
    .plugin()];
    Runtime::builder()
        .modules(modules)
        .env_vars(env_vars)
        .plugins(plugins)
        .app_id(runtime_app_id)
        // The worker vector's `RuntimeState.runtime_descriptor` slot carries the
        // blob resolved by `crates/zeroship-worker/src/sync.rs`; native startup
        // validates it and passes it directly to plugins. All isolates in this
        // target are the same deploy, so they carry the same document.
        .runtime_descriptor(Some(descriptor_document))
        .build()
}

fn call(runtime: &Runtime, method: &str, path: &str, body: &str) -> FetchOutcome {
    runtime.call_fetch_handler(
        method,
        &format!("http://localhost{path}"),
        &[("content-type".to_string(), "application/json".to_string())],
        body,
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
    )
}

async fn settle_response(outcome: FetchOutcome) -> Result<(u16, String), String> {
    match outcome {
        FetchOutcome::Response { status, body, .. } => {
            Ok((status, String::from_utf8_lossy(&body).into_owned()))
        }
        FetchOutcome::Pending { rx, .. } => {
            let settled = compio::time::timeout(Duration::from_secs(15), rx.recv())
                .await
                .map_err(|_| "request did not settle within 15 seconds".to_string())?
                .map_err(|error| format!("request dispatch failed: {error:?}"))?;
            match settled {
                SettledFetch::Response { status, body, .. } => {
                    Ok((status, String::from_utf8_lossy(&body).into_owned()))
                }
                SettledFetch::Stream { .. } => {
                    Err("expected buffered response, got stream".to_string())
                }
                SettledFetch::WebSocketUpgrade { .. } => {
                    Err("expected buffered response, got WebSocket upgrade".to_string())
                }
            }
        }
        FetchOutcome::Stream { .. } => Err("expected buffered response, got stream".to_string()),
        FetchOutcome::WebSocketUpgrade { .. } => {
            Err("expected buffered response, got WebSocket upgrade".to_string())
        }
    }
}

async fn settle_stream(
    outcome: FetchOutcome,
) -> Result<(u16, Vec<(String, String)>, StreamReader), String> {
    match outcome {
        FetchOutcome::Stream {
            status,
            headers,
            body_reader,
            ..
        } => Ok((status, headers, body_reader)),
        FetchOutcome::Pending { rx, .. } => {
            let settled = compio::time::timeout(Duration::from_secs(15), rx.recv())
                .await
                .map_err(|_| "stream RPC did not settle within 15 seconds".to_string())?
                .map_err(|error| format!("stream RPC dispatch failed: {error:?}"))?;
            match settled {
                SettledFetch::Stream {
                    status,
                    headers,
                    body_reader,
                    ..
                } => Ok((status, headers, body_reader)),
                SettledFetch::Response { status, body, .. } => Err(format!(
                    "expected stream RPC, got response status={status} body={}",
                    String::from_utf8_lossy(&body)
                )),
                SettledFetch::WebSocketUpgrade { .. } => {
                    Err("expected stream RPC, got WebSocket upgrade".to_string())
                }
            }
        }
        FetchOutcome::Response { status, body, .. } => Err(format!(
            "expected stream RPC, got response status={status} body={}",
            String::from_utf8_lossy(&body)
        )),
        FetchOutcome::WebSocketUpgrade { .. } => {
            Err("expected stream RPC, got WebSocket upgrade".to_string())
        }
    }
}

fn anchor_modules() -> Vec<ModuleEntry> {
    vec![ModuleEntry {
        specifier: "index.js".to_string(),
        source: r#"
import { env } from "zeroship";

let held;

function fetchHandler(request) {
    const path = new URL(request.url).pathname;
    if (path === "/close") {
        if (held) held.close();
        held = undefined;
        return new Response("closed", { status: 200 });
    }
    if (path === "/arm") {
        return (async () => {
            try {
                held = env.db.collection("events").openSubscription();
                await held.ready();
                return new Response("ready", { status: 200 });
            } catch (error) {
                return new Response(JSON.stringify({
                    code: error?.code,
                    message: error?.message,
                }), { status: 418 });
            }
        })();
    }
    return new Response("not found", { status: 404 });
}

export default { fetch: fetchHandler };
"#
        .to_string(),
    }]
}

fn writer_modules() -> Vec<ModuleEntry> {
    vec![ModuleEntry {
        specifier: "index.js".to_string(),
        source: format!(
            r#"
import {{ env }} from "zeroship";

async function fetchHandler(request) {{
    if (new URL(request.url).pathname !== "/write") {{
        return new Response("not found", {{ status: 404 }});
    }}
    const row = await env.db.collection("events").insert({{
        title: {probe:?},
    }});
    return new Response(JSON.stringify(row), {{
        status: 200,
        headers: {{ "content-type": "application/json" }},
    }});
}}

export default {{ fetch: fetchHandler }};
"#,
            probe = PROBE,
        ),
    }]
}

fn subscriber_modules() -> Vec<ModuleEntry> {
    let entry = ModuleEntry {
        specifier: "index.js".to_string(),
        source: r#"
import { env } from "zeroship";

async function* todosSubscribe() {
    const live = env.db.live(
        () => env.db.collection("events").find({}, {}),
        { tables: ["events"] },
    );
    let frames = 0;
    try {
        for await (const rows of live) {
            yield rows;
            frames += 1;
            if (frames === 2) return;
        }
    } finally {
        live.close();
    }
}
todosSubscribe.config = { kind: "stream" };

const procedures = { "todos.subscribe": todosSubscribe };

async function dispatch(name, input) {
    const procedure = procedures[name];
    if (typeof procedure !== "function") {
        throw Object.assign(new Error("method not found: " + name), { status: 404 });
    }
    return procedure(input);
}

async function fetchHandler(request) {
    const prefix = "/__zeroship/v1/";
    const path = new URL(request.url).pathname;
    if (!path.startsWith(prefix)) return new Response("not found", { status: 404 });
    const name = decodeURIComponent(path.slice(prefix.length));
    const envelope = await request.json();
    const result = await dispatch(name, envelope?.json);
    if (!result || typeof result[Symbol.asyncIterator] !== "function") {
        return new Response("stream handler returned a non-iterator", { status: 500 });
    }
    const encoder = new TextEncoder();
    const stream = new ReadableStream({
        async start(controller) {
            try {
                while (true) {
                    const step = await result.next();
                    if (step.done) {
                        controller.enqueue(encoder.encode("d:{}\n"));
                        break;
                    }
                    controller.enqueue(
                        encoder.encode("2:[" + JSON.stringify(step.value) + "]\n"),
                    );
                }
            } catch (error) {
                controller.enqueue(encoder.encode("e:" + JSON.stringify({
                    message: error?.message ?? String(error),
                    code: error?.code,
                }) + "\n"));
                controller.enqueue(encoder.encode("d:{}\n"));
            } finally {
                controller.close();
            }
        },
    });
    return new Response(stream, {
        status: 200,
        headers: { "content-type": "text/event-stream" },
    });
}

export default { fetch: fetchHandler, rpc: procedures };
"#
        .to_string(),
    };
    vec![entry]
}

#[derive(Debug)]
struct AnchorReady {
    thread_id: ThreadId,
}

#[derive(Debug)]
struct SubscriberResult {
    thread_id: ThreadId,
    body: String,
}

#[derive(Debug)]
struct WriterResult {
    thread_id: ThreadId,
    status: u16,
    body: String,
}

/// The anchor thread's four channel ends, carried together because they are one
/// handshake (ready -> close -> closed -> finish) rather than four parameters.
struct AnchorChannels {
    ready: std::sync::mpsc::Sender<Result<AnchorReady, String>>,
    close: flume::Receiver<()>,
    closed: std::sync::mpsc::Sender<Result<(), String>>,
    finish: flume::Receiver<()>,
}

fn spawn_anchor(
    url: String,
    runtime_app_id: AppId,
    app_id: String,
    app_bindings: Arc<SuppliedAppBindings>,
    relay: RelayConfig,
    channels: AnchorChannels,
) -> JoinHandle<Result<(), String>> {
    let AnchorChannels {
        ready,
        close,
        closed,
        finish,
    } = channels;
    thread::spawn(move || {
        init_v8();
        let thread_id = thread::current().id();
        let runtime = runtime_for(
            &url,
            runtime_app_id,
            &app_id,
            app_bindings,
            &relay,
            anchor_modules(),
        );
        let arm = call(&runtime, "GET", "/arm", "");
        let io = compio::runtime::Runtime::new()
            .map_err(|error| format!("anchor compio runtime: {error}"))?;
        io.block_on(async {
            runtime.start_pump();
            let (status, body) = settle_response(arm).await?;
            if status != 200 {
                return Err(format!(
                    "anchor readiness failed: status={status} body={body}"
                ));
            }
            ready
                .send(Ok(AnchorReady { thread_id }))
                .map_err(|_| "anchor readiness receiver dropped".to_string())?;

            let _ = compio::time::timeout(Duration::from_secs(30), close.recv_async()).await;
            let close_result = settle_response(call(&runtime, "POST", "/close", "")).await;
            match close_result {
                Ok((200, _)) => {
                    let _ = closed.send(Ok(()));
                }
                Ok((status, body)) => {
                    let message = format!("anchor close failed: status={status} body={body}");
                    let _ = closed.send(Err(message.clone()));
                    return Err(message);
                }
                Err(error) => {
                    let _ = closed.send(Err(error.clone()));
                    return Err(error);
                }
            }

            let _ = compio::time::timeout(Duration::from_secs(15), finish.recv_async()).await;
            Ok(())
        })
        .inspect_err(|error| {
            let _ = ready.send(Err(error.clone()));
        })
    })
}

fn spawn_subscriber(
    url: String,
    runtime_app_id: AppId,
    app_id: String,
    app_bindings: Arc<SuppliedAppBindings>,
    relay: RelayConfig,
    initial: std::sync::mpsc::Sender<Result<(ThreadId, String), String>>,
) -> JoinHandle<Result<SubscriberResult, String>> {
    thread::spawn(move || {
        init_v8();
        let thread_id = thread::current().id();
        let runtime = runtime_for(
            &url,
            runtime_app_id,
            &app_id,
            app_bindings,
            &relay,
            subscriber_modules(),
        );
        let outcome = call(
            &runtime,
            "POST",
            "/__zeroship/v1/todos.subscribe",
            r#"{"json":null}"#,
        );
        let io = compio::runtime::Runtime::new()
            .map_err(|error| format!("subscriber compio runtime: {error}"))?;
        let result = io.block_on(async {
            runtime.start_pump();
            let (status, headers, reader) = settle_stream(outcome).await?;
            if status != 200 {
                return Err(format!("stream RPC returned status {status}"));
            }
            let is_sse = headers.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case("content-type")
                    && value.eq_ignore_ascii_case("text/event-stream")
            });
            if !is_sse {
                return Err(format!(
                    "stream RPC missing text/event-stream header: {headers:?}"
                ));
            }

            let deadline = Instant::now() + Duration::from_secs(20);
            let mut bytes = Vec::new();
            let mut sent_initial = false;
            loop {
                while let Some(chunk) = reader.pop() {
                    bytes.extend_from_slice(&chunk);
                }
                let body = String::from_utf8_lossy(&bytes).into_owned();
                if !sent_initial && body.lines().any(|line| line.starts_with("2:")) {
                    initial
                        .send(Ok((thread_id, body.clone())))
                        .map_err(|_| "initial-frame receiver dropped".to_string())?;
                    sent_initial = true;
                }
                if reader.is_done() {
                    if !sent_initial {
                        return Err(format!(
                            "stream RPC ended before its initial data frame: {body:?}"
                        ));
                    }
                    return Ok(SubscriberResult { thread_id, body });
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero()
                    || compio::time::timeout(remaining, reader.wait_for_data())
                        .await
                        .is_err()
                {
                    return Err(format!("stream RPC stalled: partial body={body:?}"));
                }
            }
        });
        if let Err(error) = &result {
            let _ = initial.send(Err(error.clone()));
        }
        result
    })
}

fn spawn_writer(
    url: String,
    runtime_app_id: AppId,
    app_id: String,
    app_bindings: Arc<SuppliedAppBindings>,
    relay: RelayConfig,
) -> JoinHandle<Result<WriterResult, String>> {
    thread::spawn(move || {
        init_v8();
        let thread_id = thread::current().id();
        let runtime = runtime_for(
            &url,
            runtime_app_id,
            &app_id,
            app_bindings,
            &relay,
            writer_modules(),
        );
        let outcome = call(&runtime, "POST", "/write", "{}");
        let io = compio::runtime::Runtime::new()
            .map_err(|error| format!("writer compio runtime: {error}"))?;
        io.block_on(async {
            runtime.start_pump();
            let (status, body) = settle_response(outcome).await?;
            Ok(WriterResult {
                thread_id,
                status,
                body,
            })
        })
    })
}

fn receive<T>(
    receiver: &std::sync::mpsc::Receiver<Result<T, String>>,
    label: &str,
) -> Result<T, String> {
    receiver
        .recv_timeout(Duration::from_secs(20))
        .map_err(|error| format!("timed out waiting for {label}: {error}"))?
}

fn join_role<T>(handle: JoinHandle<Result<T, String>>, label: &str) -> Result<T, String> {
    handle
        .join()
        .map_err(|_| format!("{label} thread panicked"))?
}

async fn slot_state(pool: &Pool, slot: &str) -> Result<Option<bool>, String> {
    let rows = pool
        .query_text_params(
            "SELECT active FROM pg_replication_slots WHERE slot_name = $1",
            &[slot],
        )
        .await
        .map_err(|error| format!("query replication slot: {error}"))?;
    Ok(rows.first().map(|row| row.get::<_, bool>("active")))
}

/// Refuse to run unless [`RUNTIME_DESCRIPTOR`] and [`EVENTS_DDL`] describe the
/// same columns. The ORM compiles projections from the descriptor without
/// consulting the catalog, so this fixture checks its two inputs directly.
async fn assert_descriptor_matches_table(pool: &Pool, schema: &str) -> Result<(), String> {
    let descriptor: serde_json::Value = serde_json::from_str(RUNTIME_DESCRIPTOR)
        .map_err(|error| format!("RUNTIME_DESCRIPTOR is not valid JSON: {error}"))?;
    let mut declared = descriptor["collections"]["events"]["fields"]
        .as_object()
        .ok_or_else(|| "RUNTIME_DESCRIPTOR declares no `events.fields` object".to_string())?
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    declared.sort();

    let rows = pool
        .query_text_params(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = 'events'",
            &[schema],
        )
        .await
        .map_err(|error| format!("read the events table's columns: {error}"))?;
    let mut physical = rows
        .iter()
        .map(|row| row.get::<_, String>("column_name"))
        .collect::<Vec<_>>();
    physical.sort();

    if declared != physical {
        return Err(format!(
            "the runtime descriptor and the events table disagree; \
             descriptor declares {declared:?}, the table has {physical:?}"
        ));
    }
    Ok(())
}

async fn publication_exists(pool: &Pool, publication: &str) -> Result<bool, String> {
    pool.query_text_params(
        "SELECT 1 FROM pg_publication WHERE pubname = $1",
        &[publication],
    )
    .await
    .map(|rows| !rows.is_empty())
    .map_err(|error| format!("query publication: {error}"))
}

/// Stand in for `zeroship-migrate-server`, which owns the app publication.
///
/// The relay requires an existing publication and never chooses its members.
/// The harness supplies that migration-owned fixture with table-owner authority.
/// `events` must be a member for pgoutput to deliver its changes.
///
/// The publication is named after the APP, which is what the relay's source
/// derives and asks for; its members are qualified with the BINDING's schema,
/// which is where the table actually is.
async fn create_app_publication(
    pool: &Pool,
    schema: &str,
    publication: &str,
    tables: &[&str],
) -> Result<(), String> {
    let members = tables
        .iter()
        .map(|table| format!(r#""{schema}"."{table}""#))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = if members.is_empty() {
        format!(r#"CREATE PUBLICATION "{publication}""#)
    } else {
        format!(r#"CREATE PUBLICATION "{publication}" FOR TABLE {members}"#)
    };
    pool.execute(&sql, &[])
        .await
        .map(|_| ())
        .map_err(|error| format!("create migration-owned publication: {error}"))
}

#[test]
fn db_live_stream_crosses_relay_and_v8_isolates_without_worker_replication() {
    init_v8();
    test_tracing::init_test_tracing();
    let postgres = postgres::Postgres::start();
    let url = postgres.url();
    let runtime_app_id = AppId::mint();
    let app_id = runtime_app_id.as_str().to_string();
    // The edge a trusted host would have resolved for this app, composed once
    // on this thread. It is the only source of the schema the fixture
    // provisions and of the role every session narrows to, so the three
    // isolates below cannot drift onto a database of their own.
    let binding = DbBinding::to_database(
        app_id.as_str(),
        COLD_START_DEPLOY_TOKEN,
        DatabaseId::mint(),
        BindingId::mint(),
        zeroship_core::database_role::DatabaseCapability::ReadWrite,
    )
    .expect("a minted database and edge compose a legal role name");
    let schema = binding.schema().as_str().to_string();
    let app_bindings = Arc::new(SuppliedAppBindings::new());
    app_bindings
        .supply(
            &app_id,
            ResolvedBinding::from(binding.edge().expect("the binding addresses a database")),
        )
        .expect("a fresh store accepts its first binding");
    let publication = zeroship_core::replication_names::DATASTORE_PUBLICATION;
    let slot = zeroship_core::replication_names::relay_slot_name(
        &app_id,
        binding
            .database()
            .expect("the binding addresses a database")
            .as_str(),
    )
    .unwrap();

    let io = compio::runtime::Runtime::new().expect("control compio runtime");
    let pool = io.block_on(async {
        let (probe, connection) =
            compio_postgres::connect(&url, NoTls)
                .await
                .unwrap_or_else(|error| {
                    panic!(
                    "could not connect to the distributed live PostgreSQL fixture at {url}: {error}"
                )
                });
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();
        drop(probe);

        let pool = Pool::connect(&url, 4).await.expect("connect control pool");
        pool.execute(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"), &[])
            .await
            .expect("drop stale test schema");
        pool.execute(&format!("CREATE SCHEMA \"{schema}\""), &[])
            .await
            .expect("create test schema");
        pool.execute(&EVENTS_DDL.replace(SCHEMA_SLOT, &schema), &[])
            .await
            .expect("create events table");
        for index in EVENTS_INDEX_DDL {
            pool.execute(&index.replace(SCHEMA_SLOT, &schema), &[])
                .await
                .expect("create events system index");
        }
        assert_descriptor_matches_table(&pool, &schema)
            .await
            .expect("runtime descriptor agrees with the events table");
        // The ladder last, so its schema-wide grants reach the table that is
        // already there. It issues the capability role's privileges, hands the
        // binding role exactly that one role `WITH SET FALSE`, and admits this
        // admin login to the binding role; the worker login's own edge is the
        // relay fixture's.
        roles::ensure_binding_ladder(&pool, &binding)
            .await
            .expect("provision the binding role ladder");
        create_app_publication(&pool, &schema, publication, &["events"])
            .await
            .expect("create migration-owned publication");
        pool
    });

    let relay = io.block_on(relay_fixture::RelayFixture::start(&pool, &url, &binding));
    let worker_url = relay.worker_url.clone();
    let (anchor_ready_tx, anchor_ready_rx) = std::sync::mpsc::channel();
    let (close_tx, close_rx) = flume::bounded(1);
    let (closed_tx, closed_rx) = std::sync::mpsc::channel();
    let (finish_tx, finish_rx) = flume::bounded(1);
    let anchor = spawn_anchor(
        worker_url.clone(),
        runtime_app_id.clone(),
        app_id.clone(),
        app_bindings.clone(),
        relay.config.clone(),
        AnchorChannels {
            ready: anchor_ready_tx,
            close: close_rx,
            closed: closed_tx,
            finish: finish_rx,
        },
    );

    let mut subscriber: Option<JoinHandle<Result<SubscriberResult, String>>> = None;
    let mut writer: Option<JoinHandle<Result<WriterResult, String>>> = None;
    let exercise = (|| -> Result<(AnchorReady, ThreadId, String, WriterResult), String> {
        let anchor_ready = receive(&anchor_ready_rx, "anchor CDC readiness")?;

        let (initial_tx, initial_rx) = std::sync::mpsc::channel();
        subscriber = Some(spawn_subscriber(
            worker_url.clone(),
            runtime_app_id.clone(),
            app_id.clone(),
            app_bindings.clone(),
            relay.config.clone(),
            initial_tx,
        ));
        let (subscriber_thread, initial_body) = receive(&initial_rx, "subscriber initial frame")?;
        if initial_body.contains(PROBE) {
            return Err(format!(
                "probe was present before the writer ran: {initial_body:?}"
            ));
        }

        writer = Some(spawn_writer(
            worker_url.clone(),
            runtime_app_id,
            app_id.clone(),
            app_bindings.clone(),
            relay.config.clone(),
        ));
        let writer_result = join_role(writer.take().expect("writer handle"), "writer")?;
        if writer_result.status != 200 || !writer_result.body.contains(PROBE) {
            return Err(format!(
                "writer failed before cross-isolate delivery: status={} body={:?}",
                writer_result.status, writer_result.body
            ));
        }
        let subscriber_result =
            join_role(subscriber.take().expect("subscriber handle"), "subscriber").map_err(
                |error| {
                    format!(
                        "writer status={} body={:?}; {error}",
                        writer_result.status, writer_result.body
                    )
                },
            )?;
        if subscriber_result.thread_id != subscriber_thread {
            return Err("subscriber thread identity changed".to_string());
        }

        let data_frames = subscriber_result
            .body
            .lines()
            .filter(|line| line.starts_with("2:"))
            .count();
        if data_frames != 2 || !subscriber_result.body.contains(PROBE) {
            return Err(format!(
                "expected initial plus probe frames, got {data_frames}: {:?}",
                subscriber_result.body
            ));
        }
        if !subscriber_result.body.lines().any(|line| line == "d:{}") {
            return Err(format!(
                "stream RPC did not finish cleanly: {:?}",
                subscriber_result.body
            ));
        }

        let state = io.block_on(slot_state(&pool, &slot))?;
        if state != Some(true) {
            return Err(format!(
                "anchor should keep the relay slot active after the stream closes; state={state:?}"
            ));
        }

        Ok((
            anchor_ready,
            subscriber_thread,
            subscriber_result.body,
            writer_result,
        ))
    })();

    if let Some(handle) = writer.take() {
        let _ = join_role(handle, "writer cleanup");
    }
    if let Some(handle) = subscriber.take() {
        let _ = join_role(handle, "subscriber cleanup");
    }

    let _ = close_tx.try_send(());
    let close_result = receive(&closed_rx, "anchor close");
    let slot_removed = io.block_on(async {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match slot_state(&pool, &slot).await? {
                None => return Ok::<bool, String>(true),
                Some(_) if Instant::now() >= deadline => return Ok(false),
                Some(_) => compio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    });
    let _ = finish_tx.try_send(());
    let anchor_result = join_role(anchor, "anchor");

    let app_delete_result = io.block_on(async {
        // The store the isolates ran against, so retiring the app retires the
        // binding they narrowed to rather than a store that never held one.
        let service = DbService::new(DbServiceConfig {
            app_bindings: app_bindings.clone(),
            project_keys: Default::default(),
            connection: zeroship_data_orm::connection::ConnectionFactory::for_app_url(&url)
                .expect("valid database configuration"),
            cdc_relay: None,
            meter: None,
        })
        .map_err(|error| format!("db service: {error}"))?;
        service
            .lifecycle()
            .deprovision_app(&app_id)
            .await
            .map_err(|error| format!("deprovision app CDC: {error}"))?;
        // Local subscription teardown leaves migration-owned publications
        // intact. Only the operator fixture cleans up this publication.
        let publication_retained = publication_exists(&pool, publication).await?;
        pool.execute(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"), &[])
            .await
            .map_err(|error| format!("drop test schema: {error}"))?;
        // After the schema, so the capability role carries no privilege and no
        // default ACL when it is dropped.
        roles::drop_binding_ladder(&pool, &binding)
            .await
            .map_err(|error| format!("drop the binding role ladder: {error}"))?;
        // Stand in for the privileged reconciler again, so the shared test
        // server does not accumulate one publication per run of this target.
        pool.execute(
            &format!(r#"DROP PUBLICATION IF EXISTS "{publication}""#),
            &[],
        )
        .await
        .map_err(|error| format!("drop test publication: {error}"))?;
        Ok::<bool, String>(publication_retained)
    });

    io.block_on(relay.cleanup(&pool));
    drop(pool);
    io.block_on(async {
        let _ = compio_postgres::drain_connections(Duration::from_secs(3)).await;
    });

    let (anchor_ready, subscriber_thread, body, writer_result) =
        exercise.unwrap_or_else(|error| panic!("distributed live exercise failed: {error}"));
    close_result.unwrap_or_else(|error| panic!("last-subscriber close failed: {error}"));
    anchor_result.unwrap_or_else(|error| panic!("anchor failed: {error}"));
    assert!(
        slot_removed.expect("slot teardown query"),
        "last subscriber close must drop the relay logical slot"
    );
    assert!(
        app_delete_result.expect("app CDC deprovision"),
        "worker deprovision must leave the migration-owned publication in place"
    );
    assert_eq!(
        writer_result.status, 200,
        "writer body={}",
        writer_result.body
    );
    assert!(body.contains(PROBE), "subscriber body={body:?}");

    assert_ne!(anchor_ready.thread_id, subscriber_thread);
    assert_ne!(anchor_ready.thread_id, writer_result.thread_id);
    assert_ne!(subscriber_thread, writer_result.thread_id);
}
