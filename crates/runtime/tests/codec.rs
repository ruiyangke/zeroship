//! Hand-written unit tests for the codec layer that backs
//! CompressionStream / DecompressionStream and internal fetch
//! Content-Encoding handling.
//!
//! See /home/ruiyang/Projects/appbase/docs/proposals/compression-streams-native.md
//! §B (Codec layer) for the trait contract these tests pin down. The
//! tests target the Rust-side `Codec` trait directly; the JS-facing
//! CompressionStream / DecompressionStream V8 classes wait on the
//! native-streams sibling project (per the design's Dependencies
//! section).
//!
//! Spec corner cases covered:
//!
//! - Trailing-byte detection — flate2's finish() does NOT
//!   flag trailing data; the codec write contract returns
//!   (produced, consumed) so the call site can detect truncation.
//! - Idempotent finish/cancel/Drop — repeated finish/cancel
//!   are no-ops; Drop after either is safe.
//! - SAB rejection — exercised in the macro smoke test
//!   crate (this file is codec-only Rust).
//! - Multi-coding chain (gzip → br encode then br → gzip
//!   decode) round-trips, applied in REVERSE per RFC 9110 §8.4.1.
//! - Identity coding is a pass-through; unknown codings yield
//!   CodecError::UnknownCoding.
//! - Empty input through compressor → no header bytes until
//!   finish().
//! - Empty input through decompressor → CodecError::Truncated
//!   on finish() (mapped from flate2's Z_BUF_ERROR).
//! - Lenient deflate (try zlib then raw) for the internal
//!   fetch path; public API stays strict.

use zeroship_runtime::codec::{
    build_codec_chain, make_codec, CodecError, CodecMode, CompressionFormat,
};

// ---------------------------------------------------------------------------
// Round-trip tests — every format encodes then decodes back to itself
// ---------------------------------------------------------------------------

const SAMPLE: &[u8] = b"The quick brown fox jumps over the lazy dog. \
    The quick brown fox jumps over the lazy dog. \
    The quick brown fox jumps over the lazy dog.";

fn roundtrip(format: CompressionFormat) {
    let mut enc = make_codec(format, CodecMode::Compress);
    let (out, consumed) = enc.write(SAMPLE).expect("encode write");
    assert_eq!(consumed, SAMPLE.len(), "encoder must consume all input");
    let mut compressed = out;
    compressed.extend(enc.finish().expect("encode finish"));
    assert!(!compressed.is_empty(), "encoder produced no output");

    let mut dec = make_codec(format, CodecMode::Decompress);
    let (decoded_chunk, decoded_consumed) =
        dec.write(&compressed).expect("decode write");
    assert_eq!(
        decoded_consumed,
        compressed.len(),
        "decoder must consume all of a clean compressed stream"
    );
    let mut decoded = decoded_chunk;
    decoded.extend(dec.finish().expect("decode finish"));
    assert_eq!(decoded, SAMPLE, "round-trip must equal the original");
}

#[test]
fn roundtrip_gzip() {
    roundtrip(CompressionFormat::Gzip);
}

#[test]
fn roundtrip_deflate_zlib() {
    roundtrip(CompressionFormat::Deflate);
}

#[test]
fn roundtrip_deflate_raw() {
    roundtrip(CompressionFormat::DeflateRaw);
}

#[test]
fn roundtrip_brotli() {
    roundtrip(CompressionFormat::Brotli);
}

// ---------------------------------------------------------------------------
// Trailing-byte detection
// ---------------------------------------------------------------------------
//
// Per WHATWG `decompress-and-enqueue` step 6:
//   "If the end of the compressed input has been reached, and ds's
//    context has not fully consumed chunk, then throw a TypeError"
//
// flate2's `finish()` does NOT detect this — its source comment in
// `src/zlib/write.rs:357-358` and `src/gz/write.rs:614-615` says
// "ZlibDecoder consumes one zlib archive and then returns 0 for
// subsequent writes, allowing any additional data to be consumed by
// the caller." So `Codec::write` must return `(Vec<u8>, usize)` and
// the caller has to compare `consumed` against `chunk.len()`.

