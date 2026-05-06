//! `node:zlib` — gzip / deflate / deflateRaw / brotli round-trips,
//! sync + async (callback) shapes, error handling. Wave #192.

mod common;
use common::{dispatch, m};

// ---------------------------------------------------------------------------
// Round-trips — sync
// ---------------------------------------------------------------------------

#[test]
fn gzip_round_trip_sync() {
    let r = dispatch(
        m(r#"
        import { gzipSync, gunzipSync } from "node:zlib";
        export function test() {
            const input = "the quick brown fox jumps over the lazy dog";
            const compressed = gzipSync(input);
            const out = gunzipSync(compressed);
            return {
                ok: new TextDecoder().decode(out) === input,
                shrunkOrSame: compressed.length > 0,
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""ok":true"#), "got: {}", r.json);
}

#[test]
fn deflate_round_trip_sync() {
    let r = dispatch(
        m(r#"
        import { deflateSync, inflateSync } from "node:zlib";
        export function test() {
            const input = "abc".repeat(50);
            const compressed = deflateSync(input);
            const out = inflateSync(compressed);
            return { ok: new TextDecoder().decode(out) === input };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""ok":true"#), "got: {}", r.json);
}

#[test]
fn deflate_raw_round_trip_sync() {
    let r = dispatch(
        m(r#"
        import { deflateRawSync, inflateRawSync } from "node:zlib";
        export function test() {
            const input = "raw deflate has no zlib wrapper";
            const compressed = deflateRawSync(input);
            const out = inflateRawSync(compressed);
            // Raw DEFLATE has no zlib wrapper: first byte is the
            // block header, NOT 0x78.
            return {
                ok: new TextDecoder().decode(out) === input,
                noWrapper: compressed[0] !== 0x78,
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""ok":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""noWrapper":true"#), "got: {}", r.json);
}

#[test]
fn brotli_round_trip_sync() {
    let r = dispatch(
        m(r#"
        import { brotliCompressSync, brotliDecompressSync } from "node:zlib";
        export function test() {
            const input = "brotli compresses repetitive text well: " + "x".repeat(200);
            const compressed = brotliCompressSync(input);
            const out = brotliDecompressSync(compressed);
            return {
                ok: new TextDecoder().decode(out) === input,
                shrunk: compressed.length < input.length,
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""ok":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""shrunk":true"#), "got: {}", r.json);
}

// ---------------------------------------------------------------------------
// Empty input
// ---------------------------------------------------------------------------

#[test]
fn empty_gzip_round_trip() {
    let r = dispatch(
        m(r#"
        import { gzipSync, gunzipSync } from "node:zlib";
        export function test() {
            const compressed = gzipSync("");
            const out = gunzipSync(compressed);
            return { len: out.length, gzHasHeader: compressed.length >= 10 };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""len":0"#), "got: {}", r.json);
    assert!(r.json.contains(r#""gzHasHeader":true"#), "got: {}", r.json);
}

#[test]
fn empty_deflate_round_trip() {
    let r = dispatch(
        m(r#"
        import { deflateSync, inflateSync } from "node:zlib";
        export function test() {
            const compressed = deflateSync("");
            const out = inflateSync(compressed);
            return { len: out.length };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""len":0"#), "got: {}", r.json);
}

// ---------------------------------------------------------------------------
// Buffer / Uint8Array input acceptance
// ---------------------------------------------------------------------------

#[test]
fn accepts_uint8array_input() {
    let r = dispatch(
        m(r#"
        import { gzipSync, gunzipSync } from "node:zlib";
        export function test() {
            const enc = new TextEncoder();
            const input = enc.encode("hello world");
            const compressed = gzipSync(input);
            const out = gunzipSync(compressed);
            return { ok: new TextDecoder().decode(out) === "hello world" };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""ok":true"#), "got: {}", r.json);
}

// ---------------------------------------------------------------------------
// Async (callback) variant
// ---------------------------------------------------------------------------

#[test]
fn gzip_async_callback() {
    let r = dispatch(
        m(r#"
        import { gzip, gunzip } from "node:zlib";
        export async function test() {
            const compressed = await new Promise((resolve, reject) => {
                gzip("async hello", (err, out) => {
                    if (err) reject(err);
                    else resolve(out);
                });
            });
            const decompressed = await new Promise((resolve, reject) => {
                gunzip(compressed, (err, out) => {
                    if (err) reject(err);
                    else resolve(out);
                });
            });
            return { ok: new TextDecoder().decode(decompressed) === "async hello" };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""ok":true"#), "got: {}", r.json);
}

#[test]
fn deflate_async_with_options() {
    let r = dispatch(
        m(r#"
        import { deflate, inflate } from "node:zlib";
        export async function test() {
            const compressed = await new Promise((resolve, reject) => {
                deflate("opt level 9", { level: 9 }, (err, out) => {
                    if (err) reject(err);
                    else resolve(out);
                });
            });
            const out = await new Promise((resolve, reject) => {
                inflate(compressed, (err, out) => {
                    if (err) reject(err);
                    else resolve(out);
                });
            });
            return { ok: new TextDecoder().decode(out) === "opt level 9" };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""ok":true"#), "got: {}", r.json);
}

// ---------------------------------------------------------------------------
// Error paths
// ---------------------------------------------------------------------------

#[test]
fn type_error_on_non_buffer_input() {
    let r = dispatch(
        m(r#"
        import { gzipSync } from "node:zlib";
        export function test() {
            try {
                gzipSync(12345);
                return { thrown: false };
            } catch (e) {
                return { thrown: true, code: e.code, name: e.name };
            }
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""thrown":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""code":"ERR_INVALID_ARG_TYPE""#), "got: {}", r.json);
}

#[test]
fn data_error_on_corrupt_gzip() {
    let r = dispatch(
        m(r#"
        import { gunzipSync } from "node:zlib";
        export function test() {
            try {
                // Not a valid gzip stream.
                gunzipSync(new Uint8Array([1, 2, 3, 4, 5]));
                return { thrown: false };
            } catch (e) {
                return { thrown: true };
            }
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""thrown":true"#), "got: {}", r.json);
}

// ---------------------------------------------------------------------------
// Stream constructors throw with a clean error
// ---------------------------------------------------------------------------

#[test]
fn create_gzip_throws_clean_error() {
    let r = dispatch(
        m(r#"
        import { createGzip } from "node:zlib";
        export function test() {
            try {
                createGzip();
                return { thrown: false };
            } catch (e) {
                return { thrown: true, code: e.code, hasHint: e.message.includes("CompressionStream") };
            }
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""thrown":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""code":"ERR_METHOD_NOT_IMPLEMENTED""#), "got: {}", r.json);
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

#[test]
fn constants_exposes_z_default_compression() {
    let r = dispatch(
        m(r#"
        import { constants } from "node:zlib";
        export function test() {
            return {
                zNoFlush: constants.Z_NO_FLUSH,
                zBestCompression: constants.Z_BEST_COMPRESSION,
                zDefaultCompression: constants.Z_DEFAULT_COMPRESSION,
                brotliMaxQuality: constants.BROTLI_MAX_QUALITY,
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""zNoFlush":0"#), "got: {}", r.json);
    assert!(r.json.contains(r#""zBestCompression":9"#), "got: {}", r.json);
    assert!(r.json.contains(r#""zDefaultCompression":-1"#), "got: {}", r.json);
    assert!(r.json.contains(r#""brotliMaxQuality":11"#), "got: {}", r.json);
}

// ---------------------------------------------------------------------------
// Default import returns a namespace object
// ---------------------------------------------------------------------------

#[test]
fn default_import_works() {
    let r = dispatch(
        m(r#"
        import zlib from "node:zlib";
        export function test() {
            return {
                hasGzipSync: typeof zlib.gzipSync === "function",
                hasInflateSync: typeof zlib.inflateSync === "function",
                hasConstants: typeof zlib.constants === "object",
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""hasGzipSync":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasInflateSync":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasConstants":true"#), "got: {}", r.json);
}
