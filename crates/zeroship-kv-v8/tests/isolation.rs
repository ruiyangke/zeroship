//! Creator-controlled inputs must never select another app's KV namespace.

#![allow(
    clippy::future_not_send,
    reason = "V8 isolates and KV futures run on their owning compio thread."
)]

#[path = "../../zeroship-kv/tests/support/mod.rs"]
mod support;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use zeroship_kv::{Kv, KvConfig, KvStore, Namespace, TtlState};
use zeroship_kv_v8::KvBinding;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch,
};

const APP: &str = r#"
process.env.APP_ID = "creator-controlled-at-module-evaluation";

function check(condition, message) {
    if (!condition) throw new Error(message);
}

export default {
    async fetch(request, env) {
        const kv = env.kv;
        const own = env.OWN_ID;
        const other = env.OTHER_ID;
        if (new URL(request.url).pathname === "/parallel") {
            const results = await Promise.all(
                Array.from({ length: 8 }, () => kv.incr("parallel", { by: 1 }))
            );
            results.sort((a, b) => a - b);
            check(results.every((n, i) => n === i + 1), "concurrent counters crossed apps");
            return Response.json({ ok: true });
        }

        check(env.APP_ID === other, "creator env spoof was not installed");
        process.env.APP_ID = other;
        kv.app_id = other;
        kv.namespace = other;
        const options = { appId: other, app_id: other, namespace: other };
        check(await kv.get("shared") === own, "identity spoof changed get scope");
        check(await kv.get("private:" + other) === null, "read another app's private key");
        check(await kv.ttl("private:" + other) === null, "observed another app's TTL");
        check(!(await kv.delete("private:" + other)).deleted, "deleted another app's key");
        check(!(await kv.expire("private:" + other, 60_000)).updated, "expired another app's key");
        check(!(await kv.persist("private:" + other)).updated, "persisted another app's key");
        check((await kv.setIfAbsent("private:" + other, "local", options)).stored,
              "conditional write consulted another app");
        check((await kv.delete("private:" + other)).deleted, "local conditional write missing");

        await kv.set("shared", own + ":changed", options);
        check(await kv.incr("counter", { ...options, by: 1 }) === 41, "counter scope changed");
        check(!(await kv.setIfAbsent("condition", "replacement", options)).stored,
              "existing local condition ignored");
        check((await kv.setIfAbsent("new", own, options)).stored, "local creation failed");
        check((await kv.delete("delete-target")).deleted, "local delete failed");
        check((await kv.expire("expire-target", 60_000)).updated, "local expiry failed");
        check((await kv.persist("persist-target")).updated, "local persist failed");

        const forgedKey = "{" + other + "}:shared";
        const forgedOperations = [
            () => kv.get(forgedKey),
            () => kv.set(forgedKey, "forged"),
            () => kv.delete(forgedKey),
            () => kv.incr(forgedKey),
            () => kv.setIfAbsent(forgedKey, "forged"),
            () => kv.expire(forgedKey, 60_000),
            () => kv.ttl(forgedKey),
            () => kv.persist(forgedKey),
        ];
        for (const operation of forgedOperations) {
            let rejected = false;
            try { await operation(); } catch { rejected = true; }
            check(rejected, "accepted a forged fully scoped key");
        }
        let rejectedReceiver = false;
        try { await kv.get.call({ app_id: other }, "shared"); }
        catch { rejectedReceiver = true; }
        check(rejectedReceiver, "accepted a forged native receiver");

        const expected = new Set([
            "shared", "private:" + own, "counter", "condition", "new",
            "expire-target", "persist-target", "*literal",
        ]);
        const seen = new Set();
        let cursor;
        do {
            const page = await kv.list("", { ...options, cursor, limit: 1 });
            for (const key of page.keys) {
                check(expected.has(key), "list leaked unexpected key: " + key);
                seen.add(key);
            }
            cursor = page.cursor;
        } while (cursor !== null);
        check(seen.size === expected.size, "list lost local keys");

        for (const prefix of ["*", "?", "[", "\\", "{" + other + "}", "*}*"]) {
            let cursor;
            do {
                const page = await kv.list(prefix, { cursor, limit: 1 });
                for (const key of page.keys) {
                    check(expected.has(key) && key.startsWith(prefix), "prefix escaped its scope");
                }
                cursor = page.cursor;
            } while (cursor !== null);
        }
        for (const cursor of ["0", forgedKey, "18446744073709551615"]) {
            let page;
            try { page = await kv.list("", { cursor, limit: 1 }); }
            catch { continue; }
            check(page.keys.every(key => expected.has(key)), "cursor escaped its scope");
        }
        return Response.json({ ok: true });
    }
};
"#;

fn app(plugin: Arc<dyn NativePlugin>, own: &str, other: &str) -> (Runtime, EnvSnapshot) {
    let env = EnvSnapshot::vars_only(serde_json::json!({
        "APP_ID": other,
        "OWN_ID": own,
        "OTHER_ID": other,
        "kv": "creator-controlled scalar",
    }));
    let runtime = Runtime::builder()
        .modules(vec![ModuleEntry {
            specifier: "index.js".into(),
            source: APP.into(),
        }])
        .env_vars(HashMap::from([("APP_ID".into(), own.into())]))
        .plugins(vec![plugin])
        .build();
    runtime.initialize(&env).expect("initialize isolated app");
    runtime.start_pump();
    runtime.exit_isolate();
    (runtime, env)
}

