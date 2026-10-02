//! **Test-only** helpers for downstream crates (benches, unit tests)
//! that need to synthesise [`Row`] / [`Statement`] / [`Column`] values
//! without a live Postgres connection.
//!
//! `#[doc(hidden)]` rather than feature-gated: gating it stops
//! `tests/serialized_loop.rs` from building under the plain test command, so
//! the serialized transport and the split refusal that routes onto it would go
//! uncovered. Compiling a doc-hidden module into release builds is the smaller
//! cost.
//!
//! TWO HALVES, AND ONLY ONE OF THEM IS FOR THIS CRATE.
//!
//! `SerializedSocket` / `connect_serialized` are used by this crate's own
//! `tests/serialized_loop.rs` and are the reason this module cannot be
//! feature-gated.
//!
//! The `*_for_test` synthesisers are used by NOTHING here. Their callers are
//! all in `plugin-db`'s benches - `benches/bench_row_to_json.rs` and
//! `benches/bench_first_row_or_null.rs`, which price row decoding and would
//! measure a network round trip instead if they had to fetch a real row. Both
//! call `row_for_test` and `column_for_test`.
//!
//! `statement_for_test` has no caller OUTSIDE this module, but it is NOT
//! dead: `row_for_test` builds its `Statement` with it (below), so deleting
//! it breaks both benches through their `row_for_test` call. A caller search
//! that cannot see the defining module cannot answer whether a function is
//! dead.
//!
//! `Row::new` is `pub(crate)`, so an external bench cannot build one. Both
//! places in this crate that could have used them deliberately do not, and
//! their reasons are worth knowing before reaching for one:
//!
//! * `tests/integration/raw_value_column_identity.rs` wants the claim to be about what a
//!   real server sends, because a fixture cannot be wrong about a wire format
//!   in the same direction the driver is.
//! * `row.rs`'s own test module needs a `DataRow` whose field count does NOT
//!   match its `RowDescription` - the arity-panic guard - and `row_for_test`
//!   refuses to build one.
//!
//! So: reach for a live row unless you are measuring, and never reach for
//! these to test decoding of malformed input, which they cannot express.
//!
//! ## Why a builder, not just `Row::new`
//!
//! `Row::new` takes a [`DataRowBody`] whose fields are private inside
//! `postgres-protocol`; the only public path to one is feeding raw
//! wire bytes through [`postgres_protocol::message::backend::Message::parse`].
//! [`Statement::unnamed`] and [`Column`]'s fields are also `pub(crate)`.
//! This module wraps both - callers hand us column descriptors + raw
//! binary values and get back a `Row` that behaves exactly like one
//! produced by a real query (same `RowIndex` paths, same
//! `column_to_json` branches).

use crate::buf_stream::SplitStream;
use crate::config::Host;
use crate::socket::{Socket, SocketReadHalf, SocketWriteHalf};
use crate::statement::{Column, Statement};
use crate::tls::NoTlsStream;
use crate::types::Type;
use crate::{Client, Config, Connection, NoTls};
use crate::{Error, Row};
use bytes::BytesMut;
use compio::buf::{BufResult, IoBuf, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite};
use postgres_protocol::message::backend::{DataRowBody, Message};

/// A port that is free on BOTH loopback addresses the multi-address tests
/// need.
///
/// Binding `127.0.0.1:0` and then reusing the kernel's ephemeral choice on
/// `127.0.0.2` is a race, not a guarantee: the two addresses have independent
/// port spaces, so a port free on one can be taken on the other. It almost
/// always works when the test runs alone and fails occasionally inside the
/// full `--lib` run, where other tests are churning sockets.
///
/// Probe the PAIR and retry instead of asserting the first guess.
#[cfg(test)]
pub(crate) fn paired_loopback_port() -> u16 {
    let mut last: Option<std::io::Error> = None;
    for _ in 0..64 {
        let first = match std::net::TcpListener::bind(("127.0.0.1", 0)) {
            Ok(listener) => listener,
            Err(error) => {
                last = Some(error);
                continue;
            }
        };
        let port = match first.local_addr() {
            Ok(addr) => addr.port(),
            Err(error) => {
                last = Some(error);
                continue;
            }
        };
        match std::net::TcpListener::bind(("127.0.0.2", port)) {
            Ok(second) => {
                // Release both so the caller can rebind them for real. The
                // window is small and the loop covers losing it.
                drop(second);
                drop(first);
                return port;
            }
            Err(error) => last = Some(error),
        }
    }
    panic!("no port free on both 127.0.0.1 and 127.0.0.2 after 64 attempts: {last:?}");
}

