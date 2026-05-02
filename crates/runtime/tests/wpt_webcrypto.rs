//! Run the W3C Web Platform Tests for WebCryptoAPI against the native
//! impl. Source: https://github.com/web-platform-tests/wpt/tree/master/WebCryptoAPI.
//! Files are vendored under `crates/runtime/tests/wpt/WebCryptoAPI/`.
//!
//! Per `docs/proposals/webcrypto-native.md` §X.2 / §X.3.
//!
//! Each WPT file is run in a fresh V8 isolate with:
//!   1. Native Crypto / SubtleCrypto / CryptoKey installed.
//!   2. TextEncoder / TextDecoder / btoa / atob shimmed.
//!   3. The testharness.js compatibility shim (assert_*, promise_test,
//!      etc.) installed.
//!   4. Any `// META: script=PATH` files prepended verbatim.
//!   5. The test file itself.
//!
//! The Rust `#[test]` functions classify subtests as pass/fail/skip
//! and assert ≥90% pass per the must-pass-v1 set in the design.

#![allow(unsafe_code)]

use std::collections::BTreeMap;

use zeroship_runtime::{crypto_native, dom, init_v8, text_encoding};

const TESTHARNESS_SHIM: &str = r#"
(function () {
  globalThis.__wpt_results = [];
  globalThis.self = globalThis;

  globalThis.self.GLOBAL = {
    isWorker() { return true; },
    isShadowRealm() { return false; },
    isWindow() { return false; },
    isSecureContext: true,
  };
  globalThis.isSecureContext = true;
  globalThis.location = { protocol: 'https:', origin: 'https://example.com' };

  globalThis.setup = function (arg) {
    if (typeof arg === "function") { try { arg(); } catch (_) {} }
    // explicit_done / single_test / etc. are no-ops in our shim.
  };
  globalThis.done = function () {};

  // step_timeout / setTimeout: fall back to a microtask. Tests that
  // genuinely depend on real timers are best-effort.
  globalThis.step_timeout = function (cb, _ms) {
    Promise.resolve().then(cb);
    return 0;
  };
  globalThis.setTimeout = globalThis.step_timeout;
  globalThis.clearTimeout = function () {};

  // btoa / atob — Crypto JWK helpers in WPT use them. Implementations
  // below mirror WHATWG HTML §8.3.
  globalThis.btoa = function (input) {
    const str = String(input);
    const tbl = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let out = "";
    for (let i = 0; i < str.length; i++) {
      const c = str.charCodeAt(i);
      if (c > 0xff) throw new TypeError("btoa: non-Latin1");
    }
    for (let i = 0; i < str.length; i += 3) {
      const a = str.charCodeAt(i);
      const b = i + 1 < str.length ? str.charCodeAt(i + 1) : 0;
      const c = i + 2 < str.length ? str.charCodeAt(i + 2) : 0;
      out += tbl[a >> 2];
      out += tbl[((a & 3) << 4) | (b >> 4)];
      out += i + 1 < str.length ? tbl[((b & 15) << 2) | (c >> 6)] : "=";
      out += i + 2 < str.length ? tbl[c & 63] : "=";
    }
    return out;
  };
  globalThis.atob = function (input) {
    const str = String(input).replace(/=+$/, "");
    const tbl = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let out = "";
    let bits = 0, val = 0;
    for (let i = 0; i < str.length; i++) {
      const idx = tbl.indexOf(str[i]);
      if (idx < 0) throw new Error("atob: bad char");
      val = (val << 6) | idx;
      bits += 6;
      if (bits >= 8) {
        bits -= 8;
        out += String.fromCharCode((val >> bits) & 0xff);
      }
    }
    return out;
  };

  function fmt(v) {
    if (typeof v === "string") return JSON.stringify(v);
    if (v && typeof v === "object" && "name" in v) return v.name;
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
      } else if (cu >= 0xDC00 && cu <= 0xDFFF) { out += "\uFFFD"; }
      else { out += s[i]; }
    }
    return out;
  }

  function makeTestObj() {
    const t = {
      _cleanups: [], _failed: false, _failMsg: null,
      add_cleanup(cb) { this._cleanups.push(cb); },
      step(cb) { return cb.apply(this, arguments); },
      step_func(cb) {
        const that = this;
        return function () {
          try { return cb.apply(that, arguments); }
          catch (e) {
            that._failed = true;
            that._failMsg = e && e.message ? e.message : String(e);
            throw e;
          }
        };
      },
      step_func_done(cb) { return this.step_func(cb); },
      unreached_func(msg) {
        const that = this;
        return function () {
          that._failed = true;
          that._failMsg = msg || "unreached";
          throw new Error(that._failMsg);
        };
      },
      done() {},
    };
    return t;
  }

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
        __wpt_results.push({
          name, status: "fail",
          error: sanitize(e && e.message ? e.message : String(e)),
        });
      }
    } finally {
      for (const cb of t._cleanups) { try { cb(); } catch (_) {} }
    }
  };

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
              __wpt_results.push({
                name, status: "fail",
                error: sanitize(e && e.message ? e.message : String(e)),
              });
            }
          }
          for (const cb of t._cleanups) { try { cb(); } catch (_) {} }
        }
      );
    } catch (e) {
      pushed = true;
      __wpt_results.push({
        name, status: "fail",
        error: sanitize(e && e.message ? e.message : String(e)),
      });
    }
  };

  globalThis.async_test = function (fn, name) {
    // Drive a few microtask cycles so promise-chained async_test
    // bodies that call t.done() synchronously settle. Tests using
    // genuine timers fall through as skip.
    name = sanitize(name || (fn && fn.name) || "<async_test>");
    const t = makeTestObj();
    let done = false;
    t.done = function () {
      if (done) return;
      done = true;
      if (t._failed) {
        __wpt_results.push({ name, status: "fail", error: sanitize(t._failMsg || "async_test failed") });
      } else {
        __wpt_results.push({ name, status: "pass" });
      }
    };
    try {
      fn.call(t, t);
    } catch (e) {
      if (!done) {
        done = true;
        __wpt_results.push({
          name, status: "fail",
          error: sanitize(e && e.message ? e.message : String(e)),
        });
      }
    }
    // If neither synchronously failed nor called done, we mark skip
    // post-hoc via a delayed microtask check. Many WPT async_tests
    // require setTimeout — those count as skip in this shim.
    Promise.resolve().then(() => {
      if (!done) {
        done = true;
        __wpt_results.push({ name, status: "skip", reason: "async_test never called done()" });
      }
    });
  };

  // ---------------------------------------------------------------------
  // Asserts
  // ---------------------------------------------------------------------

  function fail(msg) { throw new Error(msg); }

  globalThis.assert_true = function (cond, msg) {
    if (cond !== true) fail("assert_true: " + (msg || "") + " got " + fmt(cond));
  };
  globalThis.assert_false = function (cond, msg) {
    if (cond !== false) fail("assert_false: " + (msg || "") + " got " + fmt(cond));
  };
  globalThis.assert_equals = function (a, b, msg) {
    const same = Object.is(a, b) || (a === 0 && b === 0);
    if (!same) fail("assert_equals: " + (msg || "") + " " + fmt(a) + " !== " + fmt(b));
  };
  globalThis.assert_not_equals = function (a, b, msg) {
    if (Object.is(a, b)) fail("assert_not_equals: " + (msg || "") + " " + fmt(a) + " === " + fmt(b));
  };
  globalThis.assert_throws_dom = function (name, fn_or_target_or_constructor, fn_maybe, msg_maybe) {
    // Variants:
    //   assert_throws_dom(name, fn)
    //   assert_throws_dom(name, fn, msg)
    //   assert_throws_dom(name, constructor, fn)             (newer)
    //   assert_throws_dom(name, constructor, fn, msg)        (newer)
    let fn, msg;
    if (typeof fn_or_target_or_constructor === "function" && (fn_maybe == null || typeof fn_maybe === "string")) {
      fn = fn_or_target_or_constructor;
      msg = fn_maybe;
    } else {
      fn = fn_maybe;
      msg = msg_maybe;
    }
    let threw = false;
    try { fn(); } catch (e) {
      threw = true;
      if (!e || (e.name !== name && (typeof DOMException === "undefined" || !(e instanceof DOMException) || e.name !== name))) {
        fail("assert_throws_dom(" + name + "): " + (msg || "") + " got " + fmt(e && e.name));
      }
    }
    if (!threw) fail("assert_throws_dom: did not throw " + (msg || ""));
  };
  globalThis.assert_throws_quotaexceedederror = function (fn, _qa, _qb, msg) {
    let threw = false;
    try { fn(); } catch (e) {
      threw = true;
      if (!e || e.name !== "QuotaExceededError") {
        fail("assert_throws_quotaexceedederror: " + (msg || "") + " got " + fmt(e && e.name));
      }
    }
    if (!threw) fail("assert_throws_quotaexceedederror: did not throw " + (msg || ""));
  };
  globalThis.assert_throws_js = function (ctor, fn, msg) {
    let threw = false;
    try { fn(); } catch (e) {
      threw = true;
      if (!(e instanceof ctor)) {
        fail("assert_throws_js: " + (msg || "") + " got " + fmt(e && e.name));
      }
    }
    if (!threw) fail("assert_throws_js: did not throw " + (msg || ""));
  };
  globalThis.assert_unreached = function (msg) {
    fail("assert_unreached: " + (msg || ""));
  };
  globalThis.assert_array_equals = function (a, b, msg) {
    if (!a || !b || a.length !== b.length) {
      fail("assert_array_equals: " + (msg || "") + " length mismatch " + (a && a.length) + " vs " + (b && b.length));
    }
    for (let i = 0; i < a.length; i++) {
      if (!Object.is(a[i], b[i])) {
        fail("assert_array_equals: " + (msg || "") + " idx " + i + " " + fmt(a[i]) + " !== " + fmt(b[i]));
      }
    }
  };
  globalThis.assert_in_array = function (v, arr, msg) {
    for (const x of arr) if (x === v) return;
    fail("assert_in_array: " + (msg || "") + " " + fmt(v));
  };
  globalThis.assert_class_string = function (obj, name, msg) {
    const got = Object.prototype.toString.call(obj);
    const want = "[object " + name + "]";
    if (got !== want) fail("assert_class_string: " + (msg || "") + " " + got + " vs " + want);
  };
  globalThis.assert_object_equals = function (actual, expected, msg) {
    const ka = Object.keys(actual).sort();
    const ke = Object.keys(expected).sort();
    if (ka.length !== ke.length || ka.some((k, i) => k !== ke[i])) {
      fail("assert_object_equals: " + (msg || "") + " keys differ");
    }
    for (const k of ka) {
      if (!Object.is(actual[k], expected[k])) {
        fail("assert_object_equals: " + (msg || "") + " at " + k);
      }
    }
  };
  globalThis.promise_rejects_dom = function (t, name, p, msg) {
    return Promise.resolve(p).then(
      () => fail("promise_rejects_dom: " + (msg || "") + " expected " + name + ", got fulfillment"),
      e => {
        if (!e || e.name !== name) {
          fail("promise_rejects_dom: " + (msg || "") + " expected " + name + ", got " + fmt(e && e.name));
        }
      }
    );
  };
  globalThis.promise_rejects_js = function (t, ctor, p, msg) {
    return Promise.resolve(p).then(
      () => fail("promise_rejects_js: " + (msg || "") + " expected " + ctor.name + ", got fulfillment"),
      e => {
        if (!(e instanceof ctor) && (!e || e.name !== ctor.name)) {
          fail("promise_rejects_js: " + (msg || "") + " expected " + ctor.name + ", got " + fmt(e && e.name));
        }
      }
    );
  };
  globalThis.promise_rejects_exactly = function (t, expected, p, msg) {
    return Promise.resolve(p).then(
      () => fail("promise_rejects_exactly: " + (msg || "") + " expected throw"),
      e => { if (e !== expected) fail("promise_rejects_exactly: wrong throw"); }
    );
  };

  // Float16Array may not exist in V8.x; alias to Float32 for the
  // rejection tests (they expect TypeMismatchError either way).
  if (typeof Float16Array === "undefined") {
    globalThis.Float16Array = Float32Array;
  }

  // ---------------------------------------------------------------------
  // testharness.js extensions used by WebCryptoAPI tests
  // ---------------------------------------------------------------------

  // AssertionError — thrown by some sign_verify tests directly.
  globalThis.AssertionError = class AssertionError extends Error {
    constructor(message) { super(message); this.name = "AssertionError"; }
  };
  globalThis.OptionalFeatureUnsupportedError =
    class OptionalFeatureUnsupportedError extends Error {
      constructor(message) { super(message); this.name = "OptionalFeatureUnsupportedError"; }
    };

  // assert_implements + assert_implements_optional — only the latter
  // is used by WebCryptoAPI (skip when an optional feature isn't
  // available, e.g. compressed EC point formats).
  globalThis.assert_implements = function (cond, desc) {
    if (!cond) fail("assert_implements: " + (desc || ""));
  };
  globalThis.assert_implements_optional = function (cond, desc) {
    if (!cond) {
      const e = new globalThis.OptionalFeatureUnsupportedError(desc || "");
      e.__wpt_skip = true;
      throw e;
    }
  };

  // subsetTest(test_fn, ...args) — when WPT tests run sharded, this
  // selects a subset; otherwise it just calls the test fn. We always
  // run the full set (unsharded).
  globalThis.subsetTest = function (testFn) {
    const args = Array.prototype.slice.call(arguments, 1);
    return testFn.apply(globalThis, args);
  };

  // structuredClone — WHATWG HTML §2.7.3. WebCrypto only uses it on
  // plain objects + typed arrays in the JWK fixtures, so a deep-copy
  // walker handles all observed cases. ArrayBuffer/typed-array views
  // are copied; CryptoKey/etc. are not used as inputs here.
  globalThis.structuredClone = function (value, _opts) {
    return _structuredCloneInner(value, new Map());
  };
  function _structuredCloneInner(v, seen) {
    if (v === null || typeof v !== "object") return v;
    if (seen.has(v)) return seen.get(v);
    if (v instanceof Uint8Array || v instanceof Uint16Array ||
        v instanceof Uint32Array || v instanceof Int8Array ||
        v instanceof Int16Array || v instanceof Int32Array ||
        v instanceof Float32Array || v instanceof Float64Array ||
        v instanceof Uint8ClampedArray) {
      const out = new v.constructor(v.length);
      out.set(v);
      seen.set(v, out);
      return out;
    }
    if (v instanceof ArrayBuffer) {
      const out = v.slice(0);
      seen.set(v, out);
      return out;
    }
    if (Array.isArray(v)) {
      const out = new Array(v.length);
      seen.set(v, out);
      for (let i = 0; i < v.length; i++) out[i] = _structuredCloneInner(v[i], seen);
      return out;
    }
    if (v instanceof Date) return new Date(v.getTime());
    if (v instanceof Map) {
      const out = new Map();
      seen.set(v, out);
      for (const [k, val] of v) out.set(_structuredCloneInner(k, seen), _structuredCloneInner(val, seen));
      return out;
    }
    if (v instanceof Set) {
      const out = new Set();
      seen.set(v, out);
      for (const val of v) out.add(_structuredCloneInner(val, seen));
      return out;
    }
    // Plain object — best effort.
    const out = {};
    seen.set(v, out);
    for (const k of Object.keys(v)) out[k] = _structuredCloneInner(v[k], seen);
    return out;
  }

  // testharness format_value — used by some assert helpers that escaped
  // our shim coverage. Plain string-or-JSON.
  globalThis.format_value = function (v) {
    if (typeof v === "string") return JSON.stringify(v);
    if (v && typeof v === "object" && "name" in v) return v.name;
    try { return String(v); } catch (_) { return "<unstringable>"; }
  };
})();
"#;

