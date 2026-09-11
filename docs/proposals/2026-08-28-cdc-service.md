# CDC relay extraction

PostgreSQL capture belongs in `zeroship-data-cdc-server`, a separately deployed
process that never executes creator code. SQLite is the embedded, file-backed
local-development backend and captures commits inside the application process.

```text
PostgreSQL WAL
    |
    v
zeroship-data-cdc-server
    |  committed, value-free invalidations over TLS
    |  zeroship-data-cdc-wire
    v
zeroship-data-orm::cdc::relay
    |
    v
ORM subscription broker <--- SQLite commit capture (local development)
    |
    +--- Rust subscriptions
    +--- zeroship-data-v8 ---> TypeScript live queries
```

The extraction is in progress. The relay source, transport, and native ORM client are implemented.
V8 integration, worker privilege removal, and deployment provisioning must land
with the worker cutover. An executable relay does not establish the privilege boundary
while a worker can still replicate WAL itself.

## Contracts

`zeroship-data-orm::cdc` owns domain events, subscription routing, read sets, and
consumer lifecycle. `zeroship-data-cdc-wire` owns the bounded binary subscription
request and the relay events. It has no I/O, driver, ORM, or V8 dependency. The
relay implementation is private to its executable; workers never link it.

A connection subscribes to the current app schema. The earlier proposed wire
format required datastore, database, grant, and epoch authorities that the
runtime does not produce. Those unused contracts have been removed. A future
database-decoupling change must introduce actual authority producers and update
the protocol deliberately; CDC does not manufacture placeholder identities.

The relay sends collection names and operations, never primary keys or row
values. `pk` is therefore null and `columns` is empty in PostgreSQL subscription
notifications. Workers re-read data through the ORM's normal access controls.
Row predicates cannot narrow these invalidations; collection routing still does.
SQLite can continue using locally captured row images for predicate matching.

## Readiness and delivery

The first subscriber starts capture for an app. Other worker connections share
that source. The relay sends `Ready` only after PostgreSQL accepts replication.
Workers subscribe before taking their initial snapshot.

Rows are buffered until commit. Rollback produces no invalidation. If a
transaction exceeds the configured retained-byte or change bound, its commit
produces `Resync` instead of a partial change batch. Truncate also requests a
resnapshot. Relation metadata has a separate capacity bound.

Delivery queues are bounded independently for each connection. Queue overflow
closes the lagging connection without blocking healthy subscribers. Every
reconnect requires a fresh snapshot; there is no durable event replay or exactly
once delivery promise. The client also requests resync when connectivity is lost.

Only delivered commit positions advance replication acknowledgements. A server
WAL tip advertised in an XLogData or keepalive message is not an acknowledgement
of delivered transactions.

## Process and database ownership

Deploy a relay process for each PostgreSQL database used by this app-schema
layout. A dedicated PostgreSQL session holds an advisory lock for singleton
ownership. Failure of that session stops the process; it must not reconnect the
lock behind live capture tasks. This is singleton supervision, not automated
active/standby failover.

Slots are keyed by app, shared across worker connections, and owned exclusively
by the relay. The final subscriber stops capture and drops its slot. Startup
reclaims inactive relay slots for the connected database. Active slots are never
terminated by this cleanup. Configure finite `max_slot_wal_keep_size` so a crash
cannot retain unbounded WAL.

The relay login requires `REPLICATION`, access to enrolled worker public keys,
and ordinary catalog access. It refuses superuser, `BYPASSRLS`, `CREATEROLE`, and
`CREATEDB`. Publication membership remains migration-owned. The relay does not
provision publications, app tables, policies, or database roles at startup.

The worker login must lose `REPLICATION` and `BYPASSRLS` when deployment switches
to the relay. Platform role provisioning belongs in the sanctioned migration
corpus. Platform role provisioning remains pending; migration files have not been
edited for this extraction.

## Authentication

The transport requires TLS and an assertion addressed to `svc/cdc`. Only an
active enrolled worker instance with the CDC endpoint grant may subscribe. The
relay selects its key from the live worker registry and then verifies the full
assertion; parsing an issuer alone never authenticates it. There is no fallback
to the fleet role key. Open streams periodically recheck registry status and
public-key identity, closing on revocation or an unavailable registry.

This follows the platform's current worker authority: an enrolled worker may
serve apps across the deployment. Per-app placement capabilities and independent
datastore grants are separate platform work, not authorities this service claims
to have implemented.

## Verification

Ordinary relay tests require a real PostgreSQL database. They cover the commit
boundary, rollback, shared delivery, value-free frames, truncate, and slot cleanup.
Wire and queue tests cover malformed messages, capacity exhaustion, slow-client
isolation, and generation fencing. The public ORM client is tested against a separate relay process, including
authentication, revocation, and shared capture. Worker-cutover coverage remains
required before the extraction is complete.
