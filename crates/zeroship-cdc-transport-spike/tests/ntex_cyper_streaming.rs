//! The four transport answers `docs/proposals/2026-08-28-cdc-service.md` says
//! it does not have, each as a test rather than an argument.
//!
//! The proposal's Status block refuses to put hours on the relay while this is
//! unknown, and its Transport section commits to a specific pair: "Server is
//! `ntex` v3 with `compio` ... client is `cyper` with its `stream` feature."
//! These tests drive that exact pair over a real TCP socket on the versions the
//! workspace pins.
//!
//! 1. `ntex_streams_incrementally_and_cyper_consumes_incrementally`
//!    The client observes frame N before the server has produced frame N+1.
//!    Enforced two ways, because a test that only checks the final bytes proves
//!    buffering rather than streaming:
//!      * a RENDEZVOUS - the handler will not produce frame N+1 until the
//!        client acknowledges frame N, so a buffered response deadlocks and the
//!        test fails on its timeout instead of passing on the bytes;
//!      * a TRANSCRIPT - both sides append to one shared log, and the test
//!        requires strict `produced 0, consumed 0, produced 1, consumed 1, ...`
//!        alternation. A response assembled server-side cannot produce it.
//!
//! 2. Same test: the client is `cyper::Response::bytes_stream()`, the API
//!    behind the `stream` feature, and it yields chunks as they arrive.
//!
//! 3. Same test, plus `both_ends_run_on_compio_with_no_tokio_worker_thread`:
//!    the handler and the client each assert they are on a compio runtime, and
//!    the process's own thread names are enumerated after the exchange.
//!
//! 4. `a_stalled_consumer_stops_the_transport_pulling_and_the_producer_sheds`,
//!    `the_slow_consumer_timeout_closes_the_response`, and the one-variable
//!    control `a_prompt_consumer_sheds_nothing`: what the transport does when
//!    the consumer is slower than the producer, and whether the proposal's
//!    "the relay sheds, it never blocks" is expressible over it.

// ntex's runtime is single-threaded by construction - `TestServer` holds an
// `Rc` pipeline and a `Cell<Option<Arbiter>>`, and `ShedBufferBody` is `Rc`
// backed because `streaming()` does not require `Send`. Every future here is
// therefore correctly `!Send`, and the nursery lint has nothing to warn about.
#![allow(clippy::future_not_send)]

