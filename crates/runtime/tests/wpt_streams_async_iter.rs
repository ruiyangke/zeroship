//! WPT runner for `streams/readable-streams/async-iterator.any.js`.
//! Source: https://github.com/web-platform-tests/wpt
//!
//! Mirrors `wpt_streams_tee.rs` shape. Per-file isolate, per-test
//! pass/fail/skip classification.
//!
//! v1 coverage (this dispatch — async iteration):
//!   - async-iterator.any.js
//!
//! Deferred:
//!   - patched-global iteration (deferred to D-19 polyfill cutover phase 3)
//!   - GC-driven tests (`garbageCollect()` is a no-op in our runtime)

#![allow(unsafe_code)]

use std::collections::BTreeMap;

use zeroship_runtime::init_v8;
use zeroship_runtime::streams::install_native_streams;
use zeroship_runtime::streams::strategies::{
    install_byte_length_queuing_strategy, install_count_queuing_strategy,
};

const TESTHARNESS_SHIM: &str = r#"
(function () {
  globalThis.__wpt_results = [];

  globalThis.self = globalThis;
  globalThis.self.GLOBAL = {
    isWorker() { return true; },
    isShadowRealm() { return false; },
    isWindow() { return false; },
  };

  globalThis.setup = function (arg) {
    if (typeof arg === "function") {
      try { arg(); } catch (_) {}
    }
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
        if (next >= 0xDC00 && next <= 0xDFFF) { out += s[i] + s[i + 1]; i++; }
        else { out += "\uFFFD"; }
      } else if (cu >= 0xDC00 && cu <= 0xDFFF) {
        out += "\uFFFD";
      } else { out += s[i]; }
    }
    return out;
  }

  globalThis.step_timeout = function (cb, _ms) {
    let p = Promise.resolve();
    for (let i = 0; i < 32; i++) p = p.then(() => undefined);
    p.then(cb);
    return 0;
  };
  globalThis.setTimeout = globalThis.step_timeout;
  globalThis.clearTimeout = function () {};
  globalThis.garbageCollect = function () {};

  globalThis.__wpt_skip = function (reason) {
    const e = new Error(reason);
    e.__wpt_skip = true;
    throw e;
  };

  globalThis.test = function (fn, name) {
    name = sanitize(name || fn.name || "<anonymous>");
    const t = makeTestObj();
    try {
      fn.call(t, t);
      if (t._failed) {
        __wpt_results.push({ name, status: "fail", error: sanitize(t._failMsg || "step_func failed") });
      } else {
        __wpt_results.push({ name, status: "pass" });
      }
    } catch (e) {
      if (e && e.__wpt_skip) {
        __wpt_results.push({ name, status: "skip", reason: sanitize(e.message) });
      } else {
        __wpt_results.push({ name, status: "fail", error: sanitize(e && e.message ? e.message : String(e)) });
      }
    } finally {
      for (const cb of t._cleanups) {
        try { cb(); } catch (_) {}
      }
    }
  };

  function makeTestObj() {
    const t = {
      _cleanups: [],
      _failed: false,
      _failMsg: null,
      add_cleanup(cb) { this._cleanups.push(cb); },
      step(cb) { return cb.apply(this, arguments); },
      step_func(cb) {
        const that = this;
        return function () {
          try { return cb.apply(that, arguments); }
          catch (e) { that._failed = true; that._failMsg = e && e.message ? e.message : String(e); throw e; }
        };
      },
      step_func_done(cb) { return this.step_func(cb); },
      unreached_func(msg) {
        const that = this;
        return function () { that._failed = true; that._failMsg = msg || "unreached"; throw new Error(that._failMsg); };
      },
      done() {},
    };
    return t;
  }

  globalThis.promise_test = function (fn, name) {
    name = sanitize(name || fn.name || "<anonymous async>");
    let pushed = false;
    const t = makeTestObj();
    try {
      const p = fn.call(t, t);
      Promise.resolve(p).then(
        () => {
          if (!pushed) {
            pushed = true;
            if (t._failed) {
              __wpt_results.push({ name, status: "fail", error: sanitize(t._failMsg || "step_func failed") });
            } else {
              __wpt_results.push({ name, status: "pass" });
            }
          }
          for (const cb of t._cleanups) { try { cb(); } catch (_) {} }
        },
        e => {
          if (!pushed) {
            pushed = true;
            if (e && e.__wpt_skip) {
              __wpt_results.push({ name, status: "skip", reason: sanitize(e.message) });
            } else {
              __wpt_results.push({ name, status: "fail", error: sanitize(e && e.message ? e.message : String(e)) });
            }
          }
          for (const cb of t._cleanups) { try { cb(); } catch (_) {} }
        }
      );
    } catch (e) {
      pushed = true;
      __wpt_results.push({ name, status: "fail", error: sanitize(e && e.message ? e.message : String(e)) });
    }
  };

  globalThis.async_test = function (fn, name) {
    __wpt_results.push({ name: sanitize(name || (fn && fn.name) || "<async_test>"), status: "skip", reason: "async_test stub" });
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
  globalThis.assert_object_equals = function (actual, expected, msg) {
    if (actual === null || typeof actual !== "object") fail(`${msg ? msg + ": " : ""}actual is not an object: ${fmt(actual)}`);
    const keysA = Object.keys(actual).sort();
    const keysE = Object.keys(expected).sort();
    if (keysA.length !== keysE.length || keysA.some((k, i) => k !== keysE[i])) {
      fail(`${msg ? msg + ": " : ""}keys differ: ${fmt(keysA)} vs ${fmt(keysE)}`);
    }
    for (const k of keysA) {
      if (!Object.is(actual[k], expected[k])) fail(`${msg ? msg + ": " : ""}at ${fmt(k)}: expected ${fmt(expected[k])}, got ${fmt(actual[k])}`);
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
    catch (e) { if (e === expected) return; fail(`${msg ? msg + ": " : ""}wrong throw: ${fmt(e)} vs ${fmt(expected)}`); }
    fail(`${msg ? msg + ": " : ""}expected to throw`);
  };
  globalThis.assert_unreached = function (msg) { fail(msg || "unreached"); };

  globalThis.promise_rejects_js = function (t, ctor, p, msg) {
    return p.then(
      () => fail(`${msg ? msg + ": " : ""}expected ${ctor.name}; got fulfillment`),
      e => {
        if (e instanceof ctor || (e && e.name === ctor.name)) return;
        fail(`${msg ? msg + ": " : ""}expected ${ctor.name}, got ${e && e.name ? e.name : fmt(e)}`);
      }
    );
  };
  globalThis.promise_rejects_exactly = function (t, expected, p, msg) {
    return p.then(
      () => fail(`${msg ? msg + ": " : ""}expected throw; got fulfillment`),
      e => { if (e === expected) return; fail(`${msg ? msg + ": " : ""}wrong throw: ${fmt(e)} vs ${fmt(expected)}`); }
    );
  };
  globalThis.delay = function (ms) {
    return new Promise(r => globalThis.setTimeout(r, ms));
  };
  globalThis.flushAsyncEvents = function () {
    return Promise.resolve()
      .then(() => Promise.resolve())
      .then(() => Promise.resolve())
      .then(() => Promise.resolve())
      .then(() => Promise.resolve())
      .then(() => Promise.resolve());
  };
})();
"#;

