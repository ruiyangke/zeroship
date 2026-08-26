//! Randomised pgoutput bodies, to generalise the hand-written hostile shapes.
//!
//! `tests/frame_fuzz.rs` does this for BACKEND FRAMES - the outer envelope the
//! connection reads. This file does it one layer down, for the logical
//! replication payload inside `XLogData`, which is a different parser with far
//! more structure: nested tuple data, per-column length prefixes, a format byte
//! per column, in-chunk xid prefixes, and three protocol versions' worth of
//! optional fields.
//!
//! `tests/pgoutput_allocation.rs` already covers the ALLOCATION half - a count
//! the peer chooses must not become a reservation it chooses. What was missing
//! is the plain one: arbitrary bytes must not panic the decoder. A walsender is
//! a peer like any other, and a panic in a parser is reachable by whoever is on
//! the other end of the socket.
//!
//! WHAT IS ASSERTED, and deliberately no more:
//!
//!   * decoding TERMINATES - no unbounded loop on a hostile length,
//!   * it does not PANIC - not on a slice index, not on an arithmetic
//!     overflow, not on a `unwrap` of a short read,
//!   * the STATEFUL decoder is still usable afterwards, so one bad frame
//!     cannot wedge a stream.
//!
//! It is NOT asserted that a random body fails. Random bytes occasionally form
//! a legitimate message, and a test demanding failure would be wrong about the
//! protocol rather than about the driver. Weak per-case assertions, large case
//! count - the standard fuzzing bargain, and the same one `frame_fuzz.rs`
//! makes.
//!
//! DETERMINISTIC BY CONSTRUCTION. The generator is the same inline seeded
//! xorshift `frame_fuzz.rs` uses - no dependency added - and every case is a
//! pure function of the seed, so a failure replays exactly.
//!
//! WHAT THE CORPUS REACHES. A fuzzer that is rejected on the tag byte before it
//! ever enters the parser is theatre, and uniformly random bytes are rejected
//! that way almost always: 1 tag in 10 is one the decoder knows. So most cases
//! are built from a KNOWN TAG and a mutated body, and the test asserts floors
//! on what actually happened - how many cases decoded successfully, and how
//! many distinct tags were accepted. Those floors are what stop this file
//! degrading into an expensive way to check that `UnknownTag` works.

use compio_postgres::replication::pgoutput::{self, PgOutputMessage};

#[allow(unused_imports)]
use crate::common;

/// Enough to cover every tag's mutated shapes many times over while keeping
/// the file inside a normal `cargo test` run. Raise it locally to hunt.
const CASES: u32 = 4096;

/// Sequences fed to the stateful decoder. Smaller than `CASES` because each
/// one is several frames.
const SEQUENCES: u32 = 2048;

/// Every tag the decoder claims to know, from its own match arms.
const KNOWN_TAGS: &[u8] = b"BCORYIUDTMSEcAbPKrpNntu";

/// xorshift64*, inline so this file adds no dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // A zero state is a fixed point for xorshift, so never allow one.
        Self(if seed == 0 {
            0x9e37_79b9_7f4a_7c15
        } else {
            seed
        })
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next() % bound as u64) as usize
        }
    }

    fn byte(&mut self) -> u8 {
        (self.next() >> 24) as u8
    }
}

/// One case: a tag the decoder knows, followed by a body of random bytes.
///
/// The body length is drawn small on purpose. Long random bodies are rejected
/// by the first length prefix that disagrees with them; short ones land on the
/// boundaries - a field that is one byte short, a count with nothing behind it
/// - which is where an off-by-one lives.
fn tagged_case(rng: &mut Rng) -> Vec<u8> {
    let tag = KNOWN_TAGS[rng.below(KNOWN_TAGS.len())];
    let len = rng.below(48);
    let mut frame = Vec::with_capacity(len + 1);
    frame.push(tag);
    for _ in 0..len {
        frame.push(rng.byte());
    }
    frame
}

/// One case of pure noise, including the empty body.
fn noise_case(rng: &mut Rng) -> Vec<u8> {
    let len = rng.below(64);
    (0..len).map(|_| rng.byte()).collect()
}
/// A well-formed tuple body: a column count and that many columns.
fn valid_tuple(rng: &mut Rng, columns: usize) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(columns as u16).to_be_bytes());
    for _ in 0..columns {
        match rng.below(4) {
            0 => out.push(b'n'),
            1 => out.push(b'u'),
            2 => {
                out.push(b't');
                let text = b"ab";
                out.extend_from_slice(&(text.len() as i32).to_be_bytes());
                out.extend_from_slice(text);
            }
            _ => {
                out.push(b'b');
                let bin = b"\xde\xad";
                out.extend_from_slice(&(bin.len() as i32).to_be_bytes());
                out.extend_from_slice(bin);
            }
        }
    }
    out
}

