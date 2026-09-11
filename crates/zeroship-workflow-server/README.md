# zeroship-workflow-server

The V8-free HTTP host for `zeroship-workflow::service`. App routes require a
Control-issued capability. Task routes verify an active enrolled worker's
signature and bind the service's task token to that instance. Operator deploy
notifications use the platform service allowlist.

The host library and Rust remote clients are implemented. Startup configuration,
platform policy composition and the deployed worker cutover remain in progress.

- `src/api.rs` maps typed app and task requests onto the shared service.
- `src/auth.rs` verifies app grants, service peers and enrolled worker identities.
- `tests/http.rs` runs native clients against real HTTP listeners and
  Testcontainers PostgreSQL, including cross-replica retries and revocation.

Run `cargo test -p zeroship-workflow-server` for the HTTP contracts.
