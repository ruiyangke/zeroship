# Round 7 (Fable): incarnation propagation surface + adversarial arm hunt

Repo READ-ONLY. Every file:line below was opened and read this round unless
marked "(proposed)". Note of record: **nothing in `crates/` today writes,
reads, or knows an `AppIncarnationId`, an `app_schema_state` row, or a
`__zeroship_state` row** - `grep -rn "app_schema_state|__zeroship_state|
incarnation" crates/zeroship-data-v8/src` returns zero hits. The entire
surface below is greenfield; there is no existing field to compare against, so
"omitting it" always means "the site keeps keying on the tuple it keys on
today," which is what makes each hole concrete.

---

## PART 1 - The exhaustive incarnation propagation surface

Fork C: a durable 128-bit `AppIncarnationId`, minted by the privileged
server-side function, persisted beside `state`/`epoch`, qualified by
`(system_identifier, timeline_id)`, compared **before any data SQL**, epoch
mismatch => re-resolve, incarnation mismatch => deny terminally.

Below, each site: what it keys on today, the concrete change, and whether
omitting it is **EXPLOITABLE** (cross-incarnation data or action reachable) or
**UNTIDY** (leak/confusion, no cross-tenant reach).

### A. The authority row and the privileged minting function (proposed - the choke point)

- **Today:** does not exist. `publish_schema_state` (parent :640-646) is the
  proposed writer; the row is `__zeroship_admin.app_schema_state(app_id, state,
  epoch, changed_at)` (parent :554). Neither carries an incarnation column.