/// A WELL-FORMED message for one tag.
///
/// This is what reaches the parser's interior. A random body is rejected by the
/// first length prefix or C string that disagrees with it, so the tags with
/// counts and nested tuples - exactly the complicated ones - are never entered
/// at all. MEASURED 2026-08-24 before this existed: 4096 random bodies decoded
/// 132 times and reached three tags (B, C, O), all of them fixed-layout.
fn valid_case(rng: &mut Rng) -> Vec<u8> {
    let columns = rng.below(4);
    match rng.below(9) {
        0 => {
            let mut f = vec![b'B'];
            f.extend_from_slice(&[0u8; 20]);
            f
        }
        1 => {
            let mut f = vec![b'C'];
            f.extend_from_slice(&[0u8; 25]);
            f
        }
        2 => {
            let mut f = vec![b'O'];
            f.extend_from_slice(&[0u8; 8]);
            f.extend_from_slice(b"origin\0");
            f
        }
        3 => {
            // Relation, with a column list.
            let mut f = vec![b'R'];
            f.extend_from_slice(&1u32.to_be_bytes());
            f.extend_from_slice(b"public\0");
            f.extend_from_slice(b"t\0");
            f.push(b'd');
            f.extend_from_slice(&(columns as u16).to_be_bytes());
            for _ in 0..columns {
                f.push(0);
                f.extend_from_slice(b"c\0");
                f.extend_from_slice(&23u32.to_be_bytes());
                f.extend_from_slice(&(-1i32).to_be_bytes());
            }
            f
        }
        4 => {
            let mut f = vec![b'I'];
            f.extend_from_slice(&1u32.to_be_bytes());
            f.push(b'N');
            f.extend_from_slice(&valid_tuple(rng, columns));
            f
        }
        5 => {
            let mut f = vec![b'U'];
            f.extend_from_slice(&1u32.to_be_bytes());
            if rng.below(2) == 0 {
                f.push(if rng.below(2) == 0 { b'K' } else { b'O' });
                f.extend_from_slice(&valid_tuple(rng, columns));
            }
            f.push(b'N');
            f.extend_from_slice(&valid_tuple(rng, columns));
            f
        }
        6 => {
            let mut f = vec![b'D'];
            f.extend_from_slice(&1u32.to_be_bytes());
            f.push(if rng.below(2) == 0 { b'K' } else { b'O' });
            f.extend_from_slice(&valid_tuple(rng, columns));
            f
        }
        7 => {
            let mut f = vec![b'T'];
            f.extend_from_slice(&(columns as u32).to_be_bytes());
            f.push(0);
            for _ in 0..columns {
                f.extend_from_slice(&1u32.to_be_bytes());
            }
            f
        }
        _ => {
            let mut f = vec![b'M'];
            f.push(0);
            f.extend_from_slice(&[0u8; 8]);
            f.extend_from_slice(b"prefix\0");
            let body = b"content";
            f.extend_from_slice(&(body.len() as u32).to_be_bytes());
            f.extend_from_slice(body);
            f
        }
    }
}

/// A well-formed message with ONE thing wrong.
///
/// The interesting inputs are not random noise but near-misses: a length one
/// too large, a count that outlives its buffer, a format byte that is almost
/// right. Those walk into the parser and fail deep, which is where a panic
/// would be.
fn mutated_case(rng: &mut Rng) -> Vec<u8> {
    let mut frame = valid_case(rng);
    if frame.len() < 2 {
        return frame;
    }
    match rng.below(3) {
        // Flip one byte anywhere after the tag.
        0 => {
            let at = 1 + rng.below(frame.len() - 1);
            frame[at] ^= 1 << rng.below(8);
        }
        // Cut it short, which is what an interrupted stream looks like.
        1 => {
            let keep = 1 + rng.below(frame.len() - 1);
            frame.truncate(keep);
        }
        // Append junk, which the decoder explicitly tolerates - so this must
        // NOT change the verdict, and a failure here is a real finding.
        _ => {
            for _ in 0..rng.below(8) {
                frame.push(rng.byte());
            }
        }
    }
    frame
}

/// What a run observed, so the floors below can be about reality.
#[derive(Default)]
struct Reached {
    decoded: u32,
    rejected: u32,
    accepted_tags: std::collections::BTreeSet<u8>,
}

