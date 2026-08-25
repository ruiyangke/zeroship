//! libpq connection-service (`pg_service.conf`) support.
//!
//! A service names a section of an INI-ish file whose `key=value` lines supply
//! connection parameters, so a DSN can be as short as `service=prod` and the
//! host, port, database and user live in one place operators control.
//!
//! The rules below were MEASURED against libpq 16.14 rather than read off the
//! documentation:
//!
//! * An explicitly given parameter beats the service's value for the same key,
//!   REGARDLESS OF ORDER - `dbname=x service=s` and `service=s dbname=x` both
//!   connect to `x`. So precedence is by source, not by position, which is why
//!   the caller applies its own parameters first and this module's values only
//!   fill what is left.
//! * A service that is not defined is a hard ERROR, not a silent fallback:
//!   psql reports `definition of service "nosuch" not found`. This is the
//!   opposite of [`crate::passfile`], where an absent file is silent, and the
//!   difference is deliberate on libpq's part - naming a service you do not
//!   have is a mistake, whereas having no password file is ordinary.
//! * `#` comments and blank lines are ignored.
//! * A service may supply the password.

use std::fmt;
use std::path::{Path, PathBuf};

/// Why a service could not be turned into connection parameters.
#[derive(Debug)]
pub(crate) enum ServiceError {
    /// No service file exists in any of the searched locations.
    NoServiceFile { service: String },
    /// The file exists but defines no section with this name.
    Undefined { service: String, path: PathBuf },
    /// A line inside the selected section is not `key=value`.
    Syntax { path: PathBuf, line: usize },
    /// The file could not be read.
    Unreadable { path: PathBuf },
}

impl fmt::Display for ServiceError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoServiceFile { service } => write!(
                fmt,
                "definition of service `{service}` not found: no service file"
            ),
            Self::Undefined { service, path } => write!(
                fmt,
                "definition of service `{service}` not found in {}",
                path.display()
            ),
            Self::Syntax { path, line } => {
                write!(
                    fmt,
                    "syntax error in service file {}, line {line}",
                    path.display()
                )
            }
            Self::Unreadable { path } => {
                write!(fmt, "could not read service file {}", path.display())
            }
        }
    }
}

impl std::error::Error for ServiceError {}

/// Where the service file lives: `$PGSERVICEFILE`, else
/// `$HOME/.pg_service.conf`, else `$PGSYSCONFDIR/pg_service.conf`.
///
/// MEASURED: with `PGSERVICEFILE` unset and a `~/.pg_service.conf` in place,
/// libpq found the service; with neither, it reported the service undefined.
fn candidate_paths() -> Vec<PathBuf> {
    candidate_paths_from(
        std::env::var_os("PGSERVICEFILE"),
        std::env::var_os("HOME"),
        std::env::var_os("PGSYSCONFDIR"),
    )
}

/// The environment-free half of [`candidate_paths`], so the precedence order
/// can be tested without setting process-wide variables.
fn candidate_paths_from(
    service_file: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
    sysconfdir: Option<std::ffi::OsString>,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(explicit) = service_file.filter(|value| !value.is_empty()) {
        paths.push(PathBuf::from(explicit));
    }
    if let Some(home) = home.filter(|value| !value.is_empty()) {
        paths.push(PathBuf::from(home).join(".pg_service.conf"));
    }
    if let Some(sysconf) = sysconfdir.filter(|value| !value.is_empty()) {
        paths.push(PathBuf::from(sysconf).join("pg_service.conf"));
    }
    paths
}

/// The `key=value` pairs of one section, or `None` if the file has no such
/// section.
///
/// Returns `Err` only for a malformed line INSIDE the requested section: a
/// section this caller did not ask for is skipped without being validated,
/// which is what lets one file hold services for tools that understand keys
/// this driver does not.
fn section_of(
    contents: &str,
    service: &str,
    path: &Path,
) -> Result<Option<Vec<(String, String)>>, ServiceError> {
    let mut in_section = false;
    let mut pairs = Vec::new();

    for (index, raw) in contents.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            if in_section {
                // The next section begins, so the requested one is complete.
                return Ok(Some(pairs));
            }
            in_section = name.trim() == service;
            continue;
        }

        if !in_section {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            return Err(ServiceError::Syntax {
                path: path.to_path_buf(),
                line: index + 1,
            });
        };
        pairs.push((key.trim().to_owned(), value.trim().to_owned()));
    }

    Ok(if in_section { Some(pairs) } else { None })
}

