# Flipping `zeroship.apps.id` to a typed id and deleting `AppId::from_uuid`

Execution plan. Every claim below is marked CONFIRMED (I read it in the tree this session),
PLAUSIBLE (read by a survey, not re-read by me), or NEEDS-BUILD (nothing here was compiled,
tested or linted). Paths and symbol names only; no line numbers.

---

## 1. The size, honestly, in three lines

- **The compiler-visited half is bounded and already measured**: 315 `app_id: &?Uuid` signature
  sites across 12 crates (CONFIRMED, reproduced: `grep -rhP 'app_id\s*:\s*&?\s*(uuid::)?Uuid\b'
  crates/*/src libs/*/src --include='*.rs' | wc -l`), plus one migration pair and one unsplittable
  boundary commit. That half is mechanical, sliceable, and its failure mode is a red build.
- **The half that decides the schedule is an open-ended search**, not a count: cross-service string
  equalities where rustc visits exactly one side. It has grown twice under review already - "two
  permissive parsers" became three (CONFIRMED: `parse_app_id` in
  `crates/zeroship-control/src/workflow_instance_api.rs`), "one AEAD AAD site" became two
  (CONFIRMED: `signal_key_aad`, same file). Nobody has an instrument that enumerates this class.
  Budget for the search terminating by *gate frontier*, not by a checklist emptying.
- **There is no data cost.** Pre-launch, no deployed database holds app rows; the flip is a drop
  and recreate, so re-encode vs re-mint is a distinction with no migration behind it
  (PLAUSIBLE, from `db/migrations-ts/20260906000100_apps_organization_and_billing_subject.ts`'s
  own note). The whole cost is enumeration and instruments.

Shape: **a day of typing, several days of proving, and the proving is the part that can run long.**

---

## 2. The sequence

**Which design this is.** Design 1, with the critique's three amendments, and Design 2's good ideas
grafted where they are better. From Design 2 I take: the framing that the unsplittable unit is *the
set of sites where a wrong edit is invisible* (not "315 sites"); the metering fix as a hard
prerequisite rather than a follow-up; the negative `information_schema` arm; the deprovision arm;
and the ruling that the 386-site `&str` tier must take a *derived name*, never `AppId`. I refuse
Design 2's central recommendation to drop the pinned-carrier arm - that arm is the only proposed
instrument that shows a conversion landing on one side of a two-sided equality, which is the entire
silent class. I refuse Design 2's single `SchemaName` newtype for all four `&str` derivations: role
name, KV scope, storage prefix and HKDF salt are four different things.

**Where the critique contradicted a survey, I sided with the critique** on: the third permissive
parser (CONFIRMED); `signal_key_aad` as a second AAD site (CONFIRMED); `ControlEvent` being a
`tracing::info!` line and not a wire, so converting it is free and calling it "the wire" inflates
the unsplittable commit (CONFIRMED - the only production constructor is `SpendState` in
`crates/zeroship-control/src/cron/spend_reconcile.rs`; `Deploy`/`Delete`/`PlanChange` appear only in
`crates/zeroship-core/tests/types_test.rs`); and the collation failure mode being *loud* for `=`
(collation `none`) and *silent* for `ORDER BY` and index use (NEEDS-BUILD - reasoned from the
collation-derivation rules, not induced).

**Where I sided with a survey against the critique**: Survey B's reversal to **re-mint with
`AppId::mint()`**, not re-encode the stored uuid. Base62 of a v4 body satisfies the CHECK, takes the
collation, and encodes no creation order, and every artifact in the tree would keep asserting a
sortability the column does not have.

**I also adopt the critique's self-refutation of its own amendment 1**: do NOT unify the
app-scoped-token derivation behind a new `app_derivation::control_token_subject`. Routing it through
a seam hides the visible `Uuid` round-trip that is the only clue the defect exists. Write the paired
assertions as Rust tests beside each pair instead (S0), and say in their headers that a green means
"these four still agree", never "the boundaries were enumerated".

**SAFE / NEEDS-BUILD, as used below**: SAFE means the slice's failure mode is a compile error or a
fast per-crate test, so a mistake cannot ship quietly. NEEDS-BUILD means the binding oracle needs a
live backend or was never executed, so the slice is not landable on inspection.

