# App metadata distribution

**Status. PROPOSED. NOTHING IN THIS DOCUMENT IS IMPLEMENTED.** No directory
object, no chunking, no gossip, no anycast, no zone, no manifest-by-digest and
no exception feed exists in the tree. What exists is the mechanism this document
proposes to replace, plus two pieces of machinery that a replacement would reuse
rather than build: the content-addressed blob store with its two-tier edge cache,
and the `oac_` client-id codec. Every sentence describing running code is tagged
MEASURED and carries a citation opened on 2026-09-05. Every sentence describing
the proposal is tagged DESIGNED. Read the difference as load-bearing: this repo
has a documented case of a design sentence in the present tense being built on
as though it described the tree.

---

## The problem, in one paragraph

Per-app metadata is the state a gateway or a worker must hold before it can
serve one request for an app it has never seen: which app a host maps to, what
its dispatch policy is, whether its creator has paid, and which deploy is live.
Today exactly one mechanism carries all of it. The control plane materialises a
complete table of every app on the platform, and every edge process downloads
that entire table on a five-second timer, revalidates it, recompiles it, and
swaps it wholesale. The cost per edge process is O(all apps), paid again every
interval, whether or not anything changed, and the largest term in it is a
per-app manifest that has nothing to do with routing. That shape is correct,
simple, and disqualified by the target of millions of apps. **The zone dimension
is not what breaks it.** A single-zone deployment at a million apps is already
dead under this mechanism; adding zones only multiplies the number of processes
paying the same bill. The pull shape therefore has to be replaced whether or not
the platform ever runs in two buildings, which means the work is not blocked on
a placement design.

---

## How to read this document

**Provenance.** Every citation below was re-derived by opening the file on
2026-09-05. The first pass ran against `58deea301` on `main` with nine tracked
files modified in the working tree; those nine landed the same day as
`cb0742195` and `825de4112`, so the numbers taken from them describe committed
code rather than an unsaved edit. Only one of the nine is cited here,
`crates/zeroship-gateway/src/router/dispatch.rs`, for the
`enforce::check_account` / `enforce::check_spend` call sites and
`extract_app_name`; those four were re-opened after the commit and hold.

**Re-audited on 2026-09-05 against `ed161c341`**, the commit that added this
file. That pass opened every citation a second time and found nine wrong
citations at six sites, two wrong counts, one unreproducible measurement cluster
and one over-general claim, all recorded in the corrections section below rather
than silently repaired. Read that section before trusting any number here: this
document has now been wrong twice about its own measurements.

**Tags, applied per claim.**

- **MEASURED** - I opened the code or ran the measurement, and the number came
  out of a file or a command.
- **DERIVED** - arithmetic over measured inputs. The assumptions are named
  inline; where the population is assumed, that is said in the same sentence.
- **DESIGNED** - a shape this proposal argues for. It does not exist.
- **ASSUMED** - a belief I did not bind to anything.

**On line numbers.** `tests/doc_citation_gate.sh` checks that a cited path
exists and that a cited line falls inside its file. It does not check, and by a
decision recorded in its own header at `:6-45` will not check, that the line is
the right one. Its header records the failure that decision was re-measured
against: AGENTS.md's schema-epoch paragraph carried ten line citations of which
seven were wrong, and `docs/architecture/data-system.md` drifted independently
over the same code. So: **the path is the durable claim, the line is a
courtesy.** Where a line matters to an argument here, the code is quoted.

---

# Part 1. What exists today

## The mechanism, drawn

```
        zeroship.apps
        zeroship.app_oauth_clients          zeroship.users
        zeroship.app_spend_state            zeroship.app_user_identities
        zeroship.app_members                zeroship.token_revocations
        zeroship.creator_billing_status
               |                                    |
        Registry::get_routes()  <-------------------+
        registry.rs:809 -- 5 tables, 1 statement,
        no filter but `WHERE a.archived_at IS NULL`
               |
        Registry::get_gateway_snapshot()   registry.rs:945
        + 2 more full-relation queries (8 tables, 3 statements, 1 response)
               |
        GET /internal/routes    internal.rs:171, routed at main.rs:1224
        no ETag, no If-None-Match, no cursor, no since-token, no page
               |
        every poll_interval, per gateway, the WHOLE table
        sync.rs:291-302, default interval 5s (config.rs:139-141)
               |
        RouteCache::update()   sync.rs:176-237
        per app, per poll:  validate()  ->  CompiledManifest::compile()  ->  clone
               |
        *self.routes.write() = compiled;  *self.name_index.write() = name_idx
        sync.rs:235-236 -- wholesale replacement, never a diff
```

## `RouteEntry`: nine fields, two of which the request path reads

MEASURED. `RouteEntry` is declared at `crates/zeroship-core/src/types.rs:213-259`
(the `#[derive]` is `:212`) with exactly nine fields, at `:214`, `:215`, `:216`,
`:217`, `:223`, `:236`, `:241`, `:248` and `:258`: `name`, `plan_id`,
`api_key_hash`, `deploy_hash`, `manifest`, `oauth_client_id`,
`sector_identifier`, `spend_state`, `account_state`.

MEASURED. Two of the nine are read on the request path.

- `name` is the routing key. It is indexed into `name_index:
  RwLock<HashMap<String, Uuid>>` (`crates/zeroship-gateway/src/sync.rs:43`) by
  `name_idx.insert(entry.name.clone(), id)` at `:220`, and resolved per request
  by `lookup_by_name` at `:239-244`, called from
  `crates/zeroship-gateway/src/router/dispatch.rs:1064`.
- `manifest` is the dispatch and authorization policy, compiled at `sync.rs:226`.

The other seven ride this feed for a reason unrelated to routing: it is the only
push channel from the control plane to the edge. `api_key_hash` is a credential
the gateway validates offline. `plan_id` is pricing. `oauth_client_id` and
`sector_identifier` are OAuth identity. `spend_state` and `account_state` are
billing enforcement applied before dispatch. `deploy_hash` names the live deploy.

MEASURED, and it matters for any replacement. Two of the seven default to the
permissive value on absence. `SpendState`'s `#[default]` is `Allow`
(`types.rs:172-173`, enum at `:171-177`); `AccountState`'s is `Active`
(`types.rs:205-206`, enum at `:204-209`); both fields carry `#[serde(default)]`
(`:247`, `:257`). A snapshot that loses a field, or an app with no billing row,
is unrestricted. That is deliberate and documented in place, and it is the right
default for the common free-tier case. It is stated here because any replacement
transport inherits the same direction of failure.

## Where the nine fields come from: one statement over five tables

MEASURED. `Registry::get_routes` (`crates/zeroship-control/src/registry.rs:809`)
issues one query (`:838-861`) naming five tables: `zeroship.apps a` as the
driving relation, `LEFT JOIN zeroship.app_oauth_clients c`, `LEFT JOIN
zeroship.app_spend_state s`, and a `LEFT JOIN LATERAL` at `:847-859` that itself
joins `zeroship.app_members m` to `zeroship.creator_billing_status cbs`. The only
filter is `WHERE a.archived_at IS NULL` (`:860`). No app filter, no limit, no key
range, no cursor.

**Correction.** The design conversation carried this as a four-table join. It is
five tables in three top-level `LEFT JOIN`s plus a `LATERAL` that joins two more.
The undercount does not change the argument, but it was not checked before, so
it is stated rather than silently fixed.

## The owner-fanout collapse, and why a redistribution must reproduce it

MEASURED. The `LATERAL` is:

```sql
LEFT JOIN LATERAL (
    SELECT DISTINCT ON (m.app_id) cbs.state AS account_state
    FROM zeroship.app_members m
    LEFT JOIN zeroship.creator_billing_status cbs ON cbs.creator_id = m.user_id
    WHERE m.app_id = a.id AND m.role = 'owner'
    ORDER BY m.app_id,
             CASE cbs.state
                 WHEN 'suspended' THEN 0
                 WHEN 'past_due'  THEN 1
                 WHEN 'active'    THEN 2
                 ELSE 3 END,
             m.user_id
) acct ON TRUE
```

MEASURED. The reason is written above it at `registry.rs:829-837` and is worth
repeating, because a distribution redesign will hit it again. `account_state` is
CREATOR-keyed; there is no `apps.creator_id` column, so the control plane reaches
it through the app's `app_members(role='owner')` row, and the schema permits more
than one owner row per app. Without the collapse a fan-out returns several rows
for one app, and because the loop at `:900` does `map.insert(id, ...)` into a
`HashMap`, the survivor is arbitrary. The comment says the consequence plainly:
`account_state` and every other `RouteEntry` field "could flip arbitrarily, even
un-suspending a suspended creator." The `ORDER BY` is not cosmetic - it makes the
most restrictive state win, so a fan-out can only tighten enforcement, never
relax it, and `m.user_id` makes the result deterministic under equal states.

MEASURED. This is the symptom of an open item, not a closed question: issue #47
records that there is no owner a database can hang off and that ownership is a
non-deterministic join over a table permitting several owners. The guard contains
the symptom at one call site.

DESIGNED. Any exception feed that carries `account_state` must reproduce this
collapse with the same ordering, or reproduce the bug.

## The endpoint is a full-table pull with no delta mechanism of any kind

MEASURED. `GET /internal/routes` is routed at
`crates/zeroship-control/src/main.rs:1224` and handled by `internal::get_routes`
(`crates/zeroship-control/src/internal.rs:171-185`): check the shared control
key, call `get_gateway_snapshot`, `HttpResponse::Ok().json(&snapshot)`. Grepping
`internal.rs` for `etag`, `ETag`, `If-None-Match`, `since` and `cursor` returns
nothing.

MEASURED. `get_gateway_snapshot` (`registry.rs:945-1007`) runs `get_routes` and
then two further full-relation queries: disabled, anonymized and
deletion-pending users joined to `app_user_identities` (`:948-965`), and every
`token_revocations` row inside a 24-hour window (`:985-991`). Three statements,
eight tables, one response.

MEASURED. The consumer is `sync_once` (`crates/zeroship-gateway/src/sync.rs:291`):
build `{control_url}/internal/routes` at `:292`, `serde_json::from_str` the whole
body into a `GatewaySnapshot`, hand it to `update_snapshot`. `start_sync`
(`:278-289`) sleeps `poll_interval` and loops. The default is 5 seconds
(`crates/zeroship-gateway/src/config.rs:139-141`), and it is a *shared* setting
whose canonical environment name is `ZEROSHIP_POLL_INTERVAL`
(pinned by a test at `crates/zeroship-worker/src/config.rs:363`), so one variable
moves both the gateway and worker feeds.

MEASURED. `RouteCache::update` (`sync.rs:176-237`) is not a diff. For every app
in the new table it calls `entry.manifest.validate()` (`:209`), then
`CompiledManifest::compile(&entry.manifest)` (`:226`), which clones the assets
map, the runtime-assets map and the worker-code entry
(`crates/zeroship-bundle/src/compiled.rs:376-378`). The finished map replaces the
old one wholesale (`:235-236`). Per-poll cost at the gateway is therefore
O(all apps) in JSON parse, validation, manifest compilation and allocation, paid
whether or not a single app changed, with peak memory briefly holding two full
compiled tables.

MEASURED, and it couples badly with elasticity. `start_sync` sleeps *before* its
first fetch, and `crates/zeroship-gateway/src/main.rs` has one sync call
(`:586`) with no eager first pull; the cache is constructed empty at `:559`
(`routes: sync::RouteCache::new()`). A freshly started gateway therefore has an
empty route table for at least one interval and every `lookup_by_name` misses;
`/readyz` returns 503 until the first pull lands inside the staleness budget
(`crates/zeroship-gateway/src/health.rs:40-42`, `is_ready` at `:68`, budget from
`crates/zeroship-core/src/readiness.rs:186`, which is three poll intervals with a
floor). Cold start is a full-table download, and it gates readiness.

## The worker's feed is a sibling, on a different transport

