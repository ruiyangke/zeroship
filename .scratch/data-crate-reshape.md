# Reshaping the data plane: one recommendation

## 1. What is actually wrong

**The naming problem is load-bearing; the split problem is mostly a membership problem wearing a
split's clothes.** The subsystem invented a third naming family for one subsystem, and the tree
already voted against it everywhere except in the directory names: every public type in
`crates/zeroship-data-*`, `crates/zeroship-plugin-db` and `crates/zeroship-schema` that carries the
subsystem's word spells it `Db` (`DbError`, `DbPlan`, `DbBinding`, `DbService`, `DbServiceConfig`,
`DbPlugin`, `DbResourceKey`, `DbLifecycle`, `DbPlatform`, `Db`) and not one spells it `Data`
(`grep -rhoE 'pub (struct|enum|trait|type) (Db|Data)[A-Za-z]*' crates/zeroship-data-*/src
crates/zeroship-plugin-db/src crates/zeroship-schema/src | sort -u`). The commit log agrees:
`git log --format=%s | grep -cE '^[a-z]+\(db\)'` against
`git log --format=%s | grep -cE '^[a-z]+\(data[a-z-]*\)'` returns 806 and 0, and the commits that
*created* these crates are in the first set. Worse than `data` vs `db` is that four of the names are
undecidable: nobody can answer "is this `core` or `engine`" from the words, so `zeroship-data-core`
accumulated encryption, a masking transform, budgets, a process-global broker, a thread-local schema
cache and a retry policy with no shipped caller, and `zeroship-data-query-builder` names a technique
for a crate whose principal type is `DbPlan` and whose production dependents are zero
(`grep -rl zeroship_data_query_builder --include='*.rs' crates libs sdks` outside its own tree returns
two plugin-db test targets and one `#[cfg(test)]` assertion inside `zeroship-schema/src/query.rs`).

**The split is roughly the right number of boundaries in roughly the right order, and the code does
not observe them.** `crates/zeroship-plugin-db/src/lib.rs` and `crates/zeroship-data-engine/src/lib.rs`
carry a re-export ladder (`grep -c '^pub use zeroship_' <each>`), and the ladder is why files that
most obviously span a boundary name neither owning crate: `crates/zeroship-plugin-db/src/change_stream_pg.rs`
imports `crate::backend::postgres::PostgresBackend` while living in the V8 adapter, and
`crates/zeroship-plugin-db/src/slot_reaper.rs` imports `crate::backend::pg_error` from a crate two
tiers away. The ladder exists because the split was executed so that no call site would change, which
means no reader can see it; that is also why `tests/lib/tier_direction_census.sh` prints a clean
violation table while discarding the re-export edges as "not a placement question". The genuine
structural defects are three, and none of them is the number of crates: one crate is dead
(`zeroship-data-query-builder`), one holds no code (`zeroship-data-cdc-server`: `config.rs`,
`lib.rs`, `main.rs`, and `main.rs` prints a refusal and exits), and one crate that is supposed to be
a thin host adapter holds seven modules that name no V8 at all. So: rename the family, fix membership,
delete what is dead, put code in the crate that is meant to hold it. Do not rebuild the rings.

## 2. The recommendation

Nine crates in the problem space become eight. The vendor split survives verbatim because it is the
one part of today's shape that pays for itself; the floor is rebuilt around a membership test; the
protection cluster gets one manifest; the relay gets code.

