use std::collections::{BTreeMap, BTreeSet};

use zeroship_core::service_assertion::ROLE_PATH_SEGMENTS;
use zeroship_core::service_identity::{
    authorize, endpoints, service_allowlist, MechanismTag, ServiceEndpoint, ServiceIdentity,
    ServiceName, ServicePrincipal, TrustDomain,
};

/// Every operation the catalog names, in one place for the table-wide guards.
const CATALOG: &[ServiceEndpoint] = &[
    endpoints::GATEWAY_BACKCHANNEL_LOGOUT,
    endpoints::CONTROL_ROUTES,
    endpoints::CONTROL_VERSIONS,
    endpoints::CONTROL_DEPLOYMENT_HOLD_ACQUIRE,
    endpoints::CONTROL_DEPLOYMENT_HOLD_RELEASE,
    endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_ACQUIRE,
    endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_RELEASE,
    endpoints::WORKFLOW_VERIFY_ASSIGNMENT,
    endpoints::WORKFLOW_MANAGE,
    endpoints::WORKFLOW_MANAGEMENT_STATUS,
    endpoints::WORKFLOW_SCHEDULE_REGISTER,
    endpoints::WORKFLOW_SCHEDULE_ACTIVATE,
    endpoints::WORKFLOW_SCHEDULE_DISABLE,
    endpoints::WORKFLOW_REGISTER,
    endpoints::WORKFLOW_ASSIGNMENTS,
    endpoints::WORKFLOW_RENEW,
    endpoints::WORKFLOW_RELEASE,
    endpoints::WORKFLOW_POLICY_LEASE,
    endpoints::WORKFLOW_JOB_SUBMIT,
    endpoints::WORKFLOW_JOB_CLAIM,
    endpoints::WORKFLOW_JOB_HEARTBEAT,
    endpoints::WORKFLOW_JOB_SETTLE,
    endpoints::CONTROL_APP,
    endpoints::CONTROL_APP_ENV,
    endpoints::CONTROL_APP_DATA_KEY,
    endpoints::CONTROL_APP_BINDINGS,
    endpoints::CDC_SUBSCRIBE,
    endpoints::CONTROL_WORKER_RETIRE,
    endpoints::CONTROL_WORKER_RENEW,
    endpoints::CONTROL_BILLING_RECONCILE,
    endpoints::CONTROL_SPEND_RECONCILE,
    endpoints::CONTROL_ERASURE_PREFLIGHT,
    endpoints::WORKER_DISPATCH,
    endpoints::WORKER_APP_LOGS,
    endpoints::MIGRATE_SCHEMA_BUNDLE,
    endpoints::WORKFLOW_JOURNAL_ENSURE,
];

