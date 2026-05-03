//! Content-Encoding decompression hook per design §V.9 + RFC 9110 §8.4.1.
//!
//! Implements the `with_response_body_hook` contract that the
//! compression-streams design depends on. The hook runs between
//! "headers parsed" and "Response.body exposed" inside `http_network`:
//!
//!   1. Read `Content-Encoding` from the response headers.
//!   2. Parse as a comma-separated list of codings (RFC 9110 §8.4 ABNF).
//!   3. Build a decoder chain via `codec::build_codec_chain` (which
//!      reverses the order per §8.4.1, since codings were applied in
//!      forward order).
//!   4. Pipe the inbound bytes through the chain.
//!   5. Strip `Content-Encoding` and `Content-Length` from the user-
//!      visible headers (matches undici's `handleResponseBody` and
//!      workerd's `removeContentEncoding` — D-8 of compression design).
//!
//! For the v1 fetch path, we run the chain over the **buffered** body
//! bytes — not as a streaming TransformStream — for two reasons:
//!
//!   1. The cyper backend already buffers each chunk into the body
//!      reader; threading a TransformStream through the response body
//!      adds JS-side allocations and requires the body source pipe
//!      machinery that the proposal calls "TODO from sibling" (and
//!      streaming compression's `pipe_body_through` is not yet
//!      shippable).
//!   2. The vast majority of real-world HTTP responses fit in the
//!      `MAX_RESPONSE_SIZE` cap (10 MiB) the cyper layer enforces. A
//!      streaming version is a future enhancement (D-23 noted).
//!
//! Lenient deflate: per design D-6 / MAJOR-14, we use
//! `try_zlib_then_raw_decode` for the fetch path — public
//! `DecompressionStream("deflate")` stays strict zlib-only.

use crate::codec::{
    build_codec_chain, try_zlib_then_raw_decode, CodecError,
};

/// Outcome of running the Content-Encoding chain over response bytes.
pub struct DecodedBody {
    pub bytes: Vec<u8>,
    /// The new headers list with `Content-Encoding` and `Content-Length`
    /// stripped per D-8.
    pub headers: Vec<(String, String)>,
}

/// Drive the response body through the Content-Encoding chain implied
/// by `headers`. Modifies `headers` to strip `Content-Encoding` and
/// `Content-Length`.
///
/// Returns network error (as `Err(String)`) on:
///   - Unknown coding name (D-9)
///   - Decompression failure (corrupt / truncated / trailing-bytes)
pub fn decompress_response_body(
    body: Vec<u8>,
    headers: &[(String, String)],
) -> Result<DecodedBody, String> {
    // Find Content-Encoding (case-insensitive header lookup).
    let raw_ce: Option<&str> = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-encoding"))
        .map(|(_, v)| v.as_str());

    let codings = match raw_ce {
        Some(v) => parse_content_codings(v),
        None => Vec::new(),
    };

    // Identity / empty list / pure-identity → no decode, but still
    // strip headers per D-8 if Content-Encoding was present.
    let any_real_coding = codings
        .iter()
        .any(|c| !c.eq_ignore_ascii_case("identity"));

    if codings.is_empty() || !any_real_coding {
        let stripped = strip_compression_headers(headers, raw_ce.is_some());
        return Ok(DecodedBody {
            bytes: body,
            headers: stripped,
        });
    }

    // Special-case `deflate` for the fetch path's lenient zlib-or-raw
    // policy (D-6). When the chain is single-element `["deflate"]`,
    // route through `try_zlib_then_raw_decode`. For multi-element
    // chains containing `deflate`, fall through to the strict path —
    // the lenient probe-the-bytes strategy doesn't compose, and
    // multi-coding deflate is exceedingly rare on the wire.
    let bytes = if codings.len() == 1 && codings[0].eq_ignore_ascii_case("deflate") {
        try_zlib_then_raw_decode(&body).map_err(|e| codec_error_to_string(&e))?
    } else {
        // Build a strict chain. The chain builder reverses for decode.
        let coding_refs: Vec<&str> = codings.iter().map(|s| s.as_str()).collect();
        let mut chain =
            build_codec_chain(&coding_refs, /*decode=*/ true).map_err(|e| codec_error_to_string(&e))?;

        // Run `body` through each codec in chain order. Each codec
        // ingests the previous codec's output.
        let mut current = body;
        for codec in chain.iter_mut() {
            let (mut produced, consumed) = codec.write(&current).map_err(|e| codec_error_to_string(&e))?;
            if consumed != current.len() {
                // Trailing bytes after stream end — RFC 9110 §8.4
                // requires the encoder to consume everything.
                return Err("network error: trailing bytes after Content-Encoding stream end".to_string());
            }
            let trailer = codec.finish().map_err(|e| codec_error_to_string(&e))?;
            produced.extend(trailer);
            current = produced;
        }
        current
    };

    Ok(DecodedBody {
        bytes,
        headers: strip_compression_headers(headers, true),
    })
}

