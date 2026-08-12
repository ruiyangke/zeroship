//! `IdP` login session cookie at `auth.zeroship.ai`. 12 h hard absolute,
//! 30 min sliding idle.
//!
//! The cookie always uses the `__Host-` prefix and `Secure`. The local browser
//! topology uses `.localhost`, which browsers treat as potentially trustworthy;
//! it does not require a weaker cookie shape.

/// Cookie name (`__Host-` prefix requires Secure, Path=/, and no Domain).
pub const COOKIE_NAME: &str = "__Host-zsidp_session";

pub const IDLE_MINUTES: i64 = 30;
pub const ABSOLUTE_HOURS: i64 = 12;

/// Build the `Set-Cookie` header value for the `IdP` session.
#[must_use]
pub fn set_cookie(session_id: &uuid::Uuid) -> String {
    let max_age = ABSOLUTE_HOURS * 3600;
    format!(
        "{COOKIE_NAME}={session_id}; Path=/; HttpOnly; SameSite=Lax; Secure; Max-Age={max_age}"
    )
}

/// Clear the session cookie.
#[must_use]
pub fn clear_cookie() -> String {
    format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Secure; Max-Age=0")
}

/// Parse the session id from a request's `Cookie` header value.
#[must_use]
pub fn parse_cookie(cookie_header: &str) -> Option<uuid::Uuid> {
    let prefix = format!("{COOKIE_NAME}=");
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
    fn set_cookie_has_secure_and_host_prefix() {
        let id = uuid::Uuid::new_v4();
        let c = set_cookie(&id);
        assert!(c.starts_with("__Host-zsidp_session="), "cookie name: {c}");
        assert!(c.contains("Secure"), "cookie must have Secure: {c}");
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Lax"));
        assert!(c.contains("Max-Age=43200")); // 12h * 3600
    }

    #[test]
    fn parses_host_prefixed_cookie() {
        let id = uuid::Uuid::new_v4();
        let header = format!("foo=bar; __Host-zsidp_session={id}; baz=qux");
        assert_eq!(parse_cookie(&header), Some(id));
        assert_eq!(parse_cookie("nothing-here"), None);
    }

    #[test]
    fn rejects_bare_cookie_name() {
        let id = uuid::Uuid::new_v4();
        let header = format!("foo=bar; zsidp_session={id}; baz=qux");
        assert_eq!(parse_cookie(&header), None);
    }
}
