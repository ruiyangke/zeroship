//! Environment-variable and CLI boolean truthiness helpers.

/// Return true when environment variable `key` is exactly `expected`.
///
/// Web binaries use a `SetTrue` flag plus skipped env field so `--dev-insecure`
/// and `--trust-proxy` preserve exact `"1"` environment truthiness. Bootstrap
/// flags intentionally use [`env_is_truthy`] to also accept `"true"`, while the
/// auth binary uses clap's normal boolean env parsing.
#[must_use]
pub fn env_is_exact(key: &str, expected: &str) -> bool {
    std::env::var(key).is_ok_and(|value| value == expected)
}

/// Return true when environment variable `key` is `"1"` or case-insensitive `"true"`.
///
/// This intentionally differs from [`env_is_exact`] for bootstrap-style flags
/// that accept both common truthy spellings.
#[must_use]
pub fn env_is_truthy(key: &str) -> bool {
    std::env::var(key).is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

/// Parse a boolean flag value accepting `1/0/true/false/yes/no` case-insensitively.
///
/// This is the single boolean-env truthiness used by every `Option<bool>` flag
/// (e.g. `--dev-insecure[=…]`, `--trust-proxy[=…]`) so CLI presence can override
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
        let key = format!("ZEROSHIP_TEST_ENV_EXACT_{}", std::process::id());

        std::env::remove_var(&key);
        assert!(!env_is_exact(&key, "1"));

        std::env::set_var(&key, "true");
        assert!(!env_is_exact(&key, "1"));

        std::env::set_var(&key, "1");
        assert!(env_is_exact(&key, "1"));

        std::env::remove_var(&key);
    }

    #[test]
    fn env_is_truthy_accepts_one_and_true() {
        let key = format!("ZEROSHIP_TEST_ENV_TRUTHY_{}", std::process::id());

        std::env::remove_var(&key);
        assert!(!env_is_truthy(&key));

        std::env::set_var(&key, "1");
        assert!(env_is_truthy(&key));

        std::env::set_var(&key, "true");
        assert!(env_is_truthy(&key));

        std::env::set_var(&key, "TRUE");
        assert!(env_is_truthy(&key));

        std::env::set_var(&key, "yes");
        assert!(!env_is_truthy(&key));

        std::env::remove_var(&key);
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