fn tag_of(message: &PgOutputMessage) -> u8 {
    match message {
        PgOutputMessage::Begin { .. } => b'B',
        PgOutputMessage::Commit { .. } => b'C',
        PgOutputMessage::Origin { .. } => b'O',
        PgOutputMessage::Relation { .. } => b'R',
        PgOutputMessage::Type { .. } => b'Y',
        PgOutputMessage::Insert { .. } => b'I',
        PgOutputMessage::Update { .. } => b'U',
        PgOutputMessage::Delete { .. } => b'D',
        PgOutputMessage::Truncate { .. } => b'T',
        PgOutputMessage::Message { .. } => b'M',
        PgOutputMessage::StreamStart { .. } => b'S',
        PgOutputMessage::StreamStop => b'E',
        PgOutputMessage::StreamCommit { .. } => b'c',
        PgOutputMessage::StreamAbort { .. } => b'A',
        PgOutputMessage::BeginPrepare { .. } => b'b',
        PgOutputMessage::Prepare { .. } => b'P',
        PgOutputMessage::CommitPrepared { .. } => b'K',
        PgOutputMessage::RollbackPrepared { .. } => b'r',
        PgOutputMessage::StreamPrepare { .. } => b'p',
    }
}

/// Arbitrary bytes must not panic the decoder, and must not wedge it.
#[test]
fn a_hostile_pgoutput_body_is_refused_rather_than_fatal() {
    let mut rng = Rng::new(0x5eed_0f0f_1234_5678);
    let mut reached = Reached::default();

    for case in 0..CASES {
        // Three quarters tagged, one quarter noise: the tagged cases are what
        // reach the parser, the noise is what proves an unknown tag is handled
        // rather than assumed.
        let frame = match case % 4 {
            0 => valid_case(&mut rng),
            1 | 2 => mutated_case(&mut rng),
            _ => {
                if case % 8 == 3 {
                    noise_case(&mut rng)
                } else {
                    tagged_case(&mut rng)
                }
            }
        };

        // A fresh decoder each time. The stateful one is exercised below; here
        // the question is only whether one body can be fatal.
        match pgoutput::decode(&frame) {
            Ok(message) => {
                reached.decoded += 1;
                reached.accepted_tags.insert(tag_of(&message));
            }
            Err(_) => reached.rejected += 1,
        }
    }

    assert_eq!(
        reached.decoded + reached.rejected,
        CASES,
        "every case must reach a verdict"
    );

    println!(
        "  [pgoutput fuzz] {} decoded, {} rejected, {} distinct tags accepted: {:?}",
        reached.decoded,
        reached.rejected,
        reached.accepted_tags.len(),
        reached
            .accepted_tags
            .iter()
            .map(|tag| *tag as char)
            .collect::<Vec<_>>()
    );

    // The floors. These are what make the run evidence rather than exercise:
    // a corpus that never gets past the tag byte would satisfy "no panic"
    // perfectly while testing nothing. MEASURED 2026-08-24 with CASES=4096:
    // 2180 decoded across 9 tags (B C D I M O R T U). The floors sit below that
    // with room for the generator to drift, and well above what the corpus
    // reached BEFORE `valid_case` existed - 132 decoded across 3 fixed-layout
    // tags, which is the state these floors exist to refuse.
    // These floors are ABSOLUTE, not a fraction of `CASES`, so they stop being
    // evidence if you raise the corpus to hunt: at `CASES = 200000` a floor of
    // 1500 is met by the first 3% of the run. Run 2026-08-26 at that size passed
    // in 0.19s with no panic - which is worth something, but it is a crash floor,
    // NOT a coverage one. Recalibrate both numbers if you make a raised corpus
    // permanent.
    assert!(
        reached.decoded >= 1500,
        "only {} of {CASES} bodies decoded; the corpus is being rejected before \
         it reaches the parser and this file is no longer testing anything",
        reached.decoded
    );
    assert!(
        reached.accepted_tags.len() >= 8,
        "only {} distinct tags were ever accepted ({:?}); the corpus has stopped \
         covering the decoder's arms",
        reached.accepted_tags.len(),
        reached
            .accepted_tags
            .iter()
            .map(|tag| *tag as char)
            .collect::<Vec<_>>()
    );
}

