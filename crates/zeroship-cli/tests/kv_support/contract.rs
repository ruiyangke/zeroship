use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub type Capture = BTreeMap<String, Value>;

fn rpc(http: &ureq::Agent, origin: &str, operation: &str, input: Value) -> Value {
    let response: Value = http
        .post(&format!("{origin}/__zeroship/v1/{operation}"))
        .send_json(json!({"json": input}))
        .unwrap_or_else(|error| panic!("{operation}: {error}"))
        .body_mut()
        .read_json()
        .expect("RPC JSON response");
    response
        .get("json")
        .unwrap_or_else(|| panic!("{operation} returned no result envelope: {response}"))
        .clone()
}

fn list_keys(http: &ureq::Agent, origin: &str, prefix: &str) -> Value {
    let mut keys = BTreeSet::new();
    let mut cursors = BTreeSet::new();
    let mut cursor = Value::Null;
    loop {
        let page = rpc(
            http,
            origin,
            "kv.keys.list",
            json!({
                "prefix": prefix, "cursor": cursor, "limit": 2
            }),
        );
        for key in page["keys"].as_array().expect("listing keys") {
            keys.insert(key.as_str().expect("string key").to_owned());
        }
        cursor = page.get("cursor").expect("listing cursor").clone();
        if cursor.is_null() {
            return json!({"keys": keys, "cursor": null});
        }
        assert!(
            cursors.insert(cursor.as_str().expect("opaque string cursor").to_owned()),
            "listing repeated a cursor without reaching exhaustion"
        );
    }
}

pub fn exercise(http: &ureq::Agent, origin: &str) -> Capture {
    rpc(http, origin, "kv.clear", json!({}));
    let mut capture = Capture::new();
    let mut row = |label: &str, operation: &str, input| {
        capture.insert(label.into(), rpc(http, origin, operation, input));
    };
    row("visit1", "kv.visit", json!({}));
    row("visit2", "kv.visit", json!({}));
    row("flag", "kv.flag.set", json!({"enabled":true}));
    // Keep the limiter burst inside the fixture's fixed window.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let remaining = 60_000 - now % 60_000;
    if remaining < 5_000 {
        std::thread::sleep(std::time::Duration::from_millis(remaining as u64 + 1));
    }
    for i in 1..=6 {
        row(&format!("rate{i}"), "kv.rate.hit", json!({"actor":"probe"}));
    }
    row("cache1", "kv.cache.quote", json!({"sku":"probe-sku"}));
    row("cache2", "kv.cache.quote", json!({"sku":"probe-sku"}));
    row("memo1", "kv.memo.get", json!({"label":"probe-memo"}));
    row("memo2", "kv.memo.get", json!({"label":"probe-memo"}));
    row("lease1", "kv.lease.acquire", json!({"owner":"owner-a"}));
    row("lease2", "kv.lease.acquire", json!({"owner":"owner-b"}));
    row("leaseC", "kv.lease.clear", json!({}));
    row("lease3", "kv.lease.acquire", json!({"owner":"owner-b"}));
    rpc(http, origin, "kv.lease.clear", json!({}));
    row(
        "strSet",
        "kv.string.set",
        json!({"value":"hello probe","ttlMs":60000}),
    );
    row("strExp", "kv.string.expire", json!({"ttlMs":120000}));
    row("strPer", "kv.string.persist", json!({}));
    row("strDel", "kv.string.delete", json!({}));
    let session = rpc(
        http,
        origin,
        "kv.session.create",
        json!({"name":"Test User"}),
    );
    let token = session["token"].as_str().expect("session token");
    assert!(!token.is_empty());
    let sessions = list_keys(http, origin, "session:");
    assert!(
        sessions["keys"]
            .as_array()
            .unwrap()
            .iter()
            .any(|key| key.as_str().unwrap().ends_with(token)),
        "created session must be listed"
    );
    assert_eq!(
        rpc(http, origin, "kv.session.delete", json!({"token":token}))["deleted"],
        true
    );
    let sessions = list_keys(http, origin, "session:");
    assert!(
        sessions["keys"].as_array().unwrap().is_empty(),
        "deleted session must disappear"
    );
    capture.insert("keys".into(), list_keys(http, origin, ""));
    capture
}

