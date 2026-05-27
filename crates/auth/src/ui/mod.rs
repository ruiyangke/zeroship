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
pub mod login;
pub mod oauth_google;
pub mod oauth_stash;
pub mod signup;

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
