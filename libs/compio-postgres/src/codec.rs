// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Source uses `tokio_util::codec::{Encoder, Decoder}` over a `Framed`
// wrapper. We can't depend on tokio_util, so this module replaces that
// with free functions that work against our in-house `BufStream`.
//
// The decoder semantics are preserved exactly:
//   - A single call to `read_backend` returns either one *async* message
//     (NoticeResponse / NotificationResponse / ParameterStatus) or a
//     batch of *normal* messages. The batch ends at ReadyForQuery, at an
//     async message, or at the last COMPLETE message in the buffer -
//     whichever comes first. Only ReadyForQuery sets `request_complete`,
//     so a batch is NOT a whole response and callers must not assume it is.
//   - Inside a `Normal` batch, callers iterate via `BackendMessages::next`.
//
// The length-cap + peek helpers from the earlier BufStream hardening pass
// still guard against an attacker-controlled length field.

use crate::Error;
use crate::buf_stream::{ReadFramer, WriteFramer};
use bytes::{Buf, Bytes, BytesMut};
use fallible_iterator::FallibleIterator;
use postgres_protocol::message::backend;
use postgres_protocol::message::frontend::CopyData;
use std::io;

/// A frontend (client -> server) message ready to be serialized onto the wire.
pub enum FrontendMessage {
    /// Pre-encoded bytes (the common path: `postgres_protocol::message::frontend::*`
    /// writes directly into a `BytesMut`, which we convert to `Bytes` for cheap
    /// cloning). Used for Parse, Bind, Describe, Execute, Sync, Query, etc.
    Raw(Bytes),
    /// Out-of-band COPY payload frame. Separated from `Raw` because the
    /// encoder owns the buffer and writes the framing header in one pass.
    CopyData(CopyData<Box<dyn Buf + Send>>),
}

/// A backend (server -> client) message or batch thereof.
///
/// Matches tokio-postgres's `BackendMessage` shape so the demux logic in
/// `connection.rs` can remain a close translation of the upstream source.
pub enum BackendMessage {
    /// A run of synchronous messages, optionally terminated by
    /// `ReadyForQuery`.
    Normal {
        messages: BackendMessages,
        request_complete: bool,
    },
    /// An out-of-band async notification / notice / parameter status
    /// update - must be routed to the dedicated async channel, not the
    /// in-flight request.
    ///
    /// `frame_len` is the whole frame the message OWNS: `Message::parse`
    /// splits `tag + length + body` off the read buffer and the resulting
    /// body keeps all of it. Nothing derived from the parsed view can stand
    /// in for it - a `NotificationResponse` whose two strings are followed by
    /// megabytes of padding parses fine and its fields sum to a handful of
    /// bytes. Anything that retains one of these must charge this number.
    Async {
        message: backend::Message,
        frame_len: usize,
    },
}

/// A lazily-parsed iterator of backend messages sharing a single
/// `BytesMut` buffer. Consumers call `next()` until `None` to stream
/// through a request's responses without copying.
pub struct BackendMessages(BytesMut);

impl BackendMessages {
    /// Construct an empty BackendMessages batch. Used by placeholder
    /// initialisations in `connection.rs`.
    #[allow(dead_code)]
    pub fn empty() -> BackendMessages {
        BackendMessages(BytesMut::new())
    }

