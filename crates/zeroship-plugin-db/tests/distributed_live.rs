// compio-postgres gained per-connection deadline state, which deepens the
// generated future here past rustc's default layout-query depth. The depth
// is in the async body, not in anything this file can restructure.
#![recursion_limit = "256"]

//! Distributed `db.live` regression against the compose Postgres.
//!
//! This target deliberately owns three V8 runtimes on three concurrently
//! alive OS threads:
//!
//! - isolate A opens the first native subscription and therefore owns this
//!   worker process's logical-decoding consumer;
//! - isolate B serves a stream RPC backed by the shipped `createLive` SDK
//!   implementation;
//! - isolate C performs the native insert.
//!
//! Keeping A alive until B has received the write is load-bearing. It forces
//! the decoded WAL event to cross the process broker seam from A to B. The
//! writer's local fast path is suppressed while the WAL consumer is active,
//! so C cannot make this test pass without logical decoding.
//!
//! Run with:
//!
//! ```text
//! docker compose -f deploy/compose/docker-compose.yml up -d postgres
//! cargo test -p zeroship-plugin-db --features live-db-tests \
//!   --test distributed_live -- --test-threads=1
//! ```

use std::collections::HashMap;
mod support;

use std::sync::Arc;
use std::thread::{self, JoinHandle, ThreadId};
use std::time::{Duration, Instant};

use compio_postgres::{NoTls, Pool};
use zeroship_plugin_db::service::{DbService, DbServiceConfig};
use zeroship_runtime::channel::{CancelFlag, StreamReader};
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, SettledFetch,
};

const PROBE: &str = "distributed-live-cross-isolate-probe";

