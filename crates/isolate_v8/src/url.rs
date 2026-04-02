//! Native URL implementation using ada-url (same parser as Node.js).
//!
//! Exposes `__urlParse(input, base?)` and `__urlCanParse(input, base?)` to JS.
//! The JS `URL` class wraps these native calls for spec-compliant URL parsing.

use appbase_ops::appbase_op;

/// `__urlParse(input, base?) → JSON string | null`
///
/// Returns a JSON object with all URL components, or null if invalid.
#[appbase_op]
fn url_parse(input: String, base: Option<String>) -> Option<String> {
    let result = ada_url::Url::parse(&input, base.as_deref());
    result.ok().map(|url| {
        serde_json::json!({
            "href": url.href(),
            "protocol": url.protocol(),
            "username": url.username(),
            "password": url.password(),
            "hostname": url.hostname(),
            "port": url.port(),
            "host": url.host(),
            "pathname": url.pathname(),
            "search": url.search(),
            "hash": url.hash(),
            "origin": url.origin(),
        })
        .to_string()
    })
}

/// `__urlCanParse(input, base?) → boolean`
#[appbase_op]
fn url_can_parse(input: String, base: Option<String>) -> bool {
    ada_url::Url::can_parse(&input, base.as_deref())
}