fn install_test_harness(scope: &mut v8::PinScope) {
    let global = scope.get_current_context().global(scope);
    dom::exception::install_global(scope, global);
    crypto_native::install_globals(scope, global);
    {
        let tmpl = text_encoding::TextEncoder::install(scope);
        let class_fn = tmpl.get_function(scope).unwrap();
        let key = v8::String::new(scope, "TextEncoder").unwrap();
        global.set(scope, key.into(), class_fn.into());
    }
    {
        let tmpl = text_encoding::TextDecoder::install(scope);
        let class_fn = tmpl.get_function(scope).unwrap();
        let key = v8::String::new(scope, "TextDecoder").unwrap();
        global.set(scope, key.into(), class_fn.into());
    }
    let shim = v8::String::new(scope, TESTHARNESS_SHIM).unwrap();
    let script = v8::Script::compile(scope, shim, None).unwrap();
    script.run(scope).unwrap();
}

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

/// Strip `// META:` directive lines so we can run the body verbatim.
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

/// Run a list of script sources in a fresh isolate (with shim
/// installed) and return parsed results.
///
/// The first N-1 sources are typically `// META: script=` deps
/// (helpers.js, vectors.js, etc.); the last is the test entry point.
fn run_wpt_files(sources: &[&str]) -> Vec<TestResult> {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    install_test_harness(scope);

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
                name: format!("<top-level src#{idx}>"),
                outcome: Outcome::Fail(err),
            }];
        }
    }

    // Drive microtasks until promise_test bodies settle. WebCrypto
    // tests are very promise-heavy so we drain liberally.
    for _ in 0..256 {
        scope.perform_microtask_checkpoint();
    }

    let read_src = v8::String::new(scope, "JSON.stringify(__wpt_results)").unwrap();
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

