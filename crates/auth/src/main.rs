//! zeroship-auth — auth service binary.

mod handlers;
pub mod oauth;
mod queries;
mod service;

use std::sync::Arc;

use ntex::web;

use oauth::ProviderRegistry;
use service::AuthService;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Shared application state injected into every handler.
#[allow(missing_debug_implementations)]
pub struct AppState {
    pub auth: AuthService,
    pub oauth: ProviderRegistry,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Parse a simple `--flag value` pair from the argument list.
fn arg_or_env(args: &[String], flag: &str, env_key: &str, default: &str) -> String {
    for pair in args.windows(2) {
        if pair[0] == flag {
            return pair[1].clone();
        }
    }
    env_or(env_key, default)
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    zeroship_core::observability::init_tracing("info,zeroship_auth=debug");

    let args: Vec<String> = std::env::args().collect();

    let port = arg_or_env(&args, "--port", "PORT", "9091");
    let db_url = arg_or_env(&args, "--db", "DATABASE_URL", "postgres://localhost/zeroship");

    // JWT signing secret — defaults to a random value for development.
    let jwt_secret = arg_or_env(
        &args,
        "--auth-secret",
        "AUTH_SECRET",
        &uuid::Uuid::new_v4().to_string(),
    );

    let auth = AuthService::new(&db_url, &jwt_secret);

    // --- OAuth providers (enabled by env vars) ---
    // OIDC providers (Google, Apple) perform async discovery at startup.
    let service_url = arg_or_env(&args, "--url", "SERVICE_URL", &format!("http://localhost:{port}"));
    let mut oauth_registry = ProviderRegistry::new();

    if let (Ok(client_id), Ok(client_secret)) = (
        std::env::var("GOOGLE_CLIENT_ID"),
        std::env::var("GOOGLE_CLIENT_SECRET"),
    ) {
        let redirect_uri = format!("{service_url}/auth/callback/google");
        let config = oauth::OAuthConfig { client_id, client_secret, redirect_uri };
        match oauth::google::build(config).await {
            Ok(provider) => {
                oauth_registry.register(provider);
                tracing::info!(provider = "google", "oauth provider enabled (OIDC discovery OK)");
            }
            Err(e) => {
                tracing::error!(provider = "google", error = %e, "oauth provider build failed");
            }
        }
    }

    if let (Ok(client_id), Ok(client_secret)) = (
        std::env::var("GITHUB_CLIENT_ID"),
        std::env::var("GITHUB_CLIENT_SECRET"),
    ) {
        let redirect_uri = format!("{service_url}/auth/callback/github");
        let config = oauth::OAuthConfig { client_id, client_secret, redirect_uri };
        match oauth::github::build(config).await {
            Ok(provider) => {
                oauth_registry.register(provider);
                tracing::info!(provider = "github", "oauth provider enabled");
            }
            Err(e) => {
                tracing::error!(provider = "github", error = %e, "oauth provider build failed");
            }
        }
    }

    if let (Ok(client_id), Ok(client_secret)) = (
        std::env::var("APPLE_CLIENT_ID"),
        std::env::var("APPLE_CLIENT_SECRET"),
    ) {
        let redirect_uri = format!("{service_url}/auth/callback/apple");
        let config = oauth::OAuthConfig { client_id, client_secret, redirect_uri };
        match oauth::apple::build(config).await {
            Ok(provider) => {
                oauth_registry.register(provider);
                tracing::info!(provider = "apple", "oauth provider enabled (OIDC discovery OK)");
            }
            Err(e) => {
                tracing::error!(provider = "apple", error = %e, "oauth provider build failed");
            }
        }
    }

    if let (Ok(client_id), Ok(client_secret)) = (
        std::env::var("META_CLIENT_ID"),
        std::env::var("META_CLIENT_SECRET"),
    ) {
        let redirect_uri = format!("{service_url}/auth/callback/meta");
        let config = oauth::OAuthConfig { client_id, client_secret, redirect_uri };
        match oauth::meta::build(config).await {
            Ok(provider) => {
                oauth_registry.register(provider);
                tracing::info!(provider = "meta", "oauth provider enabled");
            }
            Err(e) => {
                tracing::error!(provider = "meta", error = %e, "oauth provider build failed");
            }
        }
    }

    let state = Arc::new(AppState { auth, oauth: oauth_registry });

    let bind_addr = format!("0.0.0.0:{port}");
    tracing::info!(bind = %bind_addr, "zeroship-auth listening");

    web::server(async move || {
        web::App::new()
            .state(state.clone())
            // --- Auth ---
            .service(
                web::resource("/auth/register")
                    .route(web::post().to(handlers::register)),
            )
            .service(
                web::resource("/auth/login")
                    .route(web::post().to(handlers::login)),
            )
            .service(
                web::resource("/auth/userinfo")
                    .route(web::get().to(handlers::userinfo)),
            )
            .service(
                web::resource("/auth/consent")
                    .route(web::post().to(handlers::consent)),
            )
            .service(
                web::resource("/auth/logout")
                    .route(web::post().to(handlers::logout)),
            )
            .service(
                web::resource("/auth/authorize")
                    .route(web::get().to(handlers::authorize)),
            )
            // --- OAuth ---
            .service(
                web::resource("/auth/{provider}")
                    .route(web::get().to(handlers::oauth_start)),
            )
            .service(
                web::resource("/auth/callback/{provider}")
                    .route(web::get().to(handlers::oauth_callback)),
            )
            // --- Health ---
            .service(
                web::resource("/health")
                    .route(web::get().to(health)),
            )
    })
    .bind(&bind_addr)?
    .run()
    .await
}

async fn health() -> web::HttpResponse {
    web::HttpResponse::Ok().json(&serde_json::json!({"status":"ok"}))
}
