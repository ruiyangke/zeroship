// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// Source uses `tokio_util::codec::{Encoder, Decoder}` over a `Framed`
// wrapper. We can't depend on tokio_util, so this module replaces that
// with free functions that work against our in-house `BufStream`.
//
// The decoder semantics are preserved exactly:
//   - A single call to `read_backend` returns either one *async* message
//     (NoticeResponse / NotificationResponse / ParameterStatus) or a
//     batch of *normal* messages terminated by ReadyForQuery.
//   - Inside a `Normal` batch, callers iterate via `BackendMessages::next`.
//
// The length-cap + peek helpers from the earlier BufStream hardening pass
// still guard against an attacker-controlled length field.

use crate::Error;
use crate::buf_stream::BufStream;
use bytes::{Buf, Bytes, BytesMut};
use compio::io::{AsyncRead, AsyncWrite};
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
    Async(backend::Message),
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
pub fn write_frontend<S>(stream: &mut BufStream<S>, msg: FrontendMessage) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
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
/// it's first and returning a Normal batch terminated by ReadyForQuery
/// otherwise.
///
/// On a Normal batch, the returned `BackendMessages` owns the underlying
/// `BytesMut` slice — the stream's read buffer is drained exactly that
/// many bytes via `split_to`, so subsequent reads start fresh.
#[allow(dead_code)]
pub async fn read_backend<S>(stream: &mut BufStream<S>) -> Result<BackendMessage, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
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
        let mut need_more = false;

        while let Some(header) = backend::Header::parse(&stream.buf()[idx..]).map_err(Error::io)? {
            let msg_len = header.len() as usize + 1;
            if stream.buf()[idx..].len() < msg_len {
                // Partial message at the tail — fill more and retry.
                need_more = true;
                break;
            }

            match header.tag() {
                backend::NOTICE_RESPONSE_TAG
                | backend::NOTIFICATION_RESPONSE_TAG
                | backend::PARAMETER_STATUS_TAG => {
                    if idx == 0 {
                        // Async message sits at the head — return it alone.
                        let message = backend::Message::parse(stream.buf())
                            .map_err(Error::io)?
                            .expect("async header implies full message is buffered");
                        return Ok(BackendMessage::Async(message));
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

        if need_more {
            // Need at least one more byte than we already have to make
            // progress; fill() will read a full chunk in one syscall.
            let have = stream.buf().len();
            stream.fill(have + 1).await?;
            continue;
        }

        if idx == 0 {
            // Buffered bytes parsed into zero complete messages
            // (`Header::parse` returned None even though fill(5) ran).
            // Fill more and retry.
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