/// The deploy's `manifest.runtime_descriptor`, verbatim.
///
/// **Why the fixture carries one at all.** The data plane's sole schema
/// authority is this document: `crate::descriptor::collection_schema`
/// (`crates/zeroship-plugin-db/src/descriptor.rs:66-80`) resolves a collection
/// out of the per-isolate store or refuses it with `collection_not_declared`,
/// and the only writer of that store is `register_model_dispatch`
/// (`crates/zeroship-plugin-db/src/register_model/mod.rs:104-106`), which
/// `installSchema` drives off `globalThis.__zsRuntimeDescriptor`
/// (`sdks/bootstrap/src/runtime-entry.ts:77-167`). A deploy that ships no
/// descriptor is a schema-less app and gets no `env.db` collections - see
/// `docs/reference/zeroship-standard.md`. This target used to reach `events`
/// anyway, through a live-catalog fallback that no longer exists; shipping the
/// descriptor is what makes it exercise the documented deploy shape instead.
///
/// **Provenance.** These bytes are the migration fold's own output, not a
/// hand-invented shape that merely parses. They were produced by
/// `zeroship_migrate::render_artifacts_from_descriptors` for the collection
/// `events { title: t.string() /* required */ }` on the PostgreSQL dialect
/// (`zeroship_migrate_postgres::DIALECT`), under the shipped platform charter
/// (`policies/confined-system-shape.inject.toml` - the same seven system
/// columns, primary key and three system indexes every creator table gets).
/// That is why every field carries `readable`/`filterable`/`sortable`/
/// `projectable` and a `storage` block naming its physical column
/// (`crates/zeroship-migrate-core/src/render/gen_types.rs:327-360`), and why
/// `version` is `2`: v1 is refused outright by `assertRuntimeDescriptorV2`
/// (`sdks/bootstrap/src/install-schema.ts:162-179`) and by
/// `validate_runtime_descriptor_value`
/// (`crates/zeroship-runtime/src/core/init.rs:3734-3740`).
///
/// **It must agree with [`EVENTS_DDL`], column for column.** Nothing checks the
/// descriptor against the catalog any more, so a field here that the table does
/// not have surfaces as a Postgres `42703 column does not exist` at read time:
/// `implicit_read_projection_parts`
/// (`crates/zeroship-schema/src/query.rs:3344-3365`) projects the seven system
/// fields plus every non-system key of this map, by name.
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
          "storage": { "valueColumn": "id" }
        },
        "created_at": {
          "type": "date",
          "required": true,
          "readable": true,
          "filterable": true,
          "sortable": true,
          "projectable": true,
          "storage": { "valueColumn": "created_at" }
        },
        "updated_at": {
          "type": "date",
          "required": true,
          "readable": true,
          "filterable": true,
          "sortable": true,
          "projectable": true,
          "storage": { "valueColumn": "updated_at" }
        },
        "created_by": {
          "type": "string",
          "maxLength": 255,
          "readable": true,
          "filterable": true,
          "sortable": true,
          "projectable": true,
          "storage": { "valueColumn": "created_by" }
        },
        "updated_by": {
          "type": "string",
          "maxLength": 255,
          "readable": true,
          "filterable": true,
          "sortable": true,
          "projectable": true,
          "storage": { "valueColumn": "updated_by" }
        },
        "version": {
          "type": "int",
          "required": true,
          "default": 1,
          "readable": true,
          "filterable": true,
          "sortable": true,
          "projectable": true,
          "storage": { "valueColumn": "version" }
        },
        "deleted_at": {
          "type": "date",
          "readable": true,
          "filterable": true,
          "sortable": true,
          "projectable": true,
          "storage": { "valueColumn": "deleted_at" }
        },
        "title": {
          "type": "string",
          "required": true,
          "readable": true,
          "filterable": true,
          "sortable": true,
          "projectable": true,
          "storage": { "valueColumn": "title" }
        }
      },
      "options": {
        "softDelete": false,
        "versioning": false,
        "strictness": "strict"
      },
      "indexes": [
        { "name": "events_deleted_at_idx", "fields": ["deleted_at"] },
        { "name": "events_updated_at_idx", "fields": ["updated_at"] },
        { "name": "events_created_by_idx", "fields": ["created_by"] }
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
/// [`APP_SCHEMA_SLOT`] is the only placeholder; the caller substitutes the
/// per-app schema name.
const EVENTS_DDL: &str = r#"CREATE TABLE "APP_SCHEMA"."events" (
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
    r#"CREATE INDEX "events_deleted_at_idx" ON "APP_SCHEMA"."events" (deleted_at)"#,
    r#"CREATE INDEX "events_updated_at_idx" ON "APP_SCHEMA"."events" (updated_at)"#,
    r#"CREATE INDEX "events_created_by_idx" ON "APP_SCHEMA"."events" (created_by)"#,
];

/// The token [`EVENTS_DDL`] and [`EVENTS_INDEX_DDL`] carry where the per-app
/// schema name goes.
///
/// Not `{app_id}`: a brace-delimited placeholder inside a plain string literal
/// is what `clippy::literal_string_with_formatting_args` is looking for, and the
/// two `.replace` call sites would each raise it.
const APP_SCHEMA_SLOT: &str = "APP_SCHEMA";

/// The database this target dials, or a panic naming the provisioner.
///
/// It used to fall back to `127.0.0.1:5440/zeroship` -- the SHARED platform
/// database on the dev cluster, named here and nowhere the harness could see.
/// `crates/core/src/config/test_overlay.rs` cited this file by name as the one
/// call site that "substitutes a default"; it no longer does.
fn pg_url() -> String {
    // Single funnel for this binary: without a subscriber the runtime's
    // sanitization rail leaves every dispatch failure as a bare
    // {"message":"internal error"}. No-op unless RUST_LOG is set.
    support::init_test_tracing();
    zeroship_core::config::test_database_url()
}

