# CDC relay deployment

Production PostgreSQL uses `zeroship-data-cdc-server`. Local SQLite databases
remain file-backed and capture commits inside the ORM process.

```text
PostgreSQL WAL --> CDC relay -- TLS invalidations --> ORM broker --> Rust / V8
SQLite commit hook -------------------------------> ORM broker
```

Deploy the relay beside PostgreSQL, reachable by workers over TLS. The relay
login is `zeroship_cdc`; the platform migration grants replication and read
access to the enrolled worker identity projection. The worker login is
`zeroship_worker`, with neither replication nor RLS bypass. Publication
membership remains owned by the migration service.

Set `data_cdc_server.database_url`, `listen`, `tls_cert_file`, and `tls_key_file`.
Set `worker.cdc_relay_url` to the complete `wss` subscription endpoint and
`worker.cdc_relay_ca_file` when using a private CA. An empty CA path uses host
trust. Workers authenticate with their enrolled instance keys; the relay checks
the live registry and periodically closes revoked sessions.

For local platform Compose, run from the repository root:

```sh
zeroship dev init
deploy/ops/init-cdc-tls.sh
docker compose -f deploy/compose/docker-compose.yml up -d --build
```

The TLS helper creates a local certificate for `cdc-relay` and keeps a valid
existing pair. Production supplies its own certificate chain and private key.
Workers mount only the public certificate. Configure production role passwords
and DSNs through the deployment's secret manager; the role-name passwords in
the platform corpus are local Compose defaults.

For an existing development deployment, stop workers before applying the
platform migration through `deploy/ops/db-migrate.sh`. Remove inactive old
worker slots as the database operator, scoped to the connected database:

```sql
SELECT pg_drop_replication_slot(slot_name)
FROM pg_replication_slots
WHERE database = current_database()
  AND NOT active
  AND starts_with(slot_name, '__zs_slot_');
```

Inspect any remaining active old slots and stop their owning processes before
continuing. Start the relay and the new workers after the migration. Old worker
binaries require the privileges this migration removes and cannot participate
in a rolling cutover. No worker fallback to direct replication exists.

The relay holds a database advisory lock through a dedicated connection and
exits if that connection is lost. A supervisor restarts it; this is a singleton
deployment without automatic active/standby failover. Capture is shared by
subscription target, which is an app and the database it named, so worker
scaling adds transport connections rather than duplicate slots, while an app
subscribing to two of its databases runs two captures and holds two slots. The
final subscriber releases capture. Startup reclaims inactive relay slots in the
connected database and does not terminate active slots.

Configure finite PostgreSQL `max_slot_wal_keep_size`. Size the cluster slot and
WAL-sender budgets for active captures plus other replication consumers; a
capture is one (app, database) pair, not one app.
Relay `max_apps`, `max_connections`, and `clients_per_app` bound admission;
`queue_capacity` bounds each receiver independently. Transaction and relation
limits bound retained decoding state. Overflow or reconnect causes a fresh
snapshot through `Resync`; delivery is not a durable replay API.

Run `cargo xtask test data`. Each PostgreSQL test owns a
testcontainer with the required extensions and logical WAL.
It builds the relay and exercises TypeScript live queries through TLS using a
worker login without replication privileges. Ordinary tests in
`zeroship-data-cdc-server` cover authentication, revocation, commit ordering,
shared capture, queue overflow, and slot cleanup.
