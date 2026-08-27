// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.

//! Utilities for working with the PostgreSQL binary copy format.

use crate::types::{FromSql, IsNull, ToSql, Type, WrongType};
use crate::{CopyInSink, CopyOutStream, Error, slice_iter};
use byteorder::{BigEndian, ByteOrder};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures_util::{Sink, Stream};
use pin_project_lite::pin_project;
use postgres_types::BorrowToSql;
use std::io;
use std::io::Cursor;
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

const MAGIC: &[u8] = b"PGCOPY\n\xff\r\n\0";
const HEADER_LEN: usize = MAGIC.len() + 4 + 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BinaryCopyFinishState {
    Open,
    FrameQueued,
    FrameFlushed,
}

pin_project! {
    /// A type which serializes rows into the PostgreSQL binary copy format.
    ///
    /// The copy *must* be explicitly completed via the `finish` method. If it is not, the copy will be aborted.
    pub struct BinaryCopyInWriter {
        #[pin]
        sink: CopyInSink<Bytes>,
        types: Vec<Type>,
        buf: BytesMut,
        finish_state: BinaryCopyFinishState,
    }
}

impl BinaryCopyInWriter {
    /// Creates a new writer which will write rows of the provided types to the provided sink.
    pub fn new(sink: CopyInSink<Bytes>, types: &[Type]) -> BinaryCopyInWriter {
        let mut buf = BytesMut::new();
        buf.put_slice(MAGIC);
        buf.put_i32(0); // flags
        buf.put_i32(0); // header extension

        BinaryCopyInWriter {
            sink,
            types: types.to_vec(),
            buf,
            finish_state: BinaryCopyFinishState::Open,
        }
    }

    /// Writes a single row.
    ///
    /// # Panics
    ///
    /// Panics if the number of values provided does not match the number expected.
    pub async fn write(self: Pin<&mut Self>, values: &[&(dyn ToSql + Sync)]) -> Result<(), Error> {
        self.write_raw(slice_iter(values)).await
    }

    /// A maximally-flexible version of `write`.
    ///
    /// # Panics
    ///
    /// Panics if the number of values provided does not match the number expected.
    pub async fn write_raw<P, I>(self: Pin<&mut Self>, values: I) -> Result<(), Error>
    where
        P: BorrowToSql,
        I: IntoIterator<Item = P>,
        I::IntoIter: ExactSizeIterator,
    {
        let mut this = self.project();
        ensure_binary_copy_writable(*this.finish_state)?;

        let values = values.into_iter();
        assert!(
            values.len() == this.types.len(),
            "expected {} values but got {}",
            this.types.len(),
            values.len(),
        );

        // A row is ATOMIC in this buffer. `encode_row` writes the tuple header
        // and then hands the same buffer to each value's `to_sql`, so a value
        // that is refused -- on `accepts`, or partway through its own encoding
        // -- leaves the field count, a length placeholder still reading zero,
        // and whatever it managed to append. Nothing downstream can tell that
        // stub from a row: `[count][len=0]` is a legal empty value PostgreSQL
        // INSERTS, and stray value bytes are read as the next tuple's field
        // count. Rewinding to the row boundary is what makes a `write_raw`
        // that returned `Err` mean "this row did not happen", which is the
        // only thing a caller holding a per-row error can act on.
        let checkpoint = this.buf.len();
        if let Err(error) = encode_row(this.buf, this.types.as_slice(), values) {
            this.buf.truncate(checkpoint);
            return Err(error);
        }

        if this.buf.len() > 4096 {
            send_buffered_rows(this.sink.as_mut(), this.buf, checkpoint).await?;
        }

        Ok(())
    }

    /// Completes the copy, returning the number of rows added.
    ///
    /// This method *must* be used to complete the copy process. If it is not, the copy will be aborted.
    pub async fn finish(self: Pin<&mut Self>) -> Result<u64, Error> {
        let mut this = self.project();

        send_binary_copy_end(this.sink.as_mut(), this.buf, this.finish_state).await?;
        this.sink.finish().await
    }
}

fn ensure_binary_copy_writable(state: BinaryCopyFinishState) -> Result<(), Error> {
    if state == BinaryCopyFinishState::Open {
        Ok(())
    } else {
        Err(Error::copy_in_finished())
    }
}

