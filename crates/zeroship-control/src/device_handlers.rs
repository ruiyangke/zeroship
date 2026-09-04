//! RFC 9728 protected-resource metadata.
//!
//! Control used to own a SECOND RFC 8628 device flow here: `/api/device/auth`
//! wrote `provider = 'platform'` rows, `/api/device/approve` bound a principal
//! to one, and `/api/device/token` minted a deploy token through the auth
//! service's `/internal/platform-token`. `zeroship login` moved onto the OP's
//! own device grant in `5ae8c7f7d`, which hands the CLI a bounded access token
//! plus a rotating refresh family that this flow could never issue, and both
//! halves were deleted once nothing drove them.
//!
//! What control still owes a CLI holding no credential is the RFC 9728
//! document naming the authorization server it trusts. That is the whole of
//! the device story on this side now; the OP owns the grant itself
//! (`crates/zeroship-auth/src/oidc/device_token.rs`).

use std::sync::Arc;

use ntex::web;
use ntex::web::types::State;
use serde::Serialize;
use serde_json::json;
use zeroship_core::device_grant;

use crate::AppState;

/// RFC 9728 protected-resource metadata.
///
/// `authorization_servers` is a list in the RFC; control accepts exactly one
/// issuer (`auth.platform_issuer`, the same string
/// `zeroship_authn::BearerVerifier` checks `iss` against), so the list is
/// always a single element or the document is not served at all.
#[derive(Debug, Serialize)]
pub struct ProtectedResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource(device_grant::PROTECTED_RESOURCE_METADATA_PATH)
            .route(web::get().to(protected_resource_metadata)),
    );
}

/// Tell an unauthenticated client which authorization server to log in to.
///
/// The CLI holds one configured URL - control's. Asking control which OP it
/// trusts means the OP it logs in to is BY CONSTRUCTION the one whose tokens
/// control will accept: the same `platform_issuer` that
/// `zeroship_authn::BearerVerifier` pins `iss` to, and whose JWKS it fetches.
/// A separately configured auth URL could drift from it, and the failure would
/// surface as an opaque 401 at the first deploy rather than at login.
///
/// Unauthenticated by design: RFC 9728 metadata exists to be read by a client
/// that has no credential yet. It discloses only what a 401
/// `WWW-Authenticate` challenge would.
pub async fn protected_resource_metadata(state: State<Arc<AppState>>) -> web::HttpResponse {
    let Some(issuer) = state.auth_provider.platform_issuer() else {
        // A deployment with no platform OP has no authorization server to
        // advertise. Saying so beats naming an issuer whose tokens would be
        // refused.
        return web::HttpResponse::NotFound().json(&json!({"error": "no_platform_issuer"}));
    };
    web::HttpResponse::Ok()
        .header("cache-control", "no-store")
        .json(&ProtectedResourceMetadata {
            resource: state.expected_oauth_audience.clone(),
            authorization_servers: Vec::from([issuer.to_string()]),
        })
}