- **Change:** add `incarnation` (128-bit) to the row; the minting function
  mints it (never a caller argument, same rule as `epoch`, parent :648-651) on
  **first provision only** - the `INSERT ... ON CONFLICT DO NOTHING` arm
  (parent :667-669) mints incarnation; the `UPDATE` CAS arm (parent :652)
  **preserves** it. A migration/redeploy must NOT re-mint (that would revoke
  every healthy handle - the epoch's job, SC-5 :114-121). Deprovision leaves
  the tombstone row with its incarnation; recreation is a fresh
  first-provision that mints a NEW incarnation (SC-5 :112). Qualify the value
  the worker caches by `(system_identifier, timeline_id)` exactly as the epoch
  cache key already is (parent :687-698), read via `pg_control_system()` +
  `pg_control_checkpoint()`.
- **Omission verdict: EXPLOITABLE, and this is the WORST single omission**
  (argued at the end). If the row/minting does not carry incarnation, every
  downstream compare has nothing to compare against and silently degrades to
  the epoch, which re-resolves permissively.

### B. The prepare / authority-read compare (the data-plane gate)

- **Today:** the parent's authority batch reads `state`/`epoch` before
  `SET LOCAL ROLE` (parent :828-834). Resolution keys the live cache by
  `(app_id, epoch)` (parent :830, :850-852).
- **Change:** the same batch returns `incarnation`; `prepare` compares the
  binding's incarnation against the row's **before any data SQL**. Mismatch =>
  terminal deny (a distinct error from epoch-mismatch-re-resolve). This is the
  single point SC-5 :106,:160-162 relies on ("compared before any data SQL",
  "denied terminally at `prepare` on incarnation mismatch").
- **Omission verdict: EXPLOITABLE.** This IS the fence. Epoch mismatch =>
  re-resolve (parent permits a binding to observe a new epoch and resolve
  again, :354-358; restore/recreate publishes a fresh epoch, :868). So a
  handle cloned before deprovision, against an app recreated with a
  descriptor-compatible schema, reads the fresh epoch, re-resolves, and reaches
  the **new** app's rows. Using the epoch as the handle fence has exactly the
  two wrong outcomes SC-5 :119-121 names.

### C. `AppVersionInfo` / the `/internal/versions` projection

- **Today:** `crates/zeroship-core/src/types.rs:126-151`: `AppVersionInfo`
  carries `deploy_hash, plan_id, runtime, env_version, manifest, net_policy` -
  **no incarnation**. It is the sole channel by which a worker learns anything
  changed (`sync.rs:206-211` polls `/internal/versions`).
- **Change:** add `incarnation` to `AppVersionInfo` (and the control-plane
  projection that fills it). Without a transport field, a worker cannot even
  observe that an app id was recreated - the version map is keyed by `Uuid`
  (`sync.rs:143-152`) and a same-id recreate looks like the same entry.
- **Omission verdict: EXPLOITABLE (enabling).** This is not the fence itself,
  but it is the wire that carries the current incarnation to the worker so the
  reload path (D) can fire. Omit it and B is the only defense - which holds for
  the data plane, but every cache/handle in C-H stays stale because nothing
  tells the worker to tear down. It converts an exploitable set into a merely
  data-plane-safe-but-badly-confused one; still worst-case exploitable in
  combination with any B gap.

### D. Worker `needs_reload` / `LoadedMeta`

- **Today:** `crates/zeroship-worker/src/cache.rs:664-674` `LoadedMeta {
  deploy_hash, env_version, net_policy }`, stored in a `HashMap<Uuid,
  LoadedMeta>` (`cache.rs:677`). `sync.rs:284-297` `needs_reload` compares
  those three axes only. A same-id recreate with the same deploy_hash/env/net
  returns `needs_reload == false` - the isolate is never swapped.
- **Change:** add `incarnation` to `LoadedMeta` and a fourth arm to
  `needs_reload` (`incarnation_changed`). Feed it from `AppVersionInfo`
  (item C).
- **Omission verdict: EXPLOITABLE.** A stale isolate (with a binding carrying
  the old incarnation) keeps serving. Its data-plane ops are still caught by B
  at `prepare`, so DATA is safe if B is correct - but the isolate never
  reloads, so it never re-resolves to the new incarnation and every op denies
  terminally forever. Without B it is a straight cross-incarnation read. Rank:
  EXPLOITABLE if B is absent, otherwise a hard availability break.

### E. Worker delayed-deprovision pending set (bare UUIDs)

- **Today:** `crates/zeroship-worker/src/sync.rs:135` `pending_cdc_deprovision:
  HashSet<Uuid>`; extended with bare `app_id` copies (`:146-151`); drained by
  `deprovision_app_cdc(db_url, &app_id.to_string())` (`:158-161`). No
  incarnation. SC-5 :169-174 names this explicitly as a longer-lived handle.
- **Change:** store `(app_id, incarnation)` in the pending set and pass the
  incarnation into the teardown so it only tears down CDC state for the
  incarnation that disappeared.
- **Omission verdict: EXPLOITABLE (destructive-action).** Sequence: app A
  (incarnation-1) deleted => queued; retry fails a few polls; meanwhile A is
  recreated (incarnation-2); the delayed teardown then runs
  `deprovision_app_cdc` and **drops incarnation-2's live replication slots**
  (see item G) - a cleanup carrying incarnation-1 acting on incarnation-2, the
  exact "must not act on incarnation B" hazard SC-5 :174. Silent CDC outage on
  a live app, not cross-tenant read, but adversarially triggerable.

### F. Durable workflow runs and pinned-isolate keys

- **Today, runs:** `crates/zeroship-control/src/workflow_instance_api.rs:
  1106-1124` inserts runs keyed by `(id, app_id, deploy_id, ...)` - a
  `deploy_id` (:1081), **no incarnation**. Journal schemas deliberately survive
  app deletion (`cron/workflow_engine.rs:302-312`: `delete_app` drops
  `zeroship` rows and plugin-db drops the DATA schema, but the `app_<uuid>`
  journal is owned by `zeroship_workflow_owner` and is NOT dropped).
- **Today, pinned isolates:** `crates/zeroship-worker/src/cache.rs:29-53`
  `PinnedWorkflowKey { app_id, deploy_hash }`; loaded via
  `load_pinned_workflow_app(app_id, deploy_hash, ...)` (`cache.rs:485-503`);
  looked up by `get_workflow_runtime(&app_id, &claim.deploy_hash)`
  (`handler.rs:639,651`). No incarnation anywhere.
- **Change:** the run row gains an `incarnation` column, written at
  `insert_run`; the claim/replay path carries it; `PinnedWorkflowKey` gains
  `incarnation` so a replay isolate for incarnation-1's journal cannot be
  reused for incarnation-2, and the binding it builds carries incarnation-1 so
  B denies its data ops against incarnation-2's schema.
- **Omission verdict: EXPLOITABLE.** The journal survives deletion by design,
  so a recreated same-id app whose worker replays an OLD run rehydrates a
  pinned isolate whose binding, absent incarnation, resolves against the new
  epoch and reaches the new app's data (parent :951-953 already flags that
  deploy-pinned boot re-runs against current state). This is the second
  long-lived handle SC-5 :169-174 calls out and the sharper of the two.

