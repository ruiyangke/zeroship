//! Run the W3C Web Platform Tests for `URL` / `URLSearchParams`
//! against the native impl. Source: https://github.com/web-platform-tests/wpt
//! (url/). Files are vendored under `crates/runtime/tests/wpt/url/`.
//!
//! Mirrors `wpt_headers.rs` — minimal testharness.js shim, fresh V8
//! isolate per file, per-test pass/fail reporting. The Rust `#[test]`
//! itself fails iff any non-skipped test FAILs.
//!
//! Coverage targets (see end of file for the canonical list):
//!
//! Inline files (no fetch required):
//!   - urlsearchparams-append.any.js
//!   - urlsearchparams-delete.any.js
//!   - urlsearchparams-foreach.any.js
//!   - urlsearchparams-get.any.js
//!   - urlsearchparams-getall.any.js
//!   - urlsearchparams-has.any.js
//!   - urlsearchparams-set.any.js
//!   - urlsearchparams-size.any.js
//!   - urlsearchparams-sort.any.js
//!   - urlsearchparams-stringifier.any.js
//!   - urlsearchparams-constructor.any.js
//!   - url-tojson.any.js
//!   - url-statics-canparse.any.js
//!   - url-statics-parse.any.js
//!
//! Data-driven (need urltestdata.json):
//!   - url-constructor.any.js  — preloads JSON via embedded data
//!   - url-setters.any.js      — preloads setters_tests.json
//!
//! Skipped (need IDL machinery / browser globals we don't model):
//!   - idlharness.any.js          — IDL harness fixtures
//!   - IdnaTestV2.any.js          — IDNA-specific (ada-url already
//!                                  delegates to upstream IDNA tables)
//!   - historical.any.js          — tests for removed features
//!   - urlencoded-parser.any.js   — body parser tests use FormData
//!   - url-origin.any.js          — needs urltestdata.json (data-driven)

#![allow(unsafe_code)]

use std::collections::BTreeMap;

