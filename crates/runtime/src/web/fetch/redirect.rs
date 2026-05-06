//! Redirect helpers per Fetch §5.6 (HTTP-redirect fetch) and design §VIII.
//!
//! This module owns the algorithm-level decisions about how to mutate a
//! request when chasing a redirect. The actual HTTP retransmission lives
//! in `http_network` — this module is pure logic with no V8 entry.
//!
//! ## Decision matrix (Fetch §5.6 step 13)
//!
//! | Status | New method (if not GET/HEAD)         | Body          |
//! | ------ | ------------------------------------- | ------------- |
//! | 301    | POST → GET; others stay              | dropped on POST→GET |
//! | 302    | POST → GET; others stay              | dropped on POST→GET |
//! | 303    | All non-GET/HEAD → GET                | dropped       |
//! | 307    | Method preserved                      | preserved     |
//! | 308    | Method preserved                      | preserved     |
//!
//! When the response is 307/308 and the request
//! body source is `BodySource::Stream` (non-rewindable), the redirect is
//! a network error: we can't replay the bytes.
//!
//! ## Cross-origin Authorization stripping
//!
//! Per Fetch §5.6 step 13, on a
//! cross-origin redirect, ONLY the `Authorization` header is removed.
//! `Cookie`, `Host`, `Proxy-Authorization` are forbidden headers anyway
//! (set by the user agent, not user code), so we never put them on the
//! request in the first place.
//!
//! This is a deliberate spec-faithful narrowing. Earlier drafts stripped
//! more aggressively, breaking real-world flows (S3 presigned URLs,
//! GitHub redirects).

use crate::fetch_body::body::BodySource;

/// Result of inspecting a redirect response. Returned by
/// [`apply_redirect_method`] so the caller knows whether to drop the
/// body and what the new method is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedirectMethodChange {
    /// The new method string (uppercase for standard methods).
    pub method: String,
    /// Whether the body should be dropped on the next hop.
    pub drop_body: bool,
    /// Whether `Content-*` request headers should be stripped (per
    /// Fetch §5.6 step 14: when body is dropped, drop `Content-Length`,
    /// `Content-Encoding`, `Content-Language`, `Content-Location`,
    /// `Content-Type` from the request).
    pub strip_content_headers: bool,
}

/// Per Fetch §5.6 step 13–14, mutate the method/body for the next hop.
/// `status` is the redirect status (one of 301/302/303/307/308).
pub fn apply_redirect_method(status: u16, current_method: &str) -> RedirectMethodChange {
    let upper = current_method.to_ascii_uppercase();
    match status {
        301 | 302 => {
            // POST → GET; body dropped. Other methods preserved.
            if upper == "POST" {
                RedirectMethodChange {
                    method: "GET".to_string(),
                    drop_body: true,
                    strip_content_headers: true,
                }
            } else {
                RedirectMethodChange {
                    method: upper,
                    drop_body: false,
                    strip_content_headers: false,
                }
            }
        }
        303 => {
            // All methods except GET/HEAD become GET; body dropped.
            if upper == "GET" || upper == "HEAD" {
                RedirectMethodChange {
                    method: upper,
                    drop_body: false,
                    strip_content_headers: false,
                }
            } else {
                RedirectMethodChange {
                    method: "GET".to_string(),
                    drop_body: true,
                    strip_content_headers: true,
                }
            }
        }
        307 | 308 => {
            // Method preserved; body preserved (caller verifies
            // rewindability separately).
            RedirectMethodChange {
                method: upper,
                drop_body: false,
                strip_content_headers: false,
            }
        }
        _ => {
            // Not a redirect status — caller should not have invoked
            // this helper. Defensive: keep current method and don't
            // touch body.
            RedirectMethodChange {
                method: upper,
                drop_body: false,
                strip_content_headers: false,
            }
        }
    }
}

/// Per Fetch §5.6 step 12 "If actualResponse's status is 307 or 308 and
/// request's body's source is null, return a network error."
///
/// In our model, "body's source is null" means a `BodySource::Stream`
/// (the user fed us a ReadableStream — non-rewindable). All other
/// `BodySource` variants are rewindable via `safely_extract_body`.
pub fn body_rewindable_for_status(status: u16, source: &Option<BodySource>) -> bool {
    if !matches!(status, 307 | 308) {
        // 301/302/303 either preserve a non-body method or drop the
        // body — rewindability moot.
        return true;
    }
    match source {
        Some(s) => s.is_rewindable(),
        // No body — nothing to rewind.
        None => true,
    }
}

