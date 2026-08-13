//! The vocabulary of `zeroship.device_grants`, defined once.
//!
//! Two RFC 8628 flows share that one table, and the `provider` column is what
//! keeps them apart:
//!
//! * [`OP_PROVIDER`] - the auth service's own device grant. The approved row is
//!   redeemed at the OP's `/oauth2/token` for an OIDC access token, so the row
//!   carries `client_id`, `sid` and `auth_credential_version`.
//! * [`PLATFORM_PROVIDER`] - the control plane's deploy-token grant, the one
//!   `zeroship login` drives. The approved row is redeemed at control's
//!   `/api/device/token` for a platform access token scoped to the principal's
//!   `zeroship.principal_grants`.
//!
//! Both spellings used to be private constants in the crate that wrote them
//! (`crates/auth/src/oidc/device_token.rs` and
//! `crates/control/src/device_handlers.rs`), which is how the two flows drifted:
//! control wrote `provider = 'platform'` rows and the only page that could
//! approve anything filtered on `provider = 'op'`, so the code the CLI printed
//! was invisible to the page the CLI told the human to open. The producer and
//! the consumer now read the same constant.
//!
//! The user code shares a definition for the same reason. Control minted an
//! 8-character `XXXX-XXXX` code while the auth service's `/device` page
//! validated the OP's 12-character `XXXX-XXXX-XXXX` shape and rejected anything
//! else before it ever reached the database, so every control-minted code was
//! refused as "invalid or expired" on sight.

/// `zeroship.device_grants.provider` for the auth service's own OP grant.
pub const OP_PROVIDER: &str = "op";

/// `zeroship.device_grants.provider` for control's platform deploy-token grant.
pub const PLATFORM_PROVIDER: &str = "platform";

/// Path the OP mounts its protocol endpoints under, relative to the auth
/// service's public URL.
///
/// The auth service builds its issuer as `{public_url}{OP_PATH_PREFIX}`, so a
/// peer holding only the issuer recovers the public URL by stripping this
/// suffix - which is how control turns a configured platform issuer into the
/// `/device` page's absolute URL without a second configured hostname.
pub const OP_PATH_PREFIX: &str = "/oauth2";

/// User-code alphabet: no vowels (no accidental words) and no characters that
/// collide when read aloud or typed from a screen (`AEIOU`, `0/O`, `1/I/L`,
/// `2/Z`, `5/S`, `U/V`).
pub const USER_CODE_ALPHABET: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";

const USER_CODE_GROUP_LEN: usize = 4;
const USER_CODE_GROUPS: usize = 3;

/// Characters in a user code, excluding the group separators.
pub const USER_CODE_CHAR_LEN: usize = USER_CODE_GROUP_LEN * USER_CODE_GROUPS;

/// Length of a formatted user code, `XXXX-XXXX-XXXX`.
pub const USER_CODE_FORMATTED_LEN: usize = USER_CODE_CHAR_LEN + (USER_CODE_GROUPS - 1);

/// Public URL of the auth service that advertises `issuer`.
///
/// Returns `None` when `issuer` does not end in [`OP_PATH_PREFIX`], because
/// then it was not built by this platform's OP and no `/device` page can be
/// derived from it.
#[must_use]
pub fn op_public_url(issuer: &str) -> Option<&str> {
    let trimmed = issuer.trim_end_matches('/');
    let base = trimmed.strip_suffix(OP_PATH_PREFIX)?;
    let base = base.trim_end_matches('/');
    (!base.is_empty()).then_some(base)
}

/// Format `chars` as `XXXX-XXXX-XXXX`.
fn group(chars: &str) -> String {
    let mut formatted = String::with_capacity(USER_CODE_FORMATTED_LEN);
    for (idx, ch) in chars.chars().enumerate() {
        if idx > 0 && idx % USER_CODE_GROUP_LEN == 0 {
            formatted.push('-');
        }
        formatted.push(ch);
    }
    formatted
}

/// Draw a fresh user code from `rng`.
///
/// Takes the generator so both call sites keep their own (`OsRng` in control,
/// `thread_rng` in the auth service) rather than this module choosing for them.
pub fn generate_user_code<R: rand::Rng + ?Sized>(rng: &mut R) -> String {
    let mut chars = String::with_capacity(USER_CODE_CHAR_LEN);
    for _ in 0..USER_CODE_CHAR_LEN {
        let idx = rng.gen_range(0..USER_CODE_ALPHABET.len());
        chars.push(char::from(USER_CODE_ALPHABET[idx]));
    }
    group(&chars)
}

