//! Run the W3C Web Platform Tests for TextEncoder/TextDecoder against
//! our native impl. Source: https://github.com/web-platform-tests/wpt
//! (encoding/ subdirectory). Files are vendored under `tests/wpt/`.
//!
//! Each WPT file uses the testharness.js framework — `test(fn, name)`,
//! `assert_equals`, `assert_array_equals`, etc. We provide a minimal
//! shim, run each file in a fresh V8 isolate, and report per-test
//! pass/fail.
//!
//! Outcome classification:
//!   - PASS: assertions held
//!   - FAIL: real spec divergence
//!   - SKIP: depends on a feature we don't ship (utf-16, SAB,
//!     encodeInto)
//!
//! The Rust `#[test]` itself fails iff any non-skipped test FAILs.

#![allow(unsafe_code)]

use std::collections::BTreeMap;

use zeroship_runtime::init_v8;
use zeroship_runtime::text_encoding::{TextDecoder, TextEncoder};

// ---------------------------------------------------------------------------
// Minimal testharness.js shim
// ---------------------------------------------------------------------------

const TESTHARNESS_SHIM: &str = r#"
(function () {
  globalThis.__wpt_results = [];

  // Browsers and Web Workers expose `self` as an alias for the
  // global object. WPT tests use it freely (e.g. `self.DataView`,
  // `self.MessageChannel`). Mirror the pattern so tests don't
  // throw "self is not defined" looking up otherwise-existing
  // built-ins on globalThis.
  globalThis.self = globalThis;

  // Minimal `MessageChannel` stub: the only thing WPT encoding tests
  // use it for is detaching an ArrayBuffer via the transferable
  // list (`new MessageChannel().port1.postMessage(buf, [buf])`).
  // V8 ships `ArrayBuffer.prototype.transfer()` natively, which is
  // semantically equivalent to the postMessage-transfer detach.
  // We don't ship the real postMessage / cross-realm channel —
  // this is just enough for the detach side-effect.
  if (typeof globalThis.MessageChannel === "undefined") {
    function _MessagePort() {}
    _MessagePort.prototype.postMessage = function (_msg, transfers) {
      if (transfers) {
        for (let i = 0; i < transfers.length; i++) {
          const t = transfers[i];
          if (t instanceof ArrayBuffer && typeof t.transfer === "function") {
            // transfer(0) detaches and yields a 0-length new buffer.
            t.transfer(0);
          }
        }
      }
    };
    _MessagePort.prototype.close = function () {};
    _MessagePort.prototype.start = function () {};
    function _MessageChannel() {
      this.port1 = new _MessagePort();
      this.port2 = new _MessagePort();
    }
    globalThis.MessageChannel = _MessageChannel;
  }

  function fmt(v) {
    if (typeof v === "string") return JSON.stringify(v);
    if (Array.isArray(v)) return "[" + v.join(",") + "]";
    try { return String(v); } catch (_) { return "<unstringable>"; }
  }

  // WPT test names sometimes embed test-data strings that contain
  // unpaired surrogates (e.g. encodeInto.any.js iterates over
  // `"\uD834A\uDF06A¥Hi"` and templates it into the test name).
  // Older JSON.stringify implementations emit these as raw lone
  // surrogates in the output, breaking strict JSON parsers
  // (serde_json rejects). Replace any unpaired surrogate with
  // U+FFFD before serialization — the test outcome is unchanged,
  // only the name string is sanitized.
  function sanitize(s) {
    if (typeof s !== "string") return s;
    let out = "";
    for (let i = 0; i < s.length; i++) {
      const cu = s.charCodeAt(i);
      if (cu >= 0xD800 && cu <= 0xDBFF) {
        // High surrogate — must be followed by a low surrogate.
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
    try {
      fn();
      __wpt_results.push({ name, status: "pass" });
    } catch (e) {
      if (e && e.__wpt_skip) {
        __wpt_results.push({ name, status: "skip", reason: sanitize(e.message) });
        return;
      }
      __wpt_results.push({
        name,
        status: "fail",
        error: sanitize(e && e.message ? e.message : String(e)),
      });
    }
  };

  // Encoding tests don't use promise_test currently. If they did,
  // we'd need to drain microtasks after each file run; cheap to
  // include the wrapper.
  globalThis.promise_test = function (fn, name) {
    name = name || fn.name || "<anonymous async>";
    Promise.resolve().then(fn).then(
      () => __wpt_results.push({ name, status: "pass" }),
      e => __wpt_results.push({
        name,
        status: "fail",
        error: e && e.message ? e.message : String(e),
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

  // resources/sab.js export. SharedArrayBuffer is an ECMAScript
  // built-in shipped by V8 since ES2017 — no extra setup needed at
  // the runtime layer; the global is already there. Without
  // Workers it's behaviorally identical to ArrayBuffer (no other
  // agent to share with), but the class itself, BufferSource union
  // membership, and the [AllowShared] IDL attribute all work.
  globalThis.createBuffer = function (kind, length) {
    if (kind === "SharedArrayBuffer") {
      return new SharedArrayBuffer(length);
    }
    return new ArrayBuffer(length);
  };
})();
"#;

// ---------------------------------------------------------------------------
// Outcome model
// ---------------------------------------------------------------------------

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

/// All WPT encoding tests run against our impl. Empty by default;
/// kept as a hook for future selective skips if a specific
/// parameterization needs one.
fn should_skip_by_name(_name: &str) -> bool {
    false
}

// ---------------------------------------------------------------------------
// Source preparation
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// V8 runner
// ---------------------------------------------------------------------------

fn run_wpt(label: &str, source: &str) -> Vec<TestResult> {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    // Install our native classes.
    let global = scope.get_current_context().global(scope);
    for (name, tmpl) in [
        ("TextEncoder", TextEncoder::install(scope)),
        ("TextDecoder", TextDecoder::install(scope)),
    ] {
        let class_fn = tmpl.get_function(scope).unwrap();
        let key = v8::String::new(scope, name).unwrap();
        global.set(scope, key.into(), class_fn.into());
    }

    // Inject the testharness shim.
    let shim = v8::String::new(scope, TESTHARNESS_SHIM).unwrap();
    v8::Script::compile(scope, shim, None)
        .unwrap()
        .run(scope)
        .unwrap();

    // Compile + run the WPT file. Top-level throws are recorded as a
    // single fail entry (the file's tests don't run).
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

    // Drain promise microtasks (no-op for files that don't use them).
    scope.perform_microtask_checkpoint();

    // Read globalThis.__wpt_results out as JSON.
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

// ---------------------------------------------------------------------------
// The actual #[test]
// ---------------------------------------------------------------------------

const WPT_FILES: &[(&str, &str)] = &[
    ("api-basics", include_str!("wpt/api-basics.any.js")),
    ("api-surrogates-utf8", include_str!("wpt/api-surrogates-utf8.any.js")),
    ("textdecoder-arguments", include_str!("wpt/textdecoder-arguments.any.js")),
    ("textdecoder-byte-order-marks", include_str!("wpt/textdecoder-byte-order-marks.any.js")),
    ("textdecoder-eof", include_str!("wpt/textdecoder-eof.any.js")),
    ("textdecoder-fatal", include_str!("wpt/textdecoder-fatal.any.js")),
    ("textdecoder-streaming", include_str!("wpt/textdecoder-streaming.any.js")),
    ("textdecoder-utf16-surrogates", include_str!("wpt/textdecoder-utf16-surrogates.any.js")),
    ("encodeInto", include_str!("wpt/encodeInto.any.js")),
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
fn wpt_text_encoding_utf8_compliance() {
    let mut totals = Totals::default();
    let mut per_file: BTreeMap<&str, Totals> = BTreeMap::new();
    let mut failures: Vec<(&str, TestResult)> = Vec::new();

    for (name, source) in WPT_FILES {
        let results = run_wpt(name, source);
        let mut t = Totals::default();
        for r in results {
            // First-pass: skip by test-name heuristic (utf-16, SAB,
            // encodeInto). Only after that do we treat a "fail"
            // outcome as a real failure.
            if should_skip_by_name(&r.name) {
                t.skip += 1;
                continue;
            }
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

    eprintln!("\n=== WPT encoding/ results ===");
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
        panic!("{} WPT UTF-8 tests failed", failures.len());
    }
}
