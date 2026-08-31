// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.
//
// The control flow is identical to tokio-postgres; only the trait names
// change. compio's `AsyncWriteExt::write_all` takes an owned buffer and
// returns the buffer; we throw it away with `.0`. Shutdown is a plain
// `AsyncWrite` method.

use crate::Error;
use crate::cancel_token::CancelKey;
use crate::config::{SslMode, SslNegotiation};
use crate::connect_tls;
use crate::encryption::Encryption;
use crate::error::CancelDelivery;
use crate::maybe_tls_stream::MaybeTlsStream;
use crate::tls::TlsConnect;
use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Monotonic write-boundary state shared with the caller that owns the timeout.
///
/// The timed future can be dropped after writing and before returning an
/// outcome, so the state must outlive that future rather than travel in it.
#[derive(Clone, Default)]
pub(crate) struct CancelDeliveryTracker(Arc<AtomicBool>);

impl CancelDeliveryTracker {
    fn mark_possibly_sent(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub(crate) fn delivery(&self) -> CancelDelivery {
        if self.0.load(Ordering::Acquire) {
            CancelDelivery::PossiblySent
        } else {
            CancelDelivery::Unsent
        }
    }
}

pub async fn cancel_query_raw<S, T>(
    stream: S,
    encryption: Encryption,
    mode: SslMode,
    negotiation: SslNegotiation,
    tls: T,
    has_hostname: bool,
    process_id: i32,
    secret_key: CancelKey,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: TlsConnect<S>,
{
    // The stream belongs to the caller, so there is no second socket to dial.
    // Reproduce the transport the original session actually negotiated; do
    // not re-run `allow` or `prefer` against a new peer and risk exposing the
    // key on a different transport.
    //
    // A CancelRequest carries NO query text, but it does carry the backend's
    // process id and secret key, and that pair is a BEARER CREDENTIAL: anyone
    // who reads it off the wire can cancel that backend's queries at will.
    // Sending it unencrypted is a real exposure, not a free trade.
    //
    // Two facts, kept apart because they point in opposite directions.
    // MEASURED against PostgreSQL 16: the server accepts a plaintext cancel
    // even when it is `hostssl`-only, because the postmaster dispatches a
    // CancelRequest before startup, authentication and HBA. Cancelling a
    // `pg_sleep(30)` that way returned SQLSTATE 57014 to the TLS session that
    // had issued it. So the cancel is DELIVERED.
    //
    // That is not permission to send it in the clear. libpq's plaintext cancel
    // path - `PQcancel` / `PQrequestCancel` - is DEPRECATED, and PostgreSQL's
    // own documentation gives this exact reason: it does not send the request
    // encrypted even when the original connection required encryption. The
    // current `PQcancelCreate` / `PQcancelBlocking` path reuses the original
    // connection's `sslmode`. An earlier version of this comment cited libpq as
    // precedent for plaintext; it was citing the interface upstream retired.
    //
    // `cancel_query::cancel_query` also pins the original address. This raw
    // entry point cannot do that because the stream is caller-owned, but both
    // paths replay the original transport without a retry.
    let stream = send_cancel_request_with_exact_encryption(
        stream,
        encryption,
        mode,
        negotiation,
        tls,
        has_hostname,
        process_id,
        secret_key,
    )
    .await?;
    wait_for_server_close(stream).await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn cancel_query_with_encryption<S, T>(
    stream: S,
    encryption: Encryption,
    mode: SslMode,
    negotiation: SslNegotiation,
    tls: T,
    has_hostname: bool,
    process_id: i32,
    secret_key: CancelKey,
    delivery: &CancelDeliveryTracker,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: TlsConnect<S>,
{
    let stream = send_cancel_request(
        stream,
        encryption,
        mode,
        negotiation,
        tls,
        has_hostname,
        process_id,
        secret_key,
        true,
        delivery,
    )
    .await?;
    wait_for_server_close(stream).await
}

#[allow(clippy::too_many_arguments)]
/// Send a cancel packet over the exact transport recorded for a live session.
pub(crate) async fn send_cancel_request_with_exact_encryption<S, T>(
    stream: S,
    encryption: Encryption,
    mode: SslMode,
    negotiation: SslNegotiation,
    tls: T,
    has_hostname: bool,
    process_id: i32,
    secret_key: CancelKey,
) -> Result<MaybeTlsStream<S, T::Stream>, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: TlsConnect<S>,
{
    let delivery = CancelDeliveryTracker::default();
    send_cancel_request(
        stream,
        encryption,
        mode,
        negotiation,
        tls,
        has_hostname,
        process_id,
        secret_key,
        true,
        &delivery,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn send_cancel_request<S, T>(
    stream: S,
    encryption: Encryption,
    mode: SslMode,
    negotiation: SslNegotiation,
    tls: T,
    has_hostname: bool,
    process_id: i32,
    secret_key: CancelKey,
    exact_encryption: bool,
    delivery: &CancelDeliveryTracker,
) -> Result<MaybeTlsStream<S, T::Stream>, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: TlsConnect<S>,
{
    let mut stream = if exact_encryption {
        connect_tls::negotiate_tls_exact(stream, encryption, mode, negotiation, tls, has_hostname)
            .await?
    } else {
        connect_tls::negotiate_tls(stream, encryption, mode, negotiation, tls, has_hostname).await?
    };

    let packet_len = 12 + secret_key.as_bytes().len();
    let mut packet = Vec::with_capacity(packet_len);
    packet.extend_from_slice(&(packet_len as u32).to_be_bytes());
    packet.extend_from_slice(&80_877_102u32.to_be_bytes());
    packet.extend_from_slice(&process_id.to_be_bytes());
    packet.extend_from_slice(secret_key.as_bytes());

    // Mark before polling write_all: an error can follow a partial write.
    // compio's write_all is owned-buffer. Throw away the buffer.
    delivery.mark_possibly_sent();
    let compio::BufResult(res, _) = stream.write_all(packet).await;
    res.map_err(Error::io)?;
    stream.flush().await.map_err(Error::io)?;
    stream.shutdown().await.map_err(Error::io)?;

    Ok(stream)
}

/// Wait until the postmaster closes a connection whose cancel packet was sent.
///
/// `CancelRequest` has no protocol response. EOF is nevertheless a delivery
/// barrier: `PostgreSQL` closes this dedicated connection only after consuming
/// the startup packet. Without it, a delayed cancel could arrive after the
/// main session's `Sync` and cancel that backend's next query.
pub(crate) async fn wait_for_server_close<S, T>(
    mut stream: MaybeTlsStream<S, T>,
) -> Result<(), Error>
where
    S: AsyncRead + Unpin,
    T: AsyncRead + Unpin,
{
    let tls = matches!(&stream, MaybeTlsStream::Tls(_));
    let compio::BufResult(result, _) = stream.read(vec![0; 1]).await;
    match result {
        Ok(0) => Ok(()),
        Ok(_) => Err(Error::io(io::Error::new(
            io::ErrorKind::InvalidData,
            "PostgreSQL sent data on a CancelRequest connection",
        ))),
        // PostgreSQL closes the cancel connection without a TLS close_notify.
        // rustls reports that confirmed TCP EOF as UnexpectedEof; accepting it
        // here is specific to this one-way startup packet, not a general
        // relaxation of TLS truncation.
        Err(error) if tls && error.kind() == io::ErrorKind::UnexpectedEof => Ok(()),
        Err(error) => Err(Error::io(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::{CancelDeliveryTracker, send_cancel_request, wait_for_server_close};
    use crate::NoTls;
    use crate::cancel_token::CancelKey;
    use crate::config::{SslMode, SslNegotiation};
    use crate::encryption::Encryption;
    use crate::error::CancelDelivery;
    use crate::maybe_tls_stream::MaybeTlsStream;
    use bytes::Bytes;
    use compio::buf::{BufResult, IoBuf, IoBufMut};
    use compio::io::{AsyncRead, AsyncWrite};
    use std::io;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const PROCESS_ID: i32 = 0x1020_3040;

    fn cancel_key(bytes: &'static [u8]) -> CancelKey {
        CancelKey::new(Bytes::from_static(bytes)).expect("valid scripted cancel key")
    }

    struct PartialWriteStream {
        writes: Arc<AtomicUsize>,
    }

    impl AsyncRead for PartialWriteStream {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            let mut eof: &[u8] = &[];
            eof.read(buf).await
        }
    }

    impl AsyncWrite for PartialWriteStream {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            let attempt = self.writes.fetch_add(1, Ordering::Relaxed) + 1;
            match attempt {
                1 => {
                    assert!(buf.buf_len() > 1, "cancel packet fixture is too short");
                    BufResult(Ok(1), buf)
                }
                2 => BufResult(
                    Err(io::Error::other("scripted error after a partial write")),
                    buf,
                ),
                _ => panic!("write_all retried after its scripted error"),
            }
        }

        async fn flush(&mut self) -> io::Result<()> {
            panic!("a failed write must not be flushed")
        }

        async fn shutdown(&mut self) -> io::Result<()> {
            panic!("a failed write must not be shut down")
        }
    }

    #[compio::test]
    async fn a_partial_cancel_write_is_tracked_as_possibly_sent() {
        let writes = Arc::new(AtomicUsize::new(0));
        let delivery = CancelDeliveryTracker::default();
        let result = send_cancel_request(
            PartialWriteStream {
                writes: Arc::clone(&writes),
            },
            Encryption::Plaintext,
            SslMode::Disable,
            SslNegotiation::Postgres,
            NoTls,
            false,
            PROCESS_ID,
            cancel_key(b"key!"),
            true,
            &delivery,
        )
        .await;

        assert!(result.is_err(), "the scripted partial write did not fail");
        assert_eq!(
            writes.load(Ordering::Relaxed),
            2,
            "the fixture did not make progress before returning its error"
        );
        assert_eq!(
            delivery.delivery(),
            CancelDelivery::PossiblySent,
            "a cancel that reached the write boundary was reported as unsent"
        );
    }

    #[derive(Debug, PartialEq, Eq)]
    enum WriteEvent {
        Write(Vec<u8>),
        Flush,
        Shutdown,
    }

    struct RecordingStream {
        events: Arc<parking_lot::Mutex<Vec<WriteEvent>>>,
    }

    impl AsyncRead for RecordingStream {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            let mut eof: &[u8] = &[];
            eof.read(buf).await
        }
    }

    impl AsyncWrite for RecordingStream {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            let len = buf.buf_len();
            self.events
                .lock()
                .push(WriteEvent::Write(buf.as_init().to_vec()));
            BufResult(Ok(len), buf)
        }

        async fn flush(&mut self) -> io::Result<()> {
            self.events.lock().push(WriteEvent::Flush);
            Ok(())
        }

        async fn shutdown(&mut self) -> io::Result<()> {
            self.events.lock().push(WriteEvent::Shutdown);
            Ok(())
        }
    }

    #[compio::test]
    async fn a_variable_key_packet_is_flushed_then_shutdown() {
        let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let delivery = CancelDeliveryTracker::default();
        let result = send_cancel_request(
            RecordingStream {
                events: Arc::clone(&events),
            },
            Encryption::Plaintext,
            SslMode::Disable,
            SslNegotiation::Postgres,
            NoTls,
            false,
            PROCESS_ID,
            cancel_key(b"eightkey"),
            true,
            &delivery,
        )
        .await;
        if let Err(error) = result {
            panic!("recording the cancel request failed: {error}");
        }

        assert_eq!(
            *events.lock(),
            vec![
                WriteEvent::Write(vec![
                    0, 0, 0, 20, 4, 210, 22, 46, 16, 32, 48, 64, b'e', b'i', b'g', b'h', b't',
                    b'k', b'e', b'y',
                ]),
                WriteEvent::Flush,
                WriteEvent::Shutdown,
            ],
            "the variable-key CancelRequest write/flush/shutdown sequence changed"
        );
    }

    struct OneByteResponse;

    impl AsyncRead for OneByteResponse {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            let mut response: &[u8] = b"X";
            response.read(buf).await
        }
    }

    #[compio::test]
    async fn server_data_is_not_an_eof_delivery_barrier() {
        let result = wait_for_server_close(
            MaybeTlsStream::<OneByteResponse, OneByteResponse>::Raw(OneByteResponse),
        )
        .await;
        let error = match result {
            Ok(()) => panic!("server data was accepted as CancelRequest EOF"),
            Err(error) => error,
        };
        let io_error = std::error::Error::source(&error)
            .and_then(|cause| cause.downcast_ref::<io::Error>())
            .expect("unexpected CancelRequest data did not produce an I/O error");
        assert_eq!(io_error.kind(), io::ErrorKind::InvalidData);
    }
}
