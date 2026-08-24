//! Drive a rustls handshake directly on compio I/O, with no poll-based bridge.
//!
//! rustls is SANS-IO: [`ClientConnection`] is a state machine that never
//! touches a socket. It asks for bytes with `read_tls(&mut dyn Read)`, hands
//! bytes back with `write_tls(&mut dyn Write)`, and those are SYNCHRONOUS
//! `std::io` traits over buffers we own. That is a good fit for completion-based
//! I/O rather than an obstacle: we read into an owned buffer, feed the buffer
//! in, drain the reply into another buffer, and write that.
//!
//! # Why this exists rather than `compio-tls`
//!
//! `compio-tls` wraps `futures-rustls` and keeps the session private. Its
//! `TlsStream` is a private enum with no `split`, no `into_inner`, and no
//! accessor for the `ClientConnection` - 0.9.1 and 0.10.0 alike, checked in the
//! vendored source. `tls_rustls.rs` already works around that once, reading
//! channel-binding material off the raw `futures-rustls` stream "before handing
//! the stream to compio-tls, which exposes only the negotiated ALPN".
//!
//! Owning the handshake removes that whole class of question. It also yields
//! `(socket, connection)` as two separate values, which is what lets a TLS
//! connection be split later: the socket halves go one way, the session is
//! shared, and the connection task stops needing a second run-loop. See task
//! #49 - the point of the exercise is that TLS is a transport and must not
//! decide which protocol implementation runs.
//!
//! Nothing here duplicates `futures-rustls`'s poll bridge
//! (`AsyncStream` + `SyncStream`, 524 lines of self-referential pinned
//! futures). The state machine is driven directly.

use compio::buf::{BufResult, IoBuf, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use rustls::ClientConnection;
use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::rc::Rc;

/// Bytes requested per socket read while handshaking.
///
/// A TLS record is at most 16 KiB of plaintext plus overhead, so this holds a
/// whole record in the common case without over-allocating for the handshake,
/// which is a handful of records.
const READ_CHUNK: usize = 16 * 1024;

/// How much unsent ciphertext the outbound queue holds before `collect_outgoing`
/// stops draining rustls.
///
/// Matches rustls' own outbound limit, so the two together bound what one
/// connection can hold to roughly twice it rather than to nothing at all.
/// SOFT because it is checked before a `write_tls`, not inside one: a single
/// call may carry the queue past it, and that is fine - the point is that the
/// queue cannot grow without end, not that it never exceeds one figure.
const OUTGOING_SOFT_CAP: usize = 64 * 1024;

/// Complete a TLS handshake over `socket`, returning both halves of the result.
///
/// On success the connection is past `is_handshaking`, and any application
/// bytes the server sent alongside the final flight are already inside
/// `connection` - readable through its `reader()`. That matters: those bytes
/// have left the socket, so a caller that kept only the socket would lose them.
/// Returning the connection is what makes the pair safe to separate.
pub(crate) async fn handshake<S>(
    mut socket: S,
    mut connection: ClientConnection,
) -> io::Result<(S, ClientConnection)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        // Flush first, always. rustls will not make progress on a flight it
        // has not been allowed to send, and the server will not answer one it
        // has not received, so reading before writing deadlocks the handshake
        // rather than merely delaying it.
        while connection.wants_write() {
            let mut out = Vec::new();
            connection.write_tls(&mut out)?;
            if out.is_empty() {
                break;
            }
            let BufResult(written, _) = socket.write_all(out).await;
            written?;
        }

        if !connection.is_handshaking() {
            return Ok((socket, connection));
        }

        let BufResult(read, buffer) = socket.read(Vec::with_capacity(READ_CHUNK)).await;
        let read = read?;
        if read == 0 {
            // A truncated handshake is a TLS-level failure, not an ending:
            // rustls treats silent truncation as an attack. The message names
            // the handshake so the cause is legible in a connect error.
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the peer closed the connection during the TLS handshake",
            ));
        }

        // `read_tls` consumes as much as one record boundary allows, so this
        // loops until the chunk is drained. `process_new_packets` must run
        // between reads, not after them: it is what advances the state machine,
        // and it is where a bad certificate or a protocol violation surfaces.
        //
        // This loop does NOT return to `reader()` between steps, which is the
        // shape that broke the steady-state path (see `feed_ciphertext_step`).
        // It is safe HERE and only here: PostgreSQL sends nothing before it has
        // seen the startup packet, and the startup packet cannot be sent until
        // this function returns, so there is no application plaintext to
        // accumulate. `read_tls` refuses on unread PLAINTEXT, and pre-startup
        // there is none. Do not copy this loop into a path that runs later.
        let mut pending = &buffer[..read];
        while !pending.is_empty() {
            connection.read_tls(&mut pending)?;
            connection
                .process_new_packets()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        }
    }
}

