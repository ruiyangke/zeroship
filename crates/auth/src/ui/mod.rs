//! Server-rendered HTML UI for the `IdP`. Templates compiled via askama.
//!
//! Template sources live under `crates/auth/src/ui/templates/` and are
//! discovered through the `[package.metadata.askama] dirs = ["src/ui/templates"]`
//! entry in `crates/auth/Cargo.toml`. Each `#[derive(Debug, Template)]` struct
//! is checked at compile time — a malformed template breaks the build.
//!
//! Handlers in P2-U4 / U5 / U6 render with `.render()` and stuff the
//! resulting `String` into `ntex::web::HttpResponse::Ok().content_type(
//! "text/html; charset=utf-8").body(rendered)`.

pub mod consent;
pub mod device;
pub mod forgot;
pub mod link;
pub mod login;
pub mod logout;
pub mod magic;
pub mod me;
pub mod oauth_github;
pub mod oauth_google;
pub mod oauth_stash;
pub mod password;
pub mod reset;
pub mod signup;
pub mod verify;
pub mod webhooks;

use askama::Template;
use ntex::http::header::SET_COOKIE;
use ntex::web::HttpResponse;

use crate::headers;

/// `/login` GET page. The handler resolves `client_name` from
/// `info.client.client_name.as_deref().unwrap_or(&info.client.client_id)`
/// before constructing this struct.
#[derive(Debug, Template)]
#[template(path = "login.html")]
pub struct LoginPage<'a> {
    pub challenge: &'a str,
    pub csrf: &'a str,
    pub error: Option<&'a str>,
    pub client_name: &'a str,
    pub google_enabled: bool,
    pub github_enabled: bool,
}

/// `/signup` GET page. Same `login_challenge` carries through so that
/// after account creation we can flow straight back into hydra's
/// `accept_login`.
#[derive(Debug, Template)]
#[template(path = "signup.html")]
pub struct SignupPage<'a> {
    pub challenge: &'a str,
    pub csrf: &'a str,
    pub error: Option<&'a str>,
}

/// Generic OAuth/auth-flow error page.
#[derive(Debug, Template)]
#[template(path = "error.html")]
pub struct ErrorPage {
    pub message: PublicErrorMessage,
    pub error_code: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicErrorMessage {
    InvalidRequest,
    SessionExpired,
    PleaseTryAgain,
    AccountTemporarilyLocked,
    ContactSupport,
}

impl PublicErrorMessage {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid request",
            Self::SessionExpired => "session expired",
            Self::PleaseTryAgain => "please try again",
            Self::AccountTemporarilyLocked => "account temporarily locked",
            Self::ContactSupport => "contact support",
        }
    }

    #[must_use]
    pub const fn error_code(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::SessionExpired => "session_expired",
            Self::PleaseTryAgain => "please_try_again",
            Self::AccountTemporarilyLocked => "account_temporarily_locked",
            Self::ContactSupport => "contact_support",
        }
    }
}

/// `/link` GET/POST page — shown after a federation callback detects an
/// email collision with a locally-credentialed account. The user
/// re-enters their zeroship password to confirm the link.
#[derive(Debug, Template)]
#[template(path = "link.html")]
pub struct LinkPage<'a> {
    pub token: &'a str,
    pub csrf: &'a str,
    pub existing_email: &'a str,
    pub provider: &'a str,
    pub error: Option<&'a str>,
}

/// `/consent` GET page — third-party RP consent form. The handler translates
/// each requested scope into a human-readable string before constructing this
/// struct.
#[derive(Debug, Template)]
#[template(path = "consent.html")]
pub struct ConsentPage<'a> {
    pub challenge: &'a str,
    pub csrf: &'a str,
    pub client_id: &'a str,
    pub client_name: &'a str,
    pub client_logo_uri: Option<&'a str>,
    pub scopes: Vec<ConsentScopeView>,
    pub can_grant: bool,
    pub grant_error: Option<&'a str>,
}

#[derive(Debug)]
pub struct ConsentScopeView {
    pub label: String,
    /// App-declared scope description (`zeroship.app_scope_defs.description`),
    /// rendered as a per-scope sub-line on the consent screen. `None` for
    /// identity/platform scopes and app scopes that declared no description.
    pub description: Option<String>,
    pub unrecognized: bool,
}

/// One row in the linked-identities list on `/me`. Mirrors
/// `store::identities::Identity` but trimmed to the fields the template
/// surfaces — kept as borrowed strings so the page struct stays zero-copy.
#[derive(Debug)]
pub struct LinkedIdentity<'a> {
    pub provider: &'a str,
    pub email_at_link: &'a str,
}

/// `/magic/start` POST success page — "Check your email" with an
/// optional code-entry form for the cross-device flow. The visible form
/// is hidden behind a `<details>` toggle on the same page.
#[derive(Debug, Template)]
#[template(path = "magic_check_email.html")]
pub struct MagicCheckEmailPage<'a> {
    pub csrf: &'a str,
    pub login_challenge: &'a str,
    pub csrf_nonce: &'a str,
    pub email: &'a str,
}