/// One hostile body must not wedge the STATEFUL decoder.
///
/// The stateful decoder is the one a real stream uses, and it carries the
/// in-chunk xid state across frames. If a malformed frame could leave it
/// believing it is inside a chunk that never closes, every later frame would
/// be misparsed - a single bad frame poisoning the rest of the stream.
///
/// WHAT THIS DOES NOT CATCH: it does not assert the decoder's state is
/// UNCHANGED by a rejected frame, only that a well-formed frame still decodes
/// afterwards. Pinning the state itself would need an accessor the type does
/// not expose.
#[test]
fn a_rejected_frame_leaves_the_stateful_decoder_usable() {
    let mut rng = Rng::new(0xfeed_beef_dead_c0de);
    let mut decoder = pgoutput::Decoder::new();
    let mut survived = 0u32;

    // `StreamStop` is the shortest well-formed message there is: a bare tag.
    // After every hostile frame it must still decode, or the decoder has been
    // wedged by the frame before it.
    for _ in 0..512 {
        let hostile = tagged_case(&mut rng);
        let _ = decoder.decode(&hostile);

        match decoder.decode(b"E") {
            Ok(PgOutputMessage::StreamStop) => survived += 1,
            other => panic!(
                "a hostile frame wedged the decoder: a bare StreamStop then \
                 decoded as {other:?}"
            ),
        }
    }

    assert_eq!(survived, 512, "every probe must have been answered");
}

/// A transactional message carrying the in-chunk xid prefix.
///
/// Inside a stream chunk every `R Y I U D T M` message repeats an xid before
/// its payload. It need NOT equal the chunk's own xid: a change made after a
/// SAVEPOINT carries the subtransaction's. `valid_case` builds the unprefixed
/// form, so the prefix is spliced in after the tag.
fn prefixed(body: Vec<u8>, xid: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 4);
    out.push(body[0]);
    out.extend_from_slice(&xid.to_be_bytes());
    out.extend_from_slice(&body[1..]);
    out
}

/// Tags that take the in-chunk xid prefix, per `decode_frame`.
const PREFIXED_TAGS: &[u8] = b"RYIUDTM";

fn stream_start(xid: u32) -> Vec<u8> {
    let mut f = vec![b'S'];
    f.extend_from_slice(&xid.to_be_bytes());
    f.push(1);
    f
}

fn stream_commit(xid: u32) -> Vec<u8> {
    let mut f = vec![b'c'];
    f.extend_from_slice(&xid.to_be_bytes());
    f.push(0);
    f.extend_from_slice(&[0u8; 24]);
    f
}

fn stream_abort(xid: u32, subxid: u32) -> Vec<u8> {
    let mut f = vec![b'A'];
    f.extend_from_slice(&xid.to_be_bytes());
    f.extend_from_slice(&subxid.to_be_bytes());
    f
}

/// One sequence of frames destined for a single `Decoder`.
///
/// Three shapes, because they answer different questions: a well-formed chunk
/// reaches the state machine at all, a misframed one reaches the arms that
/// have no chunk to work with, and a mutated one lands between the two.
fn sequence(rng: &mut Rng) -> Vec<Vec<u8>> {
    let xid = 0x000a_b400 + rng.below(4) as u32;
    match rng.below(4) {
        // Well-formed chunk.
        0 | 1 => {
            let mut frames = vec![stream_start(xid)];
            for _ in 0..rng.below(4) {
                let body = valid_case(rng);
                if PREFIXED_TAGS.contains(&body[0]) {
                    // SOMETIMES a different xid: the SAVEPOINT case, which is
                    // the one this corpus exists to keep decodable.
                    let carried = if rng.below(2) == 0 { xid } else { xid + 1 };
                    frames.push(prefixed(body, carried));
                } else {
                    frames.push(body);
                }
            }
            frames.push(match rng.below(3) {
                0 => vec![b'E'],
                1 => stream_commit(xid),
                _ => stream_abort(xid, xid + 1),
            });
            frames
        }
        // Framing deliberately wrong.
        2 => match rng.below(4) {
            0 => vec![vec![b'E']],
            1 => vec![stream_start(xid), stream_start(xid + 1)],
            2 => vec![prefixed(valid_case(rng), xid)],
            _ => vec![stream_start(xid), stream_commit(xid + 7)],
        },
        // Well-formed, then damaged.
        _ => {
            let mut frames = vec![stream_start(xid)];
            frames.push(mutated_case(rng));
            frames.push(vec![b'E']);
            frames
        }
    }
}

/// What a sequence run observed.
#[derive(Default)]
struct SequenceReach {
    sequences: u32,
    opened_a_chunk: u32,
    in_chunk_tags: std::collections::BTreeSet<u8>,
    foreign_xid_accepted: u32,
}

