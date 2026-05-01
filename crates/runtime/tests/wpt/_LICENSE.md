# Vendored Web Platform Tests — encoding subset

These test files are copied verbatim from
https://github.com/web-platform-tests/wpt/tree/master/encoding,
licensed under the WPT 3-Clause-BSD / W3C terms (see
https://github.com/web-platform-tests/wpt/blob/master/LICENSE.md).

Files vendored:
- api-basics.any.js
- api-surrogates-utf8.any.js
- textdecoder-arguments.any.js
- textdecoder-byte-order-marks.any.js
- textdecoder-eof.any.js
- textdecoder-fatal.any.js
- textdecoder-streaming.any.js
- textdecoder-utf16-surrogates.any.js
- encodeInto.any.js
- encodings.js (resources/)

Driven by `tests/wpt_text_encoding.rs`, which provides a minimal
testharness.js shim and runs each file in our V8 isolate. Test names
that match `*[Ss]haredArrayBuffer*` are skipped (we don't ship SAB).
Tests for non-utf-8 encodings (utf-16le, utf-16be, etc.) are skipped
or expected-to-fail since our impl is currently UTF-8 only.
