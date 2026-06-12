//! `compio-s3` — a small, bespoke, compio-native S3 client.
//!
//! Built in the spirit of `compio-postgres` / `compio-redis`: explicit
//! modules, explicit errors, bounded responses, and **no SDK dependency**.
//! `aws-sdk-s3` / `rusoto` / `aws-sigv4` are banned (they pull `tokio`); HTTP
//! is `cyper` 0.8 and `SigV4` is hand-rolled.
//!
//! Module map (proposal §1):
//!
//! - [`config`] — `S3Config`, `s3://…` URL parsing, addressing style,
//!   provider/checksum/SSE profiles.
//! - [`credentials`] — static access-key/secret/session-token (no chain).
//! - [`clock`] — injectable UTC clock + `SigV4` date formatting.
//! - [`signer`] — hand-rolled `SigV4` canonical request / authorization header.
//! - [`client`] — `S3Client` over a request-scoped `cyper::Client`.
//! - [`list_xml`] — `ListObjectsV2` XML parsing (quick-xml).
//! - [`error`] — typed errors + retryability.

// `future_not_send`: by design. `cyper::Client` is `!Send` (its connector
// wraps I/O in `SendWrapper`) and the whole crate runs on a single compio
// thread per the request-scoped-client discipline. Our futures are
// deliberately `!Send`; this nursery lint does not apply.
#![allow(clippy::future_not_send)]
// `missing_errors_doc`: every fallible method funnels through the single,
// documented `S3Error` taxonomy in `error.rs` (mapping HTTP status / transport
// / cap / integrity). Per-method `# Errors` stanzas would restate that one
// contract verbatim across ~20 methods without adding information.
#![allow(clippy::missing_errors_doc)]
// Style preferences that fight readability of the explicit match/if-let flow
// used throughout the request builders.
#![allow(clippy::option_if_let_else, clippy::needless_pass_by_value)]

pub mod client;
pub mod clock;
pub mod config;
pub mod credentials;
pub mod error;
pub mod list_xml;
pub mod signer;

pub use client::{
    ListEntry, ListPage, ObjectMeta, PartETag, PutOptions, PutResult, S3Client, UploadId,
};
pub use clock::{Clock, FixedClock, SigningTime, SystemClock};
pub use config::{AddressingStyle, ChecksumMode, Provider, S3Config, SseMode};
pub use credentials::S3Credentials;
pub use error::{S3Error, S3Result};
