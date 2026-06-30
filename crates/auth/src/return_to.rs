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
    if value.starts_with('/')
        && !value.starts_with("//")
        && !value.contains('\\')
        && !value.is_empty()
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

