//! Byte-spellings that MORE THAN ONE shipping vendor agrees on, with exactly one
//! physical home.
//!
//! # The invariant, and how the crate split weakened it
//!
//! These two functions used to be `pub(in crate::render::backends)`: physically
//! unreachable from the engine, so a core caller that wanted to spell `"x"` had to
//! pick a named door in `render::dml` and put a vendor on the record. That
//! visibility WAS the detector, and the reason is measured in
//! `zero_migrate::render::backends`'s header — two of the three vendors agree on the
//! ANSI spelling, so an unrouted emission produces correct bytes and no assertion
//! about emitted SQL can see the missing routing.
//!
//! Across a crate boundary `pub(in …)` cannot express "these three crates and no
//! other". The vendor crates must reach these, so they are `pub`, and the engine can
//! now name them too. THE COMPILER NO LONGER ENFORCES THE RULE.
//!
//! It is replaced, not dropped, by a textual census —
//! `tests/dialect_matrix/core_does_not_spell_a_vendors_bytes.rs` — which walks every
//! crate `src` root and asserts that `zero-migrate` names neither function. That is
//! strictly weaker than a visibility error (it can be deleted; a privacy violation
//! cannot) and it is recorded here as a DOWNGRADE rather than presented as an equal
//! substitute.

/// The ANSI double-quote identifier spelling: double every embedded `"`, wrap the
/// result in `"`. THE single physical home of that byte-logic.
///
/// Two of the three shipping vendors happen to agree on this spelling, which is
/// exactly why the engine must not reach it un-named. An engine caller that spells
/// these bytes itself is spelling them FOR A VENDOR IT NEVER NAMED, and no assertion
/// about emitted SQL can see the mistake while the two vendors agree — the bytes are
/// right, the routing is absent. The engine's doors are
/// `crate::dml::escape_quote_ident_for_dialect` (to EMIT for a named dialect) and
/// `crate::dml::pg_canonical_ident` (for the PG-shaped normal form); both resolve
/// back here through the registry, so the bytes are unchanged and the vendor is on
/// the record.
///
/// This is also why the two `quote_ident` impls that use it call it DIRECTLY rather
/// than through the `*_for_dialect` seam their sibling methods use: they ARE the
/// dialect's `quote_ident`, so routing through the dispatch would recurse.
#[must_use]
pub fn ansi_double_quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Canonical (padded, non-URL-safe) base64 — the wire form the two vendors that
/// carry binary through TEXT both decode.
///
/// Same reasoning as [`ansi_double_quote_ident`]: two of the three shipping vendors
/// agree on the ENCODING, so the shared spelling lives here and each vendor still
/// names its own DECODER. The alphabet is not an engine decision.
#[must_use]
pub fn base64_standard(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