/// A rustls session plus the ciphertext it has produced and not yet handed to
/// a socket.
///
/// Every method here is SYNCHRONOUS and none of them may be made async. That is
/// the invariant that makes sharing the session between two concurrently
/// running halves sound: a `RefCell` borrow never spans a suspension point, so
/// the halves can never both hold one.
pub(crate) struct TlsSession {
    conn: ClientConnection,
    outgoing: Vec<u8>,
}

impl TlsSession {
    pub(crate) fn new(conn: ClientConnection) -> Self {
        Self {
            conn,
            outgoing: Vec::new(),
        }
    }

    pub(crate) fn connection(&self) -> &ClientConnection {
        &self.conn
    }

    /// Move any ciphertext rustls is holding into the outbound queue, up to
    /// [`OUTGOING_SOFT_CAP`].
    ///
    /// The loop stops on no progress rather than on `wants_write()` alone: a
    /// zero-byte `write_tls` that left the flag set would spin forever.
    ///
    /// The cap RESTORES a bound that draining rustls removes. rustls holds its
    /// own outbound ciphertext behind a 64 KiB limit, so a peer cannot make it
    /// buffer without end; `write_tls` moves those bytes into a plain `Vec`
    /// that has no such limit. That matters because the READ path also
    /// produces ciphertext (a `KeyUpdate` or an alert is answered), while only
    /// the WRITE half empties the queue - so on an idle `LISTEN` connection,
    /// which reads forever and never writes, a server that sent key updates in
    /// a loop would grow this `Vec` unboundedly. Leaving the surplus inside
    /// rustls hands the back-pressure back to rustls, which is where the
    /// bookkeeping for it already exists.
    fn collect_outgoing(&mut self) -> io::Result<()> {
        while self.conn.wants_write() {
            if self.outgoing.len() >= OUTGOING_SOFT_CAP {
                break;
            }
            let before = self.outgoing.len();
            self.conn.write_tls(&mut self.outgoing)?;
            if self.outgoing.len() == before {
                break;
            }
        }
        Ok(())
    }

    /// Hand rustls ONE step of received ciphertext, and report how much it
    /// took.
    ///
    /// One step, not the whole buffer, and that is the contract rather than a
    /// preference. `read_tls` refuses outright - `Err("received plaintext
    /// buffer full")` - once 64 KiB of DECRYPTED bytes are sitting unread, and
    /// rustls' own documentation on `read_tls` says to empty `reader()` after
    /// each `process_new_packets`. A loop that drains a whole socket chunk
    /// through `read_tls`/`process_new_packets` without returning to the
    /// reader in between can cross that line and kill the connection.
    ///
    /// MEASURED 2026-08-24: it did. `concurrent_large_bidirectional_queries_do
    /// _not_deadlock` streams 4 MB parameters while the server floods results
    /// back, and the connection died with `error communicating with the
    /// server` - this error, surfacing as an I/O failure several layers up.
    /// The caller loops back to `reader()` between every step now.
    /// Returns whether the peer has sent `close_notify`, which the caller needs
    /// to tell a clean shutdown from a stall: `read_tls` answers `Ok(0)`
    /// unconditionally once that alert has arrived, so "rustls took nothing"
    /// means END OF STREAM in that case and a protocol failure otherwise.
    fn feed_ciphertext_step(&mut self, src: &mut &[u8]) -> io::Result<bool> {
        self.conn.read_tls(src)?;
        let state = self
            .conn
            .process_new_packets()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let peer_has_closed = state.peer_has_closed();
        self.collect_outgoing()?;
        Ok(peer_has_closed)
    }

