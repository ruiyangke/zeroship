//! One-shot compress / decompress backends for `node:zlib`.
//!
//! Each fn takes `&[u8]` in, returns `Result<Vec<u8>, String>` out.
//! The synthetic-module callbacks in `mod.rs` thread V8 input/output
//! coercion + Node-style error/codes around these.
//!
//! Backend: `flate2` (miniz_oxide) for gzip / zlib / raw DEFLATE; the
//! `brotli` crate for Brotli. Both already shared with the WHATWG
//! CompressionStream codec layer in `web/codec.rs`.

use std::io::{Read, Write};

use flate2::Compression;
use flate2::read::{DeflateDecoder, GzDecoder, ZlibDecoder};
use flate2::write::{DeflateEncoder, GzEncoder, ZlibEncoder};

/// gzip compress (RFC 1952): zlib-wrapped DEFLATE + 10-byte gzip header
/// + 8-byte CRC/ISIZE trailer. Default level mirrors Node's
/// `Z_DEFAULT_COMPRESSION` (~6).
pub fn gzip(input: &[u8], level: u32) -> Result<Vec<u8>, String> {
    let mut enc = GzEncoder::new(Vec::with_capacity(input.len()), Compression::new(level));
    enc.write_all(input).map_err(|e| e.to_string())?;
    enc.finish().map_err(|e| e.to_string())
}

pub fn gunzip(input: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(input.len() * 2);
    GzDecoder::new(input)
        .read_to_end(&mut out)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// `deflate` (zlib-wrapped, RFC 1950): 2-byte zlib header + DEFLATE
/// + 4-byte Adler-32. This is what Node's `zlib.deflate` produces —
/// distinct from `deflateRaw` which is bare RFC 1951.
pub fn deflate(input: &[u8], level: u32) -> Result<Vec<u8>, String> {
    let mut enc = ZlibEncoder::new(Vec::with_capacity(input.len()), Compression::new(level));
    enc.write_all(input).map_err(|e| e.to_string())?;
    enc.finish().map_err(|e| e.to_string())
}

pub fn inflate(input: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(input.len() * 2);
    ZlibDecoder::new(input)
        .read_to_end(&mut out)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

pub fn deflate_raw(input: &[u8], level: u32) -> Result<Vec<u8>, String> {
    let mut enc = DeflateEncoder::new(Vec::with_capacity(input.len()), Compression::new(level));
    enc.write_all(input).map_err(|e| e.to_string())?;
    enc.finish().map_err(|e| e.to_string())
}

pub fn inflate_raw(input: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(input.len() * 2);
    DeflateDecoder::new(input)
        .read_to_end(&mut out)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// Brotli compress with a sane default quality (4) and window (22). The
/// Node defaults are quality=11 / window=22, but 11 is ~10× slower for
/// marginal ratio gains; matches our `web::codec::BrotliEncoder`.
pub fn brotli_compress(input: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(input.len());
    let mut enc = brotli::CompressorWriter::new(&mut out, 4096, 4, 22);
    enc.write_all(input).map_err(|e| e.to_string())?;
    enc.flush().map_err(|e| e.to_string())?;
    drop(enc);
    Ok(out)
}

pub fn brotli_decompress(input: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(input.len() * 2);
    let mut dec = brotli::DecompressorWriter::new(&mut out, 4096);
    dec.write_all(input).map_err(|e| e.to_string())?;
    dec.flush().map_err(|e| e.to_string())?;
    drop(dec);
    Ok(out)
}
