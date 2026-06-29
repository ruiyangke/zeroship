#![allow(unsafe_code)]

use std::net::TcpStream as StdTcpStream;
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::{
    EnvSnapshot, FetchOutcome, HostPort, ModuleEntry, NetPolicy, RequestCtx, Runtime, SettledFetch,
};

const PG_BUNDLE: &str = include_str!("fixtures/pg/pg-8.16.3.bundle.mjs");
const PG_HOST: &str = "127.0.0.1";
const PG_PORT: u16 = 5440;
const PG_USER: &str = "postgres";
const PG_PASSWORD: &str = "zeroship";
const PG_DATABASE: &str = "postgres";

struct EnvGuard {
    prev_dev: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set_dev() -> Self {
        let prev_dev = std::env::var_os("ZEROSHIP_DEV");
        unsafe {
            std::env::set_var("ZEROSHIP_DEV", "1");
        }
        Self { prev_dev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.prev_dev {
                Some(v) => std::env::set_var("ZEROSHIP_DEV", v),
                None => std::env::remove_var("ZEROSHIP_DEV"),
            }
        }
    }
}

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn lock_env() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

struct JsResult {
    status: u16,
    body: String,
}

async fn run_pg_js(module_src: String, max_wait: Duration) -> JsResult {
    assert!(
        !PG_BUNDLE.trim().is_empty(),
        "vendored pg bundle fixture must not be empty"
    );

    let modules = vec![
        ModuleEntry {
            specifier: "index.js".to_string(),
            source: module_src,
        },
        ModuleEntry {
            specifier: "fixtures/pg/pg-8.16.3.bundle.mjs".to_string(),
            source: PG_BUNDLE.to_string(),
        },
    ];
    let runtime = Runtime::builder()
        .modules(modules)
        .net_policy(
            NetPolicy::allowlist(
                vec![HostPort::new(PG_HOST, PG_PORT)],
                32,
                8 * 1024 * 1024,
            )
            .unwrap(),
        )
        .build();
    runtime.start_pump();

    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx);
    drive_fetch_outcome(outcome, max_wait).await
}

async fn drive_fetch_outcome(outcome: FetchOutcome, max_wait: Duration) -> JsResult {
    match outcome {
        FetchOutcome::Response { status, body, .. } => JsResult {
            status,
            body: String::from_utf8_lossy(&body).into_owned(),
        },
        FetchOutcome::Pending { rx, .. } => match compio::time::timeout(max_wait, rx.recv()).await
        {
            Ok(Ok(SettledFetch::Response { status, body, .. })) => JsResult {
                status,
                body: String::from_utf8_lossy(&body).into_owned(),
            },
            Ok(Ok(SettledFetch::Stream {
                status,
                body_reader,
                ..
            })) => {
                let mut body = Vec::new();
                for chunk in body_reader.drain() {
                    body.extend_from_slice(&chunk);
                }
                JsResult {
                    status,
                    body: String::from_utf8_lossy(&body).into_owned(),
                }
            }
            Ok(Ok(SettledFetch::WebSocketUpgrade { .. })) => JsResult {
                status: 500,
                body: "unexpected websocket upgrade".to_string(),
            },
            Ok(Err(e)) => JsResult {
                status: 500,
                body: format!("ERR: {}", e.message),
            },
            Err(_) => JsResult {
                status: 599,
                body: "TIMEOUT".to_string(),
            },
        },
        FetchOutcome::Stream {
            status,
            body_reader,
            ..
        } => {
            let mut body = Vec::new();
            for chunk in body_reader.drain() {
                body.extend_from_slice(&chunk);
            }
            JsResult {
                status,
                body: String::from_utf8_lossy(&body).into_owned(),
            }
        }
        FetchOutcome::WebSocketUpgrade { .. } => JsResult {
            status: 500,
            body: "unexpected websocket upgrade".to_string(),
        },
    }
}