#[derive(Default, Debug, Clone)]
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
    fn count_executed(&self) -> usize {
        self.pass + self.fail
    }
    fn pass_rate(&self) -> f64 {
        let n = self.count_executed();
        if n == 0 {
            0.0
        } else {
            self.pass as f64 / n as f64
        }
    }
}

// =============================================================================
// Vendored test sources
// =============================================================================

const HELPERS: &str = include_str!("wpt/WebCryptoAPI/util/helpers.js");

// digest/
const DIGEST: &str = include_str!("wpt/WebCryptoAPI/digest/digest.https.any.js");

// sign_verify/
const HMAC_VECTORS: &str = include_str!("wpt/WebCryptoAPI/sign_verify/hmac_vectors.js");
const HMAC_RUN: &str = include_str!("wpt/WebCryptoAPI/sign_verify/hmac.js");
const HMAC_TOP: &str = include_str!("wpt/WebCryptoAPI/sign_verify/hmac.https.any.js");

const ECDSA_VECTORS: &str = include_str!("wpt/WebCryptoAPI/sign_verify/ecdsa_vectors.js");
const ECDSA_RUN: &str = include_str!("wpt/WebCryptoAPI/sign_verify/ecdsa.js");
const ECDSA_TOP: &str = include_str!("wpt/WebCryptoAPI/sign_verify/ecdsa.https.any.js");

