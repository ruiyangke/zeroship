//! Transport spike for the CDC relay: the two pieces the live proofs need.
//!
//! This crate is not the relay and must not grow into it. See its `Cargo.toml`
//! for the scope statement and the deletion condition. The library half is two
//! things:
//!
//! * [`encode_frame`] / [`FrameDecoder`] - the length-prefixed framing the
//!   proposal already assumes ("a length-prefixed frame over a byte stream is
//!   carrier-independent"). It exists here because an HTTP chunk boundary is
//!   NOT a frame boundary, and a test that reads one chunk and calls it one
//!   frame would be measuring luck. This is deliberately NOT
//!   `zeroship-cdc-wire`'s codec: the point of the spike is the carrier, and
//!   pulling the real wire crate in would couple a throwaway experiment to the
//!   contract it is supposed to be independent of.
//!
//! * [`ShedBuffer`] - a bounded egress buffer whose `push` is not `async` and
//!   takes `&self`. The proposal's slow-consumer section names that signature
//!   as the seam ("if it ever needs awaiting, the compiler says so at every
//!   call site") and states the policy it enables: the relay sheds, it never
//!   blocks. The buffer is here so the backpressure test can ask whether the
//!   transport can express that policy at all, rather than arguing about it.
//!
//! The reader half implements `Stream<Item = Result<Bytes, io::Error>>`, which
//! is what `ntex::http::ResponseBuilder::streaming` accepts, so the answer is
//! not "in principle" - the test hands this exact type to a real ntex handler.

use std::{
    cell::RefCell,
    collections::VecDeque,
    io,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};

use futures::Stream;
use ntex::util::Bytes;

/// The media type the proposal's `POST /internal/v1/cdc/subscribe` declares on
/// both `Content-Type` and `Accept`.
pub const CDC_MEDIA_TYPE: &str = "application/vnd.zeroship.cdc.v1";

/// Width of the frame length prefix, big-endian.
pub const LENGTH_PREFIX_BYTES: usize = 4;

/// Encode one frame as a big-endian `u32` length followed by its payload.
///
/// # Panics
///
/// If `payload` is longer than `u32::MAX`. The relay caps a frame at
/// `max_frame_bytes` long before that; this spike has no frame that large.
#[must_use]
pub fn encode_frame(payload: &[u8]) -> Bytes {
    let len = u32::try_from(payload.len()).expect("frame payload fits in u32");
    let mut out = Vec::with_capacity(LENGTH_PREFIX_BYTES + payload.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    Bytes::from(out)
}

/// Reassembles frames from a byte stream whose chunk boundaries are arbitrary.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one transport chunk.
    pub fn feed(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// Pop the next complete frame, if one has arrived.
    ///
    /// # Panics
    ///
    /// On a platform where `usize` is narrower than `u32`. Not one we build for.
    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        if self.buf.len() < LENGTH_PREFIX_BYTES {
            return None;
        }
        let mut prefix = [0u8; LENGTH_PREFIX_BYTES];
        prefix.copy_from_slice(&self.buf[..LENGTH_PREFIX_BYTES]);
        let len = usize::try_from(u32::from_be_bytes(prefix)).expect("u32 fits in usize");
        let total = LENGTH_PREFIX_BYTES + len;
        if self.buf.len() < total {
            return None;
        }
        let frame = self.buf[LENGTH_PREFIX_BYTES..total].to_vec();
        self.buf.drain(..total);
        Some(frame)
    }

    /// Bytes held that do not yet complete a frame.
    #[must_use]
    pub const fn pending_bytes(&self) -> usize {
        self.buf.len()
    }
}

/// What [`ShedBuffer::push`] did with the frame.
///
/// There is no third arm and deliberately no `Err(WouldBlock)`: a caller that
/// could retry later is a caller the consumer can slow down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Push {
    /// Queued for the transport.
    Accepted,
    /// Dropped, because the buffer is at capacity or already closed. The
    /// producer keeps going.
    Shed,
}