/// `/magic/await` GET — alternate landing for the cross-device code-entry
/// form (deep-linkable variant of `MagicCheckEmailPage`'s inner form).
#[derive(Debug, Template)]
#[template(path = "magic_await_code.html")]
pub struct MagicAwaitCodePage<'a> {
    pub csrf: &'a str,
    pub login_challenge: &'a str,
    pub csrf_nonce: &'a str,
    pub email: &'a str,
}

/// `/magic/verify` cross-device branch — displays a 6-digit code on the
/// redeeming device for the user to type back on the requesting device.
#[derive(Debug, Template)]
#[template(path = "magic_show_code.html")]
pub struct MagicShowCodePage<'a> {
    pub code: &'a str,
    pub email: &'a str,
}

/// `/device` GET + POST page for OAuth 2.0 Device Authorization Grant
/// user-code entry.
#[derive(Debug, Template)]
#[template(path = "device.html")]
pub struct DevicePage<'a> {
    pub user_code: &'a str,
    pub error: Option<&'a str>,
}

/// `/verify` GET page (P5-U5) — shown after a successful email-verification
/// token redeem. Pure confirmation; no follow-up action required.
#[derive(Debug, Template)]
#[template(path = "verify_ok.html")]
pub struct VerifyOkPage<'a> {
    pub email: &'a str,
}

#[derive(Debug, Template)]
#[template(path = "token_redeem_interstitial.html")]
pub struct TokenRedeemInterstitial<'a> {
    pub title: &'a str,
    pub action: &'a str,
    pub token: &'a str,
    pub csrf: &'a str,
    pub extra_fields: Vec<(&'a str, &'a str)>,
}

pub fn render_token_interstitial(
    page: &TokenRedeemInterstitial<'_>,
    csrf_set_cookie: &str,
) -> HttpResponse {
    let body = page
        .render()
        .unwrap_or_else(|_| "<p>Redirecting...</p>".to_string());
    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.header("Cache-Control", "no-store");
    resp.header("Pragma", "no-cache");
    resp.header(
        "Content-Security-Policy",
        headers::content_security_policy_with_script_nonce(page.csrf),
    );
    resp.header(SET_COOKIE, csrf_set_cookie);
    resp.body(body)
}

/// `/forgot` GET + POST page (P5-U6).
///
/// The `sent` flag toggles the form off and renders the post-submit
/// confirmation copy ("if an account exists, we sent a link…"). The
/// form and confirmation share a page so the response is identical to
/// the attacker whether the email exists or not — enumeration defense.
#[derive(Debug, Template)]
#[template(path = "forgot.html")]
pub struct ForgotPage<'a> {
    pub csrf: &'a str,
    pub error: Option<&'a str>,
    pub sent: bool,
}

/// `/reset` GET + POST page (P5-U6).
///
/// Surfaces the new-password form; the hidden `token` field carries the
/// reset token between GET and POST so the user can re-submit on
/// validation errors without re-clicking the email link.
#[derive(Debug, Template)]
#[template(path = "reset.html")]
pub struct ResetPage<'a> {
    pub token: &'a str,
    pub csrf: &'a str,
    pub error: Option<&'a str>,
}

/// `/logout` GET page (RP-initiated logout, RFC OIDC §5 — RP redirects
/// to hydra's `end_session_endpoint`, hydra issues a `logout_challenge`
/// and 302s here). Renders a CSRF-protected confirm form; the POST
/// handler calls hydra's `accept_logout` and redirects to the
/// post-logout `redirect_to`.
#[derive(Debug, Template)]
#[template(path = "logout.html")]
pub struct LogoutPage<'a> {
    pub challenge: &'a str,
    pub csrf: &'a str,
    /// `Some(name)` when the RP that initiated logout published a
    /// human-readable `client_name`. `None` for hydra-internal flows
    /// (e.g. session-cleanup without a specific RP context).
    pub client_name: Option<&'a str>,
    pub error: Option<&'a str>,
}

/// `/me` profile page (P4-U6).
///
/// Logged-in-user only; the handler resolves the user from the
/// `__Host-zsidp_session` cookie before rendering. The `error`/`success`
/// arms are mutually exclusive in practice but kept independent so
/// future flows can layer messages without touching the template.
#[derive(Debug, Template)]
#[template(path = "me.html")]
pub struct MePage<'a> {
    pub email: &'a str,
    pub name: &'a str,
    pub avatar_url: Option<&'a str>,
    pub has_password: bool,
    pub identities: Vec<LinkedIdentity<'a>>,
    pub csrf: &'a str,
    pub error: Option<&'a str>,
    pub success: Option<&'a str>,
}

#[cfg(test)]
mod tests {
    use askama::Template;

    use super::{ErrorPage, PublicErrorMessage};

    #[test]
    fn error_page_renders_only_public_message() {
        let sensitive = "DB host=internal.zeroship.svc.cluster.local";
        let page = ErrorPage {
            message: PublicErrorMessage::ContactSupport,
            error_code: PublicErrorMessage::ContactSupport.error_code(),
        };

        let html = page.render().expect("error page renders");

        assert!(
            !html.contains(sensitive),
            "error page must not render sensitive error details"
        );
        assert!(html.contains(PublicErrorMessage::ContactSupport.as_str()));
    }
}