/// Construct a [`Column`] for tests. Mirrors the `pub(crate)` field
/// layout used internally; `table_oid` / `column_id` default to `None`
/// (the values Postgres sends for an ad-hoc expression with no
/// underlying table), and `type_modifier` defaults to `-1` (the
/// "no modifier present" sentinel `RowDescription` carries).
#[must_use]
pub fn column_for_test(name: impl Into<String>, ty: Type) -> Column {
    Column {
        name: name.into(),
        table_oid: None,
        column_id: None,
        type_modifier: -1,
        r#type: ty,
    }
}

/// Construct an unnamed [`Statement`] from a column list. Parameter
/// types default to empty - the bench harness only needs the column
/// metadata for `Row::columns()` / `Row::try_get` / `Row::raw_value`.
#[must_use]
pub fn statement_for_test(columns: Vec<Column>) -> Statement {
    Statement::unnamed(Vec::new(), columns)
}

/// Synthesise a [`Row`] from column descriptors and per-column raw
/// binary values (PostgreSQL binary wire format; `None` is SQL NULL).
///
/// Wire-format reminder: the values you pass here must match what
/// Postgres would send for the column's `Type` - e.g. `INT4` is a
/// 4-byte big-endian `i32`, `BOOL` is a single byte (0 or 1), `JSONB`
/// is a 1-byte version prefix (0x01) followed by UTF-8 JSON text,
/// `TIMESTAMPTZ` is an 8-byte BE `i64` of microseconds since
/// 2000-01-01 UTC.
///
/// # Errors
///
/// Returns the same `Error` variants as a normal `Row::new` call would
/// (parse error if the synthesised buffer is malformed). The function
/// panics on `usize -> i32 / u16 / u32` overflow because the test inputs
/// are bounded by what fits on a stack - overflow here would mean
/// something is deeply wrong with the test fixture.
pub fn row_for_test(columns: Vec<Column>, values: Vec<Option<Vec<u8>>>) -> Result<Row, Error> {
    assert_eq!(
        columns.len(),
        values.len(),
        "row_for_test: column / value count mismatch ({} vs {})",
        columns.len(),
        values.len(),
    );

    let statement = statement_for_test(columns);
    let body = build_data_row_body(&values);
    Row::new(statement, body)
}

/// Build a `DataRowBody` by writing a synthetic `DataRow` message
/// (tag 'D' + length + col_count + per-col [i32_len + bytes]) and
/// feeding it through `Message::parse`. This is the only way to
/// produce a `DataRowBody` from outside `postgres-protocol`.
fn build_data_row_body(values: &[Option<Vec<u8>>]) -> DataRowBody {
    // DataRow wire format:
    //   1 byte  : tag = b'D' (0x44)
    //   4 bytes : length (BE u32, includes the length field but not the tag)
    //   2 bytes : col_count (BE u16)
    //   per col : i32 BE length (-1 for NULL) + that many raw bytes
    let mut payload_len: usize = 2; // col_count
    for v in values {
        payload_len += 4; // i32 len
        if let Some(bytes) = v {
            payload_len += bytes.len();
        }
    }
    let total_len = 4 + payload_len; // length field includes itself
    let total_len_u32: u32 = total_len.try_into().expect("DataRow length fits in u32");
    let col_count_u16: u16 = values
        .len()
        .try_into()
        .expect("DataRow column count fits in u16");

    let mut buf = BytesMut::with_capacity(1 + total_len);
    buf.extend_from_slice(&[b'D']);
    buf.extend_from_slice(&total_len_u32.to_be_bytes());
    buf.extend_from_slice(&col_count_u16.to_be_bytes());
    for v in values {
        match v {
            None => buf.extend_from_slice(&(-1_i32).to_be_bytes()),
            Some(bytes) => {
                let len_i32: i32 = bytes
                    .len()
                    .try_into()
                    .expect("DataRow column length fits in i32");
                buf.extend_from_slice(&len_i32.to_be_bytes());
                buf.extend_from_slice(bytes);
            }
        }
    }

    match Message::parse(&mut buf).expect("synthetic DataRow parses") {
        Some(Message::DataRow(body)) => body,
        Some(_) => panic!("synthetic DataRow buffer parsed as a non-DataRow message"),
        None => panic!("synthetic DataRow buffer underflowed Message::parse"),
    }
}

