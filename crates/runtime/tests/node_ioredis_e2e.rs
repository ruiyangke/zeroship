#![allow(unsafe_code)]

#[path = "support/node_realworld.rs"]
mod node_realworld;

use std::time::Duration;

use serde_json::json;

use node_realworld::{
    EnvGuard, allowlist, ensure_redis, lock_env, module, run_js,
};

const IOREDIS_BUNDLE: &str = include_str!("fixtures/ioredis/ioredis-5.6.1.bundle.mjs");

#[test]
fn unmodified_ioredis_driver_pipeline_and_pubsub_live_redis() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let server = ensure_redis();

    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        run_js(
            format!(
                r#"
import Redis from "./fixtures/ioredis/ioredis-5.6.1.bundle.mjs";

const config = {{
  host: "{host}",
  port: {port},
  lazyConnect: true,
  maxRetriesPerRequest: 1,
  enableReadyCheck: true,
}};

function withTimeout(promise, ms, label) {{
  return Promise.race([
    promise,
    new Promise((_, reject) => setTimeout(() => reject(new Error(label + " timed out")), ms)),
  ]);
}}

async function main() {{
  const key = "zs:ioredis:e2e:" + Date.now();
  const channel = "zs:ioredis:pubsub:" + Date.now();

  const redis = new Redis(config);
  await redis.connect();
  await redis.set(key, "ok");
  const got = await redis.get(key);

  const pipeline = await redis
    .pipeline()
    .set(key + ":pipe", "1")
    .incr(key + ":pipe")
    .get(key + ":pipe")
    .exec();

  const sub = new Redis(config);
  const pub = new Redis(config);
  await Promise.all([sub.connect(), pub.connect()]);
  const messagePromise = new Promise((resolve) => {{
    sub.on("message", (receivedChannel, message) => resolve({{ channel: receivedChannel, message }}));
  }});
  await sub.subscribe(channel);
  const publishCount = await pub.publish(channel, "hello-push");
  const pushed = await withTimeout(messagePromise, 5000, "pubsub message");

  await Promise.all([sub.quit(), pub.quit(), redis.quit()]);

  return {{
    get: got,
    pipeline,
    publishCount,
    pushed,
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
                "fixtures/ioredis/ioredis-5.6.1.bundle.mjs",
                IOREDIS_BUNDLE,
            )],
            allowlist(server.host, server.port, 12, 8 * 1024 * 1024),
            Duration::from_secs(45),
        )
        .await
    });

    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    let body: serde_json::Value =
        serde_json::from_str(&result.body).expect("ioredis e2e response must be JSON");
    assert_eq!(body.get("get"), Some(&json!("ok")), "SET/GET mismatch: {body}");
    assert_eq!(
        body.get("pipeline"),
        Some(&json!([[null, "OK"], [null, 2], [null, "2"]])),
        "pipeline response mismatch: {body}"
    );
    let pushed = body.get("pushed").expect("missing pub/sub payload");
    assert!(
        pushed
            .get("channel")
            .and_then(|value| value.as_str())
            .is_some_and(|channel| channel.starts_with("zs:ioredis:pubsub:")),
        "pub/sub channel mismatch: {body}"
    );
    assert_eq!(
        pushed.get("message"),
        Some(&json!("hello-push")),
        "pub/sub payload mismatch: {body}"
    );
    assert_eq!(
        body.get("publishCount"),
        Some(&json!(1)),
        "pub/sub publish count mismatch: {body}"
    );
}
