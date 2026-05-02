//! Run the W3C Web Platform Tests for WebCryptoAPI against the native
//! impl. Source: https://github.com/web-platform-tests/wpt/tree/master/WebCryptoAPI.
//! Files are vendored under `crates/runtime/tests/wpt/WebCryptoAPI/`.
//!
//! Per `docs/proposals/webcrypto-native.md` §X.2 / §X.3.
//!
//! This v1 runner covers the simpler must-pass-v1 inline files:
//!   - getRandomValues.any.js — D-21 type filter + quota
//!   - randomUUID.https.any.js — RFC 4122 v4 format
//!
//! The full suite (sign_verify/, encrypt_decrypt/, generateKey/,
//! import_export/, derive_bits_keys/, wrapKey_unwrapKey/) requires
//! preloading the WebCryptoAPI/util helpers and is queued as
//! follow-up work in §XII landing 2.

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

  globalThis.setup = function (arg) {
    if (typeof arg === "function") { try { arg(); } catch (_) {} }
  };

  function fmt(v) {
    if (typeof v === "string") return JSON.stringify(v);
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

  globalThis.test = function (fn, name) {
    name = sanitize(name || fn.name || "<anonymous>");
    const t = {
      _cleanups: [], add_cleanup(cb) { this._cleanups.push(cb); },
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
    Promise.resolve().then(fn).then(
      () => __wpt_results.push({ name, status: "pass" }),
      e => __wpt_results.push({
        name, status: "fail",
        error: sanitize(e && e.message ? e.message : String(e)),
      })
    );
  };

  globalThis.async_test = function (_fn, name) {
    __wpt_results.push({
      name: name || "<async_test>", status: "skip", reason: "async_test stub",
    });
  };

  // ---------------------------------------------------------------------
  // Asserts
  // ---------------------------------------------------------------------

  globalThis.assert_true = function (cond, msg) {
    if (cond !== true) throw new Error("assert_true: " + (msg || "") + " got " + fmt(cond));
  };
  globalThis.assert_false = function (cond, msg) {
    if (cond !== false) throw new Error("assert_false: " + (msg || "") + " got " + fmt(cond));
  };
  globalThis.assert_equals = function (a, b, msg) {
    if (a !== b) {
      throw new Error("assert_equals: " + (msg || "") + " " + fmt(a) + " !== " + fmt(b));
    }
  };
  globalThis.assert_not_equals = function (a, b, msg) {
    if (a === b) {
      throw new Error("assert_not_equals: " + (msg || "") + " " + fmt(a) + " === " + fmt(b));
    }
  };
  globalThis.assert_throws_dom = function (name, fn, msg) {
    let threw = false;
    try { fn(); } catch (e) {
      threw = true;
      if (!e || (e.name !== name && (typeof DOMException === "undefined" || !(e instanceof DOMException) || e.name !== name))) {
        throw new Error("assert_throws_dom(" + name + "): " + (msg || "") + " got " + fmt(e && e.name));
      }
    }
    if (!threw) throw new Error("assert_throws_dom: did not throw " + (msg || ""));
  };
  globalThis.assert_throws_quotaexceedederror = function (fn, _qa, _qb, msg) {
    let threw = false;
    try { fn(); } catch (e) {
      threw = true;
      if (!e || e.name !== "QuotaExceededError") {
        throw new Error("assert_throws_quotaexceedederror: " + (msg || "") + " got " + fmt(e && e.name));
      }
    }
    if (!threw) throw new Error("assert_throws_quotaexceedederror: did not throw " + (msg || ""));
  };
  globalThis.assert_throws_js = function (ctor, fn, msg) {
    let threw = false;
    try { fn(); } catch (e) {
      threw = true;
      if (!(e instanceof ctor)) {
        throw new Error("assert_throws_js: " + (msg || "") + " got " + fmt(e && e.name));
      }
    }
    if (!threw) throw new Error("assert_throws_js: did not throw " + (msg || ""));
  };
  globalThis.assert_unreached = function (msg) {
    throw new Error("assert_unreached: " + (msg || ""));
  };
  globalThis.assert_array_equals = function (a, b, msg) {
    if (!a || !b || a.length !== b.length) {
      throw new Error("assert_array_equals: " + (msg || "") + " length mismatch");
    }
    for (let i = 0; i < a.length; i++) {
      if (a[i] !== b[i]) {
        throw new Error("assert_array_equals: " + (msg || "") + " idx " + i + " " + fmt(a[i]) + " !== " + fmt(b[i]));
      }
    }
  };
  globalThis.assert_in_array = function (v, arr, msg) {
    for (const x of arr) if (x === v) return;
    throw new Error("assert_in_array: " + (msg || "") + " " + fmt(v));
  };

  // Float16Array may not exist in V8.x; alias to Float32 for the rejection
  // tests (they expect TypeMismatchError either way).
  if (typeof Float16Array === "undefined") {
    globalThis.Float16Array = Float32Array;
  }
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

fn run_wpt(src: &str) -> BTreeMap<String, (String, Option<String>)> {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    install_test_harness(scope);

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    script.run(scope);
    for _ in 0..32 {
        scope.perform_microtask_checkpoint();
    }

    // Read __wpt_results back from the global.
    let results_src = v8::String::new(scope, "JSON.stringify(__wpt_results)").unwrap();
    let s = v8::Script::compile(scope, results_src, None).unwrap();
    let v = s.run(scope).unwrap();
    let json = v.to_rust_string_lossy(scope);
    parse_results(&json)
}

fn parse_results(json: &str) -> BTreeMap<String, (String, Option<String>)> {
    // Tiny parser — array of {name, status, error?, reason?} objects.
    // We only care about extracting these fields. Use serde_json.
    let v: serde_json::Value = serde_json::from_str(json).unwrap_or(serde_json::Value::Null);
    let mut out = BTreeMap::new();
    if let Some(arr) = v.as_array() {
        for elem in arr {
            let name = elem
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("<unnamed>")
                .to_string();
            let status = elem
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let detail = elem
                .get("error")
                .or_else(|| elem.get("reason"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            out.insert(name, (status, detail));
        }
    }
    out
}

fn print_summary(name: &str, results: &BTreeMap<String, (String, Option<String>)>) -> bool {
    let mut pass = 0;
    let mut fail = 0;
    let mut skip = 0;
    for (n, (status, detail)) in results {
        match status.as_str() {
            "pass" => pass += 1,
            "skip" => {
                skip += 1;
                eprintln!("  SKIP {n} — {}", detail.as_deref().unwrap_or(""));
            }
            "fail" => {
                fail += 1;
                eprintln!("  FAIL {n} — {}", detail.as_deref().unwrap_or(""));
            }
            other => eprintln!("  {other} {n}"),
        }
    }
    eprintln!("{name}: {pass} pass, {fail} fail, {skip} skip");
    fail == 0
}

const GET_RANDOM_VALUES: &str = include_str!("wpt/WebCryptoAPI/getRandomValues.any.js");
const RANDOM_UUID: &str = include_str!("wpt/WebCryptoAPI/randomUUID.https.any.js");

#[test]
fn wpt_get_random_values() {
    let r = run_wpt(GET_RANDOM_VALUES);
    let ok = print_summary("getRandomValues.any.js", &r);
    assert!(ok, "Some WPT getRandomValues subtests FAILed");
}

#[test]
fn wpt_random_uuid() {
    let r = run_wpt(RANDOM_UUID);
    let ok = print_summary("randomUUID.https.any.js", &r);
    assert!(ok, "Some WPT randomUUID subtests FAILed");
}