    /// Decrypted bytes, or 0 when rustls has none buffered.
    ///
    /// `WouldBlock` from rustls means "no plaintext yet", which is a state and
    /// not a failure - the caller answers it by reading more ciphertext.
    fn read_plaintext(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        match self.conn.reader().read(dst) {
            Ok(n) => Ok(n),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(0),
            Err(error) => Err(error),
        }
    }

    /// Encrypt plaintext into the outbound queue.
    fn write_plaintext(&mut self, src: &[u8]) -> io::Result<usize> {
        let n = self.conn.writer().write(src)?;
        self.collect_outgoing()?;
        Ok(n)
    }

    fn take_outgoing(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.outgoing)
    }

    fn send_close_notify(&mut self) -> io::Result<()> {
        self.conn.send_close_notify();
        self.collect_outgoing()
    }
}

/// The handle both halves share.
pub(crate) type SharedSession = Rc<RefCell<TlsSession>>;

pub(crate) fn share(conn: ClientConnection) -> SharedSession {
    Rc::new(RefCell::new(TlsSession::new(conn)))
}

/// Copy `src` into a compio buffer and declare that many bytes valid.
///
/// The ONLY `unsafe` in this crate, and the reason is mechanical: compio's
/// `IoBufMut` describes a buffer that may be partly uninitialized, so nothing
/// but the buffer itself can be told how much of it is now valid.
///
/// The copy is not free, and it is not accidental either. Reading rustls'
/// plaintext straight into the caller's buffer would need a `&mut [u8]` over
/// uninitialized memory, which on compio-buf 0.8.1 costs a second `unsafe`
/// (`assume_init_mut` is unstable, so it would be a raw-pointer cast). One
/// memcpy of at most a read chunk is the cheaper thing to be sure of.
fn commit<B: IoBufMut>(buf: &mut B, src: &[u8]) {
    let uninit = buf.as_uninit();
    // The `min` is a safety floor for `set_len`, NOT an expected outcome. The
    // caller sizes its fill by this same buffer's capacity, so a shorter buffer
    // here would mean plaintext rustls has already decrypted gets dropped on
    // the floor - a silent short read, which the protocol layer would see as a
    // truncated frame rather than as a bug here. Assert it so a future change
    // that breaks the sizing fails loudly instead of corrupting a stream.
    debug_assert!(
        src.len() <= uninit.len(),
        "commit would truncate {} decrypted bytes into a {}-byte buffer",
        src.len(),
        uninit.len()
    );
    let n = src.len().min(uninit.len());
    for (slot, byte) in uninit.iter_mut().zip(&src[..n]) {
        // `MaybeUninit::write` is safe: it initializes the slot.
        slot.write(*byte);
    }
    // SAFETY: `set_len` requires (1) `n <= as_uninit().len()`, which the `min`
    // above guarantees, and (2) that every byte in `[buf_len(), n)` is
    // initialized - the loop just wrote all n of them through
    // `MaybeUninit::write`, and `buf_len() <= n` because a freshly handed-over
    // read buffer reports length 0.
    #[allow(unsafe_code)]
    unsafe {
        buf.set_len(n);
    }
}

/// The read side of a TLS session: a source of ciphertext plus the scratch the
/// decrypt loop needs.
///
/// This owns the socket rather than borrowing it because compio's `AsyncRead`
/// is a completion API - buffers and streams are passed by value - and because
/// an inherent `&mut self` method sidesteps the `'static` bound the trait's
/// returned future imposes on any free-standing `&mut [u8]` argument.
pub(crate) struct TlsReader<R> {
    socket: R,
    session: SharedSession,
    /// Ciphertext straight off the socket.
    cipher: Vec<u8>,
    /// Bytes of `cipher` the last socket read produced.
    cipher_len: usize,
    /// How much of that rustls has taken. The gap between the two is fed one
    /// step at a time, draining decrypted bytes in between.
    cipher_read: usize,
    /// Decrypted bytes, staged before being copied into the caller's buffer.
    plain: Vec<u8>,
}

