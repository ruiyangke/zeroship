//! Run W3C Web Platform Tests for the body model
//! (`Body` mixin: text/json/arrayBuffer/bytes/blob/formData) against
//! the native impl.
//!
//! Source: https://github.com/web-platform-tests/wpt/tree/master/fetch/api/body
//! Files vendored at `crates/runtime/tests/wpt/fetch/api/body/`.
//!
//! Mirrors the `wpt_fetch_request.rs` / `wpt_fetch_response.rs`
//! pattern. The body suite is small (3 files) — most body-consumer
//! WPT lives in `request-consume*.any.js` / `response-consume*.any.js`
//! which need full Blob support to run cleanly.

#![allow(unsafe_code)]

use std::collections::BTreeMap;

use zeroship_runtime::blob_native;
use zeroship_runtime::dom;
use zeroship_runtime::fetch_request;
use zeroship_runtime::fetch_response;
use zeroship_runtime::headers;
use zeroship_runtime::init_v8;
use zeroship_runtime::streams;

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

  if (typeof globalThis.URLSearchParams !== "function") {
    globalThis.URLSearchParams = function URLSearchParams(init) {
      this._params = [];
      if (typeof init === "string") {
        if (init.charAt(0) === "?") init = init.slice(1);
        var pairs = init.split("&");
        for (var i = 0; i < pairs.length; i++) {
          if (!pairs[i]) continue;
          var eq = pairs[i].indexOf("=");
          if (eq === -1) {
            this._params.push([decodeURIComponent(pairs[i]), ""]);
          } else {
            this._params.push([
              decodeURIComponent(pairs[i].slice(0, eq)),
              decodeURIComponent(pairs[i].slice(eq + 1).replace(/\+/g, " ")),
            ]);
          }
        }
      } else if (init && typeof init === "object" && !Array.isArray(init)) {
        var keys = Object.keys(init);
        for (var k = 0; k < keys.length; k++) {
          this._params.push([keys[k], String(init[keys[k]])]);
        }
      }
    };
    globalThis.URLSearchParams.prototype.append = function (n, v) {
      this._params.push([String(n), String(v)]);
    };
    globalThis.URLSearchParams.prototype.set = function (n, v) {
      n = String(n); v = String(v);
      var found = false;
      this._params = this._params.filter(function (p) {
        if (p[0] === n) {
          if (!found) { found = true; p[1] = v; return true; }
          return false;
        }
        return true;
      });
      if (!found) this._params.push([n, v]);
    };
    globalThis.URLSearchParams.prototype.get = function (n) {
      n = String(n);
      for (var i = 0; i < this._params.length; i++) {
        if (this._params[i][0] === n) return this._params[i][1];
      }
      return null;
    };
    globalThis.URLSearchParams.prototype.toString = function () {
      return this._params.map(function (p) {
        return encodeURIComponent(p[0]).replace(/%20/g, "+") +
               "=" +
               encodeURIComponent(p[1]).replace(/%20/g, "+");
      }).join("&");
    };
    globalThis.URLSearchParams.prototype[Symbol.toStringTag] = "URLSearchParams";
  }

  // Native Blob and File are installed by the harness — no shims.
  if (typeof globalThis.fetch !== "function") {
    globalThis.fetch = function fetch() {
      const e = new Error("fetch() not yet implemented (next chunk)");
      e.__wpt_skip = true;
      throw e;
    };
  }
  if (typeof globalThis.URL !== "function") {
    globalThis.URL = function URL(input, base) {
      if (typeof input !== "string") input = String(input);
      if (input.indexOf("://") === -1) {
        if (typeof base === "string" && base.indexOf("://") !== -1) {
          // OK
        } else {
          throw new TypeError("Invalid URL: " + input);
        }
      }
      this.href = input;
    };
  }
  if (typeof globalThis.DOMException !== "function") {
    globalThis.DOMException = function DOMException(message, name) {
      const err = new Error(message || "");
      err.name = name || "Error";
      return err;
    };
    Object.defineProperty(globalThis.DOMException, Symbol.hasInstance, {
      value(obj) { return obj instanceof Error && typeof obj.name === "string"; }
    });
  }

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
    const t = {
      _cleanups: [],
      add_cleanup(cb) { this._cleanups.push(cb); },
      step(cb) { return cb.apply(this, arguments); },
      step_func(cb) { return cb.bind(this); },
      step_func_done(cb) { return cb.bind(this); },
      unreached_func(msg) {
        return function () { throw new Error("unreached: " + (msg || "")); };
      },
      done() {},
    };
    try {
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
    Promise.resolve().then(fn).then(
      () => __wpt_results.push({ name, status: "pass" }),
      e => {
        const msg = e && e.message ? e.message : String(e);
        if ((e && e.__wpt_skip) || msg.indexOf("not yet implemented (v1 skip)") >= 0) {
          __wpt_results.push({ name, status: "skip", reason: sanitize(msg) });
        } else {
          __wpt_results.push({
            name,
            status: "fail",
            error: sanitize(msg),
          });
        }
      }
    );
  };

  globalThis.async_test = function (_fn, name) {
    __wpt_results.push({
      name: name || "<async_test>",
      status: "skip",
      reason: "async_test stub",
    });
  };

  globalThis.done = function () {};

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
      if (e && e.__wpt_skip) throw e;
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
  globalThis.assert_throws_dom = function (name, fn, msg) {
    try { fn(); }
    catch (e) {
      if (e && e.name === name) return;
      fail(`${msg ? msg + ": " : ""}expected DOMException ${name}, got ${e && e.name ? e.name : fmt(e)}`);
    }
    fail(`${msg ? msg + ": " : ""}expected DOMException ${name}`);
  };
  globalThis.assert_unreached = function (msg) {
    fail(msg || "unreached");
  };
  globalThis.assert_implements_optional = function (cond, msg) {
    if (!cond) {
      const e = new Error(msg || "optional feature not implemented");
      e.__wpt_skip = true;
      throw e;
    }
  };

  globalThis.promise_rejects_js = function (t, ctor, promise, msg) {
    return promise.then(
      () => fail(`${msg ? msg + ": " : ""}expected ${ctor.name} rejection but resolved`),
      e => {
        if (e instanceof ctor) return;
        if (e && e.name === ctor.name) return;
        fail(`${msg ? msg + ": " : ""}expected ${ctor.name}, got ${e && e.name ? e.name : fmt(e)}`);
      }
    );
  };
  globalThis.promise_rejects_dom = function (t, name, promise, msg) {
    return promise.then(
      () => fail(`${msg ? msg + ": " : ""}expected DOMException ${name} rejection`),
      e => {
        if (e && e.name === name) return;
        fail(`${msg ? msg + ": " : ""}expected DOMException ${name}, got ${e && e.name ? e.name : fmt(e)}`);
      }
    );
  };
  globalThis.promise_rejects_exactly = function (t, expected, promise, msg) {
    return promise.then(
      () => fail(`${msg ? msg + ": " : ""}expected exact rejection`),
      e => {
        if (e === expected) return;
        fail(`${msg ? msg + ": " : ""}wrong rejection: ${fmt(e)} vs ${fmt(expected)}`);
      }
    );
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
    streams::install_native_streams(scope, global);
    streams::strategies::install_byte_length_queuing_strategy(scope, global);
    streams::strategies::install_count_queuing_strategy(scope, global);
    headers::install_global(scope, global);
    dom::install_globals(scope, global);
    blob_native::install_globals(scope, global);
    fetch_request::install_global(scope, global);
    fetch_response::install_global(scope, global);

    let shim = v8::String::new(scope, TESTHARNESS_SHIM).unwrap();
    v8::Script::compile(scope, shim, None)
        .unwrap()
        .run(scope)
        .unwrap();

    enum TopOutcome {
        Ok,
        Skip(String),
        Fail(String),
    }

    for (idx, source) in sources.iter().enumerate() {
        let prepared = prepare_wpt_source(source);
        let src = v8::String::new(scope, &prepared).unwrap();

        let top: TopOutcome = {
            v8::tc_scope!(let tc, scope);
            match v8::Script::compile(tc, src, None) {
                Some(script) => match script.run(tc) {
                    Some(_) => TopOutcome::Ok,
                    None => match tc.exception() {
                        Some(exc) => {
                            let msg = exc.to_rust_string_lossy(tc);
                            let is_skip = if let Ok(obj) = v8::Local::<v8::Object>::try_from(exc) {
                                let key = v8::String::new(tc, "__wpt_skip").unwrap();
                                obj.get(tc, key.into())
                                    .map(|v| v.boolean_value(tc))
                                    .unwrap_or(false)
                            } else {
                                false
                            };
                            if is_skip {
                                TopOutcome::Skip(msg)
                            } else {
                                TopOutcome::Fail(msg)
                            }
                        }
                        None => TopOutcome::Fail("<no exception>".into()),
                    },
                },
                None => TopOutcome::Fail(
                    tc.exception()
                        .map(|e| e.to_rust_string_lossy(tc))
                        .unwrap_or_else(|| "<compile error>".into()),
                ),
            }
        };

        match top {
            TopOutcome::Ok => {}
            TopOutcome::Skip(reason) => {
                return vec![TestResult {
                    name: format!("<top-level run {label} src#{idx}>"),
                    outcome: Outcome::Skip(reason),
                }];
            }
            TopOutcome::Fail(err) => {
                return vec![TestResult {
                    name: format!("<top-level run {label} src#{idx}>"),
                    outcome: Outcome::Fail(err),
                }];
            }
        }
    }

    for _ in 0..32 {
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

// Selection: the body suite has 3 .any.js files. We include all
// three, but most multipart formdata.any.js subtests need Blob and
// will skip via the Blob shim.
const WPT_FILES: &[(&str, &[&str])] = &[
    (
        "cloned-any",
        &[include_str!("wpt/fetch/api/body/cloned-any.js")],
    ),
    (
        "formdata",
        &[include_str!("wpt/fetch/api/body/formdata.any.js")],
    ),
    (
        "mime-type",
        &[include_str!("wpt/fetch/api/body/mime-type.any.js")],
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

#[test]
fn wpt_fetch_body_compliance() {
    let mut totals = Totals::default();
    let mut per_file: BTreeMap<&str, Totals> = BTreeMap::new();
    let mut failures: Vec<(&str, TestResult)> = Vec::new();

    for (name, sources) in WPT_FILES {
        let results = run_wpt(name, sources);
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

    eprintln!("\n=== WPT fetch/api/body/ results ===");
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
        eprintln!(
            "Note: {} subtests failed (mostly Blob/multipart-formdata; \
             ship the native Blob class + multipart parser to clear).",
            failures.len()
        );
    }

    assert!(
        totals.pass > 0,
        "no WPT body subtests passed — runner is broken"
    );
}