| Crate | Location | Owns | Depends on |
| --- | --- | --- | --- |
| `zeroship-db-sql` | `crates/` | The floor. All of `zeroship-schema` (the DML builders in `query.rs`, `SqlDialect`, `ident.rs`, `descriptors.rs`, `mask_codec.rs`, the `diff.rs` value types, `QueryError`) plus `zeroship-data-core`'s contract half (`storage.rs`'s eight capability traits, `capability.rs`, `binding.rs`, `budgets.rs`, `error.rs`/`DbError`), plus `change_sink.rs` promoted out of the SQLite crate, plus the three new store ports (`MaskPolicyStore`, `UnmaskAudit`, `RawColumnRead`) and `WalSink`. Deletes `lock_policy.rs`, `diff::compute_diff`, and absorbs the one surviving copy of the reserved-prefix tables. | `zeroship-core`, `serde_json`, `sha2` |
| `zeroship-db-protection` | `crates/` | `masking.rs`, `encryption/` (AES-256-GCM, `KeyStore`, AAD, the ciphertext wire format), and the five protection passes now in `zeroship-data-engine/src/crud/`: `mask_pass.rs`, `encryption_pass.rs`, `mask_policy.rs`, `protection_floor.rs`, `unmask.rs`, plus the unmask argument parsing lifted out of `v8_classes/masked_value.rs`. | `zeroship-db-sql`, `aes-gcm`, `hkdf`, `hmac`, `sha2`, `zeroize`, `base64`, `serde_json` |
| `zeroship-db-postgres` | `crates/` | All of `zeroship-data-postgres`, plus `change_stream_pg.rs` and `PostgresCanceller` moved in, plus the `PgClient` / `PgPool` / `PgCancel` newtypes and the `open_postgres_backend` composer that `backend_selection.rs` records as written and deleted. | `zeroship-db-sql`, `zeroship-db-protection`, `zeroship-core`, `compio-postgres`, `compio` |
| `zeroship-db-sqlite` | `crates/` | All of `zeroship-data-sqlite` minus `change_sink.rs`; `mask_policy_store.rs` becomes `impl MaskPolicyStore for SqliteBackend`. Drops the declared-and-unnamed `uuid`. | `zeroship-db-sql`, `zeroship-db-protection`, `zeroship-core`, `rusqlite`, `sqlite-vec`, `flume`, `compio` |
| `zeroship-db` | `crates/` | The centre and the composition root: `crud/` (minus the protection passes), `transaction/`, `exec.rs`, `tx_lanes.rs`, `tx_route.rs`, `backend_handle.rs`, `backend_selection.rs`, `backend/`, `metrics.rs`, `system_shape_charter.rs`, plus `broker.rs`, `read_set.rs`, `schema_cache.rs` from the old floor, plus `service.rs`, `context.rs`, `cdc_lifecycle.rs`, `drop_namespace.rs` and the URL-parsing composition root from `zeroship-plugin-db/src/lib.rs`. Deletes `auth/`. | `zeroship-db-sql`, `zeroship-db-protection`, `zeroship-db-postgres`, `zeroship-db-sqlite`, `zeroship-core`, `zeroship-metering`, `zeroship-migrate-policy`, `compio` |
| `zeroship-db-v8` | `crates/` | Only what names V8 or `zeroship_runtime`: `v8_bridge.rs`, `tx_scope.rs`, `op_error.rs`, `v8_classes/`, `DbPlugin` and its `NativePlugin` impl, the criterion benches. `lib.rs` stops being a ladder. | `zeroship-db`, `zeroship-db-sql`, `zeroship-db-protection`, `zeroship-runtime`, `zeroship-runtime-macros`, `zeroship-core`, `v8` |
| `zeroship-db-relay` | `crates/` | Replaces `zeroship-data-cdc-server` and holds real code on day one: `replication.rs`, `wal_consumer.rs` and `slot_reaper.rs` moved out of the V8 adapter, behind the `WalSink` port, plus the existing `config.rs` and a `main.rs` that runs. | `zeroship-db-sql`, `zeroship-db-postgres`, `zeroship-db-wire`, `zeroship-core`, `compio-postgres`, `compio`, `clap` |
| `zeroship-db-wire` | `crates/` | `zeroship-cdc-wire` renamed into the family and added to the root `[workspace.dependencies]` table, which it is absent from today (`grep -n 'cdc-wire' Cargo.toml`). Content unchanged. | `zeroship-core`, `serde`, `serde_json`, `sha2` |