#[derive(Debug)]
struct Inner {
    queue: VecDeque<Bytes>,
    queued_bytes: usize,
    capacity_bytes: usize,
    /// When the buffer first refused a frame and has not drained since. This is
    /// the input to the proposal's `slow_consumer_timeout`.
    full_since: Option<Instant>,
    closed: bool,
    shed_frames: u64,
    delivered_frames: u64,
    delivered_bytes: u64,
    waker: Option<Waker>,
}

/// Producer handle. Cheap to clone; every clone shares one queue.
#[derive(Debug, Clone)]
pub struct ShedBuffer {
    inner: Rc<RefCell<Inner>>,
}

/// Consumer half: the HTTP response body.
#[derive(Debug)]
pub struct ShedBufferBody {
    inner: Rc<RefCell<Inner>>,
}

impl ShedBuffer {
    /// Create a producer/body pair with a byte ceiling on the queue.
    #[must_use]
    pub fn with_capacity(capacity_bytes: usize) -> (Self, ShedBufferBody) {
        let inner = Rc::new(RefCell::new(Inner {
            queue: VecDeque::new(),
            queued_bytes: 0,
            capacity_bytes,
            full_since: None,
            closed: false,
            shed_frames: 0,
            delivered_frames: 0,
            delivered_bytes: 0,
            waker: None,
        }));
        (
            Self {
                inner: Rc::clone(&inner),
            },
            ShedBufferBody { inner },
        )
    }

    /// Offer one frame to the transport.
    ///
    /// NOT `async`, takes `&self`, and never yields. That is the whole point:
    /// the compiler refuses any future edit that would let a consumer stall the
    /// producer.
    #[must_use]
    pub fn push(&self, frame: Bytes) -> Push {
        let mut inner = self.inner.borrow_mut();
        if inner.closed || inner.queued_bytes + frame.len() > inner.capacity_bytes {
            inner.shed_frames += 1;
            if inner.full_since.is_none() {
                inner.full_since = Some(Instant::now());
            }
            return Push::Shed;
        }
        inner.queued_bytes += frame.len();
        inner.queue.push_back(frame);
        if let Some(waker) = inner.waker.take() {
            waker.wake();
        }
        Push::Accepted
    }

    /// End the response body once the queue drains. The relay's answer to a
    /// connection that has been full longer than `slow_consumer_timeout`.
    pub fn close(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.closed = true;
        if let Some(waker) = inner.waker.take() {
            waker.wake();
        }
    }

    /// How long the buffer has been unable to accept a frame, or `None` if it
    /// has drained since the last refusal.
    #[must_use]
    pub fn full_for(&self) -> Option<Duration> {
        self.inner
            .borrow()
            .full_since
            .map(|since| Instant::now().saturating_duration_since(since))
    }

    #[must_use]
    pub fn shed_frames(&self) -> u64 {
        self.inner.borrow().shed_frames
    }

    /// Frames the TRANSPORT has taken. This is the drain signal: it only moves
    /// when ntex polls the body, which it only does when its write buffer is
    /// below the high-water mark.
    #[must_use]
    pub fn delivered_frames(&self) -> u64 {
        self.inner.borrow().delivered_frames
    }

    #[must_use]
    pub fn delivered_bytes(&self) -> u64 {
        self.inner.borrow().delivered_bytes
    }

    #[must_use]
    pub fn queued_bytes(&self) -> usize {
        self.inner.borrow().queued_bytes
    }
}

impl Stream for ShedBufferBody {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut inner = self.inner.borrow_mut();
        if let Some(frame) = inner.queue.pop_front() {
            inner.queued_bytes -= frame.len();
            inner.delivered_frames += 1;
            inner.delivered_bytes += frame.len() as u64;
            // Draining below capacity clears the full marker, so
            // `slow_consumer_timeout` measures a CONTINUOUSLY full buffer
            // rather than the time since the first ever refusal.
            if inner.queued_bytes < inner.capacity_bytes {
                inner.full_since = None;
            }
            return Poll::Ready(Some(Ok(frame)));
        }
        if inner.closed {
            return Poll::Ready(None);
        }
        inner.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::{FrameDecoder, Push, ShedBuffer, ShedBufferBody, LENGTH_PREFIX_BYTES};
    use futures::Stream;
    use ntex::util::Bytes;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// Poll the body once with a no-op waker. Deliberately not an executor: a
    /// `block_on` would hang on `Pending`, and `Pending` is one of the answers
    /// these tests need to observe.
    fn poll_once(body: &mut ShedBufferBody) -> Poll<Option<Result<Bytes, io::Error>>> {
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        Pin::new(body).poll_next(&mut cx)
    }

