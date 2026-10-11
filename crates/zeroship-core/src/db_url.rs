//! Database URL classification shared by the runtime, worker, and ORM.
//! SQLite selectors resolve to filesystem paths. The ORM opener and runtime
//! guards use this parser so invalid or ephemeral selectors cannot disagree.

/// `true` iff `url` selects the SQLite (dev-tier) backend under the canonical
/// grammar: `sqlite:` / `sqlite://` / `file:` / a bare filesystem
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
    sqlite_file_path(url).is_some()
}

/// A SQLite path must name persistent storage, without SQLite URI options.
#[must_use]
pub fn valid_sqlite_file_path(path: &str) -> bool {
    !path.trim().is_empty()
        && !path.eq_ignore_ascii_case(":memory:")
        && !path.contains(['?', '#'])
        && !has_scheme(path)
}

fn has_scheme(path: &str) -> bool {
    !has_drive_prefix(path)
        && path.split_once(':').is_some_and(|(scheme, _)| {
            let mut chars = scheme.chars();
            matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
                && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
        })
}

/// A Windows drive-absolute path, `C:\...` or `C:/...`: one letter, a colon
/// and a separator. Its colon ends a drive, not a URL scheme, so the path is a
/// file location on every system that parses it, and the dev host's own
/// files are named this way on Windows.
fn has_drive_prefix(path: &str) -> bool {
    matches!(path.as_bytes(), [drive, b':', b'\\' | b'/', ..] if drive.is_ascii_alphabetic())
}

/// Resolve a SQLite selector to a filesystem path without opening it.
#[must_use]
pub fn sqlite_file_path(url: &str) -> Option<&str> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("postgres://") || lower.starts_with("postgresql://") {
        return None;
    }
    let path = ["sqlite://", "sqlite:", "file:"]
        .iter()
        .find(|prefix| lower.starts_with(**prefix))
        .map_or(trimmed, |prefix| &trimmed[prefix.len()..]);
    valid_sqlite_file_path(path).then_some(path)
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
        assert!(is_sqlite_url("/var/lib/zeroship/dev.sqlite"));
        assert!(is_sqlite_url("./relative/path.sqlite"));
    }

    /// A Windows drive-absolute path is a file, bare or behind a `sqlite:` or
    /// `file:` prefix, in either separator; it resolves to exactly the path
    /// written.
    /// A scheme of more than one letter is still a scheme, and a drive letter
    /// without a separator names no file.
    #[test]
    fn windows_drive_paths_are_sqlite_files() {
        for (url, path) in [
            (r"C:\Users\dev\project\.zeroship\dev.sqlite", r"C:\Users\dev\project\.zeroship\dev.sqlite"),
            (r"sqlite:C:\Users\dev\.zeroship\dev.sqlite", r"C:\Users\dev\.zeroship\dev.sqlite"),
            ("sqlite://D:/data/app.sqlite", "D:/data/app.sqlite"),
            ("file:e:/platform/metadata.sqlite", "e:/platform/metadata.sqlite"),
        ] {
            assert_eq!(super::sqlite_file_path(url), Some(path), "{url}");
        }
        assert!(!is_sqlite_url("ab:/x/app.sqlite"), "a two-letter scheme is a scheme");
        assert!(!is_sqlite_url("sqlite:C:dev.sqlite"), "a drive without a separator");
    }

    #[test]
    fn empty_and_unknown_scheme_are_not_sqlite() {
        assert!(!is_sqlite_url(""));
        assert!(!is_sqlite_url("   "));
        // mysql:// is an explicit unknown scheme — not a bare path, not sqlite.
        assert!(!is_sqlite_url("mysql://localhost/db"));
        assert!(!is_sqlite_url("redis://localhost:6379"));
    }

    #[test]
    fn sqlite_requires_a_file() {
        for url in [
            ":memory:",
            "sqlite::memory:",
            "SQLITE://:MEMORY:",
            "file::memory:",
            "file:db?mode=memory&cache=shared",
            "sqlite:file:db?mode=memory",
            "sqlite:",
            "sqlite://",
            "file:",
        ] {
            assert!(!is_sqlite_url(url), "{url}");
        }
    }
}