async fn send_binary_copy_end<S>(
    mut sink: Pin<&mut S>,
    buf: &mut BytesMut,
    finish_state: &mut BinaryCopyFinishState,
) -> Result<(), S::Error>
where
    S: Sink<Bytes>,
{
    if *finish_state == BinaryCopyFinishState::Open {
        std::future::poll_fn(|cx| sink.as_mut().poll_ready(cx)).await?;

        // Nothing can cancel between taking the buffered rows and recording
        // that the sink now owns their frame. Record ownership before
        // `start_send` as well: an error there does not return the item.
        buf.put_i16(-1);
        let frame = buf.split().freeze();
        *finish_state = BinaryCopyFinishState::FrameQueued;
        sink.as_mut().start_send(frame)?;
    }

    if *finish_state == BinaryCopyFinishState::FrameQueued {
        std::future::poll_fn(|cx| sink.as_mut().poll_flush(cx)).await?;
        *finish_state = BinaryCopyFinishState::FrameFlushed;
    }

    Ok(())
}

async fn send_buffered_rows<S>(
    mut sink: Pin<&mut S>,
    buf: &mut BytesMut,
    row_start: usize,
) -> Result<(), S::Error>
where
    S: Sink<Bytes>,
{
    // Keep rows whose calls already returned `Ok` in `buf` until the sink can
    // accept their frame. The row which crossed the threshold stays local to
    // this future, so cancellation while readiness is pending rolls back only
    // that unfinished call.
    let row = buf.split_off(row_start);
    std::future::poll_fn(|cx| sink.as_mut().poll_ready(cx)).await?;

    // No cancellation point separates removing the completed prefix from
    // transferring the combined frame into the sink. Once `start_send`
    // succeeds, the sink owns the bytes while `poll_flush` is pending.
    buf.unsplit(row);
    sink.as_mut().start_send(buf.split().freeze())?;
    std::future::poll_fn(|cx| sink.as_mut().poll_flush(cx)).await
}

/// Append one tuple -- field count, then a four-byte length and its payload per
/// value -- to `buf`.
///
/// Separated from `write_raw` so every early return is a single `?` inside one
/// function whose failures the caller undoes wholesale. Inline, each `?` was a
/// separate exit leaving a different amount of the row behind.
fn encode_row<P, I>(buf: &mut BytesMut, types: &[Type], values: I) -> Result<(), Error>
where
    P: BorrowToSql,
    I: Iterator<Item = P>,
{
    // REFUSED, not cast. The column types come from the CALLER
    // (`BinaryCopyInWriter::new` takes them as a slice), so this length is
    // caller-controlled, and `as i16` wraps SILENTLY - a cast does not panic on
    // overflow the way arithmetic does, so 32768 columns became -32768 on the
    // wire in every build. A negative field count desynchronises the COPY
    // stream, which costs the connection rather than just the row.
    //
    // `tuple_field_count` already refuses the same shape on the READ side,
    // widening to `i32` so a hostile `i16::MAX` cannot overflow. This is that
    // rule applied to what we write.
    let field_count = i16::try_from(types.len()).map_err(|_| {
        Error::encode(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "binary COPY supports at most {} columns, but {} were given",
                i16::MAX,
                types.len()
            ),
        ))
    })?;
    buf.put_i16(field_count);

    for (i, (value, type_)) in values.zip(types).enumerate() {
        let idx = buf.len();
        buf.put_i32(0);
        // Shares the bind path's domain fallback: `BinaryCopyInWriter` takes its
        // column types from the CALLER, who naturally obtains them from the
        // catalog or a prepared statement, both of which yield the DOMAIN.
        let len = match crate::query::encode_parameter(value.borrow_to_sql(), type_, buf)
            .map_err(|e| Error::to_sql(e, i))?
        {
            IsNull::Yes => -1,
            IsNull::No => i32::try_from(buf.len() - idx - 4)
                .map_err(|e| Error::encode(io::Error::new(io::ErrorKind::InvalidInput, e)))?,
        };
        BigEndian::write_i32(&mut buf[idx..], len);
    }

    Ok(())
}

struct Header {
    has_oids: bool,
}