pub fn validate(capture: &Capture) -> Result<(), String> {
    let field = |label: &str, path: &str| {
        capture
            .get(label)
            .and_then(|row| row.pointer(path))
            .ok_or_else(|| format!("missing {label}{path}"))
    };
    let equal = |label: &str, path: &str, expected: Value| -> Result<(), String> {
        let actual = field(label, path)?;
        if actual != &expected {
            return Err(format!("{label}{path}: expected {expected}, got {actual}"));
        }
        Ok(())
    };
    let band = |label: &str, path: &str, low: i64, high: i64| -> Result<(), String> {
        let actual = field(label, path)?
            .as_i64()
            .ok_or_else(|| format!("{label}{path} is not an integer"))?;
        if actual <= low || actual > high {
            return Err(format!(
                "{label}{path} is outside its TTL contract: {actual}"
            ));
        }
        Ok(())
    };
    equal("visit1", "/visits", json!(1))?;
    equal("visit2", "/visits", json!(2))?;
    equal("flag", "/checkoutEnabled", json!(true))?;
    for i in 1..=6 {
        let label = format!("rate{i}");
        equal(&label, "/count", json!(i))?;
        equal(&label, "/allowed", json!(i <= 5))?;
        equal(&label, "/remaining", json!((5_i64 - i).max(0)))?;
    }
    band("rate6", "/resetMs", 0, 60_000)?;
    for (first, second, value) in [("cache1", "cache2", "/quote"), ("memo1", "memo2", "/value")] {
        equal(first, "/source", json!("miss"))?;
        equal(second, "/source", json!("hit"))?;
        let saved = field(first, value)?;
        if !saved.is_object() || field(second, value)? != saved {
            return Err(format!("{second} did not return the original cached value"));
        }
        band(second, "/ttlMs", 0, 30_000)?;
    }
    equal("lease1", "/acquired", json!(true))?;
    equal("lease1", "/lease/owner", json!("owner-a"))?;
    equal("lease2", "/acquired", json!(false))?;
    equal("lease2", "/lease/owner", json!("owner-a"))?;
    equal("leaseC", "/lease", Value::Null)?;
    equal("lease3", "/acquired", json!(true))?;
    equal("lease3", "/lease/owner", json!("owner-b"))?;
    equal("strSet", "/value", json!("hello probe"))?;
    equal("strSet", "/has", json!(true))?;
    band("strSet", "/ttlMs", 55_000, 60_000)?;
    equal("strExp", "/updated", json!(true))?;
    band("strExp", "/ttlMs", 115_000, 120_000)?;
    equal("strPer", "/updated", json!(true))?;
    equal("strPer", "/ttlMs", Value::Null)?;
    equal("strDel", "/deleted", json!(true))?;
    equal("strDel", "/value", Value::Null)?;
    equal("strDel", "/has", json!(false))?;
    equal("keys", "/cursor", Value::Null)?;
    let keys = field("keys", "/keys")?
        .as_array()
        .ok_or("keys is not an array")?;
    for key in [
        "kv-demo:counter:visits",
        "kv-demo:cache:quote:probe-sku",
        "kv-demo:memo:probe-memo",
    ] {
        if !keys.contains(&json!(key)) {
            return Err(format!("listing omitted {key}"));
        }
    }
    for key in ["kv-demo:strings:greeting", "kv-demo:leases:deploy"] {
        if keys.contains(&json!(key)) {
            return Err(format!("listing retained deleted key {key}"));
        }
    }
    Ok(())
}

pub fn normalize(capture: Capture) -> Capture {
    fn normalize_value(value: &mut Value) {
        match value {
            Value::Object(object) => {
                for (key, value) in object {
                    if matches!(
                        key.as_str(),
                        "resetMs"
                            | "ttlMs"
                            | "expiresAt"
                            | "generatedAt"
                            | "builtAt"
                            | "createdAt"
                            | "acquiredAt"
                            | "nonce"
                            | "token"
                            | "leaseId"
                            | "price"
                    ) && !value.is_null()
                    {
                        *value = json!("<volatile>");
                    } else {
                        normalize_value(value);
                    }
                    if key == "keys" {
                        value
                            .as_array_mut()
                            .expect("keys array")
                            .sort_by_key(Value::to_string);
                    }
                }
            }
            Value::Array(values) => values.iter_mut().for_each(normalize_value),
            Value::String(key) if key.starts_with("kv-demo:rate:") => {
                if let Some((prefix, _)) = key.rsplit_once(':') {
                    *key = format!("{prefix}:<window>");
                }
            }
            _ => {}
        }
    }
    capture
        .into_iter()
        .map(|(label, mut value)| {
            normalize_value(&mut value);
            (label, value)
        })
        .collect()
}

pub fn assert_oracle_controls(capture: &Capture) {
    for (label, path, replacement) in [
        ("rate6", "/allowed", json!(true)),
        ("cache2", "/quote/price", json!(-1)),
        ("memo2", "/value/nonce", json!("recomputed")),
        ("strSet", "/ttlMs", json!(1_000)),
        ("strDel", "/has", json!(true)),
        ("keys", "/keys", json!([])),
    ] {
        let mut damaged = capture.clone();
        *damaged.get_mut(label).unwrap().pointer_mut(path).unwrap() = replacement;
        assert!(
            validate(&damaged).is_err(),
            "oracle accepted corrupted {label}{path}"
        );
    }
    let mut missing = capture.clone();
    missing.remove("strDel");
    assert!(
        validate(&missing).is_err(),
        "missing results cannot satisfy negative assertions"
    );
}
