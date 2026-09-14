//! Liveness and readiness routes for the control plane.

use std::sync::Arc;

use ntex::web;
use ntex::web::types::State;
use zeroship_core::readiness::ReadinessGate;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(web::resource("/healthz").route(web::get().to(healthz)))
        .service(web::resource("/readyz").route(web::get().to(readyz)));
}

/// Liveness must not touch Postgres. Restarting the process cannot repair a
/// database outage.
pub async fn healthz() -> web::HttpResponse {
    web::HttpResponse::Ok().json(&serde_json::json!({"ok": true}))
}

/// The control plane cannot serve requests without Postgres. Probe the shared
/// client through the process-wide bounded, cached readiness gate.
pub async fn readyz(
    db: State<Arc<compio_postgres::Client>>,
    gate: State<Arc<ReadinessGate>>,
) -> web::HttpResponse {
    if zeroship_core::config::dev_escape_active() {
        return readiness_response(false);
    }
    let ready = gate
        .ready(|| async {
            match db.check_connection().await {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(error = %error, "control readiness: postgres unreachable");
                    false
                }
            }
        })
        .await;
    readiness_response(ready)
}

fn readiness_response(ready: bool) -> web::HttpResponse {
    if ready {
        web::HttpResponse::Ok().json(&serde_json::json!({"ready": true}))
    } else {
        web::HttpResponse::ServiceUnavailable().json(&serde_json::json!({"ready": false}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntex::http::StatusCode;
    use ntex::web::test;
    use std::time::Duration;

    async fn status(
        app: &ntex::Pipeline<
            impl ntex::Service<
                ntex::http::Request,
                Response = ntex::web::WebResponse,
                Error = ntex::web::Error,
            >,
        >,
        path: &str,
    ) -> (StatusCode, String) {
        let response = test::call_service(
            app,
            test::TestRequest::get().uri(path).to_request(),
        )
        .await;
        let status = response.status();
        let body = test::read_body(response).await;
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[compio::test]
    async fn routes_use_the_owned_postgres_client() {
        let database_url = crate::test_database::url();
        let (client, connection) =
            compio_postgres::connect(&database_url, compio_postgres::NoTls)
                .await
                .expect("connect control health fixture");
        let connection = compio::runtime::spawn(async move { connection.run().await });
        let client = Arc::new(client);
        let gate = Arc::new(ReadinessGate::new(Duration::ZERO, Duration::from_secs(2)));
        let app = test::init_service(
            web::App::new()
                .state(client.clone())
                .state(gate)
                .configure(configure),
        )
        .await;

        assert_eq!(status(&app, "/healthz").await.0, StatusCode::OK);
        assert_eq!(status(&app, "/readyz").await.0, StatusCode::OK);
        let retired = status(&app, "/health").await.0;
        let unknown = status(&app, "/route-that-does-not-exist").await.0;
        assert_ne!(retired, StatusCode::OK);
        assert_eq!(retired, unknown, "the retired alias must not be a route");

        drop(app);
        drop(client);
        connection
            .await
            .expect("control health connection task")
            .expect("control health connection");
    }

    #[test]
    fn readiness_failure_is_generic() {
        let response = readiness_response(false);
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
