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

/// A frontend (client → server) message ready to be serialized onto the wire.
pub enum FrontendMessage {
    /// Pre-encoded bytes (the common path: `postgres_protocol::message::frontend::*`
    /// writes directly into a `BytesMut`, which we convert to `Bytes` for cheap
    /// cloning). Used for Parse, Bind, Describe, Execute, Sync, Query, etc.
    Raw(Bytes),
    /// Out-of-band COPY payload frame. Separated from `Raw` because the
    /// encoder owns the buffer and writes the framing header in one pass.
    CopyData(CopyData<Box<dyn Buf + Send>>),
}

/// A backend (server → client) message or batch thereof.
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
    /// update — must be routed to the dedicated async channel, not the
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
/// This does not flush — callers batch multiple frontend messages
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
/// `BytesMut` slice — the stream's read buffer is drained exactly that
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
        stream.validate_length(length)?;

        // Walk the buffered bytes looking for a complete batch. Mirrors
        // tokio-postgres's codec.rs decode().
        let mut idx = 0usize;
        let mut request_complete = false;

        while let Some(header) = backend::Header::parse(&stream.buf()[idx..]).map_err(Error::io)? {
            let msg_len = header.len() as usize + 1;
            if stream.buf()[idx..].len() < msg_len {
                // Partial message at the tail. Everything before it is
                // complete, so stop walking and hand that prefix back.
                break;
            }

            match header.tag() {
                backend::NOTICE_RESPONSE_TAG
                | backend::NOTIFICATION_RESPONSE_TAG
                | backend::PARAMETER_STATUS_TAG => {
                    if idx == 0 {
                        // Async message sits at the head — return it alone.
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
            // above `MAX_MESSAGE_SIZE`, failing a query whose largest single
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
    }

    impl ScriptedFramer {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            Self {
                chunks: chunks.into(),
                buf: BytesMut::new(),
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

        fn validate_length(&self, _length: u32) -> Result<(), Error> {
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

    /// A partial message at the tail must not make the decoder go back to the
    /// socket: the complete messages ahead of it are already deliverable.
    ///
    /// This is the whole of the bug that failed a 125 MiB result set. Refilling
    /// on a partial tail let a dense run of small `DataRow`s pile up in the read
    /// buffer until `BufStream::fill` refused a request above `MAX_MESSAGE_SIZE`
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
        assert_eq!(payloads.len(), 2, "the complete prefix was not returned whole");
    }
}