/// Compute the origin tuple `(scheme, host, port)` for a URL. Used by
/// [`is_same_origin`] to match the Fetch §3.2 origin definition.
///
/// Returns `None` for URLs without a scheme or host (e.g. `data:`,
/// invalid).
pub fn url_origin(url: &str) -> Option<(String, String, u16)> {
    let parsed = ada_url::Url::parse(url, None).ok()?;
    let scheme = parsed.protocol().trim_end_matches(':').to_lowercase();
    // hostname() is just the host (no port). host() can include `:port`.
    let host = parsed.hostname().to_lowercase();
    if host.is_empty() {
        return None;
    }
    let default_port = match scheme.as_str() {
        "http" | "ws" => 80,
        "https" | "wss" => 443,
        _ => return None,
    };
    let port_str = parsed.port();
    let port = if port_str.is_empty() {
        default_port
    } else {
        port_str.parse().unwrap_or(default_port)
    };
    Some((scheme, host, port))
}

/// True if `from` and `to` share the same Fetch §3.2 origin tuple
/// (scheme, host, port). Used by the redirect step to decide whether to
/// strip `Authorization`.
pub fn is_same_origin(from: &str, to: &str) -> bool {
    match (url_origin(from), url_origin(to)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_301_post_becomes_get() {
        let r = apply_redirect_method(301, "POST");
        assert_eq!(r.method, "GET");
        assert!(r.drop_body);
        assert!(r.strip_content_headers);
    }

    #[test]
    fn redirect_301_put_preserved() {
        let r = apply_redirect_method(301, "PUT");
        assert_eq!(r.method, "PUT");
        assert!(!r.drop_body);
    }

    #[test]
    fn redirect_303_post_becomes_get() {
        let r = apply_redirect_method(303, "POST");
        assert_eq!(r.method, "GET");
        assert!(r.drop_body);
    }

    #[test]
    fn redirect_303_put_becomes_get() {
        let r = apply_redirect_method(303, "PUT");
        assert_eq!(r.method, "GET");
        assert!(r.drop_body);
    }

    #[test]
    fn redirect_303_get_stays_get() {
        let r = apply_redirect_method(303, "GET");
        assert_eq!(r.method, "GET");
        assert!(!r.drop_body);
    }

    #[test]
    fn redirect_307_post_preserved() {
        let r = apply_redirect_method(307, "POST");
        assert_eq!(r.method, "POST");
        assert!(!r.drop_body);
    }

    #[test]
    fn redirect_308_put_preserved() {
        let r = apply_redirect_method(308, "PUT");
        assert_eq!(r.method, "PUT");
        assert!(!r.drop_body);
    }

    #[test]
    fn rewindable_307_with_stream_body_fails() {
        let s = Some(BodySource::Stream);
        assert!(!body_rewindable_for_status(307, &s));
    }

    #[test]
    fn rewindable_307_with_bytes_body_succeeds() {
        let s = Some(BodySource::Bytes(std::rc::Rc::new(vec![1, 2, 3])));
        assert!(body_rewindable_for_status(307, &s));
    }

    #[test]
    fn rewindable_303_with_stream_body_succeeds() {
        // 303 drops the body anyway, so rewindability is moot.
        let s = Some(BodySource::Stream);
        assert!(body_rewindable_for_status(303, &s));
    }

    #[test]
    fn same_origin_https() {
        assert!(is_same_origin("https://example.com/a", "https://example.com/b"));
    }

    #[test]
    fn same_origin_with_default_port() {
        assert!(is_same_origin(
            "https://example.com/a",
            "https://example.com:443/b"
        ));
    }

    #[test]
    fn cross_origin_different_host() {
        assert!(!is_same_origin(
            "https://example.com/a",
            "https://other.com/b"
        ));
    }

    #[test]
    fn cross_origin_different_scheme() {
        assert!(!is_same_origin(
            "https://example.com/a",
            "http://example.com/a"
        ));
    }

    #[test]
    fn cross_origin_different_port() {
        assert!(!is_same_origin(
            "https://example.com:8443/a",
            "https://example.com/a"
        ));
    }
}