pin_project! {
    /// A stream of rows deserialized from the PostgreSQL binary copy format.
    pub struct BinaryCopyOutStream {
        #[pin]
        stream: CopyOutStream,
        types: Arc<Vec<Type>>,
        header: Option<Header>,
        // Binary EOF is not clean stream EOF until CopyOut sees CopyDone.
        trailer_seen: bool,
        // The underlying COPY protocol ended with EOF or ErrorResponse.
        terminal: bool,
    }
}

impl BinaryCopyOutStream {
    /// Creates a stream from a raw copy out stream and the types of the columns being returned.
    pub fn new(stream: CopyOutStream, types: &[Type]) -> BinaryCopyOutStream {
        BinaryCopyOutStream {
            stream,
            types: Arc::new(types.to_vec()),
            header: None,
            trailer_seen: false,
            terminal: false,
        }
    }
}

impl Stream for BinaryCopyOutStream {
    type Item = Result<BinaryCopyOutRow, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        if *this.terminal {
            return Poll::Ready(None);
        }

        loop {
            if *this.trailer_seen {
                match ready!(this.stream.as_mut().poll_next(cx)) {
                    Some(Ok(chunk)) => {
                        return Poll::Ready(Some(Err(Error::parse(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "{} bytes of CopyData after the binary COPY trailer",
                                chunk.len()
                            ),
                        )))));
                    }
                    Some(Err(error)) => {
                        *this.terminal = true;
                        return Poll::Ready(Some(Err(error)));
                    }
                    None => {
                        *this.terminal = true;
                        return Poll::Ready(None);
                    }
                }
            }

            let chunk = match ready!(this.stream.as_mut().poll_next(cx)) {
                Some(Ok(chunk)) => chunk,
                Some(Err(e)) => {
                    *this.terminal = true;
                    return Poll::Ready(Some(Err(e)));
                }
                // The protocol exchange ended cleanly - CopyDone reached
                // CommandComplete - but the binary stream never produced its
                // -1 trailer. That is malformed DATA on a healthy connection,
                // so it must not be reported as `Error::closed()`: a pool
                // reads `is_closed()` to decide whether to discard a session,
                // and would throw away one that is still perfectly usable.
                None => {
                    *this.terminal = true;
                    return Poll::Ready(Some(Err(Error::parse(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "binary COPY stream ended without its trailer",
                    )))));
                }
            };
            let mut chunk = Cursor::new(chunk);

            let has_oids = match &this.header {
                Some(header) => header.has_oids,
                None => {
                    let header = match parse_binary_copy_header(&mut chunk) {
                        Ok(header) => header,
                        Err(e) => return Poll::Ready(Some(Err(e))),
                    };
                    let has_oids = header.has_oids;
                    *this.header = Some(header);
                    has_oids
                }
            };

            check_remaining(&chunk, 2)?;
            let raw = chunk.get_i16();
            if raw == -1 {
                if chunk.has_remaining() {
                    return Poll::Ready(Some(Err(Error::parse(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "{} trailing bytes after the binary COPY trailer",
                            chunk.remaining()
                        ),
                    )))));
                }
                *this.trailer_seen = true;
                continue;
            }

            let len = match tuple_field_count(raw, has_oids, this.types.len()) {
                Ok(len) => len,
                Err(e) => return Poll::Ready(Some(Err(e))),
            };

            let mut ranges = vec![];
            for _ in 0..len {
                check_remaining(&chunk, 4)?;
                let len = chunk.get_i32();
                if len == -1 {
                    ranges.push(None);
                } else {
                    let len = len as usize;
                    check_remaining(&chunk, len)?;
                    let start = chunk.position() as usize;
                    ranges.push(Some(start..start + len));
                    chunk.advance(len);
                }
            }

            // Every byte of a binary tuple is accounted for above, so a conforming
            // peer leaves NOTHING here: the PostgreSQL protocol's COPY Operations
            // section binds the backend to "zero or more CopyData messages (always
            // one per row)" in copy-out mode -- the frontend direction is
            // explicitly free to frame arbitrarily, this one is not. Whatever is
            // still in the chunk therefore belongs to tuples that will never be
            // returned, because the next poll reads the NEXT message. Dropping
            // them silently hands the caller a SHORT result with no error, which
            // is the one failure a caller cannot detect. Parsing on instead would
            // not close it: a peer free to pack two tuples into a message is
            // equally free to split one across two.
            if chunk.has_remaining() {
                return Poll::Ready(Some(Err(Error::parse(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} trailing bytes after a binary COPY tuple",
                        chunk.remaining()
                    ),
                )))));
            }

            return Poll::Ready(Some(Ok(BinaryCopyOutRow {
                buf: chunk.into_inner(),
                ranges,
                types: this.types.clone(),
            })));
        }
    }
}

