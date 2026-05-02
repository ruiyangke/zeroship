//! RFC 6455 §5.2 server → client frame decoder.
//!
//! Reads frames from any compio `AsyncRead`. Designed for the receive
//! task to drive in a loop. Reassembles fragmented messages, drops
//! pong frames silently, and reports peer Close + Ping back to the
//! caller (which writes the close echo / pong reply via the writer
//! task).
//!
//! Per RFC 6455 §5.1 the server MUST NOT mask its frames — bit 8
//! (MASK) MUST be 0 in every server-to-client frame. We HARD-FAIL the
//! connection on any masked server frame (RFC 6455 §5.5 violations
//! cause "the receiver of the frame MAY close the connection").
//!
//! Per RFC 6455 §5.5 control frames (Close, Ping, Pong) MUST have
//! payload ≤ 125 bytes and MUST NOT be fragmented (FIN=1 always). We
//! enforce both.
//!
//! Per RFC 6455 §5.4 a fragmented message arrives as one initial frame
//! (opcode != 0, FIN=0), zero or more continuation frames (opcode = 0,
//! FIN=0), and one final continuation (opcode = 0, FIN=1). The decoder
//! buffers fragments and emits the assembled message when FIN=1
//! arrives. Control frames are interleavable per spec §5.5.

#![cfg(feature = "runtime_native_websocket")]

use std::io;

use compio::io::{AsyncRead, AsyncReadExt};

use super::frame_writer::{
    OPCODE_BINARY, OPCODE_CLOSE, OPCODE_CONTINUATION, OPCODE_PING, OPCODE_PONG, OPCODE_TEXT,
};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// One decoded message ready for the caller.
#[derive(Debug)]
pub enum DecodedFrame {
    /// Complete text message (assembled if it was fragmented).
    Text(String),
    /// Complete binary message (assembled if it was fragmented).
    Binary(Vec<u8>),
    /// Peer Ping — caller MUST schedule an immediate Pong with the
    /// same payload (RFC 6455 §5.5.2).
    Ping(Vec<u8>),
    /// Peer Pong — caller can ignore unless tracking heartbeat.
    Pong(Vec<u8>),
    /// Peer Close — caller emits CloseEvent and writes Close echo via
    /// the writer task. `code` and `reason` per RFC 6455 §5.5.1; if
    /// the peer sent an empty payload, code=1005 (sentinel — see
    /// CRITICAL #6, never serialise on the wire).
    Close { code: u16, reason: String },
}

/// Decoder error — promoted from the wire-level violations to a
/// status code per RFC 6455 §7.4.1.
#[derive(Debug)]
pub enum DecodeError {
    /// I/O failure on the underlying stream.
    Io(io::Error),
    /// Server frame had the MASK bit set (RFC 6455 §5.1 violation).
    MaskedServerFrame,
    /// RSV1/2/3 bits non-zero (we offer no extensions).
    NonZeroReserved,
    /// Control frame had FIN=0 (RFC 6455 §5.5).
    FragmentedControlFrame,
    /// Control frame payload > 125 bytes (RFC 6455 §5.5).
    OversizedControlFrame(usize),
    /// Continuation arrived without an initial fragment.
    OrphanContinuation,
    /// Initial data frame arrived while the prior message was still
    /// being assembled (RFC 6455 §5.4).
    InterleavedDataFrame,
    /// Frame size exceeded `max_frame_size`.
    FrameTooLarge { actual: usize, limit: usize },
    /// Assembled message size exceeded `max_message_size`.
    MessageTooLarge { actual: usize, limit: usize },
    /// Invalid UTF-8 in a Text frame (RFC 6455 §5.6 — the receiver
    /// MUST fail the connection on invalid UTF-8).
    InvalidUtf8,
    /// Unknown opcode (RFC 6455 §5.2 — opcodes 0x3-0x7 + 0xB-0xF
    /// are reserved).
    UnknownOpcode(u8),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Io(e) => write!(f, "ws read I/O error: {e}"),
            DecodeError::MaskedServerFrame => write!(f, "server sent masked frame"),
            DecodeError::NonZeroReserved => write!(f, "reserved bits non-zero"),
            DecodeError::FragmentedControlFrame => write!(f, "control frame was fragmented"),
            DecodeError::OversizedControlFrame(n) => {
                write!(f, "control frame payload too large: {n} > 125")
            }
            DecodeError::OrphanContinuation => write!(f, "continuation without initial fragment"),
            DecodeError::InterleavedDataFrame => write!(f, "interleaved data frame"),
            DecodeError::FrameTooLarge { actual, limit } => {
                write!(f, "frame too large: {actual} > {limit}")
            }
            DecodeError::MessageTooLarge { actual, limit } => {
                write!(f, "assembled message too large: {actual} > {limit}")
            }
            DecodeError::InvalidUtf8 => write!(f, "invalid UTF-8 in text frame"),
            DecodeError::UnknownOpcode(op) => write!(f, "unknown opcode 0x{op:X}"),
        }
    }
}