Deleted outright: `zeroship-data-query-builder` (dead), `zeroship-schema` and `zeroship-data-core`
(dissolved into the floor and the centre). Renamed with no content change, in a separate commit:
`zeroship-plugin-kv`, `-storage`, `-workflow` become `zeroship-kv`, `zeroship-storage`,
`zeroship-workflows` (their `NativePlugin::namespace()` values are `"kv"`, `"storage"`, `"workflows"`,
so the plural is a bug fix as well as a rename).

**The rule that generates all of it:** every crate is named for the one thing it may name that no
sibling may, drawn from a closed list of six kinds (a vendor, a host, a separate process, the bytes
between two processes, the SQL sent to a database, and the keys that turn ciphertext into plaintext),
and the crate that may name all of them wears no suffix.

The prefix needs no rule: it is the `env.*` namespace the crate serves. A third backend is
`zeroship-db-mysql` without a meeting (`SqlDialect` already declares the variant). A second host is
`zeroship-db-wasm`. A fourth consumer of the query grammar earns no crate at all: it depends on
`zeroship-db-sql`, which names no backend. A candidate that cannot answer "what does your manifest
refuse" is a module.

### The rings

Read the stack downward; a crate may name anything below it and nothing above it.

```
  zeroship-worker . zeroship-cli                  the linkers
  ---------------------------------------------------------------
  zeroship-db-v8            zeroship-db-relay     RING 4  host, and process
   (isolates, marshalling)   (replication, slots, the wire)
  ---------------------------------------------------------------
  zeroship-db                                     RING 3  composition + crud + SC-1
   (composes both backends: this is not an exception, it is what a
    composition root is, and the rule that called it one is retired)
  ---------------------------------------------------------------
  zeroship-db-postgres   |   zeroship-db-sqlite   RING 2  one driver each,
   (compio-postgres)     |   (rusqlite)                   mutually blind
  ---------------------------------------------------------------
  zeroship-db-protection                          RING 1  keys and the plaintext path
  ---------------------------------------------------------------
  zeroship-db-sql                                 RING 0  SQL text and the contract
  ---------------------------------------------------------------
  zeroship-core                    zeroship-db-wire (leaf, off to the side:
                                    two processes agreeing on bytes)
```

What each manifest refuses, which is the whole of the enforcement:

- `zeroship-db-sql` refuses `compio-postgres`, `rusqlite`, `v8`, `zeroship-runtime`, `compio`.
  Dropping `lock_policy.rs` is what lets it drop `compio` and make its own abandoned claim true again.
- `zeroship-db-protection` refuses both drivers and `v8`. A reviewer asking "can app JavaScript reach
  plaintext" reads one dependency list.
- `zeroship-db-postgres` refuses `rusqlite`; `zeroship-db-sqlite` refuses `compio-postgres`. This is
  the one property of today's shape that pays for itself and it survives verbatim.
- `zeroship-db` refuses `compio-postgres` and `rusqlite`. After the newtypes and the `auth/` deletion
  there is nothing left for it to name, so the manifest omission replaces
  `tests/vendor_embedding_gate.sh` at compile time, for everyone.
- `zeroship-db-v8` is the only crate that may name `v8` or `zeroship-runtime`. Membership is a
  one-word grep.
- `zeroship-db-relay` refuses `zeroship-db`, `zeroship-db-v8`, `zeroship-runtime`, `v8`, `rusqlite`.
  The worker's CRUD pipeline, the SC-1 reducer, the lanes and the broker are outside its closure by
  compilation.

### What is grafted, and from where

- **From Proposal 1:** the migration ordering (small independently green commits first, the mechanical
  rename late and isolated, every behavioural change alone and revertable); the `WalSink` inversion as
  its own commit before any file moves; deleting `zeroship-data-engine/src/auth/`; the `PgClient` /
  `PgPool` / `PgCancel` newtypes and the restored `open_postgres_backend` composer.