| # | commit | changes | verify | risk | tree green after |
|---|---|---|---|---|---|
| S0 | `test(tests): pin the app-id frontier and the derivation seam before the sweep` | `tests/app_id_frontier_gate.sh` (3 arms, section 4) + four paired-derivation Rust tests beside the code | `bash tests/app_id_frontier_gate.sh` and `tests/gate_arm_census.sh tests --run app_id_frontier_gate.sh` | SAFE | yes (arm 3 ships with a named exemption list) |
| S1 | `fix(metering): drain on the app id producers emit and survive one bad WAL entry` | `Meter` key and `UsageSubject.app` to `AppId`; delete `Uuid::parse_str` in `Meter::drain`; make `OutboxWal::load_pending` skip-and-dead-letter instead of `?`-aborting; delete or re-key the outbox WAL in the same diff | `cargo test -p zeroship-metering -p zeroship-control`, then `tests/e2e_metering_billing.sh` (live PG + Redpanda) | NEEDS-BUILD | yes |
| S2 | `refactor(migrate): derive the app schema and its publication from one seam call` | `apply.rs` derives schema through `app_derivation::schema_name` and the publication through a raw `&app_id.to_string()` two lines apart, under its own AMBIGUOUS comment - unify | `cargo test -p zeroship-migrate-server` + a source-sweep test that the publication name is composed in exactly one production file | SAFE | yes |
| S3 | `refactor(core): give app_derivation the workflow journal schema name` | one producer for `format!("app_{}", ...)` now spelled in `zeroship-migrate-server/src/provisioning.rs` (writer) and `zeroship-plugin-workflow/src/store/pg.rs` (reader), bound by a test in a third crate | `cargo test -p zeroship-plugin-db -p zeroship-plugin-workflow -p zeroship-migrate-server` | SAFE | yes |
| S4 | `refactor(bundle)!: take AppId for the bundle and manifest path segments` | `blob.rs`, `s3_blob.rs`, `unpack.rs`; retires `bundle_path_segment` from the S0 exemption list | `cargo test -p zeroship-bundle -p zeroship-control -p zeroship-worker -p zeroship-core` | SAFE | yes |
| S5 | `refactor(workflows)!: take AppId across the journal, scheduler and rollout lock` | the 92-site workflow concern across `zeroship-plugin-workflow`, `zeroship-workflow-scheduler` and control's workflow block; 2-3 commits. **Must state three refusals out loud**: `parse_app_id`, `signal_key_aad` and `lock_app_journal_accounting`'s `workflow-journal:{app_id}` seed | `cargo test -p zeroship-plugin-workflow -p zeroship-workflow-scheduler -p zeroship-control` + the rewritten cross-crate journal oracle (compare the two derivations, never restate a literal) | NEEDS-BUILD | yes |
| S6 | `refactor(control)!: take AppId across billing, spend and usage aggregation` | 35 sites. **Must state the refusal for `invoice_item_idempotency_key`** (CONFIRMED, `cron/billing_reconcile.rs`) and its sibling `billing-correction:{app_id}:...` | `cargo test -p zeroship-control` + `tests/e2e_platform.sh` | NEEDS-BUILD | yes |
| S7 | `refactor(control)!: take AppId across app lifecycle, env, secrets and egress` | 40 sites. **`app_secret_aad` is retyped to `&AppId` here, with `.uuid()` spelled at the call and the reason in the body** - so the choice is made once under review rather than left as an open invitation in every later slice | `cargo test -p zeroship-control` + `tests/e2e_platform.sh` | NEEDS-BUILD | yes |
| S8 | `refactor(worker)!: carry AppId from the dispatch door to the isolate cache edge` | `handler.rs` stops unwrapping `.uuid()`; `logs.rs`, `sync.rs`, `policy.rs`. `cache.rs`'s interior does **not** move here - it is where the tenant becomes `APP_ID`, which is the schema name *and* the HKDF salt | `cargo test -p zeroship-worker` + `tests/run_worker_suite.sh` | NEEDS-BUILD | yes |
| S9 | `refactor(runtime): take AppId for the rpc abort registry` | plus the `zeroship-data-engine` and `zeroship-auth` singles | `cargo test -p zeroship-runtime -p zeroship-data-engine -p zeroship-auth` (never `--lib`) | SAFE | yes |
| S10 | `test(tests): assert every typed-id column is collated and apps.id decodes to a v7` | replaces `tests/organization_authority_gate.sh`'s hardcoded `relname IN (...)` (CONFIRMED) with a CHECK-constraint-driven catalog query; adds the v7 decode arm | live PG; the arm must be green *before* S11 so its verdict on S11 means something | NEEDS-BUILD | yes |
| S11 | `feat(db)!: key apps on a typed app id and delete both uuid bridges` | THE unsplittable commit - see below | full stack, section 5 | NEEDS-BUILD | yes, or not at all |
| S12 | `refactor(data-engine): the data plane takes a derived name, not the tenant string` | the 386 `&str` sites take `SchemaName` / a role newtype / a KV scope / a storage prefix - **not `AppId`**, whose `parse` refuses their literal fixtures | `cargo test -p zeroship-data-engine -p zeroship-plugin-db -p zeroship-plugin-kv -p zeroship-plugin-storage` | NEEDS-BUILD | yes |

