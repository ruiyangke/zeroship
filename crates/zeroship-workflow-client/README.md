# zeroship-workflow-client

Authenticated workflow metadata transport for native hosts. Worker and Control
clients validate assignment and receipt identity, mint fresh service assertions,
bound requests and responses, and use compio HTTP connections.

This crate depends on shared wire types, not the customer engine, manager, ORM
or V8. Remote origins require HTTPS; literal loopback HTTP supports local hosts.
Mutation callers retain their logical request identity across uncertain replies.

Run `cargo test -p zeroship-workflow-client` for wire, cancellation, timeout and
TLS contracts.