fn encode_then_append(format: CompressionFormat, suffix: &[u8]) -> Vec<u8> {
    let mut enc = make_codec(format, CodecMode::Compress);
    let (mut out, _) = enc.write(SAMPLE).expect("encode write");
    out.extend(enc.finish().expect("encode finish"));
    out.extend_from_slice(suffix);
    out
}

#[test]
fn gzip_decode_reports_partial_consumed_on_trailing_byte() {
    let with_trailer = encode_then_append(CompressionFormat::Gzip, b"X");
    let mut dec = make_codec(CompressionFormat::Gzip, CodecMode::Decompress);
    let (_decoded, consumed) = dec
        .write(&with_trailer)
        .expect("write should succeed; trailing-byte check is at the call site");
    assert!(
        consumed < with_trailer.len(),
        "trailing byte must show up as partial consumption \
         (consumed={consumed}, total={})",
        with_trailer.len()
    );
}

#[test]
fn deflate_decode_reports_partial_consumed_on_trailing_byte() {
    let with_trailer = encode_then_append(CompressionFormat::Deflate, b"!");
    let mut dec = make_codec(CompressionFormat::Deflate, CodecMode::Decompress);
    let (_decoded, consumed) = dec.write(&with_trailer).expect("write");
    assert!(consumed < with_trailer.len(), "trailing byte must be detectable");
}

#[test]
fn deflate_raw_decode_reports_partial_consumed_on_trailing_byte() {
    let with_trailer = encode_then_append(CompressionFormat::DeflateRaw, b"q");
    let mut dec = make_codec(CompressionFormat::DeflateRaw, CodecMode::Decompress);
    let (_decoded, consumed) = dec.write(&with_trailer).expect("write");
    assert!(consumed < with_trailer.len(), "trailing byte must be detectable");
}

#[test]
fn brotli_decode_reports_partial_consumed_on_trailing_byte() {
    let with_trailer = encode_then_append(CompressionFormat::Brotli, b"$");
    let mut dec = make_codec(CompressionFormat::Brotli, CodecMode::Decompress);
    let (_decoded, consumed) = dec.write(&with_trailer).expect("write");
    assert!(
        consumed < with_trailer.len(),
        "trailing byte must be detectable; brotli writer must report consumed offset"
    );
}

// ---------------------------------------------------------------------------
// Idempotent finish/cancel/Drop
// ---------------------------------------------------------------------------

#[test]
fn double_finish_is_idempotent() {
    let mut enc = make_codec(CompressionFormat::Gzip, CodecMode::Compress);
    let (_, _) = enc.write(SAMPLE).unwrap();
    let _trailer = enc.finish().expect("first finish ok");
    assert!(enc.finished());
    // Second finish is a no-op. We accept `Ok(empty)` and never
    // panic or error here.
    let second = enc.finish().expect("second finish must be no-op Ok");
    assert!(second.is_empty(), "second finish produces no further bytes");
}

#[test]
fn cancel_then_finish_is_noop() {
    let mut enc = make_codec(CompressionFormat::Gzip, CodecMode::Compress);
    let (_, _) = enc.write(SAMPLE).unwrap();
    enc.cancel();
    assert!(enc.finished());
    let after = enc.finish().expect("finish after cancel must be no-op Ok");
    assert!(after.is_empty());
}

#[test]
fn double_cancel_is_idempotent() {
    let mut dec = make_codec(CompressionFormat::Brotli, CodecMode::Decompress);
    dec.cancel();
    dec.cancel();
    // No panic, no double-free. Still finished.
    assert!(dec.finished());
}

#[test]
fn drop_after_cancel_is_safe() {
    let mut dec = make_codec(CompressionFormat::Gzip, CodecMode::Decompress);
    dec.cancel();
    drop(dec);
}

#[test]
fn drop_after_finish_is_safe() {
    let mut enc = make_codec(CompressionFormat::Brotli, CodecMode::Compress);
    let (_, _) = enc.write(SAMPLE).unwrap();
    let _ = enc.finish();
    drop(enc);
}

#[test]
fn write_after_cancel_is_noop() {
    let mut enc = make_codec(CompressionFormat::Gzip, CodecMode::Compress);
    enc.cancel();
    let (out, consumed) = enc.write(SAMPLE).expect("write after cancel is Ok");
    assert!(out.is_empty(), "no output after cancel");
    assert_eq!(consumed, 0, "no input consumed after cancel");
}

