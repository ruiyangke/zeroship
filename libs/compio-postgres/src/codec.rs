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
        /// A local framing failure found after every byte in `messages` had
        /// already been validated. This is populated only when that prefix
        /// contains an ErrorResponse: the connection must retire immediately,
        /// but the wire-earlier server diagnosis still belongs to its caller.
        deferred_error: Option<Error>,
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

impl BackendMessage {
    /// Remove a local terminal failure which follows this decoded message in
    /// wire order. The split reader sends it through the same FIFO after the
    /// message; the serialized loop dispatches and unstashes the message before
    /// returning it.
    pub(crate) fn take_deferred_error(&mut self) -> Option<Error> {
        match self {
            BackendMessage::Normal { deferred_error, .. } => deferred_error.take(),
            BackendMessage::Async { .. } => None,
        }
    }
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

    #[cfg(test)]
    pub(crate) fn from_test_bytes(bytes: BytesMut) -> BackendMessages {
        BackendMessages(bytes)
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
    /// Two connection-boundary consumers inspect this without parsing the
    /// batch. Prepare cleanup uses the first tag to decide whether a cancelled
    /// `Parse` created its named statement, then must deliver the batch intact
    /// when its caller is alive. COPY-IN error recovery uses a bare leading
    /// `ReadyForQuery` to recognize the duplicate completion and must consume
    /// and drop that batch before it reaches the next response slot.
    pub(crate) fn first_tag(&self) -> Option<u8> {
        self.0.first().copied()
    }

    /// Whether this already-validated batch contains a frame with `tag`.
    ///
    /// The connection deadline uses this before handing a COPY response to a
    /// possibly backpressured consumer: PostgreSQL owes no further bytes after
    /// `CopyInResponse` until the caller supplies input.
    pub(crate) fn contains_tag(&self, tag: u8) -> bool {
        self.first_matching_tag(&[tag]).is_some()
    }

    /// Return the first frame tag in wire order which appears in `tags`.
    ///
    /// COPY direction responses can share a decoded batch. Their order still
    /// decides which protocol state the backend entered first.
    pub(crate) fn first_matching_tag(&self, tags: &[u8]) -> Option<u8> {
        let mut offset = 0usize;
        while let Some(header_end) = offset.checked_add(5) {
            let Some(header) = self.0.get(offset..header_end) else {
                return None;
            };
            let length = u32::from_be_bytes(
                header[1..5]
                    .try_into()
                    .expect("the five-byte frame header has exactly four length bytes"),
            ) as usize;
            let Some(next) = offset
                .checked_add(1)
                .and_then(|value| value.checked_add(length))
            else {
                return None;
            };
            if length < 4 || next > self.0.len() {
                return None;
            }
            if tags.contains(&header[0]) {
                return Some(header[0]);
            }
            offset = next;
        }
        None
    }

    /// Clone and decode an error only when it precedes `stop_tag`.
    ///
    /// Successful row batches can be large, so scanning their frame headers
    /// avoids copying them merely to support a rare cache-recovery path. Frame
    /// order is significant: an error after `BindComplete` is an execution
    /// error and cannot prove that the prepared statement itself was stale.
    pub(crate) fn error_response_before(
        &self,
        stop_tag: u8,
    ) -> io::Result<Option<backend::ErrorResponseBody>> {
        let mut offset = 0usize;
        let mut has_error = false;
        while let Some(header_end) = offset.checked_add(5) {
            let Some(header) = self.0.get(offset..header_end) else {
                break;
            };
            let length = u32::from_be_bytes(
                header[1..5]
                    .try_into()
                    .expect("the five-byte frame header has exactly four length bytes"),
            ) as usize;
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
            if header[0] == stop_tag {
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

    /// Clone and decode the first error response in this batch, if any.
    ///
    /// The connection dispatcher uses this before waking a response consumer:
    /// a `FATAL` or `PANIC` response means PostgreSQL is ending the session, so
    /// a pooled borrower that drops immediately must already see the connection
    /// as retired rather than briefly returning it to the idle set.
    pub(crate) fn first_error_response(&self) -> io::Result<Option<backend::ErrorResponseBody>> {
        self.find_error_response(|body| Ok(Some(body)))
    }

    /// Clone and inspect error responses in order without consuming this batch.
    ///
    /// Inspection stops when `f` returns a value, so a usable diagnosis does not
    /// depend on parsing unrelated bytes behind it. Connection-survival
    /// classification uses this to find a later FATAL/PANIC before making the
    /// batch visible to a pooled borrower.
    pub(crate) fn find_error_response<T>(
        &self,
        mut f: impl FnMut(backend::ErrorResponseBody) -> io::Result<Option<T>>,
    ) -> io::Result<Option<T>> {
        if !self.contains_tag(backend::ERROR_RESPONSE_TAG) {
            return Ok(None);
        }

        let mut messages = BackendMessages(self.0.clone());
        while let Some(message) = messages.next()? {
            if let backend::Message::ErrorResponse(body) = message
                && let Some(found) = f(body)?
            {
                return Ok(Some(found));
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
    read_backend_with_async_storage(stream, AsyncFrameStorage::Shared).await
}

/// Decode like [`read_backend`], but copy an async frame out of the stream's
/// read allocation before parsing it. Handshake notices can outlive later
/// buffer growth; detaching keeps their retained allocation proportional to
/// their charged wire size instead of pinning an obsolete read high-watermark.
pub(crate) async fn read_backend_detached_async_frames<S>(
    stream: &mut S,
) -> Result<BackendMessage, Error>
where
    S: ReadFramer + ?Sized,
{
    read_backend_with_async_storage(stream, AsyncFrameStorage::Detached).await
}

#[derive(Clone, Copy)]
enum AsyncFrameStorage {
    Shared,
    Detached,
}

async fn read_backend_with_async_storage<S>(
    stream: &mut S,
    async_storage: AsyncFrameStorage,
) -> Result<BackendMessage, Error>
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
        let mut saw_error_response = false;
        let mut deferred_error = None;

        loop {
            let header = match backend::Header::parse(&stream.buf()[idx..]) {
                Ok(Some(header)) => header,
                Ok(None) => break,
                Err(error) if saw_error_response => {
                    deferred_error = Some(Error::io(error));
                    break;
                }
                Err(error) => return Err(Error::io(error)),
            };
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
            if let Err(error) = stream.validate_length(header.len() as u32) {
                if saw_error_response {
                    deferred_error = Some(error);
                    break;
                }
                return Err(error);
            }
            // The per-tag startup limits belong here too. They were head-only,
            // so a `BackendKeyData` claiming more than its own 264 slipped
            // through whenever it arrived behind another frame - and
            // `AuthenticationOk` in front of it is what a real startup sequence
            // looks like. Repeating them is a no-op on a data-phase stream,
            // because both constrained tags are startup-only.
            if let Err(error) = validate_startup_message_length(header.tag(), header.len() as u32) {
                if saw_error_response {
                    deferred_error = Some(error);
                    break;
                }
                return Err(error);
            }

            if matches!(
                header.tag(),
                backend::COPY_IN_RESPONSE_TAG | backend::COPY_OUT_RESPONSE_TAG
            ) {
                let body_start = idx + 5;
                let body_end = idx + msg_len;
                if let Err(error) =
                    crate::copy_format::validate_wire(&stream.buf()[body_start..body_end])
                        .map_err(Error::parse)
                {
                    if saw_error_response {
                        deferred_error = Some(error);
                        break;
                    }
                    return Err(error);
                }
            }

            if header.tag() == backend::READY_FOR_QUERY_TAG
                && let Err(error) = validate_ready_for_query(
                    header.len() as u32,
                    &stream.buf()[idx + 5..idx + msg_len],
                )
            {
                if saw_error_response {
                    deferred_error = Some(error);
                    break;
                }
                return Err(error);
            }

            match header.tag() {
                backend::NOTICE_RESPONSE_TAG
                | backend::NOTIFICATION_RESPONSE_TAG
                | backend::PARAMETER_STATUS_TAG => {
                    if idx == 0 {
                        // Async message sits at the head - return it alone.
                        // Measured BEFORE the parse consumes it: `header.len()` counts
                        // itself but not the tag, so the frame is one more.
                        let frame_len = header.len() as usize + 1;
                        let message = match async_storage {
                            AsyncFrameStorage::Shared => backend::Message::parse(stream.buf()),
                            AsyncFrameStorage::Detached => {
                                let mut frame = BytesMut::from(&stream.buf()[..frame_len]);
                                stream.buf().advance(frame_len);
                                backend::Message::parse(&mut frame)
                            }
                        }
                        .map_err(Error::io)?
                        .expect(
                            "the preceding frame-length check guarantees a complete async message",
                        );
                        return Ok(BackendMessage::Async { message, frame_len });
                    } else {
                        // Normal batch terminates at this async boundary;
                        // caller will see the async message on the next call.
                        break;
                    }
                }
                _ => {}
            }

            saw_error_response |= header.tag() == backend::ERROR_RESPONSE_TAG;
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
            deferred_error,
        });
    }
}

/// ReadyForQuery has exactly one body byte and only three transaction states.
/// Validating it here keeps a malformed terminator out of the transaction-state
/// clock and lets `read_backend` preserve a wire-earlier ErrorResponse.
fn validate_ready_for_query(length: u32, body: &[u8]) -> Result<(), Error> {
    if length != 5 {
        return Err(Error::io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid ReadyForQuery length {length}; expected 5"),
        )));
    }

    let status = body[0];
    if !matches!(status, b'I' | b'T' | b'E') {
        return Err(Error::io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid ReadyForQuery transaction status 0x{status:02x}; expected I, T, or E"),
        )));
    }
    Ok(())
}