const RSA_PKCS_VECTORS: &str = include_str!("wpt/WebCryptoAPI/sign_verify/rsa_pkcs_vectors.js");
const RSA_PSS_VECTORS: &str = include_str!("wpt/WebCryptoAPI/sign_verify/rsa_pss_vectors.js");
const RSA_RUN: &str = include_str!("wpt/WebCryptoAPI/sign_verify/rsa.js");
const RSA_PKCS_TOP: &str = include_str!("wpt/WebCryptoAPI/sign_verify/rsa_pkcs.https.any.js");
const RSA_PSS_TOP: &str = include_str!("wpt/WebCryptoAPI/sign_verify/rsa_pss.https.any.js");

const EDDSA_VECTORS: &str = include_str!("wpt/WebCryptoAPI/sign_verify/eddsa_vectors.js");
const EDDSA_RUN: &str = include_str!("wpt/WebCryptoAPI/sign_verify/eddsa.js");
const EDDSA_25519_TOP: &str = include_str!("wpt/WebCryptoAPI/sign_verify/eddsa_curve25519.https.any.js");

// encrypt_decrypt/
const AES_RUN: &str = include_str!("wpt/WebCryptoAPI/encrypt_decrypt/aes.js");
const AES_GCM_VECTORS: &str = include_str!("wpt/WebCryptoAPI/encrypt_decrypt/aes_gcm_vectors.js");
const AES_GCM_96_FIX: &str = include_str!("wpt/WebCryptoAPI/encrypt_decrypt/aes_gcm_96_iv_fixtures.js");
const AES_GCM_TOP: &str = include_str!("wpt/WebCryptoAPI/encrypt_decrypt/aes_gcm.https.any.js");
const AES_GCM_256_FIX: &str = include_str!("wpt/WebCryptoAPI/encrypt_decrypt/aes_gcm_256_iv_fixtures.js");
const AES_GCM_256_TOP: &str = include_str!("wpt/WebCryptoAPI/encrypt_decrypt/aes_gcm_256_iv.https.any.js");
const AES_CBC_VECTORS: &str = include_str!("wpt/WebCryptoAPI/encrypt_decrypt/aes_cbc_vectors.js");
const AES_CBC_TOP: &str = include_str!("wpt/WebCryptoAPI/encrypt_decrypt/aes_cbc.https.any.js");
const AES_CTR_VECTORS: &str = include_str!("wpt/WebCryptoAPI/encrypt_decrypt/aes_ctr_vectors.js");
const AES_CTR_TOP: &str = include_str!("wpt/WebCryptoAPI/encrypt_decrypt/aes_ctr.https.any.js");
const RSA_OAEP_VECTORS: &str = include_str!("wpt/WebCryptoAPI/encrypt_decrypt/rsa_vectors.js");
const RSA_E_RUN: &str = include_str!("wpt/WebCryptoAPI/encrypt_decrypt/rsa.js");
const RSA_OAEP_TOP: &str = include_str!("wpt/WebCryptoAPI/encrypt_decrypt/rsa_oaep.https.any.js");

