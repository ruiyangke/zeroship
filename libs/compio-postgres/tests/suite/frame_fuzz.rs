//! Randomised backend frames, to generalise the hand-written hostile shapes.
//!
//! `libs/compio-postgres/tests/suite/hostile_peer.rs` pins eight specific violations - an unknown tag, a
//! lying length, a message out of order. Each was chosen by a person, which
//! means the set is exactly as imaginative as whoever wrote it. This file feeds
//! the same client bytes nobody chose.
//!
//! WHAT IS ASSERTED, and deliberately no more:
//!
//!   * the operation TERMINATES inside a watchdog - the driver never waits
//!     forever for bytes that are not coming, and never spins,
//!   * it does not PANIC - a panic in a network client is reachable by the peer,
//!   * whatever it returns, the CLIENT IS LEFT IN A DEFINED STATE, so a
//!     follow-up either works or fails, and never hangs.
//!
//! It is NOT asserted that a random response produces an error. Random bytes
//! occasionally form a legitimate reply, and a test demanding failure would be
//! wrong about the protocol rather than about the driver. This is the standard
//! fuzzing bargain: weak per-case assertions, enormous case count.
//!
//! DETERMINISTIC BY CONSTRUCTION. The generator is a seeded xorshift written
//! inline - no `rand`, no dependency added to a manifest - and the seed for each
//! case is printed when it fails, so any failure replays exactly. A fuzzer whose
//! failures cannot be reproduced is a rumour generator.
//!
//! The case count is deliberately modest so this stays inside a normal test run.
//! It is a floor on coverage, not a soak: raise `CASES` locally to hunt.
//!
//! RAISE `ASYNC_WATCHDOG` IN THE SAME EDIT. It is sized for `CASES = 48`, and a
//! hunt that only raises the case count trips the test's OWN outer watchdog and
//! reports `frame fuzzing exceeded its outer watchdog: Elapsed(())`. That reads
//! exactly like a driver hang - it is not one, it is this file timing itself out.
//! Cost two runs on 2026-08-26 before the message was traced back here. MEASURED
//! that day: `CASES = 1000` with `ASYNC_WATCHDOG = 600` completes in 251s, all 15
//! tests pass, no panic and no hang. So the decoder survives 20x this corpus; the
//! hunt is worth running, it just needs both constants moved together.
//!
//! WHAT THE FRAME CORPUS REACHES, measured 2026-08-23 by printing every case's
//! outcome. The handshake section below has carried such a measurement since it
//! was written; this half had none, and "the generator reaches the driver" was
//! being taken on the strength of the generator's source. It does: all 48 cases
//! produce a driver error and none returns rows. 28 of them name the content -
//! 11 distinct unknown message tags (13 cases), "invalid message length:
//! expected buffer to be empty" (8), "unexpected message from server" (5),
//! unexpected EOF (1). The other 20 report only "connection closed", which is
//! the in-flight query's view after the CONNECTION TASK has taken the real
//! error; the diagnosis exists, it is just not on the path the query sees.
//! That is why the per-case assertions below are about termination and defined
//! state rather than about error text: for 20 of 48 there is no error text to
//! be about, and a test demanding one would be measuring which side of the
//! channel won a race.

use compio_postgres::Config;
use compio_postgres::config::SslMode;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

#[allow(unused_imports)]
use crate::common;

const ASYNC_WATCHDOG: Duration = Duration::from_secs(30);
const SOCKET_WATCHDOG: Duration = Duration::from_secs(2);
const THREAD_WATCHDOG: Duration = Duration::from_secs(3);
const OPERATION_WATCHDOG: Duration = Duration::from_secs(2);
/// Enough to cover the generator's shapes several times over while keeping the
/// whole file well inside a normal `cargo test` run.
const CASES: u32 = 48;
/// Fixed so the corpus is identical on every machine and every run. Changing it
/// explores a different corpus; it does not make a previous failure disappear,
/// because the failing case prints its own seed.
const ROOT_SEED: u64 = 0x5eed_c0de_1234_9abc;

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

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: u32) -> u32 {
        u32::try_from(self.next_u64() % u64::from(bound)).expect("bound fits u32")
    }

    fn byte(&mut self) -> u8 {
        u8::try_from(self.next_u64() & 0xff).expect("masked to a byte")
    }
}