use std::{
    io,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use futures::StreamExt;
use ntex::util::Bytes;
use ntex::web::{self, HttpResponse};
use zeroship_cdc_transport_spike::{
    encode_frame, FrameDecoder, Push, ShedBuffer, CDC_MEDIA_TYPE,
};

/// Stands in for the proposal's binary `SubscribeRequest`. Its only job here is
/// to make the request a POST WITH A BODY: a stack that buffers the request
/// before invoking the handler would still pass a GET-shaped test.
const SUBSCRIBE_REQUEST_BODY: &[u8] = b"zs-cdc-subscribe-v1\0spike";

const SUBSCRIBE_PATH: &str = "/internal/v1/cdc/subscribe";

/// Every await in these tests is bounded. An unbounded one turns "the response
/// is buffered" - the exact failure being tested for - into a hung suite.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// 1 + 2 + 3: incremental production, incremental consumption, on compio
// ---------------------------------------------------------------------------

const FRAMES: u32 = 8;

#[derive(Clone)]
struct Rendezvous {
    /// Acks from the client. The handler awaits one per frame.
    acks: flume::Receiver<u32>,
    /// Frames the handler has produced. The client asserts against this at the
    /// moment it decodes each frame.
    produced: Arc<AtomicU32>,
    transcript: Arc<Mutex<Vec<String>>>,
    /// Set by the handler: was it running on a compio runtime?
    handler_on_compio: Arc<AtomicBool>,
    /// Set by the handler: did the POST body arrive intact?
    request_body_ok: Arc<AtomicBool>,
}

async fn subscribe(state: web::types::State<Rendezvous>, body: Bytes) -> HttpResponse {
    let rz: Rendezvous = state.get_ref().clone();

    rz.request_body_ok
        .store(body.as_ref() == SUBSCRIBE_REQUEST_BODY, Ordering::SeqCst);
    // Point 3, server side. `try_with_current` is the non-panicking form, so a
    // negative answer is a failed assertion in the test rather than a panic
    // swallowed on the server thread.
    rz.handler_on_compio.store(
        compio::runtime::Runtime::try_with_current(|_| ()).is_ok(),
        Ordering::SeqCst,
    );

    let (tx, rx) = ntex::channel::mpsc::channel::<Result<Bytes, io::Error>>();

    ntex::rt::spawn(async move {
        for i in 0..FRAMES {
            rz.transcript
                .lock()
                .expect("transcript lock")
                .push(format!("produced {i}"));
            rz.produced.store(i + 1, Ordering::SeqCst);
            if tx
                .send(Ok(encode_frame(format!("frame-{i}").as_bytes())))
                .is_err()
            {
                return;
            }
            // The rendezvous. Until the client says it has frame i, frame i+1
            // does not exist. If the transport buffered, this never returns and
            // the client's timeout fires.
            if rz.acks.recv_async().await.is_err() {
                return;
            }
        }
        // Dropping `tx` closes the response body.
    });

    HttpResponse::Ok().content_type(CDC_MEDIA_TYPE).streaming(rx)
}

#[ntex::test]
async fn ntex_streams_incrementally_and_cyper_consumes_incrementally() {
    let (ack_tx, ack_rx) = flume::unbounded::<u32>();
    let produced = Arc::new(AtomicU32::new(0));
    let transcript = Arc::new(Mutex::new(Vec::<String>::new()));
    let handler_on_compio = Arc::new(AtomicBool::new(false));
    let request_body_ok = Arc::new(AtomicBool::new(false));

    let state = Rendezvous {
        acks: ack_rx,
        produced: Arc::clone(&produced),
        transcript: Arc::clone(&transcript),
        handler_on_compio: Arc::clone(&handler_on_compio),
        request_body_ok: Arc::clone(&request_body_ok),
    };

    let srv = web::test::server(move || {
        let state = state.clone();
        async move {
            web::App::new().state(state).service(
                web::resource(SUBSCRIBE_PATH).route(web::post().to(subscribe)),
            )
        }
    })
    .await;

    // Point 3, client side.
    assert!(
        compio::runtime::Runtime::try_with_current(|_| ()).is_ok(),
        "the cyper client half must run on a compio runtime"
    );

    let resp = cyper::Client::new()
        .post(srv.url(SUBSCRIBE_PATH))
        .expect("build request")
        .header("content-type", CDC_MEDIA_TYPE)
        .expect("content-type")
        .header("accept", CDC_MEDIA_TYPE)
        .expect("accept")
        .body(SUBSCRIBE_REQUEST_BODY.to_vec())
        .send()
        .await
        .expect("send subscribe");

    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some(CDC_MEDIA_TYPE)
    );
    assert!(
        resp.headers().get("content-length").is_none(),
        "a Content-Length here would mean the body was assembled before the \
         headers were sent, which is the buffered case"
    );

    let mut stream = Box::pin(resp.bytes_stream());
    let mut decoder = FrameDecoder::new();
    let mut seen: u32 = 0;
    let mut chunks: u32 = 0;
    let mut chunk_sizes: Vec<usize> = Vec::new();

    while seen < FRAMES {
        let chunk = ntex::time::timeout(STEP_TIMEOUT, stream.next())
            .await
            .unwrap_or_else(|()| {
                panic!(
                    "timed out with {seen}/{FRAMES} frames decoded: either the \
                     response body is buffered, or the client is not yielding \
                     items as they arrive"
                )
            })
            .expect("body ended before all frames arrived")
            .expect("chunk error");
        chunks += 1;
        chunk_sizes.push(chunk.len());
        decoder.feed(&chunk);

        while let Some(frame) = decoder.next_frame() {
            assert_eq!(frame, format!("frame-{seen}").into_bytes());
            // THE ORDERING ASSERTION. At the instant the client holds frame
            // `seen`, the server has produced exactly `seen + 1` frames - it is
            // parked on the ack for this one.
            assert_eq!(
                produced.load(Ordering::SeqCst),
                seen + 1,
                "client holds frame {seen} but the server has produced {} frames",
                produced.load(Ordering::SeqCst)
            );
            transcript
                .lock()
                .expect("transcript lock")
                .push(format!("consumed {seen}"));
            ack_tx.send(seen).expect("ack");
            seen += 1;
        }
    }

    let tail = ntex::time::timeout(STEP_TIMEOUT, stream.next())
        .await
        .expect("timed out waiting for the body to close");
    assert!(tail.is_none(), "expected end of body, got another chunk");
    assert_eq!(decoder.pending_bytes(), 0);

    let log = transcript.lock().expect("transcript lock").clone();
    let expected: Vec<String> = (0..FRAMES)
        .flat_map(|i| [format!("produced {i}"), format!("consumed {i}")])
        .collect();
    assert_eq!(
        log, expected,
        "production and consumption must strictly alternate; a buffered \
         response produces all frames before the first is consumed"
    );

    assert!(
        request_body_ok.load(Ordering::SeqCst),
        "the POST body did not reach the handler intact"
    );
    assert!(
        handler_on_compio.load(Ordering::SeqCst),
        "the ntex handler was not on a compio runtime"
    );

    println!("[proof 1+2] transcript: {log:?}");
    println!(
        "[proof 1+2] {FRAMES} frames arrived in {chunks} transport chunks, sizes {chunk_sizes:?}"
    );
}

