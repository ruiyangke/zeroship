//! WebIDL `USVString` — Unicode scalar value sequence boundary type.
//!
//! Per https://webidl.spec.whatwg.org/#es-USVString:
//!   1. Let s be ? ToString(V).
//!   2. Return the result of replacing any unmatched surrogate code
//!      unit in s with U+FFFD.
//!
//! Internally a UTF-8 `String` (lone surrogates are replaced before
//! the type even materialises). The `#[v8_class]` macro recognises
//! `USVString` in argument position and converts via
//! [`read_usv_string_or_throw`] before any user method body runs —
//! which means user code receives owned UTF-8 with no remaining V8
//! `Local<Value>` borrows alive.

use crate::state::OpError;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct USVString(pub String);

impl USVString {
    pub fn new() -> Self {
        USVString(String::new())
    }

    pub fn from_string(s: String) -> Self {
        USVString(s)
    }

    pub fn into_string(self) -> String {
        self.0
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for USVString {
    fn from(s: String) -> Self {
        USVString(s)
    }
}

impl From<USVString> for String {
    fn from(u: USVString) -> Self {
        u.0
    }
}

impl AsRef<str> for USVString {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for USVString {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

/// Convert a `v8::Value` to a USVString — the WHATWG WebIDL conversion
/// that replaces unmatched surrogate code units with U+FFFD. Returns
/// `None` only on `Value::to_string` failure (e.g. a Symbol triggers
/// a pending TypeError in V8 which the caller surfaces).
pub fn read_usv_string(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Option<String> {
    let s = value.to_string(scope)?;
    let len = s.length();
    let mut buf = vec![0u16; len];
    s.write_v2(scope, 0, &mut buf, v8::WriteFlags::empty());

    let mut out = String::with_capacity(len);
    let mut i = 0usize;
    while i < buf.len() {
        let cu = buf[i];
        if (0xD800..=0xDBFF).contains(&cu) {
            // High surrogate: needs a low surrogate to follow.
            if i + 1 < buf.len() {
                let next = buf[i + 1];
                if (0xDC00..=0xDFFF).contains(&next) {
                    let hi = (cu as u32) - 0xD800;
                    let lo = (next as u32) - 0xDC00;
                    let cp = 0x10000 + ((hi << 10) | lo);
                    if let Some(c) = char::from_u32(cp) {
                        out.push(c);
                    } else {
                        out.push('\u{FFFD}');
                    }
                    i += 2;
                    continue;
                }
            }
            // Lone high surrogate.
            out.push('\u{FFFD}');
            i += 1;
            continue;
        }
        if (0xDC00..=0xDFFF).contains(&cu) {
            // Lone low surrogate.
            out.push('\u{FFFD}');
            i += 1;
            continue;
        }
        out.push(char::from_u32(cu as u32).unwrap_or('\u{FFFD}'));
        i += 1;
    }
    Some(out)
}

/// Convert a `v8::Value` to a USVString or throw a TypeError. Used by
/// the `#[v8_class]` macro's `USVString` arg extraction.
pub fn read_usv_string_or_throw(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Result<String, OpError> {
    read_usv_string(scope, value)
        .ok_or_else(|| OpError::type_error("Cannot convert value to USVString"))
}
