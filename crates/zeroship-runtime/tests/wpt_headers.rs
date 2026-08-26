//! Run the W3C Web Platform Tests for `Headers` against the native impl.
//! Source: https://github.com/web-platform-tests/wpt (fetch/api/headers).
//! Files are vendored under `crates/runtime/tests/wpt/fetch/api/headers/`.
//!
//! Mirrors `wpt_text_encoding.rs` — we provide a minimal testharness.js
//! shim, run each file in a fresh V8 isolate, and report per-test
//! pass/fail. The Rust `#[test]` itself fails iff any non-skipped test
//! FAILs.
//!
//! Coverage targets per design's "Test plan" (the v1 list, excluding
//! Request/Response-gated files):
//!   - headers-basic.any.js
//!   - headers-casing.any.js
//!   - headers-combine.any.js
//!   - headers-errors.any.js
//!   - headers-normalize.any.js
//!   - header-values.any.js
//!   - header-values-normalize.any.js
//!   - headers-record.any.js
//!   - header-setcookie.any.js  (response-forbidden subtests skipped)
//!   - headers-structure.any.js
//!
//! Skipped (need Request/Response):
//!   - headers-no-cors.any.js

#![allow(unsafe_code)]

use std::collections::BTreeMap;

use zeroship_runtime::headers;
use zeroship_runtime::init_v8;

const TESTHARNESS_SHIM: &str = r#"
(function () {
  globalThis.__wpt_results = [];

  globalThis.self = globalThis;

  // testharness.js's `self.GLOBAL` exposes the test environment kind
  // (Window vs DedicatedWorker vs ServiceWorker etc.). headers-only
  // tests gate XHR/fetch checks on isWorker(); we report neither
  // (we are a Worker-like server runtime), but the headers checks
  // run unconditionally.
  globalThis.self.GLOBAL = {
    isWorker() { return true; },
    isShadowRealm() { return false; },
    isWindow() { return false; },
  };

  // testharness.js `setup(fn_or_props)` — runs the function (if any)
  // inside a try/catch that doesn't surface as a test failure. We
  // accept either a function or a properties object; for headers
  // tests the only call is `setup(function() { … })` to run setup
  // code at file load.
  globalThis.setup = function (arg) {
    if (typeof arg === "function") {
      try { arg(); } catch (_) {}
    }
    // If it's properties-object form, ignore — none of our targeted
    // headers tests use it.
  };

  // Response is Request/Response-gated; surface a stub that throws a
  // sentinel so test() classifies the call as skip rather than fail.
  // The native Response class lands alongside Request — at which
  // point this shim goes away.
  globalThis.Response = function () {
    const e = new Error("native Response not yet implemented (v1 skip)");
    e.__wpt_skip = true;
    throw e;
  };

  function fmt(v) {
    if (typeof v === "string") return JSON.stringify(v);
    if (Array.isArray(v)) return "[" + v.join(",") + "]";
    try { return String(v); } catch (_) { return "<unstringable>"; }
  }

  function sanitize(s) {
    if (typeof s !== "string") return s;
    let out = "";
    for (let i = 0; i < s.length; i++) {
      const cu = s.charCodeAt(i);
      if (cu >= 0xD800 && cu <= 0xDBFF) {
        const next = i + 1 < s.length ? s.charCodeAt(i + 1) : 0;
        if (next >= 0xDC00 && next <= 0xDFFF) {
          out += s[i] + s[i + 1];
          i++;
        } else {
          out += "\uFFFD";
        }
      } else if (cu >= 0xDC00 && cu <= 0xDFFF) {
        out += "\uFFFD";
      } else {
        out += s[i];
      }
    }
    return out;
  }

  globalThis.test = function (fn, name) {
    name = sanitize(name || fn.name || "<anonymous>");
    // testharness.js binds `this` inside the callback to a Test
    // object that exposes `add_cleanup`, `step`, etc. We provide a
    // minimal stub so headers-record's `this.add_cleanup(clearLog)`
    // doesn't TypeError. Cleanups are invoked even on test
    // success/failure to match the spec.
    const t = {
      _cleanups: [],
      add_cleanup(cb) { this._cleanups.push(cb); },
      step(cb) { return cb.apply(this, arguments); },
      step_func(cb) { return cb.bind(this); },
      done() {},
    };
    try {
      fn.call(t);
      __wpt_results.push({ name, status: "pass" });
    } catch (e) {
      if (e && e.__wpt_skip) {
        __wpt_results.push({ name, status: "skip", reason: sanitize(e.message) });
      } else {
        __wpt_results.push({
          name,
          status: "fail",
          error: sanitize(e && e.message ? e.message : String(e)),
        });
      }
    } finally {
      for (const cb of t._cleanups) {
        try { cb(); } catch (_) {}
      }
    }
  };

  globalThis.promise_test = function (fn, name) {
    name = sanitize(name || fn.name || "<anonymous async>");
    Promise.resolve().then(fn).then(
      () => __wpt_results.push({ name, status: "pass" }),
      e => __wpt_results.push({
        name,
        status: "fail",
        error: sanitize(e && e.message ? e.message : String(e)),
      })
    );
  };

  globalThis.async_test = function (_fn, name) {
    __wpt_results.push({
      name: name || "<async_test>",
      status: "skip",
      reason: "async_test stub",
    });
  };

  function fail(msg) { throw new Error(msg); }

  globalThis.assert_equals = function (actual, expected, msg) {
    const same = Object.is(actual, expected) || (actual === 0 && expected === 0);
    if (!same) fail(`${msg ? msg + ": " : ""}expected ${fmt(expected)}, got ${fmt(actual)}`);
  };
  globalThis.assert_not_equals = function (actual, expected, msg) {
    if (Object.is(actual, expected)) fail(`${msg ? msg + ": " : ""}expected NOT ${fmt(expected)}`);
  };
  globalThis.assert_true = function (cond, msg) {
    if (cond !== true) fail(`${msg ? msg + ": " : ""}expected true, got ${fmt(cond)}`);
  };
  globalThis.assert_false = function (cond, msg) {
    if (cond !== false) fail(`${msg ? msg + ": " : ""}expected false, got ${fmt(cond)}`);
  };
  globalThis.assert_array_equals = function (actual, expected, msg) {
    const a = Array.from(actual), e = Array.from(expected);
    if (a.length !== e.length) fail(`${msg ? msg + ": " : ""}length ${a.length} vs ${e.length}`);
    for (let i = 0; i < a.length; i++) {
      if (!Object.is(a[i], e[i])) fail(`${msg ? msg + ": " : ""}at [${i}]: expected ${fmt(e[i])}, got ${fmt(a[i])}`);
    }
  };
  globalThis.assert_throws_js = function (ctor, fn, msg) {
    try { fn(); }
    catch (e) {
      if (e instanceof ctor) return;
      if (e && e.name === ctor.name) return;
      fail(`${msg ? msg + ": " : ""}expected ${ctor.name}, got ${e && e.name ? e.name : fmt(e)}`);
    }
    fail(`${msg ? msg + ": " : ""}expected ${ctor.name} to be thrown`);
  };
  globalThis.assert_throws_exactly = function (expected, fn, msg) {
    try { fn(); }
    catch (e) {
      if (e === expected) return;
      fail(`${msg ? msg + ": " : ""}wrong throw: ${fmt(e)} vs ${fmt(expected)}`);
    }
    fail(`${msg ? msg + ": " : ""}expected to throw`);
  };
  globalThis.assert_unreached = function (msg) {
    fail(msg || "unreached");
  };
})();
"#;

