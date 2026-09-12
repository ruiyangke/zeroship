# KV runtime configuration

**Status:** implemented. Backend and Redis topology selection happen when the host starts.

The host owns configuration and credentials. Application code receives a scoped
`Kv` handle. Every deployment uses the same namespace enforcement and operations;
app code cannot choose a server, logical database, or another app's namespace.

```text
Host configuration
        |
        v
     KvStore
        |
        +-- Redb --> local file
        |
        +-- Redis --> standalone endpoint
                  --> cluster seeds --> slot owner
                  --> Sentinel --> current primary
```

`zeroship-kv` owns `KvConfig`, storage operations, and namespace enforcement.
`compio-redis` owns `RedisConfig`, connections, topology discovery, and recovery.
`zeroship-kv-v8` binds scoped handles into the runtime. Redis and Dragonfly use
the same Redis-compatible backend. There are no topology-specific Cargo features.
The existing `redis` and `redb` features enable the corresponding KV implementation.

## Loading configuration

The local CLI reads a TOML document from `ZEROSHIP_KV_CONFIG_FILE`. Without that
setting it opens redb at `ZEROSHIP_KV_PATH`, defaulting to `.zeroship/kv.redb`.

The worker accepts the same TOML document through `--kv-config-file`. Its
`ZEROSHIP_WORKER_KV_CONFIG` environment variable accepts either the document
or a `urn:zeroship:file:` secret reference. The worker does not load a shared
platform overlay; the KV file is supplied explicitly.
Configuration files must have owner-only permissions because they can contain
credentials. The configuration is secret-classed and its contents are redacted
from configuration reports.

An absent worker configuration leaves `env.kv` unavailable. Workers reject redb:
worker replicas need shared storage. The local Compose stack supplies a Redis
standalone configuration; a Compose override can replace that environment entry.

Changing configuration requires restarting the host. Changing a backend does
not migrate its data. Unknown fields, conflicting topology fields, invalid
endpoints, and invalid pool or timeout settings are errors. Redis connections
are established lazily on the compio thread performing an operation; successful
configuration parsing does not certify backend reachability.

## Embedded storage

```toml
backend = "redb"
path = ".zeroship/kv.redb"
```

## Standalone Redis or Dragonfly

```toml
backend = "redis"

[redis.topology]
mode = "standalone"
endpoint = "redis.internal:6379"
```

Endpoints are `host:port`, with bracketed IPv6 supported. Credentials belong in
`redis.auth`; URLs and URL query switches are not accepted as topology endpoints.

## Cluster

```toml
backend = "redis"

[redis.topology]
mode = "cluster"
seeds = ["redis-a.internal:6379", "redis-b.internal:6379"]
```

The driver discovers the slot map, routes commands to the owning primary,
follows `MOVED` and `ASK`, and refreshes topology through configured seeds or
previously discovered nodes. Advertised node addresses must be reachable from
the worker. An unconfirmed redirect address is refused before credentials are
sent to it.

The `{app_id}` hash tag places an app's keys in the same slot. This distributes
apps across shards. Listing routes to the app's slot owner and filters every
returned key through the app namespace. Listing is not a snapshot; consumers
must tolerate concurrent writes and topology changes.

Dragonfly's emulated cluster accepts this mode or standalone connections. Its
multi-shard deployment uses cluster mode. Dragonfly cluster provisioning,
replication, failover, and rebalancing belong to infrastructure management; the
KV driver consumes the resulting topology. See the
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

Sentinel addresses identify discovery servers. `service_name` names the monitored
primary group, not an app namespace. Data commands go directly to the discovered
primary. Separate Redis Sentinel processes can manage either Redis or Dragonfly
primary/replica deployments.

New connections resolve the primary through Sentinel and verify `ROLE`. Cached
connections are role-checked before reuse. When discovery reports a different
primary, idle connections are discarded, and leases from the previous pool
generation cannot return to the idle pool. Discovery failures return errors;
they never fall back to a configured replica or another backend. This follows
the [Sentinel client discovery contract](https://redis.io/docs/latest/develop/reference/sentinel-clients/).

## Connection settings

Optional settings apply independently of the chosen topology:

| Setting | Meaning |
| --- | --- |
| `redis.auth.username`, `redis.auth.password` | Data-server ACL identity or password-only authentication |
| `redis.tls` | Enable verified TLS for every data connection |
| `redis.sentinel_auth` | Separate Sentinel credentials |
| `redis.sentinel_tls` | Separate TLS settings for Sentinel connections |
| `redis.database` | Operator-selected logical database; cluster requires the default database |
| `redis.timeouts.connect_ms` | Deadline covering DNS, TCP, TLS, authentication, and database selection for a connection attempt |
| `redis.timeouts.command_ms` | Deadline for an individual command |
| `redis.pool.max_size` | Connection limit per node on each compio thread |
| `redis.pool.min_idle` | Connections opened when a pool is initialized; may defer connection creation until checkout |
| `redis.pool.idle_timeout_ms` | Age after which an idle connection is discarded |
| `redis.pool.liveness_probe_after_ms` | Age after which a direct connection is probed before reuse |

Defaults are defined by `Timeouts` and `PoolSettings` in
[`compio-redis` configuration](../../libs/compio-redis/src/config.rs).
Logical database selection is an operator placement option; app isolation always
uses the host-bound namespace.

For verified TLS with a private CA:

```toml
[redis.tls]
ca_file = "/etc/zeroship/redis-ca.pem"
```

An empty TLS table enables TLS with the system trust store. `cert_file` and
`key_file` enable client-certificate authentication and must be supplied together.
`server_name` specifies the certificate identity when discovery returns an IP
address. Without it, the driver verifies the endpoint's own identity. These same
fields apply under `redis.sentinel_tls`. Certificate verification cannot be disabled.
Certificate paths are resolved relative to the host process working directory;
use absolute paths in deployed configuration.

## Recovery and validation

A transport error after sending a mutation has an ambiguous outcome. The driver
returns that error without replaying the mutation. Explicit redirects and a
Sentinel `READONLY` rejection can be retried because they reject execution at the
old destination. Primary routing does not make asynchronous replication lossless
or provide exactly-once mutation execution across failover.

Native tests cover configuration rejection, Redis and Dragonfly operations,
cluster discovery and ownership changes, Sentinel promotion, credentials, TLS
identity checks, lost replies, and app isolation:

```sh
cargo test -p compio-redis --lib
cargo test -p zeroship-kv -p zeroship-kv-v8
```

Testcontainers owns the servers and their lifetime. Docker is required. Container
startup failures fail the tests. Image tags use major versions when published;
Dragonfly uses `latest` because its registry does not publish a major-version tag.
