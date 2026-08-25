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

// THE PATH COMES FROM THE CALLER. libpq resolves an unset service file from
// `$PGSERVICEFILE`, then `~/.pg_service.conf`, then
// `$PGSYSCONFDIR/pg_service.conf`. This crate resolves none of them: it is a
// standalone, publishable driver, and a published library takes resolved
// options from its caller rather than reading process configuration. The
// workspace enforces that in `crates/core/tests/config_env_access_gate.rs`
// against a deliberately empty exemption list.
//
// So naming a `service` also means naming the file it lives in, through
// `Config::service_file`. An application that wants libpq's search order
// performs it and passes the winner.

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

/// Resolve `service` to its connection parameters, out of `path`.
///
/// A service the file does not define is an ERROR rather than a silent
/// fallback - measured against libpq, which reports `definition of service
/// "nosuch" not found`. Naming a service you do not have is a mistake, and
/// connecting somewhere else instead would hide it.
pub(crate) fn parameters(
    service: &str,
    path: &Path,
) -> Result<Vec<(String, String)>, ServiceError> {
    let contents = std::fs::read_to_string(path).map_err(|_| ServiceError::Unreadable {
        path: path.to_path_buf(),
    })?;

    match section_of(&contents, service, path)? {
        Some(pairs) => Ok(pairs),
        None => Err(ServiceError::Undefined {
            service: service.to_owned(),
            path: path.to_path_buf(),
        }),
    }
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