#[derive(Debug, Clone)]
enum Outcome {
    Pass,
    Fail(String),
    Skip(String),
}

#[derive(Debug)]
struct TestResult {
    name: String,
    outcome: Outcome,
}

fn prepare_wpt_source(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    for line in source.lines() {
        let t = line.trim_start();
        if t.starts_with("// META:") || t.starts_with("//META:") {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn run_wpt(label: &str, source: &str) -> Vec<TestResult> {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    let global = scope.get_current_context().global(scope);
    headers::install_global(scope, global);

    let shim = v8::String::new(scope, TESTHARNESS_SHIM).unwrap();
    v8::Script::compile(scope, shim, None)
        .unwrap()
        .run(scope)
        .unwrap();

    let prepared = prepare_wpt_source(source);
    let src = v8::String::new(scope, &prepared).unwrap();

    let top_throw: Option<String> = {
        v8::tc_scope!(let tc, scope);
        match v8::Script::compile(tc, src, None) {
            Some(script) => match script.run(tc) {
                Some(_) => None,
                None => Some(
                    tc.exception()
                        .map(|e| e.to_rust_string_lossy(tc))
                        .unwrap_or_else(|| "<no exception>".into()),
                ),
            },
            None => Some(
                tc.exception()
                    .map(|e| e.to_rust_string_lossy(tc))
                    .unwrap_or_else(|| "<compile error>".into()),
            ),
        }
    };

    if let Some(err) = top_throw {
        return vec![TestResult {
            name: format!("<top-level run {label}>"),
            outcome: Outcome::Fail(err),
        }];
    }

    scope.perform_microtask_checkpoint();

    let read_src = v8::String::new(scope, "JSON.stringify(globalThis.__wpt_results)").unwrap();
    let json_v = v8::Script::compile(scope, read_src, None)
        .unwrap()
        .run(scope)
        .unwrap();
    let json = json_v.to_rust_string_lossy(scope);

    parse_results(&json)
}

#[derive(serde::Deserialize)]
struct RawResult {
    name: String,
    status: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

fn parse_results(json: &str) -> Vec<TestResult> {
    match serde_json::from_str::<Vec<RawResult>>(json) {
        Ok(raw) => raw
            .into_iter()
            .map(|r| TestResult {
                name: r.name,
                outcome: match r.status.as_str() {
                    "pass" => Outcome::Pass,
                    "fail" => Outcome::Fail(r.error.unwrap_or_else(|| "<no error>".into())),
                    "skip" => Outcome::Skip(r.reason.unwrap_or_else(|| "<no reason>".into())),
                    other => Outcome::Fail(format!("unknown status: {other}")),
                },
            })
            .collect(),
        Err(e) => vec![TestResult {
            name: "<parse __wpt_results>".to_string(),
            outcome: Outcome::Fail(format!(
                "JSON: {e}\nraw: {}",
                &json.chars().take(200).collect::<String>()
            )),
        }],
    }
}

const WPT_FILES: &[(&str, &str)] = &[
    (
        "headers-basic",
        include_str!("wpt/fetch/api/headers/headers-basic.any.js"),
    ),
    (
        "headers-casing",
        include_str!("wpt/fetch/api/headers/headers-casing.any.js"),
    ),
    (
        "headers-combine",
        include_str!("wpt/fetch/api/headers/headers-combine.any.js"),
    ),
    (
        "headers-errors",
        include_str!("wpt/fetch/api/headers/headers-errors.any.js"),
    ),
    (
        "headers-normalize",
        include_str!("wpt/fetch/api/headers/headers-normalize.any.js"),
    ),
    (
        "headers-record",
        include_str!("wpt/fetch/api/headers/headers-record.any.js"),
    ),
    (
        "header-setcookie",
        include_str!("wpt/fetch/api/headers/header-setcookie.any.js"),
    ),
    (
        "headers-structure",
        include_str!("wpt/fetch/api/headers/headers-structure.any.js"),
    ),
    // Skipped in v1 (need Request/Response):
    //   - header-values.any.js          — every test uses fetch/XHR
    //   - header-values-normalize.any.js — every test uses fetch/XHR
    //   - headers-no-cors.any.js         — Request guard semantics
    // The Headers ByteString validation that these tests would
    // exercise is covered by the hand-written `headers.rs` tests.
];

#[derive(Default, Debug)]
struct Totals {
    pass: usize,
    fail: usize,
    skip: usize,
}

impl Totals {
    fn add(&mut self, other: &Totals) {
        self.pass += other.pass;
        self.fail += other.fail;
        self.skip += other.skip;
    }
}

#[test]
fn wpt_headers_compliance() {
    let mut totals = Totals::default();
    let mut per_file: BTreeMap<&str, Totals> = BTreeMap::new();
    let mut failures: Vec<(&str, TestResult)> = Vec::new();

    for (name, source) in WPT_FILES {
        let results = run_wpt(name, source);
        let mut t = Totals::default();
        for r in results {
            match &r.outcome {
                Outcome::Pass => t.pass += 1,
                Outcome::Skip(_) => t.skip += 1,
                Outcome::Fail(_) => {
                    t.fail += 1;
                    failures.push((name, r));
                }
            }
        }
        totals.add(&t);
        per_file.insert(name, t);
    }

    eprintln!("\n=== WPT fetch/api/headers/ results ===");
    for (file, t) in &per_file {
        eprintln!(
            "  {:36} pass={:3} fail={:3} skip={:3}",
            file, t.pass, t.fail, t.skip
        );
    }
    eprintln!(
        "  {:-<36} pass={:3} fail={:3} skip={:3}",
        "total ", totals.pass, totals.fail, totals.skip
    );

    if !failures.is_empty() {
        eprintln!("\n=== Failures ===");
        for (file, r) in &failures {
            let detail = match &r.outcome {
                Outcome::Fail(s) => s.as_str(),
                _ => "?",
            };
            eprintln!("  [{file}] {}: {detail}", r.name);
        }
        panic!("{} WPT Headers tests failed", failures.len());
    }
}