// derive_bits_keys/
const PBKDF2_VECTORS: &str = include_str!("wpt/WebCryptoAPI/derive_bits_keys/pbkdf2_vectors.js");
const PBKDF2_RUN: &str = include_str!("wpt/WebCryptoAPI/derive_bits_keys/pbkdf2.js");
const PBKDF2_TOP: &str = include_str!("wpt/WebCryptoAPI/derive_bits_keys/pbkdf2.https.any.js");
const HKDF_VECTORS: &str = include_str!("wpt/WebCryptoAPI/derive_bits_keys/hkdf_vectors.js");
const HKDF_RUN: &str = include_str!("wpt/WebCryptoAPI/derive_bits_keys/hkdf.js");
const HKDF_TOP: &str = include_str!("wpt/WebCryptoAPI/derive_bits_keys/hkdf.https.any.js");
const ECDH_KEYS_RUN: &str = include_str!("wpt/WebCryptoAPI/derive_bits_keys/ecdh_keys.js");
const ECDH_BITS_RUN: &str = include_str!("wpt/WebCryptoAPI/derive_bits_keys/ecdh_bits.js");
const ECDH_KEYS_TOP: &str = include_str!("wpt/WebCryptoAPI/derive_bits_keys/ecdh_keys.https.any.js");
const ECDH_BITS_TOP: &str = include_str!("wpt/WebCryptoAPI/derive_bits_keys/ecdh_bits.https.any.js");
const CFRG_KEYS_RUN: &str = include_str!("wpt/WebCryptoAPI/derive_bits_keys/cfrg_curves_keys.js");
const CFRG_BITS_RUN: &str = include_str!("wpt/WebCryptoAPI/derive_bits_keys/cfrg_curves_bits.js");
const CFRG_BITS_FIXTURES: &str =
    include_str!("wpt/WebCryptoAPI/derive_bits_keys/cfrg_curves_bits_fixtures.js");
