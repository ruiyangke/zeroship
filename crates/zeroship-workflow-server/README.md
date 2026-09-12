# zeroship-workflow-server

The [revised target](../../docs/proposals/2026-09-11-workflow-worker.md) is a
lightweight registry, placement and management coordinator. Customer workers
own workflow history, task claims and payload storage. The data-owning HTTP
host described below is the current refactor prototype; it has not replaced the
deployed worker path and must be reshaped before cutover. Its customer-data
routes and platform journal are not part of the target coordinator contract.
The closed metadata messages live in `zeroship_core::workflow_coordination`.
They separate registration, placement, wake-up hints and lifecycle management
from execution data. Native wire tests reject customer payload and credential
fields, including inside nested command acknowledgements. Handler and store
composition are still being replaced.

The V8-free HTTP host for `zeroship-workflow::service`. App routes require a
Control-issued capability. Task routes verify an active enrolled worker's
signature and bind the service's task token to that instance. Operator deploy
notifications use the platform service allowlist. They carry no deployment
selection: the service reads Control's current immutable snapshot under the
app's transaction lock. New starts and schedule sweeps also reconcile that
selection, so delayed notifications cannot restore older code or schedules.

Streaming payload routes keep upload identities and task tokens in Rust. Clients
apply bounded backpressure and independently verify downloaded content; retries
reuse the durable upload receipt and cancel an unused source.

The executable loads an immutable signing and verification snapshot, verifies
the migrated database authority and uses PostgreSQL for assertion replay
protection across replicas. Maintenance delivers Control's durable deploy
notifications and drives schedules, topic delivery and payload collection.
Reconciliation acknowledges the notification in its journal transaction.
JSON handlers authenticate before buffering request bodies.
The deployed worker and Control cutover remain in progress.

- `src/api.rs` maps typed app and task requests onto the shared service.
- `src/auth.rs` verifies app grants, service peers and enrolled worker identities.
- `src/config.rs` declares `[workflow]` TOML settings and their CLI overrides.
- `src/server.rs` composes the platform policy, payload backend and HTTP listener.
- `tests/http.rs` runs native clients against real HTTP listeners and
  Testcontainers PostgreSQL, including cross-replica retries and revocation.
- `tests/platform_schema.rs` applies the platform migration corpus and exercises
  separate server processes, shared assertion replay and restart recovery.

Run `cargo test -p zeroship-workflow-server` for the host contracts.
`zeroship-workflow-server --config zeroship.toml --check-config` validates
configuration without connecting to dependencies. `/healthz` reports process
liveness; `/readyz` verifies database authority and worker-registry access.
