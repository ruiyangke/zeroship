//! RESP2 framing helpers built on the `redis-protocol` crate.
//!
//! Thin convenience layer: build an `Array-of-BulkString` command frame,
//! serialize into a `Vec<u8>`, decode a single reply from an accumulating
//! buffer. The `redis-protocol` crate handles correctness; we just wrap
//! its API to the shapes our client needs.

use bytes::BytesMut;
use redis_protocol::resp2::{
    decode,
    encode::encode,
    types::{OwnedFrame, Resp2Frame},
};

use crate::error::{Error, Result};

/// Test-only observation point for the cost of full-frame parsing.
///
/// The O(N^2) decode this module exists to avoid is invisible to a
/// correctness assertion: the old and new code produce identical frames,
/// they just differ in how many bytes the parser walks. This records that
/// directly (parse attempts and bytes handed to the parser) so the
/// regression test can assert a *structural* bound instead of a wall-clock
/// one.
///
/// This is `#[cfg(test)]` machinery, and it is not free of judgement: it
/// adds one thread-local increment inside [`try_decode`]. That is a per-
/// *reply-parse* cost (at most one per socket read), not a per-byte cost —
/// the byte loop it is measuring lives inside `redis-protocol` and is
/// untouched — and it does not exist at all in a non-test build.
#[cfg(test)]
pub mod decode_probe {
    use std::cell::Cell;

    thread_local! {
        static PASSES: Cell<u64> = const { Cell::new(0) };
        static BYTES: Cell<u64> = const { Cell::new(0) };
    }

    pub(super) fn record(len: usize) {
        PASSES.with(|p| p.set(p.get() + 1));
        BYTES.with(|b| b.set(b.get() + len as u64));
    }

    /// Zero both counters.
    pub fn reset() {
        PASSES.with(|p| p.set(0));
        BYTES.with(|b| b.set(0));
    }

    /// `(full parse attempts, total bytes handed to the parser)` since the
    /// last [`reset`].
    pub fn snapshot() -> (u64, u64) {
        (PASSES.with(Cell::get), BYTES.with(Cell::get))
    }
}

/// Build a command frame from its parts. Every arg becomes a BulkString.
///
/// `build_cmd(&["SET", key, value, "PX", "1000"])` → the wire equivalent of
/// `*5\r\n$3\r\nSET\r\n...`.
pub fn build_cmd(parts: &[&[u8]]) -> OwnedFrame {
    OwnedFrame::Array(
        parts
            .iter()
            .map(|p| OwnedFrame::BulkString(p.to_vec()))
            .collect(),
    )
}

/// Serialize a frame into a fresh buffer. Sizes the buffer via
/// `Resp2Frame::encode_len()` so we allocate exactly once.
pub fn encode_frame(frame: &OwnedFrame) -> Result<Vec<u8>> {
    let len = frame.encode_len(false);
    let mut buf = vec![0u8; len];
    encode(&mut buf, frame, false)
        .map_err(|e| Error::Protocol(format!("encode: {e}")))?;
    Ok(buf)
}

/// Try to decode one complete reply from the accumulated buffer.
///
/// Returns `Some((frame, consumed))` when a full reply is in the buffer,
/// `None` if the reply is still partial. The caller slices off `consumed`
/// bytes and keeps the remainder for the next decode.
pub fn try_decode(buf: &[u8]) -> Result<Option<(OwnedFrame, usize)>> {
    #[cfg(test)]
    decode_probe::record(buf.len());
    decode::decode(buf).map_err(|e| Error::Protocol(format!("decode: {e}")))
}

// ---------------------------------------------------------------------------
// Incremental reply decoding
// ---------------------------------------------------------------------------

/// Longest `<len>` field we accept after a `$`/`*` kind byte, in bytes.
/// RESP lengths are decimal and this client caps replies at tens of MB, so
/// 24 digits is far past generous — the bound exists so a server that never
/// terminates a length header is rejected instead of buffered.
const MAX_LEN_HEADER: usize = 24;

