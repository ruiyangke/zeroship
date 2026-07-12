//! Leaf-crate error types for the schema layer.
//!
//! `zero-migrate-schema` is a leaf crate (deps: `serde_json` + `sha2`, plus
//! `compio-postgres` behind the `introspect` feature). It therefore CANNOT
//! depend on a data plane's `DbError`, which is
//! built on a runtime `OpError` (and would drag v8/runtime
//! into the leaf). Instead the two fallible surfaces this crate exposes —
//! live introspection and the mask-sentinel codec — return small,
//! self-contained error types. A data-plane consumer maps them back into its
//! own error at the call boundary via `From` impls, so the wire shape and
//! SQLSTATE-derived `.code` the SDK sees stay byte-identical to before the extraction.

/// Error from the live-schema introspection helpers
/// ([`crate::diff::read_live_schema`], [`crate::diff::estimate_row_count`]).
///
/// Carries the per-call-site context phrase (e.g. `"read columns failed"`)
/// plus the underlying `compio_postgres::Error`. plugin-db's
/// `From<SchemaError> for DbError` re-creates the exact
/// `coded_sql("diff: <context>", e)` shape — the SQLSTATE classification
/// is preserved because the raw driver error is carried through, and the
/// `"diff: "` module prefix is re-attached at the boundary.
///
/// Gated behind the `introspect` feature: it names `compio_postgres::Error`,
/// so it rides the introspection profile with the PG driver. The write/diff
/// profile (the migration engine's consumer path) never sees it.
#[cfg(feature = "introspect")]
#[derive(Debug)]
pub struct SchemaError {
    /// The module-local context phrase (no `"diff: "` prefix — plugin-db
    /// adds that when mapping to `DbError` so the operator-facing message
    /// is `"diff: <context>: <driver-msg>"`, identical to the pre-extraction
    /// `crate::diff::coded_sql` shape).
    pub context: String,
    /// The underlying driver error — carried verbatim so the SQLSTATE-driven
    /// classification stays the single source of truth.
    pub source: compio_postgres::Error,
}

#[cfg(feature = "introspect")]
impl SchemaError {
    /// Wrap a `compio_postgres::Error` with a context phrase.
    #[must_use]
    pub fn new(context: impl Into<String>, source: compio_postgres::Error) -> Self {
        Self {
            context: context.into(),
            source,
        }
    }
}

#[cfg(feature = "introspect")]
impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.context, self.source)
    }
}

#[cfg(feature = "introspect")]
impl std::error::Error for SchemaError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Error from parsing a `zero-migrate:mask:` sentinel string
/// ([`crate::mask_codec::parse_mask_sentinel`]).
///
/// Carries the human-readable rejection message. plugin-db's
/// `From<MaskSentinelError> for DbError` re-creates the exact
/// `DbError::internal("mask_sentinel_malformed: …")` the SDK round-trips,
/// so the `mask_sentinel_malformed` code-discriminator is preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskSentinelError {
    /// The full rejection message, including the `mask_sentinel_malformed: `
    /// prefix the introspection layer + SDK contract expect.
    pub message: String,
}

impl MaskSentinelError {
    /// Construct from a fully-formed message (caller includes the
    /// `mask_sentinel_malformed: ` prefix).
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Borrow the message body.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for MaskSentinelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for MaskSentinelError {}