// ---------------------------------------------------------------------------
// Reaching the serialized connection loop without TLS
// ---------------------------------------------------------------------------

/// A socket that refuses to split, forcing [`crate::Connection::run`] onto its
/// SERIALIZED loop.
///
/// `Connection::run` chooses its loop by whether the transport splits into
/// owned halves. Both transports this crate ships - a plain socket and the
/// rustls stream - split, so both take the multiplexed loop; only a custom
/// `TlsConnect` whose stream refuses to split reaches the serialized one. The
/// two loops are not equivalent - the serialized loop does not read while idle
/// and does not write a second request before the first is answered - so
/// behaviour proven on one is not thereby proven on the other.
///
/// This makes the serialized loop reachable over plaintext, so the suites that
/// exercise behaviour can cover it directly rather than inferring.
#[derive(Debug)]
pub struct SerializedSocket {
    inner: Socket,
    /// Set when `Connection::run` asks this socket to split and is refused.
    /// That refusal is what routes the connection onto the serialized loop, so
    /// observing it is the only honest evidence a test is really on that loop
    /// - a shared backend pid, a working query and a committed transaction are
    /// all equally true on the multiplexed one.
    split_refused: std::rc::Rc<std::cell::Cell<bool>>,
}

impl AsyncRead for SerializedSocket {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        self.inner.read(buf).await
    }
}

impl AsyncWrite for SerializedSocket {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        self.inner.write(buf).await
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush().await
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        self.inner.shutdown().await
    }
}

impl SplitStream for SerializedSocket {
    type ReadHalf = SocketReadHalf;
    type WriteHalf = SocketWriteHalf;

    /// Always refuses. This is the whole point of the type.
    fn try_into_split(self) -> Result<(Self::ReadHalf, Self::WriteHalf), Self> {
        self.split_refused.set(true);
        Err(self)
    }
}

/// Connect to `config`'s first TCP endpoint and drive it on the SERIALIZED
/// loop.
///
/// Plaintext only, deliberately: the transport here is as simple as possible,
/// so the only thing being varied is which loop runs.
///
/// The host may be a name or a numeric address, as it may for `connect`. The
/// address the socket reached is what the session records for cancellation.
pub async fn connect_serialized(
    config: &Config,
) -> Result<
    (
        Client,
        Connection<SerializedSocket, NoTlsStream>,
        std::rc::Rc<std::cell::Cell<bool>>,
    ),
    Error,
> {
    let host = match config.get_hosts().first() {
        Some(Host::Tcp(host)) => host.clone(),
        _ => {
            return Err(Error::config("connect_serialized needs a TCP host".into()));
        }
    };
    let port = config.get_ports().first().copied().unwrap_or(5432);
    connect_serialized_via((host.as_str(), port), &host, port, config).await
}

/// [`connect_serialized`], dialling the addresses given instead of resolving
/// `config`'s host. The dial tries them in order, and the session records the
/// one the socket reached.
#[allow(
    clippy::future_not_send,
    reason = "the serialized loop is driven on one compio thread, like `connect_serialized`"
)]
async fn connect_serialized_via(
    addresses: impl compio::net::ToSocketAddrsAsync,
    hostname: &str,
    port: u16,
    config: &Config,
) -> Result<
    (
        Client,
        Connection<SerializedSocket, NoTlsStream>,
        std::rc::Rc<std::cell::Cell<bool>>,
    ),
    Error,