### G. CDC slot naming/ownership and the drop prefix

- **Today:** `crates/zeroship-data-v8/src/replication.rs:108-116`
  `worker_slot_name = "{OBJECT_PREFIX}slot_{stable_token(app_id,14)}__
  {stable_token(worker_id,10)}"` (`OBJECT_PREFIX = "__zs_"`,
  `zeroship-core/src/replication_names.rs:6`); prefix for bulk drop is
  `worker_slot_name_prefix(app_id)` (`:121-127`); `drop_worker_slots` matches
  `left(slot_name, length($1)) = $1` (`:590-608`). Keyed by app_id token only.
- **Change:** fold the incarnation into `stable_token`'s input (or add an
  incarnation segment) so incarnation-1's leftover slots and incarnation-2's
  new slots occupy disjoint namespaces, and the drop prefix targets one
  incarnation.
- **Omission verdict: mostly UNTIDY, edge EXPLOITABLE.** A logical slot has one
  active consumer; a recreated same-id app whose new worker computes the SAME
  slot name as an un-dropped old slot collides - the new consumer fails to
  attach (CDC silently stops, the parent's own "subscriptions silently stop
  forever" hazard, :868). Not a cross-tenant read, but combined with item E's
  delayed drop it is an adversarially reachable CDC-denial. Rank UNTIDY unless
  paired with E.

### H. The live cache key

- **Today (proposed):** parent :830, :850-852 keys the process-wide
  live-metadata cache by `(app_id, epoch)`; eviction removes the entry
  regardless of `Arc` holders (:852-853, :358).
- **Change:** key by `(app_id, incarnation, epoch)` - OR rely on B to deny
  before a cache lookup ever matters. Because `epoch` is 128-bit random and a
  recreated app mints a fresh epoch, a stale `(app_id, epoch)` entry can never
  be hit by the new incarnation anyway (parent :877-879 restore argument). So
  the cache key is the ONE place the epoch's entropy genuinely subsumes the
  incarnation.
- **Omission verdict: UNTIDY.** No cross-incarnation reach: the fresh epoch
  guarantees a miss. Adding incarnation to the key is defense-in-depth, not
  load-bearing. This is the item Round 6 could safely have left off; the epoch
  already does its job here.

### I. The SQLite `__zeroship_state` row (proposed, SC-2 :137-148)

- **Today (proposed):** SC-2 places `__zeroship_state(state, epoch,
  changed_at)` in the app file, written by the dev migration path. No
  incarnation.
- **Change:** add `incarnation` to `__zeroship_state`; the reservation's first
  statement reads it alongside the epoch (SC-2 :142). But note SC-2 :144-148 is
  explicit that the dev tier's protection is "reachable only through this
  actor," not grants - and SC-6 :174-180 concedes dev gives contract parity,
  not the adversarial posture. On the dev tier there is exactly one worker,
  SQLite DSNs are refused on the real worker (`main.rs:105-116`,
  SC-1 :181-182), and same-id recreation is a `rm` of a file. So the SQLite
  incarnation is **contract-parity plumbing, not a security boundary.**
- **Omission verdict: UNTIDY.** Add it for parity so the same `prepare` compare
  code runs on both tiers; omitting it cannot produce a cross-tenant read on a
  single-tenant developer machine.

### Sites Round 6 did NOT name (found this round)

1. **`AppVersionInfo` is the only transport** (item C). Round 6's brief lists
   it, but the point worth sharpening: without C there is no *channel* at all
   for a worker to discover recreation - the version map is `Uuid`-keyed
   (`sync.rs:143-152`) and a recreate is invisible. C is a prerequisite for D,
   not a parallel site.
2. **`SharedEnvs` GC + the version map itself are `Uuid`-keyed**
   (`sync.rs:188-193`, `:143-152`). A same-id recreate aliases the old env
   snapshot onto the new app until an `env_version` bump. Add incarnation to
   the reconcile identity or GC on incarnation change. Verdict: EXPLOITABLE
   (stale secret served to the new app) - **this one is genuinely unnamed
   anywhere in the seven docs.**
3. **The workflow `deploy_id` column** (`workflow_instance_api.rs:1081,1108`)
   is the concrete storage that must gain the incarnation; Round 6 named
   "durable workflow runs" abstractly but not the insert site.
4. **`broker` subscriptions and `drop_app` are `app_id`-string-keyed**
   (`broker.rs:844,896`; `deprovision_app_cdc` -> `broker::drop_app(Some(app_id))`
   at `lib.rs:909`). A subscription opened under incarnation-1 is torn down by
   an incarnation-agnostic `drop_app`. Verdict: UNTIDY (the fresh epoch on
   ChangeEvents forces resync anyway, parent :877-882), but worth an incarnation
   check so a recreate's `drop_app` cannot tear down the wrong generation.
5. **`ChangeEvent` carries neither epoch nor incarnation today**
   (`broker.rs:81`, verified: zero `epoch`/`incarnation` occurrences). The
   parent stamps epoch at produce (:503-507). Question nobody resolved: does the
   event also need incarnation? Answer: **no** - a recreate mints a fresh epoch,
   so the epoch stamp already forces the pre-recreate subscription to resync
   (parent :877-882). Recording this so an implementer does not add a redundant
   field.

### Which single omission is WORST

**Omitting the incarnation compare at the `prepare`/authority-read gate (item
B), equivalently letting the epoch double as the handle fence.** It is the
single choke point every long-lived handle (D worker isolates, E delayed
deprovision, F workflow replay) funnels through before touching data. If B is
correct, A/C/D/E/F/G degrade at worst to availability faults
(deny-forever, CDC stops); the data plane stays tenant-safe. If B is absent,
the fence collapses to the epoch - and the epoch is *designed to re-resolve*,
not deny (SC-5 :114-121). A handle cloned before deprovision then follows the
recreated app into its data exactly as it does today, which is the precise
cross-incarnation hole SC-5 exists to close. Item A (the minting/row) is its
strict prerequisite - if A is wrong B has nothing to read - so the pair (A
mints/persists, B compares terminally) is the load-bearing core; B is where the
security decision actually happens, so B is the worst to omit.

---

## PART 2 - Adversarial arm hunt (all seven documents)

Two classes: **CANNOT PASS on any implementation** and **CANNOT FAIL on today's
code** (vacuous - proves nothing about the change). I list new findings first,
then confirm the authors' own flags so the count is auditable.

### NEW FINDINGS

#### N1 (CANNOT PASS as written) - the parent's headline "Policy" arm is the unqualified version SC-6 proves impossible

- **Arm:** parent :1172-1173 - "Lowering the operator ceiling denies the next
  `unmask` in an already-built pinned isolate, with no rebuild and no deploy."
  **Unqualified by actor.**
- **Decider:** `crates/zeroship-data-v8/src/crud/mask_policy.rs:101-110` -
  `MaskPolicy::allows` returns `role == "auto"` for any role absent from the
  map; `sanitize_app_actor` (SC-6 :244-245) strips app-supplied `auto`, but the
  platform's own `auto` actor unmasks everything a policy does not explicitly
  list. SC-6 :262-270 states outright that an unqualified "denies the next
  unmask" "states something the design deliberately does not do, and an
  implementer taking the checklist literally would either fail a correct
  implementation or 'fix' the exemption and break the platform's own actor."
- **Why it matters:** the parent is the gate document; SC-6 corrected its OWN
  arm to "by a creator actor" but the parent's top-level acceptance criterion -
  the one an implementer reads first - still carries the impossible unqualified
  form. For the `auto` actor this arm CANNOT PASS on a correct implementation.
  Fix: qualify parent :1172 to "by a creator actor," matching SC-6 :257.

#### N2 (CANNOT FAIL on today's code) - SC-1's "second same-app begin waits and then succeeds, both writes durable"

- **Arm:** SC-1 :162-163 and :196 - "a second same-app top-level `begin`
  **waits** and then succeeds, with both transactions' writes durable."
- **Decider:** the serialization already exists and is deliberate -
  `transaction/mod.rs:354-361` (the quoted comment) plus `tx_claims`
  (`context.rs:182`, "held until the matching COMMIT/ROLLBACK has settled").
  On today's code both transactions already run second-and-succeed with both
  writes durable.
- **Why it matters:** the durability half passes on today's code AND on a
  broken implementation that keys admission by `tx_id` (which would remove the
  *waiting* but both begins would still succeed and both writes still land -
  just concurrently). The only discriminating observable is the *mutual
  exclusion* ("waits"), which SC-1's own Round-3 argument (:19-24) says a
  black-box suite cannot see. So as written - "waits and then succeeds, both
  durable" - the arm cannot fail on either today's code or the regression it
  is meant to catch, unless the test asserts observable serialization (e.g. a
  detectable overlap window or a shared-slot contention probe), which the arm
  does not require. SC-1 diagnoses the underdetermination in prose but leaves
  the acceptance arm in the vacuous form. Fix: require the arm to assert the
  *ordering/exclusion* directly, not just "both durable."

#### N3 (CANNOT FAIL on today's code) - SC-6's autocommit "no extra round trip" arm is already satisfied by the thing being replaced

- **Arm:** SC-6 :295-297 - "The ceiling read adds **no** server round trip to a
  warm autocommit operation, asserted by the same counting transport."
- **Decider:** today the ceiling is a thread-local read -
  `check_unmask_authorization` calls `crate::context::with(|c|
  c.mask_policy_for(app_id))` (`crud/unmask.rs:314-315`), zero round trips.
- **Why it matters:** the arm passes on today's code (0 extra round trips)
  because today's mechanism is in-memory; it also passes on the intended
  implementation (rides the prepare batch, 0 extra). It therefore cannot
  distinguish "correctly folded into prepare" from "still a thread-local that
  was never moved to `__zeroship_admin` at all." It can only FAIL on a
  strawman that adds a gratuitous round trip. This is the SC-5-class trap
  (an arm that passes before the work is done); SC-6 does not flag it. Fix:
  the arm must be paired with a positive control proving the value actually
  came from the `__zeroship_admin` batch (e.g. lowering the ceiling via the
  table is observed), or it certifies the un-migrated code.

#### N4 (CANNOT FAIL on today's code) - SC-3 ledger "reports a non-zero ruled-on count"

- **Arm:** SC-3 :154-155 - "the gate arm reports a non-zero ruled-on count."
- **Decider:** any non-empty crate satisfies "non-zero." `query.rs` exposes 86
  `pub`/`pub(crate)` items (SC-3 :26).
- **Why it matters:** "non-zero" cannot fail on any tree that has functions -
  it is the degenerate floor. SC-3 correctly rejects the hard-coded "must equal
  86" census (:123-131) and correctly requires the two-set equality (ledger
  source column == actual exports), which IS a real check. But the *stated
  acceptance arm* is only "non-zero," which is vacuous; the load-bearing check
  (set equality) is described in the mechanism section but not restated as the
  acceptance criterion. An implementer measured against ":154 non-zero" ships a
  ledger with one row. Fix: the acceptance arm must be the set-equality, not
  "non-zero."

