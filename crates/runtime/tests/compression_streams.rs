//! End-to-end tests for native `CompressionStream` / `DecompressionStream`.
//!
//! Drives V8 through the runtime's standard test harness. Tests use
//! `pipeThrough` chains rather than `getWriter()` because the native
//! streams' `WritableStreamDefaultWriter.close()` await flow is still
//! converging (separate tracker); pipeThrough exercises identical
//! transform / flush semantics through the readable side.
//!
//! Round-trip + edge-case coverage per
//! `docs/proposals/compression-streams-native.md`'s test plan.

mod common;

use common::{dispatch, m};

/// Drive a chunk of JS that exercises CompressionStream end-to-end and
/// returns "ok" or an error message.
fn run_js(body: &str) -> Result<String, String> {
    let src = format!(
        r#"
export async function run() {{
{body}
    return "ok";
}}
"#
    );
    let out = dispatch(m(&src), "run", "[]")?;
    Ok(out.json)
}

// ---------------------------------------------------------------------------
// Round-trip — every format
// ---------------------------------------------------------------------------

fn roundtrip_body(format: &str) -> String {
    format!(
        r#"
    const enc = new TextEncoder();
    const original = "Hello, server-sent events! ".repeat(40);
    const source = new ReadableStream({{
        start(c) {{ c.enqueue(enc.encode(original)); c.close(); }}
    }});
    const out = await new Response(
        source.pipeThrough(new CompressionStream("{format}"))
              .pipeThrough(new DecompressionStream("{format}"))
    ).text();
    if (out !== original) throw new Error(
        "round-trip mismatch: got " + out.length + " expected " + original.length);
"#,
    )
}

#[test]
fn compression_roundtrip_gzip() {
    let r = run_js(&roundtrip_body("gzip")).expect("gzip round-trip");
    assert_eq!(r, "\"ok\"");
}

#[test]
fn compression_roundtrip_deflate() {
    let r = run_js(&roundtrip_body("deflate")).expect("deflate round-trip");
    assert_eq!(r, "\"ok\"");
}

#[test]
fn compression_roundtrip_deflate_raw() {
    let r = run_js(&roundtrip_body("deflate-raw")).expect("deflate-raw round-trip");
    assert_eq!(r, "\"ok\"");
}

#[test]
fn compression_roundtrip_brotli() {
    let r = run_js(&roundtrip_body("brotli")).expect("brotli round-trip");
    assert_eq!(r, "\"ok\"");
}

// ---------------------------------------------------------------------------
// Constructor errors
// ---------------------------------------------------------------------------

#[test]
fn compression_invalid_format_throws_typeerror() {
    let r = run_js(
        r#"
    let threw = false;
    try {
        new CompressionStream("zstd");
    } catch (e) {
        threw = e instanceof TypeError;
    }
    if (!threw) throw new Error("expected TypeError on unknown format");
"#,
    )
    .expect("invalid format");
    assert_eq!(r, "\"ok\"");
}

#[test]
fn decompression_invalid_format_throws_typeerror() {
    let r = run_js(
        r#"
    let threw = false;
    try {
        new DecompressionStream("not-a-format");
    } catch (e) {
        threw = e instanceof TypeError;
    }
    if (!threw) throw new Error("expected TypeError on unknown format");
"#,
    )
    .expect("invalid format");
    assert_eq!(r, "\"ok\"");
}

// ---------------------------------------------------------------------------
// Empty input
// ---------------------------------------------------------------------------

#[test]
fn compression_empty_input_roundtrips() {
    let r = run_js(
        r#"
    const empty = new ReadableStream({ start(c) { c.close(); } });
    const out = await new Response(
        empty.pipeThrough(new CompressionStream("gzip"))
             .pipeThrough(new DecompressionStream("gzip"))
    ).arrayBuffer();
    if (out.byteLength !== 0) throw new Error(
        "empty round-trip non-empty: " + out.byteLength);
"#,
    )
    .expect("empty input");
    assert_eq!(r, "\"ok\"");
}

// ---------------------------------------------------------------------------
// Streaming many chunks (1 MiB, 1 KiB chunks)
// ---------------------------------------------------------------------------

#[test]
fn compression_streaming_many_chunks() {
    let r = run_js(
        r#"
    const KB = 1024;
    const total = 1024 * KB;
    const chunkSize = KB;
    const source = new ReadableStream({
        start(c) {
            for (let off = 0; off < total; off += chunkSize) {
                const chunk = new Uint8Array(chunkSize);
                for (let i = 0; i < chunkSize; i++) chunk[i] = (off + i) & 0xff;
                c.enqueue(chunk);
            }
            c.close();
        }
    });
    const decompressed = new Uint8Array(await new Response(
        source.pipeThrough(new CompressionStream("gzip"))
              .pipeThrough(new DecompressionStream("gzip"))
    ).arrayBuffer());
    if (decompressed.byteLength !== total)
        throw new Error("size mismatch: " + decompressed.byteLength + " vs " + total);
    for (let i = 0; i < total; i++) {
        if (decompressed[i] !== (i & 0xff)) {
            throw new Error("byte " + i + " mismatch: " + decompressed[i]);
        }
    }
"#,
    )
    .expect("streaming");
    assert_eq!(r, "\"ok\"");
}

// ---------------------------------------------------------------------------
// readable/writable identity + brand
// ---------------------------------------------------------------------------

#[test]
fn compression_readable_writable_identity_preserved() {
    let r = run_js(
        r#"
    const cs = new CompressionStream("gzip");
    if (cs.readable !== cs.readable) throw new Error("readable identity not stable");
    if (cs.writable !== cs.writable) throw new Error("writable identity not stable");
    if (!(cs.readable instanceof ReadableStream)) throw new Error("readable not RS");
    if (!(cs.writable instanceof WritableStream)) throw new Error("writable not WS");
"#,
    )
    .expect("identity");
    assert_eq!(r, "\"ok\"");
}

// ---------------------------------------------------------------------------
// pipeThrough integration (basic shape)
// ---------------------------------------------------------------------------

#[test]
fn compression_pipethrough_chain() {
    let r = run_js(
        r#"
    const enc = new TextEncoder();
    const original = "Server-sent events test payload, ".repeat(20);
    const src = new ReadableStream({
        start(c) { c.enqueue(enc.encode(original)); c.close(); }
    });
    const out = await new Response(
        src.pipeThrough(new CompressionStream("gzip"))
           .pipeThrough(new DecompressionStream("gzip"))
    ).text();
    if (out !== original) throw new Error("pipeThrough chain mismatch");
"#,
    )
    .expect("pipethrough");
    assert_eq!(r, "\"ok\"");
}

// ---------------------------------------------------------------------------
// Decompression of garbage → stream error
// ---------------------------------------------------------------------------

#[test]
fn decompression_invalid_input_errors_stream() {
    let r = run_js(
        r#"
    const garbage = new ReadableStream({
        start(c) {
            c.enqueue(new Uint8Array([0xde, 0xad, 0xbe, 0xef, 0xfe, 0xed, 0xfa, 0xce]));
            c.close();
        }
    });
    let threw = false;
    try {
        await new Response(
            garbage.pipeThrough(new DecompressionStream("gzip"))
        ).arrayBuffer();
    } catch (e) {
        threw = true;
    }
    if (!threw) throw new Error("expected error on garbage input");
"#,
    )
    .expect("garbage decode");
    assert_eq!(r, "\"ok\"");
}