/// Per-connection state for pulling one reply at a time off an accumulating
/// read buffer.
///
/// # Why this is not just [`try_decode`]
///
/// [`try_decode`] is a stateless parse from byte 0, and the client reads in
/// 4 KiB chunks. Calling it after every read walks an N-byte reply
/// `c = ceil(N / 4096)` times over a buffer that grows each round —
/// `~N*c/2` bytes examined, i.e. O(N^2) in element count for aggregate
/// replies (SCAN, MGET). compio is single-threaded per worker, so that time
/// is stolen from every other request on the thread.
///
/// # How this avoids it
///
/// The decoder keeps a *resume position* across reads and only measures the
/// frame's extent — it never builds values. Each call advances from where
/// the last one stopped:
///
/// * `offset` is the start of the next element whose header is unparsed. A
///   bulk string's body is skipped wholesale (`offset += len + 2`) and is
///   never scanned at all.
/// * `probe` is how far the CRLF search for the element *at* `offset` has
///   already looked, so even a line split across many reads is scanned
///   once. It is set to `buf.len() - 1`, not `buf.len()`, so a `\r` sitting
///   at the end of one read still pairs with the `\n` that opens the next.
/// * `need` is how many elements are still outstanding. An atom costs one;
///   an N-element array costs one and owes N more. Nesting needs no stack:
///   RESP2 aggregates are strictly prefix-ordered, so a flat count is exact.
///
/// When `need` hits zero the frame is known-complete, and only then is
/// `try_decode` called — once, on exactly those bytes. `redis-protocol`
/// remains the authority on what the bytes *mean*; this type only decides
/// when there are enough of them.
#[derive(Debug)]
pub struct ReplyDecoder {
    /// Ceiling on one reply, in bytes. Also bounds the outstanding element
    /// count: every wire element is at least one byte, so an aggregate
    /// declaring more elements than this cannot fit under the cap.
    max_reply_size: usize,
    offset: usize,
    probe: usize,
    need: usize,
}

impl ReplyDecoder {
    #[must_use]
    pub const fn new(max_reply_size: usize) -> Self {
        Self { max_reply_size, offset: 0, probe: 0, need: 1 }
    }

    /// Try to take one complete reply off the front of `buf`.
    ///
    /// Returns `Ok(None)` when the reply is still partial — the caller reads
    /// more bytes onto the end of `buf` and calls again, and the scan
    /// resumes rather than restarting. On success the frame's bytes have
    /// been split off the front of `buf`; anything after it (a pipelined
    /// reply) is left in place and the decoder is reset for it.
    ///
    /// # Errors
    ///
    /// [`Error::Protocol`] if the bytes are not valid RESP2, if a declared
    /// length or element count would exceed `max_reply_size`, or if the
    /// scan and `redis-protocol` disagree about framing.
    pub fn take_frame(&mut self, buf: &mut BytesMut) -> Result<Option<OwnedFrame>> {
        let Some(extent) = self.scan(buf)? else {
            return Ok(None);
        };
        let Some((frame, consumed)) = try_decode(&buf[..extent])? else {
            // Unreachable unless `scan` and `redis-protocol` disagree about
            // framing. Fail loudly rather than spin: the caller would
            // otherwise re-read forever against a frame it already has.
            return Err(Error::Protocol(format!(
                "decode: framing desync — scanned a complete {extent}-byte frame \
                 that the parser reads as partial",
            )));
        };
        let _ = buf.split_to(consumed);
        self.offset = 0;
        self.probe = 0;
        self.need = 1;
        Ok(Some(frame))
    }

