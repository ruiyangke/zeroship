//! **Test-only** helpers for downstream crates (benches, unit tests)
//! that need to synthesise [`Row`] / [`Statement`] / [`Column`] values
//! without a live Postgres connection.
//!
//! `#[doc(hidden)]` rather than feature-gated. It WAS behind a `test-utils`
//! feature, and that flag quietly split the suite: without it
//! `tests/serialized_loop.rs` did not build, so the crate's own documented
//! test command ran 24 fewer tests and covered neither the serialized
//! transport nor the split refusal that routes onto it. Compiling a
//! doc-hidden module into release builds is the smaller cost.
//!
//! TWO HALVES, AND ONLY ONE OF THEM IS FOR THIS CRATE.
//!
//! `SerializedSocket` / `connect_serialized` are used by this crate's own
//! `tests/serialized_loop.rs` (24 tests) and are the reason this module cannot
//! be feature-gated: gating it stopped that target building under the plain
//! test command.
//!
//! The `*_for_test` synthesisers are used by NOTHING here. Their callers are
//! all in `plugin-db`, and there are now TWO of them - re-counted 2026-09-02
//! by grepping CALL sites (`name(`) rather than mentions, because every hit
//! inside this crate is a doc comment explaining why the code beside it uses
//! a real row instead:
//!
//! * `benches/bench_row_to_json.rs` and `benches/bench_first_row_or_null.rs`,
//!   which price row decoding and would measure a network round trip instead
//!   if they had to fetch a real row. Both call `row_for_test` and
//!   `column_for_test`.
//!
//! The third caller this note named on 2026-08-25 - `src/audit.rs`'s
//! `#[cfg(test)]` module - is GONE; no such file exists in `plugin-db` now.
//! So the count has been wrong in both directions: two when there were three,
//! and then three when there were two.
//!
//! `statement_for_test` has ZERO call sites anywhere in the workspace. It is
//! public API of a publishable library, built for those same benches, so it is
//! left rather than deleted on one grep - but nothing in the tree justifies it,
//! which is worth knowing before the next reader assumes it is load-bearing.
//!
//! Keeping them was re-ruled on 2026-08-25. The alternative considered was
//! having the benches fetch one real row before the timed loop, which would
//! put a network round trip inside a decode benchmark. That argument stands on
//! the benches alone now; the audit-decoding leg it also rested on went with
//! the file.
//!
//! `Row::new` is `pub(crate)`, so an external bench cannot build one. Both
//! places in this crate that could have used them deliberately do not, and
//! their reasons are worth knowing before reaching for one:
//!
//! * `tests/suite/raw_value_column_identity.rs` wants the claim to be about what a
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
/// full `--lib` run, where several hundred other tests are churning sockets.
/// Measured 2026-08-28: the suite went red on `bind TLS replication probe` in
/// a 519-test run while the same test passed 2 of 2 in isolation on both
/// servers.
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
/// 2000-01-01 UTC. See `crates/plugin-db/src/v8_bridge.rs::column_to_json`
/// for the conversion table.
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
/// owned halves. A plain socket does, and takes the multiplexed loop; a TLS
/// stream cannot, because rustls keeps shared session state, so every TLS
/// connection takes the serialized one. The two loops are not equivalent -
/// the serialized loop does not read while idle and does not write a second
/// request before the first is answered - so behaviour proven over plaintext
/// is not thereby proven over TLS.
///
/// This makes that loop reachable WITHOUT TLS, so the suites that exercise
/// behaviour can cover it directly rather than inferring. Before this existed
/// the only way in was a TLS server, which is why the two loops diverged with
/// nothing going red.
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
/// loop, the one every TLS connection uses.
///
/// Plaintext only, deliberately: the point is to exercise that loop without
/// needing a TLS server, so the transport here is as simple as possible and
/// the only thing being varied is which loop runs.
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
    let tcp = compio::net::TcpStream::connect((host.as_str(), port))
        .await
        .map_err(Error::io)?;
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
        addr: crate::client::Addr::Tcp {
            ip: host
                .parse()
                .map_err(|_| Error::config("connect_serialized needs a numeric TCP host".into()))?,
            // `host` parses as an `IpAddr`, which cannot carry a zone.
            scope_id: 0,
        },
        hostname: Some(host.clone()),
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
    use super::paired_loopback_port;

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
