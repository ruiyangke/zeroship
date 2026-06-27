#![allow(unsafe_code)]

#[path = "support/node_realworld.rs"]
mod node_realworld;

use std::time::Duration;

use serde_json::json;

use node_realworld::{allowlist, ensure_pg_migrate_postgres, lock_env, module, run_js, EnvGuard};

const PG_BUNDLE: &str = include_str!("fixtures/pg/pg-8.16.3.bundle.mjs");

#[test]
fn unmodified_pg_pool_surfaces_dropped_backend_and_reconnects() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let server = ensure_pg_migrate_postgres();

    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        run_js(
            format!(
                r#"
import {{ Client, Pool }} from "./fixtures/pg/pg-8.16.3.bundle.mjs";

const config = {{
  host: "{host}",
  port: {port},
  user: "postgres",
  password: "zeroship",
  database: "postgres",
}};

function errorPayload(err) {{
  return {{
    name: err?.name ?? "Error",
    code: err?.code ?? null,
    message: err?.message ?? String(err),
  }};
}}

async function main() {{
  const events = [];
  const pool = new Pool({{ ...config, max: 1, idleTimeoutMillis: 1000 }});
  pool.on("error", (err) => events.push({{ source: "pool", event: "error", ...errorPayload(err) }}));

  const client = await pool.connect();
  client.on("error", (err) => events.push({{ source: "client", event: "error", ...errorPayload(err) }}));
  client.connection?.on?.("end", () => events.push({{ source: "connection", event: "end" }}));
  client.connection?.stream?.on?.("close", (hadError) => {{
    events.push({{ source: "stream", event: "close", hadError: Boolean(hadError) }});
  }});
  const initial = await client.query("SELECT pg_backend_pid()::int AS pid");
  const initialPid = initial.rows[0].pid;

  const killer = new Client(config);
  await killer.connect();
  const killed = await killer.query("SELECT pg_terminate_backend($1) AS killed", [initialPid]);
  await killer.end();
  if (!killed.rows[0].killed) {{
    throw new Error("pg_terminate_backend returned false");
  }}

  let rejected;
  try {{
    await client.query("SELECT 1::int AS should_not_succeed");
    rejected = {{ rejected: false }};
  }} catch (err) {{
    rejected = {{ rejected: true, error: errorPayload(err) }};
  }}

  try {{
    client.release(true);
  }} catch (err) {{
    events.push({{ source: "release", ...errorPayload(err) }});
  }}

  const after = await pool.query("SELECT 42::int AS n, pg_backend_pid()::int AS pid");
  await pool.end();

  return {{
    initialPid,
    rejected,
    after: after.rows[0],
    events,
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
                host = server.host,
                port = server.port,
            ),
            vec![module("fixtures/pg/pg-8.16.3.bundle.mjs", PG_BUNDLE)],
            allowlist(server.host, server.port, 8, 8 * 1024 * 1024),
            Duration::from_secs(45),
        )
        .await
    });

    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    let body: serde_json::Value =
        serde_json::from_str(&result.body).expect("pg reconnect e2e response must be JSON");
    assert_eq!(
        body.pointer("/rejected/rejected"),
        Some(&json!(true)),
        "query on terminated backend should reject: {body}"
    );
    assert_eq!(
        body.pointer("/after/n"),
        Some(&json!(42)),
        "subsequent pool query did not recover: {body}"
    );
    assert_ne!(
        body.get("initialPid"),
        body.pointer("/after/pid"),
        "pool reused the terminated backend PID instead of reconnecting: {body}"
    );
    assert!(
        body.pointer("/rejected/error/message")
            .and_then(|value| value.as_str())
            .is_some_and(|message| !message.is_empty()),
        "rejected query did not surface an error message: {body}"
    );
    assert!(
        body.get("events")
            .and_then(|value| value.as_array())
            .is_some_and(|events| {
                events.iter().any(|event| {
                    matches!(
                        event.get("event").and_then(|value| value.as_str()),
                        Some("close" | "end" | "error")
                    )
                })
            }),
        "terminated backend did not surface a close/end/error event: {body}"
    );
}