// ---------------------------------------------------------------------------
// Empty input through transform produces no header bytes
// ---------------------------------------------------------------------------
//
// Per spec compress-and-enqueue step 3: "If buffer is empty, return."
// `Codec::write(&[])` returns (vec![], 0); header bytes only emerge
// from `finish()`.

#[test]
fn empty_write_produces_no_output() {
    for &fmt in &[
        CompressionFormat::Gzip,
        CompressionFormat::Deflate,
        CompressionFormat::DeflateRaw,
        CompressionFormat::Brotli,
    ] {
        let mut enc = make_codec(fmt, CodecMode::Compress);
        let (out, consumed) = enc.write(&[]).expect("empty write");
        assert_eq!(consumed, 0, "{fmt:?} consumed bytes from empty input");
        assert!(
            out.is_empty(),
            "{fmt:?} emitted header bytes from a transform call (must wait for flush)"
        );
        // After finish() is called, the encoder may emit header+trailer.
        let trailer = enc.finish().expect("finish");
        assert!(
            !trailer.is_empty(),
            "{fmt:?} produced no bytes at all — header should appear at flush"
        );
    }
}

// ---------------------------------------------------------------------------
// Empty input through the decompressor is a truncation
// ---------------------------------------------------------------------------
//
// The codec was constructed but never fed any bytes; stream-end was
// never reached. flate2 surfaces Z_BUF_ERROR which we map to
// `CodecError::Truncated`. That maps to the spec's "throw a TypeError"
// from `decompress-flush-and-enqueue` step 3.

#[test]
fn empty_decompressor_finish_returns_truncated() {
    for &fmt in &[
        CompressionFormat::Gzip,
        CompressionFormat::Deflate,
        CompressionFormat::DeflateRaw,
        CompressionFormat::Brotli,
    ] {
        let mut dec = make_codec(fmt, CodecMode::Decompress);
        let res = dec.finish();
        assert!(
            matches!(res, Err(CodecError::Truncated)),
            "{fmt:?} expected Truncated for empty input, got {res:?}"
        );
    }
}

#[test]
fn truncated_input_finish_returns_truncated() {
    // Encode a sample then chop off the last byte (the gzip CRC/ISIZE
    // trailer or zlib ADLER32 will be incomplete, so finish must
    // detect "end not reached").
    let mut enc = make_codec(CompressionFormat::Gzip, CodecMode::Compress);
    let (mut full, _) = enc.write(SAMPLE).unwrap();
    full.extend(enc.finish().unwrap());
    let truncated = &full[..full.len() - 1];

    let mut dec = make_codec(CompressionFormat::Gzip, CodecMode::Decompress);
    let _ = dec.write(truncated);
    let res = dec.finish();
    assert!(
        matches!(res, Err(CodecError::Truncated)),
        "expected Truncated, got {res:?}"
    );
}

// ---------------------------------------------------------------------------
// deflate vs deflate-raw distinction — public API is strict
// ---------------------------------------------------------------------------
//
// `Codec::deflate` uses zlib wrapper (RFC 1950); `Codec::deflate_raw`
// uses raw DEFLATE (RFC 1951). The strict public-API decoders must
// reject the wrong wire format.

#[test]
fn deflate_raw_decoder_rejects_zlib_wrapped_input() {
    let mut enc = make_codec(CompressionFormat::Deflate, CodecMode::Compress);
    let (mut zlib_bytes, _) = enc.write(SAMPLE).unwrap();
    zlib_bytes.extend(enc.finish().unwrap());

    let mut dec = make_codec(CompressionFormat::DeflateRaw, CodecMode::Decompress);
    // Either write or finish must error — zlib's CMF byte (0x78) is
    // a malformed raw-DEFLATE block header in most cases.
    let write_res = dec.write(&zlib_bytes);
    let finish_res = if write_res.is_ok() { dec.finish() } else { Ok(Vec::new()) };
    assert!(
        write_res.is_err()
            || matches!(
                finish_res,
                Err(CodecError::Truncated) | Err(CodecError::DecodeData(_)) | Err(CodecError::TrailingBytes)
            ),
        "deflate-raw must reject zlib-wrapped input"
    );
}