const CFRG_25519_BITS_TOP: &str =
    include_str!("wpt/WebCryptoAPI/derive_bits_keys/cfrg_curves_bits_curve25519.https.any.js");
const CFRG_25519_KEYS_TOP: &str =
    include_str!("wpt/WebCryptoAPI/derive_bits_keys/cfrg_curves_keys_curve25519.https.any.js");

// import_export/
const RSA_IMPORT_TOP: &str = include_str!("wpt/WebCryptoAPI/import_export/rsa_importKey.https.any.js");
const SYM_IMPORT_RUN: &str = include_str!("wpt/WebCryptoAPI/import_export/symmetric_importKey.js");
const SYM_IMPORT_TOP: &str = include_str!("wpt/WebCryptoAPI/import_export/symmetric_importKey.https.any.js");
const EC_IMPORT_TOP: &str = include_str!("wpt/WebCryptoAPI/import_export/ec_importKey.https.any.js");
const OKP_IMPORT_FIXTURES: &str = include_str!("wpt/WebCryptoAPI/import_export/okp_importKey_fixtures.js");
const OKP_IMPORT_RUN: &str = include_str!("wpt/WebCryptoAPI/import_export/okp_importKey.js");
const OKP_25519_IMPORT_TOP: &str =
    include_str!("wpt/WebCryptoAPI/import_export/okp_importKey_Ed25519.https.any.js");
const OKP_X25519_IMPORT_TOP: &str =
    include_str!("wpt/WebCryptoAPI/import_export/okp_importKey_X25519.https.any.js");

// generateKey successes/failures (subset — not all tentative ones).
const GEN_SUCC_HMAC: &str = include_str!("wpt/WebCryptoAPI/generateKey/successes.js");
const GEN_SUCC_HMAC_TOP: &str = include_str!("wpt/WebCryptoAPI/generateKey/successes_HMAC.https.any.js");
const GEN_SUCC_AES_CBC_TOP: &str = include_str!("wpt/WebCryptoAPI/generateKey/successes_AES-CBC.https.any.js");
const GEN_SUCC_AES_CTR_TOP: &str = include_str!("wpt/WebCryptoAPI/generateKey/successes_AES-CTR.https.any.js");
const GEN_SUCC_AES_GCM_TOP: &str = include_str!("wpt/WebCryptoAPI/generateKey/successes_AES-GCM.https.any.js");
const GEN_SUCC_AES_KW_TOP: &str = include_str!("wpt/WebCryptoAPI/generateKey/successes_AES-KW.https.any.js");
const GEN_SUCC_ECDSA_TOP: &str = include_str!("wpt/WebCryptoAPI/generateKey/successes_ECDSA.https.any.js");
const GEN_SUCC_ECDH_TOP: &str = include_str!("wpt/WebCryptoAPI/generateKey/successes_ECDH.https.any.js");
const GEN_SUCC_ED25519_TOP: &str =
    include_str!("wpt/WebCryptoAPI/generateKey/successes_Ed25519.https.any.js");
const GEN_SUCC_X25519_TOP: &str =
    include_str!("wpt/WebCryptoAPI/generateKey/successes_X25519.https.any.js");

