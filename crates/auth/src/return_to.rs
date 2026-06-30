//! Same-origin return-target helpers for native IdP UI flows.

use ntex::http::header::{HeaderValue, LOCATION};
use ntex::http::StatusCode;
use ntex::web::{HttpRequest, HttpResponse};

pub const SAFE_DEFAULT: &str = "/me";

#[must_use]
pub fn sanitize(raw: Option<&str>, fallback: &str) -> String {
    raw.and_then(valid_path)
        .unwrap_or(fallback)
        .to_string()
}

#[must_use]
pub fn valid_path(raw: &str) -> Option<&str> {
    let value = raw.trim();
    // A same-origin path: one leading `/`, no `//` (scheme-relative), no `\`
    // (which some URL parsers fold to `/`), and NO C0 control characters or DEL.
    // The control-char ban is defense-in-depth: every sink today gates the value
    // through `HeaderValue::from_str` (rejects CR/LF) or askama auto-escaping, but
    // the contract is "same-origin path" and an embedded CR/LF/NUL must never pass
    // — `trim()` only strips the ends, so an interior `\r\n` would otherwise survive
    // and leak header/log injection to any future sink that skips the HeaderValue gate.
    if value.starts_with('/')
        && !value.starts_with("//")
        && !value.contains('\\')
        && !value.is_empty()
        && !value.bytes().any(|b| b < 0x20 || b == 0x7f)
    {
        Some(value)
    } else {
        None
    }
}

#[must_use]
pub fn request_target(req: &HttpRequest) -> String {
    if req.query_string().is_empty() {
        req.path().to_string()
    } else {
        format!("{}?{}", req.path(), req.query_string())
    }
}

#[must_use]
pub fn login_location(return_to: &str) -> String {
    with_return_to("/login", return_to)
}

#[must_use]
pub fn consent_location(return_to: &str) -> String {
    with_return_to("/consent", return_to)
}

#[must_use]
pub fn with_return_to(path: &str, return_to: &str) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("return_to", return_to)
        .finish();
    format!("{path}?{query}")
}

#[must_use]
pub fn query_param(return_to: &str, key: &str) -> Option<String> {
    let url = url::Url::parse(&format!("http://zeroship.local{return_to}")).ok()?;
    url.query_pairs().find_map(|(name, value)| {
        if name == key {
            Some(value.into_owned())
        } else {
            None
        }
    })
}

#[must_use]
pub fn see_other(location: &str) -> ntex::web::HttpResponseBuilder {
    let mut resp = HttpResponse::build(StatusCode::SEE_OTHER);
    resp.header(
        LOCATION,
        HeaderValue::from_str(location).unwrap_or_else(|_| HeaderValue::from_static(SAFE_DEFAULT)),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::{sanitize, valid_path, SAFE_DEFAULT};

    #[test]
    fn valid_path_accepts_same_origin_paths() {
        assert_eq!(valid_path("/me"), Some("/me"));
        assert_eq!(valid_path("/authorize?client_id=x&scope=openid"), Some("/authorize?client_id=x&scope=openid"));
        // Leading/trailing whitespace is trimmed, then accepted.
        assert_eq!(valid_path("  /me  "), Some("/me"));
    }

    #[test]
    fn valid_path_rejects_open_redirect_vectors() {
        // These are the post-URL-decode forms the validator actually sees
        // (ntex decodes %2f/%5c before this point).
        for bad in [
            "//evil.com",          // scheme-relative
            "///evil.com",         // multi-slash scheme-relative
            "https://evil.com",    // absolute URL (no leading `/`)
            "/\\evil.com",         // backslash fold
            "\\/\\/evil.com",      // backslash scheme-relative
            "/\\/\\evil",          // mixed
            "evil.com",            // no leading slash
            "",                    // empty
            "   ",                 // whitespace-only → empty after trim
        ] {
            assert_eq!(valid_path(bad), None, "must reject {bad:?}");
        }
    }

    #[test]
    fn valid_path_rejects_embedded_control_characters() {
        // CR/LF/NUL/DEL embedded in the MIDDLE survive trim() — they must be
        // rejected so header/log-injection can never ride a "valid" path (MED-1).
        for bad in [
            "/me\r\nSet-Cookie: x=1", // CRLF header injection
            "/me\nfoo",               // bare LF
            "/me\rfoo",               // bare CR
            "/me\u{0000}foo",         // NUL
            "/me\u{007f}foo",         // DEL
            "/me\tfoo",               // TAB (C0)
        ] {
            assert_eq!(valid_path(bad), None, "must reject control char in {bad:?}");
        }
    }

    #[test]
    fn sanitize_falls_back_on_rejected_input() {
        assert_eq!(sanitize(Some("//evil.com"), SAFE_DEFAULT), SAFE_DEFAULT);
        assert_eq!(sanitize(Some("/me\r\nx"), SAFE_DEFAULT), SAFE_DEFAULT);
        assert_eq!(sanitize(None, SAFE_DEFAULT), SAFE_DEFAULT);
        assert_eq!(sanitize(Some("/good"), SAFE_DEFAULT), "/good");
    }
}