**Parallel.** S2, S3, S4 are independent of each other and of S1. S5 is independent of S6/S7. S6 and
S7 both touch `zeroship-control` but disjoint files - runnable in parallel only with explicit commit
pathspecs (shared worktree rule). S8 and S9 are independent of S5-S7. S10 is test-only and can be
written at any point, but must LAND before S11. Everything converges on S11, which is serial by
construction.

**Every slice, before push:** `./tests/clippy_gate.sh` (needs `pnpm build` and
`crates/zeroship-runtime/tests/setup-wpt.sh` to have run, or it refuses).

### S11, the unsplittable commit

Contents, and the rule for what is in it: **every site where a wrong edit is invisible.** Binding
`&app.uuid()` against a still-`uuid` column compiles *and works*, so a missed bind's failure mode is
that the old behaviour keeps working. That is why the following move together:

- two migration files, forced by the engine's snapshot rule (a foreign key is lowered against a
  snapshot taken before the migration and cannot see a raw collation island inside it): file N
  converts and collates all 28 app-id columns including the four with no FK to `apps`
  (`app_audit.app_id`, `workflow_scheduler_timers.app_id`, `workflow_scheduler_inflight.app_id`,
  `billing_line_provider_refs.app_id`); file N+1 re-adds the 24 live FKs including the composite
  `billing_line_provider_refs_line_fk`. Bump `db/migrations-ts/op-counts.json` for both.
- `apps.id` becomes `t.text().notNull()` with `CHECK (id ~ '^app_[0-9A-Za-z]{22}$')` and **no
  default**, copying `db/migrations-ts/20260907000300_worker_instances.ts` verbatim (CONFIRMED:
  `t.text().notNull()`, a `^wkr_[0-9A-Za-z]{22}$` regex check, and a raw
  `ALTER COLUMN "id" TYPE text COLLATE "C"`). Today it is `t.uuid().notNull().default(uuidV4())`
  (CONFIRMED).
- the mint moves into `registry.rs::create_app`, whose INSERT omits `id` today (CONFIRMED:
  `INSERT INTO zeroship.apps (name, plan_id, project_id, organization_id)`). `AppId::mint` has zero
  production callers - CONFIRMED, `grep -rln 'AppId::mint' crates libs --include='*.rs'` returns
  only `crates/zeroship-core/src/{app_id,typed_id,entity_id,app_derivation}.rs`. The other three
  `INSERT INTO zeroship.apps` are in test modules (PLAUSIBLE - classified by two surveys, not
  re-classified by me).
- core's wire types (`AppRecord.id`, `RouteMap`, `VersionMap`, and the four `ControlEvent` variants,
  which are cheap and are *not* a wire).
