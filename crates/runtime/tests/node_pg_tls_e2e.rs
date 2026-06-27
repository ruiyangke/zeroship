#![allow(unsafe_code)]

#[path = "support/node_realworld.rs"]
mod node_realworld;

use std::time::Duration;

use serde_json::json;

use node_realworld::{allowlist, ensure_tls_postgres, lock_env, module, run_js, EnvGuard};

const PG_BUNDLE: &str = include_str!("fixtures/pg/pg-8.16.3.bundle.mjs");

#[test]
fn unmodified_pg_driver_uses_real_tls_with_ca_pin_against_live_postgres() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let server = ensure_tls_postgres();
    let host = "localhost";
    let ca_pem = serde_json::to_string(&server.ca_pem).unwrap();
    let wrong_ca_pem = serde_json::to_string(&server.wrong_ca_pem).unwrap();

    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        run_js(
            format!(
                r#"
import {{ Client }} from "./fixtures/pg/pg-8.16.3.bundle.mjs";

const baseConfig = {{
  host: "{host}",
  port: {port},
  user: "postgres",
  password: "zeroship",
  database: "postgres",
  connectionTimeoutMillis: 5000,
}};

const caPem = {ca_pem};
const wrongCaPem = {wrong_ca_pem};

function errorPayload(err) {{
  return {{
    name: err?.name ?? "Error",
    code: err?.code ?? null,
    message: err?.message ?? String(err),
  }};
}}

function withTimeout(promise, ms, label) {{
  return Promise.race([
    promise,
    new Promise((_, reject) => setTimeout(() => reject(new Error(label + " timed out")), ms)),
  ]);
}}

async function expectConnectRejected(config) {{
  const client = new Client(config);
  try {{
    await withTimeout(client.connect(), 5000, "expected rejection connect");
    await withTimeout(client.end(), 5000, "unexpected accepted end");
    return {{ rejected: false }};
  }} catch (err) {{
    client.connection?.stream?.destroy?.();
    return {{ rejected: true, error: errorPayload(err) }};
  }}
}}

async function main() {{
  const tlsConfig = {{
    ...baseConfig,
    ssl: {{
      ca: caPem,
      rejectUnauthorized: true,
      servername: "localhost",
    }},
  }};

  const client = new Client(tlsConfig);
  await withTimeout(client.connect(), 5000, "valid TLS pg connect");
  const headline = await withTimeout(client.query(`
    SELECT
      42::int AS n,
      (SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()) AS ssl
  `), 5000, "valid TLS pg query");
  try {{
    await withTimeout(client.end(), 5000, "valid TLS pg end");
  }} catch (err) {{
    client.connection?.stream?.destroy?.();
    throw err;
  }}

  const plain = await expectConnectRejected({{
    ...baseConfig,
    connectionTimeoutMillis: 3000,
    ssl: false,
  }});
  const wrongCa = await expectConnectRejected({{
    ...baseConfig,
    connectionTimeoutMillis: 3000,
    ssl: {{
      ca: wrongCaPem,
      rejectUnauthorized: true,
      servername: "localhost",
    }},
  }});

  return {{ headline: headline.rows, plain, wrongCa }};
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
                host = host,
                port = server.port,
                ca_pem = ca_pem,
                wrong_ca_pem = wrong_ca_pem,
            ),
            vec![module("fixtures/pg/pg-8.16.3.bundle.mjs", PG_BUNDLE)],
            allowlist(host, server.port, 8, 8 * 1024 * 1024),
            Duration::from_secs(45),
        )
        .await
    });

    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    let body: serde_json::Value =
        serde_json::from_str(&result.body).expect("pg TLS e2e response must be JSON");
    assert_eq!(
        body.get("headline"),
        Some(&json!([{ "n": 42, "ssl": true }])),
        "TLS SELECT row mismatch: {body}"
    );
    assert_eq!(
        body.pointer("/plain/rejected"),
        Some(&json!(true)),
        "plaintext connection should be rejected by pg_hba.conf: {body}"
    );
    assert_eq!(
        body.pointer("/wrongCa/rejected"),
        Some(&json!(true)),
        "wrong CA should reject the TLS handshake: {body}"
    );
}