- **From Proposal 3:** the ten-line gate arm that replaces two censuses (no crate in the family may
  `pub use` another crate's module, floored by the number of `lib.rs` files examined); the ruling that
  the plugin siblings must be renamed in the same breath or the exercise recreates the two-families
  defect it exists to close.
- **From Proposal 4:** the protection cluster gets one crate and therefore one manifest; the membership
  discipline that keeps a floor from re-forming as a grab bag; keeping `tests/decision_four_gate.sh`
  on the grounds that its subject is a cost rather than a boundary, and adding the `SqlDialect` arm it
  is missing.
- **From Proposal 5:** the gate criterion I adopt wholesale (shell gates should guard the privilege
  line and nothing else; every other boundary is guarded by a manifest that omits a dependency, or not
  at all); keeping `zeroship-cdc-wire` as a crate and wiring it; the ruling that nothing here belongs
  in `libs/`, and its proof (`zeroship-data-query-builder` has zero dependencies and hard-codes
  `PLATFORM_RESERVED_COLLECTION_PREFIXES`, `"__zs_"` and `PLATFORM_FIELD_NAMES`, so dependency count
  measures coupling, never audience); the correction that the mask-codec fork is pinned, so collapsing
  it is a maintenance win and not a security fix.
- **From Proposal 2:** one fact and no shape. `BackendHandle`'s inherent surface is already
  associated-type-free at every method (`introspect_schema` returns the concrete `LiveSchema` on both
  arms, `open_tx_session` returns `TxConnection` and not `Self::Client`, `key_store` returns a domain
  type, `pool_counts` returns a tuple), and the downcast surface it is defended with has no production
  callers: `as_postgres` and `as_sqlite` appear outside `backend_handle.rs` only in two prose comments,
  in `backend/mod.rs` past that file's first column-zero `#[cfg(test)]`, in `tx_scope.rs` inside a
  `#[cfg(test)]` module, and in a plugin-db test file. So the standard reason for refusing dependency
  inversion (three associated types on `SqlExecutor`, `SchemaIntrospect`, `ChangeStream`) prices a
  trait nobody would erase. I keep the enum anyway, for the reason in section 3, but the false blocker
  should stop being repeated in this repo's prose.

## 3. Why this over the alternatives

| | P1 six crates | P2 ports and adapters | P3 product word | P4 seams | P5 audience | This |
| --- | --- | --- | --- | --- | --- | --- |
| Graph resolves | yes | **no, two cycles** | yes | **no, one cycle** | no (floor lacks `zeroship-core`) | yes |
| Family count | 6 | 9 | 6 (+3) | 9 | 10 | 8 |
| Floor predictable from its name | no (`-backend` holds four concerns and sits below the backends) | n/a (root is the ports) | no (`-common`) | yes but cyclic | no (`-catalog`) | mostly (stated miss below) |
| Protection has one manifest | no | no (format and passes split) | no | yes | no (module) | yes |
| Privilege line | claims a win it does not buy | correct (repoints only) | **dissolves it** | honest: no change | claims a green gate it does not get | honest: destination built, move not made |
| Adapter mutual blindness | kept | kept | kept | kept | kept | kept |
| Non-mechanical commits | 1 (`WalSink`) | 1 enormous (erase `BackendHandle`) | 0 | 2 | 2 (incl. a hand split of `query.rs`) | 2 (`WalSink`, store ports) |
| Incremental rebuild | worse (one ~40k-line unit) | best | worse | good | worst | better than today for protection edits, unchanged elsewhere |
| Gates deleted | many | many + the closure gate's scope | 1938 lines | deletes the closure gate on a false argument | targeted | four, plus one new ten-line arm |

**Where the judges disagreed, and who I sided with.**

*The engineer ranked P1 first; the architect ranked it last.* The split is over whether removing the
use-case-to-adapter edge is worth a rewrite. I side with the architect that P2's measurement is correct
and with the engineer that a rename must not carry a trait-object refactor: `zeroship-db` keeps
`BackendHandle` as a closed sum, and the `Datastore` / `TxSession` / `TxCanceller` port shape is
recorded as the follow-on a third backend would pay for, not smuggled into a rename. P3 put this best
and it is the sentence I am governed by: a rename proposal that smuggles in a trait-object refactor is
how a mechanical commit becomes a six-week branch.

*Two proposals delete `crates/zeroship-worker/src/slot_reaper.rs` and one of them calls it "the commit
that pays for the whole migration". Both would turn `tests/worker_replication_privilege_gate.sh` red.*
I read the gate. Arm 5's third join is `mints_total > 0` with `reaper_live != 1`, and `reaper_live` is
computed from two greps against the worker's `main.rs` (`slot_reaper::start(` and
`run_server_with_slot_reaper(server.run(), slot_reaper_task)`). The worker still mints one slot per
(app, worker) through `cdc_lifecycle` into `change_stream_pg::spawn_consumer` into
`replication::ensure_worker_slot`, so deleting the reaper drops no privilege and removes the only
cleanup for slots that are still being created. I side with the architect and with P2's handling: the
reaper module moves into `zeroship-db-relay`, the worker keeps supervising it and links that crate, and
the edge from `zeroship-worker` onto `zeroship-db-relay` is the visible, deletable statement of the
remaining debt. It goes away in the same commit that moves the streaming path, and the gate inverts
itself then, exactly as its header says it will.

