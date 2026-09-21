//! The routes this process SERVES, held to the declarations that admit their
//! callers.
//!
//! The gateway dispatches to `WORKER_DISPATCH` and control reads logs at
//! `WORKER_APP_LOGS`, and each handler hands its own declaration to
//! `check_worker_auth`. Registering either path as a written-out literal would
//! make the route the worker serves and the route it authorizes two copies: a
//! disagreement would 404 exactly the callers the allowlist admits, and every
//! test that mounts its own copy of the route would stay green through it.
//!
//! # What these do not catch
//!
//! Nothing here reads a path. A registration re-spelled as the literal the
//! declaration happens to hold today still passes, because the two agree - what
//! it costs is the guarantee that they go on agreeing.

use super::*;
use ntex::http::Method;
use zeroship_core::service_identity::{endpoints, ServiceEndpoint};

/// Every declared route this process registers.
///
/// The ENDPOINTS, never their paths: each registration reads its path from
/// `zeroship_core::service_identity::endpoints`, and a path written out here
/// would be one more copy of the thing under test.
const SERVED: [ServiceEndpoint; 2] = [endpoints::WORKER_DISPATCH, endpoints::WORKER_APP_LOGS];

/// What every path parameter is filled with.
///
/// Any one segment does: these arms rule on ROUTING alone, and both handlers
/// refuse an unauthenticated caller before they parse a segment.
const SEGMENT: &str = "routing-probe";

/// The service name this process answers for.
const SERVED_DESTINATION: &str = "worker";

/// One declared template with every path parameter filled.
///
/// Derived from the SHAPE of the template - its `{...}` spans - rather than
/// from any parameter's name, so this and `web::resource` reach the served path
/// by different routes and a renamed parameter separates them.
fn addressed(endpoint: ServiceEndpoint) -> String {
    let template = endpoint.path_template();
    let mut addressed = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let close = open
            + rest[open..]
                .find('}')
                .unwrap_or_else(|| panic!("{template} closes every path parameter"));
        addressed.push_str(&rest[..open]);
        addressed.push_str(SEGMENT);
        rest = &rest[close + 1..];
    }
    addressed.push_str(rest);
    addressed
}

/// The method one endpoint declares, as a method a request can carry.
fn declared_method(endpoint: ServiceEndpoint) -> Method {
    Method::from_bytes(endpoint.method().as_bytes())
        .unwrap_or_else(|_| panic!("{} declares an HTTP method", endpoint.method()))
}

/// A method the endpoint does NOT declare.
fn undeclared_method(endpoint: ServiceEndpoint) -> Method {
    if declared_method(endpoint) == Method::GET {
        Method::POST
    } else {
        Method::GET
    }
}

#[compio::test]
async fn every_declared_worker_route_is_served_under_its_declared_method() {
    assert!(
        !SERVED.is_empty(),
        "an empty table would pass every assertion below"
    );
    // NO state. What is under test is the router: a request that reaches a
    // handler fails its `State` extractor, which is neither of the two statuses
    // these arms read, so a route that matched stays distinguishable from one
    // that did not without loading an app.
    let app = test::init_service(
        web::App::new()
            .configure(crate::handler::configure)
            .configure(crate::logs::configure),
    )
    .await;

    for endpoint in SERVED {
        assert_eq!(
            endpoint.destination(),
            SERVED_DESTINATION,
            "{} is not a route this process serves",
            endpoint.path_template()
        );
        let path = addressed(endpoint);
        assert!(
            !path.contains('{') && !path.contains('}'),
            "the declared parameter must be filled rather than addressed as a \
             literal: {path}"
        );

        let served = test::call_service(
            &app,
            test::TestRequest::default()
                .method(declared_method(endpoint))
                .uri(&path)
                .to_request(),
        )
        .await;
        assert_ne!(
            served.status(),
            StatusCode::NOT_FOUND,
            "{path} is declared and must be served"
        );
        assert_ne!(
            served.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{path} must answer the method its declaration names"
        );

        // The control on the method: the SAME path under a method the
        // declaration does not name is refused as a method, so the arm above is
        // the declared method matching rather than the resource answering
        // anything sent to it.
        let wrong_method = test::call_service(
            &app,
            test::TestRequest::default()
                .method(undeclared_method(endpoint))
                .uri(&path)
                .to_request(),
        )
        .await;
        assert_eq!(
            wrong_method.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{path} must answer only the method its declaration names"
        );

        // The control on the path: one segment past the declared path is not
        // routed, so the arm above is this path matching rather than a prefix
        // that swallows everything under it.
        let beyond = format!("{path}/{SEGMENT}");
        let unrouted = test::call_service(
            &app,
            test::TestRequest::default()
                .method(declared_method(endpoint))
                .uri(&beyond)
                .to_request(),
        )
        .await;
        assert_eq!(
            unrouted.status(),
            StatusCode::NOT_FOUND,
            "{beyond} is not a declared route and must not be served"
        );
    }
}