MEASURED. `GET /internal/versions` is routed at `main.rs:1212`, handled at
`internal.rs:127-141`. Behind it `Registry::get_versions` (`registry.rs:690`)
runs two full-relation statements: `zeroship.apps a LEFT JOIN zeroship.plans p`
with **no `WHERE` clause at all** (`:697-705`), and every row of
`zeroship.app_egress_rules` ordered by app (`:710-716`). Archived apps
deliberately remain in this projection, which is issue #89.

MEASURED. The worker consumes it in one process-wide poller: `start_version_poller`
(`crates/zeroship-worker/src/sync.rs:104`) spawns `version_poll_loop` (`:129-231`),
whose fetch is `poll_versions` at `:233-234`. The result reaches every ntex thread
through `SharedVersions` (`:79`) so HTTP traffic is not multiplied by thread count;
the per-thread reconcile loop is a separate spawn, `start_sync` (`:122-127`) into
`reconcile_loop` (`:240`), and it reads the shared map rather than polling.

**Correction, and a sharp one.** The conversation recorded that the worker pulls
"over the same transport" as the gateway. MEASURED, it does not:

- The **gateway** hand-rolls HTTP/1.1 over a bare `compio::net::TcpStream`
  (`gateway/src/sync.rs:347-388`): `TcpStream::connect` at `:354`,
  `parsed.port().unwrap_or(80)` at `:350`, and the credential formatted straight
  into the request line at `:356-358` as
  `Authorization: Bearer {auth_key}`.
- The **worker** uses a per-thread `cyper::Client`
  (`worker/src/sync.rs:591`, cloned at `:598`, bearer header at `:631`), and the
  workspace builds `cyper` with the `rustls` feature (root `Cargo.toml:49`).

Same shared secret, same endpoint family, two clients with two different
transport ceilings. Any design that says "move both feeds onto X" is touching two
codebases, not one.

## The manifest exists in six places

MEASURED, tracing one deploy:

1. the first tar member of the `.zship`, required by name
   (`crates/zeroship-bundle/src/unpack.rs:158`) and capped at
   `MAX_MANIFEST_BYTES = 1 MiB` (`crates/zeroship-bundle/src/limits.rs:11`,
   enforced at `unpack.rs:165` and again at `:175`);
2. blob storage at `manifests/<app_id>/<deploy_hash>.json`, written by
   `put_manifest` at `unpack.rs:338` (layout documented at
   `crates/zeroship-bundle/src/blob.rs:193-194`, built at `:220-224`; the S3 key
   is `manifests/{app_id}/{deploy_hash}.json`,
   `crates/zeroship-bundle/src/s3_blob.rs:78-80`);
3. the `zeroship.apps.manifest_json` column (written at `registry.rs:546`;
   declared nullable at `db/migrations-ts/20260702000200_control_tables.ts:162`);
4. the `zeroship.app_deploys.manifest_json` column (written at
   `registry.rs:595-601`; declared **NOT NULL** at
   `db/migrations-ts/20260705000000_durable_workflows_journal.ts:37`; granted to
   `zeroship_worker` at
   `db/migrations-ts/20260818000200_worker_database_authority.ts:96`; read by the
   workflow instance API at
   `crates/zeroship-control/src/workflow_instance_api.rs:705`);
5. inline on every `RouteEntry` of every gateway snapshot (`types.rs:223`,
   produced at `registry.rs:867-899`);
6. inline on every `AppVersionInfo` of every worker version feed entry
   (`types.rs:144`, produced at `registry.rs:774`). Its own doc comment at
   `types.rs:137-143` says it is "Carried inline on every `/internal/versions`
   poll."

**Correction.** The conversation recorded "the manifest is DUPLICATED: on
RouteEntry AND inside the .zship." It is four durable copies plus two wire
copies. Copies 5 and 6 are the only two re-transmitted on a timer, and they are
the two this proposal deletes. Naming the count changes what a fix has to cover:
removing the inline field from `RouteEntry` alone leaves the worker feed still
carrying a full manifest every poll.

## What the worker does with the manifest, corrected at HEAD

MEASURED, and this refutes a claim the conversation reached. The worker's
reconcile path touches `info.manifest` at three sites, not one:

- `worker/src/sync.rs:374-378` extracts the worker-entry blob hash via
  `worker_entry_hash` (defined at `:18-30`);
- `:424-431` resolves the runtime descriptor via `runtime_descriptor_json`
  (defined at `:40`), which fetches one blob by hash at `:49`;
- `:446` does `let declared = info.manifest.clone().unwrap_or_default();` and
  hands it to `cache::load_app` (`crates/zeroship-worker/src/cache.rs:567`) as
  the `manifest: &Manifest` parameter at `:574`, which compiles it at `:586` into
  the per-isolate declared policy.

