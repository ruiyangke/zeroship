use zeroship_core::auth::validate_api_key;
use zeroship_core::types::RouteEntry;
use ntex::web::{HttpRequest, HttpResponse};

pub fn check_api_key(req: &HttpRequest, route: &RouteEntry) -> Result<(), HttpResponse> {
    let key = req
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if key.is_empty() {
        return Err(
            HttpResponse::Unauthorized()
                .json(&serde_json::json!({"error": "missing X-Api-Key header"})),
        );
    }

    if !validate_api_key(key, &route.api_key_hash) {
        return Err(
            HttpResponse::Unauthorized()
                .json(&serde_json::json!({"error": "invalid API key"})),
        );
    }

    Ok(())
}