/// Parse the 19-byte binary-COPY file header from the front of `chunk`,
/// advancing the cursor past it (magic + flags + header-extension area).
///
/// Validates the magic value, rejects unknown critical flag bits, and reports
/// the recognized has-OIDs flag (bit 16).
fn parse_binary_copy_header(chunk: &mut Cursor<Bytes>) -> Result<Header, Error> {
    check_remaining(chunk, HEADER_LEN)?;
    if !chunk.chunk().starts_with(MAGIC) {
        return Err(Error::parse(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid magic value",
        )));
    }
    chunk.advance(MAGIC.len());

    let flags = chunk.get_i32();
    // Per PG COPY binary format spec: bits 16-31 are critical (abort on an
    // UNRECOGNIZED bit) and bits 0-15 are backward-compatible (ignore). Bit 16
    // is the recognized has-OIDs flag, so the unknown-critical range is bits
    // 17-31 (mask 0xFFFE_0000) - bit 16 must NOT be swallowed here, else the
    // has_oids report below becomes unreachable.
    if (flags as u32) & 0xFFFE_0000 != 0 {
        return Err(Error::parse(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported COPY header: critical flags set 0x{:08X}",
                flags as u32
            ),
        )));
    }
    let has_oids = (flags & (1 << 16)) != 0;

    let header_extension = chunk.get_u32() as usize;
    check_remaining(chunk, header_extension)?;
    chunk.advance(header_extension);

    Ok(Header { has_oids })
}

/// How many values a tuple header declares, checked against the column count
/// the caller asked for.
///
/// `raw` is the tuple's field count straight off the wire and `has_oids` comes
/// from a flag bit in the file header, so BOTH are chosen by the peer. The
/// `-1` end-of-data sentinel is handled by the caller; anything else reaching
/// here is a count.
fn tuple_field_count(raw: i16, has_oids: bool, expected: usize) -> Result<usize, Error> {
    // Widened before the adjustment: `raw` reaches `i16::MAX` and the OID adds
    // one, which overflows an i16 and aborts wherever overflow checks are on.
    // A negative count still lands far from any real column count once cast,
    // so it is refused below rather than needing its own arm.
    let len = i32::from(raw) + i32::from(has_oids);
    if len as usize != expected {
        return Err(Error::parse(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("expected {} values but got {}", expected, len),
        )));
    }
    Ok(len as usize)
}

fn check_remaining(buf: &Cursor<Bytes>, len: usize) -> Result<(), Error> {
    if buf.remaining() < len {
        Err(Error::parse(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "unexpected EOF",
        )))
    } else {
        Ok(())
    }
}

/// A row of data parsed from a binary copy out stream.
pub struct BinaryCopyOutRow {
    buf: Bytes,
    ranges: Vec<Option<Range<usize>>>,
    types: Arc<Vec<Type>>,
}