    /// Advance the scan over whatever is now in `buf`, returning the byte
    /// length of the complete frame at its front, or `None` if more bytes
    /// are needed.
    fn scan(&mut self, buf: &[u8]) -> Result<Option<usize>> {
        loop {
            if self.need == 0 {
                // Every element is accounted for, but the LAST one may have
                // been a bulk string whose body we skipped past the end of
                // what has arrived — `offset` is a promise, not a fact,
                // until the buffer actually reaches it.
                return Ok((self.offset <= buf.len()).then_some(self.offset));
            }
            if self.offset >= buf.len() {
                return Ok(None);
            }
            let kind = buf[self.offset];
            // Mirrors `redis_protocol::resp2`'s accepted kind bytes: anything
            // else is a protocol error there too, so reject it now instead of
            // buffering until the size cap.
            if !matches!(kind, b'+' | b'-' | b':' | b'$' | b'*') {
                return Err(Error::Protocol(format!(
                    "decode: invalid frame type byte 0x{kind:02x} at offset {}",
                    self.offset,
                )));
            }

            let from = self.probe.max(self.offset + 1);
            let Some(crlf) = find_crlf(buf, from) else {
                // Terminator hasn't arrived. Park the search watermark on the
                // last byte we looked at — a lone trailing '\r' must be
                // re-examined next time, everything before it must not.
                self.probe = from.max(buf.len().saturating_sub(1));
                if matches!(kind, b'$' | b'*')
                    && buf.len() - self.offset > MAX_LEN_HEADER + 1
                {
                    return Err(Error::Protocol(format!(
                        "decode: unterminated length header at offset {}",
                        self.offset,
                    )));
                }
                return Ok(None);
            };

            let after = crlf + 2;
            self.need -= 1;
            match kind {
                // Line-shaped frames end at the CRLF.
                b'+' | b'-' | b':' => self.offset = after,
                b'$' => {
                    // `Some(len)`: body + trailing CRLF, skipped without ever
                    // being scanned. `None`: a RESP null (`$-1`), where
                    // nothing follows the header.
                    self.offset = self
                        .parse_len(&buf[self.offset + 1..crlf])?
                        .map_or(after, |len| after + len + 2);
                }
                _ => {
                    if let Some(len) = self.parse_len(&buf[self.offset + 1..crlf])? {
                        self.need = self.need.saturating_add(len);
                        if self.need > self.max_reply_size {
                            return Err(Error::Protocol(format!(
                                "decode: reply too large: {} outstanding elements \
                                 (max {})",
                                self.need, self.max_reply_size,
                            )));
                        }
                    }
                    self.offset = after;
                }
            }
            // Everything up to `offset` is settled; never look at it again.
            self.probe = self.offset;
        }
    }

    /// Parse a `$`/`*` length field, enforcing the reply cap up front so a
    /// declared-but-unsent multi-GB body is rejected before it is buffered.
    ///
    /// `Ok(None)` is a negative length. `-1` is RESP's null; any other
    /// negative is malformed, and we let `redis-protocol` be the one to say
    /// so rather than forming a second opinion here — either way nothing
    /// follows the header, so the extent is the same.
    fn parse_len(&self, digits: &[u8]) -> Result<Option<usize>> {
        if digits.len() > MAX_LEN_HEADER {
            return Err(Error::Protocol(format!(
                "decode: length field of {} bytes is not a RESP length",
                digits.len(),
            )));
        }
        let text = std::str::from_utf8(digits)
            .map_err(|_| Error::Protocol("decode: non-ASCII length field".into()))?;
        let len: i64 = text
            .parse()
            .map_err(|_| Error::Protocol(format!("decode: bad length field {text:?}")))?;
        if len < 0 {
            return Ok(None);
        }
        let len = usize::try_from(len)
            .map_err(|_| Error::Protocol(format!("decode: length {len} does not fit in usize")))?;
        if len > self.max_reply_size {
            return Err(Error::Protocol(format!(
                "decode: reply too large: declared {len} (max {})",
                self.max_reply_size,
            )));
        }
        Ok(Some(len))
    }
}