impl<R> TlsReader<R> {
    pub(crate) fn new(socket: R, session: SharedSession) -> Self {
        Self {
            socket,
            session,
            cipher: Vec::new(),
            cipher_len: 0,
            cipher_read: 0,
            plain: Vec::new(),
        }
    }

    fn socket_mut(&mut self) -> &mut R {
        &mut self.socket
    }

    fn session(&self) -> &SharedSession {
        &self.session
    }
}

impl<R> TlsReader<R>
where
    R: AsyncRead + Unpin,
{
    /// Stage up to `cap` plaintext bytes in `self.plain`, reading ciphertext
    /// until some arrive.
    ///
    /// Returns 0 only at a genuine end of stream. Returning 0 merely because
    /// rustls had nothing buffered YET would read to the connection loop as the
    /// server hanging up.
    async fn fill(&mut self, cap: usize) -> io::Result<usize> {
        if cap == 0 {
            return Ok(0);
        }
        if self.plain.len() < cap {
            self.plain.resize(cap, 0);
        }
        loop {
            // ALWAYS the first thing in the loop. Every path back here - a
            // fresh socket chunk, or another step through one already held -
            // returns to the reader before giving rustls more, which is the
            // condition `feed_ciphertext_step` documents.
            let n = self
                .session
                .borrow_mut()
                .read_plaintext(&mut self.plain[..cap])?;
            if n > 0 {
                return Ok(n);
            }

            // Ciphertext already read but not yet handed over: give rustls one
            // step of it, then go back and drain.
            if self.cipher_read < self.cipher_len {
                let mut src = &self.cipher[self.cipher_read..self.cipher_len];
                let before = src.len();
                let outcome = self.session.borrow_mut().feed_ciphertext_step(&mut src);
                self.cipher_read += before - src.len();
                let peer_has_closed = outcome?;
                if before == src.len() {
                    // rustls took nothing. That is END OF STREAM when the peer
                    // has sent close_notify - `read_tls` answers `Ok(0)`
                    // unconditionally from then on - and a protocol failure
                    // otherwise, where dropping the remainder would silently
                    // desynchronise the stream. Treating the first case as the
                    // second turns an orderly shutdown with trailing buffered
                    // ciphertext into a spurious error.
                    if peer_has_closed {
                        return self
                            .session
                            .borrow_mut()
                            .read_plaintext(&mut self.plain[..cap]);
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "the TLS session stopped accepting ciphertext",
                    ));
                }
                continue;
            }

            // Read one chunk of ciphertext. Inlined rather than a helper on
            // purpose: every nested async frame here lands in the layout of the
            // caller's async block, and this one sits under the connection task
            // of every consumer of this crate. Splitting it back out pushed
            // five downstream crates past rustc's default query-depth limit.
            if self.cipher.is_empty() {
                self.cipher = vec![0u8; READ_CHUNK];
            }
            let buf = std::mem::take(&mut self.cipher);
            let BufResult(result, buf) = self.socket.read(buf).await;
            self.cipher = buf;
            let read = result?;
            if read == 0 {
                // The socket is done, but anything rustls decrypted before the
                // close is still owed to the caller. Only after that is this a
                // real end of stream.
                return self
                    .session
                    .borrow_mut()
                    .read_plaintext(&mut self.plain[..cap]);
            }
            self.cipher_read = 0;
            self.cipher_len = read;
        }
    }

    async fn read_into<B: IoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
        let cap = buf.buf_capacity();
        match self.fill(cap).await {
            Ok(n) => {
                commit(&mut buf, &self.plain[..n]);
                BufResult(Ok(n), buf)
            }
            Err(error) => BufResult(Err(error), buf),
        }
    }
}

