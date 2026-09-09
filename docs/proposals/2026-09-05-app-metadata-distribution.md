# App metadata distribution

**Status. PROPOSED. NOTHING IN THIS DOCUMENT IS IMPLEMENTED.** No directory
object, no chunking, no gossip, no anycast, no zone, no manifest-by-digest and
no exception feed exists in the tree. What exists is the mechanism this document
proposes to replace, plus two pieces of machinery that a replacement would reuse
rather than build: the content-addressed blob store with its two-tier edge cache,
and the `oac_` client-id codec. Every sentence describing running code is tagged
MEASURED and names the file and the symbol it was read from, opened on
2026-09-05. Every sentence describing the proposal is tagged DESIGNED. Read the
difference as load-bearing: this repo has a documented case of a design sentence
in the present tense being built on as though it described the tree.

---

## This document deliberately carries no magnitudes

**Do not add byte counts, entry sizes, corpus statistics, percentages, ratios,
population figures or chunk counts back into this file.** They were here. They
were wrong, repeatedly, in both directions, and four consecutive commits after
this document first landed changed no code at all - they existed only to repair
figures the previous repair had left stale. That loop does not converge, because
each repair is written in the medium that caused the problem.

What replaces a number here, in order of preference:

- **The shape.** What scales with what, which term dominates, and where the sign
  flips. "Resident cost scales with host count times entry width, and the packed
  record is dominated by its two digests" survives every re-measurement. The
  product does not.
- **The named instrument.** A file plus a SYMBOL, so a reader re-derives instead
  of trusting. The name outlives the value behind it.
- **A gate arm.** Where an argument DEPENDS on a quantity holding, the quantity
  belongs in a test that re-measures it on every run, never in prose. Every such
  quantity in this document is listed under "Gate arms this design requires,"
  with what it enumerates and what floor it clears. None of them states a value.
- **A reproducible script**, where a measurement was genuinely made. Where a
  figure came from a one-off that no longer exists, the figure is gone and the
  arm that would reproduce it is named instead.

If you find yourself wanting to write a number here to make an argument land,
the argument is not yet in a durable form. Write the arm.

---

## The problem, in one paragraph

Per-app metadata is the state a gateway or a worker must hold before it can
serve one request for an app it has never seen: which app a host maps to, what
its dispatch policy is, whether its creator has paid, and which deploy is live.
Today exactly one mechanism carries all of it. The control plane materialises a
complete table of every app on the platform, and every edge process downloads
that entire table on a timer, revalidates it, recompiles it, and swaps it
wholesale. The cost per edge process is O(all apps), paid again every interval,
whether or not anything changed, and the largest term in it is a per-app
manifest that has nothing to do with routing. That shape is correct, simple, and
disqualified by the target of millions of apps. **The zone dimension is not what
breaks it.** A single-zone deployment at the platform's own stated target is
already dead under this mechanism; adding zones only multiplies the number of
processes paying the same bill. The pull shape therefore has to be replaced
whether or not the platform ever runs in two buildings, which means the work is
not blocked on a placement design.

---

## How to read this document

**Provenance.** Every citation below was re-derived by opening the file on
2026-09-05. The first pass ran against `58deea301` on `main` with a dirty
working tree; those modifications landed the same day as `cb0742195` and
`825de4112`, so the readings taken from them describe committed code rather than
an unsaved edit. The only one of them cited here is
`crates/zeroship-gateway/src/router/dispatch.rs`, for the `enforce::check_account`
and `enforce::check_spend` call sites and `extract_app_name`; those were
re-opened after the commit and hold.

**Re-audited on 2026-09-05 against `ed161c341`**, the commit that added this
file. That pass opened every citation a second time and found wrong citations,
wrong counts, an unreproducible measurement cluster and an over-general claim,
all recorded in the corrections section below rather than silently repaired.
Read that section before trusting anything here: this document has been wrong
about its own measurements more than once, which is why it now carries none.

**A third pass, also on 2026-09-05, settled the replication envelope and the
chunk count.** It found something worse than a wrong number: the chunking
section preserved the authoritative negative answer in the branch where a node
holds the chunk and said nothing about the branch where it does not, which is
where the property fails silently (correction 30). It also found that a
per-entry size had been derived and never multiplied by any budget a node
actually has (29), that the chunk key is not canonical because the tree
disagrees with itself about the case of an app name (31), and that two design
rejections rested on a measured corpus while enforced ceilings sat far above it
(32, 33). Corrections 29 to 33.

**Tags, applied per claim.**

- **MEASURED** - the code was opened, or the command was run, and the claim came
  out of a file rather than out of a recollection.
- **DERIVED** - arithmetic or inference over measured inputs. The assumptions are
  named inline; where the population is assumed, that is said in the same
  sentence.
- **DESIGNED** - a shape this proposal argues for. It does not exist.
- **ASSUMED** - a belief bound to nothing.

**On citations.** `tests/doc_citation_gate.sh` checks that a cited path exists.
It does not check, and by a decision recorded in its own header will not check,
that a cited line is the right one. Its header records the failure that decision
was re-measured against: AGENTS.md's schema-epoch paragraph carried a set of
line citations of which most were wrong, and `docs/architecture/data-system.md`
drifted independently over the same code. So **this document cites paths and
symbols and no line numbers at all.** Where a line matters to an argument, the
code is quoted.

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
        control/src/registry.rs -- one statement, several tables,
        no filter but `WHERE a.archived_at IS NULL`
               |
        Registry::get_gateway_snapshot()   control/src/registry.rs
        + further full-relation queries, one response
               |
        GET /internal/routes    control/src/internal.rs::get_routes
        no ETag, no If-None-Match, no cursor, no since-token, no page
               |
        every poll_interval, per gateway, the WHOLE table
        gateway/src/sync.rs::sync_once, interval from
        gateway/src/config.rs::poll_interval
               |
        RouteCache::update()   gateway/src/sync.rs
        per app, per poll:  validate()  ->  CompiledManifest::compile()  ->  clone
               |
        *self.routes.write() = compiled;  *self.name_index.write() = name_idx
        wholesale replacement, never a diff
```

## `RouteEntry`: what the fields are for, and which the request path reads

**STALE IN ONE FIELD, AND EVERY LATER MENTION OF IT INHERITS THAT.**
`api_key_hash` is no longer on `RouteEntry`, `check_api_key` no longer exists,
and neither does the plaintext `zeroship.apps.api_key` this document's cost
argument assumed a cold app would have to check. The whole app-level key is
deleted; `db/migrations-ts/20260905000200_drop_app_api_key.ts` records why the
platform owns no such credential. Read every `api_key_hash` measurement below -
the field inventory, the payload table, the cold-app `X-Api-Key` cost, and arm
A7 - as a record of what was true when it was taken, not as a description of the
struct. The arms that DERIVE the field set from the struct rather than from
prose still measure the real thing; the prose does not.

MEASURED. `RouteEntry` is declared in `crates/zeroship-core/src/types.rs` with
the fields `name`, `plan_id`, `api_key_hash`, `deploy_hash`, `manifest`,
`oauth_client_id`, `sector_identifier`, `spend_state`, `account_state`. The
struct is the authority on how many there are; every claim in this document that
depends on the field set says so and is gated by an arm that derives the count
from the struct rather than from prose.

MEASURED. The request path reads only `name` and `manifest`.

- `name` is the routing key. It is indexed into `RouteCache::name_index`
  (`crates/zeroship-gateway/src/sync.rs`, a `RwLock<HashMap<String, Uuid>>`
  populated in `RouteCache::update`) and resolved per request by
  `RouteCache::lookup_by_name`, called from
  `crates/zeroship-gateway/src/router/dispatch.rs`.
- `manifest` is the dispatch and authorization policy, compiled by
  `CompiledManifest::compile` inside `RouteCache::update`.

The remaining fields ride this feed for a reason unrelated to routing: it is the
only push channel from the control plane to the edge. `api_key_hash` is a
credential the gateway validates offline (`crates/zeroship-gateway/src/auth.rs`,
`check_api_key`). `plan_id` is pricing. `oauth_client_id` and `sector_identifier`
are OAuth identity. `spend_state` and `account_state` are billing enforcement
applied before dispatch. `deploy_hash` names the live deploy.

MEASURED, and it matters for any replacement. `spend_state` and `account_state`
default to the permissive value on absence: `SpendState`'s `#[default]` is
`Allow` and `AccountState`'s is `Active` (both enums in
`crates/zeroship-core/src/types.rs`), and both fields carry `#[serde(default)]`.
A snapshot that loses a field, or an app with no billing row, is unrestricted.
That is deliberate, documented in place, and the right default for the common
free-tier case. It is stated here because any replacement transport inherits the
same direction of failure.

## Where the fields come from: one statement over several tables

MEASURED. `Registry::get_routes` (`crates/zeroship-control/src/registry.rs`)
issues a single query whose driving relation is `zeroship.apps`, with top-level
`LEFT JOIN`s onto `zeroship.app_oauth_clients` and `zeroship.app_spend_state`,
and a `LEFT JOIN LATERAL` that itself joins `zeroship.app_members` to
`zeroship.creator_billing_status`. The only filter is
`WHERE a.archived_at IS NULL`. No app filter, no limit, no key range, no cursor.

**Correction.** The design conversation carried this as a smaller join than it
is. The undercount does not change the argument, but it was not checked before,
so it is stated rather than silently fixed.

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

MEASURED. The reason is written above it in `registry.rs` and is worth repeating,
because a distribution redesign will hit it again. `account_state` is
CREATOR-keyed; there is no `apps.creator_id` column, so the control plane reaches
it through the app's `app_members(role='owner')` row, and the schema permits more
than one owner row per app. Without the collapse a fan-out returns several rows
for one app, and because `get_routes`'s row loop does `map.insert(id, ...)` into
a `HashMap`, the survivor is arbitrary. The comment says the consequence plainly:
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

MEASURED. `GET /internal/routes` is routed in
`crates/zeroship-control/src/main.rs` and handled by `internal::get_routes`
(`crates/zeroship-control/src/internal.rs`): check the shared control key, call
`get_gateway_snapshot`, `HttpResponse::Ok().json(&snapshot)`. Grepping
`internal.rs` for `etag`, `ETag`, `If-None-Match`, `since` and `cursor` returns
nothing.

MEASURED. `Registry::get_gateway_snapshot` (`crates/zeroship-control/src/registry.rs`)
runs `get_routes` and then further full-relation queries: disabled, anonymized
and deletion-pending users joined to `app_user_identities`, and every
`token_revocations` row inside a fixed recent window. One response, several
statements, more tables than the route projection alone.

MEASURED. The consumer is `sync_once` (`crates/zeroship-gateway/src/sync.rs`):
build `{control_url}/internal/routes`, `serde_json::from_str` the whole body into
a `GatewaySnapshot`, hand it to `RouteCache::update_snapshot`. `start_sync` in
the same file sleeps `poll_interval` and loops. The interval is a *shared*
setting (`crates/zeroship-gateway/src/config.rs`, field `poll_interval`) whose
canonical environment name is `ZEROSHIP_POLL_INTERVAL`, pinned by a test in
`crates/zeroship-worker/src/config.rs`, so one variable moves both the gateway
and worker feeds. Read the default from the `#[config]` attribute, not from here.

MEASURED. `RouteCache::update` is not a diff. For every app in the new table it
calls `entry.manifest.validate()`, then `CompiledManifest::compile(&entry.manifest)`
(`crates/zeroship-bundle/src/compiled.rs`), which clones the assets map, the
runtime-assets map and the worker-code entry. The finished map replaces the old
one wholesale. Per-poll cost at the gateway is therefore O(all apps) in JSON
parse, validation, manifest compilation and allocation, paid whether or not a
single app changed, with peak memory briefly holding two full compiled tables.

MEASURED, and it couples badly with elasticity. `start_sync` sleeps *before* its
first fetch, and `crates/zeroship-gateway/src/main.rs` has one `sync::start_sync`
call with no eager first pull; the cache is constructed empty there
(`routes: sync::RouteCache::new()`). A freshly started gateway therefore has an
empty route table for at least one interval and every `lookup_by_name` misses;
`/readyz` reports not-ready until the first pull lands inside the staleness
budget (`crates/zeroship-gateway/src/health.rs`, `readyz` and `is_ready`, budget
from `staleness_budget` in `crates/zeroship-core/src/readiness.rs`, which is a
multiple of the poll interval with a floor - read the multiple from that
function). Cold start is a full-table download, and it gates readiness.

## The worker's feed is a sibling, on a different transport

MEASURED. `GET /internal/versions` is routed in
`crates/zeroship-control/src/main.rs` and handled by `internal::get_versions`.
Behind it `Registry::get_versions` runs full-relation statements:
`zeroship.apps a LEFT JOIN zeroship.plans p` with **no `WHERE` clause at all**,
and every row of `zeroship.app_egress_rules` ordered by app. Archived apps
deliberately remain in this projection, which is issue #89.

MEASURED. The worker consumes it in one process-wide poller:
`start_version_poller` (`crates/zeroship-worker/src/sync.rs`) spawns
`version_poll_loop`, whose fetch is `poll_versions`. The result reaches every
ntex thread through `SharedVersions` so HTTP traffic is not multiplied by thread
count; the per-thread reconcile loop is a separate spawn, `start_sync` into
`reconcile_loop`, and it reads the shared map rather than polling.

**Correction, and a sharp one.** The conversation recorded that the worker pulls
"over the same transport" as the gateway. MEASURED, it does not:

- The **gateway** hand-rolls HTTP/1.1 over a bare `compio::net::TcpStream`
  (`crates/zeroship-gateway/src/sync.rs`, `http_get_inner`), with the credential
  formatted straight into the request line as `Authorization: Bearer {auth_key}`.
- The **worker** uses a per-thread `cyper::Client`
  (`crates/zeroship-worker/src/sync.rs`, `this_thread_control_client`, used by
  `http_get` and `http_get_bytes_inner`), and the workspace builds `cyper` with
  the `rustls` feature (root `Cargo.toml`).

Same shared secret, same endpoint family, two clients with two different
transport ceilings. Any design that says "move both feeds onto X" is touching two
codebases, not one.

## The manifest exists in several places, and the count scopes the fix

MEASURED, tracing one deploy. The manifest body is durably stored in:

1. the first tar member of the `.zship`, required by name and capped at
   `MAX_MANIFEST_BYTES` (`crates/zeroship-bundle/src/limits.rs`, enforced twice
   inside `ingest` in `crates/zeroship-bundle/src/unpack.rs` - once against the
   tar header size and once against the bytes actually read);
2. blob storage under the manifest keyspace, written by `BlobStore::put_manifest`
   from `ingest`; the local layout is built by
   `LocalDiskBlobStore::manifest_path` (`crates/zeroship-bundle/src/blob.rs`) and
   the S3 key by the corresponding path builder in
   `crates/zeroship-bundle/src/s3_blob.rs`;
3. the `zeroship.apps.manifest_json` column, written by
   `Registry::set_deploy_with_manifest` and declared nullable in
   `db/migrations-ts/20260702000200_control_tables.ts`;
4. the `zeroship.app_deploys.manifest_json` column, written by the same function,
   declared **NOT NULL** in
   `db/migrations-ts/20260705000000_durable_workflows_journal.ts`, granted to
   `zeroship_worker` in
   `db/migrations-ts/20260818000200_worker_database_authority.ts`, and read by
   `crates/zeroship-control/src/workflow_instance_api.rs`.