/// Every backend message tag PostgreSQL actually uses, so the generator spends
/// part of its budget on frames that are plausible rather than obviously wrong.
/// A wholly random tag is rejected immediately by the codec and exercises much
/// less of the driver than a real tag carrying a wrong body.
const REAL_TAGS: &[u8] = b"RKZTDCEInsvW1235AGHcdfGNS";

/// Build one scripted response. The shapes are weighted towards near-miss
/// frames, because bytes that are obviously not PostgreSQL get rejected at the
/// first check and tell us little.
fn generate_response(rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    let frames = 1 + rng.below(3);
    for _ in 0..frames {
        let tag = if rng.below(4) == 0 {
            rng.byte()
        } else {
            REAL_TAGS[rng.below(u32::try_from(REAL_TAGS.len()).expect("tag count fits")) as usize]
        };
        let body_len = rng.below(24) as usize;
        let mut body = Vec::with_capacity(body_len);
        for _ in 0..body_len {
            body.push(rng.byte());
        }
        // The declared length is usually honest, sometimes a lie in either
        // direction, and occasionally unrepresentable.
        let declared = match rng.below(8) {
            0 => rng.below(8),
            1 => u32::try_from(body.len() + 4).expect("body fits") + 1 + rng.below(64),
            2 => u32::try_from(body.len() + 4)
                .expect("body fits")
                .saturating_sub(1 + rng.below(4)),
            _ => u32::try_from(body.len() + 4).expect("body fits"),
        };
        out.push(tag);
        out.extend_from_slice(&declared.to_be_bytes());
        out.extend_from_slice(&body);
    }
    out
}

struct StubServer {
    addr: SocketAddr,
    done: std::sync::mpsc::Receiver<()>,
    thread: thread::JoinHandle<()>,
}

impl StubServer {
    fn spawn(script: impl FnOnce(TcpListener) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind scripted PostgreSQL peer");
        listener
            .set_nonblocking(true)
            .expect("make scripted listener bounded");
        let addr = listener.local_addr().expect("scripted listener address");
        let (done_tx, done) = std::sync::mpsc::channel();
        let thread = thread::spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                script(listener);
            }));
            let _ = done_tx.send(());
            if let Err(panic) = outcome {
                std::panic::resume_unwind(panic);
            }
        });
        Self { addr, done, thread }
    }

    fn finish(self) {
        self.done
            .recv_timeout(THREAD_WATCHDOG)
            .expect("scripted PostgreSQL peer exceeded its thread watchdog");
        self.thread
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    }
}

fn accept_bounded(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + SOCKET_WATCHDOG;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_read_timeout(Some(SOCKET_WATCHDOG))
                    .expect("set scripted peer read watchdog");
                stream
                    .set_write_timeout(Some(SOCKET_WATCHDOG))
                    .expect("set scripted peer write watchdog");
                return stream;
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "client did not connect before the scripted accept watchdog"
                );
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("scripted accept failed: {error}"),
        }
    }
}

fn backend_frame(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5 + body.len());
    frame.push(tag);
    frame.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
    frame.extend_from_slice(body);
    frame
}

fn complete_startup(stream: &mut TcpStream, process_id: i32) {
    let mut length = [0u8; 4];
    stream
        .read_exact(&mut length)
        .expect("read startup packet length");
    let length = u32::from_be_bytes(length) as usize;
    assert!(length >= 8, "startup packet is shorter than its header");
    let mut body = vec![0u8; length - 4];
    stream
        .read_exact(&mut body)
        .expect("read startup packet body");

    let mut response = backend_frame(b'R', &0u32.to_be_bytes());
    let mut key_data = Vec::with_capacity(8);
    key_data.extend_from_slice(&process_id.to_be_bytes());
    key_data.extend_from_slice(&1234i32.to_be_bytes());
    response.extend_from_slice(&backend_frame(b'K', &key_data));
    response.extend_from_slice(&backend_frame(b'Z', b"I"));
    stream
        .write_all(&response)
        .expect("write scripted startup response");
    stream.flush().expect("flush scripted startup response");
}

fn stub_config(addr: SocketAddr) -> Config {
    let mut config = Config::new();
    config
        .user("scripted-user")
        .hostaddr(addr.ip())
        .port(addr.port())
        .ssl_mode(SslMode::Disable)
        .connect_timeout(Duration::from_secs(1));
    config
}

