# PostgreSQL CDC relay

`zeroship-data-cdc-server` owns logical replication in a process that never
executes creator code. It verifies enrolled worker identities, shares capture by
app, buffers rows until commit, and sends bounded, value-free invalidations over
TLS using `zeroship-data-cdc-wire`.

The ORM client and subscription lifecycle live in `zeroship-data-orm::cdc`.
Workers do not link the relay implementation or hold replication credentials.
SQLite local development uses embedded commit capture without this service.

`server.rs` owns transport and process supervision; `auth.rs` verifies registry
identities; `source.rs` owns slots and replication acknowledgements;
`transaction.rs` enforces commit buffering; `hub.rs` bounds shared delivery.

Run `cargo test -p zeroship-data-cdc-server` with the required PostgreSQL fixture.
See `docs/runbooks/cdc-relay.md` for provisioning, TLS and deployment.