- `zeroship-worker/src/cache.rs`'s interior (`APP_ID` is the schema name and the HKDF salt).
- control's `row.get("id")` sites and its SQL bind sites.
- the eight RLS policies whose predicate is `currentSetting("zeroship.tenant_app").cast({to:"uuid"})`
  plus `zeroship-gateway/src/rls.rs::set_tenant_app`.
- the uuid arms of **all three** permissive parsers: `zeroship-authz/src/authority.rs::
  app_uuid_or_refuse`, `zeroship-cli/src/main.rs::app_id_or_refuse`, and
  `zeroship-control/src/workflow_instance_api.rs::parse_app_id`.
- **both bridges**, `AppId::from_uuid` and `canonical_app_id_for`. Deleting only `from_uuid` leaves
  a permanent uuid-to-typed adapter that is invisible because both return `AppId`.

---

## 3. What will not surface as a compile error

Ranked by how quietly it fails. This is the section that earns the document.

**1. The HKDF salt (`app_derivation::encryption_salt`, consumed by
`zeroship-data-core/src/encryption/keys.rs::derive_key`).** The app-id STRING is the salt
(`Hkdf::<Sha256>::new(Some(app_id.as_bytes()), root)`), and it arrives as the worker-injected
`APP_ID` `&str`, so nothing goes red. New salt gives new `k_enc` (loud: ciphertext stops decrypting)
and new `k_siv` - and the `k_siv` half is the quietest failure in the whole sweep: deterministic
lookup tokens stop matching, so **an equality search over an encrypted column returns fewer rows and
no error.** PLAUSIBLE (three surveys agree; I did not re-read `derive_key`). This is why the schema
name cannot be renamed early via `APP_ID`: it is one string feeding both `binding_for_isolate` and
the salt.

**2. Advisory-lock seeds spelled outside the seam.** CONFIRMED: `zeroship-control/src/
workflow_limits.rs::lock_app_journal_accounting` seeds `format!("workflow-journal:{app_id}")` and
`workflow_instance_api.rs::consume_signal_rate_limit` seeds
`format!("control:workflow_signal:app:{app_id}")`, both into `hashtext`/`pg_advisory_xact_lock`.
Move one path and not another and the two holders **stop contending**. No error, no log: journal-byte
quota accounting silently stops being serialized. The existing source-sweep test is bound to the
`zeroship:app-lifecycle:` prefix and says nothing about a different one. Same family, and reachable
by a plausible tidy-up rather than by the flip: substituting `canonical_app_id_for` for
`AppId::from_uuid` inside `lifecycle_lock_seed`'s four call sites re-keys the app lifecycle lock
while every uuid stays in place, so `archive_app` and a worker `claim` stop contending and a run is
claimed across the archive marker.

**3. The meter key.** CONFIRMED: `crates/zeroship-metering/src/meter.rs::drain` does
`Uuid::parse_str(app_id)` and the failure arm warns `"meter: drain skipped non-UUID app_id"` then
`return false` inside `apps.retain` - it **evicts**. The app serves traffic, is never billed, and
the warning decays to silence *by design* because the counters are gone. Producers are `&str` and do
not go red. S1 exists to delete the parse entirely so there is no fail-open arm left to be quiet.

**4. The metering outbox WAL poison pill.** CONFIRMED: `OutboxWal::load_pending` in
`crates/zeroship-metering/src/outbox.rs` does `serde_json::from_slice(payload.value())
.map_err(|source| OutboxWalError::Decode { seq, source })?` inside the iteration - the `?` aborts
the ENTIRE load on the first bad entry, and the caller logs "events stay in the WAL for the next
attempt". One old-shaped record makes that process publish **nothing, forever**, behind a log line
that reads as a healthy retry. Worse than #3, which loses one app. The Kafka side is defended
(`DeadLetterSink::record_decode_error` in `cron/event_forwarder.rs`); the producer-side WAL is not.
The WAL is also **not one of the compose volumes** - its path is config-derived - so `down -v` does
not clear it.

**5. The signed workflow signal token's app id.** `WorkflowSignalTokenClaims.app_id: String` in
`crates/zeroship-core/src/typed_id.rs`, validated only by an is-empty check, and the HMAC covers the
exact payload bytes - so the token is self-consistent under **either** rendering, forever. A `String`
field that silently accepts both shapes, in a security token. PLAUSIBLE.