#### N5 (verify - CANNOT PASS on any implementation, cross-doc) - the parent's ChangeEvent produce-time stamp on the suppressed path

- **Arm/claim:** parent :503-507 asserts "the epoch is stamped onto the event at
  produce time ... the producer is the mutation's own operation - which holds
  the lease and already knows the epoch," and the Delivery acceptance arm
  (:1166-1170) relies on a real WAL event carrying the epoch.
- **Decider:** the parent ITSELF refutes the premise at :518-538: in production
  the mutation-side producer is suppressed (`exec.rs:455-466`,
  `is_app_suppressed`), and the real producer is
  `wal_consumer::emit_for_tuple` (`wal_consumer.rs:589`), in which - verified
  this round - the string `epoch` appears **zero** times
  (`grep -c epoch wal_consumer.rs` => 0). So an acceptance arm that "reads a
  real WAL event" and expects a stamped epoch **cannot pass** on any
  implementation that stamps only at the suppressed producer, and the parent
  explicitly leaves the WAL carrier "open" (:534-538).
- **Why it matters:** this is not merely an open design point; it means the
  Delivery acceptance arm (:1166-1170, "reads a real WAL event") is currently
  **unimplementable as specified** until the carrier is chosen - it CANNOT PASS
  on the design as written. The parent flags the gap in prose but still lists
  the arm as an acceptance criterion. This is the highest-severity of the new
  findings after N1 because it is a security arm (mask-only plaintext in CDC)
  resting on an unbuilt mechanism.