MEASURED. `load_app`'s doc at `cache.rs:556-561` states the intent: the manifest
"is what makes the worker a real enforcer of the declared route policy rather
than a tier that trusts the gateway to have gated already." That behaviour landed
in the HEAD commit itself, `58deea301` ("feat(worker)!: refuse a dispatch the
declared policy does not admit").

So the sentence "the worker carries the entire manifest on every poll to extract
two hashes" was true before this commit and is false now. The worker needs the
whole manifest, for the same reason the gateway does. That strengthens rather
than weakens the design below: **both tiers need the manifest, and neither needs
it re-transmitted on a timer**, because it is immutable for the life of a deploy
and already has a content address.

## Sizing it, on a corpus rather than three points

MEASURED. I extracted `manifest.json` from all 31 built `.zship` artifacts under
`examples/*/dist/` (`zstd -dc | tar -xO manifest.json`) and measured field-value
lengths with `JSON.stringify`.

```
  app              manifest   assets  share    resources  share   nAssets nRes
  ------------------------------------------------------------------------------
  ws-probe             382        2    0.5%           57  14.9%       0     1
  auth-notes-db        529        2    0.4%          107  20.2%       0     3
  ssr-blog             999                (median of the corpus)
  ssg-docs            1066      708   66.4%          155  14.5%       3     3
  workflow-probe      1508      494   32.8%          597  39.6%       2     8
  kv-dashboard        2509      748   29.8%         1438  57.3%       3    18
  db-todos            3574      748   20.9%         2378  66.5%       3    30
  hr-system           6484     1257   19.4%         4904  75.6%       6   108

  corpus: n=31   min 382   median 999   mean 1216   max 6484
```

MEASURED. The three figures carried from the conversation reproduce exactly:
529 (`auth-notes-db`), 1066 (`ssg-docs`), 1508 (`workflow-probe`), and so does
the `ssg-docs` breakdown: the `assets` *value* is 708 of 1066 bytes, 66.4%.
(Counting the key and separator too it is 718, 67.4%. Both numbers are right
about different things; which is which is recorded because that difference is
exactly what gets carried forward as a contradiction.)

MEASURED. The `.zship` artifacts run 4356 bytes (`ssg-docs`) to 650908 bytes
(`hr-system`), median 31632 (`storage-gallery`). The three conversation figures
4356 / 28639 / 32758 reproduce as `ssg-docs` / `auth-notes-db` /
`workflow-probe`.

**Correction, two of them, and the second is the more dangerous kind.**

1. The `.zship` sample understated the top of the range by twenty-fold. Ten of
   the 31 artifacts exceed 130 KB. A bound quoted from three hand-picked members
   of a population is not a bound.
2. The conversation concluded that `assets` is "the only unbounded field and
   dominates." MEASURED, both halves are wrong. Seven manifest fields are
   unbounded maps or vectors: `resources` (`manifest.rs:64`), `schemas` (`:70`),
   `aliases` (`:76`), `assets` (`:86`), `runtime_assets` (`:93`), `sourcemaps`
   (`:103`) and `schedules` (`:133`). And across the corpus `assets` is 9835 of
   37708 bytes, **26.1%**, against `resources` at 17317, **45.9%**. In the
   largest manifest the split inverts: `resources` is 75.6% and `assets` 19.4%.
   `ssg-docs` at 66.4% is the corpus maximum, not the typical case. This is the
   dangerous kind of error because 66.4% is *correct about the one app it was
   measured on*.

MEASURED, and this is the claim that survives, about slope rather than share:

```
  per asset entry     240 bytes  (9835 bytes / 41 entries, corpus mean)
  per resource entry   57 bytes  (17317 bytes / 306 entries, corpus mean)
```

An asset entry costs about four times a resource entry, and the two counts are
driven by different things. `resources` grows with hand-written server procedures
and route rules; a person writes each one, and 108 of them is already a large
app. `assets` grows with build output: one entry per emitted file, per
pre-compressed variant, bounded only by the size of the site. At 240 bytes per
asset the 1 MiB ingest cap is reached at roughly **4370 assets**, an ordinary
documentation site; at 57 bytes per resource the same cap is about 18400
procedures, which nobody writes. `assets` is the field whose cardinality is set
by a build rather than by a person, and it is the one that hits the cap first.
That is the argument for digesting it, and it does not need either of the two
false claims.

MEASURED. The manifest struct itself has **19 fields**
(`crates/zeroship-bundle/src/manifest.rs:33-173`), not the sixteen the
conversation recorded.

## The arithmetic that disqualifies the shape

MEASURED, and the construction is spelled out so it can be re-run. Wrap each of
the 31 stored `manifest.json` bodies in a `RouteEntry`-shaped object whose eight
other fields carry `name = "myapp"`, `plan_id = "pln_" + 22 chars`,
`api_key_hash` and `deploy_hash` each 64 hex characters,
`oauth_client_id = "oac_" + 22 chars`,
`sector_identifier = "https://myapp.zeroship.ai"`, `spend_state = "allow"` and
`account_state = "active"`; serialise compactly with the Rust field names as
keys. That gives a **mean of 1588 bytes per entry**, of which the eight
non-manifest fields are **exactly 372 bytes** - exactly, not nearly, because
every one of those values is fixed by the construction. The manifest is between
51% and 95% of an entry (382/754 for the smallest manifest, 6484/6856 for the
largest).

DERIVED, with the assumption named. At 10^6 apps one pull is about **1.59 GB**,
or **1.48 GiB**. At the default 5-second interval that is roughly **318 MB/s of
egress per gateway**, times the number of gateways, produced by re-running a
five-table join and re-serialising the whole corpus each time. On the receiving
side each gateway spends that same interval parsing 1.48 GiB of JSON and calling
`validate` plus `compile` a million times, then briefly holding two compiled
tables.

**The assumption carrying the most weight is the population.** The 31 examples in
this repo are probes and demos, not a sample of a creator population, and a real
corpus will have a heavier tail (the measured spread already runs 17x, from 382
to 6484 bytes). Treat 1588 bytes as an order-of-magnitude anchor, not a forecast.
The conclusion does not depend on the constant: for any manifest distribution
consistent with what was measured, 10^6 apps puts the pull somewhere between
roughly 0.4 GB and 6 GB per gateway per interval, and every value in that range
is disqualifying.

MEASURED, and this is the actual wall. It is not "zones make this hard." It is
that a gateway must download and recompile the state of every app on the platform
to serve one request for one app, and there is no mechanism in the code - no
ETag, no cursor, no since-token, no per-app fetch on the routing path - to ask
for less.

## Two frozen things this design neither causes nor fixes

Stated here so that a later section does not appear to solve them by accident.

### The gateway's control transport is cleartext by construction, and declines to pretend otherwise

MEASURED. `validate_control_url` (`gateway/src/sync.rs:321-338`) accepts only the
`http` scheme and returns an error for anything else. Its doc at `:310-320`
states the reasoning without softening it: `http_get_inner` opens a plain
`TcpStream` and writes the control key as an `Authorization: Bearer` header,
there is no TLS on the path at all, so an `https://` URL "does not get encrypted
- it gets silently downgraded, and the key goes out in cleartext," and because
`Url::port()` returns `None` for a scheme-default port, `https://control` would
land on port 80 rather than 443. The code matches the comment exactly (`:350`,
`:354`, `:356-358`).

This is not a claim of protection. It is a claim that the gateway declines to
*appear* protected. The control key crosses that hop in the clear on a trusted
network, or an operator terminates TLS in front. And as measured above the
worker's client is `cyper` with `rustls` available, so two services on one shared
secret have different transport ceilings. Any design that moves metadata onto a
new channel inherits this asymmetry; it is not resolved by changing what is sent.

### The hash ring is frozen at process start

MEASURED. `HashRing` (`crates/zeroship-gateway/src/proxy.rs:60-121`) exposes six
methods - `new` (`:68`), `select` (`:79`), `select_with_affinity` (`:101`),
`acquire` (`:118`), `release` (`:119`), `num_workers` (`:120`) - and **not one of
them takes `&mut self`**. There is no method that adds, removes or replaces a
worker. It is stored as a bare field, `pub hash_ring: proxy::HashRing`
(`crates/zeroship-gateway/src/lib.rs:135`), with no lock and no `ArcSwap`, so
even if a mutation method existed there is no interior mutability to reach it
through. It is constructed once at `main.rs:441` from a comma-separated
`--worker-urls` string parsed at `:191`, with `max_per_worker` a hardcoded `500`
(`:432`) and 150 vnodes per worker (`proxy.rs:53`). There is no worker
registration endpoint on the control plane: grep for `internal/workers`,
`register_worker` and `worker_registry` across `crates/zeroship-control/src`
returns nothing.

MEASURED consequence: adding or removing a worker requires restarting every
gateway process. Combined with the cold-start property above, fleet elasticity
and the metadata pull are coupled in exactly the wrong direction - the cheapest
way to change worker capacity is the operation whose cost grows fastest with app
count. **This is a limitation at ten apps, not at a million**, and it is the fact
that justifies gossip below.

### The gateway is further from stateless than it looks

MEASURED. `crates/zeroship-gateway/src/db.rs` carries the `Send + Sync`
`DbConfig` in shared state (`:44`) and builds the `!Send` `compio_postgres::Pool`
lazily per ntex thread into a thread-local (`:68-76`), because the pool cannot
live in `GateState` (`:3-25`). `checkout` is at `:128`. It serves
`zeroship.app_session_anchors` through `anchors::create` (`:183`), `read_live`
(`:240`), `update_rotated_family` (`:280`) and the delete paths.

**Correction.** The conversation recorded this pool as used by `browser_auth.rs`
and `backchannel_logout.rs`. MEASURED, `db::checkout(` is CALLED from **five**
gateway source files - `auth_token.rs` (8), `router/auth.rs` (13),
`browser_auth.rs` (2), `backchannel_logout.rs` (1) and `router/dispatch.rs` (1) -
across **25** call sites including tests. A sixth file, `lib.rs`, names it in a
rustdoc link at `:177` and calls it nowhere; `db.rs` itself defines it. Two of
five understates how much per-request database
state a gateway holds, which matters for any design that assumes gateways are
cheap to place near users.

---

# Part 2. Four of nine fields need no delivery

Four of the nine `RouteEntry` fields do not have to be delivered at all, for two
different reasons: two are computable from the map key, and two are almost always
the default. In three of the four cases something small survives the derivation,
and dropping it would be a regression rather than a saving. This part derives
each and states precisely what survives.

## `oauth_client_id` is a copy of a computable value

MEASURED. The per-app OAuth `client_id` is a prefix swap on the app UUID. The
mint and its exact inverse are:

```rust
// crates/zeroship-core/src/typed_id.rs:337-339
pub fn app_oauth_client_id(app_id: &uuid::Uuid) -> String {
    format!("{APP_OAUTH_CLIENT_PREFIX}_{}", uuid_to_base62(app_id))
}

// crates/zeroship-core/src/typed_id.rs:346-349
pub fn app_id_from_oauth_client_id(client_id: &str) -> Option<uuid::Uuid> {
    let encoded = client_id.strip_prefix(APP_OAUTH_CLIENT_PREFIX)?.strip_prefix('_')?;
    base62_to_uuid(encoded).ok()
}
```

MEASURED. `APP_OAUTH_CLIENT_PREFIX` is `"oac"` at `typed_id.rs:237`, and its doc
at `:228-236` names itself "the SINGLE source of truth for the prefix string,"
with the control plane minting and the auth consent classifier decoding, "both
through this constant so they can never drift." The control plane's minter
delegates rather than reimplementing: `client_id_for_app`
(`crates/zeroship-control/src/app_oauth_client.rs:142-144`) calls
`app_oauth_client_id`, with a comment saying it does so "so the auth-side decoder
is the exact inverse."

MEASURED. The decode is total and refusing, not partial. `base62_to_uuid`
(`typed_id.rs:105`) rejects a tail that is not exactly 22 characters, any byte
outside the alphabet, and arithmetic overflow; the caller additionally requires
the literal `oac_` separator. `app_oauth_client_id_round_trips`
(`typed_id.rs:713-729`) binds all of it: the round trip, the 26-character length,
and `None` for `"zeroship-builder-abc"`, `"oac_not-base62"` and `"oacsomething"`.

MEASURED. The gateway already holds the app UUID: it is the key of `RouteMap`
(`types.rs:265`, `pub type RouteMap = HashMap<Uuid, RouteEntry>`). Shipping
`oauth_client_id` alongside that key ships a function of the key.

MEASURED, the second consequence. `lookup_by_oauth_client_id`
(`gateway/src/sync.rs:263-275`) resolves a client id by scanning the whole route
table comparing strings at `:268-274`. Its doc at `:259-262` justifies this
honestly and on premises this proposal removes: "O(N) over the route table - BCL
is a low-frequency webhook surface, so a linear scan is cheaper than maintaining
a third index. The table is the same handful of apps `lookup_by_name` already
serves." The single non-test caller is the back-channel logout handler at
`crates/zeroship-gateway/src/backchannel_logout.rs:85`, which iterates unverified
`aud` candidates - so the scan today is O(candidates x apps). The O(1)
replacement is one function call away: decode, then `lookup_by_app_id`
(`sync.rs:246-249`).

## The `Option` conflates the identifier with the provisioned bit

MEASURED. `oauth_client_id` is `Option<String>` and the field's doc at
`types.rs:230-236` gives two reasons: a defensive one (an empty-string
`client_id` is a footgun, because a malformed token with an empty `client_id`
claim could match `""`) and a semantic one - it is `None` until the control plane
provisions the app's OAuth client, and consumers hard-fail 503 or 401 rather than
bind to a falsy value.

Only the second reason survives derivation, and it survives entirely.

MEASURED. The `None` is produced by a `LEFT JOIN` miss, not by a policy: the
comment at `registry.rs:811-813` says a provisioned app yields
`Some(oauth_client_id)` and `Some(sector_identifier)` while an un-provisioned app
with no extension row yields NULL and therefore `None`. That row can genuinely be
absent, because provisioning is best-effort relative to the create response:
`crates/zeroship-control/src/api.rs:360-378` logs the error and returns 201
anyway, with the comment "a DB hiccup here is logged + metered, and the next
deploy re-provisions." So "this app has an OAuth client" is a real one-bit fact
about state on the OP side, and it is not a function of the app id.

MEASURED. It is a fact about two rows, not one. `upsert_db_rows`
(`app_oauth_client.rs:524`) writes `zeroship.oauth_clients` and
`zeroship.app_oauth_clients` inside a single transaction, so the extension row's
presence is a sound proxy for the OP-side client existing.

MEASURED. Splitting the two facts changes one live behaviour. The test at
`gateway/src/sync.rs:816` is named
`lookup_by_oauth_client_id_resolves_provisioned_app_and_skips_unprovisioned` and
asserts at `:850` that an un-provisioned app's `None` "is never matched by a real
client_id." A pure decode-then-lookup would match an unprovisioned app, because
the decode only inspects the string.

DESIGNED. The replacement is therefore three steps, not two: decode, then
`lookup_by_app_id`, then check a provisioned `bool` on the entry - a bit, not a
`String`.

```
  today                                   proposed
  -----                                   --------
  aud "oac_XXXX..."                       aud "oac_XXXX..."
        |                                       |
        | scan N entries, compare String        | app_id_from_oauth_client_id
        v                                       |   O(1), refusing
  Some((app_id, route))                         v
                                          Uuid -> lookup_by_app_id  (O(1))
                                                |
                                                v
                                          route.provisioned ? Some(..) : None
```

MEASURED migration cost worth naming: that test's fixture client id is the
literal `"oac_myapp"` (`sync.rs:828`, `:845`), which is not 22 base62 characters
and decodes to `None`. A decode-based lookup requires the fixture to mint a real
id.

## `sector_identifier` is the apex origin, and custom domains must not change it

MEASURED. The value is one line
(`crates/zeroship-control/src/app_oauth_client.rs:150-152`):

```rust
pub fn sector_identifier(scheme: &str, apex_host: &str) -> String {
    format!("{scheme}://{apex_host}")
}
```

with the doc immediately above at `:146-148` saying it is the app's apex origin,
used for pairwise scoping and as the post-logout redirect origin, and that
"Custom domains do NOT change the sector - all of an app's hosts share the apex."
A unit test pins the shape (`sector_identifier_is_apex_origin`, `:833`).

MEASURED. `ensure_app_client` (`:397`) treats `hosts.first()` as the apex
(`:411`, and again at `:464`); every other host contributes only redirect URIs.
The multi-host surface is unwired: the module header at `:44-47` records that it
"is not yet wired to a production caller because there is no custom-domain attach
handler in the codebase."

MEASURED, why it must not vary per host. The sector is the salt half of the
pairwise-subject HMAC. `derive_pairwise`
(`crates/zeroship-core/src/auth/mod.rs:302`) hashes the canonical global user id,
a colon, and the sector under the platform pairwise secret, emitting `pws_` plus
20 base62 characters (`PAIRWISE_SUB_PREFIX` at `:192`, length rationale at
`:183-184`). Its doc at `:269-282` states the property: the same
`(global_user_id, sector)` always yields the same subject, and two apps with
distinct sectors get different subjects for the same human. The gateway's
back-channel logout handler derives the same value to write the per-app
token-family marker (`backchannel_logout.rs:69-76`, derivation at `:507`), and
`GatewayPrincipalLifecycle.pairwise_subjects` carries already-issued subjects so
denials survive route removal.

DESIGNED consequence, derived from those measurements: a per-host sector would
give one human two `sub` values on **one** app - one under the apex, one under
the custom domain. Every row the app keyed by `sub` under the first host becomes
unreachable under the second; the account orphans, and revocation forks the same
way, because a second sector produces a second subject that no marker and no
lifecycle entry names.

MEASURED, what deriving it gateway-side would cost. `sector_identifier` is
computable as `{scheme}://{name}.{app_base_domain}` from state the *control
plane* holds - the apex is produced by `AppState::apex_host_for_app`
(`crates/zeroship-control/src/lib.rs:630-633`) as `format!("{name}.{}",
self.app_base_domain)`. The gateway does not hold it: grep for `app_base_domain`
or `base_domain` under `crates/zeroship-gateway/src` returns nothing, and
`extract_app_name` (`gateway/src/router/dispatch.rs:49`) takes the first label of
`Host` without ever knowing the suffix. So deriving this field at the gateway
means adding scheme and base domain to gateway configuration - a real change -
and it stops being derivable the moment `name` becomes mutable or a custom domain
becomes the apex. There is no app-rename write today, but
`backchannel_logout.rs:83` already calls `name` "the renameable subdomain slug,"
so the codebase does not consider it frozen.

DESIGNED, and narrower than "derive it": the sector is one string per app,
stable for the app's life and identical for every host it serves, so it belongs
to whatever carries app identity, not to a per-poll route row. Custom-domain
attach is unbuilt, so the decision costs nothing to make now and is expensive to
reverse later.

## `spend_state` and `account_state`: distribute the exceptions

MEASURED. Both enums default to their permissive variant (`types.rs:172-173`,
`:205-206`) and both fields carry `#[serde(default)]` (`:247`, `:257`).

MEASURED. Enforcement is a two-gate AND and each gate blocks on exactly one
variant. `check_spend` (`crates/zeroship-gateway/src/enforce.rs:23-29`) refuses
only `Block`, with `402 SPEND_LIMIT` at `:26`; `Warn` and `Degrade` pass, Degrade
being throttled elsewhere through the degraded registries. `check_account`
(`:41-47`) refuses only `Suspended`, with `402 ACCOUNT_SUSPENDED` at `:44`;
`PastDue` passes as the grace window. Dispatch calls account first, then spend
(`router/dispatch.rs:134` and `:137`; again at `:1183` and `:1196`).

MEASURED. They are keyed differently, and this is what a naive one-exception-list
design gets wrong. `spend_state` is per-**app**, joined on `s.app_id = a.id`.
`account_state` is per-**creator**, reached through the owner-membership
`LATERAL` quoted in Part 1.

**Correction, and the one that would have mis-sized the feed.** It is tempting to
say "only apps with a row need distributing." MEASURED, the spend evaluator
writes a row for *every* app on *every* tick. `evaluate_all`
(`crates/zeroship-control/src/spend.rs:237`) selects `SELECT id FROM
zeroship.apps` at `:240` and, when the state did not change, calls `touch_state`
at `:420` (defined at `:494`), an UPSERT performed unconditionally by explicit
decision - the comment at `:408-419` says "we deliberately DO write every tick
... The single-row UPSERT is cheap; the freshness is the point." So after one
evaluator pass essentially every app has an `app_spend_state` row carrying
`'allow'`. **The exception set is a value predicate, `state <> 'allow'`, not an
existence predicate.** Had this been written as "rows are rare," a reader would
have sized the feed by row count and been wrong by the whole app population.

MEASURED. `account_state` is the opposite shape. Its row is created only by the
dunning state machine, whose sole `INSERT INTO zeroship.creator_billing_status`
is at `crates/zeroship-control/src/account_status.rs:175`, reached only from
signature-verified Stripe webhook handling ("only a signature-verified Stripe
event ... No creator input sets it", `:25-32`). A creator who has never had a
payment fail has no row at all, so existence and exception nearly coincide - but
the row persists after recovery flips the state back to `active`, so
`state <> 'active'` is still the correct predicate.

MEASURED arithmetic on what removing the four fields saves. `RouteEntry` carries
no `serde(rename_all)` - the attribute at `types.rs:212` is only
`#[derive(Debug, Clone, Serialize, Deserialize)]` - so the JSON keys are the Rust
field names verbatim. With a representative apex host that is about **142 bytes
per entry**: 47 for `"oauth_client_id":"oac_<22>"`, 48 for
`"sector_identifier":"https://myapp.zeroship.ai"`, 22 for
`"spend_state":"allow"`, 25 for `"account_state":"active"`, each with its
separating comma. At 10^6 apps, roughly 142 MB per full pull, every interval, per
gateway.

## The empty exception list fails OPEN, and that is a property, not a bug report

State this carefully, because the tempting sentence ("an exception list is
fail-safe") is false and its opposite ("it is a security hole") overstates it.

DESIGNED characterization, on MEASURED gates. An exception list that arrives
empty - or does not arrive - serves every app unrestricted. That is fail-**open**
on billing enforcement: the platform keeps serving traffic it had decided to
block or throttle, and the loss is revenue and unbounded infrastructure cost, not
end-user data exposure. The two gates it disarms are `402 SPEND_LIMIT` and
`402 ACCOUNT_SUSPENDED` (`enforce.rs:26`, `:44`), neither of which is an
authentication or authorization boundary. It is the right availability choice.
It is the wrong word to call it safe.

MEASURED. The current pull has the same open direction for a *missing row* and
the opposite direction for a *bad value*, and both halves are deliberate.
`registry.rs:914-936` maps NULL to the permissive default in both cases and maps
an unrecognised TEXT value to the restrictive one: spend "fails closed to Block
(defensive - should never happen, the engine only writes the four known states)",
account "fails closed to Suspended". So today: unknown value, fail closed; absent
row, fail open.

DESIGNED. What changes under an exception feed is the *size of the aperture*, not
its direction. Today the fail-open case is "this app has no row," which the
evaluator closes on its next tick. Under an exception feed the case becomes "this
delivery was empty, stale, or lost," which is a transport property and can
persist. The current design also couples the two - routes and enforcement arrive
in one payload, so a gateway that cannot reach control serves nothing at all
(MEASURED: empty cache at `main.rs:559`, first sleep before first fetch at
`sync.rs:282`). Decoupling deliberately breaks that coupling: routes could be
fresh while the overlay is absent.

DESIGNED, and this is a requirement on the implementation rather than a
description of one. The exception feed must carry its own liveness - a generation
number or a not-after timestamp the gateway compares against its clock - so an
overlay that has stopped arriving is distinguishable from an overlay that
legitimately says "no exceptions." Without it, "no exceptions" and "no answer"
are the same bytes, which is the shape this repository has been bitten by before
(a sweep that matches nothing reporting success). What the gateway should *do*
when the overlay is stale is an open decision, below.

---

# Part 3. The object model

## The governing rule

**Mutable state lives in the directory. Immutable state is a content-addressed
blob.** Everything below is a consequence of applying that one rule to what the
tree already ships, and the interesting part is how little of it is new.

The rule is not aesthetic. It is the only division that lets a host answer two
different questions with two different mechanisms. "What is true about app X
right now" is a question about mutable state and needs an authority. "What did
deploy H of app X contain" is a question about an immutable object and needs only
a hash. Today both are answered by the same pulled blob, which is why the pull
has to carry everything and cannot be cached.

## The envelope decomposes

```
   what a host needs about an app
   |
   +-- mutable, authority-owned, changes without a deploy
   |     name             the host label; the routing key
   |     plan_id          billing tier
   |     api_key_hash     credential
   |     spend_state      recomputed by a cron
   |     account_state    recomputed by a cron
   |     provisioned      whether the OAuth client rows exist
   |     deploy_hash      WHICH immutable object is live
   |
   +-- immutable, deploy-scoped, addressed by content
         manifest.json    resources, assets, worker modules, schemas,
                          aliases, sourcemaps, schedules, workflows,
                          auth scopes, net hints, runtime descriptor
```

MEASURED. `deploy_hash` is not a second fact beside the manifest; it *is* the
manifest's own digest. `canonical_manifest_for_hash` strips the `deploy_hash` key
and canonicalizes (`crates/zeroship-bundle/src/unpack.rs:452`), and
`deploy_hash = sha256_hex(&canonical_omit)` at `:236-237`. So a directory entry
carrying `deploy_hash` already carries a pointer to the manifest object, and no
separate `manifest_digest` field is needed. That removes 32 bytes per entry from
the naive design.

## The immutable half nearly exists, with three corrections

**Correction, and it matters for the scheme.** The conversation said
`manifest.json` is "already a blob." MEASURED, it is not, in three separate
senses, and all three are load-bearing.

1. **It is not under `blobs/` in the archive.** Every other archive member must
   match `blobs/<64-hex>` or ingest refuses it (`unpack.rs:264-275`);
   `manifest.json` is a distinguished member outside that namespace.
2. **It is not in the blob keyspace at rest.** The trait comment says so:
   "Manifest storage - separate keyspace from blobs"
   (`crates/zeroship-bundle/src/blob.rs:131`). Local layout is
   `<root>/blobs/<hash[0..2]>/<hash[2..]>` versus
   `<root>/manifests/<app_id>/<deploy_hash>.json` (`blob.rs:193-194`, built at
   `:214-218` and `:220-224`); S3 uses `blobs/{hash}` versus
   `manifests/{app_id}/{deploy_hash}.json` (`s3_blob.rs:73-80`).
3. **The stored bytes do not hash to their own key.** Step 9 sets
   `manifest.deploy_hash = Some(deploy_hash)`, re-serializes, and stores *that*
   (`unpack.rs:334-341`). So `sha256(stored bytes) != deploy_hash`. Verifying the
   stored object requires stripping `deploy_hash` and re-canonicalizing first.

The conclusion the conversation drew - that the immutable half is nearly free to
move - survives, because `deploy_hash` *is* the digest and the gateway already
holds a hash-keyed cache. But the premise as stated would have produced an
implementation that verified stored bytes against `deploy_hash` and failed on
every manifest.

DESIGNED resolution: the manifest object moves into `blobs/` with the
`deploy_hash` field **removed from the stored body**, because the field is
redundant with the key. The alternative - every consumer runs
`canonical_manifest_for_hash` before comparing - is strictly more work at every
call site.

MEASURED, and this is what makes the move nearly free. The gateway already holds
a `BlobStore` (`crates/zeroship-gateway/src/lib.rs:146`) with a content-hash
keyed in-memory LRU (`blob_cache: BlobCache`, `:148`, defined at
`crates/zeroship-gateway/src/blob_cache.rs:25`) over an mmap-backed on-disk LRU
(`disk_cache`, `:152`, defined at `blob_cache.rs:143`). The disk tier `mmap`s the
file and hands the pointer to ntex as `Bytes::from_owner` (`mmap_to_bytes`,
`:677-684`), making the page-cache to socket path zero-copy from userspace;
publishes are atomic through a temp file and a pre-existing final file is size-
and hash-verified before being trusted (`verify_file`, `:621`); concurrent cold
misses on one hash are collapsed by a per-process single-flight (`begin_refill`,
`:516`, consumed at `router/static_serve.rs:206-221`, where a follower that wakes
to a failed refill re-loops and may become the next leader). `get_manifest`
already exists on the trait (`blob.rs:139`). A gateway that fetched manifests by
hash would be using machinery already built, already in the process, and already
sized by an operator flag.

MEASURED asymmetry, and a gap: `grep -rn BlobCache crates/zeroship-worker/src`
returns **zero**. The worker fetches blobs by hash (`worker/src/sync.rs:49`,
`:389`) with no edge cache in front of them. That is not a decision recorded
anywhere; it is an omission a manifest-by-digest design would have to close.

## The manifest carries an assets digest, not the map

DESIGNED. `Manifest.assets` and `Manifest.runtime_assets` are replaced by a
digest of a separately stored asset map. The manifest keeps inline the fields a
dispatch decision reads (`resources`, `worker`, `version`, `transformer`,
`runtime_descriptor`, `auth`, `net`) and grows one field naming the asset map by
content hash. The asset map becomes an ordinary blob under `blobs/<sha256>`,
fetched and cached by exactly the machinery that already fetches asset *bytes*.

MEASURED support that the split is clean today: no PRODUCER writes a non-empty
`runtime_assets`. Every one writes an empty map
(`crates/zeroship-bundle/src/manifest.rs:213`, `:458`;
`sdks/vite-plugin/src/zship.ts:471`; `crates/zeroship-worker/src/handler.rs:3623`).
One test constructs a populated one to exercise variant validation
(`crates/zeroship-core/tests/types_test.rs:689`), which is why this says
"producer" rather than "nothing in the tree".
Ingest *refuses* a non-empty one on a fresh deploy alongside a non-zero
`asset_version` (`unpack.rs:206-219`). The `env.assets.*` namespace that would
mutate them is listed in AGENTS.md as planned, not registered. So the manifest is
in fact immutable-after-deploy today.

DESIGNED, for when `env.assets.*` lands: the runtime asset map becomes a
*directory-side* object with its own version counter, never a field inside the
content-addressed manifest, because the moment it mutates the manifest stops
being addressable by its own digest.

## But both tiers genuinely need the asset map

This is the constraint that stops the digest from being a free win, and a design
that forgets it produces a gateway that 404s every static file.

MEASURED. The gateway serves assets itself, with no worker involved.
`serve_resource_tree_static` (`crates/zeroship-gateway/src/router/static_serve.rs:41`)
resolves a `try` chain against the manifest's asset maps; `lookup_static_hit`
(`:69`) calls `CompiledManifest::lookup_asset_for_static`
(`crates/zeroship-bundle/src/compiled.rs:444`), which reads `runtime_assets` then
`assets`; the bytes come from the three-tier fetch `fetch_static_bytes` (`:126`).

MEASURED, and worse than the wire cost: the map is resident **twice per app per
gateway process**. `CompiledManifest::compile` clones both maps into the compiled
form (`compiled.rs:376-377`), and the compiled form is stored in a
`CompiledRoute` *alongside the original `RouteEntry`*, which still owns its
`Manifest` (`sync.rs:226-233`). A gateway holding N apps holds 2N copies of every
asset map.

MEASURED, and new at HEAD as recorded in Part 1: the worker needs the whole
manifest too, as its declared dispatch policy (`cache.rs:567`, `:574`, `:586`).

DESIGNED resolution: both tiers keep reading a full manifest and a full asset
map, but obtain them by digest and cache them content-addressed, so N hosts
serving one app share one cache entry and a deploy that does not change the
assets does not re-transfer the map at all. The digest is the cache key; the
existing `BlobCache` is the cache on the gateway, and the worker gets the same
tier. This changes *where the map comes from*, not what a host can see.

DESIGNED fence, stated as an obligation rather than a fact. MEASURED, today a
manifest that fails `validate()` drops the app from the route table entirely, and
the comment at `sync.rs:195-208` says why: the manifest IS the authorization
policy, so a manifest we cannot interpret leaves no policy to enforce, and "the
only safe reading of 'no policy' is to stop serving the app, which makes dispatch
answer 404. Falling back to a permissive default here would serve every route the
manifest was meant to gate to anonymous callers." A fetched manifest or asset map
must inherit that disposition exactly: a fetch failure is a 404 for the app, not
an empty map that 404s per path while leaving the RPC surface open. **Nothing
enforces this yet.**

## The directory must be complete

DESIGNED. The directory is the mutable half: one entry per routable host, holding
the scalars and the `deploy_hash` pointer.

It must be **complete** on every host that answers requests, and completeness is
not a performance property. It is what makes a negative answer *authoritative*. A
gateway receiving `Host: nope.zeroship.ai` has to decide between "no such app,
404" and "an app I have not heard about yet, retry or ask upstream." Only a
complete local index makes the first answer sound.

MEASURED, that this is already how it behaves and is therefore being preserved
rather than invented: `lookup_by_name` (`gateway/src/sync.rs:239-244`) is two
`RwLock` read guards and two `HashMap` lookups and performs no I/O; on a miss,
dispatch returns 404 immediately (`router/dispatch.rs:1064`) without consulting
the control plane.

DESIGNED, two alternatives that cannot supply this, for structural reasons rather
than implementation gaps:

- **Content-addressing cannot.** A content-addressed store answers "give me the
  object whose digest is H." It has no key space to be exhaustive over, so it
  cannot distinguish "this host does not exist" from "I do not have that object."
  Absence of a blob is a cache state, not a fact about the world. This is exactly
  why the immutable half is content-addressed and the mutable half is not: they
  need different answers to "what if it is missing."
- **Gossip cannot.** Gossip converges eventually and offers no moment at which a
  node may conclude that what it has not heard does not exist. Every gossip
  protocol makes presence eventually-decidable and absence never-decidable. A
  gateway built on it must either fail open (serve an app it cannot authorize) or
  fail closed on every miss (404 apps deployed thirty seconds ago), and both are
  worse than a stale complete table.

## The directory must be chunked

Completeness at fleet scale cannot mean "one object." A single directory object
has to be republished in full for every spend-state flip, every deploy, every app
creation, on a platform where those events are continuous.

DESIGNED. The directory is a **root object plus N chunks**, where a host's chunk
is a deterministic function of the host itself:

```
   chunk_index = first k bits of sha256(host)

   root object                       chunk 0x000        chunk 0xfff
   +---------------------+           +-----------+      +-----------+
   | directory_version   |           | entries   |      | entries   |
   | k (chunk bits)      |   names   | for hosts | ...  | for hosts |
   | chunk_hash[0..N-1]  |   each    | whose     |      | whose     |
   +---------------------+   chunk   | prefix is |      | prefix is |
                             by hash | 0x000     |      | 0xfff     |
                                     +-----------+      +-----------+
```

Two properties follow, and they are the whole reason for the shape.

**A change republishes one chunk.** Mutating one app rewrites the chunk that
app's host falls in and the root's single hash slot for it. Every other chunk
keeps its digest, so every host that already has it keeps it. The root is the
only object that changes on every mutation, and it is a fixed-size array of
hashes regardless of fleet size.

**Completeness survives per chunk**, which is why the chunk key must be the
*lookup* key. Given `Host: nope.zeroship.ai`, a gateway computes
`sha256("nope.zeroship.ai")`, takes the first k bits, and knows exactly which
chunk that host *would* be in. If it holds that chunk at the current root's
digest and the host is not in it, the host does not exist, and the 404 is sound
without consulting anything. Absence is decided against one object.

DESIGNED consequence that is easy to miss: **chunking by `app_id` would break
this.** A gateway holding only a `Host` header cannot compute an app-id-keyed
chunk index, so it could not locate the chunk in which the host would live, and
the negative answer would again require a full scan or an upstream call.
MEASURED support that the host is the right key: the gateway's primary index is
`HashMap<String, Uuid>` keyed on name (`sync.rs:43`, built at `:220`). The two
other lookups need no second index - `lookup_by_app_id` (`:246`) is reached only
from paths that already hold an app id, and `lookup_by_oauth_client_id` (`:263`)
becomes an O(1) decode as derived in Part 2.

DESIGNED, and not an invention: prefix-sharding a content-addressed keyspace is
already this tree's habit. `LocalDiskBlobStore::blob_path` shards blobs into 256
buckets by `hash[0..2]` (`blob.rs:214-218`). Directory chunking is the same
discipline applied to a lookup key instead of a content hash.

## Size arithmetic, and the estimate that gates the premise

DESIGNED entry layout, packed binary, with derivable fields omitted:

```
  app_id                       16 B    raw UUID
  name                       1+~14 B   length-prefixed host label
  deploy_hash                  32 B    raw sha256; also the manifest address
  api_key_hash                 32 B    raw sha256
  plan ordinal                  2 B    index into the plan catalog
  spend_state, account_state,
    provisioned                 2 B
  entry generation              8 B
                             ------
                                107 B, call it 112 with framing
```

Two fields are omitted because Part 2 measured them derivable. `oauth_client_id`
is `oac_` plus base62 of `app_id` and round-trips exactly. `sector_identifier` is
`{scheme}://{apex_host}` where the apex is the derived default; when it equals
that default the field carries no information, and an app with a custom apex
carries it explicitly. ASSUMED, not measured: that the derived default covers the
overwhelming majority. There is no production corpus to check this against, and
if it is wrong the entry grows by roughly 30 bytes.

**The 112-byte figure is an ESTIMATE, not a measurement, and it is the number
that gates the whole replicate-whole premise.** For contrast, a MEASURED upper
bound, with the shape pinned so it can be re-run: the same nine facts written as
the JSON this tree already uses -
`{"app_id":"<36-char uuid>","name":"myapp","plan_id":"pln_<22>","api_key_hash":"<64>","deploy_hash":"<64>","spend_state":"allow","account_state":"active","provisioned":true,"generation":1}`
- is **347 bytes**. No separate manifest digest appears, because `deploy_hash`
already is one. The band between 112 and 347 is a factor of 3.1 and it decides
the argument:

```
  per entry   1M hosts     10M hosts
  ---------  ----------   ----------
     112 B    106.8 MiB     1.04 GiB
     128 B    122.1 MiB     1.19 GiB
     256 B    244.1 MiB     2.38 GiB
     347 B    330.9 MiB     3.23 GiB
```

Read this as the gate it is. At 1M hosts every figure in the column is something
a gateway process can hold, so replicate-whole survives at 1M on any encoding. At
10M hosts, 112 bytes is 1 GiB of resident index per gateway and 347 bytes is
3.23 GiB; the first is arguable, the second is not. **So replicate-whole is sound
at 1M and is conditional at 10M on the packed encoding landing near 112 bytes.
That conditional has not been measured and cannot be until an encoder exists.**
If it lands at 256 the premise needs a partial-replication story at 10M, and that
is a different proposal.

For scale, what the arithmetic replaces: MEASURED, the mean full `RouteEntry`
with its manifest inline is 1588 bytes, so 1M apps is a **1.48 GiB** snapshot
pulled by every gateway every 5 seconds and 10M is 14.8 GiB. The directory at 112
bytes is about 14x smaller, and unlike the snapshot it is not re-transferred
wholesale.

Chunk arithmetic, DESIGNED (dividing the estimate above):

```
  chunks   per entry   hosts    entries/chunk   chunk size   root size
  ------   ---------   -----    -------------   ----------   ---------
    4096      128 B      1M           244          30.5 KiB   128.0 KiB
    4096      128 B     10M          2441         305.1 KiB   128.0 KiB
    4096      347 B     10M          2441         827.2 KiB   128.0 KiB
   65536      128 B     10M           153          19.1 KiB     2.0 MiB
```

At 4096 chunks (k = 12, three hex characters of prefix) and 10M hosts, one app
changing republishes 305 KiB instead of 1.19 GiB: a 4096-fold reduction in write
amplification, by construction rather than by measurement. Raising k to 16
shrinks chunks by another 16x at the cost of a 2 MiB root that changes on every
mutation, which is the wrong trade while the root is the hot object. **k is a
tunable and 4096 is a starting point, not a measured optimum**; the open decision
below says what would settle it.

## What the directory does not carry

Stated explicitly, because the temptation is to let it grow back into the
envelope. The directory holds no `resources` map, no asset map, no schema
descriptor, no worker module table, no JSON schemas, no aliases, no sourcemaps.
All of those are deploy-scoped and immutable and therefore reachable from
`deploy_hash` alone. The test for a proposed new field is whether changing it
should change `deploy_hash`; if not, it does not belong in the manifest, and if
so, it does not belong in the directory.

---

# Part 4. Delivery, and multi-zone

## Three jobs, three shapes

"Distribute app metadata" hides three questions that have nothing in common
except the noun. Separating them is the design; conflating them is what makes a
DHT or a consensus store look plausible.

```
                         answered by            wire cost per answer
  ---------------------------------------------------------------------
  (1) CONTENT             fetch by digest        one HTTP GET, cached
      "give me the bytes  over HTTP, against     forever thereafter
       whose sha256 is X" a content-addressed    (the name IS the
                          store                   validator)

  (2) CHANGE + MEMBERSHIP gossip over the        O(log n) fanout of a
      "X is stale"        fleet                  few hundred bytes
      "node N joined"

  (3) EXISTENCE           the local, complete    ZERO. No wire at all.
      "is there an app    directory
       named foo?"
```

Job 1 is built and needs no new mechanism. Job 3 needs no mechanism at all,
provided the directory stays complete. Job 2 is the only place a new transport is
warranted, and it is warranted by membership rather than by invalidation.

## Job 1: fetch by digest. This exists.

MEASURED. `BlobStore` (`crates/zeroship-bundle/src/blob.rs:55`) is already a
content-addressed fetch API: `get_blob(&self, hash: &str)` (`:57`),
`local_path(hash)` (`:62`), `has_blob(hash)` (`:89`), `get_blob_to_file` (`:123`).
The S3 implementation maps a hash to `blobs/{hash}` (`s3_blob.rs:73-75`) and
re-verifies the sha256 of the returned bytes before handing them back, with the
comment that "the backend's metadata/checksum is not trusted proof of integrity"
(`:412-414`).

MEASURED. The gateway's two-tier edge cache in front of that store is complete,
as detailed in Part 3.

MEASURED. Nothing on the serving path handles an archive.
`zeroship_bundle::unpack` has exactly one non-test consumer in the tree and it is
the control plane's deploy path (`crates/zeroship-control/src/deploy.rs:2`, which
re-exports it). The worker fetches individual blobs by hash
(`worker/src/sync.rs:49` for the runtime descriptor, `:389` for the worker
bundle), never an archive.

DESIGNED. Content distribution therefore needs no new protocol. It needs the
existing digest fetch pointed at more origins, and the worker given the cache the
gateway already has. A digest is a perfect cache key and a perfect validator: a
fetch is either a hit forever or a miss exactly once. There is no coherence
problem here because there is no mutable resource.

## Job 2: invalidation and membership. Gossip, justified by membership.

DESIGNED, on the MEASURED fact from Part 1 that `HashRing` has no mutation method
and is constructed once at `main.rs:441`. Gossip is justified by that fact alone.
A membership protocol gives every gateway a live view of which workers exist and
which are healthy, which is the input `HashRing` needs and cannot currently
receive. Once a fleet-wide gossip channel exists for membership, invalidation is
a free rider: "app `a` moved to deploy hash `h`" is a few dozen bytes on the same
wire, in the same fanout, with the same failure model.

**Correction, recorded because the first version of this argument was wrong.**
Gossip was initially dismissed on the grounds that it would be a whole
distributed system added only to invalidate a cache, and that a shorter poll
interval or an ETag would do the same job for less. That reasoning is defensible
about invalidation and irrelevant to the actual blocker. It missed that the ring
is frozen at boot, so there is a second, unrelated problem with the same
solution. Building gossip for invalidation alone would be hard to justify;
building it for membership and getting invalidation free is not. The error was
scoping the mechanism to one of the two problems it solves.

## Job 3: existence. No wire at all.

DESIGNED, on the MEASURED behaviour above. Completeness is what buys the free
negative, and it is only affordable once the manifest is out of the entry, which
is the same change job 2 needs. The two requirements agree rather than trade off.

MEASURED caveat: `lookup_by_oauth_client_id` linear-scans the whole route table
(`sync.rs:263-275`) where an O(1) decode is available. Replacing the scan with
the decode is small and independent and should not wait for the rest.

## What was rejected

**A DHT.** Rejected on workload and on isolation. A DHT is right when the key
space is enormous, the participants are many and equal, and no single node can
hold the whole map. Here the map is small once the manifest is removed, every
gateway wants the *whole* map because job 3 depends on completeness, and the
reads are overwhelmingly negative lookups a complete local map answers for free.
A DHT converts those free negatives into network round trips. The second
objection is a policy objection and is stronger: a DHT places a key's replicas on
nodes chosen by hash, so an app's configuration would sit on workers that do not
serve it and have no business holding it. AGENTS.md's standing invariant is that
privilege follows the process; a worker that executes creator code for app A
should not be a replica for app B's configuration. Consistent hashing already
places *dispatch* on chosen workers; a DHT would place *config* on unrelated ones.

**libp2p.** Rejected on fit first and on the tokio invariant second, and the
order matters. libp2p is engineered for open, untrusted, NAT-traversing peer
networks with peer discovery, hole punching, relay circuits and identity from
first principles. None of that describes a fleet we provision: our nodes have
addresses we assigned, a trust root we issued, and a membership list we control.
Paying that complexity for discovery problems we do not have is a bad trade
before any dependency question arises. It also arrives on a tokio reactor, which
AGENTS.md permits only as a `[dev-dependencies]` exemption, with
`tests/zero_tokio_gate.sh` enforcing both directions. That is the second reason
and should not be the argument that gets made.

**etcd.** Rejected because its job is already done here, and because its known
failure mode is the one we would hit. MEASURED: PostgreSQL advisory locks are
already this platform's fleet coordination primitive.
`crates/zeroship-control/src/cron/lock_keys.rs` centralises seven keys
(`:38-57`) in one array (`ALL`, `:66-74`) whose pairwise distinctness is enforced
by a test at `:82` rather than by transcribed literals; the header at `:1-35`
records that this replaced four hand-written comparison tests covering ten of
twenty-one pairs, with the two keys defined in the same file compared by none of
them. **73 source files across 16 crates** name `pg_advisory_lock`,
`pg_try_advisory_lock` or `pg_advisory_xact_lock` today - 69 files in 15
`crates/` members plus 4 in `libs/compio-postgres`: `zeroship-auth`,
`zeroship-authn`, `zeroship-control`, `zeroship-data-core`,
`zeroship-data-postgres`, `zeroship-data-sqlite`, `zeroship-migrate`,
`zeroship-migrate-backend`, `zeroship-migrate-core`, `zeroship-migrate-mysql`,
`zeroship-migrate-node`, `zeroship-migrate-postgres`, `zeroship-migrate-server`,
`zeroship-plugin-db`, `zeroship-plugin-workflow` and `compio-postgres`. Adding etcd would add a
second, independently-failing source of coordination truth for a role that is
filled and tested. The deeper objection: etcd cannot fan a watch out to a large
number of watchers, which is why Kubernetes had to put an API server in front of
it to multiplex watches. Adopting etcd means adopting the obligation to build
that layer, which is to say we would build the fanout tier and etcd would be an
implementation detail underneath it. Building the fanout tier directly is shorter.

**P2P swarming (Dragonfly, Kraken and relatives).** Rejected on artifact size.
MEASURED: across the 31 built `.zship` artifacts, sizes run 4356 bytes to 650908
bytes with a median of 31632. Swarming exists to amortise moving very large
immutable objects to very many nodes at once, where origin egress is the
bottleneck and peers can supply each other. That regime does not begin at a 30 KB
median and a 651 KB maximum; at these sizes a peer negotiation costs more than
the transfer it saves. ASSUMED, and marked as such: the gigabyte scale those
tools target is external context I could not verify from this tree, so the
argument rests on our measured sizes, not theirs.

MEASURED, on the state of the tree: `gossip`, `libp2p`, `SWIM`, `consul`,
`hickory` and `trust-dns` each appear **zero** times across `crates/` and `libs/`
in any `.rs` or `.toml` (word-boundary search); `Cargo.lock` contains no package
beginning `hickory` or `trust-dns`; `etcd` appears twice, both in a blocklist of
service-discovery ports (`crates/zeroship-core/src/preview_ports.rs:66`, `:74`).
None of the four rejected options is being removed. All four are being declined.

MEASURED, before anyone reaches for a new HTTP dependency: `cyper` is already a
direct dependency of the gateway, the worker and the control plane
(`crates/zeroship-gateway/Cargo.toml:59`,
`crates/zeroship-worker/Cargo.toml:43`, `crates/zeroship-control/Cargo.toml:99`),
built with `rustls` (root `Cargo.toml:49`). A conditional or ranged HTTP GET
needs no new dependency; only the gateway's control pull is hand-rolled, and it
is hand-rolled below the level at which conditional requests exist.

## Multi-zone: anycast plus one forward hop

DESIGNED. Each zone runs its own gateways and workers. The public apex is
announced by anycast, so a client's packets reach the topologically nearest zone
with no name resolution step and no client-visible hostname difference. If the
app is not served in the receiving zone, the gateway makes **one** forward hop to
the zone that serves it and returns the response. Not a redirect: a proxy hop,
invisible to the client, the same shape the gateway already implements for its
own worker fleet.

```
     client
       |
       |  anycast: nearest zone, one hostname, no DNS decision
       v
  +--------------------+          +--------------------+
  |  ZONE A            |          |  ZONE B            |
  |                    |          |                    |
  |  gateway ----[hop]---------------- gateway         |
  |     |              |          |     |              |
  |     | dispatch     |          |     | dispatch     |
  |     v              |          |     v              |
  |  workers           |          |  workers           |
  |     |              |          |     |              |
  |     v              |          |     v              |
  |  Postgres  storage |          |  Postgres  storage |
  +--------------------+          +--------------------+

  The hop crosses zones. The DATA PATH never does: workers only ever
  touch the database and object storage of their own zone.
```

MEASURED, on why one hop is cheap to build: the gateway's forward path already
takes an arbitrary worker base URL. `forward_dispatch`
(`crates/zeroship-gateway/src/proxy.rs:209`) encodes a dispatch frame, selects a
target with `ring.select(app_id)`, and hands the URL to
`forward_to_worker_dispatch` (`:277`). The connection abstraction is already a
sum over TCP and Unix sockets (`Stream::Tcp` / `Stream::Unix`, `proxy.rs:22-24`).
There is one existing proxy endpoint on the public gateway surface,
`/__zeroship/internal/workflow-advance`, wired at
`crates/zeroship-gateway/src/main.rs:659`; a cross-zone hop is the same mechanism
with a different destination.

MEASURED, on the state of zones: there is no region, zone or datacenter concept
anywhere in the tree. `docs/architecture/data-system.md:552-555` records the same
finding independently ("no region or datacenter concept anywhere in the tree",
checked 2026-08-29) and `docs/architecture/distributed.md:93` lists "No
multi-region route propagation or data replication in the shipping code path"
among the current boundaries. Everything in this subsection is DESIGNED.

**Per-app DNS was rejected**, on two independent grounds, and the second is
decisive.

MEASURED, the first: it is entirely new infrastructure. There is no DNS client
anywhere in this repository - `hickory` and `trust-dns` appear zero times in any
`Cargo.toml`, and `Cargo.lock` contains no package beginning `hickory` or
`trust-dns`. There is likewise no DNS *management* code. Per-app DNS would mean
adopting a record-management control loop, its propagation semantics, its
TTL-versus-failover tension and its failure modes, all at once, for a routing
decision anycast makes without any of them.

MEASURED, the second, and it ends the discussion: **a zone-qualified hostname
scheme breaks sessions outright.** Both browser session credentials use the
`__Host-` cookie prefix, which is host-only by specification. The anchor is
`__Host-zeroship_app_anchor` (`crates/zeroship-gateway/src/anchors.rs:71`), set
with `Path=/; HttpOnly; SameSite=Strict; Secure` and deliberately no `Domain`
attribute (`set_anchor_cookie` at `:89-93`, rationale at `:84-87`); the
interactive credential is `__Host-zeroship_app_session`
(`crates/zeroship-gateway/src/oidc_rp.rs:962`). A `__Host-` cookie set on
`app.zeroship.ai` is not sent to `app.zone-b.zeroship.ai` and cannot be made to
be. Any scheme that puts a zone into the hostname logs every user out on every
failover. Anycast keeps one hostname, so the cookie keeps working, so failover is
invisible.

There is a third, quieter consequence worth stating: `api.<domain>` is the
gateway's `iss` claim, not merely its address, so repointing that hostname moves
end-user session issuance. Anycast leaves the name fixed and moves only the
packets, which is what makes session continuity across zones possible at all.

## What blocks app mobility today

Two things, both measured, and neither a scheduling problem. An app cannot be
moved between zones today and would not become movable by adding gossip, anycast
or a forward hop.

**Session anchors live in the gateway's own Postgres.** MEASURED, as detailed in
Part 1: a per-ntex-thread `compio-postgres` pool
(`crates/zeroship-gateway/src/db.rs:68-76`, `checkout` at `:128`) serving
`zeroship.app_session_anchors` through `anchors::create` (`:183`), `read_live`
(`:240`), `update_rotated_family` (`:280`) and the delete paths, with
`db::checkout` called from five gateway source files. A gateway in zone B validating
a session for an app whose anchor row is in zone A must read zone A's database.
Until anchors are addressable from any zone - replicated, or homed with the app
and read over the same forward hop - moving an app moves its sessions' storage
out from under whichever gateway holds the cookie.

**Encryption keys are derived from the app id with no rotation surface.**
MEASURED: `crates/zeroship-data-core/src/encryption/keys.rs` derives both AEAD
halves with HKDF-SHA256 salted by the app id. The module header spells it out at
`:11-14` (`salt = app_id`, `info = "zsenc/aead/v1/k_enc"` and `.../k_siv"`) and
`derive_key` at `:401-409` does exactly that:

```rust
let hkdf = Hkdf::<Sha256>::new(Some(app_id.as_bytes()), root);
```

The root key reaches the process out of band, never from a database (`:21-41`,
which also records that the `PgAdminTable` source was deleted with the admin
schema on 2026-08-27 under the privilege-follows-the-process invariant). The
cache invariant at `:55-58` states the limit in the module's own words: once an
`(app_id, key_id)` entry is inserted it stays for the lifetime of the `KeyStore`,
and **"There is no rotation surface today; adding one will require rewiring the
cache to track key versions."**

That is not a claim of protection; it is the module saying what it does not have.

**Correction.** The conversation concluded from this that "an app CANNOT MOVE
between zones." That does not follow, and the corrected statement is more useful.
The salt is the `app_id`, which does not change when an app moves, so derivation
is stable across zones. What pins the app is that the root key for its `key_id`
must be present, out of band, in every zone that serves it, and that with no
rotation surface there is no way to re-key an app's existing ciphertext under a
different root. So: an app can move to a zone that already holds its root, and
cannot be given a different key without a mechanism that does not exist. Either
every zone holds every root, which makes zone isolation nominal, or an app's
encrypted data cannot follow it across a zone boundary. Neither is acceptable as
an end state; choosing between them is an open decision below. It is named so
that "apps are mobile between zones" is never written as though it follows from
the delivery mechanism.

**A third constraint, on where workers can run at all.** MEASURED:
`crates/zeroship-worker/src/db_posture.rs` validates the connecting role in
`validate` (`:101-176`), which fails when either `REPLICATION` or `BYPASSRLS` is
missing (`:123-128`) and again when any role membership inherits ambiently
(`:162-176`, with the reasoning at `:148-161`: a single login role shared by
every app, whose inheriting memberships would make its ambient authority the
union of every tenant it has ever served). `main.rs:446-448` calls it before
`init_v8()` at `:450` and `std::process::exit(1)`s on failure. A worker cannot
start in a zone whose database has not been provisioned to that exact posture.

---

# Open decisions

Each is a concrete either/or with a recommendation. None is settled.

## 1. Chunk count

**Either** fix k = 12 (4096 chunks) now and treat it as a constant, **or** make k
a field of the root object so it can be raised without a flag day, at the cost of
every reader carrying a re-chunk path.

**Recommendation: put k in the root and start at 12.** The arithmetic above is
DESIGNED, not measured, and the two inputs that would settle it are both
unmeasured today: the packed entry size (ESTIMATE, 112 bytes, against a MEASURED
JSON upper bound of 347) and the app creation and mutation rate, for which this
tree has no instrument. A k baked into readers as a constant is a value that
cannot be corrected once the number it was chosen from turns out wrong, and the
number it was chosen from is currently an estimate over an assumed population.
The re-chunk path is cheap while the fleet is small and impossible to add later.

## 2. Does the envelope collapse into `manifest.json` entirely?

**Either** the manifest stays one immutable object per deploy, with the asset map
digested out of it, **or** the manifest itself becomes a small root naming
several digested parts (resources, assets, schemas, descriptor) so a host fetches
only the parts it reads.

**Recommendation: digest the asset map out, and stop there for now.** MEASURED,
the two tiers read overlapping but different parts - the gateway reads
`resources` and both asset maps (`static_serve.rs:41-101`), the worker compiles
the whole manifest as its declared policy (`cache.rs:574`, `:586`). A finer split
would let each fetch less, but the measured mean manifest is 1216 bytes and the
median 999, so the saving is small and the cost is several round trips on a cold
isolate load. Revisit if the p99 manifest measured over a real corpus is large;
that measurement is listed below.

## 3. Where session anchors live

**Either** anchors stay gateway-local and an app is pinned to the zone holding
its anchor rows, **or** anchors move behind the same one-hop forward path as
dispatch, so the zone that owns the app owns its sessions.

**Recommendation: home anchors with the app and read them over the forward hop.**
The pinning option is not actually a decision to defer - it is a decision to make
zones a partition of users rather than of apps, and MEASURED it fights the
`__Host-` cookie property that makes anycast work: one hostname, one cookie, any
zone. The forward-hop option costs a round trip on the session-validating path
only, which is already the path that does database I/O today
(`gateway/src/db.rs:128`, five calling modules). It is the smaller change to the
model even though it is the larger change to the code.

## 4. Encryption key rotation

**Either** every zone holds every root key, **or** a rotation surface is built so
an app's ciphertext can be re-keyed when it moves.

**Recommendation: build the rotation surface, and treat "every zone holds every
root" as the interim only if it is written down as such.** MEASURED, the module
already names the work: "adding one will require rewiring the cache to track key
versions" (`keys.rs:57-58`). The interim option is not free - it makes zone
isolation nominal for the one thing zone isolation would most be wanted for -
and it is the kind of interim that becomes permanent because nothing fails while
it holds. Sequencing this after the delivery work is right; sequencing it after
launch is not, because re-keying live tenant ciphertext is exactly the migration
that pre-launch is the cheap moment for.

## 5. What a zone physically is

**Either** a zone is a failure domain with its own Postgres, object storage and
worker fleet, and cross-zone traffic is only the forward hop, **or** a zone is a
placement hint over shared storage.

**Recommendation: a zone is a failure domain, and nothing in the tree records
one today.** MEASURED, there is no region, zone or datacenter concept anywhere
(zero occurrences; `data-system.md:552-555` and `distributed.md:93` say the same
independently). That means the term is currently free to define, and the two
definitions have opposite consequences for every decision above: the
failure-domain reading makes decisions 3 and 4 blocking, the placement-hint
reading makes them irrelevant and makes the platform one shared-fate system with
latency optimisation. The failure-domain reading is the one worth the cost. This
decision must be recorded as an ADR before any of the others are implemented,
because all four inherit from it.

## 6. What a gateway does when the exception overlay is stale

**Either** a stale overlay is ignored (serve everything, log loudly), **or** a
stale overlay past some age degrades or blocks.

**Recommendation: ignore it, log loudly, and alarm - but only once the overlay
carries a generation and a not-after timestamp**, so "no exceptions" and "no
answer" are distinguishable bytes. MEASURED, the current pull already fails open
on a missing row (`registry.rs:914-936`) and fails closed on a bad value, and the
open direction costs revenue rather than exposing data (`enforce.rs:26`, `:44`).
Blocking on a transport property would convert a billing outage into a total
outage. The part that is not optional is the liveness field; without it this
decision cannot be implemented in either direction.

---

# What to measure first

These three numbers gate the design. Two of them cannot be taken from this
repository at all, which is itself worth stating.

**1. Real directory entry size, measured against the `apps` table.** Build the
packed encoder, run it over a real `zeroship.apps` join, and report the byte
distribution. The 112-byte figure in Part 3 is an ESTIMATE and the MEASURED JSON
upper bound is 347; the factor of 3.1 between them decides whether replicate-whole
survives at 10M hosts. The measurement is cheap - it is an encoder and a query -
and it is the only one that changes the shape of the design rather than its
parameters. Nothing else on this list should be done first.

**2. App creation and mutation rate.** Chunk count, root republication frequency
and gossip fanout all follow from how often the directory changes, and this tree
has no instrument for it. `zeroship.apps` has `created_at` and `updated_at`
columns (`db/migrations-ts/20260702000200_control_tables.ts:163-164`), so the
creation rate is a query away on any populated database; the mutation rate is
harder, because MEASURED the spend evaluator writes an `app_spend_state` row for
every app on every tick (`spend.rs:408-420`) and those writes are not directory
mutations. The measurement to build is "how many entries would have changed since
the last root," not "how many rows were written."

**3. p50 and p99 manifest and `.zship` size over a realistic corpus.** MEASURED
here over 31 examples: manifests min 382, median 999, mean 1216, max 6484;
artifacts min 4356, median 31632, max 650908, with ten of 31 above 130 KB. That
corpus is probes and demos and its tail is almost certainly wrong in both
directions. The p99 manifest is the number that settles open decision 2, and the
p99 artifact is the number that would reopen the P2P-swarming rejection if it
were three orders of magnitude larger than measured here.

**What cannot be measured from this repository.** There is no deployed fleet, no
zone abstraction, and no app count. Every aggregate figure in this document -
bytes per poll at a million apps, directory size at ten million - is arithmetic
on measured per-entry costs and an assumed population, and is tagged DERIVED
where it appears.

---

# Corrections made while designing this

Recorded rather than quietly applied, because this repo's failure mode is a
corrected number that invites the same error back.

## Substantive errors

**1. `DbResourceKey` is a digest BECAUSE it reaches logs, not so that it cannot.**
An early draft repeated a claim from `docs/architecture/data-system.md:85-87`,
which says: "This is the discipline `DbResourceKey` already applies to DSN
passwords: a digest chosen so the secret 'cannot reach `Debug` or a log line'
(`crates/zeroship-plugin-db/src/service.rs:46`)." MEASURED, the source says the
opposite, at `service.rs:44-51`:

```
//! # What a `DbResourceKey` is for
//! ...
//! different deploys. It is a digest, not the URL, because it reaches `Debug`
//! output and logs and a DSN carries a password.
```

The reason for the digest is that the value *does* reach `Debug` and logs. The
quoted phrase "cannot reach `Debug` or a log line" appears nowhere in the source,
and the cited line `:46` is the "It is the identity of one database's resources"
sentence, not the reason at `:50-51`. This is the exact failure mode this
document's tagging discipline exists to prevent: a design intent transcribed as a
guarantee, with quotation marks that make it look measured. **It is an inversion,
and the doc it came from still carries it.** It would pass
`tests/doc_citation_gate.sh` green, because `:46` is a real line in a real file.

**2. There is no `Datastore` entity, so `cluster_id` cannot be "in" it.** An
early draft asserted that `cluster_id` lives on the `Datastore` entity. MEASURED:
`grep -rn "struct Datastore\b" crates/ --include='*.rs'` returns nothing, and
`grep -rln datastore db/migrations-ts/` returns nothing. `DatastoreId` and
`ClusterId` exist only as wire types in `crates/zeroship-cdc-wire`
(`src/ids.rs:192` and `:214`, consumed in `src/frame.rs:200`, `:404` and
`src/request.rs:180`). Both spellings are open questions - issue #178 records
that `ds_` contradicts its own proposal and `clu_` is an invention. A design that
placed zone or cluster identity on a datastore entity would have been building on
a type that does not exist.

**3. Gossip was dismissed for the wrong reason, then adopted for a different
one.** The first pass rejected gossip as "a distributed system added only to
invalidate a cache." That is a defensible objection to invalidation and
irrelevant to the actual blocker, which is that `HashRing` has no mutation method
(`proxy.rs:60-121`, six methods, none taking `&mut self`) and is built once at
`main.rs:441`, so adding a worker requires restarting every gateway. Membership
alone justifies the mechanism at ten apps; invalidation then rides free. The
error was scoping a mechanism to one of the two problems it solves.

**4. An empty enforcement exception list "fails safe" is the wrong word.** It
fails **open**: every app serves unrestricted. MEASURED, the two gates it disarms
are `402 SPEND_LIMIT` and `402 ACCOUNT_SUSPENDED` (`enforce.rs:26`, `:44`),
neither an authentication or authorization boundary, so the loss is revenue and
unbounded infrastructure cost. Open is the right availability choice here and
"safe" is the wrong word for it, because the word invites a reader to skip the
liveness requirement that makes "no exceptions" distinguishable from "no answer."

**5. The directory was first described as ONE blob.** That does not survive
churn: a single object must be republished in full for every spend-state flip,
deploy and app creation, on a platform where those are continuous. Hence the root
plus chunks in Part 3.

**6. The chunk key is the host, not the app id.** The first chunking draft keyed
on `sha256(app_id)`, which reads natural because the directory is "about apps."
It destroys the property chunking exists to preserve: a gateway holding only a
`Host` header cannot compute an app-id-keyed chunk index, so absence stops being
locally decidable and the 404 stops being sound. Caught by asking what a gateway
holds at the moment it must answer, rather than what the entry is about.

**7. The worker does NOT merely extract two hashes from the manifest, as of
HEAD.** An earlier draft said the worker carries a whole manifest on every
`/internal/versions` poll to pull out the worker-entry blob hash and the runtime
descriptor hash. MEASURED, there is a third consumption site: `worker/src/sync.rs:446`
clones the whole manifest and passes it to `cache::load_app`
(`crates/zeroship-worker/src/cache.rs:567`, parameter at `:574`), which compiles
it at `:586` into the per-isolate declared policy. That behaviour landed in the
HEAD commit itself (`58deea301`). The design conclusion is unchanged and slightly
strengthened - both tiers need the manifest, neither needs it re-transmitted on a
timer - but the sentence as drafted described the tree of one commit earlier.

**8. "Only exceptions have rows" is false for spend.** `touch_state`
(`spend.rs:494`, called at `:420`) upserts a row on every tick for every app, by
explicit decision documented at `:408-419`. The exception predicate is
`state <> 'allow'`, not row existence. Had this been written as "rows are rare,"
a reader would have sized the exception feed by row count and been wrong by the
whole app population.

**9. "Derive `sector_identifier` at the gateway" is not free.** MEASURED, the
gateway holds neither the scheme nor the base domain: grep for `app_base_domain`
under `crates/zeroship-gateway/src` returns nothing, and `extract_app_name`
(`router/dispatch.rs:49`) parses the first label of `Host` and nothing else. The
claim as first drafted read as though the gateway could recompute the sector from
what it already has. It cannot, today.

**10. "An app cannot move between zones" does not follow from key derivation.**
The HKDF salt is the `app_id` (`keys.rs:11-14`, `:401-409`), which is stable
across a move. The real constraints are root-key presence per zone and the absent
rotation surface (`:55-58`). The corrected statement is narrower and more useful.

## Counting and citation errors

**11. Four-table join was five tables.** `get_routes` names `apps`,
`app_oauth_clients`, `app_spend_state`, `app_members` and
`creator_billing_status`; the endpoint as a whole issues three statements over
eight tables.

**12. "The manifest is DUPLICATED" was an undercount by four.** Six live copies:
tar member, blob store, `apps.manifest_json`, `app_deploys.manifest_json`, inline
on `RouteEntry`, inline on `AppVersionInfo`. Only the last two are re-transmitted
on a timer. Naming the count changes what a fix has to cover.

**13. "Nine fields, ONE is about routing" was an undercount by one.** `name` is
the routing key (`sync.rs:43`, `:220`, `:239`) and `manifest` is the dispatch
policy compiled at `:226`. Two of nine do work on the request path; the original
phrasing reads as though eight are dead weight.

**14. "`assets` is the only unbounded field and dominates" - both halves
refuted.** Seven manifest fields are unbounded; `assets` is 26.1% of the corpus
against `resources` at 45.9%, and is beaten 4:1 by `resources` in the largest
manifest measured. The design conclusion survives but now rests on marginal cost
(240 bytes per asset, build-driven cardinality) rather than on a share figure
generalised from the single most asset-heavy example in the tree. This is the
more dangerous kind of error, because `ssg-docs` really is 66.4%.

**15. The `.zship` size sample was unrepresentative at the top.** 4356 / 28639 /
32758 are correct for `ssg-docs` / `auth-notes-db` / `workflow-probe` and are the
small end of a population running to 650908 bytes, median 31632, with **ten** of
31 above 130 KB (an earlier draft said seven). The anti-swarming argument
survives comfortably; the range as quoted understated the maximum twenty-fold.

**16. The `Manifest` struct has 19 fields, not 16** (`manifest.rs:33-173`).

**17. The gateway PG pool has five calling modules, not two and not six.**
`db::checkout(` is called from `auth_token.rs`, `router/auth.rs`,
`browser_auth.rs`, `backchannel_logout.rs` and `router/dispatch.rs`, across 25
call sites including tests. `lib.rs` names it only in a rustdoc link (`:177`).
An earlier version of this very list said six modules and 26 sites, counting the
doc link as a caller and the definition in `db.rs` as a call.

**18. Advisory locks reach much further than "6+ modules."** 73 source files
across 16 crates name an advisory-lock function. "6+" is technically true and
reads as an estimate of six; the real reach strengthens the argument against etcd
rather than weakening it.

**19. `egress_rules.rs` has 11 `app_id: Uuid` occurrences, not fifteen.** Two are
the `pub app_id: Uuid` struct fields at `:123` and `:167`; nine more are
parameters. The point - that app identity travels as a raw `Uuid` rather than a
typed id - stands, and the migration service is the sharper example
(`crates/zeroship-migrate-server/src/api.rs:139`, `:179`, `:190`).

**20. The app id is a raw v4 UUID and `new_app_id` is dead.** MEASURED,
`zeroship.apps.id` is `t.uuid().notNull().default(uuidV4())`
(`db/migrations-ts/20260702000200_control_tables.ts:152`) and the only production
insert omits `id` entirely (`registry.rs:280`), so the database default supplies
it. `new_app_id()` exists at `typed_id.rs:357` and mints `app_<base62(uuidv7)>`,
but grep finds no caller outside the module's own test at `:795`. So the identity
flowing through every wire type carries no time ordering and no locality hint,
and `oac_<base62>` inherits that. Relevant to chunking only in that it rules out
any scheme keyed on id locality.

**21. Line drift, measured.** Numbers carried in from the design conversation
that were wrong: `types.rs:212` for `RouteEntry` (the struct is `:213-259`;
`:212` is the derive). `typed_id.rs:334` and `:341` for the mint and decode (they
are `:337` and `:346`); `:229` for the prefix constant (it is `:237`; `:229` is
inside the doc block). `app_oauth_client.rs:146-151` for `sector_identifier`
(doc `:146-148`, function `:150-152`; a later draft's `:151-153` was also wrong).
`gateway/src/sync.rs:305-325` and `:347-360` for the transport (validation is
`:321-338`, the request `:347-388`). `registry.rs:809` as the producer of
`/internal/routes` (that is `get_routes`; the snapshot producer is
`get_gateway_snapshot` at `:945`, and both are real functions so the citation
would have passed the gate while pointing at the wrong one). `spend.rs:410-411`,
`:419-420`, `:411-418`, `:492-517` (measured `:240`, `:420`, `:408-419`, `:494`).
`db_posture.rs:101-176` was right; `keys.rs:401-408` is `:401-409`.
`compiled.rs:443-452` for `lookup_asset_for_static` is `:444`. `proxy.rs:40-45`
for the `Stream` enum is `:22-24`.

**22. Citations that reproduced exactly**, listed so a reader can calibrate how
much of the conversation held: `gateway/src/sync.rs:292` (the poll URL),
`:263-269` (inside the linear scan), `worker/src/sync.rs:49` (`get_blob`),
`proxy.rs:60-120` and `main.rs:441` (the frozen ring), `db_posture.rs`
(REPLICATION plus BYPASSRLS), `typed_id.rs:237` (the prefix constant),
`limits.rs:11` (the 1 MiB cap), and the three manifest sizes 529 / 1066 / 1508
with `assets` at 708 of 1066 in `ssg-docs`.

**23. Derived byte figures that shifted on re-measurement, three times.** An
early draft put the mean full `RouteEntry` at 1640 bytes and its non-manifest
scalars at 390-402 (one draft said 440). The version committed as `ed161c341`
said 1669 and 360, the directory JSON entry 493, and the manifest "between 57%
and 94%" of an entry. The per-asset and per-resource marginal costs went 243/59
to 240/57, and those two reproduce exactly.

**None of 1669, 360, 493 or "57%" reproduces**, and that is the finding, not the
size of the gap. Re-run against the same 31 stored manifests with the field
values the document itself names elsewhere (the 142-byte breakdown pins
`sector_identifier` at `https://myapp.zeroship.ai`, 25 characters), the mean
entry is **1588** and the non-manifest overhead is a hard **372** - hard, because
every value in it is fixed, so "nearly constant" was already a tell that
something unnamed was varying. 360 is not reachable at all with a 25-character
sector: the floor is 368, at a one-character app name. The manifest share runs
**51% to 95%**, not 57% to 94%; 57% would need a smallest manifest near 477
bytes, and the measured smallest is 382. The 493-byte directory entry could not
be reconstructed from any field set tried, which is the real defect: the shape
was never written down, so the number could not be checked, only believed.

The repair is to pin the construction rather than to publish a better number.
Both figures now carry the exact object that produced them, and both are
re-runnable from the artifacts in `examples/*/dist/`. Downstream figures moved
with them: 1.67 GB to 1.59 GB, 1.55 GiB to 1.48 GiB, 330 MB/s to 318 MB/s, "15x
smaller" to 14x, and the 112-to-JSON band from 4.4x to 3.1x. **No conclusion in
this document changes**, which is exactly why the error survived: every one of
these numbers was load-bearing for an argument that a 5% error could not move.

## Found by the adversarial re-audit of `ed161c341`

**24. Nine wrong citations at six sites, in a document whose own opening says the
line is a courtesy.** `crates/zeroship-worker/src/cache.rs:576` for the
`manifest: &Manifest` parameter (it is `:574`; `:576` is the closing paren and
return type) - cited three times, in Part 1, in open decision 2 and in
correction 7. `crates/zeroship-gateway/src/oidc_rp.rs:961` for
`APP_SESSION_COOKIE` (`:962`; `:961` is its doc line).
`crates/zeroship-bundle/src/compiled.rs:376-377` for a clone of three things, the
third of which is at `:378`. `sync.rs:279-281` for the sleep that precedes the
first fetch (the `sleep` is `:282`; the cited range stops one line short of the
statement the sentence is about). And `docs/architecture/data-system.md:550-553`
for a sentence quoted verbatim - "no region or datacenter concept anywhere in the
tree" - which is at `:554`, outside the range, cited twice. A quoted string
outside its own citation is the sharpest form of this error, because the quote
looks like proof that the line was opened.

**25. The worker's poller was cited at its neighbour.** `worker/src/sync.rs:122`
is `start_sync`, which spawns the PER-THREAD reconcile loop; the process-wide
version poller is `start_version_poller` at `:104` into `version_poll_loop` at
`:129-231`. The cited range `:122-231` therefore opened on the wrong function and
excluded the right one's entry point, while still covering the loop body - which
is why it read as correct. The claim it supports (one poller, shared through
`SharedVersions`, HTTP not multiplied by thread count) is unaffected.

**26. The advisory-lock reach was undercounted by four crates.** 73 files is
right; **16 crates**, not 12. The list omitted `zeroship-authn`,
`zeroship-migrate`, `zeroship-migrate-node` and `libs/compio-postgres` - and the
last of those is the interesting one, because a `crates/`-only scan yields 69
files across 15 members and neither of that pair's numbers is the pair that was
published. This is correction 18 committing, at smaller scale, the same error it
was written to fix.

**27. The provenance paragraph went stale within hours of being written.** It
described a dirty working tree of nine modified files as a caveat on four line
numbers. Those nine were committed the same day as `cb0742195` and `825de4112`,
so the caveat pointed at a condition that no longer existed while the numbers it
warned about were fine. A provenance note that names a transient state is a note
that expires; this one now names the commits instead.

**28. "Nothing in the tree ever writes a non-empty `runtime_assets`" is true of
producers and false as written.** `crates/zeroship-core/tests/types_test.rs:689`
constructs one with an entry, to exercise variant validation. The design point
(no production writer, so the manifest is immutable-after-deploy today) stands;
the universal quantifier did not.
