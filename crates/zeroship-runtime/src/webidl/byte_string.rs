//! WebIDL `ByteString` newtype: a sequence of 8-bit code units (bytes
//! 0x00–0xFF). The boundary type used wherever a Web spec asks for a
//! ByteString — Headers names/values, certain URL parts, etc.
//!
//! The `#[v8_class]` macro recognises `ByteString` in argument position
//! and emits a call to [`read_byte_string`] which:
//!
//! 1. Calls `Value::to_string` (per WebIDL §3.2.10 step 1: ToString).
//!    Symbols throw TypeError here naturally per ECMA-262.
//! 2. Checks `String::contains_only_onebyte()` — V8's predicate for
//!    *every* code unit being ≤ 0xFF (step 2 of the algorithm).
//! 3. If the precheck fails, throws TypeError. Otherwise calls
//!    `write_one_byte_v2` to copy the low byte of each code unit.
//!
//! The precheck is essential. `write_one_byte_v2` silently truncates
//! code units > 0xFF (see v8-147.x source string.rs:559-576); calling
//! it without the precheck would let `'\u0100'` pass through as `0x00`,
//! violating WebIDL.
//!
//! Spec: https://webidl.spec.whatwg.org/#js-to-ByteString

use crate::state::OpError;

/// A WebIDL ByteString — a sequence of bytes 0x00–0xFF, distinct from a
/// UTF-8 `String`. Internally a `Vec<u8>`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ByteString(pub Vec<u8>);

impl ByteString {
    pub fn new() -> Self {
        ByteString(Vec::new())
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        ByteString(bytes)
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for ByteString {
    fn from(v: Vec<u8>) -> Self {
        ByteString(v)
    }
}

impl From<ByteString> for Vec<u8> {
    fn from(b: ByteString) -> Self {
        b.0
    }
}

impl AsRef<[u8]> for ByteString {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl std::ops::Deref for ByteString {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

/// Convert a JS `v8::Value` to a WebIDL ByteString. On code units > 0xFF
/// throws TypeError. Used by the `#[v8_class]` macro for `ByteString`-
/// typed arguments; can also be called directly from constructor code
/// when the macro's auto-extract isn't suitable (e.g. iterating over a
/// JS sequence-of-pairs).
pub fn read_byte_string(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Result<Vec<u8>, OpError> {
    // Step 1: ToString. v8::Value::to_string handles ECMA-262 ToString
    // including Symbol → TypeError. None means an exception was thrown
    // (e.g. a Proxy trap raised, or the value was a Symbol). We surface
    // a generic TypeError; if a richer scope-aware error type lands the
    // pending exception can be preserved instead.
    let s = value
        .to_string(scope)
        .ok_or_else(|| OpError::type_error("Cannot convert value to ByteString"))?;

    // Step 2: every code unit must be ≤ 0xFF. V8 stores strings as
    // ONE_BYTE (Latin-1) when all chars are ≤ 0xFF and TWO_BYTE
    // otherwise. The exact predicate is exposed:
    //   v8::String::contains_only_onebyte()
    // which returns true iff every code unit is ≤ 0xFF (including the
    // hidden case where TWO_BYTE storage happens to hold only ≤ 0xFF
    // values — V8 internally optimises this).
    if !s.contains_only_onebyte() {
        return Err(OpError::type_error(
            "String contains code units > 0xFF (ByteString)",
        ));
    }

    // Now safe: every code unit is in 0x00–0xFF. write_one_byte_v2
    // copies the low byte of each code unit. Since all high bytes are
    // 0, this is a faithful Latin-1 → bytes copy.
    let len = s.length();
    let mut buf = vec![0u8; len];
    s.write_one_byte_v2(scope, 0, &mut buf, v8::WriteFlags::empty());
    Ok(buf)
}