#[derive(Debug, Clone)]
#[allow(dead_code)]
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

fn run_wpt(label: &str, sources: &[&str]) -> Vec<TestResult> {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    let global = scope.get_current_context().global(scope);
    install_byte_length_queuing_strategy(scope, global);
    install_count_queuing_strategy(scope, global);
    install_native_streams(scope, global);

    let shim = v8::String::new(scope, TESTHARNESS_SHIM).unwrap();
    v8::Script::compile(scope, shim, None)
        .unwrap()
        .run(scope)
        .unwrap();

    for (idx, source) in sources.iter().enumerate() {
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
                name: format!("<top-level run {label} src#{idx}>"),
                outcome: Outcome::Fail(err),
            }];
        }
    }

    // Async iterator chains can be deeply chained promises (each await is
    // 1+ microtask); pipe-like tests need many drain rounds.
    for _ in 0..1024 {
        scope.perform_microtask_checkpoint();
    }

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
            .map(|r| {
                let name = r.name;
                let outcome = match r.status.as_str() {
                    "pass" => Outcome::Pass,
                    "fail" => Outcome::Fail(r.error.unwrap_or_else(|| "<no error>".into())),
                    "skip" => Outcome::Skip(r.reason.unwrap_or_else(|| "<no reason>".into())),
                    other => Outcome::Fail(format!("unknown status: {other}")),
                };
                TestResult { name, outcome }
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

const TEST_UTILS: &str = include_str!("../../../tests/wpt/streams/resources/test-utils.js");
const RECORDING_STREAMS: &str = include_str!("../../../tests/wpt/streams/resources/recording-streams.js");
const RS_UTILS: &str = include_str!("../../../tests/wpt/streams/resources/rs-utils.js");

const WPT_FILES: &[(&str, &[&str])] = &[(
    "async-iterator",
    &[
        TEST_UTILS,
        RS_UTILS,
        RECORDING_STREAMS,
        include_str!("../../../tests/wpt/streams/readable-streams/async-iterator.any.js"),
    ],
)];

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
fn wpt_streams_async_iter_compliance() {
    let mut totals = Totals::default();
    let mut per_file: BTreeMap<&str, Totals> = BTreeMap::new();
    let mut failures: Vec<(&str, TestResult)> = Vec::new();
    let mut skips: Vec<(&str, String, String)> = Vec::new();

    for (name, sources) in WPT_FILES {
        let results = run_wpt(name, sources);
        let mut t = Totals::default();
        for r in results {
            match &r.outcome {
                Outcome::Pass => t.pass += 1,
                Outcome::Skip(reason) => {
                    t.skip += 1;
                    skips.push((name, r.name.clone(), reason.clone()));
                }
                Outcome::Fail(_) => {
                    t.fail += 1;
                    failures.push((name, r));
                }
            }
        }
        totals.add(&t);
        per_file.insert(name, t);
    }

    eprintln!("\n=== WPT streams/readable-streams/async-iterator.any.js results ===");
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

    if !skips.is_empty() {
        eprintln!("\n=== Skipped ===");
        for (file, name, reason) in &skips {
            eprintln!("  [{file}] {name}: {reason}");
        }
    }

    // Deferred-known: tests that fail for reasons explicitly out of scope of
    // this dispatch. Trim as failures get fixed.
    let deferred_known: &[(&str, &str)] = &[];

    let mut unexpected: Vec<&(&str, TestResult)> = Vec::new();
    for f in &failures {
        let known = deferred_known
            .iter()
            .any(|(file, name)| *file == f.0 && f.1.name == *name);
        if !known {
            unexpected.push(f);
        }
    }

    if !failures.is_empty() {
        eprintln!("\n=== Failures ===");
        for (file, r) in &failures {
            let detail = match &r.outcome {
                Outcome::Fail(s) => s.as_str(),
                _ => "?",
            };
            eprintln!("  [{file}] {}: {detail}", r.name);
        }
    }

    let attempted = totals.pass + totals.fail;
    if attempted == 0 {
        panic!("no WPT async-iter subtests ran");
    }
    let pass_rate = (totals.pass as f64) / (attempted as f64);
    eprintln!(
        "\n=== pass rate (excluding skips): {:.1}% ({}/{}) ===",
        pass_rate * 100.0,
        totals.pass,
        attempted
    );

    if !unexpected.is_empty() && pass_rate < 0.85 {
        panic!(
            "WPT async-iter pass rate {:.1}% < 85% threshold — {} unexpected failures",
            pass_rate * 100.0,
            unexpected.len()
        );
    }
}
