//! Browser authorization, callback response headers and signout behavior.

#![allow(clippy::future_not_send, reason = "browser fixtures run on compio")]

use crate::db::tests::postgres::Database;
use crate::tests::browser::*;
use crate::{anchors, browser_auth, GateState};
use ntex::web::{self, test};
use std::sync::Arc;
use uuid::Uuid;
use zeroship_core::{app_id::AppId, user_id::UserId};

mod authorize;
mod popup;
mod signout;
mod validation;

macro_rules! browser_app {
    ($state:expr) => {{
        web::App::new()
            .state($state.clone())
            .service(
                web::resource("/__zeroship/auth/authorize")
                    .route(web::get().to(browser_auth::authorize)),
            )
            .service(
                web::resource("/__zeroship/auth/popup-callback")
                    .route(web::get().to(browser_auth::popup_callback)),
            )
            .service(
                web::resource("/__zeroship/auth/signout")
                    .route(web::post().to(browser_auth::signout)),
            )
            .service(
                web::resource("/__zeroship/auth/session")
                    .route(web::post().to(crate::auth_token::session_post))
                    .route(web::get().to(crate::auth_token::session)),
            )
    }};
}
use browser_app;

fn state() -> (Arc<GateState>, tempfile::TempDir) {
    build_state("http://127.0.0.1:1", None)
}
fn header_str(resp: &ntex::web::WebResponse, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

async fn read_text(resp: ntex::web::WebResponse) -> String {
    let bytes = test::read_body(resp).await;
    String::from_utf8(bytes.to_vec()).expect("utf8 body")
}

fn assert_cookie_clears(resp: &ntex::web::WebResponse) {
    for name in [
        "__Host-zeroship_app_anchor",
        "__Host-zeroship_app_session",
        "zs.myapp.zeroship.ai.is.authenticated",
    ] {
        let cookie = set_cookie_with_prefix(resp, &format!("{name}=")).expect("cookie cleared");
        assert!(
            cookie
                .split(';')
                .any(|attribute| attribute.trim() == "Max-Age=0"),
            "{cookie}"
        );
    }
}