/// PostgreSQL caps the startup body whose option names this message can echo at
/// 10,000 bytes. Leave a little room for the negotiation header while keeping
/// an unauthenticated peer far below the generic 64 MiB message limit.
const MAX_NEGOTIATE_PROTOCOL_VERSION_LENGTH: u32 = 10 * 1024;

/// Apply the small startup-message limits before the decoder reads their
/// bodies. The generic connection limit is appropriate for rows but far too
/// large for either of these unauthenticated messages.
fn validate_startup_message_length(tag: u8, length: u32) -> Result<(), Error> {
    match tag {
        b'v' if !(12..=MAX_NEGOTIATE_PROTOCOL_VERSION_LENGTH).contains(&length) => {
            Err(Error::io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "invalid NegotiateProtocolVersion length {length}; expected 12 to \
                     {MAX_NEGOTIATE_PROTOCOL_VERSION_LENGTH}"
                ),
            )))
        }
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

    fn error_response(code: &str, message: &str) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(b"SERROR\0C");
        payload.extend_from_slice(code.as_bytes());
        payload.extend_from_slice(b"\0M");
        payload.extend_from_slice(message.as_bytes());
        payload.extend_from_slice(b"\0\0");

        let mut frame = vec![backend::ERROR_RESPONSE_TAG];
        frame.extend_from_slice(&(u32::try_from(payload.len()).unwrap() + 4).to_be_bytes());
        frame.extend_from_slice(&payload);
        frame
    }

    /// A complete ErrorResponse owns the diagnosis for the request even when
    /// malformed bytes later in the same socket read retire the session. The
    /// decoder must split that valid prefix from the local framing failure so
    /// dispatch can deliver SQLSTATE before connection teardown becomes
    /// visible to the caller.
    #[compio::test]
    async fn a_leading_error_response_survives_every_later_validation_failure() {
        let mut oversized = data_row(&[b'x'; 192]);
        let startup = {
            let mut frame = vec![b'K'];
            frame.extend_from_slice(&265u32.to_be_bytes());
            frame.extend_from_slice(&vec![0u8; 265 - 4]);
            frame
        };
        let malformed_copy = copy_response_frame(backend::COPY_IN_RESPONSE_TAG, b"");
        let malformed_header = [vec![b'D'], 3u32.to_be_bytes().to_vec()].concat();
        let malformed_ready_length = [
            vec![backend::READY_FOR_QUERY_TAG],
            4u32.to_be_bytes().to_vec(),
        ]
        .concat();
        let malformed_ready_status = [
            vec![backend::READY_FOR_QUERY_TAG],
            5u32.to_be_bytes().to_vec(),
            vec![b'X'],
        ]
        .concat();

        let cases = [
            ("invalid header", malformed_header, usize::MAX),
            ("message ceiling", std::mem::take(&mut oversized), 128),
            ("startup limit", startup, usize::MAX),
            ("COPY metadata", malformed_copy, usize::MAX),
            ("ReadyForQuery length", malformed_ready_length, usize::MAX),
            ("ReadyForQuery status", malformed_ready_status, usize::MAX),
        ];

        let mut failures = Vec::new();
        for (case, malformed, max_message_size) in cases {
            let mut batch = error_response("23505", "queued unique violation");
            batch.extend_from_slice(&malformed);
            let mut framer = ScriptedFramer::new(vec![batch]);
            framer.max_message_size = max_message_size;

            let mut messages = match read_backend(&mut framer).await {
                Ok(BackendMessage::Normal {
                    messages,
                    request_complete: false,
                    ..
                }) => messages,
                Ok(BackendMessage::Normal {
                    request_complete: true,
                    ..
                }) => {
                    failures.push(format!(
                        "{case} was dispatched as a completed response before SQLSTATE 23505"
                    ));
                    continue;
                }
                Ok(BackendMessage::Async { .. }) => {
                    failures.push(format!(
                        "{case} reclassified the leading ErrorResponse as asynchronous"
                    ));
                    continue;
                }
                Err(error) => {
                    failures.push(format!(
                        "{case} discarded SQLSTATE 23505: {}",
                        error_chain(&error)
                    ));
                    continue;
                }
            };

            match messages.next() {
                Ok(Some(backend::Message::ErrorResponse(body))) => {
                    let error = crate::error::DbError::parse(&mut body.fields())
                        .expect("parse the scripted ErrorResponse");
                    if error.code().code() != "23505" {
                        failures.push(format!(
                            "{case} changed SQLSTATE 23505 to {}",
                            error.code().code()
                        ));
                    }
                }
                Ok(Some(_)) => failures.push(format!(
                    "{case} delivered a local frame before SQLSTATE 23505"
                )),
                Ok(None) => failures.push(format!("{case} dropped SQLSTATE 23505")),
                Err(error) => {
                    failures.push(format!("{case} made SQLSTATE 23505 unparsable: {error}"))
                }
            }
            match messages.next() {
                Ok(None) => {}
                Ok(Some(_)) => failures.push(format!(
                    "{case} delivered the malformed frame with SQLSTATE 23505"
                )),
                Err(error) => failures.push(format!(
                    "{case} let the malformed frame corrupt SQLSTATE 23505: {error}"
                )),
            }

            let error = match read_backend(&mut framer).await {
                Ok(_) => {
                    failures.push(format!(
                        "{case} disappeared after SQLSTATE 23505 was delivered"
                    ));
                    continue;
                }
                Err(error) => error,
            };
            if error.as_db_error().is_some() {
                failures.push(format!(
                    "{case} was mislabeled as a second server ErrorResponse"
                ));
            }
        }

        assert!(
            failures.is_empty(),
            "coalesced local validation outranked the server diagnosis:\n{}",
            failures.join("\n")
        );
    }

    /// Rediscovering a malformed tail on a later decode cannot substitute for
    /// attaching its failure to the valid prefix. The split reader may publish
    /// the first batch before it reads again, so dispatch needs this side
    /// channel immediately to retire the session in the same turn.
    async fn assert_coalesced_failure_is_deferred(
        case: &str,
        malformed: Vec<u8>,
        max_message_size: usize,
        expected_error: &str,
    ) {
        let mut batch = error_response("23505", "queued unique violation");
        batch.extend_from_slice(&malformed);
        let mut framer = ScriptedFramer::new(vec![batch]);
        framer.max_message_size = max_message_size;

        let mut decoded = match read_backend(&mut framer).await {
            Ok(decoded) => decoded,
            Err(error) => panic!(
                "{case} outranked the wire-earlier ErrorResponse: {}",
                error_chain(&error)
            ),
        };
        let deferred = decoded
            .take_deferred_error()
            .unwrap_or_else(|| panic!("{case} was not attached to the first decoded batch"));
        let chain = error_chain(&deferred);
        assert!(
            chain.contains(expected_error),
            "{case} attached the wrong deferred failure: {chain}"
        );
    }

    /// `Header::parse` has a separate deferred-error arm from all semantic
    /// validators below it. Keep this non-group case when splitting the former
    /// aggregate test.
    #[compio::test]
    async fn a_malformed_header_after_error_response_is_deferred() {
        let malformed = [vec![b'D'], 3u32.to_be_bytes().to_vec()].concat();
        assert_coalesced_failure_is_deferred(
            "invalid header",
            malformed,
            usize::MAX,
            "invalid message length",
        )
        .await;
    }

    /// Bind the generic per-frame message ceiling's deferred-error copy.
    #[compio::test]
    async fn a_message_ceiling_failure_after_error_response_is_deferred() {
        assert_coalesced_failure_is_deferred(
            "message ceiling",
            data_row(&[b'x'; 192]),
            128,
            "message too large",
        )
        .await;
    }

    /// Bind the startup-tag-specific length validator's deferred-error copy.
    #[compio::test]
    async fn a_startup_limit_failure_after_error_response_is_deferred() {
        let mut malformed = vec![b'K'];
        malformed.extend_from_slice(&265u32.to_be_bytes());
        malformed.extend_from_slice(&vec![0u8; 265 - 4]);
        assert_coalesced_failure_is_deferred(
            "startup limit",
            malformed,
            usize::MAX,
            "BackendKeyData",
        )
        .await;
    }

    /// CopyInResponse and CopyOutResponse share one metadata-validation body;
    /// exercise both tags without coupling that body to the other validators.
    #[compio::test]
    async fn a_copy_metadata_failure_after_error_response_is_deferred() {
        let cases = [
            ("CopyInResponse", backend::COPY_IN_RESPONSE_TAG),
            ("CopyOutResponse", backend::COPY_OUT_RESPONSE_TAG),
        ];
        let mut ruled_on = 0;
        for (case, tag) in cases {
            assert_coalesced_failure_is_deferred(
                case,
                copy_response_frame(tag, b""),
                usize::MAX,
                "COPY response",
            )
            .await;
            ruled_on += 1;
        }
        assert_eq!(ruled_on, 2, "both COPY response tags must be ruled on");
    }

    /// Bind ReadyForQuery's length/status validator's deferred-error copy.
    #[compio::test]
    async fn a_ready_for_query_failure_after_error_response_is_deferred() {
        let malformed = [
            vec![backend::READY_FOR_QUERY_TAG],
            5u32.to_be_bytes().to_vec(),
            vec![b'X'],
        ]
        .concat();
        assert_coalesced_failure_is_deferred(
            "ReadyForQuery",
            malformed,
            usize::MAX,
            "ReadyForQuery",
        )
        .await;
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

    /// A negotiation carries two u32 values followed by the names of any
    /// unrecognized `_pq_.` options. PostgreSQL caps the startup body that
    /// supplied those names at 10,000 bytes, so this ceiling accepts every
    /// response it can produce while rejecting an attacker-sized body from the
    /// five-byte header.
    #[compio::test]
    async fn an_oversized_negotiation_is_rejected_from_its_header() {
        let mut frame = vec![b'v'];
        frame.extend_from_slice(&(MAX_NEGOTIATE_PROTOCOL_VERSION_LENGTH + 1).to_be_bytes());
        let mut framer = ScriptedFramer::new(vec![frame]);

        let error = match read_backend(&mut framer).await {
            Ok(_) => panic!("an oversized negotiation header was accepted"),
            Err(error) => error,
        };
        let chain = error_chain(&error);
        assert!(
            chain.contains("NegotiateProtocolVersion")
                && chain.contains(&MAX_NEGOTIATE_PROTOCOL_VERSION_LENGTH.to_string()),
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
            ..
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

    fn copy_response_frame(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut frame = vec![tag];
        frame.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
        frame.extend_from_slice(body);
        frame
    }

    #[test]
    fn first_matching_tag_advances_past_the_entire_leading_frame() {
        let bytes = [
            copy_response_frame(backend::PARSE_COMPLETE_TAG, b""),
            copy_response_frame(backend::COPY_IN_RESPONSE_TAG, b"\x00\x00\x00"),
        ]
        .concat();
        let messages = BackendMessages::from_test_bytes(BytesMut::from(bytes.as_slice()));

        assert_eq!(
            messages.first_matching_tag(&[
                backend::COPY_IN_RESPONSE_TAG,
                backend::COPY_OUT_RESPONSE_TAG,
            ]),
            Some(backend::COPY_IN_RESPONSE_TAG),
            "the scan did not advance by the leading frame's tag plus declared length"
        );
    }

    #[test]
    fn error_response_before_advances_past_the_entire_leading_frame() {
        let bytes = [
            copy_response_frame(backend::PARSE_COMPLETE_TAG, b""),
            error_response("26000", "scripted stale statement"),
        ]
        .concat();
        let messages = BackendMessages::from_test_bytes(BytesMut::from(bytes.as_slice()));

        let body = messages
            .error_response_before(backend::BIND_COMPLETE_TAG)
            .expect("the frame scan rejected a valid batch")
            .expect("the scan did not reach the later ErrorResponse");
        let error = Error::db(body);
        assert_eq!(
            error.code().map(crate::error::SqlState::code),
            Some("26000"),
            "the later ErrorResponse was not decoded intact"
        );
    }

    /// CopyInResponse and CopyOutResponse are not opaque transition tags. The
    /// body declares a bounded column count followed by exactly that many
    /// format codes, and every code has protocol meaning. Validate at the
    /// decoder boundary so a malformed response retires the connection even
    /// when the eventual response consumer has already been dropped.
    #[compio::test]
    async fn malformed_copy_response_metadata_is_rejected_before_dispatch() {
        let malformed: &[(&str, &[u8])] = &[
            ("empty body", b""),
            ("short fixed fields", b"\x00\x00"),
            ("missing column code", b"\x00\x00\x01"),
            ("surplus column code", b"\x00\x00\x00\x00\x00"),
            ("invalid overall code", b"\x02\x00\x00"),
            ("invalid column code", b"\x00\x00\x01\x00\x02"),
            ("binary column in text copy", b"\x00\x00\x01\x00\x01"),
        ];

        let mut ruled_on = 0usize;
        for tag in [
            backend::COPY_IN_RESPONSE_TAG,
            backend::COPY_OUT_RESPONSE_TAG,
        ] {
            for (case, body) in malformed {
                ruled_on += 1;
                let mut framer = ScriptedFramer::new(vec![copy_response_frame(tag, body)]);
                let error = match read_backend(&mut framer).await {
                    Ok(_) => panic!(
                        "{case} was accepted for COPY response tag {}",
                        char::from(tag)
                    ),
                    Err(error) => error,
                };
                let chain = error_chain(&error);
                assert!(
                    chain.contains("COPY") && chain.contains("response"),
                    "{case} for tag {} produced an unnamed error: {chain}",
                    char::from(tag)
                );
            }
        }
        assert_eq!(ruled_on, 14, "the malformed COPY metadata matrix shrank");
    }

    /// Text, binary, and the protocol's future-facing mixed-column shape all
    /// remain deliverable. Present PostgreSQL emits one format for every
    /// column, but the message design explicitly does not require that when
    /// the overall format is binary.
    #[compio::test]
    async fn well_formed_copy_response_metadata_reaches_dispatch() {
        let valid: &[&[u8]] = &[
            b"\x00\x00\x00",
            b"\x01\x00\x01\x00\x01",
            b"\x01\x00\x02\x00\x00\x00\x01",
        ];

        let mut ruled_on = 0usize;
        for tag in [
            backend::COPY_IN_RESPONSE_TAG,
            backend::COPY_OUT_RESPONSE_TAG,
        ] {
            for body in valid {
                ruled_on += 1;
                let mut framer = ScriptedFramer::new(vec![copy_response_frame(tag, body)]);
                let BackendMessage::Normal { mut messages, .. } = read_backend(&mut framer)
                    .await
                    .expect("a well-formed COPY response was rejected")
                else {
                    panic!("a COPY response was classified as asynchronous");
                };
                assert!(messages.next().unwrap().is_some());
                assert!(messages.next().unwrap().is_none());
            }
        }
        assert_eq!(ruled_on, 6, "the valid COPY metadata matrix shrank");
    }
}