*P4 asserts the mask-sentinel codec fork is unpinned and builds a security case on it; P5 checked and
corrected itself.* P5 is right: `mod cross_codec_parity` is at the bottom of
`crates/zeroship-schema/src/mask_codec.rs` and `crates/zeroship-migrate-backend/src/mask_codec.rs` is
the second implementation it pins, through a dev-dependency running from the lower crate up into the
higher one. Collapsing the fork deletes ten hand-written enum translators and reverses a dangerous
dev-edge direction. It is a maintenance win. It is not a live vulnerability and I will not sell it as one.

*P4 deletes `tests/data_crate_closure_gate.sh` on the argument that an unlisted dependency is `E0433`.*
That argument does not reach the mechanism the gate exists for, which its own header states: a crate
acquires a dependency, that dependency pulls the driver, and no line of the crate's own source changes.
I side with P2 and P5: the gate survives, with P5's scope. Its `GUARDED_CRATES` becomes
`zeroship-db-sql`, `zeroship-db-protection`, `zeroship-db-wire` (what both sides of the privilege line
link) rather than a list that puts the whole CRUD pipeline inside a guard no privileged service will
ever link. Arm 3's `RELAY_LINKER` inversion goes, because the relay now has real dependents.

*The owner's judge docked P4 for going nine crates to nine, then grafted a set of changes that also
lands at nine.* Saying so openly: the count is not the metric, but it was the complaint, so the shape
has to answer it. It answers by deleting what is dead and merging what refuses nothing. The
`-sql` / `-contract` split that P4 wanted (and drew backwards) buys ordering legibility and no refusal,
so it is a module boundary; that is the merge that takes this to eight. The non-vendor, non-service
crates go from five (`data-core`, `data-engine`, `query-builder`, `schema`, `plugin-db`) to three
(`-sql`, `-protection`, `db`).

*One correction to all three panels and to `AGENTS.md`.* Every panel repeats that the DB-3 audit gap is
open, that `sanitize_app_actor` strips the actor whole and leaves a denied unmask indistinguishable from
anonymous traffic. It is closed. `SanitizedActor` now carries `rejected_claim`, it is threaded through
`UnmaskFieldArgs` and `BulkUnmaskArgs`, and `UnmaskAuditRow` has a `claimed_actor` column whose doc says
it is kept apart from the trusted columns on purpose. That removes one of P4's arguments for the
protection crate, and I have not counted it. The protection crate still earns itself on the manifest,
and `AGENTS.md`'s paragraph should be corrected by whoever next touches it.

**The one thing I will not claim.** `zeroship-db-sql` under-describes three of the eight capability
traits: `LockManager`, `Backup` and `ChangeStream` are not about SQL text. They stay because the only
alternative is a second floor crate that refuses nothing, and by volume the crate is overwhelmingly the
builders, the dialect, the identifier rules and the sentinel codec, which is what the name predicts.
The sentinel codec in particular belongs there and not in `-protection`: it renders `COMMENT ON COLUMN`
payloads, it is SQL text, and it is called from `query.rs`'s production sentinel emitters
(`crate::mask_codec::build_mask_sentinel`, `crate::mask_codec::ENC_SENTINEL_PREFIX`). That edge is
exactly what makes P2's and P4's graphs cycle: both put the codec above or beside `query.rs` while
`query.rs` calls it. Keeping `query.rs`, `mask_codec.rs` and `descriptors.rs` in one crate below
`storage.rs` is the only cut that survives both live edges without a hoist, and it is why the two
buildable proposals are the two that did it.

