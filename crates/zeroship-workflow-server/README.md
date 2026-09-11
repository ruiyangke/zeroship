# zeroship-workflow-server

The V8-free HTTP host for `zeroship-workflow::service`. App routes require a
Control-issued capability. Task routes verify an active enrolled worker's
signature and bind the service's task token to that instance. Operator deploy
notifications use the platform service allowlist.

Streaming payload routes keep upload identities and task tokens in Rust. Clients
apply bounded backpressure and independently verify downloaded content; retries
reuse the durable upload receipt and cancel an unused source.

The executable loads an immutable signing and verification snapshot, verifies
the migrated database authority and uses PostgreSQL for assertion replay
protection across replicas. Maintenance drives schedules, topic delivery and
payload collection. JSON handlers authenticate before buffering request bodies.
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