    /// Consume one complete raw frame with `tag` from the head of this batch.
    ///
    /// Startup uses this only for protocol messages postgres-protocol 0.6.12
    /// cannot represent: `NegotiateProtocolVersion` and protocol 3.2's
    /// variable-length `BackendKeyData`. Every ordinary message continues
    /// through its parser below.
    pub(crate) fn take_raw_frame(&mut self, tag: u8) -> io::Result<Option<Bytes>> {
        let Some(header) = backend::Header::parse(&self.0)? else {
            return Ok(None);
        };
        if header.tag() != tag {
            return Ok(None);
        }

        let total_len = header.len() as usize + 1;
        if self.0.len() < total_len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete PostgreSQL startup frame",
            ));
        }

        let frame = self.0.split_to(total_len).freeze();
        // `slice(5..)` is in range only because `Header::parse` REFUSES a
        // declared length below 4 (`invalid message length: header length < 4`,
        // checked in postgres-protocol 0.6.12, the resolved version). Without
        // that upstream guard a hostile peer could declare 3, making
        // `total_len` 4 and this slice panic. If the header parse is ever
        // replaced with one of ours, the check has to come with it.
        Ok(Some(frame.slice(5..)))
    }

    /// Return the status byte from a trailing `ReadyForQuery` frame without
    /// consuming the messages that still belong to the response stream.
    pub(crate) fn ready_for_query_status(&self) -> Option<u8> {
        const FRAME_LEN: usize = 6;
        let frame = self.0.get(self.0.len().checked_sub(FRAME_LEN)?..)?;
        if frame[0] == backend::READY_FOR_QUERY_TAG && frame[1..5] == [0, 0, 0, 5] {
            Some(frame[5])
        } else {
            None
        }
    }

    /// Return the tag of the first frame without consuming it.
    ///
    /// The prepare path uses this at the connection boundary to decide
    /// whether a cancelled `Parse` created its named statement. The response
    /// still has to be delivered intact when its caller is alive.
    pub(crate) fn first_tag(&self) -> Option<u8> {
        self.0.first().copied()
    }

    /// Whether this already-validated batch contains a frame with `tag`.
    ///
    /// The connection deadline uses this before handing a COPY response to a
    /// possibly backpressured consumer: PostgreSQL owes no further bytes after
    /// `CopyInResponse` until the caller supplies input.
    pub(crate) fn contains_tag(&self, tag: u8) -> bool {
        let mut offset = 0usize;
        while let Some(header_end) = offset.checked_add(5) {
            let Some(header) = self.0.get(offset..header_end) else {
                return false;
            };
            let length = u32::from_be_bytes(header[1..5].try_into().unwrap()) as usize;
            let Some(next) = offset
                .checked_add(1)
                .and_then(|value| value.checked_add(length))
            else {
                return false;
            };
            if length < 4 || next > self.0.len() {
                return false;
            }
            if header[0] == tag {
                return true;
            }
            offset = next;
        }
        false
    }

    /// Clone and decode an error only when this batch actually contains one.
    /// Successful row batches can be large, so scanning their frame headers
    /// avoids copying them merely to support a rare cache-recovery path.
    pub(crate) fn error_response(&self) -> io::Result<Option<backend::ErrorResponseBody>> {
        let mut offset = 0usize;
        let mut has_error = false;
        while let Some(header_end) = offset.checked_add(5) {
            let Some(header) = self.0.get(offset..header_end) else {
                break;
            };
            let length = u32::from_be_bytes(header[1..5].try_into().unwrap()) as usize;
            if length < 4 {
                break;
            }
            let Some(next) = offset
                .checked_add(1)
                .and_then(|value| value.checked_add(length))
            else {
                break;
            };
            if next > self.0.len() {
                break;
            }
            if header[0] == backend::ERROR_RESPONSE_TAG {
                has_error = true;
                break;
            }
            offset = next;
        }
        if !has_error {
            return Ok(None);
        }

        let mut messages = BackendMessages(self.0.clone());
        while let Some(message) = messages.next()? {
            if let backend::Message::ErrorResponse(body) = message {
                return Ok(Some(body));
            }
        }
        Ok(None)
    }
}

impl FallibleIterator for BackendMessages {
    type Item = backend::Message;
    type Error = io::Error;

    fn next(&mut self) -> io::Result<Option<backend::Message>> {
        backend::Message::parse(&mut self.0)
    }
}

/// Encode a frontend message into the stream's write buffer.
///
/// This does not flush - callers batch multiple frontend messages
/// (Parse + Bind + Describe + Execute + Sync) into one flush, matching
/// tokio-postgres's `Framed::send` + `Sink::poll_flush` split.
#[allow(dead_code)]
pub fn write_frontend<S>(stream: &mut S, msg: FrontendMessage) -> Result<(), Error>
where
    S: WriteFramer + ?Sized,
{
    let dst = stream.write_buf_mut();
    match msg {
        FrontendMessage::Raw(buf) => dst.extend_from_slice(&buf),
        FrontendMessage::CopyData(data) => data.write(dst),
    }
    Ok(())
}

