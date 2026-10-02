#![no_main]

use compio_postgres::replication::pgoutput::{self, Decoder};
use libfuzzer_sys::fuzz_target;

/// Frame a sequence as a chain of `u32` big-endian length prefixes. A prefix
/// longer than the bytes that follow consumes the tail, so a truncated frame is
/// still decoded rather than skipped.
fn frames(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = data;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let (frame, tail) = if rest.len() >= 4 {
            let declared = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
            let body = &rest[4..];
            if declared <= body.len() {
                body.split_at(declared)
            } else {
                (body, &body[..0])
            }
        } else {
            (rest, &rest[..0])
        };
        rest = tail;
        Some(frame)
    })
}

fuzz_target!(|data: &[u8]| {
    // The stateful decoder needs a SEQUENCE to reach its stream-block framing,
    // so the input is a chain of length-prefixed frames. Every frame also goes
    // through the stateless entry point, which refuses streaming frames without
    // a chunk to carry their state.
    let mut decoder = Decoder::new();
    for frame in frames(data) {
        let _ = pgoutput::decode(frame);
        let _ = decoder.decode(frame);
    }
});