fn require_pg_port() {
    if StdTcpStream::connect((PG_HOST, PG_PORT)).is_ok() {
        return;
    }

    let output = Command::new("docker")
        .args(["start", "appbase-migrate-postgres-1"])
        .output()
        .unwrap_or_else(|err| panic!("failed to spawn docker start appbase-migrate-postgres-1: {err}"));
    assert!(
        output.status.success(),
        "failed to start appbase-migrate-postgres-1 for pg e2e\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(45) {
        if StdTcpStream::connect((PG_HOST, PG_PORT)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    StdTcpStream::connect((PG_HOST, PG_PORT)).unwrap_or_else(|err| {
        panic!(
            "headline pg e2e requires live Postgres at {PG_HOST}:{PG_PORT}; \
             start appbase-migrate-postgres-1 without tearing it down: {err}"
        )
    });
}

#[test]
fn unmodified_pg_client_and_pool_return_live_rows() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    require_pg_port();

    // The shared migrate Postgres answers "N" to the PostgreSQL SSLRequest
    // probe in this worktree, so this fixture proves live pg over node:net.
    // The CA-pinned node:tls path remains covered by tests/node_tls.rs.
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        run_pg_js(
            format!(
                r#"
import {{ Client, Pool }} from "./fixtures/pg/pg-8.16.3.bundle.mjs";

const config = {{
    host: "{host}",
    port: {port},
    user: "{user}",
    password: "{password}",
    database: "{database}",
}};

async function main() {{
    const ordering = [];
    Promise.resolve().then(() => ordering.push("promise"));
    process.nextTick(() => ordering.push("tick"));
    await Promise.resolve();

    const client = new Client(config);
    await client.connect();
    const headline = await client.query("SELECT 42::int AS n, 'ok' AS s");

    const pool = new Pool({{ ...config, max: 10, idleTimeoutMillis: 1000 }});
    const concurrent = await Promise.all(
        Array.from({{ length: 25 }}, (_, i) =>
            pool.query("SELECT $1::int AS n, ($1::int * 2)::int AS doubled", [i])
        )
    );
    const pooled = await pool.connect();
    let roundtrip;
    try {{
        await pooled.query("CREATE TEMP TABLE zs_pg_e2e (id int PRIMARY KEY, label text)");
        await pooled.query("INSERT INTO zs_pg_e2e (id, label) VALUES ($1, $2)", [7, "pool-ok"]);
        roundtrip = await pooled.query("SELECT id, label FROM zs_pg_e2e WHERE id = $1", [7]);
    }} finally {{
        pooled.release();
        await pool.end();
    }}

    const large = await client.query("SELECT generate_series(1, 50000)::int AS n");

    await client.query("CREATE TEMP TABLE zs_pg_txn_e2e (id int PRIMARY KEY, label text)");
    await client.query("BEGIN");
    await client.query("INSERT INTO zs_pg_txn_e2e (id, label) VALUES ($1, $2)", [11, "txn-ok"]);
    const inTxn = await client.query("SELECT id, label FROM zs_pg_txn_e2e WHERE id = $1", [11]);
    await client.query("COMMIT");
    const afterCommit = await client.query("SELECT count(*)::int AS count FROM zs_pg_txn_e2e");

    const listener = new Client(config);
    const notifier = new Client(config);
    const channel = "zs_pg_e2e_" + Date.now();
    await listener.connect();
    await notifier.connect();
    const notificationPromise = new Promise((resolve, reject) => {{
        const timer = setTimeout(() => reject(new Error("LISTEN/NOTIFY timed out")), 5000);
        listener.on("notification", (msg) => {{
            clearTimeout(timer);
            resolve({{ channel: msg.channel, payload: msg.payload }});
        }});
    }});
    await listener.query(`LISTEN ${{channel}}`);
    await notifier.query("SELECT pg_notify($1, $2)", [channel, "hello-push"]);
    const notification = await notificationPromise;
    await listener.end();
    await notifier.end();

    await client.end();

    return {{
        nextTickOrdering: ordering,
        headline: headline.rows,
        concurrent: concurrent.map((r) => r.rows[0]),
        pool: roundtrip.rows,
        large: {{
            rowCount: large.rowCount,
            first: large.rows[0].n,
            last: large.rows[large.rows.length - 1].n,
        }},
        transaction: {{
            inTxn: inTxn.rows,
            afterCommit: afterCommit.rows,
        }},
        notification,
    }};
}}

export default {{
    async fetch() {{
        try {{
            return Response.json(await main());
        }} catch (err) {{
            return new Response(JSON.stringify({{
                name: err?.name ?? "Error",
                code: err?.code ?? null,
                message: err?.message ?? String(err),
                stack: err?.stack ?? null,
            }}), {{ status: 500, headers: {{ "content-type": "application/json" }} }});
        }}
    }},
}};
"#,
                host = PG_HOST,
                port = PG_PORT,
                user = PG_USER,
                password = PG_PASSWORD,
                database = PG_DATABASE,
            ),
            Duration::from_secs(20),
        )
        .await
    });

    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    let body: serde_json::Value =
        serde_json::from_str(&result.body).expect("pg e2e response must be JSON");
    assert_eq!(
        body.get("headline"),
        Some(&serde_json::json!([{ "n": 42, "s": "ok" }])),
        "headline SELECT row mismatch: {body}"
    );
    assert_eq!(
        body.get("pool"),
        Some(&serde_json::json!([{ "id": 7, "label": "pool-ok" }])),
        "pool temp-table roundtrip mismatch: {body}"
    );
    assert_eq!(
        body.get("nextTickOrdering"),
        Some(&serde_json::json!(["tick", "promise"])),
        "process.nextTick did not run before Promise microtasks: {body}"
    );
    assert_eq!(
        body.get("concurrent"),
        Some(&serde_json::json!(
            (0..25)
                .map(|i| serde_json::json!({ "n": i, "doubled": i * 2 }))
                .collect::<Vec<_>>()
        )),
        "concurrent pool query results mismatch: {body}"
    );
    assert_eq!(
        body.get("large"),
        Some(&serde_json::json!({ "rowCount": 50000, "first": 1, "last": 50000 })),
        "large result-set mismatch: {body}"
    );
    assert_eq!(
        body.pointer("/transaction/inTxn"),
        Some(&serde_json::json!([{ "id": 11, "label": "txn-ok" }])),
        "transaction in-flight row mismatch: {body}"
    );
    assert_eq!(
        body.pointer("/transaction/afterCommit"),
        Some(&serde_json::json!([{ "count": 1 }])),
        "transaction commit row mismatch: {body}"
    );
    assert_eq!(
        body.pointer("/notification/payload"),
        Some(&serde_json::Value::String("hello-push".to_string())),
        "LISTEN/NOTIFY payload mismatch: {body}"
    );
}
