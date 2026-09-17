# KV runtime configuration

**Status:** implemented. Backend and topology are selected when the host
starts.

This page is the host side of the KV contract; [kv.md](../reference/kv.md) is the SDK side.
Your app never selects a backend, a server, a logical database, or another
app's namespace. The host chooses the backend and hands your app a scoped
`env.kv` handle, and the SDK behaves identically on every backend. The config
keys, backend names, defaults, and limits below are part of the surface your
app can rely on.

Two backends sit behind the one configuration document:

```text
Host configuration
        |
        v
    KV backend
        |
        +-- redb  --> local file, single process
        |
        +-- redis --> standalone endpoint
                   --> cluster (seeds) --> slot owner
                   --> Sentinel    --> current primary
```

`redb` is the embedded, single-process store that local development uses by
default. `redis` is the Redis-compatible backend: Redis and Dragonfly are both
served by it, in one of three topologies — a standalone endpoint, a cluster
reached through seed nodes, or a Sentinel deployment that resolves the current
primary.

## Loading configuration

The host owns configuration and credentials; your app receives a scoped handle
and the same operations and namespace enforcement on every deployment.

**Local development** uses the embedded store with no configuration: the dev
runtime opens a local file at `.zeroship/kv.redb`. Two optional knobs override
it:

- `ZEROSHIP_KV_PATH` re-points that local file.
- `ZEROSHIP_KV_CONFIG_FILE` names a TOML file that selects a non-default
  backend.

**Deployed workers** receive the configuration as secret material, because it
can carry data-server and Sentinel credentials. A worker requires shared
Redis-compatible storage; supplying the embedded file tier there is an error,
and an absent configuration leaves `env.kv` unavailable.

Configuration files must be owner-only: they can contain credentials, and a
file readable by group or other is refused. Their contents are secret-classed
and redacted from configuration reports.

Changing configuration requires restarting the host, and changing the backend
does not migrate its data. A configuration is validated when it is loaded —
unknown keys, a conflicting topology, an invalid endpoint, or an invalid pool
or timeout are errors — but a Redis connection is opened lazily on first use,
so a configuration that parses is not a guarantee that the backend is
reachable.

## Embedded storage

```toml
backend = "redb"
path = ".zeroship/kv.redb"
```

The embedded store is one file under an exclusive lock, so it is single-process
by design. It is the local-development and single-host tier; multi-process
deployments use the shared backend. Local dev state lives under `.zeroship/`
and survives restarts, like the SQLite dev database.

## Standalone Redis or Dragonfly

```toml
backend = "redis"

[redis.topology]
mode = "standalone"
endpoint = "redis.internal:6379"
```

Endpoints are `host:port`, with bracketed IPv6 supported. Credentials belong in
`redis.auth`; URLs and URL query switches are not accepted as topology
endpoints.

## Cluster

```toml
backend = "redis"

[redis.topology]
mode = "cluster"
seeds = ["redis-a.internal:6379", "redis-b.internal:6379"]
```

The driver discovers the slot map, routes commands to the owning primary,
follows redirects, and refreshes topology through the configured seeds and the
nodes it has already discovered. Advertised node addresses must be reachable
from the worker; an address that is neither a configured seed nor a node in a
verified topology is refused before credentials are sent to it.

Each app's keys carry a hash tag that places them all in the same slot, so one
app lives on one shard. Listing routes to the app's slot owner and returns only
keys in the app's namespace. A listing is not a snapshot; tolerate concurrent
writes and topology changes while paging through it.

