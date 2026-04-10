//! Internal API handlers — worker-facing endpoints.

use std::sync::Arc;

use ntex::web;
use ntex::web::types::State;

use crate::AppState;

pub async fn get_versions(_state: State<Arc<AppState>>) -> web::HttpResponse {
    web::HttpResponse::Ok().finish()
}

pub async fn get_bundle(
    _state: State<Arc<AppState>>,
    _app_id: ntex::web::types::Path<String>,
) -> web::HttpResponse {
    web::HttpResponse::Ok().finish()
}

pub async fn get_routes(_state: State<Arc<AppState>>) -> web::HttpResponse {
    web::HttpResponse::Ok().finish()
}

pub async fn report_usage(_state: State<Arc<AppState>>) -> web::HttpResponse {
    web::HttpResponse::Ok().finish()
}

pub async fn health() -> web::HttpResponse {
    web::HttpResponse::Ok().finish()
}