/// Read the next backend message batch (or a single async message) from
/// the stream.
///
/// Mirrors `PostgresCodec::decode` from tokio-postgres: the loop walks
/// the buffer one header at a time, peeling off an Async message when
/// it's first and returning a Normal batch otherwise.
///
/// A Normal batch ends at whichever comes first: `ReadyForQuery` (which
/// also sets `request_complete`), an async message at a non-zero offset, or
/// the last COMPLETE message in the buffer. That third terminator is what
/// keeps the read buffer bounded - the decoder returns what it has rather
/// than reading until the run happens to end on a message boundary.
///
/// On a Normal batch, the returned `BackendMessages` owns the underlying
/// `BytesMut` slice - the stream's read buffer is drained exactly that
/// many bytes via `split_to`, so subsequent reads start fresh.
#[allow(dead_code)]
pub async fn read_backend<S>(stream: &mut S) -> Result<BackendMessage, Error>
where
    S: ReadFramer + ?Sized,
{
    loop {
        // Ensure we have at least one full message header (1-byte tag + 4-byte length).
        stream.fill(5).await?;

        // Reject oversize frames up-front so a malicious server can't
        // coerce us into buffering gigabytes per connection.
        let length = stream
            .peek_u32_be(1)
            .expect("fill(5) guarantees 5 bytes are buffered");
        let tag = stream.buf()[0];
        validate_startup_message_length(tag, length)?;
        stream.validate_length(length)?;

        // Walk the buffered bytes looking for a complete batch. Mirrors
        // tokio-postgres's codec.rs decode().
        let mut idx = 0usize;
        let mut request_complete = false;

        while let Some(header) = backend::Header::parse(&stream.buf()[idx..]).map_err(Error::io)? {
            // EVERY frame in the batch, not just the head one validated above.
            // A single read can append a whole `READ_CHUNK`, so a peer that
            // puts a small frame in front of an oversized one had the second
            // delivered: the walk only measured the first.
            //
            // This is a contract violation rather than an allocation vector -
            // `Header::parse` takes `&[u8]` and cannot reserve, the walk breaks
            // below unless the frame is already buffered, and a header
            // declaring a huge length breaks the walk and is refused as the
            // head frame next time round. What escaped was bounded by what the
            // read actually delivered. But a caller who set a ceiling still
            // received a message above it, which is the whole of what the
            // setting promises.
            let msg_len = header.len() as usize + 1;
            if stream.buf()[idx..].len() < msg_len {
                // Partial message at the tail. Everything before it is
                // complete, so stop walking and hand that prefix back.
                break;
            }
            // Measured AFTER the completeness check, so only a frame this walk
            // is about to DELIVER is judged. Checking before it would also
            // judge a header the walk is going to abandon, and after a
            // desynchronising peer - one whose declared length is shorter than
            // its payload - that header is misaligned payload bytes rather
            // than a claim the peer made. Reporting "message too large" for
            // those replaces the accurate "unexpected EOF" with a diagnosis
            // the peer never earned; `a_length_shorter_than_its_payload_is
            // _refused` pins that.
            //
            // `Header::parse` reads the length as `i32` and refuses anything
            // below 4, so the value is in `4..=i32::MAX` and this cast is
            // exact.
            stream.validate_length(header.len() as u32)?;
            // The per-tag startup limits belong here too. They were head-only,
            // so a `BackendKeyData` claiming more than its own 264 slipped
            // through whenever it arrived behind another frame - and
            // `AuthenticationOk` in front of it is what a real startup sequence
            // looks like. Repeating them is a no-op on a data-phase stream,
            // because both constrained tags are startup-only.
            validate_startup_message_length(header.tag(), header.len() as u32)?;

            match header.tag() {
                backend::NOTICE_RESPONSE_TAG
                | backend::NOTIFICATION_RESPONSE_TAG
                | backend::PARAMETER_STATUS_TAG => {
                    if idx == 0 {
                        // Async message sits at the head - return it alone.
                        // Measured BEFORE the parse consumes it: `header.len()` counts
                        // itself but not the tag, so the frame is one more.
                        let frame_len = header.len() as usize + 1;
                        let message = backend::Message::parse(stream.buf())
                            .map_err(Error::io)?
                            .expect("async header implies full message is buffered");
                        return Ok(BackendMessage::Async { message, frame_len });
                    } else {
                        // Normal batch terminates at this async boundary;
                        // caller will see the async message on the next call.
                        break;
                    }
                }
                _ => {}
            }

            idx += msg_len;

            if header.tag() == backend::READY_FOR_QUERY_TAG {
                request_complete = true;
                break;
            }
        }

        if idx == 0 {
            // The message at the head is still partial - the only way `idx`
            // stays 0, since the `fill(5)` above already guaranteed a parseable
            // header. Reading is the only way forward. Asking for one more byte
            // than we hold is what makes this terminate: `fill` must either read
            // at least one byte, fail, or reject the size, so it cannot return
            // successfully without progress.
            //
            // Refilling ONLY here is what bounds the read buffer. Refilling
            // whenever any message at the tail was partial - which is what
            // this did until it was measured - lets a dense run of small
            // messages accumulate untouched until `fill` refuses the request
            // above `DEFAULT_MAX_MESSAGE_SIZE`, failing a query whose largest single
            // message was kilobytes. It also re-walked every buffered header
            // on each chunk, which is quadratic in the size of the run.
            let have = stream.buf().len();
            stream.fill(have + 1).await?;
            continue;
        }

        let messages = BackendMessages(stream.buf().split_to(idx));
        return Ok(BackendMessage::Normal {
            messages,
            request_complete,
        });
    }
}

