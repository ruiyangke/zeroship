# Work order: every entity id on `declare_entity_id!`

Target: `AppId` declared by the macro, a new `UserId` declared by the macro, and
`AppId::uuid`, `AppId::from_uuid` and `canonical_app_id_for` deleted from the tree.

Read-only measurements below were re-run at `bf147b3b8`. Nothing was built, no PostgreSQL was
touched. Claims are marked CONFIRMED (I ran the command or read the code), PLAUSIBLE (argued from
what I read), or NEEDS-BUILD (needs cargo, pnpm or a live server).

---

## 1. Can the macro carry `AppId`?

**Yes, with nothing bespoke and no opt-in bits accessor.** The reason is not that the printed id is
better at the two call sites the module header names; it is that neither of them is what the header
says it is, and after the column flip there is no `Uuid` in the process for `uuid()` to return. The
AAD is not a caller at all: `app_secret_aad` in `crates/zeroship-control/src/env_store.rs` takes a
bare `Uuid` and calls `as_bytes()` on it, and that file contains zero occurrences of `AppId` or
`app_derivation` (CONFIRMED, `grep -c 'app_derivation\|AppId' crates/zeroship-control/src/env_store.rs`
returns 0). It is also the LOUD one, not the quiet one the header claims: AES-256-GCM tag mismatch
propagates as `EnvError::SecretDecrypt`. The ring is the sole non-test production consumer of
`uuid()` inside the derivation seam, and its contract is that every gateway agrees, not that the
value is any particular number: `HashRing` is an in-memory `BTreeMap` built in
`crates/zeroship-gateway/src/main.rs`, no migration persists a ring position, and the ring already
mixes string-derived vnode hashes with the app hash in one keyspace. So `AppId` collapses to
`declare_entity_id! { AppId, APP_PREFIX, app_id_tests }` and the three ids that have no route to the
bits keep having none. **I side with the critique against all three prior documents on the one
argument they share**: `ring_key` returns `[u8; 16]` and `select_with_affinity` in
`crates/zeroship-gateway/src/proxy.rs` concatenates that fixed-width prefix, a `b':'`, then the
affinity (CONFIRMED by reading both). A fixed-width prefix is injective by width whatever its
alphabet, so there is no crafted affinity that aliases one app onto another and there never was. Do
not write that defect into a commit message or a test; it is not real, and two of Design 1's binding
tests were specified against it and are unwritable.

**Cost if the ruling were wrong and the macro grew an opt-in bits accessor** (it is not wrong, so
this is the argument for refusing the option, not a plan): the macro's flat promise that the type
exposes no route to the bits becomes conditional, so a reader must check each invocation instead of
reading the macro once; `uuid()` returns a `uuid::Uuid`, whose inherent `as_bytes` is exactly what
`AppId`'s `there_is_no_inherent_as_bytes` test exists to keep out of reach, so `InviteId`,
`OrganizationId` and `ProjectId` would each acquire the hazard one method call away instead of
absent; and every future entity id gains a design decision where both answers compile.

---

## 2. The order, and why

**One design: Design 2's shape (a single atomic flip), with Design 1's macro-test prerequisite and
its RLS finding folded in. Design 1's eight-commit staging is rejected.** Staging's whole safety
case is that mixing the two renderings is loud, and that premise is false where it matters. In
`crates/zeroship-gateway/src/enforce.rs` a rendering mismatch does not miss, it auto-vivifies:
`RateLimitRegistry` and `ConcurrencyRegistry` both end their lookup in `entry(...).or_insert_with(...)`,
and both carry a `degraded: RwLock<HashSet<AppId>>` whose only reader is `is_degraded`, a bare
`contains` (CONFIRMED, all four sites read). A disagreement there silently resets a token bucket and
silently turns off the spend-limit Degrade tier: no error, no log, traffic flows. The idempotency
namespace fails the same way, and there a miss means re-executing a mutation the caller asked to run
once. Eight commits of both renderings circulating is eight windows onto exactly that.

