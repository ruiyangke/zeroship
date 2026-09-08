use std::collections::{BTreeMap, BTreeSet};

use zeroship_core::service_identity::{
    authorize, endpoints, service_allowlist, MechanismTag, ServiceEndpoint, ServiceIdentity,
    ServiceName, ServicePrincipal, TrustDomain,
};

/// Every operation the catalog names, in one place for the table-wide guards.
const CATALOG: [ServiceEndpoint; 14] = [
    endpoints::GATEWAY_BACKCHANNEL_LOGOUT,
    endpoints::GATEWAY_WORKFLOW_ADVANCE,
    endpoints::CONTROL_ROUTES,
    endpoints::CONTROL_WORKFLOW_SIGNAL_INGRESS,
    endpoints::CONTROL_VERSIONS,
    endpoints::CONTROL_APP,
    endpoints::CONTROL_APP_ENV,
    endpoints::CONTROL_WORKER_ENROL,
    endpoints::CONTROL_BILLING_RECONCILE,
    endpoints::CONTROL_SPEND_RECONCILE,
    endpoints::CONTROL_ERASURE_PREFLIGHT,
    endpoints::WORKER_DISPATCH,
    endpoints::WORKER_WORKFLOW_ADVANCE,
    endpoints::WORKER_APP_LOGS,
];

/// The principals the table grants to, which must each own exactly one row.
const PRINCIPALS: [&str; 5] = [
    "svc/control",
    "svc/auth",
    "svc/migrate-server",
    "svc/gateway",
    "svc/worker",
];

fn identity(trust_domain: &str, name: &str) -> ServiceIdentity {
    ServiceIdentity::new(
        ServicePrincipal::new(TrustDomain::new(trust_domain), ServiceName::new(name)),
        MechanismTag::new("test-stub"),
        BTreeMap::new(),
    )
}

fn assert_allowlist_row(name: &str, expected: &[ServiceEndpoint], all: &[ServiceEndpoint]) {
    let identity = identity("zeroship.ai", name);
    let row = service_allowlist()
        .iter()
        .find(|row| row.applies_to(&identity))
        .expect("known identity has an explicit allowlist row");

    assert_eq!(row.endpoints(), expected);
    for endpoint in all {
        assert_eq!(
            authorize(&identity, *endpoint),
            expected.contains(endpoint),
            "unexpected authorization for {name} at {endpoint:?}"
        );
    }
}

fn assert_endpoint(
    endpoint: ServiceEndpoint,
    destination: &str,
    method: &str,
    path_template: &str,
) {
    assert_eq!(endpoint.destination(), destination);
    assert_eq!(endpoint.method(), method);
    assert_eq!(endpoint.path_template(), path_template);
}

#[test]
fn endpoint_catalog_records_exact_measured_operations() {
    for (endpoint, destination, method, path_template) in [
        (
            endpoints::GATEWAY_BACKCHANNEL_LOGOUT,
            "gateway",
            "POST",
            "/oidc/backchannel-logout",
        ),
        (
            endpoints::GATEWAY_WORKFLOW_ADVANCE,
            "gateway",
            "POST",
            "/__zeroship/internal/workflow-advance",
        ),
        (
            endpoints::CONTROL_ROUTES,
            "control",
            "GET",
            "/internal/routes",
        ),
        (
            endpoints::CONTROL_WORKFLOW_SIGNAL_INGRESS,
            "control",
            "POST",
            "/internal/workflows/signals/ingress",
        ),
        (
            endpoints::CONTROL_VERSIONS,
            "control",
            "GET",
            "/internal/versions",
        ),
        (
            endpoints::CONTROL_APP,
            "control",
            "GET",
            "/internal/apps/{app_id}",
        ),
        (
            endpoints::CONTROL_APP_ENV,
            "control",
            "GET",
            "/internal/apps/{app_id}/env",
        ),
        (
            endpoints::CONTROL_WORKER_ENROL,
            "control",
            "POST",
            "/internal/workers/enrol",
        ),
        (
            endpoints::CONTROL_BILLING_RECONCILE,
            "control",
            "POST",
            "/internal/billing/reconcile",
        ),
        (
            endpoints::CONTROL_SPEND_RECONCILE,
            "control",
            "POST",
            "/internal/spend/reconcile",
        ),
        (
            endpoints::CONTROL_ERASURE_PREFLIGHT,
            "control",
            "GET",
            "/internal/principals/{principal_id}/erasure-preflight",
        ),
        (
            endpoints::WORKER_DISPATCH,
            "worker",
            "POST",
            "/dispatch/{app_id}",
        ),
        (
            endpoints::WORKER_WORKFLOW_ADVANCE,
            "worker",
            "POST",
            "/workflow-advance-unsigned/{app_id}",
        ),
        (
            endpoints::WORKER_APP_LOGS,
            "worker",
            "GET",
            "/logs/{app_id}",
        ),
    ] {
        assert_endpoint(endpoint, destination, method, path_template);
    }
}