/// The principals the table grants to, which must each own exactly one row.
const PRINCIPALS: [&str; 6] = [
    "svc/control",
    "svc/auth",
    "svc/migrate-server",
    "svc/gateway",
    "svc/worker",
    "svc/workflow",
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
            endpoints::WORKFLOW_VERIFY_ASSIGNMENT,
            "workflow",
            "POST",
            "/v1/assignments/verify",
        ),
        (
            endpoints::CONTROL_DEPLOYMENT_HOLD_ACQUIRE,
            "control",
            "POST",
            "/v1/deployment-holds/acquire",
        ),
        (
            endpoints::CONTROL_DEPLOYMENT_HOLD_RELEASE,
            "control",
            "POST",
            "/v1/deployment-holds/release",
        ),
        (
            endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_ACQUIRE,
            "control",
            "POST",
            "/v1/deployment-holds/queue/acquire",
        ),
        (
            endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_RELEASE,
            "control",
            "POST",
            "/v1/deployment-holds/queue/release",
        ),
        (
            endpoints::WORKFLOW_MANAGE,
            "workflow",
            "POST",
            "/v1/management/enqueue",
        ),
        (
            endpoints::WORKFLOW_MANAGEMENT_STATUS,
            "workflow",
            "POST",
            "/v1/management/status",
        ),
        (
            endpoints::WORKFLOW_REGISTER,
            "workflow",
            "POST",
            "/v1/workers/register",
        ),
        (
            endpoints::WORKFLOW_ASSIGNMENTS,
            "workflow",
            "POST",
            "/v1/assignments/list",
        ),
        (
            endpoints::WORKFLOW_RENEW,
            "workflow",
            "POST",
            "/v1/assignments/renew",
        ),
        (
            endpoints::WORKFLOW_RELEASE,
            "workflow",
            "POST",
            "/v1/assignments/release",
        ),
        (
            endpoints::WORKFLOW_JOB_SUBMIT,
            "workflow",
            "POST",
            "/v1/jobs/submit",
        ),
        (
            endpoints::WORKFLOW_POLICY_LEASE,
            "workflow",
            "POST",
            "/v1/policy/lease",
        ),
        (
            endpoints::WORKFLOW_SCHEDULE_REGISTER,
            "workflow",
            "POST",
            "/v1/schedules/register",
        ),
        (
            endpoints::WORKFLOW_SCHEDULE_ACTIVATE,
            "workflow",
            "POST",
            "/v1/schedules/activate",
        ),
        (
            endpoints::WORKFLOW_SCHEDULE_DISABLE,
            "workflow",
            "POST",
            "/v1/schedules/disable",
        ),
        (
            endpoints::WORKFLOW_JOB_CLAIM,
            "workflow",
            "POST",
            "/v1/jobs/claim",
        ),
        (
            endpoints::WORKFLOW_JOB_HEARTBEAT,
            "workflow",
            "POST",
            "/v1/jobs/heartbeat",
        ),
        (
            endpoints::WORKFLOW_JOB_SETTLE,
            "workflow",
            "POST",
            "/v1/jobs/settle",
        ),
        (
            endpoints::GATEWAY_BACKCHANNEL_LOGOUT,
            "gateway",
            "POST",
            "/oidc/backchannel-logout",
        ),
        (
            endpoints::CONTROL_ROUTES,
            "control",
            "GET",
            "/internal/routes",
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
            endpoints::CONTROL_APP_DATA_KEY,
            "control",
            "GET",
            "/internal/apps/{app_id}/data-key",
        ),
        (
            endpoints::CONTROL_APP_BINDINGS,
            "control",
            "GET",
            "/internal/apps/{app_id}/bindings",
        ),
        (
            endpoints::CONTROL_WORKER_RETIRE,
            "control",
            "POST",
            "/internal/workers/retire",
        ),
        (
            endpoints::CONTROL_WORKER_RENEW,
            "control",
            "POST",
            "/internal/workers/renew",
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

    assert_eq!(service_allowlist().len(), PRINCIPALS.len());
    assert_allowlist_row(
        "svc/control",
        &[
            endpoints::WORKFLOW_VERIFY_ASSIGNMENT,
            endpoints::WORKFLOW_MANAGE,
            endpoints::WORKFLOW_MANAGEMENT_STATUS,
            endpoints::WORKFLOW_SCHEDULE_REGISTER,
            endpoints::WORKFLOW_SCHEDULE_ACTIVATE,
            endpoints::WORKFLOW_SCHEDULE_DISABLE,
            endpoints::WORKER_APP_LOGS,
            // Control learns an app registered; the MANAGER holds the journal
            // artifacts, so Control asks rather than provisioning itself.
            endpoints::WORKFLOW_JOURNAL_ENSURE,
        ],
        all,
    );
    assert_allowlist_row(
        "svc/auth",
        &[
            endpoints::GATEWAY_BACKCHANNEL_LOGOUT,
            endpoints::CONTROL_ERASURE_PREFLIGHT,
        ],
        all,
    );
    assert_allowlist_row("svc/migrate-server", &[], all);
    assert_allowlist_row(
        "svc/workflow",
        &[
            endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_ACQUIRE,
            endpoints::CONTROL_QUEUE_DEPLOYMENT_HOLD_RELEASE,
            // The manager is the ONE principal that may install a platform
            // schema in a creator database, because it is the one that owns the
            // artifacts.
            endpoints::MIGRATE_SCHEMA_BUNDLE,
        ],
        all,
    );
    assert_allowlist_row(
        "svc/gateway",
        &[
            endpoints::CONTROL_ROUTES,
            endpoints::WORKER_DISPATCH,
        ],
        all,
    );
    assert_allowlist_row(
        "svc/worker",
        &[
            endpoints::CONTROL_VERSIONS,
            endpoints::CONTROL_DEPLOYMENT_HOLD_ACQUIRE,
            endpoints::CONTROL_DEPLOYMENT_HOLD_RELEASE,
            endpoints::WORKFLOW_REGISTER,
            endpoints::WORKFLOW_ASSIGNMENTS,
            endpoints::WORKFLOW_RENEW,
            endpoints::WORKFLOW_RELEASE,
            endpoints::WORKFLOW_POLICY_LEASE,
            endpoints::WORKFLOW_JOB_SUBMIT,
            endpoints::WORKFLOW_JOB_CLAIM,
            endpoints::WORKFLOW_JOB_HEARTBEAT,
            endpoints::WORKFLOW_JOB_SETTLE,
            endpoints::CDC_SUBSCRIBE,
            endpoints::CONTROL_APP,
            endpoints::CONTROL_APP_ENV,
            endpoints::CONTROL_APP_DATA_KEY,
            // And the app's resolved database binding, which the worker
            // composes no part of.
            endpoints::CONTROL_APP_BINDINGS,
            // An instance may retire ITSELF on graceful exit, and nothing
            // else: the endpoint takes no selector.
            endpoints::CONTROL_WORKER_RETIRE,
            // The same shape for extending its own lease.
            endpoints::CONTROL_WORKER_RENEW,
            // A host that REFUSED a journal reports it. The worker holds no DDL
            // authority of its own: it names a schema and asks.
            endpoints::WORKFLOW_JOURNAL_ENSURE,
        ],
        all,
    );
}