#[ntex::test]
async fn both_ends_run_on_compio_with_no_tokio_worker_thread() {
    assert!(
        compio::runtime::Runtime::try_with_current(|_| ()).is_ok(),
        "the test itself must be on compio"
    );

    // A direct observation, and an honest one about its limits: this catches a
    // tokio MULTI-THREAD runtime, whose workers are named `tokio-runtime-worker`.
    // A current-thread tokio runtime driven on an existing thread would not
    // appear here. What rules that out is the other test passing at all - a
    // hyper path that reached `tokio::net` or `tokio::time` without a reactor
    // panics with "there is no reactor running" rather than streaming frames.
    let mut names = Vec::new();
    let dir = std::fs::read_dir("/proc/self/task").expect("read /proc/self/task");
    for entry in dir {
        let path = entry.expect("task entry").path().join("comm");
        if let Ok(name) = std::fs::read_to_string(&path) {
            names.push(name.trim().to_string());
        }
    }
    names.sort_unstable();
    names.dedup();
    assert!(
        !names.iter().any(|n| n.contains("tokio")),
        "a tokio runtime thread exists in this process: {names:?}"
    );
    println!("[proof 3] thread names in this process: {names:?}");
}

// ---------------------------------------------------------------------------
// 4: the consumer is slower than the producer
// ---------------------------------------------------------------------------
//
// THE FIRST VERSION OF THIS SECTION WAS WRONG AND IS WORTH RECORDING, because
// its failure looked exactly like a transport defect. It pushed one 4 KiB frame
// per millisecond - 4 MB/s - and asserted the transport stopped pulling within
// 700 ms of the consumer stalling. It did not: 43 frames (176,300 bytes) at
// 700 ms, 94 at 1,500 ms, still climbing, and the assertion read "write
// backpressure does not reach the body stream". That conclusion was false. This
// box's `net.ipv4.tcp_wmem` maxes at 4,194,304 and `tcp_rmem` at 6,291,456, so
// a loopback socket pair alone can absorb ~10 MiB before anything pushes back,
// which at 4 MB/s takes seconds. The producer was slower than the buffers, so
// the experiment measured the buffers, not the backpressure.
//
// Two consequences, both kept: the producer now pushes a batch per tick so it
// outruns the socket, and the test WAITS FOR THE PLATEAU rather than asserting
// one at a hardcoded instant. The plateau's byte count is the answer to "what
// happens when the consumer is slower than the producer", and hardcoding a
// window would have hidden it.