    fn frame(n: usize) -> Bytes {
        Bytes::from(vec![b'x'; n])
    }

    #[test]
    fn push_is_synchronous_and_sheds_at_capacity() {
        let (buf, _body) = ShedBuffer::with_capacity(100);
        assert_eq!(buf.push(frame(40)), Push::Accepted);
        assert_eq!(buf.push(frame(40)), Push::Accepted);
        // 80 + 40 > 100. Shed, not blocked, not an error the caller can retry.
        assert_eq!(buf.push(frame(40)), Push::Shed);
        assert_eq!(buf.shed_frames(), 1);
        assert_eq!(buf.queued_bytes(), 80);
        assert!(buf.full_for().is_some());
    }

    #[test]
    fn delivery_clears_the_full_marker() {
        let (buf, mut body) = ShedBuffer::with_capacity(100);
        assert_eq!(buf.push(frame(60)), Push::Accepted);
        assert_eq!(buf.push(frame(60)), Push::Shed);
        assert!(buf.full_for().is_some());

        assert!(matches!(poll_once(&mut body), Poll::Ready(Some(Ok(_)))));
        assert_eq!(buf.delivered_frames(), 1);
        assert_eq!(buf.delivered_bytes(), 60);
        assert!(
            buf.full_for().is_none(),
            "a drain must reset the slow-consumer clock, or the timeout measures \
             time since the first ever refusal"
        );
        assert_eq!(buf.push(frame(60)), Push::Accepted);
    }

    #[test]
    fn an_empty_open_buffer_parks_rather_than_ending_the_body() {
        let (_buf, mut body) = ShedBuffer::with_capacity(100);
        assert!(matches!(poll_once(&mut body), Poll::Pending));
    }

    #[test]
    fn close_ends_the_body_only_after_the_queue_drains() {
        let (buf, mut body) = ShedBuffer::with_capacity(100);
        assert_eq!(buf.push(frame(10)), Push::Accepted);
        buf.close();
        assert!(matches!(poll_once(&mut body), Poll::Ready(Some(Ok(_)))));
        assert!(matches!(poll_once(&mut body), Poll::Ready(None)));
    }

    #[test]
    fn a_closed_buffer_sheds_instead_of_growing() {
        let (buf, _body) = ShedBuffer::with_capacity(100);
        buf.close();
        assert_eq!(buf.push(frame(10)), Push::Shed);
        assert_eq!(buf.queued_bytes(), 0);
    }

    #[test]
    fn frame_decoder_reassembles_across_arbitrary_chunk_boundaries() {
        let wire = [
            super::encode_frame(b"alpha"),
            super::encode_frame(b""),
            super::encode_frame(b"gamma-gamma"),
        ]
        .concat();

        // One byte at a time is the worst case a transport can hand us, and the
        // case a test that assumed chunk == frame would never see.
        let mut dec = FrameDecoder::new();
        let mut out = Vec::new();
        for byte in &wire {
            dec.feed(std::slice::from_ref(byte));
            while let Some(f) = dec.next_frame() {
                out.push(f);
            }
        }
        assert_eq!(
            out,
            vec![b"alpha".to_vec(), Vec::new(), b"gamma-gamma".to_vec()]
        );
        assert_eq!(dec.pending_bytes(), 0);
    }

    #[test]
    fn frame_decoder_holds_a_partial_length_prefix() {
        let mut dec = FrameDecoder::new();
        dec.feed(&[0, 0, 0]);
        assert!(dec.next_frame().is_none());
        assert_eq!(dec.pending_bytes(), 3);
        dec.feed(&[2, b'h', b'i']);
        assert_eq!(dec.next_frame(), Some(b"hi".to_vec()));
        assert_eq!(dec.pending_bytes(), 0);
        assert_eq!(LENGTH_PREFIX_BYTES, 4);
    }
}