/// Feed one generated response to a real client. Returns nothing: the claim is
/// that this function RETURNS AT ALL, without panicking, for every input.
async fn drive_one_case(seed: u64, response: Vec<u8>) {
    let server = StubServer::spawn(move |listener| {
        let mut stream = accept_bounded(&listener);
        complete_startup(&mut stream, 900);
        // Drain the query frame if it arrives; the client may also give up
        // first, and neither is a failure of the peer.
        let mut scratch = [0u8; 512];
        let _ = stream.read(&mut scratch);
        let _ = stream.write_all(&response);
        let _ = stream.flush();
        thread::sleep(Duration::from_millis(120));
    });

    let (client, connection) = match stub_config(server.addr).connect(common::suite_tls()).await {
        Ok(pair) => pair,
        Err(_) => {
            // A handshake this generator never touches; nothing to drive.
            server.finish();
            return;
        }
    };
    let driver = compio::runtime::spawn(async move { connection.run().await });

    // (1) TERMINATION. The failure this catches is a hang, which without the
    // timeout would present as the whole suite wedging on one unlucky seed.
    let _outcome = compio::time::timeout(OPERATION_WATCHDOG, client.simple_query("SELECT 1"))
        .await
        .unwrap_or_else(|_| {
            panic!("seed {seed:#x}: the driver hung on a generated response instead of returning")
        });

    // (2) DEFINED STATE. Whatever happened, the client answers a follow-up one
    // way or the other. A client that hangs here has been left mid-protocol
    // with no way back, which is the shape of every poisoning bug this crate
    // has had.
    let reuse = compio::time::timeout(OPERATION_WATCHDOG, client.simple_query("SELECT 2")).await;
    assert!(
        reuse.is_ok(),
        "seed {seed:#x}: the client hung on reuse after a generated response"
    );

    let _ = compio::time::timeout(OPERATION_WATCHDOG, driver).await;
    drop(client);
    server.finish();
}

/// The whole corpus, one case at a time. Panics are not caught: a panic
/// anywhere below is a real finding and should fail loudly with its seed.
#[compio::test]
async fn generated_backend_frames_never_hang_or_panic_the_driver() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let mut root = Rng::new(ROOT_SEED);
        for case in 0..CASES {
            let seed = root.next_u64();
            let mut rng = Rng::new(seed);
            let response = generate_response(&mut rng);
            assert!(
                !response.is_empty(),
                "case {case} generated an empty response, which exercises nothing"
            );
            drive_one_case(seed, response).await;
        }
    })
    .await
    .expect("frame fuzzing exceeded its outer watchdog");
}

// ---------------------------------------------------------------------------
// Handshake fuzzing.
//
// Everything above fuzzes frames AFTER a clean startup, so it never reaches
// `src/connect_raw.rs` - which holds 11 of the driver's 43
// `unexpected_message()` sites, the largest single cluster, and which neither
// `libs/compio-postgres/tests/suite/hostile_peer.rs` nor the corpus above touches. The handshake is also
// the part a client runs before it trusts anything, so a panic or a hang there
// is reachable by any peer that can complete a TCP accept.
//
// The generator answers the startup packet with plausible-looking authentication
// traffic rather than noise: real `R` frames carrying random auth codes, key
// data of the wrong size, `ReadyForQuery` arriving early, and error responses
// with malformed field sequences. Random bytes would be refused by the first
// length check and would leave the interesting states unvisited.
//
// MEASURED, so this is coverage rather than hope. Instrumenting the 48-case
// corpus to print each outcome gave eight distinct error classes, and 23 of the
// 48 reported "unexpected message from server" - that is
// `Error::unexpected_message()`, the connect_raw cluster this file exists to
// reach. Re-measured 2026-08-23, unchanged, and the full 48 are: unexpected
// message (23); unexpected EOF under two different wordings, "error parsing
// response from server" and "error communicating with the server" (8 each);
// "failed to fill whole buffer" (3); two different length validations,
// "expected buffer to be empty" (3) and "error fields is not drained" (1); an
// unknown authentication tag (1); an unsupported authentication method,
// Kerberos V5 (1). The enumeration here summed to 45 until that re-run, which
// is how the "failed to fill whole buffer" class went unlisted for a week while
// the class COUNT next to it said eight - the count was right and the list was
// short, and only adding them up finds that. No generated handshake ever
// completed successfully, so this corpus bounds the FAILURE paths and says
// nothing about the success path.
// ---------------------------------------------------------------------------