/// 4 KiB payload plus the 4-byte length prefix.
const FIREHOSE_PAYLOAD: usize = 4096;
/// Frames offered per producer tick. 8 x 4100 bytes every millisecond is about
/// 32 MB/s, which outruns the socket buffers in well under a second and which a
/// reading loopback client keeps up with without shedding.
const PUSH_BATCH: u64 = 8;
/// Small on purpose: the question is whether the producer can be stalled, and a
/// large buffer only delays the answer.
const EGRESS_CAPACITY_BYTES: usize = 64 * 1024;
/// Long enough to never fire. The arm that measures the plateau must not have
/// its producer shut down underneath it.
const NO_SLOW_CONSUMER_CLOSE: Duration = Duration::from_secs(3600);
/// The proposal's `slow_consumer_timeout` knob, shrunk from 15 s so a test can
/// observe it.
const SLOW_CONSUMER_TIMEOUT: Duration = Duration::from_millis(500);
/// Only the control arm caps frames; it needs a natural end. The stalled arms
/// must not hit a cap, because a producer that stopped on its own would fake
/// the plateau they exist to measure.
const CONTROL_FRAME_CAP: u64 = 4096;
const NO_FRAME_CAP: u64 = u64::MAX;

/// How long to wait for the transport to stop pulling, and how many unchanged
/// samples count as stopped.
const PLATEAU_SAMPLE_MS: u32 = 100;
const PLATEAU_SAMPLES_TO_CONFIRM: u32 = 3;
const PLATEAU_DEADLINE_SAMPLES: u32 = 150;
/// How long the slow-consumer arm reads nothing: four times the timeout.
const STALL_MS: u32 = 2000;

#[derive(Clone)]
struct Firehose {
    /// `push` calls that were queued.
    accepted: Arc<AtomicU64>,
    /// `push` calls the bounded buffer refused. The producer kept going.
    shed: Arc<AtomicU64>,
    /// Frames the TRANSPORT took, republished from the server thread so the
    /// test can sample it. This only moves when ntex polls the body, which it
    /// only does when `io.poll_flush` reports the write buffer below its
    /// high-water mark (`ntex-3.7.2/src/http/h1/dispatcher.rs`,
    /// `poll_send_payload`).
    delivered: Arc<AtomicU64>,
    delivered_bytes: Arc<AtomicU64>,
    /// Proof that the producer is still running. If a consumer could stall it,
    /// this would stop advancing while the consumer read nothing.
    producer_iterations: Arc<AtomicU64>,
    closed_for_slow_consumer: Arc<AtomicBool>,
    closed_for_frame_cap: Arc<AtomicBool>,
    /// Test-driven shutdown, for the arm whose producer must outlive the
    /// measurement.
    stop: Arc<AtomicBool>,
    slow_consumer_timeout: Duration,
    frame_cap: u64,
}

async fn firehose(state: web::types::State<Firehose>, _body: Bytes) -> HttpResponse {
    let st: Firehose = state.get_ref().clone();
    let (buf, body) = ShedBuffer::with_capacity(EGRESS_CAPACITY_BYTES);

    ntex::rt::spawn(async move {
        let frame = encode_frame(&vec![0xAB; FIREHOSE_PAYLOAD]);
        'produce: loop {
            st.producer_iterations.fetch_add(1, Ordering::SeqCst);
            for _ in 0..PUSH_BATCH {
                if st.accepted.load(Ordering::SeqCst) >= st.frame_cap {
                    st.closed_for_frame_cap.store(true, Ordering::SeqCst);
                    break 'produce;
                }
                match buf.push(frame.clone()) {
                    Push::Accepted => st.accepted.fetch_add(1, Ordering::SeqCst),
                    Push::Shed => st.shed.fetch_add(1, Ordering::SeqCst),
                };
            }
            st.delivered
                .store(buf.delivered_frames(), Ordering::SeqCst);
            st.delivered_bytes
                .store(buf.delivered_bytes(), Ordering::SeqCst);

            if buf
                .full_for()
                .is_some_and(|d| d >= st.slow_consumer_timeout)
            {
                st.closed_for_slow_consumer.store(true, Ordering::SeqCst);
                break 'produce;
            }
            if st.stop.load(Ordering::SeqCst) {
                break 'produce;
            }
            // The producer's ONLY await is its own clock. Nothing here yields to
            // the consumer, and `push` could not be made to even if someone
            // tried: it is not `async` and takes `&self`.
            ntex::time::sleep(ntex::time::Millis(1)).await;
        }
        buf.close();
    });

    HttpResponse::Ok()
        .content_type(CDC_MEDIA_TYPE)
        .streaming(body)
}