/// Parse RFC 9110 §8.4 `Content-Encoding` value as a list of codings.
///
/// Grammar: `1#content-coding`
///   content-coding = token
///   1# = comma-separated list, OWS allowed around commas (§5.6.1).
///
/// We split on `,`, OWS-trim each element, drop empty entries (the spec
/// `1#rule` ABNF tolerates them as the "list extension" clause).
pub fn parse_content_codings(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|s| s.trim_matches(|c: char| c == ' ' || c == '\t').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Remove `Content-Encoding` and `Content-Length` from a headers list.
/// `had_content_encoding` is true iff the original list contained
/// Content-Encoding — the caller already determined this.
fn strip_compression_headers(
    headers: &[(String, String)],
    had_content_encoding: bool,
) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(k, _)| {
            let lk = k.to_ascii_lowercase();
            if had_content_encoding && (lk == "content-encoding" || lk == "content-length") {
                return false;
            }
            true
        })
        .cloned()
        .collect()
}

fn codec_error_to_string(err: &CodecError) -> String {
    format!("network error: {err}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(k: &str, v: &str) -> (String, String) {
        (k.to_string(), v.to_string())
    }

    #[test]
    fn parse_codings_simple() {
        assert_eq!(parse_content_codings("gzip"), vec!["gzip".to_string()]);
    }

    #[test]
    fn parse_codings_multi() {
        assert_eq!(
            parse_content_codings("gzip, br"),
            vec!["gzip".to_string(), "br".to_string()]
        );
    }

    #[test]
    fn parse_codings_with_extra_spaces() {
        assert_eq!(
            parse_content_codings("  gzip  ,  br  "),
            vec!["gzip".to_string(), "br".to_string()]
        );
    }

    #[test]
    fn parse_codings_empty() {
        assert!(parse_content_codings("").is_empty());
    }

    #[test]
    fn no_content_encoding_passes_through() {
        let body = b"hello".to_vec();
        let headers = vec![header("Content-Type", "text/plain")];
        let r = decompress_response_body(body.clone(), &headers).unwrap();
        assert_eq!(r.bytes, body);
        assert_eq!(r.headers.len(), 1);
        assert_eq!(r.headers[0].0, "Content-Type");
    }

    #[test]
    fn identity_only_passes_through() {
        let body = b"hello".to_vec();
        let headers = vec![
            header("Content-Encoding", "identity"),
            header("Content-Length", "5"),
            header("Content-Type", "text/plain"),
        ];
        let r = decompress_response_body(body.clone(), &headers).unwrap();
        assert_eq!(r.bytes, body);
        // Per D-8: when CE was present (even identity), strip CE+CL.
        assert!(r.headers.iter().all(|(k, _)| !k.eq_ignore_ascii_case("content-encoding")));
        assert!(r.headers.iter().all(|(k, _)| !k.eq_ignore_ascii_case("content-length")));
    }

    #[test]
    fn gzip_decodes_round_trip() {
        use crate::codec::{make_codec, CodecMode, CompressionFormat};
        let mut enc = make_codec(CompressionFormat::Gzip, CodecMode::Compress);
        let (mut bytes, _) = enc.write(b"hello world").unwrap();
        bytes.extend(enc.finish().unwrap());
        let headers = vec![
            header("Content-Encoding", "gzip"),
            header("Content-Length", "9999"),
        ];
        let r = decompress_response_body(bytes, &headers).unwrap();
        assert_eq!(r.bytes, b"hello world");
        // CE and CL stripped.
        assert!(r.headers.is_empty());
    }

    #[test]
    fn unknown_coding_errors() {
        let r = decompress_response_body(
            b"x".to_vec(),
            &[header("Content-Encoding", "snappy")],
        );
        assert!(r.is_err());
    }

    #[test]
    fn deflate_lenient_path_accepts_zlib() {
        use crate::codec::{make_codec, CodecMode, CompressionFormat};
        let mut enc = make_codec(CompressionFormat::Deflate, CodecMode::Compress);
        let (mut bytes, _) = enc.write(b"hello world").unwrap();
        bytes.extend(enc.finish().unwrap());
        let headers = vec![header("Content-Encoding", "deflate")];
        let r = decompress_response_body(bytes, &headers).unwrap();
        assert_eq!(r.bytes, b"hello world");
    }

    #[test]
    fn deflate_lenient_path_accepts_raw() {
        use crate::codec::{make_codec, CodecMode, CompressionFormat};
        let mut enc = make_codec(CompressionFormat::DeflateRaw, CodecMode::Compress);
        let (mut bytes, _) = enc.write(b"hello world").unwrap();
        bytes.extend(enc.finish().unwrap());
        let headers = vec![header("Content-Encoding", "deflate")];
        let r = decompress_response_body(bytes, &headers).unwrap();
        assert_eq!(r.bytes, b"hello world");
    }

    #[test]
    fn multi_coding_gzip_then_br_decodes_in_reverse() {
        // Server applied: gzip first, then brotli.
        // Wire bytes: br(gzip(plaintext))
        // Header: "Content-Encoding: gzip, br"
        // RFC 9110 §8.4.1: decode in reverse → unbrotli, then ungzip.
        use crate::codec::{make_codec, CodecMode, CompressionFormat};

        // Apply gzip first.
        let mut g = make_codec(CompressionFormat::Gzip, CodecMode::Compress);
        let (mut b1, _) = g.write(b"hello world").unwrap();
        b1.extend(g.finish().unwrap());

        // Then apply brotli over gzip's output.
        let mut br = make_codec(CompressionFormat::Brotli, CodecMode::Compress);
        let (mut b2, _) = br.write(&b1).unwrap();
        b2.extend(br.finish().unwrap());

        let headers = vec![header("Content-Encoding", "gzip, br")];
        let r = decompress_response_body(b2, &headers).unwrap();
        assert_eq!(r.bytes, b"hello world");
    }
}