/// JOINING IS NOT IN THIS TABLE AT ALL, and its absence is the contract rather
/// than an omission.
///
/// A joining process has no service identity yet: it presents a join token a
/// trusted signer minted, under its own `typ`, verified against Control's
/// signer registry. So there is no endpoint constant to grant, no principal to
/// grant it to, and no assertion any process can mint that reaches the join
/// route.
///
/// What CAN be asserted is the consequence: the worker role - the only role a
/// joined instance resolves to - holds exactly the grants a running worker
/// needs and no grant that would let one instance act for another. Renewal and
/// retirement are selector-free by design, so holding them is holding them over
/// oneself.
#[test]
fn no_principal_holds_a_join_grant_and_the_worker_holds_its_own_lifecycle() {
    let worker = identity("zeroship.ai", "svc/worker");
    assert!(authorize(&worker, endpoints::CONTROL_WORKER_RETIRE));
    assert!(authorize(&worker, endpoints::CONTROL_WORKER_RENEW));

    // No endpoint in the table names the join route. A grant that appeared here
    // would mean some assertion could reach it, which is exactly what the join
    // contract exists to prevent.
    for endpoint in CATALOG {
        assert_ne!(
            endpoint.path_template(),
            "/internal/workers/join",
            "joining must have no allowlist endpoint: it takes a token, not an assertion"
        );
    }

    // The control, one variable apart: another role that DOES appear in the
    // table holds neither of the worker's own lifecycle grants, so the two
    // acceptances above are the worker's row rather than a table that grants
    // everything.
    let gateway = identity("zeroship.ai", "svc/gateway");
    assert!(!authorize(&gateway, endpoints::CONTROL_WORKER_RETIRE));
    assert!(!authorize(&gateway, endpoints::CONTROL_WORKER_RENEW));
}

#[test]
fn authorization_keys_on_individual_compound_identity() {
    let control = identity("zeroship.ai", "svc/control");
    let auth = identity("zeroship.ai", "svc/auth");
    let unknown = identity("zeroship.ai", "svc/unknown");
    let wrong_domain = identity("attacker.example", "svc/control");

    assert!(authorize(&control, endpoints::WORKFLOW_MANAGE));
    assert!(!authorize(&auth, endpoints::WORKFLOW_MANAGE));
    assert!(!authorize(&unknown, endpoints::WORKFLOW_MANAGE));
    assert!(!authorize(&wrong_domain, endpoints::WORKFLOW_MANAGE));
}

#[test]
fn retired_management_delivery_paths_have_no_service_grant() {
    let worker = identity("zeroship.ai", "svc/worker");
    assert!(authorize(&worker, endpoints::WORKFLOW_JOB_CLAIM));
    assert!(authorize(&worker, endpoints::WORKFLOW_JOB_SETTLE));
    for path in ["/v1/management/poll", "/v1/management/acknowledge"] {
        assert!(service_allowlist()
            .iter()
            .all(|row| row.endpoints().iter().all(|endpoint| {
                endpoint.destination() != "workflow" || endpoint.path_template() != path
            })));
    }
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

/// Every row names a ROLE, never one instance of a role.
///
/// This is the premise the arity rule in `ServiceIssuer::parse` rests on. That
/// parser reads a role path as a role and one segment more as an INSTANCE of
/// that role, handing the role to the principal either way, so a row written
/// against an instance path would be a grant no verified principal could ever
/// match: an instance's assertion resolves to its role, and equality with the
/// finer name would fail. `matches_principal` stays exact equality precisely
/// because the hierarchy is resolved before it - the row and the principal are
/// both roles by the time they meet.
///
/// Measured over `PRINCIPALS`, which `each_principal_owns_exactly_one_allowlist_row`
/// binds to the table itself: every name there matches exactly one row and the
/// two counts are equal, so no row exists that this loop did not rule on.
///
/// The bound comes from the parser rather than a literal, so moving the rule
/// re-rules the table against the new one instead of silently agreeing with the
/// old one.
#[test]
fn every_allowlist_row_names_a_role_and_not_an_instance_of_one() {
    for name in PRINCIPALS {
        assert_eq!(
            name.split('/').count(),
            ROLE_PATH_SEGMENTS,
            "{name} is not a role path, so no assertion an instance of it mints could match \
             this row"
        );
    }
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
    assert!(!authorize(&gateway, endpoints::CONTROL_BILLING_RECONCILE));
    assert!(!authorize(&gateway, endpoints::CONTROL_SPEND_RECONCILE));
}