**6. The third permissive parser, on the auth path.** CONFIRMED: `parse_app_id` in
`crates/zeroship-control/src/workflow_instance_api.rs` tries `Uuid::parse_str` then
`typed_id::parse_with_prefix(raw, APP_PREFIX)`, and has three call sites - the `x-zeroship-app-id`
channel header, the *unverified* signal-token app id (which **selects which secret verifies a
signature**), and `claims.app_id` on the verified token. Both designs and every survey said there
were exactly two parsers. It is invisible to their enumerations because the app id is in its
**return** type, not a parameter. It normalizes and discards the rendering, which is exactly what
makes #7 survivable-looking on the wire and fatal at the HMAC.

**7. The app-scoped control token: rustc visits one side of an HMAC.** CONFIRMED, all three sides:
producer A is `zeroship-plugin-workflow/src/lib.rs::build_backend(&self, app_id: &str)` calling
`client::app_scoped_token(control_key, app_id)` on the **server-injected `APP_ID` string** (the
`&str` tier - the compiler never visits it); producer B is `zeroship-worker/src/handler.rs` calling
`derive_app_scoped_control_token(&config.control_key, &request.app_id.to_string())` (`Uuid` tier -
visited); the verifier is `workflow_instance_api.rs::check_app_scoped_auth(app_id: &Uuid)` calling
`validate_app_scoped_control_token(key, control_key, &app_id.to_string())` on a `Uuid` that came out
of #6. After the flip the injected string is `app_<b62>`, the verifier re-renders a hyphenated uuid,
and **every `env.workflows.*` call from inside an isolate 401s** behind a
`"workflow instance API: auth rejected"` line that reads as a control-key mismatch.

**8. The OAuth resource audience: two services, one signed claim, only one side moves.** CONFIRMED:
producer `zeroship-auth/src/oidc/authorization_code.rs::OAuthClient::resource_audience()` renders
`format!("app:{id}")` from `app_id: Option<Uuid>` read out of `app_oauth_clients.app_id`, a column
that flips; consumer `zeroship-gateway/src/router/auth.rs` derives
`app_id_from_oauth_client_id(expected_client_id)` and renders `format!("app:{expected_app_id}")`,
then compares against `claims.aud`. The `oac_<base62>` derivation is bit-invariant under the flip -
which is precisely the defect: the consumer's input never changes, so it keeps compiling and keeps
rendering a hyphenated uuid. Result: `BearerOutcome::Invalid` on every per-app raw-OP Bearer token,
fleet-wide. Compounding it, `Option<Uuid>` is **invisible to the 315 pattern**: CONFIRMED, 11 sites
via `grep -rnP 'app_id\s*:\s*(&\s*)?Option<\s*(uuid::)?Uuid\s*>' crates/*/src libs/*/src
--include='*.rs' | wc -l`.

**9. The replication-slot orphan.** The slot name embeds a hash of the app-id text, so the flip
mints a new slot and orphans the old one. The reaper sees it, then computes
`worker_is_live = worker_token == self.own_worker_token || slots.iter().any(|slot| slot.active)` -
the orphan and the new live slot share a worker token, so the group is skipped and the orphan's
inactivity clock is cleared **every sweep, forever** (PLAUSIBLE; quoted by two surveys and the
critique from `crates/zeroship-plugin-db/src/slot_reaper.rs`). An unconsumed logical slot pins
`restart_lsn` and grows `pg_wal` without bound. It arrives hours to days after a green deploy and
presents as a full disk, which this repo already records as "a plausible innocent test failure".
There is no production `DROP PUBLICATION` and no production `DROP SCHEMA` anywhere, so publications
and per-app schemas leak too - harmlessly, but unboundedly.

**10. Deprovision that succeeds while the data stays.** `crates/zeroship-plugin-db/src/
drop_namespace.rs::drop_namespace` takes `&str` (CONFIRMED it exists and is `&str`-shaped) and
issues `DROP SCHEMA IF EXISTS ... CASCADE`. It does not go red; under a wrong repair it names a
schema that does not exist, `IF EXISTS` swallows it, and it returns `Ok` while the tenant's data
stays resident.