## 4. What it costs

Pre-launch, so no shims, no aliases, no re-export bridges: every old name disappears in the commit that
replaces it. Eleven commits, each independently green.

1. `refactor(db): delete the unwired query ir crate`. Remove `zeroship-data-query-builder`, its root
   workspace entry, its two dev-dependency lines, the third arm of
   `reserved_collection_prefixes_match_migration_engine`, and its entry in the closure gate. Repoint the
   two plugin-db test targets. *Tedious.*
2. `refactor(db): delete the unshipped bounded lock retry`. `lock_policy.rs` has no shipped caller: its
   PostgreSQL user `lock_guard.rs` is declared `#[cfg(any(test, feature = "test-helpers"))]`, its engine
   user sits past `backend/mod.rs`'s first column-zero `#[cfg(test)]`, and `zeroship-data-sqlite/src`
   names `BoundedLockAcquire` zero times. Inline the backoff into `lock_guard.rs`. This is what lets the
   floor drop `compio`. *Tedious.*
3. `refactor(db): delete the per-app role bootstrap from the data plane`. Remove
   `zeroship-data-engine/src/auth/`; merge its role provisioning into
   `zeroship-migrate-server/src/provisioning.rs`, which already does `CREATE ROLE` and `GRANT` in a
   process that runs no creator code. Repoint the plugin-db test targets. Removes the largest single
   source of vendor signatures in the engine. *Tedious, with one real design statement in it.*
4. `refactor(db): encapsulate the postgresql driver behind newtypes`. `PgClient`, `PgPool`, `PgCancel`;
   `type Client = PgClient`, mirroring what the SQLite crate already does with `SqliteSessionHandle`;
   restore `open_postgres_backend`, and delete the comment in `backend_selection.rs` that records why it
   was written and removed the same hour. *Tedious.*
5. `feat(db): invert the wal consumer onto a sink port`. Add `WalSink`, implement `BrokerWalSink` beside
   the existing `BrokerChangeSink`, and route `wal_consumer.rs`'s three broker calls through an injected
   sink. **No files move in this commit. GENUINELY RISKY:** it is priced from a measurement (the consumer
   imports exactly `SuppressGuard`, `has_subscribers`, `publish`), and if the coupling is deeper than
   those three symbols the relay would need the broker, which lives in the centre, which is a cycle. It
   gets its own commit so the bet is settled before anything moves.
6. `refactor(migrate): collapse the forked mask sentinel codec`. Delete
   `crates/zeroship-migrate-backend/src/mask_codec.rs`, take a normal dependency on the surviving copy,
   delete `mod cross_codec_parity` and its ten enum translators, delete `zeroship-schema`'s upward
   dev-dependencies and the two rows in `tests/sync_claim_gate.sh`. This also reverses a dev-edge that
   closes into a cycle the day the migrate family takes a normal dependency downward. **RISKY in review,
   not in mechanism:** it changes what the migration engine writes into the catalog, so it lands before
   the renames and alone.
7. `refactor(db): move the protection passes behind store ports`. `MaskPolicyStore`, `UnmaskAudit`,
   `RawColumnRead` into the contract; the `BackendHandle` methods whose PostgreSQL arm is a no-op become
   vendor trait impls; `protection_floor` takes a `SchemaIntrospect` rather than a `TxRoute`. **THE ONE
   PIECE OF REAL ENGINEERING, and the shape's single precondition:** if the operations the passes need
   are more than the enumerable set, the protection crate needs a backend and the graph cycles. Enumerate
   them first, in a diff someone else reads:
   `grep -rn 'TxRoute\|BackendHandle\|route\.' crates/zeroship-data-engine/src/crud/{unmask,mask_policy,protection_floor}.rs`.
   If it comes up short, take branch B in section 6 and everything else in this plan is unchanged.
