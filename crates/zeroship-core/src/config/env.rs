//! The one module in the workspace that touches the process environment.
//!
//! Everything above it presents a typed key and a consumer token; the three
//! functions here are the only place a `&str` name reaches `std::env`. That is
//! what makes the claim "every first-party environment read is enumerable" a
//! structural property rather than a convention: the source gate in
//! `crates/core/tests/config_env_access_gate.rs` exempts this file BY PATH and
//! nothing else, so a read anywhere else fails the build.
//!
//! The truthiness helpers that used to live here took a `&str` name and read
//! the environment themselves, which made them a second, unattributable read
//! surface of exactly the kind Section 4.5 of
//! `docs/proposals/2026-08-11-config-name-alignment.md` names. They are now
//! PURE: they classify a value somebody else already read through a typed key.

use std::ffi::OsString;

/// The sole raw string read. Callers must present a typed key.
#[allow(clippy::disallowed_methods)]
pub(crate) fn raw_var(key: &str) -> Result<Option<String>, ()> {
    match std::env::var(key) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(()),
    }
}

/// The sole raw `OsString` read. Callers must present a typed key.
#[allow(clippy::disallowed_methods)]
pub(crate) fn raw_var_os(key: &str) -> Option<OsString> {
    std::env::var_os(key)
}

/// The sole whole-environment snapshot.
///
/// Non-Unicode entries are dropped rather than lossily converted: the one
/// caller forwards this to creator app code as a `HashMap<String, String>`,
/// and inventing a replacement character for a name or value it will later use
/// as a lookup key would be worse than omitting it.
#[allow(clippy::disallowed_methods)]
pub(crate) fn raw_vars() -> Vec<(String, String)> {
    std::env::vars().collect()
}

/// Return true when an already-read value is exactly `expected`.
///
/// Some bootstrap flags preserve exact `"1"` truthiness, while others
/// intentionally use [`env_is_truthy`] to also accept `"true"`.
#[must_use]
pub fn env_is_exact(value: Option<&str>, expected: &str) -> bool {
    value == Some(expected)
}

/// Return true when an already-read value is `"1"` or case-insensitive `"true"`.
///
/// This intentionally differs from [`env_is_exact`] for bootstrap-style flags
/// that accept both common truthy spellings.
#[must_use]
pub fn env_is_truthy(value: Option<&str>) -> bool {
    value.is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

/// Parse a boolean flag value accepting `1/0/true/false/yes/no` case-insensitively.
///
/// This is the single boolean-env truthiness used by every `Option<bool>` flag
/// (for example, `--trust-proxy[=...]`) so CLI presence can override
/// a stray env value with proper `CLI > env` precedence. It is used as a clap
/// `value_parser`, hence the [`String`] error type.
///
/// # Errors
///
/// Returns an explanatory message when `s` is not a recognised boolean spelling.
pub fn parse_bool_flag(s: &str) -> Result<bool, String> {
    match s.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => Ok(true),
        "0" | "false" | "no" => Ok(false),
        other => Err(format!(
            "invalid boolean {other:?}; expected one of 1, 0, true, false, yes, no"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{env_is_exact, env_is_truthy, parse_bool_flag};

    #[test]
    fn env_is_exact_matches_only_expected_value() {
        assert!(!env_is_exact(None, "1"));
        assert!(!env_is_exact(Some("true"), "1"));
        assert!(!env_is_exact(Some(""), "1"));
        assert!(env_is_exact(Some("1"), "1"));
    }

    #[test]
    fn env_is_truthy_accepts_one_and_true() {
        assert!(!env_is_truthy(None));
        assert!(env_is_truthy(Some("1")));
        assert!(env_is_truthy(Some("true")));
        assert!(env_is_truthy(Some("TRUE")));
        assert!(!env_is_truthy(Some("yes")));
        assert!(!env_is_truthy(Some("")));
    }

    #[test]
    fn parse_bool_flag_accepts_known_truthy_and_falsy() {
        for truthy in ["1", "true", "TRUE", "yes", "Yes", "YES", "True"] {
            assert_eq!(parse_bool_flag(truthy), Ok(true), "{truthy:?} should be true");
        }
        for falsy in ["0", "false", "FALSE", "no", "No", "NO", "False"] {
            assert_eq!(parse_bool_flag(falsy), Ok(false), "{falsy:?} should be false");
        }
    }

    #[test]
    fn parse_bool_flag_rejects_unknown() {
        assert!(parse_bool_flag("maybe").is_err());
    }
}