fn firehose_state(slow_consumer_timeout: Duration, frame_cap: u64) -> Firehose {
    Firehose {
        accepted: Arc::new(AtomicU64::new(0)),
        shed: Arc::new(AtomicU64::new(0)),
        delivered: Arc::new(AtomicU64::new(0)),
        delivered_bytes: Arc::new(AtomicU64::new(0)),
        producer_iterations: Arc::new(AtomicU64::new(0)),
        closed_for_slow_consumer: Arc::new(AtomicBool::new(false)),
        closed_for_frame_cap: Arc::new(AtomicBool::new(false)),
        stop: Arc::new(AtomicBool::new(false)),
        slow_consumer_timeout,
        frame_cap,
    }
}

async fn firehose_server(state: Firehose) -> web::test::TestServer {
    web::test::server(move || {
        let state = state.clone();
        async move {
            web::App::new()
                .state(state)
                .service(web::resource(SUBSCRIBE_PATH).route(web::post().to(firehose)))
        }
    })
    .await
}

async fn open_firehose(srv: &web::test::TestServer) -> cyper::Response {
    let resp = cyper::Client::new()
        .post(srv.url(SUBSCRIBE_PATH))
        .expect("build request")
        .body(SUBSCRIBE_REQUEST_BODY.to_vec())
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status().as_u16(), 200);
    resp
}

/// Drain until end of body, returning (chunks, bytes).
async fn drain<S, B, E>(stream: &mut std::pin::Pin<Box<S>>) -> (u64, u64)
where
    S: futures::Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
    E: std::fmt::Debug,
{
    let (mut chunks, mut bytes) = (0u64, 0u64);
    loop {
        let next = ntex::time::timeout(STEP_TIMEOUT, stream.next())
            .await
            .expect("timed out draining the body");
        match next {
            Some(chunk) => {
                bytes += chunk.expect("chunk error").as_ref().len() as u64;
                chunks += 1;
            }
            None => return (chunks, bytes),
        }
    }
}

#[ntex::test]
async fn a_stalled_consumer_stops_the_transport_pulling_and_the_producer_sheds() {
    let st = firehose_state(NO_SLOW_CONSUMER_CLOSE, NO_FRAME_CAP);
    let srv = firehose_server(st.clone()).await;
    let mut stream = Box::pin(open_firehose(&srv).await.bytes_stream());

    // One chunk, then stop reading and hold the connection open.
    let first = ntex::time::timeout(STEP_TIMEOUT, stream.next())
        .await
        .expect("timed out on the first chunk")
        .expect("body ended immediately")
        .expect("chunk error");
    assert!(!first.is_empty());

    // Wait for the transport to stop taking frames, rather than assuming when.
    let mut last = u64::MAX;
    let mut stable = 0u32;
    let mut plateau = None;
    let iterations_at_stall = st.producer_iterations.load(Ordering::SeqCst);
    for _ in 0..PLATEAU_DEADLINE_SAMPLES {
        ntex::time::sleep(ntex::time::Millis(PLATEAU_SAMPLE_MS)).await;
        let now = st.delivered.load(Ordering::SeqCst);
        if now == last {
            stable += 1;
            if stable >= PLATEAU_SAMPLES_TO_CONFIRM {
                plateau = Some(now);
                break;
            }
        } else {
            stable = 0;
            last = now;
        }
    }
    let iterations_after = st.producer_iterations.load(Ordering::SeqCst);
    let plateau_frames = plateau.unwrap_or_else(|| {
        panic!(
            "the transport never stopped pulling: {} frames taken after \
             {} ms with a consumer reading nothing. Write backpressure does \
             not reach the body stream, so a bounded egress buffer cannot \
             bound anything.",
            st.delivered.load(Ordering::SeqCst),
            u64::from(PLATEAU_DEADLINE_SAMPLES) * u64::from(PLATEAU_SAMPLE_MS)
        )
    });

    println!(
        "[proof 4] stalled consumer: the transport absorbed {plateau_frames} frames \
         ({} bytes) and then stopped pulling; producer iterations {iterations_at_stall} \
         -> {iterations_after}; accepted {} shed {}",
        st.delivered_bytes.load(Ordering::SeqCst),
        st.accepted.load(Ordering::SeqCst),
        st.shed.load(Ordering::SeqCst)
    );

    assert!(
        st.shed.load(Ordering::SeqCst) > 0,
        "a stalled consumer must make the bounded buffer shed"
    );
    assert!(
        iterations_after > iterations_at_stall,
        "the producer stopped running while the consumer was stalled: the \
         consumer CAN stall the producer, which is the failure this shape \
         exists to prevent"
    );
    assert!(
        !st.closed_for_slow_consumer.load(Ordering::SeqCst),
        "this arm must not end via the slow-consumer close; that is the next test"
    );

    // Resume: the client drains what the socket buffered, the producer sees the
    // stop flag, and the body ends.
    st.stop.store(true, Ordering::SeqCst);
    let (chunks, bytes) = drain(&mut stream).await;
    println!("[proof 4] client then drained {chunks} chunks / {bytes} bytes, then EOF");
}

