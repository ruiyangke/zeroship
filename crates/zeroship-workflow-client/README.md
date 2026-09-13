# zeroship-workflow-client

Authenticated workflow metadata transport for native hosts. Worker and Control
clients validate assignment and receipt identity, mint fresh service assertions,
bound requests and responses, and use compio HTTP connections.

This crate depends on shared wire types, not the customer engine, manager, ORM
or V8. Remote origins require HTTPS; literal loopback HTTP supports local hosts.
Mutation callers retain their logical request identity across uncertain replies.

`WorkerCoordinator` submits creator intents, claims jobs, heartbeats live grants
and settles creator-committed outcomes. `LeasedJob` validates response identity
and holds a local monotonic deadline derived from remaining manager authority,
charging the full request exchange. A late heartbeat cannot revive an expired
grant. The executor must also enforce its original hard execution deadline.
Exact settlement receipts remain retryable after local lease expiry.

`WorkerCoordinator::policy_lease` accepts an app assignment and verifies the
returned app, worker, exact signing-key thumbprint and assignment revision. It
validates the complete shared raw policy and rejects a remaining duration beyond
that policy's lease ceiling. `LeasedPolicy` anchors expiration before transport;
cloning preserves that deadline. The host installs it using the refresh ticket
reserved before the request. A valid client response cannot revive a replaced
host binding, and unavailable source authority never selects default policy.

`ControlCoordinator::register_schedules` prepares input-free metadata and checks
the complete accepted declaration. `activate_schedules` checks the returned job's
app, deployment, operation and activation revision. These publication methods
require the exact Control service signer. Callers preserve the original command
and revision after an uncertain reply; an activation receipt means durable
manager acceptance, while creator readiness is a separate job outcome.

`disable_schedules` checks the exact accepted app and revision. A historical
disable receipt remains valid after restore without disabling the newer revision.
This operation acknowledges calendar publication fencing, not creator policy or
executor quiescence.

Run `cargo test -p zeroship-workflow-client` for wire, cancellation, timeout and
TLS contracts.
