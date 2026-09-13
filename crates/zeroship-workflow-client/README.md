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

Run `cargo test -p zeroship-workflow-client` for wire, cancellation, timeout and
TLS contracts.