#[ntex::test]
async fn the_slow_consumer_timeout_closes_the_response() {
    let st = firehose_state(SLOW_CONSUMER_TIMEOUT, NO_FRAME_CAP);
    let srv = firehose_server(st.clone()).await;
    let mut stream = Box::pin(open_firehose(&srv).await.bytes_stream());

    let first = ntex::time::timeout(STEP_TIMEOUT, stream.next())
        .await
        .expect("timed out on the first chunk")
        .expect("body ended immediately")
        .expect("chunk error");
    assert!(!first.is_empty());

    // Stall for four times the timeout, reading nothing.
    ntex::time::sleep(ntex::time::Millis(STALL_MS)).await;

    assert!(
        st.closed_for_slow_consumer.load(Ordering::SeqCst),
        "slow_consumer_timeout never fired: the buffer was never continuously \
         full, or the producer could not observe that it was"
    );
    assert!(
        st.shed.load(Ordering::SeqCst) > 0,
        "the close must follow shedding, not precede it"
    );

    let (chunks, bytes) = drain(&mut stream).await;
    println!(
        "[proof 4] slow_consumer_timeout ({} ms) closed the response; the client \
         then drained {chunks} chunks / {bytes} bytes and saw EOF; accepted {} shed {}",
        SLOW_CONSUMER_TIMEOUT.as_millis(),
        st.accepted.load(Ordering::SeqCst),
        st.shed.load(Ordering::SeqCst)
    );
}

/// The one-variable control: same server, same producer, same buffer, same
/// rate. Only the consumer's speed differs.
#[ntex::test]
async fn a_prompt_consumer_sheds_nothing() {
    let st = firehose_state(SLOW_CONSUMER_TIMEOUT, CONTROL_FRAME_CAP);
    let srv = firehose_server(st.clone()).await;
    let mut stream = Box::pin(open_firehose(&srv).await.bytes_stream());

    let (chunks, bytes) = drain(&mut stream).await;

    println!(
        "[proof 4 control] prompt consumer: {chunks} chunks / {bytes} bytes; \
         accepted {} shed {} delivered {}",
        st.accepted.load(Ordering::SeqCst),
        st.shed.load(Ordering::SeqCst),
        st.delivered.load(Ordering::SeqCst)
    );

    assert_eq!(
        st.shed.load(Ordering::SeqCst),
        0,
        "a consumer that keeps up must not cause a shed; if it does, the shed \
         in the stalled arms is not attributable to the stall"
    );
    assert!(
        st.closed_for_frame_cap.load(Ordering::SeqCst),
        "the control must end at the frame cap"
    );
    assert!(
        !st.closed_for_slow_consumer.load(Ordering::SeqCst),
        "slow_consumer_timeout must not fire against a prompt consumer"
    );
    assert_eq!(
        bytes,
        CONTROL_FRAME_CAP * (FIREHOSE_PAYLOAD as u64 + 4),
        "every accepted frame must reach the client byte for byte"
    );
}