Dragonfly's emulated cluster accepts this mode or a standalone connection; its
multi-shard deployment uses cluster mode. Provisioning, replication, failover,
and rebalancing of a Dragonfly cluster belong to infrastructure management;
the driver consumes the resulting topology. See the
[Dragonfly cluster contract](https://www.dragonflydb.io/docs/managing-dragonfly/cluster-mode).

## Sentinel

```toml
backend = "redis"

[redis.topology]
mode = "sentinel"
endpoints = ["sentinel-a.internal:26379", "sentinel-b.internal:26379"]
service_name = "kv"

[redis.auth]
username = "kv-service"
password = "replace-with-data-server-secret"

[redis.sentinel_auth]
username = "kv-discovery"
password = "replace-with-sentinel-secret"
```

Sentinel endpoint addresses identify discovery servers. `service_name` names
the monitored primary group, not an app namespace. Data commands go directly
to the discovered primary. Separate Sentinel processes can manage either Redis
or Dragonfly primary/replica deployments.

New connections resolve the primary through Sentinel and verify its role. When
discovery reports a different primary, the previous connections are discarded.
Discovery failures return errors; they never fall back to a configured replica
or another backend. This follows the
[Sentinel client discovery contract](https://redis.io/docs/latest/develop/reference/sentinel-clients/).

## Connection settings

Optional settings apply independently of the chosen topology:

| Setting | Meaning | Default |
| --- | --- | --- |
| `redis.auth.username`, `redis.auth.password` | Data-server ACL identity, or password-only authentication | none |
| `redis.tls` | Enable verified TLS for every data connection | disabled |
| `redis.sentinel_auth` | Separate Sentinel credentials | none |
| `redis.sentinel_tls` | Separate TLS settings for Sentinel connections | none |
| `redis.database` | Operator-selected logical database; cluster requires the default | `0` |
| `redis.timeouts.connect_ms` | Deadline for the DNS, TCP, TLS, authentication, and database-selection steps of one connection attempt | `5000` |
| `redis.timeouts.command_ms` | Deadline for one command | `5000` |
| `redis.pool.max_size` | Connection limit per node | `16` |
| `redis.pool.min_idle` | Connections opened when a pool initializes; may defer to checkout | `1` |
| `redis.pool.idle_timeout_ms` | Age after which an idle connection is discarded | `600000` |
| `redis.pool.liveness_probe_after_ms` | Age after which an idle connection is probed before reuse | `30000` |

Logical-database selection is a placement option for the operator; app
isolation always comes from the host-bound namespace, never from the logical
database.

For verified TLS with a private CA:

```toml
[redis.tls]
ca_file = "/etc/zeroship/redis-ca.pem"
```

An empty TLS table enables TLS with the system trust store. `cert_file` and
`key_file` enable client-certificate authentication and must be supplied
together. `server_name` sets the certificate identity to verify when discovery
returns an IP address; without it the endpoint's own identity is verified. The
same fields apply under `redis.sentinel_tls`. Certificate verification cannot
be disabled. Certificate paths resolve relative to the host process working
directory; use absolute paths in deployed configuration.

## Recovery and validation

A configuration is rejected before use when it is malformed:

- The backend must be `redb` or `redis`; unknown keys are rejected.
- A topology is exactly one of `standalone` (an `endpoint`), `cluster`
  (`seeds`), or `sentinel` (`endpoints` plus `service_name`); missing or
  conflicting fields are rejected.
- Endpoints must be `host:port` with no credentials, path, or query, and a
  non-empty port; an empty list of seeds or Sentinel endpoints is rejected.
- A Sentinel `service_name` is required.
- Cluster requires the default logical database.
- An ACL username requires a password.
- `cert_file` and `key_file` must be supplied together.
- Sentinel connection settings are rejected with a non-Sentinel topology.
- Timeouts and `pool.max_size` must be non-zero, and `min_idle` must not
  exceed `max_size`.

A transport failure after a mutation has an ambiguous outcome: the operation
may or may not have applied, and it is not replayed. A redirect, or a rejection
from a demoted primary, can be retried because the command was refused on the
old destination. Primary routing does not make asynchronous replication
lossless, and it does not provide exactly-once execution across a failover;
see the `kv_connection` retry guidance in [kv.md](../reference/kv.md).