### CONFIRMED author-flagged arms (already found; listed for completeness/audit)

- SC-1 :167-176 - "two same-app txns in different isolates do not contend" is
  PostgreSQL-ONLY; backend-neutral form CANNOT PASS on SQLite (one `tx_conn`
  per app, SC-2 :55-61). Flagged, resolved. Verified `take_tx_client_for(app_id)`
  keyed by app_id (`transaction/mod.rs:1029`, `context.rs:801`).
- SC-2 :196-204 - "reservation observed as a *changed* epoch" CANNOT PASS
  (deferred WAL snapshot is by-design invisible); replaced with
  coherently-old-or-new + `SQLITE_BUSY_SNAPSHOT`. Flagged.
- SC-2 :175-189 / parent :1206-1210 - "dropping a caller future cancels and
  rolls back unconditionally" CANNOT PASS (actor may commit before poll);
  replaced with the CAS race (`AlreadyCompleted`). Flagged. Confirmed against
  `session.rs:155-161`'s own "the SQL has already committed by then."
- SC-4 :104-118,:153-167 - "`__zeroshipNodeBuiltin` absent from the production
  constructor" CANNOT catch the real bypass (it carries no capability); the SSRF
  `if dev_mode_enabled() { return Ok(()); }` (`ssrf.rs:206-207`) is the live
  hole. Replaced with the `ZEROSHIP_DEV=1`-present arm. Flagged; verified
  `ssrf.rs:206-208` and `worker/main.rs:112-113`.