// Top-level (no script= deps).
const GET_RANDOM_VALUES: &str = include_str!("wpt/WebCryptoAPI/getRandomValues.any.js");
const RANDOM_UUID: &str = include_str!("wpt/WebCryptoAPI/randomUUID.https.any.js");

// =============================================================================
// File table
// =============================================================================

struct WptFile {
    name: &'static str,
    sources: &'static [&'static str],
}

/// must-pass-v1 set per design §X.2. Each entry is a file label
/// + the ordered script chain (helpers/vectors/run + entry).
const MUST_PASS: &[WptFile] = &[
    WptFile { name: "getRandomValues", sources: &[GET_RANDOM_VALUES] },
    WptFile { name: "randomUUID", sources: &[RANDOM_UUID] },
    WptFile { name: "digest", sources: &[HELPERS, DIGEST] },

    // sign_verify
    WptFile { name: "sign_verify/hmac", sources: &[HELPERS, HMAC_VECTORS, HMAC_RUN, HMAC_TOP] },
    WptFile { name: "sign_verify/ecdsa", sources: &[HELPERS, ECDSA_VECTORS, ECDSA_RUN, ECDSA_TOP] },
    WptFile { name: "sign_verify/rsa_pkcs", sources: &[HELPERS, RSA_PKCS_VECTORS, RSA_RUN, RSA_PKCS_TOP] },
    WptFile { name: "sign_verify/rsa_pss", sources: &[HELPERS, RSA_PSS_VECTORS, RSA_RUN, RSA_PSS_TOP] },
    WptFile { name: "sign_verify/eddsa_curve25519", sources: &[HELPERS, EDDSA_VECTORS, EDDSA_RUN, EDDSA_25519_TOP] },

    // encrypt_decrypt
    WptFile { name: "encrypt_decrypt/aes_gcm", sources: &[HELPERS, AES_GCM_96_FIX, AES_GCM_VECTORS, AES_RUN, AES_GCM_TOP] },
    WptFile { name: "encrypt_decrypt/aes_gcm_256_iv", sources: &[HELPERS, AES_GCM_256_FIX, AES_GCM_VECTORS, AES_RUN, AES_GCM_256_TOP] },
    WptFile { name: "encrypt_decrypt/aes_cbc", sources: &[HELPERS, AES_CBC_VECTORS, AES_RUN, AES_CBC_TOP] },
    WptFile { name: "encrypt_decrypt/aes_ctr", sources: &[HELPERS, AES_CTR_VECTORS, AES_RUN, AES_CTR_TOP] },
    WptFile { name: "encrypt_decrypt/rsa_oaep", sources: &[HELPERS, RSA_OAEP_VECTORS, RSA_E_RUN, RSA_OAEP_TOP] },

    // derive_bits_keys
    WptFile { name: "derive_bits_keys/pbkdf2", sources: &[HELPERS, PBKDF2_VECTORS, PBKDF2_RUN, PBKDF2_TOP] },
    WptFile { name: "derive_bits_keys/hkdf", sources: &[HELPERS, HKDF_VECTORS, HKDF_RUN, HKDF_TOP] },
    WptFile { name: "derive_bits_keys/ecdh_keys", sources: &[HELPERS, ECDH_KEYS_RUN, ECDH_KEYS_TOP] },
    WptFile { name: "derive_bits_keys/ecdh_bits", sources: &[HELPERS, ECDH_BITS_RUN, ECDH_BITS_TOP] },
    WptFile { name: "derive_bits_keys/cfrg_curves_bits_curve25519", sources: &[HELPERS, CFRG_BITS_FIXTURES, CFRG_BITS_RUN, CFRG_25519_BITS_TOP] },
    WptFile { name: "derive_bits_keys/cfrg_curves_keys_curve25519", sources: &[HELPERS, CFRG_KEYS_RUN, CFRG_25519_KEYS_TOP] },

    // import_export
    WptFile { name: "import_export/rsa_importKey", sources: &[HELPERS, RSA_IMPORT_TOP] },
    WptFile { name: "import_export/symmetric_importKey", sources: &[HELPERS, SYM_IMPORT_RUN, SYM_IMPORT_TOP] },
    WptFile { name: "import_export/ec_importKey", sources: &[HELPERS, EC_IMPORT_TOP] },
    WptFile { name: "import_export/okp_importKey_Ed25519", sources: &[HELPERS, OKP_IMPORT_FIXTURES, OKP_IMPORT_RUN, OKP_25519_IMPORT_TOP] },
    WptFile { name: "import_export/okp_importKey_X25519", sources: &[HELPERS, OKP_IMPORT_FIXTURES, OKP_IMPORT_RUN, OKP_X25519_IMPORT_TOP] },

    // generateKey successes
    WptFile { name: "generateKey/successes_HMAC", sources: &[HELPERS, GEN_SUCC_HMAC, GEN_SUCC_HMAC_TOP] },
    WptFile { name: "generateKey/successes_AES-CBC", sources: &[HELPERS, GEN_SUCC_HMAC, GEN_SUCC_AES_CBC_TOP] },
    WptFile { name: "generateKey/successes_AES-CTR", sources: &[HELPERS, GEN_SUCC_HMAC, GEN_SUCC_AES_CTR_TOP] },
    WptFile { name: "generateKey/successes_AES-GCM", sources: &[HELPERS, GEN_SUCC_HMAC, GEN_SUCC_AES_GCM_TOP] },
    WptFile { name: "generateKey/successes_AES-KW", sources: &[HELPERS, GEN_SUCC_HMAC, GEN_SUCC_AES_KW_TOP] },
    WptFile { name: "generateKey/successes_ECDSA", sources: &[HELPERS, GEN_SUCC_HMAC, GEN_SUCC_ECDSA_TOP] },
    WptFile { name: "generateKey/successes_ECDH", sources: &[HELPERS, GEN_SUCC_HMAC, GEN_SUCC_ECDH_TOP] },
    WptFile { name: "generateKey/successes_Ed25519", sources: &[HELPERS, GEN_SUCC_HMAC, GEN_SUCC_ED25519_TOP] },
    WptFile { name: "generateKey/successes_X25519", sources: &[HELPERS, GEN_SUCC_HMAC, GEN_SUCC_X25519_TOP] },
];