#[test]
fn measured_allowlist_is_encoded_and_enforced_row_by_row() {
    let all = CATALOG;

    assert_eq!(service_allowlist().len(), 5);
    assert_allowlist_row(
        "svc/control",
        &[
            endpoints::GATEWAY_WORKFLOW_ADVANCE,
            endpoints::WORKER_APP_LOGS,
        ],
        &all,
    );
    assert_allowlist_row(
        "svc/auth",
        &[
            endpoints::GATEWAY_BACKCHANNEL_LOGOUT,
            endpoints::CONTROL_ERASURE_PREFLIGHT,
        ],
        &all,
    );
    assert_allowlist_row("svc/migrate-server", &[], &all);
    assert_allowlist_row(
        "svc/gateway",
        &[
            endpoints::CONTROL_ROUTES,
            endpoints::CONTROL_WORKFLOW_SIGNAL_INGRESS,
            endpoints::WORKER_DISPATCH,
            endpoints::WORKER_WORKFLOW_ADVANCE,
        ],
        &all,
    );
    assert_allowlist_row(
        "svc/worker",
        &[
            endpoints::CONTROL_VERSIONS,
            endpoints::CONTROL_APP,
            endpoints::CONTROL_APP_ENV,
            // Enrolment authenticates with the SHARED role key, so this grant
            // DISTINGUISHES instances rather than fencing a role-key holder
            // out. It is in the row because per-instance identity is what makes
            // narrowing the two app reads above writable at all.
            endpoints::CONTROL_WORKER_ENROL,
        ],
        &all,
    );
}

#[test]
fn authorization_keys_on_individual_compound_identity() {
    let control = identity("zeroship.ai", "svc/control");
    let auth = identity("zeroship.ai", "svc/auth");
    let unknown = identity("zeroship.ai", "svc/unknown");
    let wrong_domain = identity("attacker.example", "svc/control");

    assert!(authorize(&control, endpoints::GATEWAY_WORKFLOW_ADVANCE));
    assert!(!authorize(&auth, endpoints::GATEWAY_WORKFLOW_ADVANCE));
    assert!(!authorize(&unknown, endpoints::GATEWAY_WORKFLOW_ADVANCE));
    assert!(!authorize(
        &wrong_domain,
        endpoints::GATEWAY_WORKFLOW_ADVANCE
    ));
}

/// `authorize` resolves a principal to the FIRST matching row, so a second row
/// for the same principal would silently discard every grant below it. Nothing
/// in the type of the table prevents that, and the table is expected to grow.
#[test]
fn each_principal_owns_exactly_one_allowlist_row() {
    for name in PRINCIPALS {
        let identity = identity("zeroship.ai", name);
        let rows = service_allowlist()
            .iter()
            .filter(|row| row.applies_to(&identity))
            .count();

        assert_eq!(rows, 1, "{name} must own exactly one allowlist row");
    }
    assert_eq!(service_allowlist().len(), PRINCIPALS.len());
}

/// Endpoints are matched by value, not by constant, so two catalog entries
/// carrying the same destination, method and path template would be one
/// operation under two names: granting either would silently grant both.
#[test]
fn catalog_operations_are_pairwise_distinct() {
    let distinct: BTreeSet<ServiceEndpoint> = CATALOG.iter().copied().collect();

    assert_eq!(distinct.len(), CATALOG.len());
}

#[test]
fn gateway_cannot_reach_env_or_reconcile_endpoints() {
    let gateway = identity("zeroship.ai", "svc/gateway");

    assert!(!authorize(&gateway, endpoints::CONTROL_APP_ENV));
    assert!(!authorize(
        &gateway,
        endpoints::CONTROL_BILLING_RECONCILE
    ));
    assert!(!authorize(
        &gateway,
        endpoints::CONTROL_SPEND_RECONCILE
    ));
}
