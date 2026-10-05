//! Web API surface — the spec-facing classes and helpers we install on
//! every isolate (Headers, URL, Blob, fetch, streams, crypto, WebSocket,
//! TextEncoder, etc.).
//!
//! Distinct from `core/` (V8 + dispatch + module loader) and `transport/`
//! (Rust-side HTTP plumbing — cyper client, SSRF guard, kernel bridge).

pub mod base64;
pub mod blob;
pub mod codec;
pub mod crypto;
pub mod dom;
pub mod encoding;
pub mod eventsource;
pub mod fetch;
pub mod headers;
pub mod streams;
pub mod structured_clone;
pub mod url;
pub mod websocket;

// Back-compat shim: `node:crypto` is a Node-API surface, not a Web
// API, so it was moved to `crate::node::crypto`. Re-export here so
// existing `crate::web::crypto_node::*` paths keep resolving until
// callers naturally migrate.
pub use crate::node::crypto as crypto_node;

/// The longest text, in bytes, [`js_text`] turns into a JS string.
const MAX_JS_TEXT_BYTES: usize = 16 * 1024;

/// A JS string for a message that may carry creator-supplied text (a URL,
/// an error message from creator code). `v8::String::new` refuses text past
/// V8's string length limit, and a panic inside a V8 callback aborts the
/// process, so the text is cut at [`MAX_JS_TEXT_BYTES`] on a character
/// boundary, marked with an ellipsis, and an empty string stands in if V8
/// still refuses it.
pub(crate) fn js_text<'s>(scope: &mut v8::PinScope<'s, '_>, text: &str) -> v8::Local<'s, v8::String> {
    v8::String::new(scope, &bounded_text(text)).unwrap_or_else(|| v8::String::empty(scope))
}

/// `text` cut to at most [`MAX_JS_TEXT_BYTES`] on a character boundary,
/// with an ellipsis when it was cut.
fn bounded_text(text: &str) -> std::borrow::Cow<'_, str> {
    if text.len() <= MAX_JS_TEXT_BYTES {
        return text.into();
    }
    let mut end = MAX_JS_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end]).into()
}

#[cfg(test)]
mod tests {
    use super::{bounded_text, MAX_JS_TEXT_BYTES};

    #[test]
    fn bounded_text_keeps_short_text_and_cuts_long_text_on_a_character_boundary() {
        assert_eq!(bounded_text("short"), "short");
        let exact = "a".repeat(MAX_JS_TEXT_BYTES);
        assert_eq!(bounded_text(&exact), exact.as_str());

        let long = "a".repeat(MAX_JS_TEXT_BYTES * 4);
        let cut = bounded_text(&long);
        assert_eq!(cut.len(), MAX_JS_TEXT_BYTES + 3);
        assert!(cut.ends_with("..."));

        // A three-byte character straddling the limit is dropped whole.
        let straddling = format!("{}{}", "a".repeat(MAX_JS_TEXT_BYTES - 1), "\u{20ac}".repeat(4));
        let cut = bounded_text(&straddling);
        assert_eq!(cut, format!("{}...", "a".repeat(MAX_JS_TEXT_BYTES - 1)));
    }
}