8. `refactor(db)!: rename the data plane onto the product's own word`. **THE BIG MECHANICAL ONE, and
   there is no way to make it small.** Seven `git mv`s, one root `[workspace.dependencies]` edit, one
   manifest per crate, the file moves listed in section 2, and the deletion of both re-export ladders
   with every `crate::backend::` / `crate::broker::` / `crate::query::` / `crate::crud::` path rewritten
   to name its owning crate. Most of them resolve unchanged because the module is now in the crate that
   spells it, which is the point. It is not splittable: the ladders are what make a partial move compile.
   It contains no logic. Budget the review here and nowhere else. *Tedious, and large.*
9. `refactor(db): move the cdc modules into the relay crate`. `replication.rs`, `wal_consumer.rs`,
   `slot_reaper.rs` into `zeroship-db-relay`; the relay's `main.rs` stops printing a refusal and runs;
   the relay links `zeroship-db-wire` for its status surface. **Precondition, stated in the commit
   message so a later editor does not "finish the job": do not delete
   `crates/zeroship-worker/src/slot_reaper.rs` or its supervision while the worker still mints slots.**
   *Tedious, with one trap.*
10. `refactor(runtime)!: name the plugin crates for their namespace`. `zeroship-plugin-kv`, `-storage`,
    `-workflow` become `zeroship-kv`, `zeroship-storage`, `zeroship-workflows`. Separate from the db
    rename so a bisect never lands mid-family. Also updates the scope vocabulary in `CONTRIBUTING.md`.
    *Tedious, and optional; skipping it leaves two naming families, which is the defect being closed.*
11. `test(gate): retire the boundary gates the manifests now hold`. Delete
    `tests/vendor_embedding_gate.sh`, `tests/lib/tier_direction_census.sh`,
    `tests/lib/tier_signature_census.sh`, `tests/lib/vendor_value_flow_census.sh`. Retarget
    `tests/data_crate_closure_gate.sh` at the floor, protection and the wire crate, and drop its
    `RELAY_LINKER` inversion. Keep `tests/decision_four_gate.sh`, repath its baseline, ratchet
    `SITE_CEILING` and `BACKEND_COST_CEILING` down by what commits 3 and 7 removed, and add the arm it is
    missing: its header claims "no dialect match" while its arms never read `SqlDialect`, which the
    pipeline branches on in `crud/bytes_pass.rs`, `crud/mod.rs`, `crud/write_pipeline.rs` and
    `tx_route.rs`. Keep `tests/worker_replication_privilege_gate.sh` unchanged in subject, paths
    repointed. Keep `tests/contract_feature_invariance_gate.sh` exactly as it is: `storage.rs`'s own
    header records three real breakages from a `cfg` on a trait member, and the new store ports inherit
    that hazard. Add the ten-line arm: `for f in crates/zeroship-db*/src/lib.rs; do grep -n '^pub use
    zeroship_' "$f"; done` must be empty, floored by the number of `lib.rs` files examined so it cannot
    pass by looking at nothing. Re-run `tests/gate_arm_census.sh tests`.

**Risky versus tedious, stated plainly.** Genuinely risky: commit 5 (a coupling bet that can produce a
cycle), commit 7 (an enumeration that can produce a cycle), commit 6 (a behaviour change in what the
catalog is written with, landing just before its pins are deleted; collapse first, delete the pins in the
same commit only because the fork is gone, never before). Merely tedious, in descending size: commit 8,
then 1 through 4, then 9 through 11. Commit 8 is the one that hurts and the one that contains nothing.

## 5. What it does not fix

- **The privilege violation.** The worker still executes `START_REPLICATION SLOT ... LOGICAL`, still
  mints a slot per (app, worker) from creator JavaScript, and still holds the `REPLICATION` attribute.
  This reshape builds the destination and the port; it does not make the move, which is blocked on the
  wire decode loop and on `DatastoreId`, which is minted by nothing. Anyone who reads commit 9 as having
  moved the privilege will delete the worker's reaper and turn the gate red.