**Yes, the column flip comes first, and the owner's instinct to just do it is right.** There is no
useful type-threading that precedes it. Threading `AppId` into a caller does not delete that caller's
`.uuid()`, it relocates the call down to wherever the value meets a uuid column, so `.uuid()` rises
before it falls; and every terminus that is a uuid column cannot be freed by any amount of Rust
work. What Design 1 got right and Design 2 missed is that the flip is bigger than a retype: eight
`tenant_isolation` RLS policies in `db/migrations-ts/20260702000800_policies_rls.ts` compare
`app_id` against `currentSetting("zeroship.tenant_app").cast({ to: "uuid" })` (CONFIRMED, eight
occurrences, all in that one file), PostgreSQL will refuse `ALTER COLUMN ... TYPE` under a dependent
policy expression, so all eight drop and recreate inside the flip. Those are the tenant fences.

What DOES go first is instruments, not staging. Four prerequisite commits, each green on today's
tree, each landable today, none of which puts a second rendering into circulation. They exist
because the flip's review shape is the hardest one there is: the reviewer's job is to spot the
ABSENCE of a change at the `app_id: &str` sites and the UNCHANGED LINE at `app_derivation::encryption_salt`.

I also take the critique's correction of Design 2's headline: it is **four** of thirteen `.uuid()`
sites that vanish by construction when the column becomes text, not twelve. The rest terminate in a
`String` map key, a persisted string keyspace, a process-local `Uuid`-keyed map, or a hash input, and
each needs a deliberate re-keying inside the flip.

---

## 3. The commits

Scopes below are from `CONTRIBUTING.md`'s vocabulary (CONFIRMED by reading it).
"Green after" means the whole tree builds and its gates pass at that commit.

### Prerequisites (P1..P5). All parallel except where noted. All green after.

