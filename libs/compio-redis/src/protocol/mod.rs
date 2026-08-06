//! RESP2 framing helpers built on the `redis-protocol` crate.
//!
//! Thin convenience layer: build an `Array-of-BulkString` command frame,
//! serialize into a `Vec<u8>`, decode a single reply from an accumulating
//! buffer. The `redis-protocol` crate handles correctness; we just wrap
//! its API to the shapes our client needs.

use redis_protocol::resp2::{
    decode,
    encode::encode,
    types::{OwnedFrame, Resp2Frame},
};

use crate::error::{Error, Result};

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
    decode::decode(buf).map_err(|e| Error::Protocol(format!("decode: {e}")))
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
