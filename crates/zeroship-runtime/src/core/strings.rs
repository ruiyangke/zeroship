//! Native text as V8 strings, without a panic on length.
//!
//! V8 refuses to build a string from more than `v8::String::MAX_LENGTH` bytes
//! of UTF-8 or Latin-1 source, and `v8::String::new` reports the refusal as
//! `None`. Native code builds strings from text whose length creator code
//! chooses: a decoded buffer, a percent-encoded URL, a `DOMString` stored as
//! UTF-8 (where one code unit can take three bytes), a message that quotes an
//! argument. Unwrapping that `None` panics inside a V8 callback.
//!
//! [`new`] and [`one_byte`] answer the refusal the way JavaScript itself does,
//! with `RangeError: Invalid string length`; the `_or_throw` forms throw it
//! into the isolate for a callback to return on. [`message`] is for text that
//! only describes something, such as an exception's message: it cannot fail,
//! and it bounds what it keeps, since a message that quotes a creator argument
//! has no use for all of it.

use crate::state::OpError;

/// What JavaScript throws for a string longer than the engine allows.
pub const INVALID_LENGTH: &str = "Invalid string length";

/// The longest text, in bytes, [`message`] turns into a JS string.
const MAX_MESSAGE_BYTES: usize = 16 * 1024;

/// `text` as a V8 string.
///
/// # Errors
///
/// A `RangeError` ("Invalid string length") when V8 refuses text that long.
pub fn new<'s>(
    scope: &v8::PinScope<'s, '_>,
    text: &str,
) -> Result<v8::Local<'s, v8::String>, OpError> {
    v8::String::new(scope, text).ok_or_else(|| OpError::range_error(INVALID_LENGTH))
}

/// `bytes` as a V8 string of Latin-1 code units, the shape of a `ByteString`.
///
/// # Errors
///
/// A `RangeError` ("Invalid string length") when V8 refuses that many bytes.
pub fn one_byte<'s>(
    scope: &v8::PinScope<'s, '_>,
    bytes: &[u8],
) -> Result<v8::Local<'s, v8::String>, OpError> {
    v8::String::new_from_one_byte(scope, bytes, v8::NewStringType::Normal)
        .ok_or_else(|| OpError::range_error(INVALID_LENGTH))
}

/// [`new`] for a callback: on refusal, throws the `RangeError` into `scope`
/// and returns `None`, so the callback returns with the exception pending.
#[must_use]
pub fn new_or_throw<'s>(
    scope: &v8::PinScope<'s, '_>,
    text: &str,
) -> Option<v8::Local<'s, v8::String>> {
    let string = v8::String::new(scope, text);
    if string.is_none() {
        throw_invalid_length(scope);
    }
    string
}

/// [`one_byte`] for a callback: on refusal, throws the `RangeError` into
/// `scope` and returns `None`.
#[must_use]
pub fn one_byte_or_throw<'s>(
    scope: &v8::PinScope<'s, '_>,
    bytes: &[u8],
) -> Option<v8::Local<'s, v8::String>> {
    let string = v8::String::new_from_one_byte(scope, bytes, v8::NewStringType::Normal);
    if string.is_none() {
        throw_invalid_length(scope);
    }
    string
}

/// The `RangeError` JavaScript throws for an over-long string, as a value
/// to throw or reject with.
#[must_use]
pub fn invalid_length_error<'s>(scope: &v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
    let text = message(scope, INVALID_LENGTH);
    v8::Exception::range_error(scope, text)
}

/// Resolve `resolver` with `text` as a string, or reject it with the
/// `RangeError` when V8 refuses text that long.
pub fn resolve_text(
    scope: &v8::PinScope,
    resolver: v8::Local<v8::PromiseResolver>,
    text: &str,
) {
    if let Some(string) = v8::String::new(scope, text) {
        resolver.resolve(scope, string.into());
    } else {
        let error = invalid_length_error(scope);
        resolver.reject(scope, error);
    }
}

fn throw_invalid_length(scope: &v8::PinScope) {
    let error = invalid_length_error(scope);
    scope.throw_exception(error);
}

/// `text` as an exception message or other descriptive string, which may
/// carry creator-supplied text (a URL, a header name, an error message from
/// creator code).
///
/// Never fails: text longer than [`MAX_MESSAGE_BYTES`] is cut there on a
/// character boundary and marked with `...`, which V8 always accepts.
#[must_use]
pub fn message<'s>(scope: &v8::PinScope<'s, '_>, text: &str) -> v8::Local<'s, v8::String> {
    v8::String::new(scope, &bounded(text)).unwrap_or_else(|| v8::String::empty(scope))
}