fn runtime_for(
    url: &str,
    app_uuid: uuid::Uuid,
    app_id: &str,
    worker_id: &str,
    modules: Vec<ModuleEntry>,
) -> Runtime {
    let mut env_vars = HashMap::new();
    env_vars.insert("APP_ID".to_string(), app_id.to_string());
    let plugins: Vec<Arc<dyn NativePlugin>> = vec![
        DbService::new(DbServiceConfig {
            url: url.to_string(),
            worker_id: worker_id.to_string(),
            meter: None,
        })
        .expect("db service")
        .plugin(),
    ];
    Runtime::builder()
        .modules(modules)
        .env_vars(env_vars)
        .plugins(plugins)
        .app_id(app_uuid)
        // The worker vector's `RuntimeState.runtime_descriptor` slot
        // (`crates/zeroship-worker/src/sync.rs:40-70` resolves the blob;
        // `crates/zeroship-runtime/src/core/init.rs:3415-3434` validates it and
        // exposes it as `globalThis.__zsRuntimeDescriptor`). All three isolates
        // in this target are the same deploy, so they carry the same document.
        .runtime_descriptor(Some(RUNTIME_DESCRIPTOR.to_string()))
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
        FetchOutcome::Stream { .. } => {
            Err("expected buffered response, got stream".to_string())
        }
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
            held = env.db.collection("events").openSubscription();
            await held.ready();
            return new Response("ready", { status: 200 });
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
import { createLive } from "@zeroship/db/internal";

async function* todosSubscribe() {
    const live = createLive(
        {},
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
    let sdk = ModuleEntry {
        specifier: "@zeroship/db/internal".to_string(),
        source: include_str!("../../../sdks/db/dist/internal.js").to_string(),
    };
    vec![entry, sdk]
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
    app_uuid: uuid::Uuid,
    app_id: String,
    worker_id: String,
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
        let runtime = runtime_for(&url, app_uuid, &app_id, &worker_id, anchor_modules());
        let arm = call(&runtime, "GET", "/arm", "");
        let io = compio::runtime::Runtime::new()
            .map_err(|error| format!("anchor compio runtime: {error}"))?;
        io.block_on(async {
            runtime.start_pump();
            let (status, body) = settle_response(arm).await?;
            if status != 200 {
                return Err(format!("anchor readiness failed: status={status} body={body}"));
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
    app_uuid: uuid::Uuid,
    app_id: String,
    worker_id: String,
    initial: std::sync::mpsc::Sender<Result<(ThreadId, String), String>>,
) -> JoinHandle<Result<SubscriberResult, String>> {
    thread::spawn(move || {
        init_v8();
        let thread_id = thread::current().id();
        let runtime = runtime_for(
            &url,
            app_uuid,
            &app_id,
            &worker_id,
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
                return Err(format!("stream RPC missing text/event-stream header: {headers:?}"));
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
    app_uuid: uuid::Uuid,
    app_id: String,
    worker_id: String,
) -> JoinHandle<Result<WriterResult, String>> {
    thread::spawn(move || {
        init_v8();
        let thread_id = thread::current().id();
        let runtime = runtime_for(&url, app_uuid, &app_id, &worker_id, writer_modules());
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
    Ok(rows
        .first()
        .map(|row| row.get::<_, bool>("active")))
}

/// Refuse to run the exercise unless [`RUNTIME_DESCRIPTOR`] and [`EVENTS_DDL`]
/// describe the same eight columns.
///
/// This is not belt-and-braces. The data plane BELIEVES the descriptor: it
/// projects `SELECT` lists straight out of the declared field map
/// (`crates/zeroship-schema/src/query.rs:3344-3365`) and reads no catalog at
/// all, so a field the table lacks is a Postgres `42703` in the middle of the
/// stream and a column the descriptor lacks is data silently never read. Either
/// way the failure lands as a stalled or empty SSE frame, which is exactly what
/// a real cross-isolate delivery bug looks like. Checking set equality up front
/// makes the two indistinguishable cases distinguishable, and names which side
/// is wrong.
async fn assert_descriptor_matches_table(pool: &Pool, app_id: &str) -> Result<(), String> {
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
            &[app_id],
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
/// The worker only PROVES the publication exists: `replication::
/// ensure_worker_slot` fails closed with `replication_publication_missing`
/// when it does not, and `replication.rs`'s
/// `worker_setup_only_probes_for_the_migrated_publication` pins that the
/// worker setup path carries no publication DDL at all. Membership is an
/// authorization decision the migration service makes while holding
/// table-owner authority (`crates/zeroship-migrate-server/src/publication.rs`), so
/// this harness makes it on the migration service's behalf, exactly as
/// `tests/integration.rs::c1_create_publication_for_tables` does for the C1
/// suite. `events` has to be IN the set or pgoutput sends nothing and the
/// cross-isolate delivery this target exists to prove never happens.
async fn create_app_publication(
    pool: &Pool,
    app_id: &str,
    publication: &str,
    tables: &[&str],
) -> Result<(), String> {
    let members = tables
        .iter()
        .map(|table| format!(r#""{app_id}"."{table}""#))
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

async fn provision_app_role(pool: &Pool, app_id: &str) -> Result<(), String> {
    let role = zeroship_core::database_role::per_app_role_name(app_id)
        .expect("distributed live app id must produce a valid PostgreSQL role name");
    pool.execute(
        &format!(
            r#"DO $distributed_live_role$ BEGIN
                IF NOT EXISTS (
                    SELECT 1 FROM pg_roles
                    WHERE rolname = '__zeroship_app_role_template'
                ) THEN
                    CREATE ROLE __zeroship_app_role_template
                        NOLOGIN NOREPLICATION NOCREATEDB NOCREATEROLE NOINHERIT;
                END IF;
                IF NOT EXISTS (
                    SELECT 1 FROM pg_roles
                    WHERE rolname = '{role}'
                ) THEN
                    CREATE ROLE "{role}"
                        NOLOGIN NOREPLICATION NOCREATEDB NOCREATEROLE
                        INHERIT IN ROLE __zeroship_app_role_template;
                END IF;
            END $distributed_live_role$"#
        ),
        &[],
    )
    .await
    .map_err(|error| format!("create per-app role: {error}"))?;
    for grant in [
        format!(r#"GRANT USAGE ON SCHEMA "{app_id}" TO "{role}""#),
        format!(
            r#"GRANT SELECT, INSERT, UPDATE, DELETE
                ON ALL TABLES IN SCHEMA "{app_id}" TO "{role}""#
        ),
        format!(
            r#"GRANT USAGE, SELECT
                ON ALL SEQUENCES IN SCHEMA "{app_id}" TO "{role}""#
        ),
    ] {
        pool.execute(&grant, &[])
            .await
            .map_err(|error| format!("grant per-app role privileges: {error}"))?;
    }
    Ok(())
}

async fn drop_app_role(pool: &Pool, app_id: &str) -> Result<(), String> {
    let role = zeroship_core::database_role::per_app_role_name(app_id)
        .expect("distributed live app id must produce a valid PostgreSQL role name");
    pool.execute(&format!(r#"DROP ROLE IF EXISTS "{role}""#), &[])
        .await
        .map_err(|error| format!("drop per-app role: {error}"))?;
    Ok(())
}

#[test]
fn db_live_stream_crosses_v8_isolates_and_releases_worker_slot() {
    init_v8();
    let url = pg_url();
    let app_uuid = uuid::Uuid::new_v4();
    let app_id = app_uuid.to_string();
    let worker_id = format!("distributed-live-worker-{app_uuid}");
    let slot = zeroship_plugin_db::replication::worker_slot_name(&app_id, &worker_id)
        .expect("valid worker slot name");
    let publication = zeroship_plugin_db::replication::publication_name(&app_id)
        .expect("valid publication name");

    let io = compio::runtime::Runtime::new().expect("control compio runtime");
    let pool = io.block_on(async {
        let (probe, connection) = compio_postgres::connect(&url, NoTls)
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "live-db-tests requires compose Postgres at {url}; start it with `docker compose -f deploy/compose/docker-compose.yml up -d postgres`: {error}"
                )
            });
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();
        drop(probe);

        let pool = Pool::connect(&url, 4).await.expect("connect control pool");
        pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app_id}\" CASCADE"), &[])
            .await
            .expect("drop stale test schema");
        pool.execute(&format!("CREATE SCHEMA \"{app_id}\""), &[])
            .await
            .expect("create test schema");
        pool.execute(&EVENTS_DDL.replace(APP_SCHEMA_SLOT, &app_id), &[])
            .await
            .expect("create events table");
        for index in EVENTS_INDEX_DDL {
            pool.execute(&index.replace(APP_SCHEMA_SLOT, &app_id), &[])
                .await
                .expect("create events system index");
        }
        assert_descriptor_matches_table(&pool, &app_id)
            .await
            .expect("runtime descriptor agrees with the events table");
        provision_app_role(&pool, &app_id)
            .await
            .expect("provision per-app role");
        create_app_publication(&pool, &app_id, &publication, &["events"])
            .await
            .expect("create migration-owned publication");
        pool
    });

    let (anchor_ready_tx, anchor_ready_rx) = std::sync::mpsc::channel();
    let (close_tx, close_rx) = flume::bounded(1);
    let (closed_tx, closed_rx) = std::sync::mpsc::channel();
    let (finish_tx, finish_rx) = flume::bounded(1);
    let anchor = spawn_anchor(
        url.clone(),
        app_uuid,
        app_id.clone(),
        worker_id.clone(),
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
            url.clone(),
            app_uuid,
            app_id.clone(),
            worker_id.clone(),
            initial_tx,
        ));
        let (subscriber_thread, initial_body) =
            receive(&initial_rx, "subscriber initial frame")?;
        if initial_body.contains(PROBE) {
            return Err(format!(
                "probe was present before the writer ran: {initial_body:?}"
            ));
        }

        writer = Some(spawn_writer(
            url.clone(),
            app_uuid,
            app_id.clone(),
            worker_id.clone(),
        ));
        let writer_result = join_role(writer.take().expect("writer handle"), "writer")?;
        if writer_result.status != 200 || !writer_result.body.contains(PROBE) {
            return Err(format!(
                "writer failed before cross-isolate delivery: status={} body={:?}",
                writer_result.status, writer_result.body
            ));
        }
        let subscriber_result = join_role(
            subscriber.take().expect("subscriber handle"),
            "subscriber",
        )
        .map_err(|error| {
            format!(
                "writer status={} body={:?}; {error}",
                writer_result.status, writer_result.body
            )
        })?;
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
                "anchor should keep the worker slot active after the stream closes; state={state:?}"
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
        let service = DbService::new(DbServiceConfig {
            url: url.clone(),
            worker_id: "distributed-live-deprovision".to_string(),
            meter: None,
        })
        .map_err(|error| format!("db service: {error}"))?;
        service
            .lifecycle()
            .deprovision_app(&app_id)
            .await
            .map_err(|error| format!("deprovision app CDC: {error}"))?;
        // The worker's deprovision drops ITS replication slots and stops
        // there. The publication is migration-owned and stays behind for a
        // privileged reconciler to remove - see `drop_namespace.rs`'s
        // "Publication ownership" section and `DbLifecycle::deprovision_app`.
        // This target asserted the opposite until now: the worker did hold
        // that authority, and 2a44ea8ef took it away without updating here.
        let publication_retained = publication_exists(&pool, &publication).await?;
        pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app_id}\" CASCADE"), &[])
            .await
            .map_err(|error| format!("drop test schema: {error}"))?;
        drop_app_role(&pool, &app_id).await?;
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
        "last subscriber close must drop this worker's logical slot"
    );
    assert!(
        app_delete_result.expect("app CDC deprovision"),
        "worker deprovision must leave the migration-owned publication in place"
    );
    assert_eq!(writer_result.status, 200, "writer body={}", writer_result.body);
    assert!(body.contains(PROBE), "subscriber body={body:?}");

    assert_ne!(anchor_ready.thread_id, subscriber_thread);
    assert_ne!(anchor_ready.thread_id, writer_result.thread_id);
    assert_ne!(subscriber_thread, writer_result.thread_id);
}
