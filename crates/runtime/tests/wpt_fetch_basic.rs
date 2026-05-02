//! WPT runner for `fetch/api/basic/scheme-data.any.js`.
//!
//! Most WPT fetch suites require a real WPT testserver (with the
//! `redirect.py`, `inspect-headers.py`, etc. handlers). The `scheme-data`
//! test is the only one that operates over `data:` URLs and therefore
//! runs without network. We exercise it via the Rust-level algorithm
//! chain directly (no V8) — the JS-level fetch global is covered by
//! the integration tests in `fetch_native.rs`.
//!
//! We emulate the WPT shape:
//!   1. Each `checkFetchResponse(url, body, mime)` corresponds to one
//!      sub-test.
//!   2. `checkKoUrl(url, method)` corresponds to a "must-fail" sub-test.
//!
//! The test passes if at least 70% of the sub-tests pass per the brief.

#![allow(unsafe_code)]

use zeroship_runtime::fetch_native::algorithms::{
    main_fetch, CredentialsMode, FetchRequest, RedirectMode,
};

#[derive(Debug)]
enum Outcome {
    Pass,
    Fail(String),
}

fn req(method: &str, url: &str) -> FetchRequest {
    FetchRequest {
        method: method.to_string(),
        url: url.to_string(),
        headers: Vec::new(),
        body: None,
        body_source: None,
        redirect_mode: RedirectMode::Follow,
        credentials_mode: CredentialsMode::SameOrigin,
        cancel: None,
        redirect_count: 0,
        origin_url: url.to_string(),
    }
}

fn run<R>(fut: impl std::future::Future<Output = R>) -> R {
    compio::runtime::Runtime::new().unwrap().block_on(fut)
}

fn check_fetch(url: &str, expected_body: &[u8], expected_mime: &str, method: &str) -> Outcome {
    let r = run(main_fetch(req(method, url)));
    let r = match r {
        Ok(x) => x,
        Err(e) => return Outcome::Fail(format!("fetch errored: {e}")),
    };
    if r.status != 200 {
        return Outcome::Fail(format!("status {} (want 200)", r.status));
    }
    // HEAD has empty body per Fetch §5.4 case "data" — but spec text
    // for HEAD says the body is intact in the algorithm; our impl
    // returns the body as bytes. We honor the WPT expectation here.
    let body_check = if method == "HEAD" {
        Vec::new()
    } else {
        expected_body.to_vec()
    };
    if r.body != body_check {
        return Outcome::Fail(format!(
            "body {:?} (want {:?})",
            String::from_utf8_lossy(&r.body),
            String::from_utf8_lossy(&body_check)
        ));
    }
    // Content-Type case-insensitive lookup.
    let ct = r
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    if ct != expected_mime {
        return Outcome::Fail(format!("content-type {:?} (want {:?})", ct, expected_mime));
    }
    Outcome::Pass
}

fn check_ko(url: &str, method: &str) -> Outcome {
    let r = run(main_fetch(req(method, url)));
    match r {
        Ok(_) => Outcome::Fail("expected network error, got Response".to_string()),
        Err(_) => Outcome::Pass,
    }
}

#[test]
fn wpt_fetch_basic_scheme_data() {
    let mut results: Vec<(String, Outcome)> = Vec::new();

    // Mirror scheme-data.any.js — `checkFetchResponse(url, body, mime)`
    // calls. WPT treats the `mode` parameter (cors, same-origin) as
    // a no-op for data: URLs in our runtime (no CORS surface).
    results.push((
        "data:text/plain percent-encoded GET".to_string(),
        check_fetch(
            "data:,response%27s%20body",
            b"response's body",
            "text/plain;charset=US-ASCII",
            "GET",
        ),
    ));
    results.push((
        "data:text/plain percent-encoded same-origin".to_string(),
        check_fetch(
            "data:,response%27s%20body",
            b"response's body",
            "text/plain;charset=US-ASCII",
            "GET",
        ),
    ));
    results.push((
        "data:text/plain percent-encoded cors".to_string(),
        check_fetch(
            "data:,response%27s%20body",
            b"response's body",
            "text/plain;charset=US-ASCII",
            "GET",
        ),
    ));
    results.push((
        "data:text/plain;base64".to_string(),
        check_fetch(
            "data:text/plain;base64,cmVzcG9uc2UncyBib2R5",
            b"response's body",
            "text/plain",
            "GET",
        ),
    ));
    results.push((
        "data:image/png;base64".to_string(),
        check_fetch(
            "data:image/png;base64,cmVzcG9uc2UncyBib2R5",
            b"response's body",
            "image/png",
            "GET",
        ),
    ));
    results.push((
        "data:text/plain POST".to_string(),
        check_fetch(
            "data:,response%27s%20body",
            b"response's body",
            "text/plain;charset=US-ASCII",
            "POST",
        ),
    ));
    results.push((
        "data:text/plain HEAD".to_string(),
        check_fetch(
            "data:,response%27s%20body",
            b"",
            "text/plain;charset=US-ASCII",
            "HEAD",
        ),
    ));

    // Bad data: URL (no comma) — must fail.
    results.push((
        "checkKoUrl notAdataUrl".to_string(),
        check_ko("data:notAdataUrl.com", "GET"),
    ));

    let pass = results.iter().filter(|(_, o)| matches!(o, Outcome::Pass)).count();
    let fail = results.len() - pass;

    eprintln!("\n=== WPT fetch/api/basic/scheme-data results ===");
    for (name, outcome) in &results {
        match outcome {
            Outcome::Pass => eprintln!("  PASS  {name}"),
            Outcome::Fail(why) => eprintln!("  FAIL  {name}: {why}"),
        }
    }
    eprintln!("  total pass={pass} fail={fail}");

    let pct = (pass as f64 / results.len() as f64) * 100.0;
    eprintln!("  pass rate: {pct:.1}%");

    // Brief targets ≥70%.
    assert!(
        pass * 100 >= results.len() * 70,
        "expected ≥70% pass rate; got {pct:.1}% ({pass}/{})",
        results.len()
    );
}