/// Push everything queued to `socket`.
///
/// Takes the queue by value so the `RefCell` borrow ends before the write is
/// awaited. Anything the read path appends meanwhile leaves with the next
/// flush.
async fn flush_outgoing<W>(socket: &mut W, session: &SharedSession) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    loop {
        let pending = session.borrow_mut().take_outgoing();
        if pending.is_empty() {
            return Ok(());
        }
        let BufResult(result, _) = socket.write_all(pending).await;
        result?;
    }
}

async fn write_through<W>(socket: &mut W, session: &SharedSession, src: &[u8]) -> io::Result<usize>
where
    W: AsyncWrite + Unpin,
{
    let n = session.borrow_mut().write_plaintext(src)?;
    flush_outgoing(socket, session).await?;
    Ok(n)
}

async fn shutdown_through<W>(socket: &mut W, session: &SharedSession) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    session.borrow_mut().send_close_notify()?;
    flush_outgoing(socket, session).await?;
    socket.shutdown().await
}

/// A TLS stream that still owns its socket, and so can be split.
pub(crate) struct TlsStreamCore<S> {
    reader: TlsReader<S>,
}

impl<S> TlsStreamCore<S> {
    pub(crate) fn new(socket: S, session: SharedSession) -> Self {
        Self {
            reader: TlsReader::new(socket, session),
        }
    }

    pub(crate) fn into_parts(self) -> (S, SharedSession) {
        (self.reader.socket, self.reader.session)
    }

    pub(crate) fn session(&self) -> &SharedSession {
        self.reader.session()
    }
}

impl<S> AsyncRead for TlsStreamCore<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        self.reader.read_into(buf).await
    }
}

impl<S> AsyncWrite for TlsStreamCore<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let session = self.reader.session.clone();
        let result = write_through(self.reader.socket_mut(), &session, buf.as_init()).await;
        BufResult(result, buf)
    }

    async fn flush(&mut self) -> io::Result<()> {
        let session = self.reader.session.clone();
        flush_outgoing(self.reader.socket_mut(), &session).await?;
        self.reader.socket_mut().flush().await
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        let session = self.reader.session.clone();
        shutdown_through(self.reader.socket_mut(), &session).await
    }
}

/// Owned read half: the socket's read side plus a share of the session.
pub struct TlsReadHalf<R> {
    reader: TlsReader<R>,
}

impl<R> TlsReadHalf<R> {
    pub(crate) fn new(socket: R, session: SharedSession) -> Self {
        Self {
            reader: TlsReader::new(socket, session),
        }
    }
}

impl<R> AsyncRead for TlsReadHalf<R>
where
    R: AsyncRead + Unpin,
{
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        self.reader.read_into(buf).await
    }
}

/// Owned write half: the socket's write side plus a share of the session.
pub struct TlsWriteHalf<W> {
    socket: W,
    session: SharedSession,
}

impl<W> TlsWriteHalf<W> {
    pub(crate) fn new(socket: W, session: SharedSession) -> Self {
        Self { socket, session }
    }
}

