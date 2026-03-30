//! Tower middleware stack for appbase HTTP server.

use axum::http::Method;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};

/// Build the standard middleware stack for production.
pub fn production_layers() -> (CorsLayer, CompressionLayer) {
    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any)
        .allow_origin(Any);

    let compression = CompressionLayer::new();
    (cors, compression)
}
