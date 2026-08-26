//! The vocabulary of `zeroship.device_grants`, defined once.
//!
//! One RFC 8628 flow writes that table now: [`OP_PROVIDER`], the auth
//! service's own device grant. The approved row is redeemed at the OP's
//! `/oauth2/token` for an OIDC access token, so the row carries `client_id`,
//! `sid` and `auth_credential_version`.
//!
//! [`PLATFORM_PROVIDER`] named the control plane's parallel deploy-token
//! grant, which was deleted once `zeroship login` moved onto the OP's
//! endpoints. The spelling survives because `zeroship.identity_links` still
//! uses it to mark a platform-native principal (see
//! `crates/control/src/identity_bridge.rs`); nothing writes a
//! `device_grants` row with it.
//!
//! The auth service also reconciles a first-party [`PLATFORM_CLI_CLIENT_ID`]
//! registration at startup. Its OP device grants use [`OP_PROVIDER`] and may
//! request only [`PLATFORM_CLI_REGISTERED_SCOPES`]. Redemption issues a public
//! subject equal to the platform principal UUID and the configured control
//! audience; app clients keep their app-sector pairwise subjects.
//!
//! `zeroship login` drives THAT flow. It asks for
//! [`OFFLINE_ACCESS_SCOPE`], so redemption also opens a rotating refresh
//! family and the access token takes the OP's ordinary short lifetime. That is
//! the trade the whole arrangement exists for: a self-contained bearer cannot
//! be called back once minted, so the long-lived half has to be the
//! DB-backed refresh token, which reuse detection and family revocation can
//! kill.
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

/// Marks a platform-native principal in `zeroship.identity_links`.
///
/// Named for control's deleted deploy-token device grant, which is where the
/// spelling started. No `zeroship.device_grants` row carries it any more.
pub const PLATFORM_PROVIDER: &str = "platform";

/// OAuth client id reserved for first-party CLI platform access tokens.
///
/// Auth reconciles this registration at startup and fixes this id onto the
/// tokens its device grant issues to `zeroship login`.
pub const PLATFORM_CLI_CLIENT_ID: &str = "zeroship-cli";

/// Exact AUTHORITY ceiling registered for the first-party platform CLI client.
///
/// These are the scopes a CLI token may actually carry authority for. The
/// bearer path intersects them with the principal's stored grants
/// (`crates/authn/src/lib.rs`, `platform_cli_entitlement`).
///
/// This is deliberately NOT the same list as
/// [`PLATFORM_CLI_REGISTERED_SCOPES`]: `offline_access` may be REQUESTED (it
/// asks for a refresh token) but confers no resource authority, and folding it
/// in here would let it through that intersection as though
/// it did.
pub const PLATFORM_CLI_ISSUABLE_SCOPES: [&str; 4] = [
    "apps:deploy",
    "apps:read",
    "apps:write",
    "secrets:read",
];

/// The scope that asks an OAuth authorization server for a refresh token.
pub const OFFLINE_ACCESS_SCOPE: &str = "offline_access";

/// Exact `oauth_clients.scopes` the CLI registration is reconciled to.
///
/// The device-authorization endpoint checks the requested scope against the
/// REGISTRATION, so a scope missing here is refused with `invalid_scope`
/// before anything else happens. `offline_access` therefore has to be listed
/// even though it grants no authority.
pub const PLATFORM_CLI_REGISTERED_SCOPES: [&str; 5] = [
    "apps:deploy",
    "apps:read",
    "apps:write",
    "secrets:read",
    OFFLINE_ACCESS_SCOPE,
];

/// Maximum lifetime of a platform CLI token, in seconds.
///
/// Account deletion recalls these tokens with a platform-family marker. Other
/// revocation reasons still rely on expiry, so this remains an authorization
/// ceiling. Markers are retained for 24 hours and outlast this 12-hour maximum.
///
/// This is a CEILING, not the lifetime the OP device grant issues: that grant
/// now returns a refresh family, so its access token takes the OP's ordinary
/// short lifetime (`ACCESS_TOKEN_TTL_SECS`) and the long life lives in the
/// rotating, revocable refresh token instead. What the ceiling still bounds is
/// `validate_registered_ttl`, which the OP applies to every mint, and the
/// signing-key retention horizon that has to outlast the longest live token.
pub const PLATFORM_TOKEN_MAX_TTL_SECS: i64 = 12 * 60 * 60;

/// The OP's device-authorization endpoint, relative to the issuer.
///
/// Declared here rather than in the auth service so the producer of the
/// discovery document and the CLI that consumes it read one definition. That
/// is the same rule the `provider` column and the user-code shape are under,
/// and for the same reason: the two spellings drifted the moment they were
/// two constants.
pub const DEVICE_AUTHORIZATION_PATH: &str = "/device/authorization";

/// The OP's token endpoint, relative to the issuer.
pub const TOKEN_PATH: &str = "/token";

/// RFC 9728 protected-resource metadata, relative to the resource server.
///
/// The CLI asks CONTROL which authorization server to talk to rather than
/// being configured with one, so the OP it logs in to is by construction the
/// OP whose tokens control accepts.
pub const PROTECTED_RESOURCE_METADATA_PATH: &str = "/.well-known/oauth-protected-resource";

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