- SC-5 :128-137,:175-179 - the `ISOLATE_CTX`-is-per-thread trap: arms asserting
  two isolates share one slot/context CANNOT FAIL on today's code (thread-local
  already shares). Flagged with "must be shown to fail when the singleflight is
  removed." Verified `context.rs:888-893`.
- SC-6 :99-111,:275-286 - the deny-only in-transaction arm CANNOT FAIL on a
  totally broken implementation (per-app-role read => `permission denied` =>
  "failure is denial" => vacuous pass); paired with a granted-path control.
  Flagged; verified `transaction/mod.rs:202-217,:540` issues `SET LOCAL ROLE`
  for the transaction's life.
- SC-6 :211-253 - "the ceiling can only ever narrow" is FALSE (the `auto`
  fallback), self-corrected. Verified `mask_policy.rs:101-110`.

---

## Argue against my own most consequential choice

My most consequential Part-1 claim is that **item B (the prepare-gate compare)
is the worst omission and item H (cache key) is merely untidy.** The
counter-argument: if a future change ever makes the live cache lookup happen
*before* the incarnation compare - e.g. a "speculative construction against the
cached epoch's resolved metadata" which the parent explicitly permits
(:451-454) - then a stale `(app_id, epoch)` entry could be consulted before B
runs. Under normal operation the 128-bit random epoch guarantees a miss
(recreate mints a fresh epoch), so H stays untidy; but the parent's speculative
path is exactly the kind of reordering that could let H become load-bearing
without anyone re-deciding. **What would make me switch:** if the implementation
resolves metadata speculatively against a cache entry and only confirms the
epoch (not the incarnation) afterward, then H must key on incarnation and my
"untidy" verdict is wrong. The safe rule is: incarnation is compared in the
same read that gates data SQL AND is part of any cache key a speculative path
can hit - belt and suspenders - because the cost is one column and the failure
is cross-tenant.

For Part 2, my most consequential pick is **N1** (the parent's unqualified
Policy arm cannot pass). Argue against: one could say this is "already found" -
SC-6 corrected it. But SC-6 corrected *its own* copy; the parent's headline
criterion (:1172) is the one a gate/implementer treats as authoritative and it
still reads unqualified, so the contradiction is live in the governing document.
What would make me drop N1: if the parent is edited to defer all mask-policy
acceptance to SC-6 rather than restating a criterion - then there is no
standalone impossible arm, only a pointer.
