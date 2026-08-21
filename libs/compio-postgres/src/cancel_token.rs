// Ported from tokio-postgres (MIT/Apache-2.0). Copyright (c) 2016 Steven Fackler.

use crate::config::{SslMode, SslNegotiation};
use crate::tls::TlsConnect;
use crate::{Error, Socket, cancel_query, cancel_query_raw, client::SocketConfig, tls::MakeTlsConnect};
use compio::io::{AsyncRead, AsyncWrite};

/// The capability to request cancellation of in-progress queries on a
/// connection.
#[derive(Clone)]
pub struct CancelToken {
    pub(crate) socket_config: Option<SocketConfig>,
    pub(crate) ssl_mode: SslMode,
    pub(crate) ssl_negotiation: SslNegotiation,
    pub(crate) process_id: i32,
    pub(crate) secret_key: i32,
}

impl CancelToken {
    /// Attempts to cancel the in-progress query on the connection associated
    /// with this `CancelToken`.
    ///
    /// The server provides no information about whether a cancellation attempt was successful or not. An error will
    /// only be returned if the client was unable to connect to the database.
    ///
    /// Cancellation is inherently racy. There is no guarantee that the
    /// cancellation request will reach the server before the query terminates
    /// normally, or that the connection associated with this token is still
    /// active.
    pub async fn cancel_query<T>(&self, tls: T) -> Result<(), Error>
    where
        T: MakeTlsConnect<Socket>,
    {
        cancel_query::cancel_query(
            self.socket_config.clone(),
            self.ssl_mode,
            self.ssl_negotiation,
            tls,
            self.process_id,
            self.secret_key,
        )
        .await
    }

    /// Send cancellation and wait until the postmaster closes its dedicated
    /// connection after consuming the packet.
    ///
    /// Pool timeout recovery uses this stronger internal primitive before it
    /// allows the original backend to be reused. The public method preserves
    /// its established fire-and-forget contract.
    pub(crate) async fn cancel_query_confirmed<T>(&self, tls: T) -> Result<(), Error>
    where
        T: MakeTlsConnect<Socket>,
    {
        cancel_query::cancel_query_confirmed(
            self.socket_config.clone(),
            self.ssl_mode,
            self.ssl_negotiation,
            tls,
            self.process_id,
            self.secret_key,
        )
        .await
    }

    /// Like `cancel_query`, but uses a stream which is already connected to the server rather than opening a new
    /// connection itself.
    pub async fn cancel_query_raw<S, T>(&self, stream: S, tls: T) -> Result<(), Error>
    where
        S: AsyncRead + AsyncWrite + Unpin,
        T: TlsConnect<S>,
    {
        cancel_query_raw::cancel_query_raw(
            stream,
            self.ssl_mode,
            self.ssl_negotiation,
            tls,
            // `true`, deliberately, and NOT derived from `self.socket_config`.
            //
            // Here `has_hostname` asks whether the CONNECTOR the caller just
            // handed us has a name to validate against. Only the caller knows:
            // this method takes their stream and their `TlsConnect`, and
            // nothing says either has to match the address the original
            // session used. `true` means "attempt validation", which fails
            // closed - a connector with no name refuses the handshake.
            //
            // Deriving it from the token was tried and reverted. It reads as
            // more precise and is a downgrade: `Config::connect_raw` leaves
            // `socket_config` as `None` forever (`config.rs`), so every
            // caller-owned-stream client got `false`, and under the default
            // `sslmode=prefer` that skips TLS and puts the cancel key - a
            // bearer credential, see `cancel_query_raw` - on the wire in
            // cleartext. It did not even fix the Unix case it was written for:
            // `cancel_query_raw` still picks `Encryption::first_for(mode)`, so
            // Unix plus `require` fails either way, just with a different
            // message.
            //
            // Upstream tokio-postgres passes `true` here too.
            true,
            self.process_id,
            self.secret_key,
        )
        .await
    }
}
