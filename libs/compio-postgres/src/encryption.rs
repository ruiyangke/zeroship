//! Which transport a connection attempt uses.
//!
//! This is a LEAF on purpose. `Encryption` is shared vocabulary: five modules
//! name it - `connect`, `connect_tls`, `maybe_tls_stream`, `cancel_token` and
//! `cancel_query_raw`. Keeping it here lets `maybe_tls_stream` name the answer
//! without depending on `connect_tls`, while `connect_tls` depends on
//! `maybe_tls_stream` to build the stream; those two edges would otherwise form
//! a two-module cycle.
//!
//! A type that answers "what did we end up on" belongs beside neither the code
//! that decides it nor the code that carries it. Nothing here may depend on
//! anything but `config`.

use crate::config::SslMode;

/// Which transport a single connection attempt should use.
///
/// libpq's `current_enc_method`. It is a decision, not a preference: by the
/// time it reaches `negotiate_tls` the mode has already been consulted.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum Encryption {
    /// Send the startup packet in the clear, with no `SSLRequest` at all.
    Plaintext,
    /// Ask for TLS and hand the startup packet to the encrypted stream.
    Tls,
}

impl Encryption {
    /// The transport this mode offers first.
    ///
    /// libpq's `select_next_encryption_method`, whose whole content is this
    /// ordering swap: `allow` offers plaintext first, every other mode offers
    /// TLS first (and `disable` has only plaintext to offer).
    pub(crate) const fn first_for(mode: SslMode) -> Encryption {
        match mode {
            SslMode::Disable | SslMode::Allow => Encryption::Plaintext,
            SslMode::Prefer | SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => {
                Encryption::Tls
            }
        }
    }
}
