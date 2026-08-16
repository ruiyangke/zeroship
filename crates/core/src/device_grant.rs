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

/// OAuth client id fixed onto access tokens issued to `zeroship login`.
pub const PLATFORM_CLI_CLIENT_ID: &str = "zeroship-cli";

/// Scopes the control service may issue to the platform CLI client.
pub const PLATFORM_CLI_ISSUABLE_SCOPES: [&str; 4] = [
    "apps:deploy",
    "apps:read",
    "apps:write",
    "secrets:read",
];

/// Maximum lifetime of a platform CLI token, in seconds.
///
/// There is no supported early-recall operation for these tokens today, so
/// expiry is the effective revocation bound. Token-family markers are retained
/// for 24 hours, so a marker written while a token is live outlasts this
/// 12-hour maximum remaining lifetime by at least 12 hours.
pub const PLATFORM_TOKEN_MAX_TTL_SECS: i64 = 12 * 60 * 60;

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

/// Where control POSTs the platform deploy-token mint, validated.
///
/// This is the OUTBOUND half of what [`op_public_url`] used to do on its own,
/// and it is deliberately not derived from the issuer. See the doc comment on
/// `ControlSettings::auth_platform_mint_url` for why the trust anchor and the
/// route cannot be one string.
///
/// The rules exist because the request this URL addresses carries the dedicated
/// platform-mint key in an `Authorization` header, so whatever this names
/// receives that credential. What the rules bound:
///
///   * ABSOLUTE http/https only. A relative or scheme-less value cannot be
///     resolved against anything here, and `auth:9092` - which reads as a
///     host:port - parses as the scheme `auth`, so it is refused by name rather
///     than silently treated as a path.
///   * NO userinfo. `https://auth.internal@evil.example` names `evil.example`
///     while reading as the internal host; a value carrying `@` is refused.
///   * NO path, query or fragment. Control appends a fixed endpoint path, and
///     a configured suffix could otherwise move or re-parse the effective
///     target.
///
/// What it does NOT bound: the host itself. Nothing here can tell an internal
/// address from a hostile one, and a check that pretended to would be claiming
/// a protection it does not have. The real bound is on the SUPPLY: this value
/// comes only from a flag, an environment variable, or the operator-owned
/// config overlay, never from a request, a token, a database row, or the
/// issuer string.
///
/// # Errors
///
/// Returns the specific rule the value broke, so the boot diagnostic can name
/// it rather than saying "invalid".
pub fn platform_mint_base_url(raw: &str) -> Result<&str, MintUrlError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(MintUrlError::Missing);
    }
    let parsed = url::Url::parse(trimmed).map_err(|_| MintUrlError::NotAbsolute)?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(MintUrlError::UnsupportedScheme);
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err(MintUrlError::NoHost);
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(MintUrlError::Userinfo);
    }
    if parsed.path() != "/" {
        return Err(MintUrlError::HasPath);
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(MintUrlError::QueryOrFragment);
    }
    Ok(trimmed.trim_end_matches('/'))
}

/// Why a configured platform mint URL was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MintUrlError {
    #[error("no value is set")]
    Missing,

    #[error("must be an absolute URL, e.g. http://auth:9092")]
    NotAbsolute,

    #[error("must use http or https")]
    UnsupportedScheme,

    #[error("must name a host")]
    NoHost,

    #[error("must not carry userinfo (a `user:pass@` prefix)")]
    Userinfo,

    #[error("must be a bare origin with no path")]
    HasPath,

    #[error("must not carry a query string or fragment")]
    QueryOrFragment,
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

    #[test]
    fn a_mint_url_is_a_bare_reachable_origin() {
        assert_eq!(
            platform_mint_base_url("http://auth:9092"),
            Ok("http://auth:9092")
        );
        // A trailing slash is the same origin, and the caller appends a path
        // beginning with `/`, so it is trimmed rather than refused.
        assert_eq!(
            platform_mint_base_url("https://auth.internal/  "),
            Ok("https://auth.internal")
        );
        assert_eq!(
            platform_mint_base_url("http://127.0.0.1:9481"),
            Ok("http://127.0.0.1:9481")
        );
    }

    #[test]
    fn a_mint_url_that_could_redirect_the_control_key_is_refused() {
        // Each case is a way the resolved destination could differ from the
        // host an operator reading the value would name.
        assert_eq!(platform_mint_base_url(""), Err(MintUrlError::Missing));
        assert_eq!(platform_mint_base_url("   "), Err(MintUrlError::Missing));
        // Reads as host:port, parses as the scheme `auth`.
        assert_eq!(
            platform_mint_base_url("auth:9092"),
            Err(MintUrlError::UnsupportedScheme)
        );
        assert_eq!(
            platform_mint_base_url("//auth:9092"),
            Err(MintUrlError::NotAbsolute)
        );
        assert_eq!(
            platform_mint_base_url("file:///etc/passwd"),
            Err(MintUrlError::UnsupportedScheme)
        );
        // Reads as the internal host, resolves to evil.example.
        assert_eq!(
            platform_mint_base_url("https://auth.internal@evil.example"),
            Err(MintUrlError::Userinfo)
        );
        assert_eq!(
            platform_mint_base_url("http://auth:9092/oauth2"),
            Err(MintUrlError::HasPath)
        );
        assert_eq!(
            platform_mint_base_url("http://auth:9092/?next=x"),
            Err(MintUrlError::QueryOrFragment)
        );
        assert_eq!(
            platform_mint_base_url("http://auth:9092/#frag"),
            Err(MintUrlError::QueryOrFragment)
        );
    }

    #[test]
    fn the_issuer_and_the_mint_url_are_not_interchangeable() {
        // The one-variable pair that names the whole bug. `op_public_url` maps
        // an issuer to the PUBLIC origin that advertises it - correct for the
        // browser page it feeds, and the wrong answer for an outbound POST on a
        // host with no egress-and-back route. The mint URL is that outbound
        // answer, and it is not derivable from the issuer: nothing in the
        // issuer string mentions `auth:9092`.
        let issuer = "https://auth.zeroship.co/oauth2";
        assert_eq!(op_public_url(issuer), Some("https://auth.zeroship.co"));
        assert_eq!(
            platform_mint_base_url("http://auth:9092"),
            Ok("http://auth:9092")
        );
        // And the issuer is not itself a legal mint URL, so a deployment
        // cannot quietly paste one into the other and keep today's behaviour.
        assert_eq!(
            platform_mint_base_url(issuer),
            Err(MintUrlError::HasPath)
        );
    }
}