And it is re-transmitted on a timer in two more places:

5. inline on every `RouteEntry` of every gateway snapshot
   (`crates/zeroship-core/src/types.rs`, produced by `Registry::get_routes`);
6. inline on every `AppVersionInfo` of every worker version feed entry
   (`crates/zeroship-core/src/types.rs`, produced by `Registry::get_versions`).
   Its own doc comment there says it is "Carried inline on every
   `/internal/versions` poll."

**Correction.** The conversation recorded "the manifest is DUPLICATED: on
RouteEntry AND inside the .zship," which understated it by several copies. The
durable copies are not the problem; the two wire copies are, and they are the two
this proposal deletes. Naming the split changes what a fix has to cover: removing
the inline field from `RouteEntry` alone leaves the worker feed still carrying a
full manifest every poll. An arm below enumerates every storing and transmitting
site so a copy added later cannot slip past this list.

## What the worker does with the manifest, corrected at HEAD

MEASURED, and this refutes a claim the conversation reached. The worker's
reconcile path touches `info.manifest` at more than one site
(`crates/zeroship-worker/src/sync.rs`):

- `worker_entry_hash` extracts the worker-entry blob hash;
- `runtime_descriptor_json` resolves the runtime descriptor, fetching one blob by
  hash;
- `reconcile_once` does `let declared = info.manifest.clone().unwrap_or_default();`
  and hands it to `cache::load_app` (`crates/zeroship-worker/src/cache.rs`) as its
  `manifest: &Manifest` parameter, which compiles it into the per-isolate declared
  policy.

