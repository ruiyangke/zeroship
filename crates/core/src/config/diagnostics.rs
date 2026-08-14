//! The shared scanner behind "does this refusal name something an operator can
//! actually set".
//!
//! A startup refusal that names a variable the binary does not read is worse
//! than a generic one: it sends the operator to do something that cannot work.
//! Every binary pins that property the same way - derive the set of names it
//! really reads from its clap command plus its generated `SPECS`, drive the
//! real refusal, and require every environment-shaped token in the message to
//! be in the derived set.
//!
//! Only the token scanner is shared. The derived readable set is deliberately
//! NOT shared, because it must come from each binary's own declaration types;
//! a common list would be a second spelling that could be edited to agree with
//! a stale diagnostic, which is the failure these tests exist to catch.

/// The substrings of `text` that are shaped like an environment variable name.
///
/// A maximal run of `[A-Z0-9_]` at least four characters long and holding at
/// least one underscore. That shape admits `WORKER_KEY` and
/// `ZEROSHIP_AUTH_PLATFORM_ISSUER` while excluding ordinary prose and bare
/// acronyms such as `URL`, `JWKS` or `HMAC`.
///
/// Deliberately SHAPE-based rather than dictionary-based: a scanner that only
/// recognised names it already knew could not see the stale one.
#[must_use]
pub fn env_like_tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
        .filter(|token| token.len() >= 4 && token.contains('_'))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::env_like_tokens;

    #[test]
    fn the_scanner_sees_env_names_and_ignores_prose() {
        assert_eq!(
            env_like_tokens("ZEROSHIP_WORKER_KEY is required; set a strong value"),
            vec!["ZEROSHIP_WORKER_KEY".to_owned()]
        );

        // The three bare spellings this module's callers were caught printing.
        assert_eq!(
            env_like_tokens("STASH_SIGNING_KEY is too short (5 bytes)"),
            vec!["STASH_SIGNING_KEY".to_owned()]
        );

        // A label carrying both tiers yields the env name only; the flag is
        // lowercase and so is not env-shaped.
        assert_eq!(
            env_like_tokens("ZEROSHIP_PAIRWISE_SALT / --pairwise-salt-file is required"),
            vec!["ZEROSHIP_PAIRWISE_SALT".to_owned()]
        );

        // Prose, bare acronyms and underscore-free runs are not names.
        assert!(env_like_tokens("refusing to start; the JWKS URL is unset").is_empty());
        assert!(env_like_tokens("").is_empty());

        // Does NOT cover: a lowercase or dotted canonical spelling (`worker_key`,
        // `gateway.stash_signing_key`). Those are TOML/flag tiers, and a refusal
        // naming one is not the defect this scanner exists to catch.
    }
}