/// Index of the first `\r\n` at or after `from`. A bare `\r` is not a
/// terminator — `redis-protocol` reads to the two-byte CRLF as well.
fn find_crlf(buf: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 1 < buf.len() {
        if buf[i] == b'\r' && buf[i + 1] == b'\n' {
            return Some(i);
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Reply-shape extractors — turn a frame into the Rust type the caller wants,
// or return Error::Unexpected with a clear message.
// ---------------------------------------------------------------------------

pub fn expect_ok(frame: OwnedFrame) -> Result<()> {
    match frame {
        OwnedFrame::SimpleString(s) if s == b"OK" => Ok(()),
        OwnedFrame::Error(msg) => Err(Error::Server(msg)),
        other => Err(Error::Unexpected(format!("expected OK, got {:?}", frame_kind(&other)))),
    }
}

pub fn expect_integer(frame: OwnedFrame) -> Result<i64> {
    match frame {
        OwnedFrame::Integer(i) => Ok(i),
        OwnedFrame::Error(msg) => Err(Error::Server(msg)),
        other => Err(Error::Unexpected(format!("expected integer, got {:?}", frame_kind(&other)))),
    }
}

pub fn expect_bulk_or_null(frame: OwnedFrame) -> Result<Option<Vec<u8>>> {
    match frame {
        OwnedFrame::BulkString(b) => Ok(Some(b)),
        OwnedFrame::SimpleString(s) => Ok(Some(s)),
        OwnedFrame::Null => Ok(None),
        OwnedFrame::Error(msg) => Err(Error::Server(msg)),
        other => Err(Error::Unexpected(format!(
            "expected bulk/simple/null, got {:?}",
            frame_kind(&other)
        ))),
    }
}

pub fn expect_array(frame: OwnedFrame) -> Result<Vec<OwnedFrame>> {
    match frame {
        OwnedFrame::Array(items) => Ok(items),
        OwnedFrame::Null => Ok(Vec::new()),
        OwnedFrame::Error(msg) => Err(Error::Server(msg)),
        other => Err(Error::Unexpected(format!(
            "expected array, got {:?}",
            frame_kind(&other)
        ))),
    }
}

fn frame_kind(f: &OwnedFrame) -> &'static str {
    match f {
        OwnedFrame::SimpleString(_) => "SimpleString",
        OwnedFrame::Error(_) => "Error",
        OwnedFrame::Integer(_) => "Integer",
        OwnedFrame::BulkString(_) => "BulkString",
        OwnedFrame::Array(_) => "Array",
        OwnedFrame::Null => "Null",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_and_encode_simple_set() {
        let cmd = build_cmd(&[b"SET", b"k", b"v"]);
        let wire = encode_frame(&cmd).unwrap();
        assert_eq!(wire, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n");
    }

    #[test]
    fn decode_roundtrip_integer() {
        // Redis would reply ":42\r\n" for e.g. INCR
        let (frame, consumed) = try_decode(b":42\r\n").unwrap().unwrap();
        assert_eq!(consumed, 5);
        assert_eq!(expect_integer(frame).unwrap(), 42);
    }

    #[test]
    fn decode_partial_returns_none() {
        // Half a bulk string — decoder must not panic or claim success
        let out = try_decode(b"$5\r\nhel").unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn decode_null() {
        let (frame, _) = try_decode(b"$-1\r\n").unwrap().unwrap();
        assert!(expect_bulk_or_null(frame).unwrap().is_none());
    }

    #[test]
    fn decode_error_surfaces_as_server_error() {
        let (frame, _) = try_decode(b"-ERR wrong type\r\n").unwrap().unwrap();
        let err = expect_ok(frame).unwrap_err();
        match err {
            Error::Server(msg) => assert!(msg.contains("wrong type")),
            other => panic!("expected Server, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Incremental decode: correctness across chunk boundaries
    //
    // The client feeds `ReplyDecoder` whatever a single socket read
    // returned, so a frame can be cut at ANY byte — mid-header, mid-length
    // prefix, between the `\r` and the `\n`, or one byte into a bulk body.
    // The invariant these tests pin: for any chunking of a payload, the
    // decoder must yield exactly the frame `try_decode` yields for the whole
    // payload, and must leave exactly the trailing bytes untouched.
    // -----------------------------------------------------------------

    const TEST_MAX: usize = 64 * 1024 * 1024;

    /// Feed `chunks` to a fresh decoder in order, returning the first frame
    /// it produced (if any) and whatever bytes were left in the buffer.
    fn drive<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> Result<(Option<OwnedFrame>, Vec<u8>)> {
        let mut dec = ReplyDecoder::new(TEST_MAX);
        let mut buf = BytesMut::new();
        let mut frame = None;
        for c in chunks {
            buf.extend_from_slice(c);
            if frame.is_none()
                && let Some(f) = dec.take_frame(&mut buf)?
            {
                frame = Some(f);
            }
        }
        Ok((frame, buf.to_vec()))
    }

    /// The payloads every chunking test is run against. Each one targets a
    /// specific place a split can land.
    fn corpus() -> Vec<Vec<u8>> {
        let long_body = "0123456789".repeat(10); // 100 bytes -> multi-digit length
        let mut out: Vec<Vec<u8>> = vec![
            b"+OK\r\n".as_slice(),                        // simple string
            b"-ERR wrong type\r\n",                       // error line
            b":42\r\n",                                   // integer
            b":-1\r\n",                                   // negative integer (not a null)
            b"$0\r\n\r\n",                                // empty bulk: body is zero bytes
            b"$-1\r\n",                                   // null bulk
            b"$5\r\nhello\r\n",                           // ordinary bulk
            b"$4\r\n\r\n\r\n\r\n",                        // bulk body that IS two CRLFs
            b"$6\r\nab\r\ncd\r\n",                        // CRLF in the middle of a body
            b"$10\r\n$5\r\nnested\r\n",                   // body that looks like a frame
            b"*-1\r\n",                                   // null array
            b"*0\r\n",                                    // empty array
            b"*2\r\n$1\r\n0\r\n*2\r\n$3\r\nfoo\r\n$3\r\nbar\r\n", // SCAN shape
            b"*3\r\n:1\r\n*2\r\n+a\r\n$-1\r\n$2\r\nhi\r\n",       // mixed + nested + null
            b"*1\r\n*1\r\n*1\r\n$1\r\nz\r\n",             // deep nesting
            b"*3\r\n$3\r\nfoo\r\n$-1\r\n$3\r\nbar\r\n",   // MGET with a hole
        ]
        .into_iter()
        .map(|p: &[u8]| p.to_vec())
        .collect();
        // Multi-digit length prefix: a split can land between two digits.
        out.push(format!("${}\r\n{long_body}\r\n", long_body.len()).into_bytes());
        out
    }

    #[test]
    fn every_two_way_split_decodes_identically() {
        for payload in corpus() {
            let payload = &payload[..];
            let (want, want_consumed) = try_decode(payload)
                .unwrap()
                .unwrap_or_else(|| panic!("corpus entry is not a complete frame: {payload:?}"));
            assert_eq!(want_consumed, payload.len(), "corpus entry has trailing bytes: {payload:?}");

            for split in 0..=payload.len() {
                let (got, rest) = drive([&payload[..split], &payload[split..]])
                    .unwrap_or_else(|e| panic!("split {split} of {payload:?} errored: {e}"));
                assert_eq!(
                    got.as_ref(),
                    Some(&want),
                    "split at {split} of {payload:?} decoded differently"
                );
                assert!(rest.is_empty(), "split at {split} of {payload:?} left {rest:?}");
            }
        }
    }

    #[test]
    fn every_three_way_split_decodes_identically() {
        for payload in corpus() {
            let payload = &payload[..];
            let (want, _) = try_decode(payload).unwrap().unwrap();
            for a in 0..=payload.len() {
                for b in a..=payload.len() {
                    let (got, rest) = drive([&payload[..a], &payload[a..b], &payload[b..]])
                        .unwrap_or_else(|e| panic!("splits {a}/{b} of {payload:?} errored: {e}"));
                    assert_eq!(
                        got.as_ref(),
                        Some(&want),
                        "splits {a}/{b} of {payload:?} decoded differently"
                    );
                    assert!(rest.is_empty(), "splits {a}/{b} of {payload:?} left {rest:?}");
                }
            }
        }
    }

    #[test]
    fn byte_at_a_time_decodes_identically() {
        for payload in corpus() {
            let payload = &payload[..];
            let (want, _) = try_decode(payload).unwrap().unwrap();
            let (got, rest) = drive(payload.chunks(1))
                .unwrap_or_else(|e| panic!("1-byte chunking of {payload:?} errored: {e}"));
            assert_eq!(got.as_ref(), Some(&want), "1-byte chunking of {payload:?} decoded differently");
            assert!(rest.is_empty());
        }
    }

    /// Exhaustive: EVERY chunking (all 2^(n-1) ways of cutting the payload)
    /// of a few short frames. The two- and three-way split tests sample this
    /// space; for payloads this small we can cover it outright.
    #[test]
    fn every_possible_chunking_of_short_frames() {
        let payloads: [&[u8]; 6] = [
            b"$0\r\n\r\n",           // empty body: extent runs one CRLF past the header
            b"$-1\r\n",              // null
            b"*0\r\n",               // empty aggregate
            b"*1\r\n$1\r\nz\r\n",    // aggregate + bulk
            b"$4\r\n\r\n\r\n\r\n",   // body that is nothing but CRLFs
            b"*2\r\n:1\r\n+ab\r\n",  // two line-shaped elements
        ];
        for payload in payloads {
            assert!(payload.len() <= 16, "keep the 2^n enumeration small");
            let (want, _) = try_decode(payload).unwrap().unwrap();
            let cuts = payload.len() - 1; // candidate split points
            for mask in 0u32..(1 << cuts) {
                let mut chunks: Vec<&[u8]> = Vec::new();
                let mut start = 0;
                for (bit, at) in (1..payload.len()).enumerate() {
                    if mask & (1 << bit) != 0 {
                        chunks.push(&payload[start..at]);
                        start = at;
                    }
                }
                chunks.push(&payload[start..]);
                let (got, rest) = drive(chunks.iter().copied())
                    .unwrap_or_else(|e| panic!("chunking {mask:#b} of {payload:?} errored: {e}"));
                assert_eq!(
                    got.as_ref(),
                    Some(&want),
                    "chunking {mask:#b} of {payload:?} decoded differently"
                );
                assert!(rest.is_empty(), "chunking {mask:#b} of {payload:?} left {rest:?}");
            }
        }
    }

    /// A realistic multi-read reply cut at sizes that land headers, length
    /// prefixes and CRLFs at every alignment relative to a read boundary.
    #[test]
    fn large_scan_reply_survives_awkward_chunk_sizes() {
        let payload = scan_reply(64); // ~33 KiB
        let (want, _) = try_decode(&payload).unwrap().unwrap();
        for chunk in [1, 2, 3, 5, 7, 13, 511, 512, 513, 4095, 4096, 4097] {
            let (got, rest) = drive(payload.chunks(chunk))
                .unwrap_or_else(|e| panic!("chunk size {chunk} errored: {e}"));
            assert_eq!(got.as_ref(), Some(&want), "chunk size {chunk} decoded differently");
            assert!(rest.is_empty(), "chunk size {chunk} left {} bytes", rest.len());
        }
    }

    /// A line-shaped frame (no length prefix) far longer than one read: the
    /// CRLF search has to resume across reads instead of restarting, and it
    /// must still pair a `\r` that ends one read with the `\n` that opens
    /// the next.
    #[test]
    fn long_line_frame_split_across_many_reads() {
        let mut payload = b"+".to_vec();
        payload.extend(std::iter::repeat_n(b'x', 20_000));
        payload.extend_from_slice(b"\r\n");
        let (want, _) = try_decode(&payload).unwrap().unwrap();
        for chunk in [1, 4096, payload.len() - 1] {
            let (got, rest) = drive(payload.chunks(chunk))
                .unwrap_or_else(|e| panic!("chunk size {chunk} errored: {e}"));
            assert_eq!(got.as_ref(), Some(&want), "chunk size {chunk} decoded differently");
            assert!(rest.is_empty());
        }
    }

    #[test]
    fn partial_frame_never_yields_a_frame() {
        for payload in corpus() {
            let payload = &payload[..];
            for prefix in 0..payload.len() {
                let (got, _) = drive([&payload[..prefix]])
                    .unwrap_or_else(|e| panic!("prefix {prefix} of {payload:?} errored: {e}"));
                assert!(
                    got.is_none(),
                    "prefix of {prefix}/{} bytes of {payload:?} decoded as a whole frame",
                    payload.len()
                );
            }
        }
    }

    #[test]
    fn trailing_bytes_are_left_for_the_next_reply() {
        // Two replies arriving in one read: take_frame must return only the
        // first and leave the second's bytes intact, then decode the second.
        let first: &[u8] = b"*2\r\n$3\r\nfoo\r\n$3\r\nbar\r\n";
        let second: &[u8] = b"+OK\r\n";
        let mut wire = BytesMut::new();
        wire.extend_from_slice(first);
        wire.extend_from_slice(second);

        let mut dec = ReplyDecoder::new(TEST_MAX);
        let f1 = dec.take_frame(&mut wire).unwrap().expect("first frame");
        assert_eq!(f1, try_decode(first).unwrap().unwrap().0);
        assert_eq!(&wire[..], second, "second reply's bytes must be untouched");

        let f2 = dec.take_frame(&mut wire).unwrap().expect("second frame");
        assert_eq!(f2, try_decode(second).unwrap().unwrap().0);
        assert!(wire.is_empty());
    }

    #[test]
    fn oversized_nested_bulk_header_is_rejected_without_buffering() {
        // The client's front-of-buffer peek only sees the OUTER header, so a
        // nested oversized declaration has to be caught by the decoder.
        let mut buf = BytesMut::from(&b"*1\r\n$2000000000\r\n"[..]);
        let mut dec = ReplyDecoder::new(TEST_MAX);
        let err = dec
            .take_frame(&mut buf)
            .expect_err("nested 2 GB bulk header must be rejected");
        assert!(err.to_string().contains("too large"), "got: {err}");
    }

    #[test]
    fn oversized_nested_array_header_is_rejected_without_buffering() {
        let mut buf = BytesMut::from(&b"*1\r\n*2000000000\r\n"[..]);
        let mut dec = ReplyDecoder::new(TEST_MAX);
        let err = dec
            .take_frame(&mut buf)
            .expect_err("nested 2 G-element array header must be rejected");
        assert!(err.to_string().contains("too large"), "got: {err}");
    }

    #[test]
    fn invalid_frame_type_errors_instead_of_buffering_forever() {
        let mut buf = BytesMut::from(&b"#not-resp\r\n"[..]);
        let mut dec = ReplyDecoder::new(TEST_MAX);
        assert!(dec.take_frame(&mut buf).is_err(), "garbage kind byte must error");
    }

    // -----------------------------------------------------------------
    // Incremental decode: COST
    //
    // Correctness cannot see the defect this shape exists to fix — the
    // naive "re-parse the whole buffer after every read" decoder produces
    // exactly the same frames, it just walks ~N*chunks/2 bytes doing it.
    // So we assert on the parse work itself (see `decode_probe`), never on
    // elapsed time: a wall-clock assertion on a shared machine is flaky by
    // construction.
    // -----------------------------------------------------------------

    /// A SCAN-shaped reply: `[cursor, [key; n]]` with 512-byte keys, i.e.
    /// exactly the shape `zeroship-kv` produces at `LIST_MAX_LIMIT`.
    fn scan_reply(keys: usize) -> Vec<u8> {
        let key = "k".repeat(512);
        let mut out = Vec::with_capacity(keys * 525 + 32);
        out.extend_from_slice(b"*2\r\n$1\r\n0\r\n");
        out.extend_from_slice(format!("*{keys}\r\n").as_bytes());
        for _ in 0..keys {
            out.extend_from_slice(b"$512\r\n");
            out.extend_from_slice(key.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out
    }

    /// Replay `payload` through the decoder in `chunk`-sized reads, exactly
    /// as the client's read loop does, and report `(full parse attempts,
    /// bytes handed to the parser)`.
    fn decode_cost(payload: &[u8], chunk: usize) -> (u64, u64) {
        decode_probe::reset();
        let mut dec = ReplyDecoder::new(TEST_MAX);
        let mut buf = BytesMut::new();
        let mut got = false;
        for c in payload.chunks(chunk) {
            buf.extend_from_slice(c);
            if dec.take_frame(&mut buf).expect("decode").is_some() {
                got = true;
            }
        }
        assert!(got, "payload did not decode");
        decode_probe::snapshot()
    }

    #[test]
    fn aggregate_reply_decode_cost_is_linear_not_quadratic() {
        // `client.rs`'s READ_CHUNK — the read size that makes the naive
        // decoder quadratic.
        const CHUNK: usize = 4096;

        let small = scan_reply(800); // ~412 KiB, ~101 reads
        let big = scan_reply(1600); // ~824 KiB, ~202 reads

        let (small_passes, small_bytes) = decode_cost(&small, CHUNK);
        let (big_passes, big_bytes) = decode_cost(&big, CHUNK);

        let small_reads = small.len().div_ceil(CHUNK) as u64;
        let big_reads = big.len().div_ceil(CHUNK) as u64;

        // Visible with `--nocapture`; the assertions below are what fails.
        println!(
            "small: {} bytes / {small_reads} reads -> {small_passes} parses, {small_bytes} parser bytes\n\
             big:   {} bytes / {big_reads} reads -> {big_passes} parses, {big_bytes} parser bytes",
            small.len(),
            big.len(),
        );

        // (1) Full parses must not scale with the number of reads. The
        //     decoder may only hand the parser a frame it already knows is
        //     complete, so this is 1 per reply.
        assert!(
            small_passes <= 2 && big_passes <= 2,
            "full parses scale with reads: {small_passes} over {small_reads} reads, \
             {big_passes} over {big_reads} reads (a re-parse-per-read decoder \
             gives one per read)"
        );

        // (2) Total bytes walked by the parser must be O(N), not O(N*reads).
        for (label, bytes, len) in [
            ("small", small_bytes, small.len() as u64),
            ("big", big_bytes, big.len() as u64),
        ] {
            assert!(
                bytes <= 2 * len,
                "{label}: parser walked {bytes} bytes for a {len}-byte reply \
                 (quadratic decode walks ~{})",
                len * len.div_ceil(CHUNK as u64) / 2
            );
        }

        // (3) The scaling claim itself: doubling the reply must roughly
        //     double the work. Quadratic decode quadruples it (~4.0x);
        //     linear decode gives ~2.0x. 2.5x separates the two cleanly.
        assert!(
            big_bytes * 10 <= small_bytes * 25,
            "work grew {}% when the reply doubled ({small_bytes} -> {big_bytes} \
             bytes over {small_reads} -> {big_reads} reads); linear is ~200%, \
             quadratic is ~400%",
            big_bytes * 100 / small_bytes.max(1)
        );
    }

    /// The full-scale reproduction from the bug report: a 5.2 MB SCAN reply
    /// (10k keys x 512 B) replayed in 4096-byte reads. Ignored by default —
    /// it is slow under a debug build, and
    /// `aggregate_reply_decode_cost_is_linear_not_quadratic` already pins
    /// the property. Run with:
    ///   cargo test -p compio-redis --release -- --ignored --nocapture
    #[test]
    #[ignore = "slow at scale; the scaled test pins the same property"]
    fn scan_reply_decode_cost_at_full_scale() {
        const CHUNK: usize = 4096;
        let payload = scan_reply(10_000);
        let reads = payload.len().div_ceil(CHUNK);
        let (passes, bytes) = decode_cost(&payload, CHUNK);
        println!(
            "reply={} bytes, reads={reads}, full parses={passes}, parser bytes={bytes} \
             ({}% of the reply size)",
            payload.len(),
            bytes * 100 / payload.len() as u64,
        );
        assert!(passes <= 2, "expected one full parse, got {passes}");
        assert!(bytes <= 2 * payload.len() as u64, "parser walked {bytes} bytes");
    }

    #[test]
    fn decode_array_of_bulk_strings() {
        // SCAN reply: cursor=0, keys=[foo, bar]
        let payload = b"*2\r\n$1\r\n0\r\n*2\r\n$3\r\nfoo\r\n$3\r\nbar\r\n";
        let (frame, consumed) = try_decode(payload).unwrap().unwrap();
        assert_eq!(consumed, payload.len());
        let arr = expect_array(frame).unwrap();
        assert_eq!(arr.len(), 2);
    }
}
