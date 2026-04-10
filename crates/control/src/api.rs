//! Admin API handlers — app CRUD, deploy, plan, usage.

use std::sync::Arc;

use ntex::web;
use ntex::web::types::{Path, State};

use crate::AppState;

pub async fn create_app(_state: State<Arc<AppState>>) -> web::HttpResponse {
    web::HttpResponse::Ok().finish()
}

pub async fn list_apps(_state: State<Arc<AppState>>) -> web::HttpResponse {
    web::HttpResponse::Ok().finish()
}

pub async fn get_app(
    _state: State<Arc<AppState>>,
    _id: Path<String>,
) -> web::HttpResponse {
    web::HttpResponse::Ok().finish()
}

pub async fn delete_app(
    _state: State<Arc<AppState>>,
    _id: Path<String>,
) -> web::HttpResponse {
    web::HttpResponse::Ok().finish()
}

pub async fn deploy(
    _state: State<Arc<AppState>>,
    _id: Path<String>,
) -> web::HttpResponse {
    web::HttpResponse::Ok().finish()
}

pub async fn set_plan(
    _state: State<Arc<AppState>>,
    _id: Path<String>,
) -> web::HttpResponse {
    web::HttpResponse::Ok().finish()
}

pub async fn get_usage(
    _state: State<Arc<AppState>>,
    _id: Path<String>,
) -> web::HttpResponse {
    web::HttpResponse::Ok().finish()
}