> {
    let tcp = compio::net::TcpStream::connect(addresses)
        .await
        .map_err(Error::connect)?;
    // The address this socket actually reached, which is what `connect`
    // records too: it resolves a name and keeps the address it dialled. A
    // cancel must reach the same server, and re-resolving the name at cancel
    // time could pick another of its addresses.
    let dialled = tcp.peer_addr().map_err(Error::connect)?;
    let split_refused = std::rc::Rc::new(std::cell::Cell::new(false));
    let socket = SerializedSocket {
        inner: Socket::new_tcp(tcp),
        split_refused: std::rc::Rc::clone(&split_refused),
    };
    let (mut client, connection) = crate::connect_raw::connect_raw(
        socket,
        NoTls,
        crate::encryption::Encryption::Plaintext,
        true,
        config,
        None,
    )
    .await?;

    // `connect_raw` deliberately leaves the socket config unset - its own
    // comment says such a stream "can never issue a CancelToken". A cancel
    // opens a SECOND connection and needs somewhere to dial, so a harness that
    // omitted this would make every cancellation fail with "unknown host" and
    // look like a driver defect. Recorded here exactly as `connect` does.
    client.set_socket_config(crate::client::SocketConfig {
        addr: crate::client::Addr::tcp(dialled),
        hostname: Some(hostname.to_owned()),
        port,
        connect_timeout: config.get_connect_timeout().copied(),
        tcp_user_timeout: config.get_tcp_user_timeout().copied(),
        keepalive: None,
        require_peer: config.get_require_peer().map(str::to_owned),
        encryption: crate::encryption::Encryption::Plaintext,
        ssl_sni: config.get_ssl_sni(),
        ssl_cert_mode: config.get_ssl_cert_mode(),
        // Plaintext, so nothing is verified - the same value the connect path
        // records for an unencrypted session.
        server_verification: crate::tls::ServerVerification::None,
    });

    Ok((client, connection, split_refused))
}

#[cfg(test)]
mod tests {
    use super::{connect_serialized, connect_serialized_via, paired_loopback_port};
    use crate::{Config, NoTls};
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::time::Duration;

    const PROCESS_ID: i32 = 4_711;
    const SECRET_KEY: i32 = 1_234_567;

    fn backend_frame(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut frame = vec![tag];
        frame.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
        frame.extend_from_slice(body);
        frame
    }

    fn read_length_prefixed(stream: &mut std::net::TcpStream) -> Vec<u8> {
        let mut length = [0u8; 4];
        stream
            .read_exact(&mut length)
            .expect("read a packet length");
        let length = u32::from_be_bytes(length) as usize;
        assert!(
            (8..=10_000).contains(&length),
            "implausible packet length {length}"
        );
        let mut body = vec![0u8; length - 4];
        stream.read_exact(&mut body).expect("read a packet body");
        body
    }

    /// A scripted server on `listener`: it completes one startup, then
    /// accepts the cancel on the SAME listener and reports the packet it
    /// carried.
    fn scripted_cancel_server(
        listener: TcpListener,
    ) -> (
        std::sync::mpsc::Receiver<Vec<u8>>,
        std::thread::JoinHandle<()>,
    ) {
        let (cancel_seen, cancel_observed) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut session, _) = listener.accept().expect("accept the session");
            read_length_prefixed(&mut session);
            let mut key_data = PROCESS_ID.to_be_bytes().to_vec();
            key_data.extend_from_slice(&SECRET_KEY.to_be_bytes());
            let mut startup = backend_frame(b'R', &0u32.to_be_bytes());
            startup.extend_from_slice(&backend_frame(b'K', &key_data));
            startup.extend_from_slice(&backend_frame(b'Z', b"I"));
            session.write_all(&startup).expect("complete startup");

