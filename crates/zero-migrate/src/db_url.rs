//! Vendored DSN classifier (`is_sqlite_url`).
//!
//! Copied byte-identically from the upstream db-url module so this crate can be
//! embedded as a lean library without a runtime dependency on the upstream core.
//! Nothing in THIS repository checks this copy against its upstream: the core it was
//! copied from is not here, so there is no second copy to compare against. An earlier
//! version of this note promised a `tests/core_id_parity.rs` guard "while both crates
//! coexist in-tree" - that condition is false where it was written, and no such test
//! has ever existed here.
//!
//! Unlike the id encoding, this classifier has no cross-repository guard either. That
//! is a HOLE, not a handoff.

/// `true` iff `url` selects the SQLite (dev-tier) backend under the canonical
/// grammar: `sqlite:` / `sqlite://` / `file:` / `:memory:` / a bare filesystem
/// path. Postgres (`postgres://` / `postgresql://`) is `false`; an empty URL is
/// `false` (no backend); an unknown explicit scheme (`scheme://…`) is `false`
/// (it is neither SQLite nor a bare path).
///
/// Matching is ASCII-case-insensitive on the scheme, mirroring
/// `backend_for_url`. A bare path with no recognised scheme is SQLite (the
/// dev-file selector), which is why an empty string must be special-cased to
/// `false` before the bare-path fallthrough.
#[must_use]
pub fn is_sqlite_url(url: &str) -> bool {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return false;
    }

    let lower = trimmed.to_ascii_lowercase();
    if lower == ":memory:" {
        return true;
    }
    if lower.starts_with("postgres://") || lower.starts_with("postgresql://") {
        return false;
    }
    if lower.starts_with("sqlite://") || lower.starts_with("sqlite:") || lower.starts_with("file:")
    {
        return true;
    }

    // An explicit but unrecognised scheme (`scheme://…` / `scheme:…`, where the
    // scheme is `[a-z][a-z0-9+.-]*`) is NOT a bare path and is NOT SQLite —
    // `backend_for_url` rejects it as unsupported. Anything else is a bare
    // filesystem path, which selects SQLite.
    let has_unknown_scheme = trimmed
        .split_once(':')
        .map(|(scheme, _)| {
            let mut chars = scheme.chars();
            matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
                && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
        })
        .unwrap_or(false);
    !has_unknown_scheme
}