**P1 - `test(core): raise declare_entity_id to the app id test surface and pair its last probe`**
Changes: move the nine tests `crates/zeroship-core/src/app_id.rs` has and
`crates/zeroship-core/src/entity_id.rs` does not into the macro, so `InviteId`, `OrganizationId` and
`ProjectId` GAIN them. The load-bearing three are `there_is_no_inherent_as_bytes` (with `uuid::Uuid`
as its paired control), `works_as_a_serde_map_key_and_refuses_a_uuid_shaped_one` (the only map-key
position test in the tree, and the only thing that makes a typed `RouteMap` safe on the wire), and
`parse_refuses_the_derived_oauth_client_id` (the `oac_` prefix carries the same 128 bits in the same
encoding; the macro's own foreign-prefix arm substitutes `zzz`, a prefix that exists nowhere). Also
add the missing control: `entity_id.rs` asserts eight `!Probe::<$name>::IMPLEMENTED` and runs seven
`Probe::<String>::IMPLEMENTED` controls - `probe_partial_eq_str` is unpaired (CONFIRMED by reading
the probe block and both assertion blocks), so that assertion is currently unbound in all three
macro types.
Verify: `cargo test -p zeroship-core`. NEEDS-BUILD. Why it must precede the flip: adopting the macro
without it deletes those tests with no compile error, no diff line saying "deleted a test", and no
gate failure, at the exact moment two of them become load-bearing.

**P2 - `fix(metering): key the meter on the app id instead of parsing it back`**
Changes: `Meter` keys on the app id text; `UsageSubject.app` stops being an `Option<Uuid>`; delete
`Uuid::parse_str` in `Meter::drain` AND its evict arm - a subject the meter cannot attribute must
refuse, never silently drop counters `drain_metrics` already zeroed (CONFIRMED: the `Err` arm warns
once and returns `false` from the `retain` closure). Replace that module's test helper, which mints
its app id as `Uuid::new_v4().to_string()` and is the only reason this failure path has never been
seen (CONFIRMED, every assertion in `crates/zeroship-metering/src/meter.rs`'s test module runs
`Uuid::parse_str` on it).
Verify: `cargo test -p zeroship-metering -p zeroship-worker -p zeroship-gateway`. NEEDS-BUILD.
This is the single highest-value pre-flip commit and it depends on nothing.

**P3 - `test(control): prove app_secret_aad is injective over the app and the key name`**
Changes: a property test that no two `(app, key_name)` pairs produce the same AAD - the adversarial
pair that would collide if the `0x00` separator were dropped or a variable-length app id were
admitted without one. True under both renderings, so it can land now. This exists because a golden
vector cannot bind the commit that re-freezes it, and the flip re-bases this derivation.
Verify: `cargo test -p zeroship-control --lib env_store`. NEEDS-BUILD.

**P4 - `test(tests): ratchet the compiler-blind app-id surface`**
Changes: `git mv tests/app_id_frontier_gate.sh tests/app_id_surface_gate.sh` and rewrite it. Arms,
each with its own floor beside the code that produces the number, per `tests/lib/gate_arms.sh`: the
existing `AppId::from_uuid` ratchet; a `canonical_app_id_for` ratchet; an `AppId::uuid()` ratchet
(mechanically filterable because exactly one `fn uuid(` exists in the workspace - CONFIRMED, the
sole hit is `crates/zeroship-core/src/app_id.rs`); and the lead arm, a ratchet on
`app_id: &str|String`, the half rustc cannot see. Rename rather than add, so `GATE_FILE_COUNT=56` in
`tests/gate_arm_census.sh` does not move (CONFIRMED: `ls tests/*_gate.sh | wc -l` is 56 and the pin
is 56). Fix the gate's composition comment while there: it calls
`crates/zeroship-plugin-db/src/replication.rs`'s `from_uuid` a production call site, and it is inside
that file's `#[cfg(test)] mod tests` (CONFIRMED by locating both).
Verify: `./tests/app_id_surface_gate.sh && ./tests/gate_arm_census.sh tests --run tests/app_id_surface_gate.sh`.
SAFE (shell only).

**P5 - `feat(core): mint UserId from the shared macro`**
Changes: `declare_entity_id! { UserId, USER_PREFIX, user_id_tests }` in a new
`crates/zeroship-core/src/user_id.rs`, registered in `lib.rs`; delete `typed_id::new_user_id`, which
has zero production callers (CONFIRMED: every reference is its own definition, its own unit tests,
or doc prose in `crates/zeroship-schema/src/query.rs`). Nothing else compiles differently.
`USER_PREFIX` already exists, and `crates/zeroship-schema/src/query.rs` and
`crates/zeroship-migrate-ir/src/validate.rs` already RESERVE `usr_` against creator collections for
ids the platform does not yet mint. The `zeroship.users.id` column flip is a separate, later change
on the same template as the app flip; do not fold it in.
Verify: `cargo test -p zeroship-core`. NEEDS-BUILD.
Parallelism: P1 and P5 both touch `crates/zeroship-core` (`lib.rs` and possibly `entity_id.rs`).
Either serialise them or accept a one-line conflict. P2, P3, P4 are disjoint from both and from each
other.

**P6 is a measurement, not a commit.** Before writing the flip's migration, on the live 18.4
container: does `uuid` to `text` need `USING` on PostgreSQL; does the retype of a primary key with 26
inbound foreign keys succeed with the referencing columns retyped in the same batch; what is the
exact refusal text for `ALTER COLUMN ... TYPE` under a dependent policy expression; and do mismatched
implicit collations in `=` raise 42P22 or merely disable the index. All NEEDS-BUILD. The last one
matters because the tree asserts twice that it is silent; if it errors, a whole paragraph of
collation risk evaporates.

### The flip (F). One commit. Not parallel with anything. Green after.

**F - `feat(db)!: make zeroship.apps.id a typed app id`**

It cannot be split. Splitting means either a red tree between the DDL and the sweep, or holding
`Uuid` in memory and rendering at every bind through a transitional - which is the "two intermediate
versions" AGENTS.md forbids, load-bearing at roughly thirty sites.

Contents, all together:

- DDL. `zeroship.apps.id` to `text NOT NULL COLLATE "C"`, and the 29 `app_id` copies with it
  (CONFIRMED: `grep -rn --include='*.ts' -P '^\s*app_id\s*:\s*t\.' db/migrations-ts/*.ts` returns 29,
  and `grep -rn 'references: { table: "apps"' db/migrations-ts/*.ts` returns 26). Expressed through
  `raw()` with the `ALTER TABLE ... ALTER COLUMN ... TYPE text COLLATE "C"` pattern already used by
  five migrations, extended with `USING` if P6 says so - `setColumnType` variant `"using"` is marked
  `postgres: "unsupported"` in `packages/zero-migrate/src/generated/dialect-table.ts`. I side with
  the critique against Design 2 here: the "no migration in this tree has ever altered uuid to text"
  observation is right, but the precedent that matters is the raw-SQL island, and appending `USING`
  is a one-token extension of a path already in use.
- The `apps_id_shape` CHECK (`^app_[0-9A-Za-z]{22}$`), matching `organizations_id_shape` and
  `projects_id_shape`, added AFTER the retype. Design 2's own commit 1 proposed it before, and its
  own worse-case refutes that: a regex CHECK cannot be added to a `uuid` column, and every existing
  row would violate it. That commit is deleted from the plan, not moved earlier.
- The eight `tenant_isolation` policies dropped and recreated without the `::uuid` cast;
  `rls::set_tenant_app` in `crates/zeroship-gateway/src/rls.rs` takes an `&AppId` (it already binds
  the GUC as text). Name in the body the consequence nobody else named: the `::uuid` cast is what
  makes a malformed GUC a hard 22P02 today, and a text comparison makes the same malformation a
  silent empty result set. Both fail closed; only one says so.
- All 30 columns registered in `db/migrations-ts/20260831000001_sortable_entity_id_collations.ts`,
  whose map today carries `apps: ["plan_id"]` and not `id` (CONFIRMED) - so a sweep finds the table
  name present and the column absent. Rewrite that file's "deliberately excludes raw UUID domains"
  sentence in the same commit.
- The AAD re-based: `app_secret_aad` takes an `&AppId` and appends `as_str().as_bytes()`, the
  capacity hint moves off 16, and `APP_SECRET_AAD_PREFIX` goes `v1\0` to `v2\0` to record that the
  field changed shape. Failure is loud and bounded: a developer's local secrets, repaired by
  re-setting them.
- `ring_key` returns the printed bytes. In this commit, never earlier: earlier, both renderings are
  still live and one app can occupy two ring positions with no error anywhere.
- The meter producers moved together with P2's re-keyed meter, and
  `app_derivation::meter_key` finally wired - this is the first moment it is correct. It has zero
  production callers today and its one test builds its app through `from_uuid`, whose text IS a
  parseable uuid, so that test cannot see the hazard.
- Both dual-rendering parsers reduced to one shape: `workflow_instance_api::parse_app_id` and
  `authz::authority::app_uuid_or_refuse`. These are the "accept both shapes" end-states AGENTS.md
  forbids and they die here.
- Wire types typed: `RouteMap`, `VersionMap`, `ControlEvent`, `UsageSubject.app`,
  `WorkflowSignalTokenClaims.app_id`. The signal-token field is the strongest independent security
  fix on the list and it could go earlier - but only by adding a fresh `.uuid()` at its consumer, so
  it goes here.
- The idempotency namespace re-keyed.
- `uuid()`, `from_uuid`, `canonical_app_id_for` and
  `app_derivation::lifecycle_lock_seed_for_stored_uuid` deleted; `AppId` becomes the macro
  invocation; the three hand-written `impl` blocks in `crates/zeroship-core/src/app_id.rs` go with it
  (CONFIRMED: exactly three exist tree-wide outside `zeroship-cdc-wire`, all in that file).
- `tests/app_id_surface_gate.sh` loses its three transitional arms and keeps the `&str` arm.

Commit body must carry, and this is a review instrument rather than prose: one sentence each for the
four derivations whose value changes with no diff line - `encryption_salt`, `publication_name`,
`meter_key`, `ring_key` - so the reviewer is told where to look for nothing; a pointer to P1 naming
which of its tests binds which of this commit's changes, so the reviewer does not read them as
settled pre-existing infrastructure; and the statement that a revert requires recreating the
database.

Verify, in this order: `deploy/ops/db-migrate.sh` against a fresh database, then
`tests/platform_migration_corpus_gate.sh`, `tests/rls_binding_gate.sh`,
`cargo build --workspace --all-targets`, `./tests/clippy_gate.sh`, `tests/run_auth_suite.sh`, the
plugin-db live suite, and `./tests/e2e_platform.sh`. All NEEDS-BUILD.

**Process commitment, not optional.** The live suites cannot be parallelised on this machine (the
auth suite drops a fixed-name database; concurrent suites share one PostgreSQL), and main takes
roughly a hundred commits a day. So the realistic failure of this plan is: verify at H, rebase onto
H+100, merge on the strength of the pre-rebase run because re-running costs the hours the rebase was
meant to save. The suites are re-run AFTER the final rebase, and the run's HEAD is recorded in the
PR. A verification whose HEAD is not written down is indistinguishable from one that never ran.

---

## 4. What will not be a compile error

Ranked quietest first. The wire pairs are the new item, and the point about them is not typing: the
macro's `Deserialize` routes `visit_str` through `parse`, so a field that moves from `Uuid` to
`AppId` starts REFUSING a uuid-shaped value at decode. `AppId` already has that impl hand-written
(CONFIRMED), so adopting the macro is a wire no-op for `AppId`-typed fields - but there are almost
none today, and the flip creates them. Each pair below must flip on both sides in this commit.

1. **`app_derivation::encryption_salt`.** Already `app.as_str().as_bytes()`, so its VALUE changes
   with no code edit, no diff line and no compile error. Its own doc says the `k_siv` half makes an
   equality search over an encrypted column return fewer rows and no error. Quietest thing in the
   change. Pre-launch answer is to drop and recreate every encrypted column, which is a per-app-schema
   data-plane action no platform migration performs.
2. **The `app_id: &str` half of the data plane.** rustc cannot see it, and it is the majority of the
   surface. `app_derivation::schema_name` returns `app_<base62>` while `database_role::per_app_role_name`
   and `replication_names::publication_name` compose from whatever string the data plane holds, fed
   by the worker's `APP_ID` injection. Divergence puts an app's tables in a schema nobody queries.
   The hazard is real; its SIZE is two producers that must agree, not one per call site - I side with
   the critique against Design 2's framing here.
3. **`enforce.rs` auto-vivification and the degraded sets.** `entry(...).or_insert_with(...)` means a
   rendering mismatch resets a token bucket rather than missing; `is_degraded`'s bare `contains`
   means the Degrade tier stops throttling. Fails OPEN, silently. Contained only because the flip is
   one commit.
4. **The idempotency namespace.** A key-space miss is not an absence, it is re-execution of a
   mutation the caller asked to be executed once.
5. **`Meter::drain` parse-and-evict.** An app serves traffic, accrues counters, and is never billed;
   the warning does not repeat because the entry is gone. Worse than not billing, if only some
   producers move: the same app's counters land under two keys, one parseable and one evicted, and a
   partial invoice looks like a correct one. P2 removes this before any producer moves.
6. **`publication_name`.** A rename leaks cluster-wide objects nothing reclaims and nothing notices;
   a leaked slot also blocks `DROP DATABASE`, so "recreate your dev database" is not always available.
7. **WIRE PAIR - control to gateway `GET /internal/routes`.** `RouteMap` and `VersionMap` are JSON
   map keys. Worst blast radius on the list: `sync_once` returns `Err` on a snapshot decode failure
   and the caller keeps the previous table behind a log line, so one bad key freezes routing at the
   last good snapshot and new deploys never appear. Today one bad key drops one entry. The binding
   test is that a snapshot with one malformed key is refused in a way a caller can distinguish from a
   successful poll, not only logged.
8. **WIRE PAIR - worker to control `GET /internal/apps/{id}/env`.** `internal.rs` accepts the uuid
   only; it is the one boundary where the worker sends a uuid it got from `.uuid()`. Both sides in
   this commit or every env fetch 400s.
9. **WIRE PAIR - worker to stream to control ingest, `UsageSubject.app`.** A present wrong-shaped
   value is a loud decode error; an ABSENT one is silently `None` because the field carries
   `serde(default)`. That asymmetry is why P2 lands first.
10. **WIRE PAIR - control to gateway `ControlEvent::{Deploy,Delete,PlanChange,SpendState}`.** Loud
    decode failure, low blast radius.
11. **The RLS `::uuid` cast.** Removing it turns a malformed GUC from 22P02 into an empty result set.
    Both fail closed; the loud one is being traded away, so say so in the body.
12. **SQL literals and fixtures.** Uuid-shaped string literals across Rust, `tests/`, `sdks/` and
    `deploy/`. Loudest class once the `apps_id_shape` CHECK exists, and therefore the class most
    likely to make the flip LOOK better-instrumented than it is. The CHECK fires only on writes to
    `zeroship.apps`; it covers none of items 1 through 6.

---

## 5. The completeness proof

**Sufficient combination: two structural checks, plus one ratchet arm that survives.**

```
# (a) AppId is declared by the macro and carries no hand-written impl
grep -rn --include='*.rs' 'declare_entity_id! *{' crates --exclude-dir=.worktrees      # expect 5
grep -rn --include='*.rs' -P '^\s*impl(<[^>]*>)?\s+.*\bAppId\b' crates \
  --exclude-dir=.worktrees | grep -v cdc-wire                                          # expect empty

# (b) no free function reconstitutes the bits
grep -rn --include='*.rs' -P '\bfn\s+uuid\s*\(|from_uuid|canonical_app_id_for' \
  crates libs --exclude-dir=.worktrees                                                 # expect empty
```

Today (a) returns 3 and three impl blocks, and (b) returns hits (CONFIRMED, all four commands run).
`declare_entity_id!` returning 5 accounts for `AppId`, `UserId`, `InviteId`, `OrganizationId` and
`ProjectId`; `zeroship-cdc-wire` has its own typed ids from its own macro and is excluded on purpose.

Why this combination and not the candidates individually. "`uuid()` absent" alone is not sufficient:
a hand-written `AppId` with the accessor removed passes it, and that is not the goal. "`AppId`
declared by the macro" is nearly sufficient on its own - a macro-generated type structurally cannot
carry a second field, `uuid()` or `from_uuid` - and what it does NOT rule out is a free function
elsewhere rebuilding the bits, which is exactly what `canonical_app_id_for` was. So (a) proves the
type and (b) proves nothing outside it re-opens the seam. **Deleting the frontier gate is a
consequence, not evidence** - it can be deleted while a caller still exists, so it proves nothing.

**What replaces it:** P4 already renamed the file to `tests/app_id_surface_gate.sh` at pin 56. After
the flip it keeps one arm, and it is the arm nothing else covers: the ratchet on `app_id: &str|String`,
the compiler-blind half, counting down. The three transitional arms are deleted with the transitionals.
Add one structural arm asserting check (a) - zero hand-written `impl` blocks for a macro-declared id
outside `entity_id.rs` - so a future entity id cannot be introduced hand-written and then quietly
grow an accessor. That arm is permanent; the ratchet arm retires when its floor reaches zero.

---

## 6. What the owner has to decide

1. **`APP_ID` becomes `app_<base62>` for creator code.** It is a deploy-contract surface read by app
   code, injected by `crates/zeroship-worker/src/cache.rs`. Pre-launch it may change, but it must
   change as a decision, not as a side effect of a `HashMap` key type. Recommend: yes, change it, and
   say so in `docs/reference/zeroship-standard.md` in the same commit.
2. **Local dev data is dropped, not migrated.** App secrets stop decrypting (loud, `EnvError::SecretDecrypt`)
   and encrypted-column equality search silently returns fewer rows (quiet), so the answer is drop
   and recreate every encrypted column and re-set secrets. Recommend: accept, and announce it in the
   commit body since other sessions share this machine.
3. **`zeroship.users.id` stays a uuid for now.** P5 gives `UserId` the type; the column flip is 43
   foreign keys across four migration files and belongs in its own change. Recommend: split. If you
   want them together, the flip commit roughly doubles and the review shape gets worse in the
   direction section 4 says is already hardest.
4. **`WorkflowSignalTokenClaims.app_id` timing.** It is the strongest independent security fix here
   and it touches no database, but landing it early costs one new `.uuid()` at its consumer.
   Recommend: keep it in the flip. Override if you want the fix sooner than the flip.
5. **P6 is a gate on writing F's migration, not on planning it.** If the live 18.4 probe says the
   retype of a primary key with 26 inbound foreign keys cannot be done in one batch, the DDL becomes
   a multi-statement `raw()` island and the plan is unchanged; if it says something worse, bring it
   back before F is written.
