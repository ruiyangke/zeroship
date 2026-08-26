//! RFC 6455 §5.2 client → server frame encoder.
//!
//! Pure functions, no I/O. The writer task in `network.rs` calls these
//! to build a `Vec<u8>` and writes the bytes via compio's `AsyncWrite`.
//!
//! Wire format (per https://datatracker.ietf.org/doc/html/rfc6455#section-5.2):
//!
//! ```text
//!   0                   1                   2                   3
//!   0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//!  +-+-+-+-+-------+-+-------------+-------------------------------+
//!  |F|R|R|R| opcode|M| Payload len |    Extended payload length    |
//!  |I|S|S|S|  (4)  |A|     (7)     |             (16/64)           |
//!  |N|V|V|V|       |S|             |   (if payload len==126/127)   |
//!  | |1|2|3|       |K|             |                               |
//!  +-+-+-+-+-------+-+-------------+ - - - - - - - - - - - - - - - +
//!  |     Extended payload length continued, if payload len == 127  |
//!  + - - - - - - - - - - - - - - - +-------------------------------+
//!  |                               |Masking-key, if MASK set to 1  |
//!  +-------------------------------+-------------------------------+
//!  | Masking-key (continued)       |          Payload Data         |
//!  +-------------------------------- - - - - - - - - - - - - - - - +
//!  :                     Payload Data continued ...                :
//!  + - - - - - - - - - - - - - - - - - - - - - - - - - - - - - - - +
//!  |                     Payload Data continued ...                |
//!  +---------------------------------------------------------------+
//! ```
//!
//! Client → server frames MUST be masked (RFC 6455 §5.3) — every payload
//! byte is XOR'd with `mask[i % 4]`. The mask key is 4 bytes of strong
//! randomness, picked per frame.
//!
//! Permessage-deflate is off because we offer no extensions, so
//! RSV1/RSV2/RSV3 are always zero.

#![cfg(feature = "runtime_native_websocket")]

use aws_lc_rs::rand;

// Opcodes per RFC 6455 §5.2:
pub const OPCODE_CONTINUATION: u8 = 0x0;
pub const OPCODE_TEXT: u8 = 0x1;
pub const OPCODE_BINARY: u8 = 0x2;
pub const OPCODE_CLOSE: u8 = 0x8;
pub const OPCODE_PING: u8 = 0x9;
pub const OPCODE_PONG: u8 = 0xA;

/// Generate a fresh 4-byte mask key per RFC 6455 §5.3 — MUST be
/// strong random for every client frame so the same bytes never get
/// the same XOR pattern (basic per-frame masking guarantee, not a
/// security primitive — the mask exists to defeat cache poisoning of
/// transparent intermediaries that mishandle WebSocket traffic, NOT
/// to provide confidentiality).
fn random_mask() -> [u8; 4] {
    let mut buf = [0u8; 4];
    rand::fill(&mut buf).expect("aws_lc_rs::rand::fill failed for mask");
    buf
}

/// Encode a complete client → server frame.
/// Returns the bytes ready to write to the wire.
///
/// `fin`: bit 0 of byte 0. v1 ships unfragmented frames only — `fin`
/// is always true. (The reader handles incoming fragments per spec.)
pub fn encode_client_frame(opcode: u8, fin: bool, payload: &[u8]) -> Vec<u8> {
    debug_assert!(opcode <= 0xF, "opcode must fit in 4 bits");
    let mask = random_mask();
    encode_with_mask(opcode, fin, payload, mask)
}

