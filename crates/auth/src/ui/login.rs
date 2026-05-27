//! `/login` GET handler.
//!
//! Algorithm (P2 §8.1 GET path):
//!
//! 1. Read `login_challenge` from the query.
//! 2. Fetch challenge metadata from hydra via `admin.get_login(challenge)`.
//! 3. If `info.skip == true`, hydra already has a session for this subject —
//!    immediately call `accept_login` and redirect back to hydra.
//! 4. Otherwise render the form with a fresh CSRF token cookie.
//!
//! Route wiring happens in P2-U6 (`server::configure`); the handler here is
//! a plain `pub async fn` with no `#[ntex::web::*]` attribute.

use askama::Template;
use ntex::http::header::{HeaderValue, LOCATION, SET_COOKIE};
use ntex::web::{HttpRequest, HttpResponse};
use serde::Deserialize;
use std::sync::Arc;

use crate::config::AuthConfig;
use crate::csrf;
use crate::hydra_client::types::AcceptLoginRequest;
use crate::hydra_client::HydraAdmin;
use crate::ui::LoginPage;

#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    pub login_challenge: String,
}

pub async fn get(
    req: HttpRequest,
    query: ntex::web::types::Query<LoginQuery>,
    admin: ntex::web::types::State<HydraAdmin>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
) -> HttpResponse {
    let _ = req; // header extraction (UA, request-id) lands in later phases.
    let challenge = &query.login_challenge;

    // Fetch challenge details from hydra.
    let info = match admin.get_login(challenge).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, challenge = %challenge, "login challenge fetch failed");
            return render_error("invalid login request", Some(&e.to_string()));
        }
    };

    // Skip path: hydra already knows the subject.
    if info.skip {
        let accept = AcceptLoginRequest {
            subject: info.subject.clone(),
            remember: Some(true),
            remember_for: Some(3600),
            ..Default::default()
        };
        match admin.accept_login(challenge, &accept).await {
            Ok(resp) => return redirect(&resp.redirect_to),
            Err(e) => {
                tracing::error!(error = %e, "accept_login (skip path) failed");
                return render_error("internal error", Some(&e.to_string()));
            }
        }
    }

    // Render the form with a fresh CSRF token cookie.
    let csrf_token = csrf::generate_token();
    let page = LoginPage {
        challenge,
        csrf: &csrf_token,
        error: None,
        client_name: info.client.client_name.as_deref().unwrap_or(&info.client.client_id),
    };
    let body = match page.render() {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "render login.html failed");
            return render_error("internal error", Some("template render"));
        }
    };

    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token, cfg.insecure_dev));
    resp.body(body)
}

fn redirect(to: &str) -> HttpResponse {
    let mut resp = HttpResponse::Found();
    resp.header(
        LOCATION,
        HeaderValue::from_str(to).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    resp.finish()
}

fn render_error(error: &str, error_description: Option<&str>) -> HttpResponse {
    use crate::ui::ErrorPage;
    let page = ErrorPage { error, error_description };
    let body = page.render().unwrap_or_else(|_| format!("<h1>{error}</h1>"));
    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.body(body)
}
