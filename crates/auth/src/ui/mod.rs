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
pub mod forgot;
pub mod link;
pub mod login;
pub mod magic;
pub mod me;
pub mod oauth_github;
pub mod oauth_google;
pub mod oauth_stash;
pub mod reset;
pub mod signup;
pub mod verify;
pub mod webhooks;

use askama::Template;

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
pub struct ErrorPage<'a> {
    pub error: &'a str,
    pub error_description: Option<&'a str>,
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

/// `/consent` GET page — third-party RP consent form (P4-U5). The handler
/// translates each requested scope into a human-readable string via
/// `consent::translate_scope` before constructing this struct.
#[derive(Debug, Template)]
#[template(path = "consent.html")]
pub struct ConsentPage<'a> {
    pub challenge: &'a str,
    pub csrf: &'a str,
    pub client_id: &'a str,
    pub client_name: &'a str,
    pub scopes: Vec<&'a str>,
    pub error: Option<&'a str>,
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

/// `/verify` GET page (P5-U5) — shown after a successful email-verification
/// token redeem. Pure confirmation; no follow-up action required.
#[derive(Debug, Template)]
#[template(path = "verify_ok.html")]
pub struct VerifyOkPage<'a> {
    pub email: &'a str,
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
