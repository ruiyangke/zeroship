//! CORS preflight + actual-response header injection.
//!
//! The resource-tree dispatch calls `build_preflight_response` when
//! `OPTIONS` arrives with an `Origin`, and `inject_cors_response_headers`
//! after a normal response is built so the browser sees the right
//! Allow-Origin / Vary / Expose-Headers / Allow-Credentials surface.

use ntex::web::HttpResponse;

/// Build a 204 preflight response for a matching CORS rule. Called when
/// the request is `OPTIONS` with an `Origin` header AND a CORS-bearing
/// rule matches the `Access-Control-Request-Method` + path. The body is
/// empty; the headers tell the browser whether to proceed with the
/// actual request.
pub(super) fn build_preflight_response(
    cors: &zeroship_bundle::Cors,
    origin: &str,
    wall_start: std::time::Instant,
) -> HttpResponse {
    use ntex::http::StatusCode;
    let mut resp = HttpResponse::build(StatusCode::NO_CONTENT);
    // Allow-Origin: "*" only when credentials disabled; specific origin
    // when in the explicit allow list. Anything else → no Allow-Origin
    // header at all and the browser blocks.
    let wildcard_ok = cors.allow_origins.iter().any(|o| o == "*") && !cors.allow_credentials;
    let exact_match = cors.allow_origins.iter().any(|o| o == origin);
    if wildcard_ok {
        resp.header("access-control-allow-origin", "*");
    } else if exact_match {
        resp.header("access-control-allow-origin", origin);
        resp.header("vary", "Origin");
    }
    if (wildcard_ok || exact_match) && !cors.allow_methods.is_empty() {
        let methods = cors
            .allow_methods
            .iter()
            .map(|m| m.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        resp.header("access-control-allow-methods", methods);
    }
    if (wildcard_ok || exact_match) && !cors.allow_headers.is_empty() {
        resp.header(
            "access-control-allow-headers",
            cors.allow_headers.join(", "),
        );
    }
    if cors.allow_credentials && exact_match {
        resp.header("access-control-allow-credentials", "true");
    }
    if let Some(seconds) = cors.max_age_seconds {
        if wildcard_ok || exact_match {
            resp.header("access-control-max-age", seconds.to_string());
        }
    }
    resp.header(
        "x-wall-time-ms",
        format!("{:.2}", wall_start.elapsed().as_secs_f64() * 1000.0),
    );
    resp.finish()
}

/// Inject `Access-Control-*` response headers on a non-preflight
/// response. Mirrors the preflight logic for Allow-Origin/Vary; also
/// emits `Expose-Headers` and `Allow-Credentials` so the browser exposes
/// the right response surface.
pub(super) fn inject_cors_response_headers(
    headers: &mut ntex::http::HeaderMap,
    cors: &zeroship_bundle::Cors,
    origin: &str,
) {
    use ntex::http::header::{HeaderName, HeaderValue};
    let wildcard_ok = cors.allow_origins.iter().any(|o| o == "*") && !cors.allow_credentials;
    let exact_match = cors.allow_origins.iter().any(|o| o == origin);
    if wildcard_ok {
        if let Ok(v) = HeaderValue::from_str("*") {
            headers.insert(HeaderName::from_static("access-control-allow-origin"), v);
        }
    } else if exact_match {
        if let Ok(v) = HeaderValue::from_str(origin) {
            headers.insert(HeaderName::from_static("access-control-allow-origin"), v);
        }
        headers.insert(
            HeaderName::from_static("vary"),
            HeaderValue::from_static("Origin"),
        );
    } else {
        // Origin not in allow list → no headers; the browser blocks.
        return;
    }
    if !cors.expose_headers.is_empty() {
        if let Ok(v) = HeaderValue::from_str(&cors.expose_headers.join(", ")) {
            headers.insert(
                HeaderName::from_static("access-control-expose-headers"),
                v,
            );
        }
    }
    if cors.allow_credentials && exact_match {
        headers.insert(
            HeaderName::from_static("access-control-allow-credentials"),
            HeaderValue::from_static("true"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::compiled::CompiledManifest;
    use zeroship_bundle::{Cors, HttpMethod, Manifest, ResourceEntry};

    /// Single-resource manifest with a CORS policy attached to `/api/*`.
    /// All preflight tests use this shape; the path narrowness keeps
    /// the assertions specific.
    fn manifest_with_cors(cors: Cors) -> Manifest {
        let mut resources = std::collections::HashMap::new();
        resources.insert(
            "/api/*".into(),
            ResourceEntry {
                cors: Some(cors),
                ..Default::default()
            },
        );
        Manifest {
            version: 1,
            resources,
            ..Manifest::default()
        }
    }

    fn header_str<'a>(resp: &'a HttpResponse, name: &str) -> Option<&'a str> {
        resp.headers().get(name).and_then(|v| v.to_str().ok())
    }

    #[test]
    fn preflight_allowed_origin() {
        let cors = Cors {
            allow_origins: vec!["https://example.com".into()],
            allow_methods: vec![HttpMethod::Get, HttpMethod::Post],
            allow_headers: vec!["content-type".into(), "authorization".into()],
            expose_headers: vec![],
            allow_credentials: false,
            max_age_seconds: Some(600),
        };
        let m = manifest_with_cors(cors.clone());
        let c = CompiledManifest::compile(&m);
        // Preflight asks "can I POST /api/users?" — the matched
        // resource carries CORS so we answer 204.
        let policy = c
            .lookup_resource("/api/users")
            .expect("matches /api/* resource");
        let policy_cors = policy.cors.as_ref().expect("resource has cors policy");
        assert_eq!(policy_cors.allow_origins, cors.allow_origins);
        let resp = build_preflight_response(
            policy_cors,
            "https://example.com",
            std::time::Instant::now(),
        );
        assert_eq!(resp.status(), ntex::http::StatusCode::NO_CONTENT);
        assert_eq!(
            header_str(&resp, "access-control-allow-origin"),
            Some("https://example.com")
        );
        assert_eq!(header_str(&resp, "vary"), Some("Origin"));
        let methods = header_str(&resp, "access-control-allow-methods").unwrap();
        assert!(methods.contains("GET"));
        assert!(methods.contains("POST"));
        let headers = header_str(&resp, "access-control-allow-headers").unwrap();
        assert!(headers.contains("content-type"));
        assert_eq!(header_str(&resp, "access-control-max-age"), Some("600"));
    }

    #[test]
    fn preflight_disallowed_origin() {
        // Origin is not in the allow list — respond 204 but WITHOUT
        // `Access-Control-Allow-Origin`. The browser blocks.
        let cors = Cors {
            allow_origins: vec!["https://allowed.com".into()],
            allow_methods: vec![HttpMethod::Post],
            allow_headers: vec![],
            expose_headers: vec![],
            allow_credentials: false,
            max_age_seconds: None,
        };
        let resp = build_preflight_response(
            &cors,
            "https://evil.com",
            std::time::Instant::now(),
        );
        assert_eq!(resp.status(), ntex::http::StatusCode::NO_CONTENT);
        assert!(
            resp.headers().get("access-control-allow-origin").is_none(),
            "no allow-origin header for disallowed origin"
        );
        assert!(resp.headers().get("vary").is_none());
    }

    #[test]
    fn preflight_wildcard_origin_no_credentials() {
        let cors = Cors {
            allow_origins: vec!["*".into()],
            allow_methods: vec![HttpMethod::Get],
            allow_headers: vec![],
            expose_headers: vec![],
            allow_credentials: false,
            max_age_seconds: None,
        };
        let resp = build_preflight_response(
            &cors,
            "https://anything.example",
            std::time::Instant::now(),
        );
        assert_eq!(
            header_str(&resp, "access-control-allow-origin"),
            Some("*")
        );
        // No `Vary` for wildcard responses — they don't depend on origin.
        assert!(resp.headers().get("vary").is_none());
        assert!(
            resp.headers()
                .get("access-control-allow-credentials")
                .is_none(),
            "no credentials header on wildcard preflight"
        );
    }

    #[test]
    fn preflight_no_matching_resource_falls_through() {
        // Manifest has a CORS resource at /api/*, but the request
        // targets /other. lookup_resource returns None → router falls
        // through to a 404 response.
        let cors = Cors {
            allow_origins: vec!["https://example.com".into()],
            allow_methods: vec![HttpMethod::Post],
            allow_headers: vec![],
            expose_headers: vec![],
            allow_credentials: false,
            max_age_seconds: None,
        };
        let m = manifest_with_cors(cors);
        let c = CompiledManifest::compile(&m);
        assert!(c.lookup_resource("/other").is_none());
    }

    #[test]
    fn actual_request_injects_cors_headers() {
        // Non-preflight: real POST with Origin matching → response
        // headers include allow-origin + Vary.
        let cors = Cors {
            allow_origins: vec!["https://example.com".into()],
            allow_methods: vec![HttpMethod::Post],
            allow_headers: vec![],
            expose_headers: vec!["x-request-id".into()],
            allow_credentials: false,
            max_age_seconds: None,
        };
        let mut resp = HttpResponse::Ok().finish();
        inject_cors_response_headers(resp.headers_mut(), &cors, "https://example.com");
        assert_eq!(
            header_str(&resp, "access-control-allow-origin"),
            Some("https://example.com")
        );
        assert_eq!(header_str(&resp, "vary"), Some("Origin"));
        assert_eq!(
            header_str(&resp, "access-control-expose-headers"),
            Some("x-request-id")
        );
    }

    #[test]
    fn actual_request_disallowed_origin_no_headers() {
        let cors = Cors {
            allow_origins: vec!["https://allowed.com".into()],
            allow_methods: vec![HttpMethod::Post],
            allow_headers: vec![],
            expose_headers: vec!["x-request-id".into()],
            allow_credentials: false,
            max_age_seconds: None,
        };
        let mut resp = HttpResponse::Ok().finish();
        inject_cors_response_headers(resp.headers_mut(), &cors, "https://evil.com");
        assert!(
            resp.headers().get("access-control-allow-origin").is_none(),
            "no allow-origin for disallowed origin"
        );
        assert!(resp.headers().get("vary").is_none());
        assert!(
            resp.headers()
                .get("access-control-expose-headers")
                .is_none(),
            "expose-headers requires an allowed origin"
        );
    }

    #[test]
    fn actual_request_wildcard_no_credentials() {
        let cors = Cors {
            allow_origins: vec!["*".into()],
            allow_methods: vec![HttpMethod::Get],
            allow_headers: vec![],
            expose_headers: vec![],
            allow_credentials: false,
            max_age_seconds: None,
        };
        let mut resp = HttpResponse::Ok().finish();
        inject_cors_response_headers(resp.headers_mut(), &cors, "https://anything.example");
        assert_eq!(
            header_str(&resp, "access-control-allow-origin"),
            Some("*")
        );
        assert!(resp.headers().get("vary").is_none(), "no Vary on wildcard");
    }
}
