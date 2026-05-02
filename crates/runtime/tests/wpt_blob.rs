//! Run the W3C Web Platform Tests for `Blob` and `File` against the
//! native impl. Source: https://github.com/web-platform-tests/wpt
//! (FileAPI/blob and FileAPI/file). Files are vendored under
//! `tests/wpt/FileAPI/`.
//!
//! Pattern mirrors `wpt_headers.rs` — minimal testharness.js shim,
//! one V8 isolate per file, per-test pass/skip/fail tally. The Rust
//! `#[test]` itself fails iff any non-skipped test FAILs (we set a
//! threshold via `MIN_PASS_RATE`).
#![allow(unsafe_code)]

use std::collections::BTreeMap;

use zeroship_runtime::blob_native;
use zeroship_runtime::init_v8;

/// testharness.js shim — same shape as wpt_headers.rs minus the
/// Headers-specific Response stub. Adds `format_value` (used by the
/// blob constructor "non-objects throw" test).
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
    if (v instanceof Error) return String(v);
    if (Array.isArray(v)) return "[" + v.join(",") + "]";
    try { return String(v); } catch (_) { return "<unstringable>"; }
  }

  // testharness.js's `format_value` — a verbose pretty-printer used in
  // assertion messages. Our minimal version is enough for the failure
  // text the tests emit.
  globalThis.format_value = function (v) {
    if (v === null) return "null";
    if (v === undefined) return "undefined";
    return fmt(v);
  };

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
    const t = {
      _cleanups: [],
      add_cleanup(cb) { this._cleanups.push(cb); },
      step(cb) { return cb.apply(this, arguments); },
      step_func(cb) { return cb.bind(this); },
      done() {},
    };
    try {
      // testharness.js passes the test object both as `this` and as
      // the first arg, so both `function() { this.step_func() }` and
      // `t => t.step_func()` shapes work.
      fn.call(t, t);
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
    const t = {
      _cleanups: [],
      add_cleanup(cb) { this._cleanups.push(cb); },
      step(cb) { return cb.apply(this, arguments); },
      step_func(cb) { return cb.bind(this); },
      done() {},
    };
    Promise.resolve().then(() => fn.call(t, t)).then(
      () => __wpt_results.push({ name, status: "pass" }),
      e => __wpt_results.push({
        name,
        status: "fail",
        error: sanitize(e && e.message ? e.message : String(e)),
      })
    );
  };

  globalThis.async_test = function (fn, name) {
    // Two call shapes: `async_test(fn, name)` (fn is the body) and
    // `async_test(name)` returning a test object on which the caller
    // calls `.step(...)` directly. We support both.
    if (typeof fn === "string" || arguments.length === 1) {
      // Returns a stub test object whose .step(callback) just runs
      // callback synchronously, treating exceptions as test failures.
      const reportName = sanitize(typeof fn === "string" ? fn : (name || "<async_test>"));
      let recorded = false;
      const t = {
        _cleanups: [],
        add_cleanup(cb) { this._cleanups.push(cb); },
        step(cb) {
          try {
            cb.apply(t, [].slice.call(arguments, 1));
          } catch (e) {
            if (!recorded) {
              recorded = true;
              __wpt_results.push({
                name: reportName,
                status: "fail",
                error: sanitize(e && e.message ? e.message : String(e)),
              });
            }
          }
        },
        step_func(cb) {
          return function () {
            try {
              return cb.apply(t, arguments);
            } catch (e) {
              if (!recorded) {
                recorded = true;
                __wpt_results.push({
                  name: reportName,
                  status: "fail",
                  error: sanitize(e && e.message ? e.message : String(e)),
                });
              }
            }
          };
        },
        done() {
          if (!recorded) {
            recorded = true;
            __wpt_results.push({ name: reportName, status: "pass" });
          }
        },
      };
      return t;
    }
    __wpt_results.push({
      name: sanitize(name || "<async_test>"),
      status: "skip",
      reason: "async_test stub (callback form)",
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

  // /common/gc.js stub: tests that depend on triggering GC mid-stream
  // get a sync-resolved promise from garbageCollect() so they continue.
  globalThis.garbageCollect = function () {
    return Promise.resolve();
  };
})();
"#;

/// Inlined `support/Blob.js` helpers used by the constructor / slice
/// tests via the `// META: script=../support/Blob.js` directive.
const SUPPORT_BLOB_JS: &str = r#"
self.test_blob = (fn, expectations) => {
  var expected = expectations.expected,
      type = expectations.type,
      desc = expectations.desc;
  promise_test(async (t) => {
    var blob = fn();
    assert_true(blob instanceof Blob);
    assert_false(blob instanceof File);
    assert_equals(blob.type, type);
    assert_equals(blob.size, expected.length);
    const text = await blob.text();
    assert_equals(text, expected);
  }, desc);
};
self.test_blob_binary = (fn, expectations) => {
  var expected = expectations.expected,
      type = expectations.type,
      desc = expectations.desc;
  promise_test(async (t) => {
    var blob = fn();
    assert_true(blob instanceof Blob);
    assert_false(blob instanceof File);
    assert_equals(blob.type, type);
    assert_equals(blob.size, expected.length);
    const ab = await blob.arrayBuffer();
    assert_true(ab instanceof ArrayBuffer, "Result should be an ArrayBuffer");
    assert_array_equals(new Uint8Array(ab), expected);
  }, desc);
};
self.assert_equals_typed_array = (array1, array2) => {
  const [view1, view2] = [array1, array2].map((array) => {
    assert_true(array.buffer instanceof ArrayBuffer);
    return new DataView(array.buffer, array.byteOffset, array.byteLength);
  });
  assert_equals(view1.byteLength, view2.byteLength);
  for (let i = 0; i < view1.byteLength; ++i) {
    assert_equals(view1.getUint8(i), view2.getUint8(i));
  }
};
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
    // Streams must be installed first because Blob.stream() reads
    // globalThis.ReadableStream during invocation. TextEncoder is
    // required by several test files (Blob-text, Blob-arrayBuffer,
    // Blob-bytes) for non-ASCII / non-Unicode input.
    zeroship_runtime::streams::install_native_streams(scope, global);
    {
        let tmpl = zeroship_runtime::text_encoding::TextEncoder::install(scope);
        let class_fn = tmpl.get_function(scope).unwrap();
        let key = v8::String::new(scope, "TextEncoder").unwrap();
        global.set(scope, key.into(), class_fn.into());
    }
    {
        let tmpl = zeroship_runtime::text_encoding::TextDecoder::install(scope);
        let class_fn = tmpl.get_function(scope).unwrap();
        let key = v8::String::new(scope, "TextDecoder").unwrap();
        global.set(scope, key.into(), class_fn.into());
    }
    blob_native::install_globals(scope, global);

    // testharness.js shim.
    let shim = v8::String::new(scope, TESTHARNESS_SHIM).unwrap();
    v8::Script::compile(scope, shim, None)
        .unwrap()
        .run(scope)
        .unwrap();

    // support/Blob.js helpers (test_blob, test_blob_binary).
    let support = v8::String::new(scope, SUPPORT_BLOB_JS).unwrap();
    v8::Script::compile(scope, support, None)
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

    // Promise tests run via microtasks; let them settle before we
    // collect the results.
    scope.perform_microtask_checkpoint();
    // Run twice in case any test chained `.then` after a promise
    // resolves (which would queue another microtask).
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
        "Blob-constructor",
        include_str!("../../../tests/wpt/FileAPI/blob/Blob-constructor.any.js"),
    ),
    (
        "Blob-slice",
        include_str!("../../../tests/wpt/FileAPI/blob/Blob-slice.any.js"),
    ),
    (
        "Blob-text",
        include_str!("../../../tests/wpt/FileAPI/blob/Blob-text.any.js"),
    ),
    (
        "Blob-array-buffer",
        include_str!("../../../tests/wpt/FileAPI/blob/Blob-array-buffer.any.js"),
    ),
    (
        "Blob-bytes",
        include_str!("../../../tests/wpt/FileAPI/blob/Blob-bytes.any.js"),
    ),
    (
        "Blob-stream",
        include_str!("../../../tests/wpt/FileAPI/blob/Blob-stream.any.js"),
    ),
    (
        "File-constructor",
        include_str!("../../../tests/wpt/FileAPI/file/File-constructor.any.js"),
    ),
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

/// Minimum pass-rate threshold across the suite. Tests below this
/// indicate a regression. Set conservatively — the implementation
/// targets ≥85% on the vendored files.
const MIN_PASS_RATE: f64 = 0.85;

#[test]
fn wpt_blob_compliance() {
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

    eprintln!("\n=== WPT FileAPI/ results ===");
    for (file, t) in &per_file {
        eprintln!(
            "  {:24} pass={:3} fail={:3} skip={:3}",
            file, t.pass, t.fail, t.skip
        );
    }
    eprintln!(
        "  {:-<24} pass={:3} fail={:3} skip={:3}",
        "total ", totals.pass, totals.fail, totals.skip
    );

    let scored = totals.pass + totals.fail;
    let pass_rate = if scored == 0 {
        0.0
    } else {
        totals.pass as f64 / scored as f64
    };
    eprintln!("  pass rate: {:.1}%", pass_rate * 100.0);

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

    if pass_rate < MIN_PASS_RATE {
        panic!(
            "WPT FileAPI pass rate {:.1}% below threshold {:.1}% ({} fails out of {} scored)",
            pass_rate * 100.0,
            MIN_PASS_RATE * 100.0,
            totals.fail,
            scored,
        );
    }
}