**11. A Stripe idempotency key derived from the printed app id.** CONFIRMED:
`crates/zeroship-control/src/cron/billing_reconcile.rs::invoice_item_idempotency_key` composes
`billitem:{organization_id}:{app_id}:{period_start_unix}:{segment_no}` and it is sent to Stripe as
`Idempotency-Key` and recomputed at the orphan-reconcile site to name the items it must DELETE. Both
repairs compile (`.as_str()` re-keys, `.uuid()` preserves). A re-keyed namespace means a retried
period posts a **second** invoice item - the over-billing mirror of the under-billing that
function's own doc warns about. Sibling: `format!("billing-correction:{app_id}:{meter}:...")`.
Pre-launch bounds the money; it does not bound the shape.

**12. A half-flipped bind stays green.** `&app.uuid()` bound against a still-`uuid` column compiles
AND works. The failure mode of a missed site is that the old behaviour keeps working, in both
directions. No type check and no test can see it. This is the whole reason S11 is one commit.

**13. Loud but misattributed** (listed because the wasted debugging is the cost, not the outage):
`signal_key_aad` in `workflow_instance_api.rs` (CONFIRMED) is `Display`-derived so it *will* not
compile - but the natural `.as_str()` repair re-keys every `zeroship.workflow_signal_keys.secret_ct`
and surfaces as `WorkflowApiError::Database("decrypt workflow signal key: ...")` on both mint and
verify, which reads as master-key rotation. `env_store::app_secret_aad` retyped with
`.as_str().as_bytes()` compiles and makes every stored app secret undecryptable - loud on READ, not
on write, so it lands on a later commit, and `env_store`'s re-encrypt recovery path is the one that
breaks hardest. `app_derivation::ring_key` is fenced by its `[u8; 16]` return type, but if it ever
moved to the printed form every app's CHWBL position changes at once and the fleet cold-starts
inside one poll cycle with no error at all - a latency cliff.

**14. Ordering and index use after the collation.** The critique's correction, and I side with it:
two text columns with differing implicit collations derive collation `none`, and `=` then **raises**
`could not determine which collation to use` rather than returning a wrong set. What is genuinely
silent is `ORDER BY` / keyset pagination order and index non-use. NEEDS-BUILD. Fix the *reason* in
the commit body; the gate is right either way.

**15. The 386 `&str` sites never go red at all**, and they outnumber the 315. They keep compiling and
silently carry a different string. That is why S12 exists and why "it is done when it compiles" is
false in both directions.

---

## 4. The completeness proof

**Deleting `from_uuid` is not the proof.** It proves the derivation half converted; six production
callers (PLAUSIBLE - `crates/zeroship-plugin-db/src/replication.rs`'s hit is inside a test module,
contradicting the brief, and I side with the two surveys that classified it). Its count also **rises
before it falls**: converting a callee forces its still-uuid caller to construct one. A ratcheting
ceiling on that number is red on every correct slice, so do not build one.

**The single check.** One green run of `tests/app_id_frontier_gate.sh` at S11, whose arms are:

1. **bridges**: `AppId::from_uuid` and `canonical_app_id_for` have zero production callers, matched
   *exactly* (a bare `from_uuid` grep also matches `zeroship_core::typed_id::from_uuid_string`, a
   different function used for PLAN ids), with a positive control pattern that still hits today so a
   silently-broken enumeration cannot print what a clean tree prints.
2. **carriers**: the pinned set of production files declaring an app id as a raw `Uuid` is empty.
   `tests/lib/gate_arms.sh` refuses a floor of 0, so this arm must count **what it searched** (the
   number of production `.rs` files swept, floored well under that) and report the empty carrier set
   as its verdict. The anti-vacuity contract forces the right construction here.
3. **seam**: every `pub fn` in `crates/zeroship-core/src/app_derivation.rs` has at least one
   production caller, exemption list empty. This arm is RED TODAY and that is the finding - six
   derivations (`storage_prefix`, `meter_key`, `kv_scope`, `encryption_salt`, `bundle_path_segment`,
   `attach_alias`) have zero production callers and are hand-spelled beside the seam (PLAUSIBLE,
   measured by two surveys with `lifecycle_lock_seed` as the positive control).