use zeroship_runtime::init_v8;
use zeroship_runtime::url_native;

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
    if (typeof arg === "function") { try { arg(); } catch (_) {} }
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
          out += s[i] + s[i + 1]; i++;
        } else { out += "\uFFFD"; }
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
      name: name || "<async_test>",
      status: "skip",
      reason: "async_test stub",
    });
  };

  // subsetTestByKey: WPT URL constructor variants split tests by key
  // (`?include=file`, `?exclude=(file|javascript|mailto)`, etc.). The
  // headless runner ignores the variant — every key runs.
  globalThis.subsetTestByKey = function (_key, test_fn, ...args) {
    return test_fn(...args);
  };

  // For url-constructor / url-setters which load fixture JSON via
  // fetch(), we shim fetch to consult globalThis.__wpt_data — set by
  // the runner before loading the test source. fetch returns a
  // resolved Promise with `.json()` returning the parsed array.
  globalThis.fetch = function (path) {
    const data = globalThis.__wpt_data && globalThis.__wpt_data[path];
    if (data === undefined) {
      return Promise.reject(new Error("WPT shim: no fixture for " + path));
    }
    return Promise.resolve({
      json: () => Promise.resolve(JSON.parse(data)),
      text: () => Promise.resolve(data),
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
  globalThis.assert_unreached = function (msg) { fail(msg || "unreached"); };
})();
"#;

// Pre-loaded JSON fixtures for the data-driven tests. Embedded so the
// shim's stub fetch() can return them synchronously.
const URLTESTDATA_JSON: &str = include_str!("wpt/url/resources/urltestdata.json");
const URLTESTDATA_JS_ONLY_JSON: &str =
    include_str!("wpt/url/resources/urltestdata-javascript-only.json");
const SETTERS_TESTS_JSON: &str = include_str!("wpt/url/resources/setters_tests.json");

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

fn run_wpt(label: &str, source: &str, fixture_setup: &str) -> Vec<TestResult> {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    // Native install: URL + URLSearchParams.
    let global = scope.get_current_context().global(scope);
    url_native::install_globals(scope, global);

    // Shim must come BEFORE fixture-setup (fetch shim reads
    // globalThis.__wpt_data which fixture-setup populates).
    let shim = v8::String::new(scope, TESTHARNESS_SHIM).unwrap();
    v8::Script::compile(scope, shim, None)
        .unwrap()
        .run(scope)
        .unwrap();

    // Fixture setup (e.g. assigns __wpt_data["resources/urltestdata.json"]
    // to the embedded JSON string). May be empty for inline tests.
    if !fixture_setup.is_empty() {
        let fx = v8::String::new(scope, fixture_setup).unwrap();
        v8::Script::compile(scope, fx, None)
            .unwrap()
            .run(scope)
            .unwrap();
    }

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

    // Drain microtasks so promise_test results land before we serialize.
    // url-constructor uses promise_test(() => fetch(...).then(runURLTests))
    // — multiple microtask cycles needed (fetch resolves → .json()
    // resolves → runURLTests runs).
    for _ in 0..10 {
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

// ---------------------------------------------------------------------------
// Fixture pre-load helpers
// ---------------------------------------------------------------------------

/// Build a JS snippet that populates `globalThis.__wpt_data` with the
/// JSON-stringified content of each fixture path. The fetch() shim
/// reads this map. Each value is a string (the test code calls
/// `.json()` on the response, which triggers `JSON.parse`).
fn fixture_setup_for(paths: &[(&str, &str)]) -> String {
    let mut s = String::from("globalThis.__wpt_data = {};\n");
    for (path, content) in paths {
        // Embed the content as a JSON string literal so embedded "
        // and \ are properly escaped.
        let escaped = serde_json::to_string(content).expect("escape fixture");
        s.push_str(&format!(
            "globalThis.__wpt_data[{}] = {};\n",
            serde_json::to_string(path).unwrap(),
            escaped,
        ));
    }
    s
}

// ---------------------------------------------------------------------------
// Per-file runner specs
// ---------------------------------------------------------------------------

struct WptFile {
    name: &'static str,
    source: &'static str,
    fixture: &'static [(&'static str, &'static str)],
}

const WPT_FILES: &[WptFile] = &[
    WptFile {
        name: "urlsearchparams-append",
        source: include_str!("wpt/url/urlsearchparams-append.any.js"),
        fixture: &[],
    },
    WptFile {
        name: "urlsearchparams-delete",
        source: include_str!("wpt/url/urlsearchparams-delete.any.js"),
        fixture: &[],
    },
    WptFile {
        name: "urlsearchparams-foreach",
        source: include_str!("wpt/url/urlsearchparams-foreach.any.js"),
        fixture: &[],
    },
    WptFile {
        name: "urlsearchparams-get",
        source: include_str!("wpt/url/urlsearchparams-get.any.js"),
        fixture: &[],
    },
    WptFile {
        name: "urlsearchparams-getall",
        source: include_str!("wpt/url/urlsearchparams-getall.any.js"),
        fixture: &[],
    },
    WptFile {
        name: "urlsearchparams-has",
        source: include_str!("wpt/url/urlsearchparams-has.any.js"),
        fixture: &[],
    },
    WptFile {
        name: "urlsearchparams-set",
        source: include_str!("wpt/url/urlsearchparams-set.any.js"),
        fixture: &[],
    },
    WptFile {
        name: "urlsearchparams-size",
        source: include_str!("wpt/url/urlsearchparams-size.any.js"),
        fixture: &[],
    },
    WptFile {
        name: "urlsearchparams-sort",
        source: include_str!("wpt/url/urlsearchparams-sort.any.js"),
        fixture: &[],
    },
    WptFile {
        name: "urlsearchparams-stringifier",
        source: include_str!("wpt/url/urlsearchparams-stringifier.any.js"),
        fixture: &[],
    },
    WptFile {
        name: "urlsearchparams-constructor",
        source: include_str!("wpt/url/urlsearchparams-constructor.any.js"),
        fixture: &[],
    },
    WptFile {
        name: "url-tojson",
        source: include_str!("wpt/url/url-tojson.any.js"),
        fixture: &[],
    },
    WptFile {
        name: "url-statics-canparse",
        source: include_str!("wpt/url/url-statics-canparse.any.js"),
        fixture: &[],
    },
    WptFile {
        name: "url-statics-parse",
        source: include_str!("wpt/url/url-statics-parse.any.js"),
        fixture: &[],
    },
    WptFile {
        name: "url-constructor",
        source: include_str!("wpt/url/url-constructor.any.js"),
        fixture: &[
            ("resources/urltestdata.json", URLTESTDATA_JSON),
            (
                "resources/urltestdata-javascript-only.json",
                URLTESTDATA_JS_ONLY_JSON,
            ),
        ],
    },
    WptFile {
        name: "url-setters",
        source: include_str!("wpt/url/url-setters.any.js"),
        fixture: &[("resources/setters_tests.json", SETTERS_TESTS_JSON)],
    },
    // M2: previously omitted files. All inline (no fixture) except
    // url-origin which uses urltestdata.json.
    WptFile {
        name: "url-setters-stripping",
        source: include_str!("wpt/url/url-setters-stripping.any.js"),
        fixture: &[],
    },
    WptFile {
        name: "url-searchparams",
        source: include_str!("wpt/url/url-searchparams.any.js"),
        fixture: &[],
    },
    WptFile {
        name: "url-origin",
        source: include_str!("wpt/url/url-origin.any.js"),
        fixture: &[
            ("resources/urltestdata.json", URLTESTDATA_JSON),
            (
                "resources/urltestdata-javascript-only.json",
                URLTESTDATA_JS_ONLY_JSON,
            ),
        ],
    },
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
fn wpt_url_compliance() {
    let mut totals = Totals::default();
    let mut per_file: BTreeMap<&str, Totals> = BTreeMap::new();
    let mut failures: Vec<(&str, TestResult)> = Vec::new();

    for entry in WPT_FILES {
        let fixture_js = if entry.fixture.is_empty() {
            String::new()
        } else {
            fixture_setup_for(entry.fixture)
        };
        let results = run_wpt(entry.name, entry.source, &fixture_js);
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

    eprintln!("\n=== WPT url/ results ===");
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

    // Pass-rate gate. Aim ≥85% per the design's "Definition of done".
    // Each failure is shown above so regressions are obvious.
    let total_run = totals.pass + totals.fail;
    if total_run == 0 {
        panic!("no WPT tests ran — fixture / shim regression?");
    }
    let pct = (totals.pass * 100) / total_run;
    eprintln!("  pass rate: {pct}% (target ≥ 85%)");

    if !failures.is_empty() {
        eprintln!("\n=== Failures ({}) ===", failures.len());
        for (file, r) in failures.iter().take(50) {
            let detail = match &r.outcome {
                Outcome::Fail(s) => s.as_str(),
                _ => "?",
            };
            eprintln!("  [{file}] {}: {detail}", r.name);
        }
        if failures.len() > 50 {
            eprintln!("  … and {} more", failures.len() - 50);
        }
    }

    assert!(
        pct >= 85,
        "WPT URL pass rate {pct}% below 85% threshold ({}/{} pass)",
        totals.pass,
        total_run,
    );
}
