//! `IdP` login session cookie at `auth.zeroship.ai`. 12 h hard absolute,
//! 30 min sliding idle.
//!
//! Cookie name is `__Host-zsidp_session` in production. Dev mode
//! (`insecure_dev = true`) drops the `__Host-` prefix and emits
//! `zsidp_session` — RFC 6265bis §4.1.3.2 requires `__Host-` cookies to
//! carry `Secure`, and dev runs over plain HTTP without it.

/// Production cookie name (`__Host-` prefix → Secure required).
pub const COOKIE_NAME_PROD: &str = "__Host-zsidp_session";
/// Dev cookie name (no prefix → no Secure requirement).
pub const COOKIE_NAME_DEV: &str = "zsidp_session";

pub const IDLE_MINUTES: i64 = 30;
pub const ABSOLUTE_HOURS: i64 = 12;

/// Resolve the cookie name for the current environment.
#[must_use]
pub fn cookie_name(insecure_dev: bool) -> &'static str {
    if insecure_dev { COOKIE_NAME_DEV } else { COOKIE_NAME_PROD }
}

/// Build the `Set-Cookie` header value for the `IdP` session.
///
/// `insecure_dev = true` drops the `Secure` flag AND the `__Host-`
/// prefix from the cookie name (RFC 6265bis §4.1.3.2 — `__Host-`
/// requires Secure).
#[must_use]
pub fn set_cookie(session_id: &uuid::Uuid, insecure_dev: bool) -> String {
    let name = cookie_name(insecure_dev);
    let secure = if insecure_dev { "" } else { "; Secure" };
    let max_age = ABSOLUTE_HOURS * 3600;
    format!(
        "{name}={session_id}; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age={max_age}"
    )
}

/// Clear the session cookie.
#[must_use]
pub fn clear_cookie(insecure_dev: bool) -> String {
    let name = cookie_name(insecure_dev);
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!("{name}=; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age=0")
}

/// Parse the session id from a request's `Cookie` header value.
#[must_use]
pub fn parse_cookie(cookie_header: &str, insecure_dev: bool) -> Option<uuid::Uuid> {
    let name = cookie_name(insecure_dev);
    let prefix = format!("{name}=");
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(&prefix) {
            return uuid::Uuid::parse_str(rest).ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_cookie_has_secure_and_host_prefix_in_prod() {
        let id = uuid::Uuid::new_v4();
        let c = set_cookie(&id, false);
        assert!(c.starts_with("__Host-zsidp_session="), "prod cookie name: {c}");
        assert!(c.contains("Secure"), "prod cookie must have Secure: {c}");
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Lax"));
        assert!(c.contains("Max-Age=43200")); // 12h * 3600
    }

    #[test]
    fn set_cookie_drops_secure_and_host_prefix_in_dev() {
        let id = uuid::Uuid::new_v4();
        let c = set_cookie(&id, true);
        assert!(!c.starts_with("__Host-"), "dev cookie must NOT have __Host- prefix: {c}");
        assert!(c.starts_with("zsidp_session="), "dev cookie name: {c}");
        assert!(!c.contains("Secure"), "dev cookie must NOT have Secure: {c}");
    }

    #[test]
    fn parses_cookie_prod() {
        let id = uuid::Uuid::new_v4();
        let header = format!("foo=bar; __Host-zsidp_session={id}; baz=qux");
        assert_eq!(parse_cookie(&header, false), Some(id));
        assert_eq!(parse_cookie("nothing-here", false), None);
    }

    #[test]
    fn parses_cookie_dev() {
        let id = uuid::Uuid::new_v4();
        let header = format!("foo=bar; zsidp_session={id}; baz=qux");
        assert_eq!(parse_cookie(&header, true), Some(id));
        // Prod name must not match in dev mode.
        let prod_header = format!("__Host-zsidp_session={id}");
        assert_eq!(parse_cookie(&prod_header, true), None);
    }
}