impl<W> AsyncWrite for TlsWriteHalf<W>
where
    W: AsyncWrite + Unpin,
{
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let result = write_through(&mut self.socket, &self.session, buf.as_init()).await;
        BufResult(result, buf)
    }

    async fn flush(&mut self) -> io::Result<()> {
        flush_outgoing(&mut self.socket, &self.session).await?;
        self.socket.flush().await
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        shutdown_through(&mut self.socket, &self.session).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compio::buf::{IoBuf, IoBufMut};
    use std::sync::Arc;

    /// A peer that accepts every byte and answers every read with EOF.
    ///
    /// Enough to drive the handshake loop through one full iteration: the
    /// ClientHello is flushed, the connection is still handshaking, and the
    /// read that follows finds the peer gone.
    struct SilentPeer {
        written: usize,
    }

    impl AsyncRead for SilentPeer {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
            BufResult(Ok(0), buf)
        }
    }

    impl AsyncWrite for SilentPeer {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            self.written += buf.buf_len();
            BufResult(Ok(buf.buf_len()), buf)
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A peer that hands over one scripted chunk per read, then EOF.
    ///
    /// One chunk per read is the whole point. `read_tls` will happily swallow
    /// an entire buffer in a single call, so a peer that delivered everything
    /// at once could never leave bytes sitting behind an already-processed
    /// `close_notify` - which is the state under test.
    struct ScriptedPeer {
        chunks: std::collections::VecDeque<Vec<u8>>,
    }

    impl AsyncRead for ScriptedPeer {
        async fn read<B: IoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
            match self.chunks.pop_front() {
                Some(bytes) => {
                    assert!(
                        bytes.len() <= buf.buf_capacity(),
                        "the test peer's chunk must fit in one read"
                    );
                    let n = bytes.len();
                    commit(&mut buf, &bytes);
                    BufResult(Ok(n), buf)
                }
                None => BufResult(Ok(0), buf),
            }
        }
    }

    /// Drive a real client/server handshake entirely in memory and return the
    /// finished client session plus a sink for whatever the server says next.
    ///
    /// A real handshake, not a stub: the state this test is about
    /// (`read_tls` answering `Ok(0)` forever once `close_notify` has arrived)
    /// only exists behind live keys.
    fn handshaken_pair() -> (ClientConnection, rustls::ServerConnection) {
        let issued = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("generate a self-signed certificate");
        let cert = rustls::pki_types::CertificateDer::from(issued.cert.der().to_vec());
        let key = rustls::pki_types::PrivateKeyDer::try_from(issued.signing_key.serialize_der())
            .expect("serialize the test key");

        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let server_config = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .expect("server protocol versions")
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key)
            .expect("server config");

        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert).expect("trust the test certificate");
        let client_config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("client protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();

        let mut client = ClientConnection::new(
            Arc::new(client_config),
            rustls::pki_types::ServerName::try_from("localhost").expect("server name"),
        )
        .expect("client connection");
        let mut server =
            rustls::ServerConnection::new(Arc::new(server_config)).expect("server connection");

        // Pump both directions until neither has anything left to say.
        for _ in 0..16 {
            let mut to_server = Vec::new();
            while client.wants_write() {
                client.write_tls(&mut to_server).expect("client write_tls");
            }
            if !to_server.is_empty() {
                let mut cursor = &to_server[..];
                while !cursor.is_empty() {
                    server.read_tls(&mut cursor).expect("server read_tls");
                    server.process_new_packets().expect("server process");
                }
            }
            let mut to_client = Vec::new();
            while server.wants_write() {
                server.write_tls(&mut to_client).expect("server write_tls");
            }
            if !to_client.is_empty() {
                let mut cursor = &to_client[..];
                while !cursor.is_empty() {
                    client.read_tls(&mut cursor).expect("client read_tls");
                    client.process_new_packets().expect("client process");
                }
            }
            if !client.is_handshaking() && !server.is_handshaking() {
                return (client, server);
            }
        }
        panic!("the in-memory handshake did not converge");
    }

    /// An orderly `close_notify` with ciphertext still buffered behind it is
    /// END OF STREAM, not a protocol error.
    ///
    /// `read_tls` answers `Ok(0)` unconditionally once that alert has been
    /// processed. The decrypt loop reads "rustls took nothing" as a stall and
    /// refuses, which is right for a genuine stall and wrong here - so it has
    /// to ask whether the peer closed before deciding. Anything after the alert
    /// is unreachable by definition, and a trailing byte is what forces the
    /// loop to look at the buffer again after the close.
    ///
    /// This test does NOT cover a stall that is NOT a close: that arm still
    /// returns the protocol error, and nothing here exercises it.
    #[compio::test]
    async fn a_close_notify_with_trailing_ciphertext_ends_the_stream() {
        let (client, mut server) = handshaken_pair();

        let mut wire = Vec::new();
        server
            .writer()
            .write_all(b"payload")
            .expect("queue application data");
        server.send_close_notify();
        while server.wants_write() {
            server.write_tls(&mut wire).expect("server write_tls");
        }
        // A SEPARATE read, delivered after the alert has been processed. It is
        // unreachable by the protocol, and that is the point: the loop has to
        // consult rustls once more with bytes in hand, and see `Ok(0)`. Put in
        // the same chunk it would simply be swallowed with everything else.
        let session = share(client);
        let mut reader = TlsReader::new(
            ScriptedPeer {
                chunks: std::collections::VecDeque::from(vec![wire, vec![0x17]]),
            },
            session,
        );

        let mut out = vec![0u8; 64];
        let n = reader
            .fill(out.len())
            .await
            .expect("the application data before the alert must still arrive");
        out[..n].copy_from_slice(&reader.plain[..n]);
        assert_eq!(&out[..n], b"payload");

        assert_eq!(
            reader
                .fill(64)
                .await
                .expect("a close_notify is an ending, not a protocol failure"),
            0,
            "the stream must report end of file after close_notify"
        );
    }

    fn client_connection() -> ClientConnection {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("default protocol versions")
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        ClientConnection::new(
            Arc::new(config),
            rustls::pki_types::ServerName::try_from("example.invalid").expect("server name"),
        )
        .expect("client connection")
    }

    /// A peer that vanishes mid-handshake is a TLS-level failure, not a quiet
    /// end. rustls treats silent truncation as an attack rather than a close,
    /// and a caller that saw a bare EOF here could not tell "the server hung

    /// Draining rustls must not turn its bounded outbound buffer into an
    /// unbounded one of ours.
    ///
    /// rustls caps what it will hold; `write_tls` moves those bytes into a
    /// plain `Vec`. Since the READ path also produces ciphertext while only
    /// the WRITE half empties the queue, an idle connection whose peer keeps
    /// generating responses would grow that `Vec` forever if nothing stopped
    /// it. The queue is pre-loaded to the cap here, which is the state an idle
    /// connection reaches; a fresh session has only a ClientHello to give and
    /// could never demonstrate the bound.
    #[test]
    fn a_full_outbound_queue_stops_draining_the_session() {
        let mut session = TlsSession::new(client_connection());
        assert!(
            session.conn.wants_write(),
            "the fixture must have something to write, or this asserts nothing"
        );

        session.outgoing = vec![0u8; OUTGOING_SOFT_CAP];
        session
            .collect_outgoing()
            .expect("collecting into a full queue is not an error");
        assert_eq!(
            session.outgoing.len(),
            OUTGOING_SOFT_CAP,
            "a queue already at the cap must not grow"
        );
        assert!(
            session.conn.wants_write(),
            "the ciphertext must still be inside rustls, which is what bounds it"
        );

        // The control: with room, the same call DOES drain. Without this the
        // assertion above would also pass if `collect_outgoing` never worked.
        session.outgoing.clear();
        session
            .collect_outgoing()
            .expect("collecting into an empty queue");
        assert!(
            !session.outgoing.is_empty(),
            "collect_outgoing moved nothing even with an empty queue"
        );
    }
    /// up" from "someone cut the connection during key exchange".
    #[compio::test]
    async fn a_peer_that_closes_mid_handshake_is_refused() {
        let mut peer = SilentPeer { written: 0 };
        let error = handshake(&mut peer, client_connection())
            .await
            .err()
            .expect("a truncated handshake must not succeed");

        let rendered = format!("{error}");
        let chain = std::iter::successors(std::error::Error::source(&error), |error| {
            std::error::Error::source(*error)
        })
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join("; ");
        assert!(
            rendered.contains("TLS") || chain.contains("TLS") || chain.contains("handshake"),
            "the refusal must read as a TLS failure: {rendered} / {chain}"
        );
    }

    /// The control for the test above: the ClientHello really was flushed
    /// before the read happened. Without this, the refusal would also be
    /// produced by a driver that read first and never wrote at all - which
    /// would deadlock against a real server, because the server answers a
    /// flight it has not received with nothing.
    #[compio::test]
    async fn the_client_hello_is_flushed_before_the_first_read() {
        let mut peer = SilentPeer { written: 0 };
        let _ = handshake(&mut peer, client_connection()).await;
        assert!(
            peer.written > 0,
            "no bytes reached the peer, so the handshake read before it wrote"
        );
    }
}