- **The third-backend cost.** `SqlDialect` still branches inside the pipeline and `BackendHandle`'s arms
  still carry literal per-dialect SQL. `tests/decision_four_gate.sh`'s ceilings are code properties, not
  crate properties; this shape moves them by what commits 3 and 7 delete and no further. Only moving the
  per-arm SQL into the dialect renderer fixes it, and that is a different project.
- **The other schema fork.** `zeroship-migrate-core/src/schema/query.rs` still carries nineteen
  identically-named public functions against the floor's copy, and the live `compute_diff` is
  migrate-core's. We collapse the sentinel codec only, because that one has a writer in one family and a
  reader in the other. `AGENTS.md`'s "shared schema authority" line describes a fork, not a sharing:
  `grep -rn zeroship_schema crates/zeroship-migrate-server/src` returns one production line
  (`use zeroship_schema::SchemaName;` in `apply.rs`), and no other migrate manifest declares it at all.
- **The size of the centre.** `zeroship-db` still holds the CRUD pipeline, the SC-1 protocol, the
  dispatch enum, the lane owner, the broker and the composition root. Protection leaving means editing a
  protection pass no longer recompiles the reducer, which is the common edit in this subsystem; editing
  `crud/read_pipeline.rs` still does. A better-named large crate is still a large crate.
- **`tx_lanes` keyed by `app_id`.** It is correct only because one V8 isolate per (app, live deploy) owns
  the thread. A second host sharing those lanes is a tenant-isolation bug rather than a compile error.
  `zeroship-db-wasm` must not exist until the lanes are keyed by a host-supplied session identity, and
  today that is a comment, not a manifest.
- **`zeroship-core` stays fat.** `typed_id.rs` and `entity_id.rs` have no `use` lines and sit behind a
  closure containing `cyper`, `jsonwebtoken`, `ed25519-dalek`, `compio` and `clap`. Extracting
  `zeroship-id` is a real find and a real win, and it is a workspace-wide rename touching every crate
  that names `typed_id`. It is a separate decision, deliberately not bundled here; bundling it would put
  three mechanical renames in one quarter.
- **`BackendHandle` stays a closed sum.** The erasure is possible and cheaper than this repo believes,
  and it is still not what a rename should buy.

## 6. The one decision

The fork is real, it is taste rather than engineering, and everything else in the shape is identical on
both branches.

**Does `zeroship-db-protection` exist as a crate?**

*Branch A, which I recommend.* It exists. The masking transforms, AES-256-GCM, the key store, and the
five protection passes share one manifest that cannot name `v8` and cannot name a driver, so the question
"can app JavaScript reach plaintext" is answered by reading one dependency list rather than by tracing
five files across two crates. The price is commit 7: one piece of genuine engineering that must land
before the rename, whose correctness turns on an enumeration of the operations the passes need from a
backend, and which cycles if that enumeration is short by one. Family of eight. Take this branch if you
expect to be reviewing this subsystem's security surface more than once in the next year, which its own
history suggests.

*Branch B.* It does not. Protection becomes `protect/` inside `zeroship-db`, holding the same files in
the same order, with the same one-parser sanitiser it already has. The migration loses its only
non-mechanical commit besides the `WalSink` inversion, the rename can land inside a week, and the family
is seven crates, which answers the count complaint harder. The price is that the audit surface has no
manifest: nothing stops a protection pass from acquiring a `use crate::tx_lanes::` or a
`use crate::broker::` edge, and extracting the crate later means cutting a large crate under security
pressure, which is the worst time to cut anything. Take this branch if what you actually dislike is that
one crate holds too much and you want the smallest possible diff that fixes the names.

I would take A. The reason is not the DB-3 history, which is less open than the panels believe; it is
that `-protection` is the only crate in the family whose manifest refuses something a human would
otherwise have to check by hand every time, and that is the exact test this whole shape is generated by.