impl std::error::Error for DecodeError {}

impl From<io::Error> for DecodeError {
    fn from(e: io::Error) -> Self {
        DecodeError::Io(e)
    }
}

/// Stateful frame reader — accumulates fragments across calls.
pub struct FrameReader {
    /// Reassembly buffer for a fragmented message. `Some` once an
    /// initial Text/Binary frame with FIN=0 has arrived.
    fragmented_message: Option<FragmentedMessage>,
    /// Cap per WebSocketInit.maxFrameSize — applies to any single
    /// frame's payload (data or control).
    max_frame_size: usize,
    /// Cap per WebSocketInit.maxMessageSize — applies to the
    /// reassembled message size after fragments are joined.
    max_message_size: usize,
}

struct FragmentedMessage {
    /// Whether this is a Text (true) or Binary (false) message.
    is_text: bool,
    /// Accumulated payload bytes.
    bytes: Vec<u8>,
}

impl FrameReader {
    pub fn new(max_frame_size: usize, max_message_size: usize) -> Self {
        FrameReader {
            fragmented_message: None,
            max_frame_size,
            max_message_size,
        }
    }

    /// Read one frame from `r`. Returns the decoded frame, OR `Ok(None)`
    /// when the frame was a non-final fragment that was buffered
    /// internally — caller should loop.
    ///
    /// The caller drives this in a `loop { read_frame().await }` and
    /// dispatches each `DecodedFrame` it gets.
    pub async fn read_frame<R: AsyncRead>(
        &mut self,
        r: &mut R,
    ) -> Result<DecodedFrame, DecodeError> {
        loop {
            if let Some(frame) = self.read_one(r).await? {
                return Ok(frame);
            }
            // None = continuation collected; loop and read the next frame.
        }
    }