            let (mut cancel, _) = listener.accept().expect("accept the cancel");
            let request = read_length_prefixed(&mut cancel);
            cancel_seen.send(request).expect("report the cancel");
            drop(cancel);
            drop(session);
        });
        (cancel_observed, server)
    }

    /// Cancel through `client` and require the scripted server to receive
    /// the key it handed out. Nothing else listens on the server's port, so a
    /// cancel sent to any other address is refused and fails here.
    #[allow(
        clippy::future_not_send,
        reason = "driven by the test's own compio runtime"
    )]
    async fn assert_cancel_arrives(
        client: crate::Client,
        cancel_observed: &std::sync::mpsc::Receiver<Vec<u8>>,
    ) {
        client
            .cancel_token()
            .cancel_query(NoTls)
            .await
            .expect("deliver the CancelRequest");
        let request = cancel_observed
            .recv_timeout(Duration::from_secs(10))
            .expect("the cancel never reached the scripted server");

        let mut expected = 80_877_102u32.to_be_bytes().to_vec();
        expected.extend_from_slice(&PROCESS_ID.to_be_bytes());
        expected.extend_from_slice(&SECRET_KEY.to_be_bytes());
        assert_eq!(request, expected, "the cancel carried another key");
    }

    /// A listener on 127.0.0.1:P behind 127.0.0.2:P, which refuses.
    ///
    /// 127.0.0.2:P is held by a socket that is bound and never listens, so the
    /// port stays ours and a dial to it is refused. Linux routes all of
    /// 127.0.0.0/8 to the loopback, so both addresses exist on every Linux
    /// host, with no name that has to resolve to two of them.
    fn listener_behind_a_refusing_address() -> (TcpListener, socket2::Socket, [SocketAddr; 2]) {
        let mut refusals = Vec::new();
        for _ in 0..64 {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind 127.0.0.1");
            let reached = listener.local_addr().expect("scripted server address");
            let refusing = SocketAddr::from(([127, 0, 0, 2], reached.port()));
            let held = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)
                .expect("create the refusing socket");
            match held.bind(&refusing.into()) {
                Ok(()) => return (listener, held, [refusing, reached]),
                Err(error) => refusals.push(format!("{refusing}: {error}")),
            }
        }
        panic!("no port was free on both 127.0.0.1 and 127.0.0.2: {refusals:?}");
    }

    /// A dial that fails over records the address it REACHED, not the first
    /// one it was given: its cancel must reach the server it is connected to.
    ///
    /// The dial is handed [127.0.0.2:P, 127.0.0.1:P]; the first refuses and
    /// the scripted server listens on the second. A session that recorded the
    /// first address sends its cancel to 127.0.0.2:P, which refuses it.
    #[compio::test]
    async fn a_failed_over_dial_records_the_address_it_reached() {
        let (listener, _held, [refusing, reached]) = listener_behind_a_refusing_address();
        assert!(
            std::net::TcpStream::connect_timeout(&refusing, Duration::from_secs(2)).is_err(),
            "the first address accepted a connection, so the dial would not fail over"
        );
        let (cancel_observed, server) = scripted_cancel_server(listener);

        let config: Config = format!("postgres://u@127.0.0.1:{}/d", reached.port())
            .parse()
            .expect("parse the scripted server's DSN");
        let (client, connection, _) = connect_serialized_via(
            &[refusing, reached][..],
            "127.0.0.1",
            reached.port(),
            &config,
        )
        .await
        .expect("the dial did not fail over to the listening address");
        let driver = compio::runtime::spawn(async move { connection.run().await });

        assert_cancel_arrives(client, &cancel_observed).await;
        let _ = driver.await;
        server.join().expect("the scripted server panicked");
    }

    /// A named host is accepted: `connect_serialized` resolves it and its
    /// session cancels through the address it reached.
    #[compio::test]
    async fn a_named_host_connects_and_its_cancel_reaches_it() {
        use compio::net::ToSocketAddrsAsync;

        let first = ("localhost", 0)
            .to_socket_addrs_async()
            .await
            .expect("resolve localhost")
            .next()
            .expect("localhost resolved to no address");
        let listener = TcpListener::bind(first).expect("bind the address localhost resolves to");
        let port = listener
            .local_addr()
            .expect("scripted server address")
            .port();
        let (cancel_observed, server) = scripted_cancel_server(listener);

        let config: Config = format!("postgres://u@localhost:{port}/d")
            .parse()
            .expect("parse a named-host DSN");
        let (client, connection, _) = connect_serialized(&config)
            .await
            .expect("connect_serialized refused a named host");
        let driver = compio::runtime::spawn(async move { connection.run().await });

        assert_cancel_arrives(client, &cancel_observed).await;
        let _ = driver.await;
        server.join().expect("the scripted server panicked");
    }

    #[test]
    fn paired_loopback_port_is_bindable_on_both_addresses() {
        let port = paired_loopback_port();
        let first = std::net::TcpListener::bind(("127.0.0.1", port))
            .expect("bind returned port on 127.0.0.1");
        let second = std::net::TcpListener::bind(("127.0.0.2", port))
            .expect("bind returned port on 127.0.0.2 while 127.0.0.1 is held");

        assert_eq!(
            first.local_addr().expect("read first bind address").port(),
            port
        );
        assert_eq!(
            second
                .local_addr()
                .expect("read second bind address")
                .port(),
            port
        );
    }
}