/// Whole SEQUENCES through one stateful decoder, which is the only way to
/// reach its state machine.
///
/// `a_hostile_pgoutput_body_is_refused_rather_than_fatal` feeds single bodies
/// to the stateless `decode`, and that cannot enter the chunk logic at all -
/// the stateless entry point refuses `S E c A p` outright.
///
/// One assertion here is about BEHAVIOUR rather than robustness, and it is the
/// reason this file bothers with sequences: an in-chunk xid that DIFFERS from
/// the chunk's must be accepted. Commit 17c09e7ce added a check that the two
/// must agree and shipped it; a transaction with a SAVEPOINT carries its
/// subtransaction's xid and was rejected. `tests/pgoutput_subtransactions.rs`
/// pins that against a live server. This pins it against a corpus.
///
/// WHAT THIS DOES NOT CATCH: it does not assert that a malformed ORDER is
/// refused. Some orders are indistinguishable from forward-compatible ones,
/// and the decoder tolerates trailing bytes on purpose, so demanding an error
/// would be a claim about the protocol rather than about the driver.
#[test]
fn a_hostile_pgoutput_sequence_is_refused_rather_than_fatal() {
    let mut rng = Rng::new(0x51ea_d0a1_b2c3_d4e5);
    let mut reach = SequenceReach::default();

    for _ in 0..SEQUENCES {
        let frames = sequence(&mut rng);
        let mut decoder = pgoutput::Decoder::new();
        let mut inside = false;
        let mut chunk_xid = None;

        reach.sequences += 1;
        for frame in &frames {
            match decoder.decode(frame) {
                Ok(PgOutputMessage::StreamStart { xid, .. }) => {
                    inside = true;
                    chunk_xid = Some(xid);
                    reach.opened_a_chunk += 1;
                }
                Ok(message) => {
                    let tag = tag_of(&message);
                    if inside {
                        reach.in_chunk_tags.insert(tag);
                        if let (Some(chunk), Some(carried)) = (chunk_xid, carried_xid(&message))
                            && carried != chunk
                        {
                            reach.foreign_xid_accepted += 1;
                        }
                    }
                    if matches!(tag, b'E' | b'c' | b'A' | b'p') {
                        inside = false;
                    }
                }
                Err(_) => {}
            }
        }

        // A bad sequence must not wedge the decoder. `StreamStop` is the
        // shortest well-formed frame there is.
        assert!(
            matches!(decoder.decode(b"E"), Ok(PgOutputMessage::StreamStop)),
            "a sequence wedged the decoder"
        );
    }

    println!(
        "  [pgoutput sequence fuzz] {} sequences, {} opened a chunk, {} distinct in-chunk tags {:?}, {} foreign xids accepted",
        reach.sequences,
        reach.opened_a_chunk,
        reach.in_chunk_tags.len(),
        reach
            .in_chunk_tags
            .iter()
            .map(|tag| *tag as char)
            .collect::<Vec<_>>(),
        reach.foreign_xid_accepted
    );

    // Floors, in the same spirit as the single-body run: a corpus that never
    // opens a chunk cannot have tested the state machine, however green it is.
    // MEASURED 2026-08-24 with SEQUENCES=2048.
    assert!(
        reach.opened_a_chunk >= 800,
        "only {} sequences opened a chunk; the corpus is not reaching the state \
         machine",
        reach.opened_a_chunk
    );
    assert!(
        reach.in_chunk_tags.len() >= 4,
        "only {} distinct tags decoded inside a chunk ({:?})",
        reach.in_chunk_tags.len(),
        reach
            .in_chunk_tags
            .iter()
            .map(|tag| *tag as char)
            .collect::<Vec<_>>()
    );
    // The behavioural one. If a future change reinstates the xid equality
    // check, this drops to zero long before any live test notices.
    assert!(
        reach.foreign_xid_accepted >= 100,
        "only {} messages carried an xid differing from their chunk's and were \
         accepted; a SAVEPOINT's subtransaction xid must not be refused",
        reach.foreign_xid_accepted
    );
}

/// The xid a decoded message carried inside its chunk, if it takes one.
fn carried_xid(message: &PgOutputMessage) -> Option<u32> {
    match message {
        PgOutputMessage::Relation { xid, .. }
        | PgOutputMessage::Type { xid, .. }
        | PgOutputMessage::Insert { xid, .. }
        | PgOutputMessage::Update { xid, .. }
        | PgOutputMessage::Delete { xid, .. }
        | PgOutputMessage::Truncate { xid, .. }
        | PgOutputMessage::Message { xid, .. } => *xid,
        _ => None,
    }
}
