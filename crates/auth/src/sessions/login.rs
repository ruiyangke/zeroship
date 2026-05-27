//! `IdP` login session cookie at `auth.zeroship.ai`. Cookie name:
//! `__Host-zsidp_session`. 12 h hard absolute, 30 min sliding idle.

pub const COOKIE_NAME: &str = "__Host-zsidp_session";

pub const IDLE_MINUTES: i64 = 30;
pub const ABSOLUTE_HOURS: i64 = 12;

/// Build the `Set-Cookie` header value for the `IdP` session.
///
/// `insecure_dev = true` drops the `Secure` flag (so localhost HTTP works).
/// In production this MUST be false.
#[must_use]
pub fn set_cookie(session_id: &uuid::Uuid, insecure_dev: bool) -> String {
    let secure = if insecure_dev { "" } else { "; Secure" };
    let max_age = ABSOLUTE_HOURS * 3600;
    format!(
        "{COOKIE_NAME}={session_id}; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age={max_age}"
    )
}

/// Clear the session cookie.
#[must_use]
pub fn clear_cookie(insecure_dev: bool) -> String {
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age=0")
}

/// Parse the session id from a request's `Cookie` header value.
#[must_use]
pub fn parse_cookie(cookie_header: &str) -> Option<uuid::Uuid> {
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(&format!("{COOKIE_NAME}=")) {
            return uuid::Uuid::parse_str(rest).ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_cookie_has_secure_in_prod() {
        let id = uuid::Uuid::new_v4();
        let c = set_cookie(&id, false);
        assert!(c.contains("Secure"), "prod cookie must have Secure: {c}");
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Lax"));
        assert!(c.contains("Max-Age=43200")); // 12h * 3600
    }

    #[test]
    fn set_cookie_drops_secure_in_dev() {
        let id = uuid::Uuid::new_v4();
        let c = set_cookie(&id, true);
        assert!(!c.contains("Secure"), "dev cookie must NOT have Secure: {c}");
    }

    #[test]
    fn parses_cookie() {
        let id = uuid::Uuid::new_v4();
        let header = format!("foo=bar; __Host-zsidp_session={id}; baz=qux");
        assert_eq!(parse_cookie(&header), Some(id));
        assert_eq!(parse_cookie("nothing-here"), None);
    }
}