/// Canonical `XXXX-XXXX-XXXX` form of a code a human typed.
///
/// Whitespace and separators are dropped and letters upper-cased. Input that is
/// not a user code at all is returned upper-cased and whitespace-stripped so the
/// caller can echo it back into the form field; [`valid_user_code`] is what
/// decides whether it is worth a database round trip.
#[must_use]
pub fn normalize_user_code(value: &str) -> String {
    let chars: String = value
        .trim()
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace() && *ch != '-')
        .map(|ch| ch.to_ascii_uppercase())
        .collect();

    if chars.len() != USER_CODE_CHAR_LEN
        || !chars
            .as_bytes()
            .iter()
            .all(|ch| USER_CODE_ALPHABET.contains(ch))
    {
        return value
            .trim()
            .chars()
            .filter(|ch| !ch.is_ascii_whitespace())
            .map(|ch| ch.to_ascii_uppercase())
            .collect();
    }

    group(&chars)
}

/// Whether `value` normalizes to a well-formed user code.
#[must_use]
pub fn valid_user_code(value: &str) -> bool {
    let code = normalize_user_code(value);
    let bytes = code.as_bytes();
    bytes.len() == USER_CODE_FORMATTED_LEN
        && bytes.iter().enumerate().all(|(idx, ch)| {
            if (idx + 1) % (USER_CODE_GROUP_LEN + 1) == 0 {
                *ch == b'-'
            } else {
                USER_CODE_ALPHABET.contains(ch)
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_code_is_accepted_by_the_validator() {
        // The property that was missing: control generated codes in one shape
        // and the auth service validated another, so `zeroship login` printed a
        // code the `/device` page refused before querying for it. Both sides now
        // call these two functions, so the round trip cannot diverge again
        // without failing here.
        let mut rng = rand::thread_rng();
        for _ in 0..64 {
            let code = generate_user_code(&mut rng);
            assert_eq!(code.len(), USER_CODE_FORMATTED_LEN, "{code}");
            assert!(valid_user_code(&code), "{code}");
            assert_eq!(normalize_user_code(&code), code, "{code}");
        }
    }

    #[test]
    fn the_eight_character_shape_control_used_to_mint_is_refused() {
        // The one-variable control for the case above: same alphabet, same
        // separator, one thing changed - two groups instead of three. This is
        // the literal shape control minted, and it is what the `/device` page
        // was rejecting.
        assert!(!valid_user_code("BCDF-GHJK"));
        assert!(valid_user_code("BCDF-GHJK-LMNP"));
    }

    #[test]
    fn a_human_typed_code_normalizes_to_the_stored_form() {
        assert_eq!(normalize_user_code("bcdf ghjk lmnp"), "BCDF-GHJK-LMNP");
        assert_eq!(normalize_user_code("bcdfghjklmnp"), "BCDF-GHJK-LMNP");
        assert_eq!(normalize_user_code(" BCDF-GHJK-LMNP "), "BCDF-GHJK-LMNP");
    }

    #[test]
    fn non_codes_are_refused_rather_than_coerced() {
        assert!(!valid_user_code(""));
        assert!(!valid_user_code("AEIO-UAEI-OUAE"), "vowels are not in the alphabet");
        assert!(!valid_user_code(&"A".repeat(32)));
        assert!(!valid_user_code(&"B".repeat(33)));
    }

    #[test]
    fn the_op_public_url_is_the_issuer_without_its_protocol_prefix() {
        assert_eq!(
            op_public_url("http://auth.zeroship.localhost/oauth2"),
            Some("http://auth.zeroship.localhost")
        );
        assert_eq!(
            op_public_url("https://auth.zeroship.ai/oauth2/"),
            Some("https://auth.zeroship.ai")
        );
        // A GoTrue issuer is not this platform's OP, so no /device page follows
        // from it. Returning None is what makes control refuse to print a
        // verification URI it cannot serve.
        assert_eq!(op_public_url("https://project.supabase.co/auth/v1"), None);
        assert_eq!(op_public_url("/oauth2"), None);
    }
}