/// Build a scripted answer to a startup packet. Shapes are drawn from what a
/// broken or hostile server plausibly emits, not from uniform noise.
fn generate_handshake(rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    let steps = 1 + rng.below(4);
    for _ in 0..steps {
        match rng.below(6) {
            // An authentication request with an arbitrary code. Real codes are
            // 0, 2, 3, 5, 7, 8, 9, 10, 12; anything else must be refused rather
            // than treated as "no authentication required".
            0 => {
                let code = rng.below(16);
                out.extend_from_slice(&backend_frame(b'R', &code.to_be_bytes()));
            }
            // BackendKeyData with an arbitrary fixed or variable-length key.
            1 => {
                let len = rng.below(16) as usize;
                let mut body = Vec::with_capacity(len);
                for _ in 0..len {
                    body.push(rng.byte());
                }
                out.extend_from_slice(&backend_frame(b'K', &body));
            }
            // ParameterStatus with a body that may lack its NUL terminators.
            2 => {
                let len = rng.below(20) as usize;
                let mut body = Vec::with_capacity(len);
                for _ in 0..len {
                    body.push(rng.byte());
                }
                out.extend_from_slice(&backend_frame(b'S', &body));
            }
            // ReadyForQuery, possibly with an invalid transaction status byte
            // and possibly arriving before authentication has completed.
            3 => {
                let status = if rng.below(2) == 0 {
                    b"I".to_vec()
                } else {
                    vec![rng.byte()]
                };
                out.extend_from_slice(&backend_frame(b'Z', &status));
            }
            // An ErrorResponse whose field sequence may not terminate.
            4 => {
                let len = rng.below(24) as usize;
                let mut body = Vec::with_capacity(len);
                for _ in 0..len {
                    body.push(rng.byte());
                }
                out.extend_from_slice(&backend_frame(b'E', &body));
            }
            // A message that is well formed but has no business in a handshake.
            _ => {
                out.extend_from_slice(&backend_frame(b'D', b"\x00\x00"));
            }
        }
    }
    out
}

/// Attempt one connection against a scripted handshake. The claim is that this
/// RETURNS - with either a client or an error - and never panics or hangs.
async fn drive_one_handshake(seed: u64, response: Vec<u8>) {
    let server = StubServer::spawn(move |listener| {
        let mut stream = accept_bounded(&listener);
        // Read the startup packet, then answer with whatever was generated.
        let mut length = [0u8; 4];
        if stream.read_exact(&mut length).is_ok() {
            let length = u32::from_be_bytes(length) as usize;
            if (8..=1024 * 1024).contains(&length) {
                let mut body = vec![0u8; length - 4];
                let _ = stream.read_exact(&mut body);
            }
        }
        let _ = stream.write_all(&response);
        let _ = stream.flush();
        thread::sleep(Duration::from_millis(120));
    });

    // A connection that SUCCEEDS is a legitimate outcome: some generated
    // sequences are a valid trust handshake. Both arms are acceptable; only a
    // hang or a panic is not.
    let outcome = compio::time::timeout(
        OPERATION_WATCHDOG,
        stub_config(server.addr).connect(common::suite_tls()),
    )
    .await
    .unwrap_or_else(|_| {
        panic!("seed {seed:#x}: connect hung on a generated handshake instead of returning")
    });

    if let Ok((client, connection)) = outcome {
        let driver = compio::runtime::spawn(async move { connection.run().await });
        drop(client);
        let _ = compio::time::timeout(OPERATION_WATCHDOG, driver).await;
    }
    server.finish();
}

#[compio::test]
async fn generated_handshakes_never_hang_or_panic_the_driver() {
    compio::time::timeout(ASYNC_WATCHDOG, async {
        let mut root = Rng::new(ROOT_SEED ^ 0x484e_4453_484b_4521);
        for case in 0..CASES {
            let seed = root.next_u64();
            let mut rng = Rng::new(seed);
            let response = generate_handshake(&mut rng);
            assert!(
                !response.is_empty(),
                "case {case} generated an empty handshake, which exercises nothing"
            );
            drive_one_handshake(seed, response).await;
        }
    })
    .await
    .expect("handshake fuzzing exceeded its outer watchdog");
}