impl BinaryCopyOutRow {
    /// Like `get`, but returns a `Result` rather than panicking.
    pub fn try_get<'a, T>(&'a self, idx: usize) -> Result<T, Error>
    where
        T: FromSql<'a>,
    {
        let type_ = match self.types.get(idx) {
            Some(type_) => type_,
            None => return Err(Error::column(idx.to_string())),
        };

        if !T::accepts(type_) {
            return Err(Error::from_sql(
                Box::new(WrongType::new::<T>(type_.clone())),
                idx,
            ));
        }

        let r = match &self.ranges[idx] {
            Some(range) => T::from_sql(type_, &self.buf[range.clone()]),
            None => T::from_sql_null(type_),
        };

        r.map_err(|e| Error::from_sql(e, idx))
    }

    /// Deserializes a value from the row.
    ///
    /// # Panics
    ///
    /// Panics if the index is out of bounds or if the value cannot be converted to the specified type.
    pub fn get<'a, T>(&'a self, idx: usize) -> T
    where
        T: FromSql<'a>,
    {
        match self.try_get(idx) {
            Ok(value) => value,
            Err(e) => panic!("error retrieving column {}: {}", idx, e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::future::Future;
    use std::task::Waker;

    struct BackpressuredSink {
        ready: bool,
        flush_ready: bool,
        sent: Vec<Bytes>,
    }

    impl Sink<Bytes> for BackpressuredSink {
        type Error = Infallible;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            if self.ready {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }

        fn start_send(mut self: Pin<&mut Self>, item: Bytes) -> Result<(), Self::Error> {
            self.sent.push(item);
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            if self.flush_ready {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Rows from completed `write_raw` calls stay buffered if the call which
    /// crosses the flush threshold is cancelled while the COPY sink applies
    /// backpressure. The in-progress row has no completed result and is rolled
    /// back, so retrying it cannot duplicate it later.
    #[test]
    fn cancelling_a_backpressured_flush_keeps_completed_rows() {
        const COMPLETED: &[u8] = b"rows whose writes returned Ok";
        const IN_PROGRESS: &[u8] = b"row whose write is pending";

        let mut buf = BytesMut::from(COMPLETED);
        let row_start = buf.len();
        buf.extend_from_slice(IN_PROGRESS);
        let mut sink = Box::pin(BackpressuredSink {
            ready: false,
            flush_ready: true,
            sent: Vec::new(),
        });

        {
            let mut sending = Box::pin(send_buffered_rows(sink.as_mut(), &mut buf, row_start));
            let mut context = Context::from_waker(Waker::noop());
            assert!(
                sending.as_mut().poll(&mut context).is_pending(),
                "the control sink did not apply backpressure"
            );
        }

        assert_eq!(
            &buf[..],
            COMPLETED,
            "cancelling one write discarded rows whose write_raw calls returned Ok"
        );
        assert!(
            sink.sent.is_empty(),
            "a sink which never became ready accepted bytes"
        );

        let row_start = buf.len();
        buf.extend_from_slice(IN_PROGRESS);
        sink.as_mut().get_mut().ready = true;
        {
            let mut sending = Box::pin(send_buffered_rows(sink.as_mut(), &mut buf, row_start));
            let mut context = Context::from_waker(Waker::noop());
            assert!(
                matches!(sending.as_mut().poll(&mut context), Poll::Ready(Ok(()))),
                "the control sink did not accept bytes after becoming ready"
            );
        }

        let mut expected = COMPLETED.to_vec();
        expected.extend_from_slice(IN_PROGRESS);
        assert_eq!(sink.sent, [Bytes::from(expected)]);
        assert!(buf.is_empty(), "a transferred frame remained buffered");
    }

    #[test]
    fn a_queued_binary_copy_trailer_refuses_more_rows() {
        assert!(
            ensure_binary_copy_writable(BinaryCopyFinishState::FrameQueued).is_err(),
            "binary COPY accepted a row after its trailer was queued"
        );
    }

    #[test]
    fn cancelling_binary_copy_finish_before_readiness_keeps_buffered_rows() {
        const COMPLETED: &[u8] = b"rows whose writes returned Ok";

        let mut buf = BytesMut::from(COMPLETED);
        let mut finish_state = BinaryCopyFinishState::Open;
        let mut sink = Box::pin(BackpressuredSink {
            ready: false,
            flush_ready: true,
            sent: Vec::new(),
        });

        {
            let mut sending = Box::pin(send_binary_copy_end(
                sink.as_mut(),
                &mut buf,
                &mut finish_state,
            ));
            let mut context = Context::from_waker(Waker::noop());
            assert!(
                sending.as_mut().poll(&mut context).is_pending(),
                "the binary COPY finish fixture did not apply readiness backpressure"
            );
        }

        assert_eq!(
            &buf[..],
            COMPLETED,
            "cancelling binary COPY finish discarded completed rows"
        );
        assert_eq!(finish_state, BinaryCopyFinishState::Open);
        assert!(sink.sent.is_empty());
    }

    #[test]
    fn retrying_binary_copy_finish_after_enqueue_does_not_duplicate_the_trailer() {
        let mut buf = BytesMut::from(&b"completed rows"[..]);
        let mut finish_state = BinaryCopyFinishState::Open;
        let mut sink = Box::pin(BackpressuredSink {
            ready: true,
            flush_ready: false,
            sent: Vec::new(),
        });

        {
            let mut sending = Box::pin(send_binary_copy_end(
                sink.as_mut(),
                &mut buf,
                &mut finish_state,
            ));
            let mut context = Context::from_waker(Waker::noop());
            assert!(
                sending.as_mut().poll(&mut context).is_pending(),
                "the binary COPY finish fixture did not park while flushing"
            );
        }
        assert_eq!(sink.sent.len(), 1, "the first trailer was not queued");

        sink.as_mut().get_mut().flush_ready = true;
        {
            let mut sending = Box::pin(send_binary_copy_end(
                sink.as_mut(),
                &mut buf,
                &mut finish_state,
            ));
            let mut context = Context::from_waker(Waker::noop());
            assert!(matches!(
                sending.as_mut().poll(&mut context),
                Poll::Ready(Ok(()))
            ));
        }

        assert_eq!(
            sink.sent.len(),
            1,
            "retrying binary COPY finish duplicated its trailer frame"
        );
        assert_eq!(finish_state, BinaryCopyFinishState::FrameFlushed);
    }

    /// Build a 19-byte binary-COPY file header: MAGIC + flags (BE i32) +
    /// header-extension-length 0 (BE u32), wrapped in a `Cursor<Bytes>`.
    fn header_buf(flags: i32) -> Cursor<Bytes> {
        let mut buf = BytesMut::new();
        buf.put_slice(MAGIC);
        buf.put_i32(flags);
        buf.put_u32(0); // header extension length
        Cursor::new(buf.freeze())
    }

    #[test]
    fn header_accepts_oid_flag() {
        // Bit 16 is the recognized has-OIDs flag - it must NOT be rejected as
        // an unknown critical bit, and must surface as `has_oids == true`.
        let mut chunk = header_buf(1 << 16);
        let header = parse_binary_copy_header(&mut chunk).expect("OID-flag header must parse");
        assert!(header.has_oids, "bit 16 must be reported as has_oids");
    }

    #[test]
    fn header_rejects_unknown_critical_flag() {
        // Bit 17 is in the unknown-critical range (17-31): must abort.
        let mut chunk = header_buf(1 << 17);
        assert!(
            parse_binary_copy_header(&mut chunk).is_err(),
            "an unknown critical flag (bit 17) must be rejected"
        );
    }

    #[test]
    fn header_ignores_noncritical_low_bits() {
        // Bits 0-15 are backward-compatible (non-critical): accept, no OIDs.
        let mut chunk = header_buf(1 << 0);
        let header = parse_binary_copy_header(&mut chunk).expect("low-bit header must parse");
        assert!(!header.has_oids, "low bits must not set has_oids");
    }

    #[test]
    fn header_plain_no_oids() {
        let mut chunk = header_buf(0);
        let header = parse_binary_copy_header(&mut chunk).expect("plain header must parse");
        assert!(!header.has_oids, "no flags means no OIDs");
    }

    #[test]
    fn header_rejects_bad_magic() {
        let mut buf = BytesMut::new();
        buf.put_slice(b"NOTPGCOPY\0\0"); // 11 bytes, wrong magic
        buf.put_i32(0);
        buf.put_u32(0);
        let mut chunk = Cursor::new(buf.freeze());
        assert!(
            parse_binary_copy_header(&mut chunk).is_err(),
            "a bad magic value must be rejected"
        );
    }

    /// A peer that sets the has-OIDs flag and declares `i16::MAX` fields must
    /// get an error, not a panic.
    ///
    /// Both inputs are chosen by the peer: the flag is bit 16 of the file
    /// header and the count is the tuple's own `i16`. Adding one to
    /// `i16::MAX` overflows, and `[profile.dev]` in the workspace root sets no
    /// `overflow-checks`, so it inherits the default ON and the add ABORTS the
    /// process. Release wraps instead, and the wrapped value then fails the
    /// comparison, so this is a debug-profile panic rather than a wrong
    /// answer -- but a driver should not abort on bytes a peer chose.
    ///
    /// No conforming PostgreSQL sends this: has-OIDs went away in PG12 and the
    /// server's column ceiling is 1664. It takes a hostile or broken peer,
    /// which is the threat model `tests/hostile_peer.rs` already works in.
    /// WHAT THIS DOES NOT CATCH. Its discriminating power is DEBUG-ONLY. The
    /// regression it guards is reverting to `raw + has_oids as i16`, and that
    /// only aborts where overflow checks are on. In a release build the same
    /// arithmetic wraps to -32768, which still fails the `!= expected`
    /// comparison, so the error below still arrives and this test still passes.
    ///
    /// That is acceptable rather than fixed because `cargo test` builds in
    /// debug, so the ordinary path does discriminate - but a release-profile
    /// run of this suite would not, and nobody should read a green release run
    /// as evidence about the overflow.
    #[test]
    fn a_tuple_field_count_at_i16_max_with_oids_errors_rather_than_overflowing() {
        let error = tuple_field_count(i16::MAX, true, 1)
            .expect_err("a field count that cannot match must be refused");
        assert!(
            error
                .to_string()
                .contains("error parsing response from server"),
            "unexpected error: {error}"
        );
    }

    /// One variable away from the case above: same count, no OID flag, so no
    /// addition happens. It must still be REFUSED (32767 != 1). This is what
    /// keeps the test above honest -- were this one to panic as well, the
    /// overflow would not be the thing the other test caught.
    #[test]
    fn the_same_count_without_the_oid_flag_is_refused_without_arithmetic() {
        let error =
            tuple_field_count(i16::MAX, false, 1).expect_err("32767 fields cannot match 1 column");
        assert!(
            error
                .to_string()
                .contains("error parsing response from server"),
            "unexpected error: {error}"
        );
    }

    /// The counting still has to WORK, or "never panics" could be satisfied by
    /// refusing everything.
    #[test]
    fn a_matching_tuple_field_count_is_accepted_with_and_without_oids() {
        assert_eq!(
            tuple_field_count(2, false, 2).expect("2 fields against 2 columns"),
            2
        );
        // With the OID flag the wire count is one SHORT of the column count,
        // because the OID is the extra value.
        assert_eq!(
            tuple_field_count(1, true, 2).expect("1 field plus an OID against 2 columns"),
            2
        );
    }

    /// A column count that does not fit the wire's `int16` is refused, not
    /// wrapped.
    ///
    /// `BinaryCopyInWriter::new` takes the column types from the CALLER, so the
    /// count is caller-controlled. `types.len() as i16` wraps SILENTLY - a cast
    /// does not panic on overflow the way arithmetic does, so this was quiet in
    /// every build - and 32768 columns became -32768 on the wire. A negative
    /// field count desynchronises the COPY stream, which costs the connection
    /// rather than just the row.
    ///
    /// The read side already refuses its twin (`tuple_field_count` widens to
    /// `i32` before comparing, precisely so a hostile `i16::MAX` cannot
    /// overflow). This is the same rule applied to what we WRITE.
    #[test]
    fn a_column_count_that_cannot_fit_the_wire_is_refused() {
        let types = vec![Type::INT4; usize::from(u16::MAX) + 2];
        let mut buf = BytesMut::new();
        let values: Vec<&(dyn ToSql + Sync)> = Vec::new();
        let error = encode_row(&mut buf, &types, values.into_iter())
            .expect_err("a column count past i16::MAX must be refused, not wrapped");
        let chain = std::iter::successors(std::error::Error::source(&error), |e| {
            std::error::Error::source(*e)
        })
        .fold(format!("{error}"), |acc, e| format!("{acc}: {e}"));
        assert!(
            chain.contains("at most") && chain.contains("columns"),
            "the refusal should name the column limit and the count given: {chain}"
        );
    }

    /// THE CONTROL, one variable: a count that DOES fit is still written, and
    /// written as itself. Refusing everything would satisfy the test above.
    #[test]
    fn a_column_count_that_fits_is_written_unchanged() {
        let types = vec![Type::INT4, Type::INT4, Type::INT4];
        let mut buf = BytesMut::new();
        let values: Vec<&(dyn ToSql + Sync)> = vec![&1i32, &2i32, &3i32];
        encode_row(&mut buf, &types, values.into_iter()).expect("a normal row encodes");
        assert_eq!(
            i16::from_be_bytes([buf[0], buf[1]]),
            3,
            "the field count on the wire must be the real column count"
        );
    }
}