    /// Read a single frame off the wire and either return a complete
    /// `DecodedFrame` or buffer it as a fragment and return `Ok(None)`.
    async fn read_one<R: AsyncRead>(
        &mut self,
        r: &mut R,
    ) -> Result<Option<DecodedFrame>, DecodeError> {
        // Read the 2-byte fixed header.
        let mut header = vec![0u8; 2];
        let res = r.read_exact(header).await;
        header = res.1;
        res.0?;
        let h0 = header[0];
        let h1 = header[1];

        let fin = (h0 & 0x80) != 0;
        let rsv = h0 & 0x70;
        let opcode = h0 & 0x0F;
        let masked = (h1 & 0x80) != 0;
        let len_marker = h1 & 0x7F;

        // RFC 6455 §5.1: server MUST NOT mask.
        if masked {
            return Err(DecodeError::MaskedServerFrame);
        }
        // RSV1/2/3 must be zero (no extensions negotiated).
        if rsv != 0 {
            return Err(DecodeError::NonZeroReserved);
        }

        // Decode payload length per §5.2.
        let payload_len: usize = if len_marker <= 125 {
            len_marker as usize
        } else if len_marker == 126 {
            let mut buf = vec![0u8; 2];
            let res = r.read_exact(buf).await;
            buf = res.1;
            res.0?;
            u16::from_be_bytes([buf[0], buf[1]]) as usize
        } else {
            // len_marker == 127
            let mut buf = vec![0u8; 8];
            let res = r.read_exact(buf).await;
            buf = res.1;
            res.0?;
            let v = u64::from_be_bytes([
                buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
            ]);
            // RFC 6455 §5.2: most-significant bit MUST be 0 in 64-bit
            // length. Convert to usize.
            if v > usize::MAX as u64 {
                return Err(DecodeError::FrameTooLarge {
                    actual: usize::MAX,
                    limit: self.max_frame_size,
                });
            }
            v as usize
        };

        // Frame size cap.
        if payload_len > self.max_frame_size {
            return Err(DecodeError::FrameTooLarge {
                actual: payload_len,
                limit: self.max_frame_size,
            });
        }

        // Control-frame guards (§5.5).
        let is_control = (opcode & 0x08) != 0;
        if is_control {
            if !fin {
                return Err(DecodeError::FragmentedControlFrame);
            }
            if payload_len > 125 {
                return Err(DecodeError::OversizedControlFrame(payload_len));
            }
        }

        // Read the payload.
        let mut payload = vec![0u8; payload_len];
        if payload_len > 0 {
            let res = r.read_exact(payload).await;
            payload = res.1;
            res.0?;
        }

        // Dispatch by opcode.
        match opcode {
            OPCODE_CONTINUATION => {
                let Some(buf) = self.fragmented_message.as_mut() else {
                    return Err(DecodeError::OrphanContinuation);
                };
                let new_total = buf.bytes.len().saturating_add(payload_len);
                if new_total > self.max_message_size {
                    return Err(DecodeError::MessageTooLarge {
                        actual: new_total,
                        limit: self.max_message_size,
                    });
                }
                buf.bytes.extend_from_slice(&payload);
                if fin {
                    let buf = self.fragmented_message.take().unwrap();
                    if buf.is_text {
                        let s = String::from_utf8(buf.bytes).map_err(|_| DecodeError::InvalidUtf8)?;
                        Ok(Some(DecodedFrame::Text(s)))
                    } else {
                        Ok(Some(DecodedFrame::Binary(buf.bytes)))
                    }
                } else {
                    Ok(None)
                }
            }
            OPCODE_TEXT | OPCODE_BINARY => {
                if self.fragmented_message.is_some() {
                    return Err(DecodeError::InterleavedDataFrame);
                }
                if fin {
                    if payload_len > self.max_message_size {
                        return Err(DecodeError::MessageTooLarge {
                            actual: payload_len,
                            limit: self.max_message_size,
                        });
                    }
                    if opcode == OPCODE_TEXT {
                        let s = String::from_utf8(payload).map_err(|_| DecodeError::InvalidUtf8)?;
                        Ok(Some(DecodedFrame::Text(s)))
                    } else {
                        Ok(Some(DecodedFrame::Binary(payload)))
                    }
                } else {
                    if payload_len > self.max_message_size {
                        return Err(DecodeError::MessageTooLarge {
                            actual: payload_len,
                            limit: self.max_message_size,
                        });
                    }
                    self.fragmented_message = Some(FragmentedMessage {
                        is_text: opcode == OPCODE_TEXT,
                        bytes: payload,
                    });
                    Ok(None)
                }
            }
            OPCODE_CLOSE => {
                // §5.5.1: payload may be empty, or 2 bytes code + UTF-8 reason.
                let (code, reason) = if payload.is_empty() {
                    // 1005 sentinel — never on the wire, but here we
                    // synthesize it so the JS observable is "no code".
                    (1005u16, String::new())
                } else if payload.len() == 1 {
                    // Per RFC 6455 §5.5.1 ANY non-empty close payload
                    // MUST have at least 2 bytes for the code. A single-
                    // byte payload is a protocol violation.
                    return Err(DecodeError::OversizedControlFrame(1));
                } else {
                    let code = u16::from_be_bytes([payload[0], payload[1]]);
                    let reason = String::from_utf8(payload[2..].to_vec())
                        .map_err(|_| DecodeError::InvalidUtf8)?;
                    (code, reason)
                };
                Ok(Some(DecodedFrame::Close { code, reason }))
            }
            OPCODE_PING => Ok(Some(DecodedFrame::Ping(payload))),
            OPCODE_PONG => Ok(Some(DecodedFrame::Pong(payload))),
            other => Err(DecodeError::UnknownOpcode(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // compio's `AsyncRead` is implemented for `&[u8]` directly (with the
    // slice advanced as bytes are read), so the tests below feed the
    // generated frames through a `&[u8]` cursor.

    fn server_frame(opcode: u8, fin: bool, payload: &[u8]) -> Vec<u8> {
        // Build an unmasked server frame: same as `encode_with_mask`
        // but no MASK bit, no mask key, raw payload.
        let mut out = Vec::new();
        let fin_bit = if fin { 0x80 } else { 0 };
        out.push(fin_bit | (opcode & 0x0F));
        let len = payload.len();
        if len <= 125 {
            out.push(len as u8);
        } else if len <= u16::MAX as usize {
            out.push(126);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            out.push(127);
            out.extend_from_slice(&(len as u64).to_be_bytes());
        }
        out.extend_from_slice(payload);
        out
    }

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        compio::runtime::Runtime::new().unwrap().block_on(fut)
    }

    #[test]
    fn decode_text_hello() {
        let bytes = server_frame(OPCODE_TEXT, true, b"hello");
        let mut r: &[u8] = &bytes;
        let mut reader = FrameReader::new(1 << 20, 1 << 20);
        let result = block_on(reader.read_frame(&mut r)).unwrap();
        match result {
            DecodedFrame::Text(s) => assert_eq!(s, "hello"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn decode_binary() {
        let bytes = server_frame(OPCODE_BINARY, true, &[1, 2, 3, 4]);
        let mut r: &[u8] = &bytes;
        let mut reader = FrameReader::new(1 << 20, 1 << 20);
        let result = block_on(reader.read_frame(&mut r)).unwrap();
        match result {
            DecodedFrame::Binary(b) => assert_eq!(b, vec![1, 2, 3, 4]),
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn decode_fragmented_text() {
        // First fragment: text "He", FIN=0
        let mut bytes = server_frame(OPCODE_TEXT, false, b"He");
        // Second fragment: continuation "ll", FIN=0
        bytes.extend(server_frame(OPCODE_CONTINUATION, false, b"ll"));
        // Final fragment: continuation "o", FIN=1
        bytes.extend(server_frame(OPCODE_CONTINUATION, true, b"o"));
        let mut r: &[u8] = &bytes;
        let mut reader = FrameReader::new(1 << 20, 1 << 20);
        let result = block_on(reader.read_frame(&mut r)).unwrap();
        match result {
            DecodedFrame::Text(s) => assert_eq!(s, "Hello"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn decode_close_with_code_and_reason() {
        // 0x03 0xE8 = 1000, "ok" reason.
        let payload = [0x03, 0xE8, b'o', b'k'];
        let bytes = server_frame(OPCODE_CLOSE, true, &payload);
        let mut r: &[u8] = &bytes;
        let mut reader = FrameReader::new(1 << 20, 1 << 20);
        let result = block_on(reader.read_frame(&mut r)).unwrap();
        match result {
            DecodedFrame::Close { code, reason } => {
                assert_eq!(code, 1000);
                assert_eq!(reason, "ok");
            }
            other => panic!("expected Close, got {other:?}"),
        }
    }

    #[test]
    fn decode_close_empty() {
        let bytes = server_frame(OPCODE_CLOSE, true, &[]);
        let mut r: &[u8] = &bytes;
        let mut reader = FrameReader::new(1 << 20, 1 << 20);
        let result = block_on(reader.read_frame(&mut r)).unwrap();
        match result {
            DecodedFrame::Close { code, reason } => {
                assert_eq!(code, 1005);
                assert_eq!(reason, "");
            }
            other => panic!("expected Close, got {other:?}"),
        }
    }

    #[test]
    fn decode_ping() {
        let bytes = server_frame(OPCODE_PING, true, b"ping-payload");
        let mut r: &[u8] = &bytes;
        let mut reader = FrameReader::new(1 << 20, 1 << 20);
        let result = block_on(reader.read_frame(&mut r)).unwrap();
        match result {
            DecodedFrame::Ping(p) => assert_eq!(p, b"ping-payload".to_vec()),
            other => panic!("expected Ping, got {other:?}"),
        }
    }

    #[test]
    fn decode_pong() {
        let bytes = server_frame(OPCODE_PONG, true, b"pong-payload");
        let mut r: &[u8] = &bytes;
        let mut reader = FrameReader::new(1 << 20, 1 << 20);
        let result = block_on(reader.read_frame(&mut r)).unwrap();
        match result {
            DecodedFrame::Pong(p) => assert_eq!(p, b"pong-payload".to_vec()),
            other => panic!("expected Pong, got {other:?}"),
        }
    }

    #[test]
    fn reject_masked_server_frame() {
        // Frame with MASK=1.
        let bytes = vec![0x81, 0x85, 0, 0, 0, 0, b'H', b'e', b'l', b'l', b'o'];
        let mut r: &[u8] = &bytes;
        let mut reader = FrameReader::new(1 << 20, 1 << 20);
        let err = block_on(reader.read_frame(&mut r)).unwrap_err();
        assert!(matches!(err, DecodeError::MaskedServerFrame));
    }

    #[test]
    fn reject_nonzero_rsv() {
        let bytes = vec![0x81 | 0x40, 0x05, b'H', b'e', b'l', b'l', b'o'];
        let mut r: &[u8] = &bytes;
        let mut reader = FrameReader::new(1 << 20, 1 << 20);
        let err = block_on(reader.read_frame(&mut r)).unwrap_err();
        assert!(matches!(err, DecodeError::NonZeroReserved));
    }

    #[test]
    fn reject_fragmented_close() {
        // Close frame with FIN=0 — protocol violation.
        let bytes = server_frame(OPCODE_CLOSE, false, &[]);
        let mut r: &[u8] = &bytes;
        let mut reader = FrameReader::new(1 << 20, 1 << 20);
        let err = block_on(reader.read_frame(&mut r)).unwrap_err();
        assert!(matches!(err, DecodeError::FragmentedControlFrame));
    }

    #[test]
    fn reject_oversize_ping() {
        // Ping with 200-byte payload — > 125 cap.
        let bytes = server_frame(OPCODE_PING, true, &vec![b'x'; 200]);
        let mut r: &[u8] = &bytes;
        let mut reader = FrameReader::new(1 << 20, 1 << 20);
        let err = block_on(reader.read_frame(&mut r)).unwrap_err();
        assert!(matches!(err, DecodeError::OversizedControlFrame(200)));
    }

    #[test]
    fn reject_orphan_continuation() {
        let bytes = server_frame(OPCODE_CONTINUATION, true, b"oops");
        let mut r: &[u8] = &bytes;
        let mut reader = FrameReader::new(1 << 20, 1 << 20);
        let err = block_on(reader.read_frame(&mut r)).unwrap_err();
        assert!(matches!(err, DecodeError::OrphanContinuation));
    }

    #[test]
    fn frame_with_16bit_length() {
        let payload = vec![b'a'; 200];
        let bytes = server_frame(OPCODE_BINARY, true, &payload);
        // sanity: the length encoding should use the 16-bit form.
        assert_eq!(bytes[1], 126);
        let mut r: &[u8] = &bytes;
        let mut reader = FrameReader::new(1 << 20, 1 << 20);
        let result = block_on(reader.read_frame(&mut r)).unwrap();
        match result {
            DecodedFrame::Binary(b) => assert_eq!(b.len(), 200),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn frame_with_64bit_length() {
        let payload = vec![b'a'; 70_000];
        let bytes = server_frame(OPCODE_BINARY, true, &payload);
        // sanity: the length encoding should use the 64-bit form.
        assert_eq!(bytes[1], 127);
        let mut r: &[u8] = &bytes;
        let mut reader = FrameReader::new(1 << 20, 1 << 20);
        let result = block_on(reader.read_frame(&mut r)).unwrap();
        match result {
            DecodedFrame::Binary(b) => assert_eq!(b.len(), 70_000),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn invalid_utf8_in_text_fails() {
        // 0xC0 0xC0 is invalid UTF-8.
        let bytes = server_frame(OPCODE_TEXT, true, &[0xC0, 0xC0]);
        let mut r: &[u8] = &bytes;
        let mut reader = FrameReader::new(1 << 20, 1 << 20);
        let err = block_on(reader.read_frame(&mut r)).unwrap_err();
        assert!(matches!(err, DecodeError::InvalidUtf8));
    }

    #[test]
    fn frame_too_large_rejected() {
        let bytes = server_frame(OPCODE_BINARY, true, &vec![b'x'; 1024]);
        let mut r: &[u8] = &bytes;
        let mut reader = FrameReader::new(512, 1024); // limit = 512
        let err = block_on(reader.read_frame(&mut r)).unwrap_err();
        assert!(matches!(err, DecodeError::FrameTooLarge { actual: 1024, .. }));
    }
}