/// Resolve `service` to its connection parameters.
///
/// Searches each candidate path in order and uses the FIRST file that exists,
/// matching libpq: a `PGSERVICEFILE` that exists but lacks the service is an
/// error rather than a reason to look in `~/.pg_service.conf`.
pub(crate) fn parameters(service: &str) -> Result<Vec<(String, String)>, ServiceError> {
    for path in candidate_paths() {
        if !path.is_file() {
            continue;
        }
        let contents = std::fs::read_to_string(&path)
            .map_err(|_| ServiceError::Unreadable { path: path.clone() })?;
        return match section_of(&contents, service, &path)? {
            Some(pairs) => Ok(pairs),
            None => Err(ServiceError::Undefined {
                service: service.to_owned(),
                path,
            }),
        };
    }

    Err(ServiceError::NoServiceFile {
        service: service.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(contents: &str, service: &str) -> Result<Option<Vec<(String, String)>>, ServiceError> {
        section_of(contents, service, Path::new("/test/pg_service.conf"))
    }

    fn pairs(contents: &str, service: &str) -> Vec<(String, String)> {
        parse(contents, service)
            .expect("well-formed")
            .expect("the section exists")
    }

    fn os(value: &str) -> Option<std::ffi::OsString> {
        Some(std::ffi::OsString::from(value))
    }

    #[test]
    fn the_service_file_is_searched_in_libpq_order() {
        let paths = candidate_paths_from(os("/explicit.conf"), os("/home/u"), os("/etc/pg"));
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/explicit.conf"),
                PathBuf::from("/home/u/.pg_service.conf"),
                PathBuf::from("/etc/pg/pg_service.conf"),
            ]
        );
    }

    /// An unset OR EMPTY variable contributes no candidate. Empty matters
    /// because an exported-but-blank `PGSERVICEFILE` would otherwise put the
    /// path `""` ahead of the home directory and suppress it.
    #[test]
    fn an_unset_or_empty_variable_contributes_no_candidate() {
        assert_eq!(
            candidate_paths_from(None, os("/home/u"), None),
            vec![PathBuf::from("/home/u/.pg_service.conf")]
        );
        assert_eq!(
            candidate_paths_from(os(""), os("/home/u"), None),
            vec![PathBuf::from("/home/u/.pg_service.conf")]
        );
        assert!(candidate_paths_from(None, None, None).is_empty());
    }

    #[test]
    fn a_section_yields_its_pairs() {
        let found = pairs("[prod]\nhost=db.example\nport=5432\n", "prod");
        assert_eq!(
            found,
            vec![
                ("host".to_owned(), "db.example".to_owned()),
                ("port".to_owned(), "5432".to_owned()),
            ]
        );
    }

    /// The control: a file that defines a DIFFERENT service must not answer
    /// for the one asked about, or every other test here passes vacuously.
    #[test]
    fn a_file_without_the_service_yields_nothing() {
        let found = parse("[other]\nhost=db.example\n", "prod").expect("well-formed");
        assert_eq!(found, None);
    }

    #[test]
    fn a_later_section_does_not_leak_into_an_earlier_one() {
        let found = pairs(
            "[prod]\nhost=prod.example\n[staging]\nhost=staging.example\n",
            "prod",
        );
        assert_eq!(found, vec![("host".to_owned(), "prod.example".to_owned())]);
    }

    #[test]
    fn a_section_after_another_is_still_found() {
        let found = pairs(
            "[staging]\nhost=staging.example\n[prod]\nhost=prod.example\n",
            "prod",
        );
        assert_eq!(found, vec![("host".to_owned(), "prod.example".to_owned())]);
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let found = pairs(
            "# leading\n\n[prod]\n# inside\nhost=db.example\n\n# trailing\n",
            "prod",
        );
        assert_eq!(found, vec![("host".to_owned(), "db.example".to_owned())]);
    }

    #[test]
    fn surrounding_whitespace_is_not_part_of_a_key_or_value() {
        let found = pairs("[prod]\n  host = db.example  \n", "prod");
        assert_eq!(found, vec![("host".to_owned(), "db.example".to_owned())]);
    }

    #[test]
    fn a_line_without_an_equals_sign_is_a_syntax_error_naming_its_line() {
        let error = parse("[prod]\nhost=db.example\nnonsense\n", "prod")
            .expect_err("a bare word is not a parameter");
        match error {
            ServiceError::Syntax { line, .. } => assert_eq!(line, 3),
            other => panic!("expected a syntax error, got {other:?}"),
        }
    }

    /// A malformed line in a section nobody asked for is not this connection's
    /// problem - one file can serve tools with different vocabularies.
    #[test]
    fn a_malformed_line_in_another_section_is_not_an_error() {
        let found = pairs("[other]\nnonsense\n[prod]\nhost=db.example\n", "prod");
        assert_eq!(found, vec![("host".to_owned(), "db.example".to_owned())]);
    }

    #[test]
    fn a_value_may_contain_an_equals_sign() {
        let found = pairs("[prod]\noptions=-c geqo=off\n", "prod");
        assert_eq!(
            found,
            vec![("options".to_owned(), "-c geqo=off".to_owned())]
        );
    }
}