/// Same as `encode_client_frame` but with a caller-supplied mask key.
/// Used by the spec smoke tests.
pub fn encode_with_mask(opcode: u8, fin: bool, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let payload_len = payload.len();
    // Header sizing:
    //   2 bytes (FIN+opcode + MASK+len7)
    // + 2/8 bytes for extended length when len > 125
    // + 4 bytes mask key
    // + payload bytes
    let header_extra = if payload_len <= 125 {
        0
    } else if payload_len <= u16::MAX as usize {
        2
    } else {
        8
    };
    let mut out = Vec::with_capacity(2 + header_extra + 4 + payload_len);

    // Byte 0: FIN | RSV1 | RSV2 | RSV3 | opcode (4 bits)
    let fin_bit = if fin { 0x80 } else { 0x00 };
    out.push(fin_bit | (opcode & 0x0F));

    // Byte 1: MASK (always 1 client → server) | payload-len (7 bits)
    if payload_len <= 125 {
        out.push(0x80 | (payload_len as u8));
    } else if payload_len <= u16::MAX as usize {
        out.push(0x80 | 126);
        out.extend_from_slice(&(payload_len as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(payload_len as u64).to_be_bytes());
    }

    // 4-byte mask key.
    out.extend_from_slice(&mask);

    // Masked payload.
    let payload_offset = out.len();
    out.extend_from_slice(payload);
    for (i, byte) in out[payload_offset..].iter_mut().enumerate() {
        *byte ^= mask[i & 3];
    }

    out
}

/// Encode a TEXT frame (FIN=1, opcode=0x1).
pub fn encode_text_frame(payload: &str) -> Vec<u8> {
    encode_client_frame(OPCODE_TEXT, true, payload.as_bytes())
}

/// Encode a BINARY frame (FIN=1, opcode=0x2).
pub fn encode_binary_frame(payload: &[u8]) -> Vec<u8> {
    encode_client_frame(OPCODE_BINARY, true, payload)
}

/// Encode a CLOSE frame (FIN=1, opcode=0x8). Optional (code, reason)
/// payload per RFC 6455 §5.5.1: 2-byte big-endian status code followed
/// by UTF-8 reason. Empty payload is valid.
///
/// Caller must enforce: 1005 / 1006 / 1015 are NEVER serialised on the
/// wire (RFC 6455 §7.4.1).
pub fn encode_close_frame(code: Option<u16>, reason: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    if let Some(c) = code {
        payload.extend_from_slice(&c.to_be_bytes());
        payload.extend_from_slice(reason.as_bytes());
    }
    encode_client_frame(OPCODE_CLOSE, true, &payload)
}

/// Encode a PING frame (FIN=1, opcode=0x9). Optional payload (≤ 125 bytes
/// per RFC 6455 §5.5).
pub fn encode_ping_frame(payload: &[u8]) -> Vec<u8> {
    encode_client_frame(OPCODE_PING, true, payload)
}

/// Encode a PONG frame (FIN=1, opcode=0xA). Used to reply to peer Ping
/// frames per RFC 6455 §5.5.3 — payload is the Ping's payload echoed.
pub fn encode_pong_frame(payload: &[u8]) -> Vec<u8> {
    encode_client_frame(OPCODE_PONG, true, payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Apply `mask` to the masked region of an encoded frame and return
    /// just the unmasked payload — utility for tests so we don't have to
    /// reproduce the mask twice.
    fn unmask_payload(frame: &[u8], header_len: usize, mask: [u8; 4]) -> Vec<u8> {
        let payload = &frame[header_len + 4..];
        payload.iter().enumerate().map(|(i, b)| b ^ mask[i & 3]).collect()
    }

    #[test]
    fn text_hello_with_known_mask() {
        // Masked frame for text "Hello" with mask 37,fa,21,3d (RFC 6455 §5.7.2).
        let mask = [0x37, 0xfa, 0x21, 0x3d];
        let frame = encode_with_mask(OPCODE_TEXT, true, b"Hello", mask);
        // Header: 0x81 0x85 followed by 4-byte mask, then 5 bytes payload.
        assert_eq!(frame[0], 0x81); // FIN=1, opcode=text
        assert_eq!(frame[1], 0x85); // MASK=1, len=5
        assert_eq!(&frame[2..6], &mask);
        // Per RFC 6455 §5.7.2, the encoded payload is:
        //   0x7f 0x9f 0x4d 0x51 0x58
        assert_eq!(&frame[6..], &[0x7f, 0x9f, 0x4d, 0x51, 0x58]);
        // Round-trip: unmasking yields "Hello".
        assert_eq!(unmask_payload(&frame, 2, mask), b"Hello");
    }

    #[test]
    fn binary_three_bytes() {
        let frame = encode_binary_frame(&[0x01, 0x02, 0x03]);
        assert_eq!(frame[0], 0x82); // FIN=1, opcode=binary
        assert_eq!(frame[1], 0x83); // MASK=1, len=3
        // Verify unmasking yields original.
        let mask: [u8; 4] = frame[2..6].try_into().unwrap();
        assert_eq!(unmask_payload(&frame, 2, mask), vec![0x01, 0x02, 0x03]);
    }

    #[test]
    fn close_with_code_and_reason() {
        let frame = encode_close_frame(Some(1000), "ok");
        // Payload: 0x03 0xE8 'o' 'k' = 4 bytes.
        assert_eq!(frame[0], 0x88); // FIN=1, opcode=close
        assert_eq!(frame[1], 0x84); // MASK=1, len=4
        let mask: [u8; 4] = frame[2..6].try_into().unwrap();
        let unmasked = unmask_payload(&frame, 2, mask);
        assert_eq!(unmasked, vec![0x03, 0xE8, b'o', b'k']);
    }

    #[test]
    fn close_empty_payload() {
        let frame = encode_close_frame(None, "");
        // Empty payload — no mask bytes to verify.
        assert_eq!(frame[0], 0x88);
        assert_eq!(frame[1], 0x80); // MASK=1, len=0
        // Header(2) + mask(4) = 6 bytes total.
        assert_eq!(frame.len(), 6);
    }

    #[test]
    fn ping_no_payload() {
        let frame = encode_ping_frame(&[]);
        assert_eq!(frame[0], 0x89); // FIN=1, opcode=ping
        assert_eq!(frame[1], 0x80); // MASK=1, len=0
        assert_eq!(frame.len(), 6);
    }

    #[test]
    fn pong_with_payload() {
        let frame = encode_pong_frame(b"x");
        assert_eq!(frame[0], 0x8A); // FIN=1, opcode=pong
        assert_eq!(frame[1], 0x81); // MASK=1, len=1
        let mask: [u8; 4] = frame[2..6].try_into().unwrap();
        assert_eq!(unmask_payload(&frame, 2, mask), b"x");
    }

    #[test]
    fn empty_text_frame() {
        let frame = encode_text_frame("");
        assert_eq!(frame[0], 0x81); // FIN=1, opcode=text
        assert_eq!(frame[1], 0x80); // MASK=1, len=0
        assert_eq!(frame.len(), 6);
    }

    #[test]
    fn payload_126_bytes_uses_16bit_length() {
        let payload = vec![b'a'; 126];
        let frame = encode_binary_frame(&payload);
        assert_eq!(frame[0], 0x82);
        assert_eq!(frame[1], 0x80 | 126); // MASK=1, len-marker=126
        // Extended length is 2 bytes big-endian = 126.
        assert_eq!(&frame[2..4], &[0x00, 0x7E]);
        // Mask bytes at 4..8, payload at 8..
        assert_eq!(frame.len(), 4 + 4 + 126);
    }

    #[test]
    fn payload_65535_bytes_still_16bit_length() {
        let payload = vec![b'b'; 65535];
        let frame = encode_binary_frame(&payload);
        assert_eq!(frame[1], 0x80 | 126);
        assert_eq!(&frame[2..4], &[0xFF, 0xFF]);
    }

    #[test]
    fn payload_65536_bytes_uses_64bit_length() {
        let payload = vec![b'c'; 65536];
        let frame = encode_binary_frame(&payload);
        assert_eq!(frame[1], 0x80 | 127);
        // 8-byte big-endian length.
        assert_eq!(&frame[2..10], &[0, 0, 0, 0, 0, 1, 0, 0]);
        // Header(2) + extlen(8) + mask(4) + payload(65536).
        assert_eq!(frame.len(), 2 + 8 + 4 + 65536);
    }

    #[test]
    fn fin_bit_off() {
        let frame = encode_with_mask(OPCODE_TEXT, false, b"x", [0; 4]);
        assert_eq!(frame[0], 0x01); // FIN=0, opcode=text
    }

    #[test]
    fn random_mask_changes_every_call() {
        // Two encodes of the same payload should produce different
        // masked payload bytes (extremely high probability — the mask
        // is 32 bits of strong randomness).
        let f1 = encode_text_frame("test");
        let f2 = encode_text_frame("test");
        assert_ne!(f1[2..], f2[2..], "mask should not repeat across calls");
    }
}