#[test]
fn deflate_zlib_decoder_rejects_raw_deflate_input() {
    let mut enc = make_codec(CompressionFormat::DeflateRaw, CodecMode::Compress);
    let (mut raw_bytes, _) = enc.write(SAMPLE).unwrap();
    raw_bytes.extend(enc.finish().unwrap());

    let mut dec = make_codec(CompressionFormat::Deflate, CodecMode::Decompress);
    // Strict zlib expects valid CMF/FLG header bytes. The first byte
    // of arbitrary DEFLATE-encoded data won't satisfy the (CMF*256+FLG)
    // mod 31 == 0 invariant in general; the decoder will report a
    // header error during write, or fail to reach stream-end on finish.
    let write_res = dec.write(&raw_bytes);
    let finish_res = if write_res.is_ok() { dec.finish() } else { Ok(Vec::new()) };
    assert!(
        write_res.is_err() || finish_res.is_err(),
        "strict deflate must reject raw-DEFLATE input"
    );
}

// ---------------------------------------------------------------------------
// Multi-coding decode chain runs in reverse
// ---------------------------------------------------------------------------

fn encode_chain(codings: &[CompressionFormat], data: &[u8]) -> Vec<u8> {
    let mut current = data.to_vec();
    for fmt in codings {
        let mut enc = make_codec(*fmt, CodecMode::Compress);
        let (mut out, _) = enc.write(&current).unwrap();
        out.extend(enc.finish().unwrap());
        current = out;
    }
    current
}

fn decode_chain_via_helper(coding_names: &[&str], encoded: &[u8]) -> Vec<u8> {
    let mut chain = build_codec_chain(coding_names, /*decode=*/ true).expect("build chain");
    let mut current = encoded.to_vec();
    for codec in chain.iter_mut() {
        let (mut out, consumed) = codec.write(&current).unwrap();
        assert_eq!(consumed, current.len(), "decoder consumed only {consumed} of {}", current.len());
        out.extend(codec.finish().unwrap());
        current = out;
    }
    current
}

#[test]
fn multi_coding_decode_chain_reverses_encode_order() {
    // Server encodes `gzip, br` → applied gzip first, then br.
    // Per RFC 9110 §8.4.1: "the content codings MUST be listed in the
    // order in which they were applied" — so to decode we go in
    // REVERSE: un-br, then un-gzip.
    let payload = b"Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
                    Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.";
    let encoded = encode_chain(
        &[CompressionFormat::Gzip, CompressionFormat::Brotli],
        payload,
    );

    // Coding header would say "gzip, br". `build_codec_chain` is
    // expected to return decoders in the right order to feed sequentially.
    let decoded = decode_chain_via_helper(&["gzip", "br"], &encoded);
    assert_eq!(decoded, payload);
}

#[test]
fn multi_coding_chain_three_codings() {
    let payload = b"three-step coding chain";
    let encoded = encode_chain(
        &[
            CompressionFormat::Deflate,
            CompressionFormat::Gzip,
            CompressionFormat::Brotli,
        ],
        payload,
    );
    let decoded = decode_chain_via_helper(&["deflate", "gzip", "br"], &encoded);
    assert_eq!(decoded, payload);
}

// ---------------------------------------------------------------------------
// identity, x-gzip aliases, unknown codings
// ---------------------------------------------------------------------------

#[test]
fn identity_coding_is_passthrough() {
    let payload = b"identity passes through unchanged";
    let decoded = decode_chain_via_helper(&["identity"], payload);
    assert_eq!(decoded, payload);
}

#[test]
fn unknown_coding_errors() {
    let res = build_codec_chain(&["zstd"], /*decode=*/ true);
    assert!(matches!(res, Err(CodecError::UnknownCoding(_))));
}

#[test]
fn x_gzip_alias_works() {
    // RFC 9110 lists `x-gzip` as a synonym for `gzip` (some old
    // servers still send it). The chain helper should accept it.
    let payload = b"x-gzip is gzip";
    let encoded = encode_chain(&[CompressionFormat::Gzip], payload);
    let decoded = decode_chain_via_helper(&["x-gzip"], &encoded);
    assert_eq!(decoded, payload);
}

// The lenient deflate fallback is `pub(crate)`; tests for it
// live in `src/codec.rs` `#[cfg(test)] mod internal_tests`.