MEASURED. `load_app`'s doc states the intent: the manifest "is what makes the
worker a real enforcer of the declared route policy rather than a tier that
trusts the gateway to have gated already." That behaviour landed in the HEAD
commit itself, `58deea301` ("feat(worker)!: refuse a dispatch the declared policy
does not admit").

So the sentence "the worker carries the entire manifest on every poll to extract
two hashes" was true before this commit and is false now. The worker needs the
whole manifest, for the same reason the gateway does. That strengthens rather
than weakens the design below: **both tiers need the manifest, and neither needs
it re-transmitted on a timer**, because it is immutable for the life of a deploy
and already has a content address.

## Sizing it: the shape, and the instrument that reproduces it

MEASURED, and stated as a procedure rather than a table so it can be re-run
rather than believed. Extract `manifest.json` from every built `.zship` artifact
under `examples/*/dist/` with `zstd -dc | tar -xO manifest.json` and measure
field-value lengths with `JSON.stringify`. What that procedure shows, and what
survives any re-run:

- The corpus spread is wide relative to its own median. Manifests in this repo
  differ from each other by more than an order of magnitude, so **no single
  example is a bound** and no three hand-picked ones are a population.
- **`assets` is not the dominant field across the corpus; `resources` is.** In
  the largest manifest measured the split inverts entirely, with `resources`
  dominating and `assets` a minority share. The single most asset-heavy example
  in the tree is the corpus MAXIMUM for the assets share, not the typical case.

**Correction, two of them, and the second is the more dangerous kind.**

1. The `.zship` sample used in the design conversation was drawn from the small
   end of the population and understated the maximum badly. A bound quoted from
   three hand-picked members of a population is not a bound.
2. The conversation concluded that `assets` is "the only unbounded field and
   dominates." MEASURED, both halves are wrong. Several manifest fields are
   unbounded maps or vectors - `resources`, `schemas`, `aliases`, `assets`,
   `runtime_assets`, `sourcemaps` and `schedules`, all declared in
   `crates/zeroship-bundle/src/manifest.rs` - and the share claim generalised the
   most asset-heavy example in the tree to the corpus. This is the dangerous kind
   of error precisely because the share figure was *correct about the one app it
   was measured on*.

MEASURED, and this is the claim that survives, about slope rather than share:
**an asset entry costs substantially more than a resource entry, and the two
counts are driven by different things.** `resources` grows with hand-written
server procedures and route rules; a person writes each one, and a large app has
a number of them a person could plausibly have typed. `assets` grows with build
output: one entry per emitted file, per pre-compressed variant, bounded only by
the size of the site. So `assets` is the field whose cardinality is set by a
build rather than by a person, and it is the one that reaches the manifest cap
first. That is the argument for digesting it, and it needs neither of the two
false claims. The marginal-cost ordering is the durable half and is gated by an
arm below; the per-entry byte figures are not restated here.

MEASURED. The `Manifest` struct's own field count is larger than the design
conversation recorded. The struct in `crates/zeroship-bundle/src/manifest.rs` is
the authority; do not transcribe a count from prose.

**The argument above is a slope argument, and there is a stronger one beside it
that this document did not make: the ENFORCED ceiling, not the measured corpus.**
MEASURED, `crates/zeroship-bundle/src/limits.rs` declares `MAX_MANIFEST_BYTES`,
`MAX_BLOB_BYTES` and `MAX_BLOBS_PER_DEPLOY`, the last enforced inside `ingest`.
Two things follow that the measured corpus cannot show, because every artifact in
it sits orders of magnitude below the ceiling:

1. **A legal deploy can put a full `MAX_MANIFEST_BYTES` inline on every poll,
   forever.** The manifest rides on every `RouteEntry`
   (`crates/zeroship-core/src/types.rs`) at the configured poll interval, so ONE
   app at the cap costs each gateway that cap per interval, times every gateway,
   for an object that is immutable for the life of the deploy and never changes
   between polls. That is not a projection about a hypothetical creator
   population; it is what ingest already accepts today. An arm below rules that
   no `RouteEntry` field's size is a function of deploy content, and it is red by
   construction until the digest lands.
2. **The two enforced caps contradict each other, and the digest is what
   reconciles them.** At the measured marginal cost of an asset entry, a deploy
   with `MAX_BLOBS_PER_DEPLOY` assets needs strictly more `assets` value than
   `MAX_MANIFEST_BYTES` permits for the whole manifest. So the blob-count cap
   permits a deploy the manifest cap refuses, and the binding limit is reached at
   an asset count well below `MAX_BLOBS_PER_DEPLOY`. A creator who builds an
   ordinary documentation site hits a refusal whose stated reason is manifest
   size and whose real cause is that the manifest carries a per-file map at all.
   Digesting the asset map out removes the map from the manifest budget entirely:
   `MAX_BLOBS_PER_DEPLOY` becomes the real ceiling as it was presumably meant to
   be, and the per-poll cost of an asset-heavy app falls from *refused* to one
   digest. The contradiction and its repair are both gated by an arm below that
   reads both constants from `limits.rs` and grows a real manifest against the
   real serializer until it is refused.

This is the argument for the digest that does not depend on a corpus at all. The
slope argument says assets will grow; the ceiling argument says the shape is
already broken at limits the code enforces today.

## The arithmetic that disqualifies the shape

DERIVED, and the construction is spelled out so it can be re-run. Wrap each
stored `manifest.json` body from the corpus above in a `RouteEntry`-shaped object
whose other fields carry the real value shapes - a name, a `pln_`-prefixed typed
id, two sha256 hex digests, an `oac_`-prefixed typed id, an apex-origin URL and
the two enum literals - and serialise compactly with the Rust field names as
keys. Three properties of the result are durable, and no product of them is:

- The non-manifest fields are a **fixed** overhead per entry, not a nearly-fixed
  one. Every value in that set has a shape determined by a definition in this
  tree, so the overhead is exactly computable and a phrase like "nearly constant"
  is a tell that something unnamed was varying.
- The **manifest dominates the entry** across the whole corpus, and its share
  rises with manifest size. Even the smallest manifest measured is the majority
  of its entry.
- The per-pull cost is therefore **the whole manifest corpus, per gateway, per
  interval**, produced by re-running a multi-table join and re-serialising every
  app, and consumed by parsing the same bytes and calling `validate` plus
  `compile` once per app.

**The assumption carrying the most weight is the population.** The examples in
this repo are probes and demos, not a sample of a creator population, and a real
corpus will have a heavier tail. Any per-entry figure taken from them is an
order-of-magnitude anchor and not a forecast. **The conclusion does not depend on
the constant**: across the entire range consistent with what was measured, a pull
at the platform's stated target app count is disqualifying at every value in the
range. That is why this document states the range's disqualifying property and
not a point inside it.

MEASURED, and this is the actual wall. It is not "zones make this hard." It is
that a gateway must download and recompile the state of every app on the platform
to serve one request for one app, and there is no mechanism in the code - no
ETag, no cursor, no since-token, no per-app fetch on the routing path - to ask
for less. The premise that actually carries the argument is the ABSENCE of a
delta affordance, and that is what the arm below rules on, one named affordance
at a time, rather than a byte total that goes stale.

## Two frozen things this design neither causes nor fixes

Stated here so that a later section does not appear to solve them by accident.

### The gateway's control transport is cleartext by construction, and declines to pretend otherwise

MEASURED. `validate_control_url` (`crates/zeroship-gateway/src/sync.rs`) accepts
only the `http` scheme and returns an error for anything else. Its doc states the
reasoning without softening it: `http_get_inner` opens a plain `TcpStream` and
writes the control key as an `Authorization: Bearer` header, there is no TLS on
the path at all, so an `https://` URL "does not get encrypted - it gets silently
downgraded, and the key goes out in cleartext," and because `Url::port()` returns
`None` for a scheme-default port, an `https://control` URL would connect on the
plaintext default port rather than the TLS one. The code matches the comment
exactly - `http_get_inner` is where the connect, the port default and the header
formatting all live.

This is not a claim of protection. It is a claim that the gateway declines to
*appear* protected. The control key crosses that hop in the clear on a trusted
network, or an operator terminates TLS in front. And as measured above the
worker's client is `cyper` with `rustls` available, so two services on one shared
secret have different transport ceilings. Any design that moves metadata onto a
new channel inherits this asymmetry; it is not resolved by changing what is sent.

### The hash ring is frozen at process start

MEASURED. `HashRing` (`crates/zeroship-gateway/src/proxy.rs`) exposes `new`,
`select`, `select_with_affinity`, `acquire`, `release` and `num_workers`, and
**not one of them takes `&mut self`**. There is no method that adds, removes or
replaces a worker. It is stored as a bare field, `pub hash_ring: proxy::HashRing`
(`crates/zeroship-gateway/src/lib.rs`), with no lock and no `ArcSwap`, so even if
a mutation method existed there is no interior mutability to reach it through. It
is constructed once in `crates/zeroship-gateway/src/main.rs` from a
comma-separated `--worker-urls` string parsed by `parse_worker_urls`, with a
`max_per_worker` literal in that same function and a `VNODES_PER_WORKER` constant
in `proxy.rs`. There is no worker registration endpoint on the control plane:
grep for `internal/workers`, `register_worker` and `worker_registry` across
`crates/zeroship-control/src` returns nothing.

MEASURED consequence: adding or removing a worker requires restarting every
gateway process. Combined with the cold-start property above, fleet elasticity
and the metadata pull are coupled in exactly the wrong direction - the cheapest
way to change worker capacity is the operation whose cost grows fastest with app
count. **This is a limitation at a handful of apps, not only at the target
scale**, and it is the fact that justifies gossip below. An arm asserts the ring
has no mutation path and no registration endpoint, so the day either lands is the
day this section needs rewriting and something says so.

### The gateway is further from stateless than it looks

MEASURED. `crates/zeroship-gateway/src/db.rs` carries the `Send + Sync`
`DbConfig` in shared state and builds the `!Send` `compio_postgres::Pool` lazily
per ntex thread into a `thread_local!`, because the pool cannot live in
`GateState`. The entry point is `db::checkout`. It serves
`zeroship.app_session_anchors` through `anchors::create`, `anchors::read_live`,
`anchors::update_rotated_family` and the delete paths
(`crates/zeroship-gateway/src/anchors.rs`).

**Correction.** The conversation recorded this pool as used by two modules. It is
called from more than that, across both production and test call sites - and the
count published in an earlier draft of this document was itself wrong in the
other direction, because it counted a rustdoc link in
`crates/zeroship-gateway/src/lib.rs` as a caller and the definition in `db.rs` as
a call. The durable claim is the one the miscount does not touch: **the gateway
holds meaningfully more per-request database state than a two-module reading
suggests**, which matters for any design that assumes gateways are cheap to place
near users. The count belongs in an arm that excludes doc links and the definer
by construction, and that arm is listed below.

---

# Part 2. The fields that need no delivery

Several of the `RouteEntry` fields do not have to be delivered at all, for two
different reasons: some are computable from the map key, and some are almost
always the default. In most of those cases something small survives the
derivation, and dropping it would be a regression rather than a saving. This part
derives each and states precisely what survives.

## `oauth_client_id` is a copy of a computable value

MEASURED. The per-app OAuth `client_id` is a prefix swap on the app UUID. The
mint and its exact inverse are, from `crates/zeroship-core/src/typed_id.rs`:

```rust
pub fn app_oauth_client_id(app_id: &uuid::Uuid) -> String {
    format!("{APP_OAUTH_CLIENT_PREFIX}_{}", uuid_to_base62(app_id))
}

pub fn app_id_from_oauth_client_id(client_id: &str) -> Option<uuid::Uuid> {
    let encoded = client_id.strip_prefix(APP_OAUTH_CLIENT_PREFIX)?.strip_prefix('_')?;
    base62_to_uuid(encoded).ok()
}
```

MEASURED. `APP_OAUTH_CLIENT_PREFIX` is `"oac"` in the same file, and its doc names
itself "the SINGLE source of truth for the prefix string," with the control plane
minting and the auth consent classifier decoding, "both through this constant so
they can never drift." The control plane's minter delegates rather than
reimplementing: `client_id_for_app`
(`crates/zeroship-control/src/app_oauth_client.rs`) calls `app_oauth_client_id`,
with a comment saying it does so "so the auth-side decoder is the exact inverse."

MEASURED. The decode is total and refusing, not partial. `base62_to_uuid` rejects
a tail whose length is not exactly the encoded-uuid width, any byte outside the
alphabet, and arithmetic overflow; the caller additionally requires the literal
`oac_` separator. The test `app_oauth_client_id_round_trips` in the same file
binds all of it: the round trip, the minted length, and `None` for
`"zeroship-builder-abc"`, `"oac_not-base62"` and `"oacsomething"`.

MEASURED. The gateway already holds the app UUID: it is the key of `RouteMap`
(`crates/zeroship-core/src/types.rs`, `pub type RouteMap = HashMap<Uuid, RouteEntry>`).
Shipping `oauth_client_id` alongside that key ships a function of the key.

MEASURED, the second consequence. `RouteCache::lookup_by_oauth_client_id`
(`crates/zeroship-gateway/src/sync.rs`) resolves a client id by scanning the whole
route table comparing strings. Its doc justifies this honestly and on premises
this proposal removes: "O(N) over the route table - BCL is a low-frequency webhook
surface, so a linear scan is cheaper than maintaining a third index. The table is
the same handful of apps `lookup_by_name` already serves." The single non-test
caller is the back-channel logout handler
(`crates/zeroship-gateway/src/backchannel_logout.rs`), which iterates unverified
`aud` candidates - so the scan today is O(candidates) times O(apps). The O(1)
replacement is one function call away: decode, then `RouteCache::lookup_by_app_id`.

## The `Option` conflates the identifier with the provisioned bit

MEASURED. `oauth_client_id` is `Option<String>` and the field's doc in
`crates/zeroship-core/src/types.rs` gives two reasons: a defensive one (an
empty-string `client_id` is a footgun, because a malformed token with an empty
`client_id` claim could match `""`) and a semantic one - it is `None` until the
control plane provisions the app's OAuth client, and consumers hard-fail rather
than bind to a falsy value.

Only the second reason survives derivation, and it survives entirely.

MEASURED. The `None` is produced by a `LEFT JOIN` miss, not by a policy: the
comment in `Registry::get_routes` says a provisioned app yields
`Some(oauth_client_id)` and `Some(sector_identifier)` while an un-provisioned app
with no extension row yields NULL and therefore `None`. That row can genuinely be
absent, because provisioning is best-effort relative to the create response:
`create_app` (`crates/zeroship-control/src/api.rs`) calls
`provision_app_oauth_client`, logs any error and returns the created response
anyway, with the comment "a DB hiccup here is logged + metered, and the next
deploy re-provisions." So "this app has an OAuth client" is a real one-bit fact
about state on the OP side, and it is not a function of the app id.

MEASURED. It is a fact about two rows, not one. `upsert_db_rows`
(`crates/zeroship-control/src/app_oauth_client.rs`) writes `zeroship.oauth_clients`
and `zeroship.app_oauth_clients` inside a single transaction, so the extension
row's presence is a sound proxy for the OP-side client existing.

MEASURED. Splitting the two facts changes one live behaviour. The test
`lookup_by_oauth_client_id_resolves_provisioned_app_and_skips_unprovisioned`
(`crates/zeroship-gateway/src/sync.rs`) asserts that an un-provisioned app's
`None` "is never matched by a real client_id." A pure decode-then-lookup would
match an unprovisioned app, because the decode only inspects the string.

DESIGNED. The replacement is therefore three steps, not two: decode, then
`lookup_by_app_id`, then check a provisioned `bool` on the entry - a bit, not a
`String`.

```
  today                                   proposed
  -----                                   --------
  aud "oac_XXXX..."                       aud "oac_XXXX..."
        |                                       |
        | scan the table, compare String        | app_id_from_oauth_client_id
        v                                       |   O(1), refusing
  Some((app_id, route))                         v
                                          Uuid -> lookup_by_app_id  (O(1))
                                                |
                                                v
                                          route.provisioned ? Some(..) : None
```

MEASURED migration cost worth naming: that test's fixture client id is a literal
that is not a valid base62 payload and decodes to `None`. A decode-based lookup
requires the fixture to mint a real id.

## `sector_identifier` is the apex origin, and custom domains must not change it

MEASURED. The value is one line
(`crates/zeroship-control/src/app_oauth_client.rs`):

```rust
pub fn sector_identifier(scheme: &str, apex_host: &str) -> String {
    format!("{scheme}://{apex_host}")
}
```

with the doc immediately above saying it is the app's apex origin, used for
pairwise scoping and as the post-logout redirect origin, and that "Custom domains
do NOT change the sector - all of an app's hosts share the apex." The unit test
`sector_identifier_is_apex_origin` in the same file pins the shape.

MEASURED. `ensure_app_client` treats `hosts.first()` as the apex; every other host
contributes only redirect URIs. The multi-host surface is unwired: the module
header records that it "is not yet wired to a production caller because there is
no custom-domain attach handler in the codebase."

MEASURED, why it must not vary per host. The sector is the salt half of the
pairwise-subject HMAC. `derive_pairwise` (`crates/zeroship-core/src/auth/mod.rs`)
hashes the canonical global user id, a colon, and the sector under the platform
pairwise secret, emitting `PAIRWISE_SUB_PREFIX` plus a base62 body whose length
rationale is stated beside the constant. Its doc states the property: the same
`(global_user_id, sector)` always yields the same subject, and two apps with
distinct sectors get different subjects for the same human. The gateway's
back-channel logout handler derives the same value to write the per-app
token-family marker (`crates/zeroship-gateway/src/backchannel_logout.rs`), and
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
(`crates/zeroship-control/src/lib.rs`) as `format!("{name}.{}", self.app_base_domain)`.
The gateway does not hold it: grep for `app_base_domain` or `base_domain` under
`crates/zeroship-gateway/src` returns nothing, and `extract_app_name`
(`crates/zeroship-gateway/src/router/dispatch.rs`) takes the first label of `Host`
without ever knowing the suffix. So deriving this field at the gateway means
adding scheme and base domain to gateway configuration - a real change - and it
stops being derivable the moment `name` becomes mutable or a custom domain
becomes the apex. There is no app-rename write today, but
`backchannel_logout.rs` already calls `name` "the renameable subdomain slug," so
the codebase does not consider it frozen.

DESIGNED, and narrower than "derive it": the sector is one string per app,
stable for the app's life and identical for every host it serves, so it belongs
to whatever carries app identity, not to a per-poll route row. Custom-domain
attach is unbuilt, so the decision costs nothing to make now and is expensive to
reverse later.

## `spend_state` and `account_state`: distribute the exceptions

MEASURED. Both enums default to their permissive variant and both fields carry
`#[serde(default)]` (`crates/zeroship-core/src/types.rs`).

MEASURED. Enforcement is a two-gate AND and each gate blocks on exactly one
variant. `check_spend` (`crates/zeroship-gateway/src/enforce.rs`) refuses only
`Block`, with a payment-required response carrying `SPEND_LIMIT`; `Warn` and
`Degrade` pass, Degrade being throttled elsewhere through the degraded registries.
`check_account` in the same file refuses only `Suspended`, with a payment-required
response carrying `ACCOUNT_SUSPENDED`; `PastDue` passes as the grace window.
Dispatch calls account first, then spend, at each of its call sites in
`crates/zeroship-gateway/src/router/dispatch.rs`.

MEASURED. They are keyed differently, and this is what a naive one-exception-list
design gets wrong. `spend_state` is per-**app**, joined on `s.app_id = a.id`.
`account_state` is per-**creator**, reached through the owner-membership
`LATERAL` quoted in Part 1.

**Correction, and the one that would have mis-sized the feed.** It is tempting to
say "only apps with a row need distributing." MEASURED, the spend evaluator
writes a row for *every* app on *every* tick. `SpendEngine::evaluate_all`
(`crates/zeroship-control/src/spend.rs`) selects `SELECT id FROM zeroship.apps`
and, when the state did not change, calls `touch_state`, an UPSERT performed
unconditionally by explicit decision - the comment above the call says "we
deliberately DO write every tick ... The single-row UPSERT is cheap; the freshness
is the point." So after one evaluator pass essentially every app has an
`app_spend_state` row carrying `'allow'`. **The exception set is a value
predicate, `state <> 'allow'`, not an existence predicate.** Had this been written
as "rows are rare," a reader would have sized the feed by row count and been wrong
by the whole app population. An arm below binds this against a live evaluator pass
so an "optimisation" that skips the unconditional UPSERT cannot silently change
the feed sizing.

MEASURED. `account_state` is the opposite shape. Its row is created only by the
dunning state machine, whose sole `INSERT INTO zeroship.creator_billing_status`
is in `AccountStatusStore::record_payment_failed`
(`crates/zeroship-control/src/account_status.rs`), reached only from
signature-verified Stripe webhook handling ("only a signature-verified Stripe
event ... No creator input sets it"). A creator who has never had a payment fail
has no row at all, so existence and exception nearly coincide - but the row
persists after recovery flips the state back to `active`, so `state <> 'active'`
is still the correct predicate.

MEASURED shape of what removing these fields saves. `RouteEntry` carries no
`serde(rename_all)` - the derive on the struct is only
`#[derive(Debug, Clone, Serialize, Deserialize)]` - so the JSON keys are the Rust
field names verbatim, repeated identically in every entry. The saving is the
key-plus-value string of each removed field, paid on every entry of every pull,
per gateway. Two of the four are fixed-shape identifiers and two are short enum
literals, so the saving is exactly computable from the definitions and is
therefore an arm, not a sentence.

## The empty exception list fails OPEN, and that is a property, not a bug report

State this carefully, because the tempting sentence ("an exception list is
fail-safe") is false and its opposite ("it is a security hole") overstates it.

DESIGNED characterization, on MEASURED gates. An exception list that arrives
empty - or does not arrive - serves every app unrestricted. That is fail-**open**
on billing enforcement: the platform keeps serving traffic it had decided to
block or throttle, and the loss is revenue and unbounded infrastructure cost, not
end-user data exposure. The gates it disarms are the `SPEND_LIMIT` and
`ACCOUNT_SUSPENDED` refusals in `crates/zeroship-gateway/src/enforce.rs`, neither
of which is an authentication or authorization boundary. It is the right
availability choice. It is the wrong word to call it safe. The recommendation
below is sound only while that aperture stays billing-only, so an arm enumerates
every gate reachable from the overlay and rules that none of them authenticates
or authorizes.

MEASURED. The current pull has the same open direction for a *missing row* and
the opposite direction for a *bad value*, and both halves are deliberate. The
row-mapping code in `Registry::get_routes` maps NULL to the permissive default in
both cases and maps an unrecognised TEXT value to the restrictive one: spend
"fails closed to Block (defensive - should never happen, the engine only writes
the four known states)", account "fails closed to Suspended". So today: unknown
value, fail closed; absent row, fail open.

DESIGNED. What changes under an exception feed is the *size of the aperture*, not
its direction. Today the fail-open case is "this app has no row," which the
evaluator closes on its next tick. Under an exception feed the case becomes "this
delivery was empty, stale, or lost," which is a transport property and can
persist. The current design also couples the two - routes and enforcement arrive
in one payload, so a gateway that cannot reach control serves nothing at all
(MEASURED: the cache is constructed empty in `crates/zeroship-gateway/src/main.rs`
and `start_sync` sleeps before its first fetch). Decoupling deliberately breaks
that coupling: routes could be fresh while the overlay is absent.

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
manifest's own digest. `canonical_manifest_for_hash`
(`crates/zeroship-bundle/src/unpack.rs`) strips the `deploy_hash` key and
canonicalizes, and `ingest` computes `deploy_hash = sha256_hex(&canonical_omit)`.
So a directory entry carrying `deploy_hash` already carries a pointer to the
manifest object, and no separate `manifest_digest` field is needed. That removes
a whole digest from the naive design's entry.

## The immutable half nearly exists, with three corrections

**Correction, and it matters for the scheme.** The conversation said
`manifest.json` is "already a blob." MEASURED, it is not, in three separate
senses, and all three are load-bearing.

1. **It is not under `blobs/` in the archive.** Every other archive member must
   match `blobs/<hex-sha256>` or `ingest` refuses it, validated by
   `blob::validate_hash_format`; `manifest.json` is a distinguished member outside
   that namespace.
2. **It is not in the blob keyspace at rest.** The trait comment says so:
   "Manifest storage - separate keyspace from blobs"
   (`crates/zeroship-bundle/src/blob.rs`). Local layout is
   `LocalDiskBlobStore::blob_path` versus `LocalDiskBlobStore::manifest_path`;
   S3 uses a `blobs/{hash}` key versus a `manifests/{app_id}/{deploy_hash}.json`
   key (`crates/zeroship-bundle/src/s3_blob.rs`).
3. **The stored bytes do not hash to their own key.** `ingest`'s final step sets
   `manifest.deploy_hash = Some(deploy_hash)`, re-serializes, and stores *that*.
   So `sha256(stored bytes) != deploy_hash`. Verifying the stored object requires
   stripping `deploy_hash` and re-canonicalizing first.

The conclusion the conversation drew - that the immutable half is nearly free to
move - survives, because `deploy_hash` *is* the digest and the gateway already
holds a hash-keyed cache. But the premise as stated would have produced an
implementation that verified stored bytes against `deploy_hash` and failed on
every manifest. That failure is exactly what the round-trip arm below rules on.

DESIGNED resolution: the manifest object moves into `blobs/` with the
`deploy_hash` field **removed from the stored body**, because the field is
redundant with the key. The alternative - every consumer runs
`canonical_manifest_for_hash` before comparing - is strictly more work at every
call site.

MEASURED, and this is what makes the move nearly free. The gateway already holds
a `BlobStore` (`crates/zeroship-gateway/src/lib.rs`) with a content-hash keyed
in-memory LRU (`BlobCache`, `crates/zeroship-gateway/src/blob_cache.rs`) over an
mmap-backed on-disk LRU (`DiskBlobCache`, same file). The disk tier `mmap`s the
file and hands the pointer to ntex as `Bytes::from_owner` (`mmap_to_bytes`),
making the page-cache to socket path zero-copy from userspace; publishes are
atomic through `reserve_temp` plus `publish_temp` and a pre-existing final file
is size- and hash-verified by `verify_file` before being trusted; concurrent cold
misses on one hash are collapsed by a per-process single-flight
(`DiskBlobCache::begin_refill`, consumed in
`crates/zeroship-gateway/src/router/static_serve.rs`, where a follower that wakes
to a failed refill re-loops and may become the next leader). `BlobStore::get_manifest`
already exists on the trait. A gateway that fetched manifests by hash would be
using machinery already built, already in the process, and already sized by an
operator flag.

MEASURED asymmetry, and a gap: `grep -rn BlobCache crates/zeroship-worker/src`
returns nothing. The worker fetches blobs by hash (`crates/zeroship-worker/src/sync.rs`,
in `runtime_descriptor_json` and in the worker-bundle fetch inside
`reconcile_once`) with no edge cache in front of them. That is not a decision
recorded anywhere; it is an omission a manifest-by-digest design would have to
close.

## The manifest carries an assets digest, not the map

DESIGNED. `Manifest.assets` and `Manifest.runtime_assets` are replaced by a
digest of a separately stored asset map. The manifest keeps inline the fields a
dispatch decision reads (`resources`, `worker`, `version`, `transformer`,
`runtime_descriptor`, `auth`, `net`) and grows one field naming the asset map by
content hash. The asset map becomes an ordinary blob under `blobs/<sha256>`,
fetched and cached by exactly the machinery that already fetches asset *bytes*.

MEASURED support that the split is clean today: no PRODUCER writes a non-empty
`runtime_assets`. Every producer writes an empty map - the two construction sites
in `crates/zeroship-bundle/src/manifest.rs`, the emitter in
`sdks/vite-plugin/src/zship.ts`, and the worker's own construction in
`crates/zeroship-worker/src/handler.rs`. One test in
`crates/zeroship-core/tests/types_test.rs` constructs a populated one to exercise
variant validation, which is why this says "producer" rather than "nothing in the
tree". Ingest *refuses* a non-empty one on a fresh deploy alongside a non-zero
`asset_version` (the fresh-deploy invariants inside `ingest`). The `env.assets.*`
namespace that would mutate them is listed in AGENTS.md as planned, not
registered. So the manifest is in fact immutable-after-deploy today - and because
that premise is what makes content-addressing the manifest legal at all, an arm
enumerates the writers rather than leaving it to a sentence.

DESIGNED, for when `env.assets.*` lands: the runtime asset map becomes a
*directory-side* object with its own version counter, never a field inside the
content-addressed manifest, because the moment it mutates the manifest stops
being addressable by its own digest.

## But both tiers genuinely need the asset map

This is the constraint that stops the digest from being a free win, and a design
that forgets it produces a gateway that answers not-found for every static file.

MEASURED. The gateway serves assets itself, with no worker involved.
`serve_resource_tree_static` (`crates/zeroship-gateway/src/router/static_serve.rs`)
resolves a `try` chain against the manifest's asset maps; `lookup_static_hit`
calls `CompiledManifest::lookup_asset_for_static`
(`crates/zeroship-bundle/src/compiled.rs`), which reads `runtime_assets` then
`assets`; the bytes come from the three-tier `fetch_static_bytes`.

MEASURED, and worse than the wire cost: the map is resident **twice per app per
gateway process**. `CompiledManifest::compile` clones both maps into the compiled
form, and the compiled form is stored in a `CompiledRoute`
(`crates/zeroship-gateway/src/sync.rs`) *alongside the original `RouteEntry`*,
which still owns its `Manifest`. A gateway holding N apps holds two copies per app
of every asset map.

MEASURED, and new at HEAD as recorded in Part 1: the worker needs the whole
manifest too, as its declared dispatch policy (`cache::load_app`).

DESIGNED resolution: both tiers keep reading a full manifest and a full asset
map, but obtain them by digest and cache them content-addressed, so N hosts
serving one app share one cache entry and a deploy that does not change the
assets does not re-transfer the map at all. The digest is the cache key; the
existing `BlobCache` is the cache on the gateway, and the worker gets the same
tier. This changes *where the map comes from*, not what a host can see.

DESIGNED fence, stated as an obligation rather than a fact. MEASURED, today a
manifest that fails `validate()` drops the app from the route table entirely, and
the comment in `RouteCache::update` says why: the manifest IS the authorization
policy, so a manifest we cannot interpret leaves no policy to enforce, and "the
only safe reading of 'no policy' is to stop serving the app, which makes dispatch
answer 404. Falling back to a permissive default here would serve every route the
manifest was meant to gate to anonymous callers." A fetched manifest or asset map
must inherit that disposition exactly: a fetch failure removes the app, not the
map - never an empty map that misses per path while leaving the RPC surface open.
**Nothing enforces this yet.**

## The directory must be complete

DESIGNED. The directory is the mutable half: one entry per routable host, holding
the scalars and the `deploy_hash` pointer.

It must be **complete** on every host that answers requests, and completeness is
not a performance property. It is what makes a negative answer *authoritative*. A
gateway receiving `Host: nope.zeroship.ai` has to decide between "no such app,
answer not-found" and "an app I have not heard about yet, retry or ask upstream."
Only a complete local index makes the first answer sound.

MEASURED, that this is already how it behaves and is therefore being preserved
rather than invented: `RouteCache::lookup_by_name`
(`crates/zeroship-gateway/src/sync.rs`) is two `RwLock` read guards and two
`HashMap` lookups and performs no I/O; on a miss, dispatch answers not-found
immediately (`crates/zeroship-gateway/src/router/dispatch.rs`) without consulting
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
  fail closed on every miss (refuse apps deployed seconds ago), and both are
  worse than a stale complete table.

## The directory must be chunked

Completeness at fleet scale cannot mean "one object." A single directory object
has to be republished in full for every spend-state flip, every deploy, every app
creation, on a platform where those events are continuous.

DESIGNED. The directory is a **root object plus N chunks**, where a host's chunk
is a deterministic function of the host itself:

```
   chunk_index = first k bits of sha256(host)

   root object                       chunk 0            chunk N-1
   +---------------------+           +-----------+      +-----------+
   | directory_version   |           | entries   |      | entries   |
   | k (chunk bits)      |   names   | for hosts | ...  | for hosts |
   | chunk_hash[0..N-1]  |   each    | whose     |      | whose     |
   +---------------------+   chunk   | prefix is |      | prefix is |
                             by hash | 0         |      | N-1       |
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
digest and the host is not in it, the host does not exist, and the not-found is
sound without consulting anything. Absence is decided against one object.

DESIGNED consequence that is easy to miss: **chunking by `app_id` would break
this.** A gateway holding only a `Host` header cannot compute an app-id-keyed
chunk index, so it could not locate the chunk in which the host would live, and
the negative answer would again require a full scan or an upstream call.
MEASURED support that the host is the right key: the gateway's primary index is
`RouteCache::name_index`, a `HashMap<String, Uuid>` keyed on name. The two other
lookups need no second index - `lookup_by_app_id` is reached only from paths that
already hold an app id, and `lookup_by_oauth_client_id` becomes an O(1) decode as
derived in Part 2.

DESIGNED, and not an invention: prefix-sharding a content-addressed keyspace is
already this tree's habit. `LocalDiskBlobStore::blob_path`
(`crates/zeroship-bundle/src/blob.rs`) shards blobs into buckets by a leading
slice of the hex hash. Directory chunking is the same discipline applied to a
lookup key instead of a content hash.

## But chunking breaks the negative answer, unless a third state is added

**This is a defect in the section above, not a refinement of it.** The sentence
two paragraphs up is conditional - "*if* it holds that chunk at the current
root's digest and the host is not in it" - and the document never says what
happens when the condition fails. That complement is where the whole property
dies, and the code has no way to represent it.

MEASURED, the shape of the hole. A lookup today has exactly TWO outcomes.
`RouteCache::lookup_by_name` is a `HashMap::get` returning `Option`, and dispatch
turns `None` into a not-found with no I/O. There is no third state because under
a single atomic full-table pull there cannot be one: a table that landed IS
complete, so "absent" and "absent from a complete table" are the same fact.
Chunking separates them and nothing in the code notices.

DERIVED consequence, and it is worse than a stale table. A gateway missing a
chunk answers **authoritative not-founds for every app in that chunk** - a
hash-random slice of the platform, spread across unrelated tenants and
indistinguishable at the edge from a correct negative. No tenant loses
everything, no pattern appears in any single app's metrics, and the fleet-wide
signal is a small uniform rise in not-founds, which is what a public edge sees
from scanners all day. The current design fails LOUDLY here (an empty table
answers not-found for everything, and `/readyz` says so); the chunked design
would fail *quietly and partially*, which is strictly worse.

MEASURED, that this is not hypothetical and the existing mitigation does not
generalise. `/readyz` is a **freshness** test, not a completeness test:
`is_ready` (`crates/zeroship-gateway/src/health.rs`) is
`!dev_escape && freshness.is_fresh(staleness_budget(poll_interval))`, over
`SyncFreshness` and the budget from `staleness_budget`
(`crates/zeroship-core/src/readiness.rs`). Under one atomic pull those coincide.
Under chunking a node holding a current root and all but one chunk is *fresh* and
*incomplete*, and `is_ready` returns true. The orchestrator would route traffic to
it, and it would serve confident not-founds.

DESIGNED repair, and it is the price of the negative answer rather than an
add-on. The reader tracks per-chunk residency against the root, and the lookup
has THREE outcomes, not two:

```
   chunk_state[i] : Missing | Held { root_version }

   held at CURRENT root, host absent   -> not-found. Authoritative. The
                                          design's whole claim, and only here.
   held at an OLDER root, host absent  -> not-found, but a KNOWN-STALE
                                          negative: the host may have been
                                          created since. Serve it, count it,
                                          alarm on the age.
   chunk MISSING entirely              -> NOT a negative. Unavailable, or a
                                          fetch, or an upstream ask. Never
                                          a not-found.
```

Three consequences follow, and each is a change to something that exists:

- **The root is fetched first and separately, and it is the completeness
  certificate, not merely an index of digests.** A node with no root cannot
  answer any negative authoritatively, because it cannot know which chunks it is
  missing. This is what makes the root the object worth signing.
- **`/readyz` must gain a completeness term**: ready means "I hold every chunk
  named by the current root," not "a pull landed recently." That is a change to
  `is_ready`'s inputs, and it is the mechanism that keeps the cold-start hole
  from becoming permanent and partial. The arm below withholds exactly one chunk
  and rules on all three outcomes, so a two-state implementation cannot pass.
- **The chunk key must be CANONICAL, and today it is not.** `chunk_index =
  first k bits of sha256(host)` is only deterministic if producer and consumer
  agree on the exact bytes. MEASURED, they do not agree on case:
  `extract_app_name` (`crates/zeroship-gateway/src/router/dispatch.rs`) returns
  the Host label verbatim, with no normalisation; the index is a case-sensitive
  `HashMap<String, Uuid>`; `Registry::create_app`
  (`crates/zeroship-control/src/registry.rs`) accepts any `is_ascii_alphanumeric()`
  name, uppercase included; and `apps_name_key`
  (`db/migrations-ts/20260702000600_constraints_indexes_fks.ts`) is a plain UNIQUE
  over `text`, which compares by bytes. So `MyApp` and `myapp` are two permitted,
  distinct rows resolving from ONE case-insensitive DNS name. Meanwhile the
  reserved-name check IS case-insensitive - `is_reserved_app_name`
  (`crates/zeroship-control/src/reserved_names.rs`) uses `eq_ignore_ascii_case` -
  so the tree already treats the label as case-folded where a name is *claimed*
  and as case-sensitive where it is *routed*. Today that inconsistency produces
  an ordinary lookup miss. Under chunking it sends the lookup to the wrong chunk,
  where the host is legitimately absent, and the rule above then certifies the
  not-found as authoritative. **A canonicalisation rule - lowercase, LDH, and the
  DNS label length ceiling - is a prerequisite of the chunk scheme, not hygiene
  alongside it.** This is open issue #208, and it is gated by the
  claim-time-versus-route-time agreement arm below.

## Entry size, derived from definitions rather than estimated

This section previously carried an ESTIMATE beside a MEASURED JSON figure and
called the band between them the open question that gates replicate-whole. The
band was measured wrongly - it paired a long-name packed figure against a
short-name JSON one - and, more importantly, it was the wrong question. Both
halves are re-derived below from the schema and the id formats. There is still NO
PRODUCTION CORPUS: zeroship is pre-launch, so every field width here comes from a
real definition and every population statement is arithmetic over an assumed
name-length distribution, tagged as such.

DERIVED entry layout, packed binary, one line per field, each width justified by
the definition beneath it rather than transcribed here:

```
  app_id             raw uuid, not its canonical text form
  deploy_hash        raw sha256, and ALSO the manifest address
  api_key_hash       raw sha256
  generation         unsigned counter
  name arena offset  unsigned offset into the chunk's string arena
                     (narrower if the arena is chunk-local)
  name length        unsigned byte, bounded by the name validator
  plan ordinal       unsigned index into the root's plan table
  state              bitfield: spend | account | provisioned | archived
                   -------
                     a FIXED-WIDTH record; its width is the sum of the
                     above and is computed by the arm, never quoted here
  name             + L bytes in the chunk's string arena
```

MEASURED, each from the definition named:

- **`app_id` is a raw uuid, not a typed id.** `zeroship.apps.id` is
  `t.uuid().notNull().default(uuidV4())`
  (`db/migrations-ts/20260702000200_control_tables.ts`). The raw form is
  materially narrower than the canonical text form, and that difference is the
  design decision.
- **A digest is hex in every store and on every wire we have.** `ingest` computes
  `deploy_hash = sha256_hex(&canonical_omit)`
  (`crates/zeroship-bundle/src/unpack.rs`) and `hash_client_secret`
  (`crates/zeroship-core/src/auth/mod.rs`) is `hex::encode(hasher.finalize())`.
  Both columns are `t.text()`. Packing them raw rather than as hex is where the
  largest single share of the JSON encoding's cost goes.
- **`plan_id` IS a typed id, contrary to the shape a `t.text()` column suggests.**
  `builtin_plan_id` (`crates/zeroship-control/src/plan_catalog.rs`) mints a
  `PLAN_PREFIX` typed id over a frozen SHA-256 derivation, so the value is ASCII
  text over, underneath, uuid bytes. It is also a small closed set - the built-in
  tiers minted by `free_plan_id`, `pro_plan_id` and `unlimited_plan_id`, with no
  operator API to mint more - so an ordinal into a plan table carried by the root
  replaces the whole string with an index.
- **`live` needs more than one bit, and the column its name suggests DOES NOT
  EXIST.** `zeroship.apps.suspended` is created in the base migration
  (`db/migrations-ts/20260702000200_control_tables.ts`) and then DROPPED, together
  with `audit_locked`, by
  `db/migrations-ts/20260817000200_drop_app_freeze_flags.ts` - whose own comment
  records that both were operator freeze levers that "never reached the data
  plane." Derived from the migration corpus, NOT from a live introspection: no
  database on the inspection cluster carries a `zeroship.apps` at all, so what is
  stated here is the shape the corpus produces. The live states are therefore
  `SpendState` and `AccountState` (`crates/zeroship-core/src/types.rs`) plus
  `provisioned` and, for a directory specifically, an archived bit that today's
  route table does not need because the projection filters archived apps out
  entirely (`WHERE a.archived_at IS NULL` in `Registry::get_routes`). A directory
  cannot do that: an archived app KEEPS its name (`apps_name_key` is unique and
  archive retains the row), so "archived" and "no such app" must be
  distinguishable or the authoritative negative answer is wrong. The product of
  the two enum cardinalities and the two bits fits comfortably inside one byte -
  and an arm derives that product from the live enums rather than trusting this
  sentence, because a new variant in either enum is the only event that can
  invalidate the byte and no prose would notice it.
- **`zone` does not exist.** No region, zone or datacenter concept is in the
  tree. ASSUMED: a small unsigned zone id, on the grounds that anything narrower
  re-opens as soon as a third region lands and anything wider is unjustifiable.
- **The name must be STORED, not just hashed.** A chunk keyed on `sha256(name)`
  could hold a truncated hash instead of the name, but a truncated hash admits
  collisions, and a collision on a lookup turns an authoritative NEGATIVE into a
  wrong POSITIVE - a request routed to another tenant's app. The name is the
  confirmation, so it is load-bearing and is charged for. The arm below builds a
  deliberate collision and asserts no positive is returned on a hash match alone.

The earlier estimate happened to land close to the derived width, and it did so
**by cancellation, not by construction**: it omitted the arena pointer a
fixed-width mmap-and-binary-search record needs, and over-charged the flag byte,
and the two errors nearly cancelled. That is the whole lesson of the number, and
it is why the number itself is not restated. The durable form is **the entry is a
fixed record plus the name**, with the fixed part derived from the definitions
above by an arm.

**The lookup key is the LABEL, not the FQDN, and that is what keeps the name
short.** `extract_app_name` takes the first label of `Host` and the cache index is
keyed on `entry.name`. `crates/zeroship-control/src/reserved_names.rs` states it
outright in its header: "An app's name IS its hostname label." The apex is a
deployment constant (`{$ZEROSHIP_DOMAIN}` in `deploy/ops/Caddyfile`), so it costs
nothing per entry. The moment custom domains ship, the key becomes an FQDN and the
name term grows substantially; that is the single largest future mover of the
total, and it is a design decision, not a distribution.

MEASURED bound on the name, and the hole it exposes: `Registry::create_app`
(`crates/zeroship-control/src/registry.rs`) refuses a name that is empty, is over
a hardcoded length ceiling, or is not `[A-Za-z0-9_-]`. That is the ONLY check -
`zeroship.apps.name` carries no CHECK constraint, and other live
`INSERT INTO zeroship.apps` sites bypass it entirely
(`crates/zeroship-migrate-server/src/schema_apply_store.rs`,
`crates/zeroship-worker/src/handler.rs`,
`crates/zeroship-control/src/cron/spend_recompute.rs`). Two consequences for the
encoder: a one-byte `name_length` is only safe while that one Rust function
holds, and **the ceiling is wrong anyway**. A DNS label has a hard length ceiling
fixed by RFC 1035 (sections 2.3.4 and 3.1: the length octet's high two bits are
zero, so six bits bound the label), and `_` is not a legal hostname character
(RFC 1123 section 2.1); a name at or over the label ceiling, or bearing an
underscore, is a name that cannot be resolved or certificated. The validator
should say the RFC ceiling and LDH. Both halves are arms below: one enumerates
the insert sites and rules that each passes the single validator (it is red
today), and one rules that the validator's ceiling is within the encoded length
field's width.

ASSUMED, and this is the only invented distribution in the section: names cluster
short. The only sample available is this repository's own app-shaped directories
under `examples/`, which is a biased sample - probes and demos, named by
engineers - and it is stated as the assumption rather than dressed up as a
population. The name length is the ONE free variable in the entry, so it is
listed first under "What to measure first" and it appears in no arithmetic here.

MEASURED, by construction of the exact byte strings: the debuggable JSON form of
the same facts, with `RouteEntry`'s field names verbatim (there is no
`serde(rename_all)` on the struct), is a fixed skeleton plus the name, the two
enum literals, the boolean literal and the generation digits. The skeleton is
dominated by two things that a packed form removes entirely: hex and uuid TEXT,
which carries four bits of entropy per character, and key names repeated
identically in every entry.

So the packed-to-JSON comparison is a property of the ENCODING, and it is only
meaningful when both sides are taken at the SAME name length. Every earlier
statement of that band failed exactly there.

DERIVED, on compressibility, and this settles the question the section was asking
by answering a different one than the framing expected. **JSON's size penalty is
a RESIDENT-memory penalty and very nearly not a bandwidth penalty at all**,
because a general-purpose compressor recovers most of what packing recovers: the
JSON's redundancy is exactly the hex text and the repeated keys, which is what
compressors are good at, while the packed form barely compresses because it has
already removed the redundancy the compressor was living on. Compressed JSON on
the wire costs close to what raw packed costs in memory.

**That measurement came from a one-off generator that no longer exists, so by
this repository's own standard it is unreproducible and its figures are gone.**
What survives is the ordering above, and the obligation: a generator script
committed beside this document, plus an arm that regenerates a synthetic corpus
and rules on the ORDERING and a declared band with whatever compressor is
installed. Until that lands, treat the compressibility claim as DERIVED and
unbound.

DERIVED sensitivity, one input at a time, stated as an ordering because the
ordering is what decides anything:

1. **Whether BOTH sha256 digests belong in the entry.** They are the dominant
   share of the fixed record, and no other single change is close.
2. **The name-length assumption**, and it is asymmetric: the downside is bounded
   by a name of length zero, the upside is not - every name at the legal ceiling,
   or an FQDN key after custom domains, moves the total substantially.
3. Everything else - integer widths, arena-pointer width, padding - moves the
   total by amounts that no decision turns on.

`deploy_hash` earns its place: it is the manifest address and the whole immutable
half hangs off it. **`api_key_hash` is the one to interrogate.** MEASURED, it has
exactly ONE production consumer, `check_api_key`
(`crates/zeroship-gateway/src/auth.rs`), on a header-bearing request path. Moving
it out of the directory and into a lazily-fetched per-app credential object
removes one of the two digests from every entry on every node. That is a real
decision with a real cost - the first `X-Api-Key` request against a cold app would
take a fetch - and it is now a decision rather than a guess. Its whole premise is
the singleton, so an arm enumerates the readers of `api_key_hash` and the callers
of `check_api_key` - explicitly INCLUDING the defining file, since this repo has a
recorded failure mode where a caller search excluding the definer hides internal
calls - and a second consumer breaks it.

## The replication envelope, and where replicate-whole stops

DERIVED, and stated as a division rather than a verdict, because both of its
terms are read from the tree and both move.

```
   resident directory bytes  =  hosts  x  (fixed record + mean name)
   break point               =  resident budget / bytes per entry
```

MEASURED, the only two per-node budgets this tree actually declares, both gateway
settings in `crates/zeroship-gateway/src/config.rs`: `blob_cache_mem_mb` and
`blob_cache_disk_gb`, turned into byte counts in
`crates/zeroship-gateway/src/main.rs`. They are not the directory's budget, but
they are the operator's own statement of what a gateway node's memory and disk are
worth, so they are the right yardstick.

**The directory is not a cache and that is what decides this.** `BlobCache` may be
any size an operator likes because a miss has a fetch behind it. The directory's
entire value is that a miss is an ANSWER, so it has no miss path and no eviction
policy: every byte of it is working set, permanently, on every node. At the
platform's stated target the directory becomes the LARGEST resident structure in
the gateway process and OVERRUNS the cache budget the operator was actually asked
to size - and that comparison is exactly the thing a startup refusal arm should
make on every boot, rather than a table that ages.

DERIVED, the mmap question, which is what decides the upper end. A fixed-width
record array is mmap-able and should be mmap'd rather than parsed onto the heap
(the tree already does exactly this for cached blob bodies: `mmap_to_bytes`
(`crates/zeroship-gateway/src/blob_cache.rs`) hands a mapping to ntex as
`Bytes::from_owner`, over files verified by `verify_file`). Two things follow, and
the second is why chunking is load-bearing for reasons the section above never
mentions:

- A fixed-width record array packs many records per page, so the array is a large
  number of pages at fleet scale.
- A binary search over the WHOLE array touches a distinct page per probe until
  the last few probes fall inside one page, so the cold-lookup page count is
  logarithmic in the array length. Searching within a CHUNK instead cuts that
  logarithm down by the chunk-count bits; an open-addressed index inside the
  chunk cuts it to a small constant.

**A page fault on an mmap'd directory is a synchronous, blocking fault on the
ntex worker thread.** compio and io_uring cannot see it, cannot schedule around
it, and cannot overlap it with other work, so a major fault stalls every
connection multiplexed onto that thread, not just the request that caused it.
This is the difference between the directory and the blob cache: the blob cache
faults while streaming a response body that was already going to take a while,
and the directory would fault *before the routing decision*, on every request
including the negatives it exists to make free. The design consequence is
categorical: **the directory must be page-cache resident, not merely mmap-able**,
and chunk-local indexing is what keeps the resident-page count per lookup in the
low single digits so the residency assumption is affordable. The per-lookup page
count is a bench arm, ruled on for both layouts in the same run.

**Verdict, stated so it cannot go stale.** Replicate-whole holds while
`hosts x bytes-per-entry` fits a node's resident budget, and both terms are
derivable in-tree. Evaluate the division against the fleet you actually have; do
not carry a verdict forward from a fleet you assumed. What the arithmetic settles
independently of any population is the ORDER of the fixes: get the manifest out
of the entry first (it dominates by orders of magnitude), then reconsider
`api_key_hash` (the largest remaining single term), and only then argue about
integer widths.

### Past that point: split the ENTRY, never the directory

DESIGNED, and it corrects the instinct rather than confirming it. The natural
move - "shard by zone, and keep a coarse host-to-zone map for the rest" - does
not work as stated, because **completeness is not shardable**. Every gateway
must answer "no app at this host" for hosts served by every zone, so whatever
structure answers that question must be complete on every node. A coarse
host-to-zone map that is complete IS a directory; calling it a fallback for "the
rest" understates it, because it is consulted on every lookup, including every
negative.

What does work is splitting the ENTRY into two tiers with different completeness
obligations:

```
  tier                       held by            entry              complete?
  ------------------------   ----------------   ----------------   ---------
  existence + placement      EVERY node         truncated hash     YES, always
                                                + zone id
  full record                the owning zone    the packed entry   per zone
```

DERIVED, why this actually buys something: the placement entry is a small
constant fraction of the full record, so the tier that cannot shard shrinks by
that factor while the tier that can shard divides by the zone count. The scheme's
real ceiling is set by the tier that cannot shard, and it is a fixed number of
bytes per host on every node forever. That is where the whole approach ends, and
it ends at a host count the division above names.

DESIGNED, and the reason to prefer this over anything cleverer: the placement
tier changes on **app creation, deletion and movement only** - never on a
deploy, a spend-state flip, a plan change or a key rotation, which are the
events that drive directory churn. It is both the smallest object and the
slowest-changing one, so its chunks are near-static and its root is nearly quiet.

DERIVED caveat, and it is the same argument the full directory uses against
truncation, reaching a *different* answer for a *different* reason. The full
directory refuses a truncated hash because a collision turns an authoritative
negative into a cross-tenant POSITIVE. In the placement tier a collision between
a real host and a nonexistent one costs only a wasted forward hop, because the
destination zone re-checks the full name and answers the authoritative not-found
itself. But a collision between **two real hosts in different zones** is not
benign: one of them would forward to the wrong zone and get a confident not-found
for a live app. A truncated hash makes that rare rather than absent, and
rare-rather-than-absent is exactly the case a design must name: **a colliding slot
must widen** - carry both zones, or carry the full name for that slot - rather
than silently pick one. The probability is not the claim; the widening rule is,
and it is a deterministic arm.

## Chunk count, derived rather than chosen

This section previously carried a table at a chosen `k` and admitted the value was
"a starting point, not a measured optimum." It is derivable, and the derivation
is what belongs here.

DERIVED. Let `H` be hosts, `b` the bytes per entry, `N` the chunk count and `d`
the bytes per root slot (one sha256). Under the model where the root is refetched
whole whenever it changes - which is what "the root is the only object that
changes on every mutation" means - the bytes a node moves per mutation are:

```
   J(N) = N*d        (the root)
        + H*b/N      (the one chunk that changed)

   dJ/dN = d - H*b/N^2 = 0   ->   N* = sqrt(H*b/d)
```

Three properties fall out that are worth more than any evaluated number. **At the
optimum the root and a chunk are the same size**, both `sqrt(H*b*d)` - that is
what setting the derivative to zero means here. **The minimum cost is
`2*sqrt(H*b*d)`, which grows as `sqrt(H)`**, so growing the fleet costs
per-mutation traffic only at the square root of that growth. And
`k* = 0.5 * log2(H*b/d)`: **k is logarithmic in the fleet**, so it is not a
constant that might have been chosen badly, it is a function of a number that
grows monotonically.

DERIVED, the cost of getting it wrong: **the objective is flat near its minimum
and steep at both ends.** Flat is why a guessed value can look fine - a wide band
around `N*` costs almost nothing. Steep is why the ends are ruinous: too few
chunks republishes a large chunk for a single spend-state flip, and too many makes
the root the dominant object on every mutation. So the failure mode of guessing is
not "slightly suboptimal," it is "fine until the fleet moves, then bad fast."

**Verdict.** Do not write a `k` into this document. Write `N* = sqrt(H*b/d)` into
the root, evaluate it for the fleet that exists, and let readers re-chunk. The arm
that matters is not one that checks a value; it is one that feeds a reader two
roots with different `k` and asserts both are served correctly, because **a reader
that cannot re-chunk is a reader that is wrong on a schedule.**

DESIGNED, and it is a real fork the model above hides. Part 4 commits to gossip
for membership, with invalidation riding free. **If gossip carries a signed
per-chunk announcement** (`chunk i is now digest D at directory version V`), the
`N*d` root term disappears from the steady state, `J(N) = H*b/N` is monotonically
decreasing, and `N` should be pushed far higher - bounded not by bytes but by
(a) the per-request overhead being worth amortising, which puts a floor on chunk
SIZE and therefore a ceiling on `N`, and (b) cold-start warm time, since a node
must fetch **all N chunks** before it may answer any negative authoritatively, so
a large `N` at bounded concurrency is many round-trip batches before the node is
complete.

**The fork is a signing question, not a sizing question, and it should be
settled first.** If only the ROOT is signed, every node must fetch the whole root
to validate any chunk, the `N*d` term is real, and `N* = sqrt(H*b/d)` stands. If
per-chunk announcements are independently signed by the authority, the root
becomes a slow-path completeness certificate rather than a hot object, and the
right `N` is far larger. **The two regimes give different optima, and the
difference between them exceeds every correction this document has made to an
entry size.** Once the signing decision is made, an arm should assert that a chunk
is accepted only under the scheme that was chosen, so the sizing model's premise
is enforced by code rather than assumed by a reader.

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

  (2) CHANGE + MEMBERSHIP gossip over the        logarithmic fanout of
      "X is stale"        fleet                  a small message
      "node N joined"

  (3) EXISTENCE           the local, complete    ZERO. No wire at all.
      "is there an app    directory
       named foo?"
```

Job 1 is built and needs no new mechanism. Job 3 needs no mechanism at all,
provided the directory stays complete. Job 2 is the only place a new transport is
warranted, and it is warranted by membership rather than by invalidation.

## Job 1: fetch by digest. This exists.

MEASURED. `BlobStore` (`crates/zeroship-bundle/src/blob.rs`) is already a
content-addressed fetch API: `get_blob`, `local_path`, `has_blob`,
`get_blob_to_file`. The S3 implementation maps a hash to a `blobs/{hash}` key and
re-verifies the sha256 of the returned bytes before handing them back
(`crates/zeroship-bundle/src/s3_blob.rs`), with the comment that "the backend's
metadata/checksum is not trusted proof of integrity."

MEASURED. The gateway's two-tier edge cache in front of that store is complete,
as detailed in Part 3.

MEASURED. Nothing on the serving path handles an archive.
`zeroship_bundle::unpack` has exactly one non-test consumer in the tree and it is
the control plane's deploy path (`crates/zeroship-control/src/deploy.rs`, which
re-exports it). The worker fetches individual blobs by hash
(`crates/zeroship-worker/src/sync.rs`, for the runtime descriptor and for the
worker bundle), never an archive.

DESIGNED. Content distribution therefore needs no new protocol. It needs the
existing digest fetch pointed at more origins, and the worker given the cache the
gateway already has. A digest is a perfect cache key and a perfect validator: a
fetch is either a hit forever or a miss exactly once. There is no coherence
problem here because there is no mutable resource.

## Job 2: invalidation and membership. Gossip, justified by membership.

DESIGNED, on the MEASURED fact from Part 1 that `HashRing` has no mutation method
and is constructed once in `crates/zeroship-gateway/src/main.rs`. Gossip is
justified by that fact alone. A membership protocol gives every gateway a live
view of which workers exist and which are healthy, which is the input `HashRing`
needs and cannot currently receive. Once a fleet-wide gossip channel exists for
membership, invalidation is a free rider: "app `a` moved to deploy hash `h`" is a
small message on the same wire, in the same fanout, with the same failure model.

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
where an O(1) decode is available. Replacing the scan with the decode is small and
independent and should not wait for the rest.

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
`crates/zeroship-control/src/cron/lock_keys.rs` centralises the lock keys in one
array (`ALL`) whose pairwise distinctness is enforced by the test
`all_keys_are_pairwise_distinct`, which derives its own comparison set from `ALL`
rather than from transcribed literals; the header records that this replaced
hand-written comparison tests that between them covered only part of the pair
space, with two keys defined in the same file compared by none of them. The reach
of advisory locks across this workspace is broad - many source files across many
crates name `pg_advisory_lock`, `pg_try_advisory_lock` or `pg_advisory_xact_lock`,
in both `crates/` and `libs/compio-postgres`. **The count is deliberately not
stated**: it moved twice inside this document and the argument never depended on
its value, only on the role being filled. Adding etcd would add a second,
independently-failing source of coordination truth for a role that is filled and
tested. The deeper objection: etcd cannot fan a watch out to a large number of
watchers, which is why Kubernetes had to put an API server in front of it to
multiplex watches. Adopting etcd means adopting the obligation to build that
layer, which is to say we would build the fanout tier and etcd would be an
implementation detail underneath it. Building the fanout tier directly is shorter.

**P2P swarming (Dragonfly, Kraken and relatives).** Rejected on artifact size.
MEASURED: the built `.zship` artifacts in this tree are small, and their spread is
narrow relative to what swarming is for. Swarming exists to amortise moving very
large immutable objects to very many nodes at once, where origin egress is the
bottleneck and peers can supply each other. That regime does not begin at the
sizes this pipeline produces; at these sizes a peer negotiation costs more than
the transfer it saves. ASSUMED, and marked as such: the scale those tools target
is external context I could not verify from this tree, so the argument rests on
our measured sizes, not theirs.

**That rejection is CONDITIONAL, and this document did not say so.** The argument
above rests entirely on a measured corpus of probes and demos. What the platform
actually enforces is orders of magnitude larger: MEASURED, `MAX_COMPRESSED_BYTES`
and `MAX_DECOMPRESSED_BYTES` in `crates/zeroship-bundle/src/limits.rs`, with
`MAX_BLOB_BYTES` and `MAX_BLOBS_PER_DEPLOY` bounding the parts. A legal `.zship`
today may be very much larger than the largest one ever built here, so "swarming
does not begin at these sizes" is a statement about the sample, not about the
artifact format. Rejecting a mechanism on a corpus while the enforced ceiling
sits far above it is the exact error corrected elsewhere in this document,
applied to a design decision instead of a byte count.

The rejection still stands, for the reason it should have been given: at the
sizes this platform's build pipeline currently produces, peer negotiation costs
more than the transfer. What must travel with it is the **re-open trigger**: when
the p99 artifact rises to a material FRACTION of `MAX_COMPRESSED_BYTES`, the
origin egress term that swarming exists to amortise becomes real and this decision
must be taken again. That trigger is a ratio arm, listed below, and item 3 of
"What to measure first".

MEASURED, on the state of the tree: `gossip`, `libp2p`, `SWIM`, `consul`,
`hickory` and `trust-dns` each appear **zero** times across `crates/` and `libs/`
in any `.rs` or `.toml` (word-boundary search); `Cargo.lock` contains no package
beginning `hickory` or `trust-dns`; `etcd` appears only in a blocklist of
service-discovery ports (`crates/zeroship-core/src/preview_ports.rs`). None of the
four rejected options is being removed. All four are being declined. A
zero-occurrence arm keeps that true, and prints the set and the roots it searched,
because a sweep that matches nothing otherwise reports success.

MEASURED, before anyone reaches for a new HTTP dependency: `cyper` is already a
direct dependency of the gateway, the worker and the control plane (each crate's
`Cargo.toml`), built with `rustls` (root `Cargo.toml`). A conditional or ranged
HTTP GET needs no new dependency; only the gateway's control pull is hand-rolled,
and it is hand-rolled below the level at which conditional requests exist.

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
(`crates/zeroship-gateway/src/proxy.rs`) encodes a dispatch frame, selects a
target with `HashRing::select`, and hands the URL to the worker-dispatch
forwarder. The connection abstraction is already a sum over TCP and Unix sockets
(the `Stream` enum in the same file). There is a proxy endpoint on the public
gateway surface already, `/__zeroship/internal/workflow-advance`, wired in
`crates/zeroship-gateway/src/main.rs`; a cross-zone hop is the same mechanism with
a different destination.

DESIGNED, and it is what the envelope arithmetic in Part 3 forces rather than a
placement preference: **zones do not shard the directory, they shard the
ENTRY.** Every gateway in every zone still holds a complete existence-and-
placement tier, because a negative answer is only authoritative against a
complete structure and no zone may be asked to answer for hosts it does not know.
What becomes zone-local is the full record. The placement tier is what cannot
shard, and its per-host cost on every node is where this whole approach ends; the
derivation, including why truncating the key is tolerable there and not in the
full directory, is in Part 3.

MEASURED, on the state of zones: there is no region, zone or datacenter concept
anywhere in the tree. `docs/architecture/data-system.md` records the same finding
independently ("no region or datacenter concept anywhere in the tree", checked
2026-08-29) and `docs/architecture/distributed.md` lists "No multi-region route
propagation or data replication in the shipping code path" among the current
boundaries. Everything in this subsection is DESIGNED.

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
`ANCHOR_COOKIE` (`crates/zeroship-gateway/src/anchors.rs`), set by
`set_anchor_cookie` with `Path=/; HttpOnly; SameSite=Strict; Secure` and
deliberately no `Domain` attribute, with the rationale stated immediately above
it; the interactive credential is `APP_SESSION_COOKIE`
(`crates/zeroship-gateway/src/oidc_rp.rs`). A `__Host-` cookie set on
`app.zeroship.ai` is not sent to `app.zone-b.zeroship.ai` and cannot be made to
be. Any scheme that puts a zone into the hostname logs every user out on every
failover. Anycast keeps one hostname, so the cookie keeps working, so failover is
invisible. The "both" in that first sentence is a completeness claim over a set,
so an arm enumerates every browser session credential the gateway sets and rules
each carries the prefix and no `Domain`; a third credential added without it
silently reopens a design this document closed.

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
(`crates/zeroship-gateway/src/db.rs`, entered through `db::checkout`) serving
`zeroship.app_session_anchors` through `anchors::create`, `anchors::read_live`,
`anchors::update_rotated_family` and the delete paths, called from several gateway
modules. A gateway in zone B validating a session for an app whose anchor row is
in zone A must read zone A's database. Until anchors are addressable from any zone
- replicated, or homed with the app and read over the same forward hop - moving an
app moves its sessions' storage out from under whichever gateway holds the cookie.

**Encryption keys are derived from the app id with no rotation surface.**
MEASURED: `crates/zeroship-data-orm/src/encryption/keys.rs` derives both AEAD
halves with HKDF-SHA256 salted by the app id. The module header spells it out
(`salt = app_id`, `info = "zsenc/aead/v1/k_enc"` and `.../k_siv"`) and
`derive_key` does exactly that:

```rust
let hkdf = Hkdf::<Sha256>::new(Some(app_id.as_bytes()), root);
```

The root key reaches the process out of band, never from a database - the module
header also records that the `PgAdminTable` source was deleted with the admin
schema on 2026-08-27 under the privilege-follows-the-process invariant. The cache
invariant stated in that same header is the limit, in the module's own words: once
an `(app_id, key_id)` entry is inserted it stays for the lifetime of the
`KeyStore`, and **"There is no rotation surface today; adding one will require
rewiring the cache to track key versions."**

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
`validate`, which fails when either `REPLICATION` or `BYPASSRLS` is missing and
again when any role membership inherits ambiently - the reasoning is stated in the
function: a single login role shared by every app, whose inheriting memberships
would make its ambient authority the union of every tenant it has ever served.
`crates/zeroship-worker/src/main.rs` calls `validate_database_url` before
`init_v8()` and exits non-zero on failure. A worker cannot start in a zone whose
database has not been provisioned to that exact posture.

---

# Open decisions

Each is a concrete either/or with a recommendation. None is settled.

## 1. Chunk count

**Either** fix `k` now and treat it as a constant, **or** make `k` a field of the
root object so it can be raised without a flag day, at the cost of every reader
carrying a re-chunk path.

**Recommendation: put `k` in the root, and derive its starting value from
`N* = sqrt(H*b/d)` for the fleet that exists rather than transcribing one from
this document.** This recommendation previously named a value; the value moved on
the first re-derivation, which is the argument for not naming one. The stronger
argument for k-in-the-root is structural: `k* = 0.5 * log2(H*b/d)` is logarithmic
in the fleet, so `k` is not a constant that might have been chosen badly, it is a
function of a number that grows monotonically. A reader that cannot re-chunk is a
reader that is wrong on a schedule.

**A prior decision determines the target, and it is not this one.** If gossip
carries independently signed per-chunk announcements, the root's `N*d` term leaves
the steady state and the right `k` is substantially larger; if only the root is
signed, `N* = sqrt(H*b/d)` stands. The two regimes differ by more than any
correction this document has made to an entry size. **Settle what is signed before
settling `k`**, and record that choice as an ADR, because it is a blocking design
decision rather than a tunable.

## 2. Does the envelope collapse into `manifest.json` entirely?

**Either** the manifest stays one immutable object per deploy, with the asset map
digested out of it, **or** the manifest itself becomes a small root naming
several digested parts (resources, assets, schemas, descriptor) so a host fetches
only the parts it reads.

**Recommendation: digest the asset map out, and stop there for now.** MEASURED,
the two tiers read overlapping but different parts - the gateway reads `resources`
and both asset maps (`serve_resource_tree_static` and `lookup_static_hit`), the
worker compiles the whole manifest as its declared policy (`cache::load_app`). A
finer split would let each fetch less, but the manifests this tree produces are
small relative to a round trip, so the saving is small and the cost is several
round trips on a cold isolate load. **This recommendation has a mechanical expiry:
the p99 manifest as a FRACTION of `MAX_MANIFEST_BYTES`, measured over a real
corpus, is the trigger to revisit it, and that ratio is a gate arm rather than a
sentence that will read as current forever.**

## 3. Where session anchors live

**Either** anchors stay gateway-local and an app is pinned to the zone holding
its anchor rows, **or** anchors move behind the same one-hop forward path as
dispatch, so the zone that owns the app owns its sessions.

**Recommendation: home anchors with the app and read them over the forward hop.**
The pinning option is not actually a decision to defer - it is a decision to make
zones a partition of users rather than of apps, and MEASURED it fights the
`__Host-` cookie property that makes anycast work: one hostname, one cookie, any
zone. The forward-hop option costs a round trip on the session-validating path
only, which is already the path that does database I/O today (`db::checkout`,
called from several gateway modules). It is the smaller change to the model even
though it is the larger change to the code.

## 4. Encryption key rotation

**Either** every zone holds every root key, **or** a rotation surface is built so
an app's ciphertext can be re-keyed when it moves.

**Recommendation: build the rotation surface, and treat "every zone holds every
root" as the interim only if it is written down as such.** MEASURED, the module
already names the work: "adding one will require rewiring the cache to track key
versions" (`crates/zeroship-data-orm/src/encryption/keys.rs`). The interim option
is not free - it makes zone isolation nominal for the one thing zone isolation
would most be wanted for - and it is the kind of interim that becomes permanent
because nothing fails while it holds. Sequencing this after the delivery work is
right; sequencing it after launch is not, because re-keying live tenant ciphertext
is exactly the migration that pre-launch is the cheap moment for.

## 5. What a zone physically is

**Either** a zone is a failure domain with its own Postgres, object storage and
worker fleet, and cross-zone traffic is only the forward hop, **or** a zone is a
placement hint over shared storage.

**Recommendation: a zone is a failure domain, and nothing in the tree records
one today.** MEASURED, there is no region, zone or datacenter concept anywhere
(zero occurrences; `docs/architecture/data-system.md` and
`docs/architecture/distributed.md` say the same independently). That means the
term is currently free to define, and the two definitions have opposite
consequences for every decision above: the failure-domain reading makes decisions
3 and 4 blocking, the placement-hint reading makes them irrelevant and makes the
platform one shared-fate system with latency optimisation. The failure-domain
reading is the one worth the cost. This decision must be recorded as an ADR before
any of the others are implemented, because all four inherit from it.

## 6. What a gateway does when the exception overlay is stale

**Either** a stale overlay is ignored (serve everything, log loudly), **or** a
stale overlay past some age degrades or blocks.

**Recommendation: ignore it, log loudly, and alarm - but only once the overlay
carries a generation and a not-after timestamp**, so "no exceptions" and "no
answer" are distinguishable bytes. MEASURED, the current pull already fails open
on a missing row and fails closed on a bad value (both in `Registry::get_routes`'s
row mapping), and the open direction costs revenue rather than exposing data
(`check_spend` and `check_account` in `crates/zeroship-gateway/src/enforce.rs`).
Blocking on a transport property would convert a billing outage into a total
outage. The part that is not optional is the liveness field; without it this
decision cannot be implemented in either direction. The recommendation is sound
only while the overlay's aperture stays billing-only, which is a gate arm.

---

# What to measure first

Three things gate the design. Two of them cannot be taken from this repository at
all, which is itself worth stating.

**1. The NAME LENGTH DISTRIBUTION - and only that.** This item used to read "real
directory entry size," on the grounds that the packed entry width was an estimate
and its band against JSON decided whether replicate-whole survives at the target.
Part 3 now derives both halves from the schema, and the answer is that **the entry
is a fixed record plus the name**: every fixed field has a width fixed by a
definition in this tree, so there is nothing left to measure in it. What is left
is the ONE free variable. `name` is creator-chosen, bounded only by
`Registry::create_app`, and its distribution is the single input the arithmetic
still assumes. A real corpus moves the resident-directory figure over a range
whose ends are the shortest legal name and the legal ceiling, so it is worth a
query the day one exists; it is NOT worth building an encoder to find out, because
the encoder's output is already known per name length. Nothing else on this list
should be done first.

**2. App creation and mutation rate.** Root republication frequency and gossip
fanout follow from how often the directory changes, and this tree has no
instrument for it. `zeroship.apps` has `created_at` and `updated_at` columns
(`db/migrations-ts/20260702000200_control_tables.ts`), so the creation rate is a
query away on any populated database; the mutation rate is harder, because
MEASURED the spend evaluator writes an `app_spend_state` row for every app on
every tick (`SpendEngine::evaluate_all` and `touch_state`,
`crates/zeroship-control/src/spend.rs`) and those writes are not directory
mutations. The measurement to build is "how many entries would have changed since
the last root," not "how many rows were written."

**This item used to say the CHUNK COUNT follows from it. It does not.**
`N* = sqrt(H*b/d)` contains no rate term: the mutation rate scales the whole cost
curve without moving its minimum, because every mutation pays the same
`root + chunk` and the ratio between those two terms is what `k` trades. So the
rate decides how much the scheme costs and whether gossip must carry per-chunk
deltas; it does not decide `k`. That is why `k` could be derived here and the rate
could not.

**3. p50 and p99 manifest and `.zship` size over a realistic corpus.** What is
measurable here is a corpus of probes and demos whose tail is almost certainly
wrong in both directions. The p99 manifest is what settles open decision 2, and
the p99 artifact is what would reopen the P2P-swarming rejection.

**Both have an ENFORCED ceiling, and the ceiling is what to compare against, not
the corpus.** MEASURED: `MAX_MANIFEST_BYTES` and `MAX_COMPRESSED_BYTES` in
`crates/zeroship-bundle/src/limits.rs` sit far above anything this tree has ever
built. **The re-open triggers are therefore RATIOS, not absolute sizes**: p99
manifest over `MAX_MANIFEST_BYTES` crossing a declared threshold makes open
decision 2 live, and p99 artifact over `MAX_COMPRESSED_BYTES` crossing one makes
the swarming rejection live. Neither needs a new instrument; both are one query
over a populated deploy table, and both are gate arms below so the triggers fire
rather than being remembered.

**What cannot be measured from this repository.** There is no deployed fleet, no
zone abstraction, and no app count. Every aggregate in this document is arithmetic
on measured per-entry costs and an assumed population, and is tagged DERIVED where
it appears - which is also why no aggregate is written down as a value.

---

# Gate arms this design requires

Every quantity this design leans on lives here, as an arm rather than a sentence.
Each entry names what the arm ENUMERATES, what FLOOR it clears, and which file
produces the number. Per `tests/lib/gate_arms.sh`, an arm declares the number of
items THAT ARM RULED ON and a floor that number must clear, and the floor is
derived by the arm from the code beside it - never transcribed from this document.
**None of these entries states a current value. If a future edit adds one, delete
it.**

## Arms on the directory entry

**A1. The fixed-record width, recomputed from the definitions.** Enumerates the
directory entry's fields and recomputes the fixed-record width from the tree:
`zeroship.apps.id`'s column type
(`db/migrations-ts/20260702000200_control_tables.ts`), the two sha256 hex columns,
the closed plan-id set (`crates/zeroship-control/src/plan_catalog.rs`), the
`SpendState` and `AccountState` variant counts
(`crates/zeroship-core/src/types.rs`) and the declared bit layout. Rules that the
computed sum is within a declared fixed-record ceiling. Goes RED when any field
widens - `app_id` to text, `plan_id` to its `pln_` string, a new field added -
which is exactly the top of Part 3's sensitivity ordering. FLOOR: the number of
entry fields ruled on equals the number the directory schema declares, derived by
the arm.

**A2. The state byte.** Derives the state cardinality from the live `SpendState`
and `AccountState` enums plus the directory's declared bits, and asserts the
product fits the encoded field width. Goes RED when a variant is added to either
enum, which is the only event that can invalidate the byte and the one no prose
would notice. FLOOR: the number of enum variants enumerated, read from the enums.

**A3. Every app-name insert passes the one validator.** Enumerates every
`INSERT INTO zeroship.apps` site in the tree and rules that each passes through
`Registry::create_app`'s name validation. It is RED today: the validator is in
`crates/zeroship-control/src/registry.rs` and there are live insert sites that
bypass it (`crates/zeroship-migrate-server/src/schema_apply_store.rs`,
`crates/zeroship-worker/src/handler.rs`,
`crates/zeroship-control/src/cron/spend_recompute.rs`). FLOOR: the number of insert
sites the arm itself discovers, never a transcribed count.

**A4. The name ceiling fits the encoded length field.** Rules that the validator's
length ceiling is within the width of the packed record's name-length field. With
A3, this removes the name-length DISTRIBUTION from prose entirely; only the
ceiling is ever asserted.

**A5. Name canonicalisation is a refusal, not a number.** Rules that
`Registry::create_app` REJECTS a name at or over the DNS label ceiling, rejects an
underscore-bearing name, rejects any non-LDH name, and ACCEPTS a maximal legal LDH
name. The octet count lives in the assertion, not in this document.

**A6. Claim-time and route-time normalisation agree.** A differential arm over
every name-handling site: for a case-varying corpus, assert that claim-time
normalisation (`Registry::create_app`, `apps_name_key` in
`db/migrations-ts/20260702000600_constraints_indexes_fks.ts`, `is_reserved_app_name`
in `crates/zeroship-control/src/reserved_names.rs`) and route-time normalisation
(`extract_app_name` in `crates/zeroship-gateway/src/router/dispatch.rs`, and the
index in `crates/zeroship-gateway/src/sync.rs`) agree on every input. FLOOR: the
number of name-handling sites ruled on, ENUMERATED by the arm, so a site added
later breaks the gate. Tracks open issue #208.

**A7. `api_key_hash` has one consumer.** Enumerates readers of `api_key_hash` and
callers of `check_api_key` (`crates/zeroship-gateway/src/auth.rs`), explicitly
INCLUDING the defining file, since this repo has a recorded failure mode where a
caller search excluding the definer hides internal calls. Rules the count against
a declared floor. A second consumer must break it, because the decision to move
that digest out of the entry rests entirely on the singleton.

**A8. Hash collisions are handled behaviourally.** Constructs a deliberate hash
collision in the fixture and rules that (a) no positive lookup result is returned
on a hash match alone - the full name is compared - and (b) the placement tier's
colliding-slot path widens (carries both zones, or the full name) rather than
picking one. Deterministic; no probability appears anywhere.

## Arms on the feed and the manifest

**A9. No delta affordance exists.** Over `crates/zeroship-control/src/internal.rs`
and `Registry::get_gateway_snapshot`, rules that no delta affordance exists - no
ETag, no `If-None-Match`, no cursor, no since-token, no limit, no app filter - and
that the route SQL's only predicate is `archived_at IS NULL`. FLOOR: the number of
affordances checked, each NAMED in the arm's output, so adding one flips the gate
rather than requiring a prose edit. This replaces every aggregate byte figure in
"The arithmetic that disqualifies the shape."

**A10. `RouteEntry` carries nothing deploy-scoped.** Rules that no `RouteEntry`
field's serialized size is a function of deploy content, i.e. the per-app
per-poll payload is O(1) in manifest size. RED today by construction; GREEN once
the manifest is a digest. FLOOR: zero deploy-scoped fields on the pulled row, with
the arm enumerating the fields from the struct rather than being handed a list.

**A11. The manifest cap and the blob-count cap contradict each other.** Grows an
asset map against the REAL serializer until `MAX_MANIFEST_BYTES` refuses, then
rules that the refusing asset count is strictly LESS than `MAX_BLOBS_PER_DEPLOY`
(proving the caps contradict today) and strictly GREATER after the digest lands
(proving the digest reconciled them). Both constants read from
`crates/zeroship-bundle/src/limits.rs` by the arm. No asset count and no ratio
appears anywhere.

**A12. Asset entries cost more than resource entries.** Adds one asset entry and
one resource entry to a real manifest, measures the serialized delta, and rules
only on the ORDERING (an asset entry costs strictly more) plus a declared
per-asset ceiling. The ratio is the durable claim; the two byte figures are the
perishable one.

**A13. Every manifest copy is enumerated and classified.** Enumerates every site
that stores or transmits a manifest body, classifies each as durable-at-rest or
on-the-wire, and rules that the on-the-wire set is EMPTY after the digest lands.
FLOOR: the number of copies the arm discovers - a copy added later must break it,
since a missed copy is exactly the failure mode correction 12 describes.

**A14. The manifest round-trips against its own key.** Over every stored manifest:
strip `deploy_hash`, canonicalize via `canonical_manifest_for_hash`, sha256, and
assert equality with the storage key. RED today - that is precisely the defect
this document found by hand - and GREEN after the stored body drops the field.
FLOOR: the number of stored manifests ruled on, derived from the store, so an
empty store cannot print a pass.

**A15. `runtime_assets` has no non-empty producer.** Enumerates every writer of
`Manifest.runtime_assets` and asserts the written map is empty, paired with an
ingest-refusal arm over the fresh-deploy invariants in `ingest`
(`crates/zeroship-bundle/src/unpack.rs`). FLOOR: the number of writer sites the arm
itself discovers - a new producer must break the gate, because a new producer is
exactly the event that ends the immutability premise.

**A16. Every `RouteEntry` field is classified by its request-path readers.** For
each field, enumerate request-path readers and rule its classification (routing
key / dispatch policy / credential / billing gate / OAuth identity / deploy
pointer). FLOOR: the number of fields ruled on equals the number the struct
declares, derived by the arm, so a new field cannot be added without a
classification. This is the whole of Part 2's premise, and correction 13 records
the classification was already wrong once.

## Arms on the residency and chunking design

**A17. Directory residency is refused at the boundary, not tabulated.** The
gateway computes directory bytes from `entries x sizeof(Entry) + arena` and
refuses to become ready - or degrades loudly - when that exceeds a declared
resident budget expressed as a multiple of `blob_cache_mem_mb`
(`crates/zeroship-gateway/src/config.rs`). The arm rules that the refusal actually
fires at the boundary, reading both config symbols. This replaces the entire
budget-comparison table.

**A18. Per-lookup page count, measured for both layouts in one run.** Counts page
faults, or distinct pages touched, per lookup over a real record array in the
whole-array and chunk-indexed layouts, and rules each against a declared
per-lookup ceiling. FLOOR: the arm must rule on BOTH layouts in the same run, so
the comparison is measured rather than computed from a logarithm, and it must go
RED if `sizeof(Entry)` changes such that records-per-page drops.

**A19. Readiness distinguishes three lookup outcomes.** Withholds exactly one
chunk and asserts (a) `/readyz` reports NOT ready and (b) a lookup whose host falls
in the missing chunk returns unavailable-or-fetch, never a not-found. FLOOR: the
arm rules on ALL THREE outcomes - held at the current root, held at an older root,
chunk missing - so a two-state implementation cannot pass.

**A20. The reader reads `k` from the root.** Feeds the reader two roots with
different `k` and asserts both are served correctly, and that no chunk count is
compiled into any reader. This gates the property open decision 1 actually argues
for. The specific `k` lives in a configuration value the arm reads, never in prose.

**A21. Chunks are accepted only under the chosen signing scheme.** Once the
root-only-versus-per-chunk signing decision is recorded as an ADR, asserts a chunk
is accepted only under that scheme, so the sizing model's premise is enforced by
code rather than assumed by a reader.

## Arms on enforcement, coordination and the rejections

**A22. The spend evaluator writes a row per app per tick.** Over one live
evaluator pass, asserts `count(app_spend_state) == count(apps)`, and that the
exception feed's cardinality equals `count(state <> 'allow')`. FLOOR: the app count
in the fixture, derived by the arm. Goes RED the day `touch_state`'s unconditional
UPSERT is "optimised away", which is exactly when the feed sizing silently changes.

**A23. The overlay's aperture is billing-only.** Enumerates every enforcement gate
reachable from the exception overlay and rules that none of them is an
authentication or authorization check. FLOOR: the number of gates enumerated,
derived by the arm. A gate wired into the overlay that authenticates must break it,
because the fail-open recommendation is sound only while the set stays
billing-only.

**A24. Every browser session credential is `__Host-` prefixed.** Enumerates every
browser session credential the gateway sets and asserts each carries the `__Host-`
prefix and no `Domain` attribute. FLOOR: the number of credentials discovered,
derived by the arm. A credential added without the prefix silently reopens the
per-app-DNS rejection.

**A25. The ring has no mutation path and there is no worker registry.** Asserts
`HashRing` has no mutating method and no interior mutability, and that no
worker-registration endpoint exists on the control plane. Must go RED the day
either lands - because that is precisely the day the gossip justification changes
and Part 4 needs rewriting, and nothing else in the tree would announce it.

**A26. One coordination registry, and its pairwise-distinctness test derives its
own floor.** `all_keys_are_pairwise_distinct`
(`crates/zeroship-control/src/cron/lock_keys.rs`) is already the right shape - it
derives its comparison set from `ALL` rather than from transcribed literals. Add
one arm ruling that exactly ONE lock-key registry exists (no second coordination
source). The file and crate counts stay OUT of prose: they moved twice inside this
document and the etcd argument never depended on their value, only on the role
being filled.

**A27. The rejected mechanisms stay absent.** Fails when `gossip`, `libp2p`,
`SWIM`, `consul`, `hickory` or `trust-dns` appears outside the known port
blocklist (`crates/zeroship-core/src/preview_ports.rs`). A sweep that matches
nothing must PRINT the set it searched and the roots it searched, per this repo's
standing lesson that a sweep matching nothing otherwise reports success. When it
goes red, every DESIGNED tag in Part 4 is what to re-audit.

**A28. The two re-open triggers are ratios.** Over a populated deploy table,
computes p99 manifest size as a fraction of `MAX_MANIFEST_BYTES` and p99 artifact
size as a fraction of `MAX_COMPRESSED_BYTES`, and fails when either crosses a
declared threshold. The ratio is the claim; neither corpus figures nor multiples
need appear. Until a populated table exists the arm rules zero rows and SAYS so,
which is itself the honest state.

**A29. Compressibility, with a committed generator.** A generator script committed
beside this document regenerates a synthetic directory corpus; the arm rules on
the ORDERING (compressed JSON bytes per entry within a declared factor of packed
resident bytes per entry) with whatever compressor is installed. Until this lands
the compressibility claim in Part 3 is DERIVED and unbound, because the figures it
came from were produced by a one-off that no longer exists.

**A30. `db::checkout`'s callers are enumerated mechanically.** Enumerates callers
of `db::checkout` excluding rustdoc links and the definition site BY CONSTRUCTION
- both errors this document committed - and rules the calling-module count against
a declared floor. The document's own two-value history for this count is the case
for mechanising it rather than restating it.

---

# Corrections made while designing this

Recorded rather than quietly applied, because this repo's failure mode is a
corrected number that invites the same error back. **The figures are gone from
these entries by design.** What is preserved is the SHAPE of each error, because
that is what a future reader needs in order not to repeat it; a wrong value and
its replacement teach nothing that "this was estimated rather than derived, and
the estimate was wrong in both directions" does not teach better.

## Substantive errors

**1. `DbResourceKey` is a digest BECAUSE it reaches logs, not so that it cannot.**
An early draft repeated a claim from `docs/architecture/data-system.md`, which
says: "This is the discipline `DbResourceKey` already applies to DSN passwords: a
digest chosen so the secret 'cannot reach `Debug` or a log line'." MEASURED, the
source (`crates/zeroship-plugin-db/src/service.rs`) says the opposite:

```
//! # What a `DbResourceKey` is for
//! ...
//! different deploys. It is a digest, not the URL, because it reaches `Debug`
//! output and logs and a DSN carries a password.
```

The reason for the digest is that the value *does* reach `Debug` and logs. The
quoted phrase "cannot reach `Debug` or a log line" appears nowhere in the source,
and the cited line was the "It is the identity of one database's resources"
sentence rather than the reason a few lines below it. This is the exact failure
mode this document's tagging discipline exists to prevent: a design intent
transcribed as a guarantee, with quotation marks that make it look measured. **It
is an inversion, and the doc it came from still carries it.** It would pass
`tests/doc_citation_gate.sh` green, because the citation named a real line in a
real file.

**2. There is no `Datastore` entity, so `cluster_id` cannot be "in" it.** An early
draft asserted that `cluster_id` lives on the `Datastore` entity. MEASURED:
`grep -rn "struct Datastore\b" crates/ --include='*.rs'` returns nothing, and
`grep -rln datastore db/migrations-ts/` returns nothing. `DatastoreId` and
`ClusterId` exist only as wire types in `crates/zeroship-cdc-wire`
(`src/ids.rs`, consumed in `src/frame.rs` and `src/request.rs`). Both spellings
are open questions - issue #178 records that `ds_` contradicts its own proposal
and `clu_` is an invention. A design that placed zone or cluster identity on a
datastore entity would have been building on a type that does not exist.

**3. Gossip was dismissed for the wrong reason, then adopted for a different
one.** The first pass rejected gossip as "a distributed system added only to
invalidate a cache." That is a defensible objection to invalidation and irrelevant
to the actual blocker, which is that `HashRing`
(`crates/zeroship-gateway/src/proxy.rs`) has no mutation method and is built once
in `crates/zeroship-gateway/src/main.rs`, so adding a worker requires restarting
every gateway. Membership alone justifies the mechanism at a handful of apps;
invalidation then rides free. The error was scoping a mechanism to one of the two
problems it solves.

**4. An empty enforcement exception list "fails safe" is the wrong word.** It
fails **open**: every app serves unrestricted. MEASURED, the gates it disarms are
the `SPEND_LIMIT` and `ACCOUNT_SUSPENDED` refusals in
`crates/zeroship-gateway/src/enforce.rs`, neither an authentication or
authorization boundary, so the loss is revenue and unbounded infrastructure cost.
Open is the right availability choice here and "safe" is the wrong word for it,
because the word invites a reader to skip the liveness requirement that makes "no
exceptions" distinguishable from "no answer."

**5. The directory was first described as ONE blob.** That does not survive churn:
a single object must be republished in full for every spend-state flip, deploy and
app creation, on a platform where those are continuous. Hence the root plus chunks
in Part 3.

**6. The chunk key is the host, not the app id.** The first chunking draft keyed
on `sha256(app_id)`, which reads natural because the directory is "about apps." It
destroys the property chunking exists to preserve: a gateway holding only a `Host`
header cannot compute an app-id-keyed chunk index, so absence stops being locally
decidable and the negative answer stops being sound. Caught by asking what a
gateway holds at the moment it must answer, rather than what the entry is about.

**7. The worker does NOT merely extract two hashes from the manifest, as of
HEAD.** An earlier draft said the worker carries a whole manifest on every
`/internal/versions` poll to pull out the worker-entry blob hash and the runtime
descriptor hash. MEASURED, `reconcile_once`
(`crates/zeroship-worker/src/sync.rs`) also clones the whole manifest and passes it
to `cache::load_app` (`crates/zeroship-worker/src/cache.rs`), which compiles it
into the per-isolate declared policy. That behaviour landed in the HEAD commit
itself (`58deea301`). The design conclusion is unchanged and slightly strengthened
- both tiers need the manifest, neither needs it re-transmitted on a timer - but
the sentence as drafted described the tree of one commit earlier.

**8. "Only exceptions have rows" is false for spend.** `touch_state`
(`crates/zeroship-control/src/spend.rs`), called from `evaluate_all`, upserts a row
on every tick for every app, by explicit decision documented at the call site. The
exception predicate is `state <> 'allow'`, not row existence. Had this been written
as "rows are rare," a reader would have sized the exception feed by row count and
been wrong by the whole app population. Now arm A22.

**9. "Derive `sector_identifier` at the gateway" is not free.** MEASURED, the
gateway holds neither the scheme nor the base domain: grep for `app_base_domain`
under `crates/zeroship-gateway/src` returns nothing, and `extract_app_name`
(`crates/zeroship-gateway/src/router/dispatch.rs`) parses the first label of `Host`
and nothing else. The claim as first drafted read as though the gateway could
recompute the sector from what it already has. It cannot, today.

**10. "An app cannot move between zones" does not follow from key derivation.**
The HKDF salt is the `app_id` (`crates/zeroship-data-orm/src/encryption/keys.rs`,
`derive_key`), which is stable across a move. The real constraints are root-key
presence per zone and the absent rotation surface, both stated in that module's own
header. The corrected statement is narrower and more useful.

## Counting and citation errors

The point of this subsection is no longer WHICH numbers were wrong. It is that
every one of them was published as though it had been counted, and most of them
had not been. The counts are therefore removed and the shape of each miss is kept.

**11. The route join was undercounted.** `Registry::get_routes` names more tables
than the draft claimed, in top-level `LEFT JOIN`s plus a `LATERAL` that joins two
more; the endpoint as a whole issues several statements over more tables again.
Now covered by arm A9, which reads the SQL rather than a sentence.

**12. "The manifest is DUPLICATED" was a serious undercount.** There are durable
copies at rest AND wire copies re-transmitted on a timer, and only the latter are
what this proposal deletes. Naming the split changes what a fix has to cover.
Now arm A13.

**13. "One field is about routing" was an undercount.** `name` is the routing key
and `manifest` is the dispatch policy, and both are read on the request path; the
original phrasing read as though every other field were dead weight. Now arm A16.

**14. "`assets` is the only unbounded field and dominates" - both halves refuted.**
Several manifest fields are unbounded, and across the corpus `assets` is beaten by
`resources`, decisively so in the largest manifest measured. The design conclusion
survives but now rests on MARGINAL COST and build-driven cardinality rather than on
a share figure generalised from the single most asset-heavy example in the tree.
This is the more dangerous kind of error, because the share figure really was
correct about the one app it was measured on. Now arm A12.

**15. The `.zship` size sample was unrepresentative at the top.** The three
artifacts carried in from the conversation are real and are the SMALL end of a
population whose maximum is far larger; an earlier draft of this very correction
also miscounted how many artifacts sat above a threshold. The anti-swarming
argument survives comfortably; the range as quoted understated the maximum badly.

**16. The `Manifest` struct's field count was wrong.** The struct in
`crates/zeroship-bundle/src/manifest.rs` is the authority; no count is restated
here.

**17. The gateway PG pool's caller count was published twice and was wrong both
times.** One draft counted a rustdoc link in `crates/zeroship-gateway/src/lib.rs`
as a caller and the definition in `db.rs` as a call; the correction that replaced
it was also off. The durable claim never depended on the value. Now arm A30, which
excludes doc links and the definer by construction.

**18. Advisory locks reach much further than the draft's "a handful of modules."**
The draft's phrasing was technically true and read as an estimate at the low end
of its own range; the real reach across `crates/` and `libs/compio-postgres` is
far broader and strengthens the argument against etcd rather than weakening it.
The counts are now deliberately absent from prose - see correction 26 for why.

**19. An occurrence count in `crates/zeroship-control/src/egress_rules.rs` was
wrong.** The point - that app identity travels as a raw `Uuid` rather than a typed
id - stands, and `crates/zeroship-migrate-server/src/api.rs` is the sharper
example.

**20. The app id is a raw v4 UUID and `new_app_id` is dead.** MEASURED,
`zeroship.apps.id` is `t.uuid().notNull().default(uuidV4())`
(`db/migrations-ts/20260702000200_control_tables.ts`) and the only production
insert omits `id` entirely (`Registry::create_app`), so the database default
supplies it. `new_app_id()` exists in `crates/zeroship-core/src/typed_id.rs` and
mints `app_<base62(uuidv7)>`, but grep finds no caller outside that module's own
test. That function has since been DELETED, on the strength of this measurement:
the app id's one minter is now `AppId::mint` in `crates/zeroship-core/src/app_id.rs`,
which is typed and still unwired, so the observation above stands unchanged as a
statement about the column. So the identity flowing through every wire type carries no time ordering and
no locality hint, and `oac_<base62>` inherits that. Relevant to chunking only in
that it rules out any scheme keyed on id locality.

**21. Line drift, measured, and then removed as a category.** A large set of line
citations carried in from the design conversation pointed at the wrong line: at a
derive rather than its struct, at a doc block rather than the constant it
documents, at a function's neighbour, at a range that stopped one line short of
the statement the sentence was about. Several of them named REAL lines in REAL
files and would therefore have passed `tests/doc_citation_gate.sh` while pointing
at the wrong thing. **The repair is not a better set of line numbers. This document
now cites no line numbers at all**, and quotes code where a location matters.

**22. Some citations reproduced exactly**, and that is worth recording alongside
the misses, because it calibrates how much of a design conversation survives
re-checking: the poll URL construction, the interior of the linear scan, the blob
fetch, the frozen ring and its construction site, the database posture check, the
`oac` prefix constant, the manifest cap, and the manifest sizes for three named
examples. All of them are now cited by symbol.

**23. Derived byte figures shifted on re-measurement, four times.** Successive
drafts published a mean full `RouteEntry`, a non-manifest overhead, a directory
JSON entry and a manifest-share range. **None of the second set reproduced from
the first**, and that is the finding rather than the size of the gap. Two specific
lessons survive and are worth more than any of the values:

- **"Nearly constant" was the tell.** The non-manifest overhead is EXACTLY fixed,
  because every value in it has a shape fixed by a definition in this tree. A
  figure described as nearly constant meant something unnamed was varying, and
  something was.
- **A number whose construction is not written down cannot be checked, only
  believed.** One of the published figures could not be reconstructed from any
  field set tried. The repair was to pin the CONSTRUCTION, not to publish a better
  number - and the final repair, taken here, is to publish neither and gate the
  claim instead.

Every downstream aggregate moved with those inputs, and **no conclusion in this
document changed**, which is exactly why the errors survived: each of those numbers
was load-bearing for an argument that a small error could not move. That is the
definition of a decorative number, and decorative numbers are what this rewrite
removes.

**The packed-to-JSON band moved a FOURTH time, and that one was a category error
rather than an arithmetic one.** The two sides of the band had never been taken at
the same name length, so the ratio compared two different apps. Part 3 now derives
the packed entry as a fixed record plus the name from the column definitions, and
the JSON as a fixed skeleton plus the name and literals from the exact byte string,
so both move together and the band is a property of the ENCODING alone. Two things
the re-derivation found that no amount of re-checking the ratio would have: the
earlier packed estimate was right BY CANCELLATION (an omitted arena pointer against
an over-charged flag byte), and the question the band was posed to settle was the
wrong one - a general-purpose compressor recovers most of the JSON's penalty, so
the penalty is resident memory and not bandwidth. **A ratio between two numbers is
not a measurement of either.**

## Found by the adversarial re-audit of `ed161c341`

**24. Wrong line citations at several sites, in a document whose own opening said
the line is a courtesy.** A worker-cache parameter cited at its closing paren and
return type - repeated three times across Part 1, an open decision and correction
7. A cookie constant cited at its doc line. A clone of three things cited at a
range that excluded the third. A sleep cited at the line before it. And a sentence
quoted VERBATIM from `docs/architecture/data-system.md` cited at a range that did
not contain it, twice. **A quoted string outside its own citation is the sharpest
form of this error**, because the quote looks like proof that the line was opened.
This is the correction that motivated dropping line numbers from the document
entirely.

**25. The worker's poller was cited at its neighbour.**
`crates/zeroship-worker/src/sync.rs` has both `start_sync`, which spawns the
PER-THREAD reconcile loop, and `start_version_poller`, which spawns the
process-wide `version_poll_loop`. The citation opened on the wrong function and
excluded the right one's entry point, while still covering the loop body - which is
why it read as correct. The claim it supports (one poller, shared through
`SharedVersions`, HTTP not multiplied by thread count) is unaffected.

**26. The advisory-lock reach was published, corrected, and was wrong both times.**
The first count was wrong; the correction that replaced it was ALSO ambiguous,
because a `crates/`-only scan and a `crates/` plus `libs/` scan give different
answers and neither of the published pair was the pair that had been measured. This
is correction 18 committing, at smaller scale, the same error it was written to
fix - which is the strongest possible argument for removing the counts and keeping
only "the role is filled." Now arm A26.

**27. The provenance paragraph went stale within hours of being written.** It
described a dirty working tree as a caveat on some line numbers. Those
modifications were committed the same day, so the caveat pointed at a condition
that no longer existed while the readings it warned about were fine. **A provenance
note that names a transient state is a note that expires**; it now names the
commits instead.

**28. "Nothing in the tree ever writes a non-empty `runtime_assets`" is true of
producers and false as written.** `crates/zeroship-core/tests/types_test.rs`
constructs one with an entry, to exercise variant validation. The design point (no
production writer, so the manifest is immutable-after-deploy today) stands; the
universal quantifier did not. Now arm A15.

## Found by settling the replication envelope

**29. The document derived a per-entry size and never multiplied it by anything.**
Part 3 spent a section deriving the entry width field by field, and "replicate the
whole directory to every node" was never checked against any budget a node actually
has. The two the tree declares are `blob_cache_mem_mb` and `blob_cache_disk_gb`
(`crates/zeroship-gateway/src/config.rs`). Against those, the directory at the
platform's stated target OVERRUNS the whole memory budget. **The per-entry
derivation was necessary and, on its own, decided nothing: an entry size is not an
envelope.** The load-bearing distinction the envelope exposes is that **the
directory is not a cache**. `BlobCache` may be any size because a miss has a fetch
behind it; the directory's whole value is that a miss is an ANSWER, so it has no
miss path, no eviction and no tunable - every byte is permanent working set on
every node. Now arm A17, which makes the comparison at boot instead of in a table.

**30. Chunking breaks the authoritative negative, and the document stated only the
branch where it works.** "The directory must be chunked" said a gateway that holds
the right chunk at the current root can answer a negative soundly. True, and the
complement was never written: a gateway that does NOT hold that chunk has no way to
say so. MEASURED, a lookup has exactly two outcomes today -
`RouteCache::lookup_by_name` returns an `Option` and `None` becomes a not-found in
dispatch - because under one atomic full-table pull "absent" and "absent from a
complete table" are the same fact. Chunking separates them, and a missing chunk
becomes authoritative not-founds for a hash-random slice of the platform,
indistinguishable at the edge from correct negatives. The existing mitigation does
not generalise: `is_ready` (`crates/zeroship-gateway/src/health.rs`) tests
FRESHNESS (`staleness_budget` and `is_fresh` in
`crates/zeroship-core/src/readiness.rs`), not completeness, so a node with a current
root and all but one chunk reports ready. **This is the sharpest correction in the
document**, because the chunking section was written to preserve the very property
it silently removed, and the failure it introduces is quieter than the one it
replaced: today a cold gateway answers not-found for everything and `/readyz` says
so. Now arm A19.

**31. The chunk key is not canonical, and the tree disagrees with itself about the
case of an app name.** `chunk_index = f(sha256(host))` is deterministic only if
producer and consumer hash identical bytes. MEASURED, `extract_app_name` returns
the Host label verbatim, the index is a case-sensitive `HashMap`,
`Registry::create_app` accepts uppercase, and `apps_name_key`
(`db/migrations-ts/20260702000600_constraints_indexes_fks.ts`) is a byte-comparing
UNIQUE - so `MyApp` and `myapp` are two permitted rows behind one case-insensitive
DNS name. Yet `is_reserved_app_name`
(`crates/zeroship-control/src/reserved_names.rs`) folds case. **The tree case-folds
where a name is CLAIMED and does not where it is ROUTED.** Today that yields an
ordinary miss; under chunking it routes the lookup to a chunk where the host is
legitimately absent, and correction 30's rule then certifies the not-found as
authoritative. Canonicalisation becomes a prerequisite, not hygiene. Open issue
#208; now arms A5 and A6.

**32. The P2P rejection was measured against a corpus while an enforced ceiling sat
far above it.** "Rejected on artifact size" rested on the built examples in this
tree. `MAX_COMPRESSED_BYTES` (`crates/zeroship-bundle/src/limits.rs`) is what the
platform actually admits, and it is orders of magnitude larger. The verdict survives
- peer negotiation does cost more than the transfer at the sizes this pipeline
produces - but **it was stated as a property of the artifact format when it is a
property of the sample**, which is the error this document corrects in itself
repeatedly over byte counts and had not noticed it was making over a design
decision. The repair is a named re-open trigger, expressed as a ratio: now arm A28.

**33. The assets-digest argument was made on slope alone, and the enforced caps
make a stronger one.** The document argued that `assets` cardinality is set by a
build rather than a person, so it reaches the cap first. Both true. Unstated:
`MAX_MANIFEST_BYTES` rides inline on every `RouteEntry` at the configured poll
interval, so one legal app at the cap costs every gateway that cap per interval
forever, for an object that never changes; and `MAX_BLOBS_PER_DEPLOY` permits an
asset map strictly larger than the whole manifest is allowed to be, so the two
enforced limits contradict each other and the binding one refuses well below the
blob-count cap. The digest reconciles them. **This argument depends on no corpus
and no forecast**, which is why it is the one that survives after every number in
this document was removed. Now arms A10 and A11.