/// `text` cut to at most [`MAX_MESSAGE_BYTES`] on a character boundary,
/// with `...` when it was cut.
fn bounded(text: &str) -> std::borrow::Cow<'_, str> {
    if text.len() <= MAX_MESSAGE_BYTES {
        return text.into();
    }
    let mut end = MAX_MESSAGE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end]).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Text one byte past V8's limit, as a zeroed buffer the OS has not yet
    /// backed with memory: V8 checks the length before reading any of it.
    fn past_the_limit() -> Vec<u8> {
        vec![0u8; v8::String::MAX_LENGTH + 1]
    }

    fn as_text(bytes: &[u8]) -> &str {
        // SAFETY: zero bytes are valid UTF-8; validating would read, and so
        // back, every page.
        unsafe { std::str::from_utf8_unchecked(bytes) }
    }

    /// Every constructor here refuses text past V8's limit, as a
    /// `RangeError` value or a thrown one, and accepts text at it; a message
    /// past the bound is cut, and one within it is kept whole.
    #[test]
    fn text_past_the_engine_limit_is_a_range_error_or_a_bounded_message() {
        crate::init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handles, &mut isolate);
        let context = v8::Context::new(handles, v8::ContextOptions::default());
        let scope = &mut v8::ContextScope::new(handles, context);
        let bytes = past_the_limit();
        let text = as_text(&bytes);

        assert!(new(scope, "within").is_ok(), "short text is accepted");
        let refused = new(scope, text).expect_err("text past the limit is refused");
        assert!(matches!(refused.kind, crate::state::OpErrorKind::RangeError));
        assert_eq!(refused.message, INVALID_LENGTH);
        assert!(one_byte(scope, b"within").is_ok(), "short bytes are accepted");
        assert!(one_byte(scope, &bytes).is_err(), "bytes past the limit are refused");

        v8::tc_scope!(let tc, scope);
        assert!(new_or_throw(tc, "within").is_some());
        assert!(!tc.has_caught(), "accepted text throws nothing");
        assert!(new_or_throw(tc, text).is_none());
        let thrown = tc.exception().expect("the refusal is thrown");
        assert_eq!(thrown.to_rust_string_lossy(tc), format!("RangeError: {INVALID_LENGTH}"));
        tc.reset();
        assert!(one_byte_or_throw(tc, &bytes).is_none());
        assert!(tc.has_caught(), "the byte refusal is thrown");
        tc.reset();

        let resolver = v8::PromiseResolver::new(tc).expect("resolver");
        resolve_text(tc, resolver, text);
        let promise = resolver.get_promise(tc);
        assert_eq!(promise.state(), v8::PromiseState::Rejected, "an over-long result rejects");
        assert_eq!(promise.result(tc).to_rust_string_lossy(tc), format!("RangeError: {INVALID_LENGTH}"));
        let resolver = v8::PromiseResolver::new(tc).expect("resolver");
        resolve_text(tc, resolver, "kept");
        assert_eq!(resolver.get_promise(tc).result(tc).to_rust_string_lossy(tc), "kept");

        let cut = message(tc, text).to_rust_string_lossy(tc);
        assert_eq!(cut.len(), MAX_MESSAGE_BYTES + 3, "a message past the bound is cut");
        assert_eq!(message(tc, "kept").to_rust_string_lossy(tc), "kept");
    }

    #[test]
    fn bounded_keeps_short_text_and_cuts_long_text_on_a_character_boundary() {
        assert_eq!(bounded("short"), "short");
        let exact = "a".repeat(MAX_MESSAGE_BYTES);
        assert_eq!(bounded(&exact), exact.as_str());

        let long = "a".repeat(MAX_MESSAGE_BYTES * 4);
        let cut = bounded(&long);
        assert_eq!(cut.len(), MAX_MESSAGE_BYTES + 3);
        assert!(cut.ends_with("..."));

        // A three-byte character straddling the limit is dropped whole.
        let straddling = format!("{}{}", "a".repeat(MAX_MESSAGE_BYTES - 1), "\u{20ac}".repeat(4));
        let cut = bounded(&straddling);
        assert_eq!(cut, format!("{}...", "a".repeat(MAX_MESSAGE_BYTES - 1)));
    }
}
