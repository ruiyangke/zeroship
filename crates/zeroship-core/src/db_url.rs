//! Database-DSN classification shared across the stack.
//!
//! The canonical backend-selection grammar lives in
//! `zeroship-plugin-db::backend_for_url` (it is the code that actually opens a
//! pool / SQLite backend). But two non-plugin-db call sites must classify a
//! DSN *without* taking a dependency on plugin-db (which itself depends on
//! `zeroship-runtime`, so the runtime cannot depend back on it):
//!
//! - `zeroship-runtime`'s `serve::start_server` — the SQLite dev-tier
//!   single-isolate clamp (SQLite-engine wiring design R3.4 fix 1): a
//!   `sqlite:`/`file:` DSN forces `num_workers → 1`.
//! - `zeroship-worker`'s startup — the hard-abort guard (fix 2): a worker is
//!   multi-replica by identity, so a SQLite DSN there is always a misconfig.
//!
//! Both crates depend on `zeroship-core`, so this is the shared home for the
//! classifier. plugin-db's `backend_for_url` / `is_sqlite_url` delegate here so
//! the grammar has exactly one source of truth (no drift between the opener and
//! the classifiers).

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
    if lower.starts_with("sqlite://")
        || lower.starts_with("sqlite:")
        || lower.starts_with("file:")
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

#[cfg(test)]
mod tests {
    use super::is_sqlite_url;

    #[test]
    fn postgres_is_not_sqlite() {
        assert!(!is_sqlite_url("postgres://localhost/dev"));
        assert!(!is_sqlite_url("postgresql://u:p@host:5432/db"));
        assert!(!is_sqlite_url("POSTGRES://Localhost/dev"));
    }

    #[test]
    fn sqlite_schemes_and_bare_paths_are_sqlite() {
        assert!(is_sqlite_url("sqlite:.zeroship/dev.sqlite"));
        assert!(is_sqlite_url("sqlite://./data/app.sqlite"));
        assert!(is_sqlite_url("file:./local.db"));
        assert!(is_sqlite_url(":memory:"));
        assert!(is_sqlite_url("/var/lib/zeroship/dev.sqlite"));
        assert!(is_sqlite_url("./relative/path.sqlite"));
    }

    #[test]
    fn empty_and_unknown_scheme_are_not_sqlite() {
        assert!(!is_sqlite_url(""));
        assert!(!is_sqlite_url("   "));
        // mysql:// is an explicit unknown scheme — not a bare path, not sqlite.
        assert!(!is_sqlite_url("mysql://localhost/db"));
        assert!(!is_sqlite_url("redis://localhost:6379"));
    }
}