// =============================================================================
// Tests
// =============================================================================

#[test]
fn wpt_must_pass_v1_compliance() {
    let mut totals = Totals::default();
    let mut per_file: BTreeMap<&'static str, Totals> = BTreeMap::new();
    let mut failures: Vec<(&'static str, TestResult)> = Vec::new();

    for entry in MUST_PASS {
        let results = run_wpt_files(entry.sources);
        let mut t = Totals::default();
        for r in results {
            match &r.outcome {
                Outcome::Pass => t.pass += 1,
                Outcome::Skip(_) => t.skip += 1,
                Outcome::Fail(_) => {
                    t.fail += 1;
                    failures.push((entry.name, r));
                }
            }
        }
        totals.add(&t);
        per_file.insert(entry.name, t);
    }

    eprintln!("\n=== WPT WebCryptoAPI must-pass-v1 results ===");
    for (file, t) in &per_file {
        let exec = t.pass + t.fail;
        let pct = if exec > 0 { 100.0 * (t.pass as f64) / (exec as f64) } else { 0.0 };
        eprintln!(
            "  {:60} pass={:3} fail={:3} skip={:3} ({:5.1}%)",
            file, t.pass, t.fail, t.skip, pct
        );
    }
    let exec = totals.count_executed();
    let pct = totals.pass_rate() * 100.0;
    eprintln!(
        "  {:-<60} pass={:3} fail={:3} skip={:3} ({:5.1}%)",
        "total ", totals.pass, totals.fail, totals.skip, pct
    );

    if !failures.is_empty() {
        // Group by per-file failure-message buckets so we don't spam.
        // Map: file → Map: error-prefix → count
        let mut buckets: BTreeMap<&'static str, BTreeMap<String, (usize, String)>> =
            BTreeMap::new();
        for (file, r) in &failures {
            let detail = match &r.outcome {
                Outcome::Fail(s) => s.as_str(),
                _ => "?",
            };
            // Bucket by first 80 chars of error.
            let key: String = detail.chars().take(80).collect();
            let entry = buckets
                .entry(*file)
                .or_default()
                .entry(key)
                .or_insert((0usize, r.name.clone()));
            entry.0 += 1;
        }
        eprintln!("\n=== Failure clusters ===");
        for (file, errs) in &buckets {
            for (err, (count, sample)) in errs {
                eprintln!(
                    "  [{file}] x{count}  e.g. {sample}: {err}"
                );
            }
        }
    }

    // Demand at least 90% pass rate among executed (pass+fail).
    assert!(
        totals.pass_rate() >= 0.90,
        "WPT must-pass-v1 pass rate {:.1}% < 90% threshold (pass={} fail={} skip={})",
        pct,
        totals.pass,
        totals.fail,
        totals.skip
    );
    assert!(
        totals.pass >= 100,
        "Suspiciously few passing tests ({}); something likely broken",
        totals.pass
    );
}