/// Apply the small, protocol-defined limits for startup-only messages before
/// the decoder reads their bodies. The generic connection limit is 64 MiB by
/// default, which is appropriate for rows but far too large for either of
/// these unauthenticated messages.
fn validate_startup_message_length(tag: u8, length: u32) -> Result<(), Error> {
    match tag {
        b'v' if length != 12 => Err(Error::io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "invalid NegotiateProtocolVersion length {length}; expected 12 when no protocol options were sent"
            ),
        ))),
        b'K' if !(12..=264).contains(&length) => Err(Error::io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid BackendKeyData length {length}; expected 12 to 264"),
        ))),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buf_stream::ReadFramer;
    use std::collections::VecDeque;

    /// A [`ReadFramer`] that hands out exactly the byte chunks it was scripted
    /// with, and fails once they run out.
    ///
    /// The live suite cannot pin this behaviour on its own. `READ_CHUNK` is a
    /// MAXIMUM, not a promise: `AsyncRead::read` may return any positive count,
    /// so the phase of the partial tail against the read boundary is a property
    /// of the transport on the day, not of the decoder. A run that happens to
    /// leave fewer than 5 bytes at the tail drains even on the pre-fix decoder.
    /// Scripting the chunks removes the schedule from the experiment.
    struct ScriptedFramer {
        chunks: VecDeque<Vec<u8>>,
        buf: BytesMut,
        /// The ceiling `validate_length` enforces. `usize::MAX` for the tests
        /// that are not about the ceiling at all.
        max_message_size: usize,
    }

    impl ScriptedFramer {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            Self {
                chunks: chunks.into(),
                buf: BytesMut::new(),
                max_message_size: usize::MAX,
            }
        }
    }

    impl ReadFramer for ScriptedFramer {
        async fn fill(&mut self, min_bytes: usize) -> Result<(), Error> {
            while self.buf.len() < min_bytes {
                match self.chunks.pop_front() {
                    Some(chunk) => self.buf.extend_from_slice(&chunk),
                    // The decoder asked for bytes the script does not have.
                    // That request is itself the failure under test.
                    None => {
                        return Err(Error::io(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "decoder asked for more bytes than the script holds",
                        )));
                    }
                }
            }
            Ok(())
        }

        fn buf(&mut self) -> &mut BytesMut {
            &mut self.buf
        }

        fn peek_u32_be(&self, offset: usize) -> Option<u32> {
            let end = offset.checked_add(4)?;
            let slice = self.buf.get(offset..end)?;
            Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
        }

        fn validate_length(&self, length: u32) -> Result<(), Error> {
            let total = 1u64 + u64::from(length);
            if total > self.max_message_size as u64 {
                return Err(Error::io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "message too large: {total} bytes (max {})",
                        self.max_message_size
                    ),
                )));
            }
            Ok(())
        }
    }

    /// One single-column, non-NULL `DataRow` on the wire: tag, i32 length
    /// (counting itself but not the tag), i16 column count, i32 field length,
    /// payload.
    fn data_row(payload: &[u8]) -> Vec<u8> {
        let field_len = i32::try_from(payload.len()).expect("test payload exceeds a wire field");
        let body_len = 4 + 2 + 4 + field_len;
        let mut out = vec![b'D'];
        out.extend_from_slice(&body_len.to_be_bytes());
        out.extend_from_slice(&1i16.to_be_bytes());
        out.extend_from_slice(&field_len.to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn error_chain(error: &Error) -> String {
        std::iter::successors(std::error::Error::source(error), |source| {
            std::error::Error::source(*source)
        })
        .fold(error.to_string(), |chain, source| {
            format!("{chain}: {source}")
        })
    }

    /// Every frame in a coalesced batch is measured, not just the first.
    ///
    /// `read_backend` validates the head frame from its header and then walks
    /// the buffered bytes handing back everything complete. Only that first
    /// header went through `validate_length`, so a peer that puts a small frame
    /// in front of an oversized one - both arriving in a single read - had the
    /// second delivered despite the ceiling. `fill(5)` can append a whole
    /// `READ_CHUNK`, so one read is enough to carry both.
    ///
    /// This is a CONTRACT violation rather than an allocation vector, and the
    /// distinction is worth keeping: `Header::parse` takes `&[u8]` and cannot
    /// reserve, the walk breaks unless the frame is already buffered, and a
    /// header declaring a huge length breaks the walk and is refused as the
    /// head frame on the next iteration. What leaks through is bounded by what
    /// one read actually delivered - but a caller who set a ceiling still got a
    /// message above it.
    #[compio::test]
    async fn an_oversized_frame_behind_a_small_one_is_still_rejected() {
        // BindComplete: tag `2`, length 4, no body. Then a DataRow whose
        // declared length puts the frame over the ceiling below.
        let mut batch = vec![b'2'];
        batch.extend_from_slice(&4u32.to_be_bytes());
        batch.push(b'D');
        batch.extend_from_slice(&200u32.to_be_bytes());
        batch.extend_from_slice(&vec![0u8; 200 - 4]);

        let mut framer = ScriptedFramer::new(vec![batch]);
        framer.max_message_size = 128;

        let error = match read_backend(&mut framer).await {
            Ok(_) => panic!("an oversized frame passed the ceiling by riding behind a small one"),
            Err(error) => error,
        };
        let chain = error_chain(&error);
        assert!(
            chain.contains("message too large"),
            "the batched frame was not measured against the ceiling: {chain}"
        );
    }

    /// The negotiation body is exactly two u32 values when the client sent no
    /// `_pq_.` options. Its fixed limit must be enforced from the five-byte
    /// header, before the decoder asks the socket for an attacker-sized body.
    #[compio::test]
    async fn an_oversized_negotiation_is_rejected_from_its_header() {
        let mut frame = vec![b'v'];
        frame.extend_from_slice(&13u32.to_be_bytes());
        let mut framer = ScriptedFramer::new(vec![frame]);

        let error = match read_backend(&mut framer).await {
            Ok(_) => panic!("an oversized negotiation header was accepted"),
            Err(error) => error,
        };
        let chain = error_chain(&error);
        assert!(
            chain.contains("NegotiateProtocolVersion") && chain.contains("12"),
            "the header-specific limit was not enforced: {chain}"
        );
    }

    /// A protocol 3.2 cancel key is at most 256 bytes, so `BackendKeyData`
    /// cannot have a length field above 264. Reject that claim before reading
    /// any body bytes from an unauthenticated peer.
    #[compio::test]
    async fn an_oversized_backend_key_is_rejected_from_its_header() {
        let mut frame = vec![b'K'];
        frame.extend_from_slice(&265u32.to_be_bytes());
        let mut framer = ScriptedFramer::new(vec![frame]);

        let error = match read_backend(&mut framer).await {
            Ok(_) => panic!("an oversized BackendKeyData header was accepted"),
            Err(error) => error,
        };
        let chain = error_chain(&error);
        assert!(
            chain.contains("BackendKeyData") && chain.contains("264"),
            "the header-specific limit was not enforced: {chain}"
        );
    }

    /// A partial message at the tail must not make the decoder go back to the
    /// socket: the complete messages ahead of it are already deliverable.
    ///
    /// This is the whole of the bug that failed a 125 MiB result set. Refilling
    /// on a partial tail let a dense run of small `DataRow`s pile up in the read
    /// buffer until `BufStream::fill` refused a request above `DEFAULT_MAX_MESSAGE_SIZE`
    /// - a query killed by an accumulation whose largest single message was
    /// 16 KiB. Here the script simply runs out, so a decoder that refills gets
    /// `UnexpectedEof` and one that returns its prefix gets both rows.
    #[compio::test]
    async fn a_partial_tail_does_not_hold_back_the_complete_messages_before_it() {
        let mut framer = ScriptedFramer::new(vec![
            [
                data_row(b"first"),
                data_row(b"second"),
                // Enough of a third row to parse a header, never enough to
                // complete it.
                data_row(&[b'x'; 64])[..10].to_vec(),
            ]
            .concat(),
        ]);

        let message = read_backend(&mut framer)
            .await
            .expect("the decoder refilled instead of returning its complete prefix");

        let BackendMessage::Normal {
            mut messages,
            request_complete,
        } = message
        else {
            panic!("expected a Normal batch");
        };

        assert!(
            !request_complete,
            "a batch that never reached ReadyForQuery must not claim to be complete"
        );

        let mut payloads = Vec::new();
        while let Some(msg) = messages.next().unwrap() {
            match msg {
                backend::Message::DataRow(body) => {
                    let ranges: Vec<_> = body.ranges().collect().unwrap();
                    payloads.push(ranges.len());
                }
                _ => panic!("a message other than DataRow appeared in the batch"),
            }
        }
        assert_eq!(
            payloads.len(),
            2,
            "the complete prefix was not returned whole"
        );
    }

    /// The tag-specific startup limits apply to every frame in a batch too.
    ///
    /// `read_backend` checks `validate_startup_message_length` on the HEAD
    /// frame only. The generic ceiling is now repeated for each frame in the
    /// walk, but the per-tag limits were not, so a `BackendKeyData` claiming
    /// 265 - above the 264 its own limit allows - passed the walk whenever it
    /// arrived behind another frame in the same read. `AuthenticationOk` in
    /// front of it is exactly that shape, and it is what a real startup
    /// sequence looks like.
    ///
    /// These limits exist to refuse an over-long cancel key from an
    /// UNAUTHENTICATED peer. Bypassed, the body is buffered first and only the
    /// later cancel-key parse objects.
    ///
    /// Safe to repeat per frame because both constrained tags - `v` and `K` -
    /// are startup-only messages, so the check is a no-op on a data-phase
    /// stream.
    #[compio::test]
    async fn a_startup_limit_applies_to_a_frame_behind_another_one() {
        // AuthenticationOk: tag R, length 8, body = success code 0.
        let mut batch = vec![b'R'];
        batch.extend_from_slice(&8u32.to_be_bytes());
        batch.extend_from_slice(&0u32.to_be_bytes());
        // BackendKeyData claiming 265, one past its own ceiling of 264.
        batch.push(b'K');
        batch.extend_from_slice(&265u32.to_be_bytes());
        batch.extend_from_slice(&vec![0u8; 265 - 4]);

        let mut framer = ScriptedFramer::new(vec![batch]);

        let error = match read_backend(&mut framer).await {
            Ok(_) => panic!("an oversized BackendKeyData passed by riding behind AuthenticationOk"),
            Err(error) => error,
        };
        let chain = error_chain(&error);
        assert!(
            chain.contains("BackendKeyData") && chain.contains("264"),
            "the per-tag limit was not applied to the batched frame: {chain}"
        );
    }
}
