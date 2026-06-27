#![allow(unsafe_code)]

#[path = "support/node_realworld.rs"]
mod node_realworld;

use std::time::Duration;

use serde_json::json;

use node_realworld::{allowlist, ensure_memcached, lock_env, module, run_js, EnvGuard};

const MEMJS_BUNDLE: &str = include_str!("fixtures/memjs/memjs-1.3.2.bundle.mjs");

#[test]
fn unmodified_memjs_driver_round_trips_live_memcached_binary_protocol() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let server = ensure_memcached();

    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        run_js(
            format!(
                r#"
import memjs from "./fixtures/memjs/memjs-1.3.2.bundle.mjs";

async function main() {{
  const client = memjs.Client.create("{host}:{port}", {{
    conntimeout: 2,
    timeout: 2,
    keepAlive: true,
    retries: 1,
  }});
  const key = "zs:memjs:e2e:" + Date.now();

  const set = await client.set(key, "live-ok", {{ expires: 30 }});
  const got = await client.get(key);
  const deleted = await client.delete(key);
  const afterDelete = await client.get(key);
  client.close();

  return {{
    set,
    value: got.value && got.value.toString("utf8"),
    flagsLength: got.flags ? got.flags.length : null,
    deleted,
    afterDelete: afterDelete.value === null,
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
            vec![module(
                "fixtures/memjs/memjs-1.3.2.bundle.mjs",
                MEMJS_BUNDLE,
            )],
            allowlist(server.host, server.port, 4, 4 * 1024 * 1024),
            Duration::from_secs(45),
        )
        .await
    });

    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    let body: serde_json::Value =
        serde_json::from_str(&result.body).expect("memjs e2e response must be JSON");
    assert_eq!(body.get("set"), Some(&json!(true)), "SET failed: {body}");
    assert_eq!(
        body.get("value"),
        Some(&json!("live-ok")),
        "GET returned wrong value: {body}"
    );
    assert_eq!(
        body.get("deleted"),
        Some(&json!(true)),
        "DELETE failed: {body}"
    );
    assert_eq!(
        body.get("afterDelete"),
        Some(&json!(true)),
        "deleted key was still visible: {body}"
    );
}