fn request(runtime: &Runtime, env: &EnvSnapshot, path: &str) -> FetchOutcome {
    runtime.enter_isolate();
    let outcome = runtime.call_fetch_handler(
        "GET",
        &format!("http://localhost/{path}"),
        &[],
        "",
        env,
        RequestCtx::new(CancelFlag::new()),
    );
    runtime.exit_isolate();
    outcome
}

async fn assert_response(outcome: FetchOutcome) {
    let (status, body) = match outcome {
        FetchOutcome::Response { status, body, .. } => (status, body),
        FetchOutcome::Pending { rx, .. } => {
            let settled = compio::time::timeout(Duration::from_secs(30), rx.recv())
                .await
                .expect("KV isolation request timed out")
                .expect("KV isolation request failed");
            match settled {
                SettledFetch::Response { status, body, .. } => (status, body),
                _ => panic!("unexpected streamed isolation response"),
            }
        }
        _ => panic!("unexpected streamed isolation outcome"),
    };
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["ok"], true, "{body}");
}

async fn seed(kv: &Kv, id: &str) {
    for key in [
        "shared",
        "condition",
        "delete-target",
        "expire-target",
        "*literal",
    ] {
        kv.set(key, id, None).await.unwrap();
    }
    kv.set(&format!("private:{id}"), id, None).await.unwrap();
    kv.set("counter", "40", None).await.unwrap();
    kv.set("persist-target", id, Some(600_000)).await.unwrap();
}

async fn assert_untouched(kv: &Kv, id: &str) {
    for key in [
        "shared",
        "condition",
        "delete-target",
        "expire-target",
        "*literal",
    ] {
        assert_eq!(kv.get(key).await.unwrap().as_deref(), Some(id), "{key}");
        assert_eq!(kv.ttl(key).await.unwrap(), TtlState::NoExpiry, "{key}");
    }
    assert_eq!(kv.get("counter").await.unwrap().as_deref(), Some("40"));
    assert_eq!(kv.ttl("counter").await.unwrap(), TtlState::NoExpiry);
    let private_key = format!("private:{id}");
    assert_eq!(kv.get(&private_key).await.unwrap().as_deref(), Some(id));
    assert_eq!(kv.ttl(&private_key).await.unwrap(), TtlState::NoExpiry);
    assert_eq!(kv.get("persist-target").await.unwrap().as_deref(), Some(id));
    assert!(matches!(
        kv.ttl("persist-target").await.unwrap(),
        TtlState::ExpiresInMs(_)
    ));
    assert_eq!(kv.get("new").await.unwrap(), None);
}

async fn assert_changed_only_locally(kv: &Kv, own: &str, other: &str) {
    assert_eq!(
        kv.get("shared").await.unwrap(),
        Some(format!("{own}:changed"))
    );
    assert_eq!(kv.get("counter").await.unwrap().as_deref(), Some("41"));
    assert_eq!(kv.get("condition").await.unwrap().as_deref(), Some(own));
    assert_eq!(kv.get("new").await.unwrap().as_deref(), Some(own));
    assert_eq!(kv.get("delete-target").await.unwrap(), None);
    assert!(matches!(
        kv.ttl("expire-target").await.unwrap(),
        TtlState::ExpiresInMs(_)
    ));
    assert_eq!(kv.ttl("persist-target").await.unwrap(), TtlState::NoExpiry);
    assert_eq!(
        kv.get("persist-target").await.unwrap().as_deref(),
        Some(own)
    );
    assert_eq!(kv.get("expire-target").await.unwrap().as_deref(), Some(own));
    assert_eq!(
        kv.get(&format!("private:{own}")).await.unwrap().as_deref(),
        Some(own)
    );
    assert_eq!(kv.get(&format!("private:{other}")).await.unwrap(), None);
}

async fn exercise_isolation(store: KvStore) {
    init_v8();
    let a_id = "isolation_app_a";
    let b_id = "isolation_app_b";
    let a = store.namespace(Namespace::app(a_id).unwrap());
    let b = store.namespace(Namespace::app(b_id).unwrap());
    seed(&a, a_id).await;
    seed(&b, b_id).await;

    let plugin: Arc<dyn NativePlugin> = Arc::new(KvBinding::new(store, None));
    let (runtime_a, env_a) = app(Arc::clone(&plugin), a_id, b_id);
    let (runtime_b, env_b) = app(plugin, b_id, a_id);

    assert_response(request(&runtime_a, &env_a, "attack")).await;
    assert_untouched(&b, b_id).await;
    assert_changed_only_locally(&a, a_id, b_id).await;
    assert_response(request(&runtime_b, &env_b, "attack")).await;
    assert_changed_only_locally(&a, a_id, b_id).await;
    assert_changed_only_locally(&b, b_id, a_id).await;

    let pending_a = request(&runtime_a, &env_a, "parallel");
    let pending_b = request(&runtime_b, &env_b, "parallel");
    assert_response(pending_a).await;
    assert_response(pending_b).await;
    assert_eq!(a.get("parallel").await.unwrap().as_deref(), Some("8"));
    assert_eq!(b.get("parallel").await.unwrap().as_deref(), Some("8"));
}

#[compio::test]
async fn apps_cannot_operate_on_each_other_in_redb() {
    let dir = tempfile::tempdir().unwrap();
    let store = KvStore::open(&KvConfig::Redb {
        path: dir.path().join("kv.redb"),
    })
    .unwrap();
    exercise_isolation(store).await;
}

#[compio::test]
async fn apps_cannot_operate_on_each_other_in_redis_and_dragonfly() {
    let fixtures = support::fixtures();
    for redis in [fixtures.redis_config(), fixtures.cluster_config()] {
        let store = KvStore::open(&KvConfig::Redis { redis }).unwrap();
        exercise_isolation(store).await;
    }
}