**And four things a shell gate cannot do, which must also hold:**

4. **The four paired-derivation Rust tests pass**, each beside its own code, each asserting the two
   sides produce equal bytes: the app-scoped control token, the OAuth resource audience, the
   workflow journal schema name, the app-id channel header. Written as Rust so each fails to
   *compile* when either side's type moves. Their header must say a green means "these four still
   agree", never "the boundaries were enumerated" - a hand-enumerated list is a census, and this repo
   has already recorded four gates going green while ruling on nothing.
5. **Live PG, negative**: zero columns in the `zeroship` schema named `app_id` or `owner_app` are
   still type `uuid`, with the floor computed from the migration corpus rather than written down.
   This is what catches the four Group-B columns with no FK to `apps`, which an FK-driven sweep
   cannot see.
6. **Live PG, positive**: `apps.id` is `text COLLATE "C"`, its CHECK is `^app_[0-9A-Za-z]{22}$`, and
   **decoding it back yields `Uuid::get_version() == Some(7)`**. Without the version arm a base62 v4
   body passes every other check while encoding no ordering, and the generalized collation arm would
   certify it.
7. **The ORM cutover, asserted rather than inferred** - it is the directive's stated reason:
   `Ident::parse_as(<a live app's schema name>, IdentRole::Namespace)` returns `Ok`. Today
   `zeroship-schema`'s `validate_schema` accepts `-` while `zeroship-data-query-builder`'s
   `Ident::parse_as` accepts only `[A-Za-z0-9_]`; the two validators disagree and the flip is what
   reconciles them. Assert that both accept the same string.

**Should a gate ratchet the frontier while the sweep is in flight? Yes - but pin FILE SETS, not
counts.** Arms 1 and 2 are pinned exact sets that shrink only inside the slice that earns the move,
so any pin edit appears in a diff beside the code that justifies it. Two honest limits, which go in
the gate's own header rather than being discovered:

- The pin is a **direction** check, not a completeness check. An enumeration keyed on the identifier
  `app_id` misses real app-id parameters spelled otherwise (`registry.rs::get_app(&self, id: &Uuid)`
  and its neighbours), and misses `Option<Uuid>` entirely (11 sites, CONFIRMED). Completeness is
  proved by the compiler at S11 when both bridges die; the gate exists only because the compiler
  cannot see a *new* uuid-typed app-id parameter added inside an already-converted crate.
- The arm is **weak for `zeroship-control` specifically**, and that is where 126 of the 315 sites
  are. S6 and S7 convert control by concern across files that keep other uuid app ids, so control's
  files stay pinned across several commits while the arm rules on them without shrinking. Say so in
  the header rather than letting a green line imply cover it does not give.

Arm 2 is **deleted in S11 itself**: a pinned list over an empty set rules on nothing.

---

## 5. What a developer has to do to their local database

Exact commands, in this order. `down` alone is not enough, and
`tests/provision_test_backends.sh` runs no migrations and no reset.

```
docker compose -f deploy/compose/docker-compose.yml down -v
tests/provision_test_backends.sh
deploy/ops/db-migrate.sh
tests/sweep_test_databases.sh --apply
```

`down -v` is mandatory because the named volumes `pgdata`, `bundles`, `app-storage` and `redis-data`
are **each keyed by the app-id rendering** - the per-app PostgreSQL schema, `bundle_path_segment`,
`storage_prefix` and the `{app_id}` Redis hash tag respectively. Nothing in the tree issues a
production `DROP SCHEMA` or `DROP PUBLICATION`, so without the wipe the old per-app schemas, roles,
publications and replication slots are orphaned, and an orphaned logical slot pins WAL until the
volume fills (section 3, item 9).

**Then, separately, the metering outbox WAL** - it is NOT a compose volume and `down -v` does not
touch it. Find and remove it before starting anything that produces usage:

```
grep -rn "default_wal_path\|wal_path" crates/zeroship-metering/src/outbox.rs
# then remove the resolved redb file(s) for this host, e.g.
find . ~/.local/share -name '*.redb' -path '*meter*' -print
```

Leaving one old-shaped record there stops that process publishing any usage at all, permanently,
behind a log line that reads as a healthy retry.

**What re-provisions itself, so nobody needs a flag:** `tests/run_auth_suite.sh` and
`tests/run_worker_suite.sh` name their database for a hash of `db/migrations-ts/*.ts`, so a branch
that adds a migration gets its own database automatically; `sweep_test_databases.sh --apply`
reclaims the orphans. The e2e suites `CREATE DATABASE` per run.

**If you skip the wipe**, editing an already-applied migration is loud, not silent:
`crates/zeroship-migrate-core/src/apply/executor.rs` calls `check_checksum_drift` before any pending
work and returns `ApplyError::ChecksumDrift { version, recorded, expected }` on the first mismatch
(NEEDS-BUILD - read from source, not induced). Adding *new* files is the dangerous case: it applies
cleanly and leaves the orphans above.

**Before `./tests/clippy_gate.sh`**, `pnpm build` and `crates/zeroship-runtime/tests/setup-wpt.sh`
must have run, or it refuses by name rather than linting a smaller workspace.

---

## 6. Open decisions for the owner

1. **Does `users.id` flip in the same sweep? My recommendation: no.** They are separable and the
   asymmetry is structural: there is no `UserId` type and no user equivalent of `app_derivation` - a
   user id names no schema, no role, no publication, no slot, no salt, no meter key, no storage
   prefix. It is a key and a subject, so its flip is mechanical where the app flip is dangerous. The
   2026-09-06 organization work already cut the strongest coupling by retargeting billing off
   `creator_id`, and the one composite key over `(app_id, user_id)` was `app_members`, which is
   dropped. The **shared** piece is S10's generalized collation arm - build it once, during this
   sweep, and the user flip inherits it. (PLAUSIBLE; Survey B measured 43 FKs to `users.id` against
   24 live for `apps.id`.)

2. **Re-mint or re-encode?** I recommend **re-mint with `AppId::mint()` (v7)**. Re-encoding the
   stored uuid preserves every `oac_` client id and every `ring_key` - genuinely attractive - but it
   produces a column that is collated, CHECK-passing, deterministic and **not time-ordered**, while
   every artifact in the tree keeps asserting sortability. The cost you are accepting: locally
   registered OAuth relying parties must be re-registered, and any preserved `bundles` /
   `app-storage` fixture corpus is orphaned with no mechanical rename back. Name that in the commit
   body.

3. **Do S6 and S7 (control, by concern) ship at all, or fold into S11?** The honest position: the
   sliced sequence is only legitimate if S11 lands. If it stalls - a freeze, a reprioritisation -
   the tree is left with both bridges, three permissive parsers and no forcing function, and a pin
   that has stopped shrinking looks exactly like a pin that is finished. If you cannot commit to
   finishing the sweep in one stretch, the right answer is one commit for everything, accepting that
   it is unreviewable, because an unreviewable commit that lands beats a reviewable sequence that
   stops in the middle. **This is the only decision that changes the shape of the plan.**

4. **`app_secret_aad` and `signal_key_aad` keep the bits.** I have put the `app_secret_aad` retype
   into S7 deliberately so the `.uuid()` choice is made once under review rather than left as an
   invitation in every later slice. Confirm you want that, rather than leaving it a bare `Uuid` and
   accepting that the file reads as unfinished.

5. **Scope name for S12.** `data-engine` is a new scope (CONTRIBUTING permits one per crate) and does
   not appear in recent history. Say the word if you would rather it be `plugin-db`.

6. **`docs/runbooks/local-dev.md` gets section 5.** Nothing enforces `down -v`; if it lives only in
   a commit body it will be discovered painfully exactly once per developer.

---

NEEDS-BUILD: nothing in this document was compiled, tested or linted. Every CONFIRMED claim was read
from the tree this session; every PLAUSIBLE claim is a survey's reading I did not re-run. No file
outside `.scratch/` was edited, created, deleted or committed.
