# Does SC-2's `__zeroship_state` authority row survive decisions 4, 7 and 8?

**Date:** 2026-08-28
**Scope:** one question, three parts. Design only; no code was changed.
**Subject:** the authority row specified at
`docs/proposals/2026-08-26-sc2-sqlite-actor-protocol.md:326-345`, and the
`AuthorityRead` command that reads it (`sc2:242`, `sc2:248-268`).

---

## Verdict

**The row goes, in full. All six columns, plus the `AuthorityRead` command that
exists only to return them.** Not one half dead and one half homeless: the two
halves have three different fates and none of them is this row.

| column | fate | why |
| --- | --- | --- |
| `epoch` | **dead** | Decision 7. The descriptor owns schema-revision identity, and on SQLite it always did. |
| `state` | **dead** | It encoded `stable`/`changing`, a state of the epoch/lease step decision 7 deleted. No reader survives it. |
| `changed_at` | **dead** | Metadata of the above. |
| `ceiling` | **HOMED, not homeless** | Decision 4 gave it a home: worker/composition configuration, a field of the binding. The parent already issues the retraction (`design:227-228`); SC-2 has not absorbed it. |
| `incarnation` | **homeless AND subjectless on this tier** | Fork C is unhomed (decision 7), and the lifecycle it fences does not run on SQLite. |
| `deprovisioned_at` | same | same |

**The reading in the brief is right about the epoch half and half-right about the
identity half.** The correction is that `ceiling` is not Fork C's and is not
homeless. It was homed by **decision 4, on 2026-08-27**, which the brief does not
name and which SC-2 does not record either. That makes three unrecorded
decisions bearing on this one row, not two.

Everything below is checked against the tree at `cca4e3553` plus the working
directory.

---

## 1. Does the epoch half survive? No, and the asymmetry runs the other way

### What the reservation's first statement actually needs

SC-2 gives the row exactly one job in the protocol: "Every reservation's first
statement reads it" (`sc2:329-330`). Two things could be meant by that, and they
have opposite answers.

**If the job is the ACT of reading** - anchoring the deferred WAL snapshot - the
row is not needed and never was. SC-2's own acceptance arm concedes this in
writing (`sc2:405-409`):

> Coherence is supplied by SQLite itself: a deferred WAL read transaction holds
> one snapshot for its lifetime, so "coherently old or coherently new, never
> torn" is a property of the engine rather than of anything this protocol does,
> and the arm stays green on an implementation that does none of the work this
> document proposes.

A deferred transaction takes its snapshot at its first read, whatever that read
is. Making it a `SELECT` against `__zeroship_state` moves the anchor a few
microseconds earlier and changes nothing else.

**If the job is the VALUE read** - "has the schema changed under me" - the
descriptor is the channel that answers it, and on the SQLite tier it is a
**live** channel, not a frozen one. Both halves of that need stating, because
the production and dev vectors differ and SQLite runs only in the second.

The chain is fixed and pre-user (`descriptor.rs:34-49`):
`manifest.runtime_descriptor` -> worker blob resolve
(`crates/zeroship-worker/src/sync.rs:40-72`) -> `RuntimeState.runtime_descriptor`
(`crates/zeroship-runtime/src/core/state.rs:587`, stamped at
`core/runtime.rs:1199`) -> `setup_globals` validates and exposes
`globalThis.__zsRuntimeDescriptor`
(`crates/zeroship-runtime/src/core/init.rs:3424-3432`, called once from
`init.rs:2221`) -> `installSchema` (`sdks/bootstrap/src/install-schema.ts:1320-1350`)
-> `register_model_dispatch` -> the per-isolate store
(`crates/zeroship-data-v8/src/register_model/mod.rs`).

**In production the descriptor is frozen after boot, by capability deletion.**
`registerModel` is not on `env.db`; it is reachable only through the private
`__zsDbPlatform` resolver, which `runtime-entry.ts:229-231` deletes
unconditionally after `installSchema` has run, with the reason stated at
`runtime-entry.ts:214-223`. Store entries are never evicted
(`register_model/mod.rs:101-103`).

**On the dev tier the descriptor is deliberately mutable, and is re-applied on
HMR.** `applyRuntimeDescriptorJson` sets and deletes the global
(`packages/vite-plugin/src/dev-bootstrap/index.ts:55-81`) and is wired to the HMR
callback beside `entry.resetSchemaInstalled()`
(`dev-bootstrap/index.ts:182-186`); `cache_schema` overwrites in place, and says
why: "Idempotent: a re-register overwrites, which is what a dev re-deploy of the
same deploy token means" (`crates/zeroship-data-v8/src/context.rs`).

That is the fact that decides this sub-question, and it decides it against the
row. **On the only tier SQLite runs on, "the schema changed" already has a live
delivery channel, and it is the same channel the migration command writes.**
`migrate-dev.ts:106-113` regenerates `schema.runtime.json` and applies the
migrations in one invocation; the dev bootstrap picks the regenerated descriptor
up over HMR. An epoch row would be a second, later, less informative signal
racing a channel that is already carrying the answer.

There is also no revision value in the descriptor for a row to be compared
against, which is worth stating because it looks as if there is one. The
descriptor's `version: 2` is a **wire-format** version
(`sdks/bootstrap/src/install-schema.ts:157-160`;
`crates/zeroship-migrate-core/src/render/gen_types.rs:126-129`), enforced at
`init.rs:3735-3740`, and it is stripped in JS before Rust ever sees it -
`runtime-entry.ts:112` forwards only `c.fields`. `descriptor.rs` defines no Rust
type at all; the entries are `Arc<serde_json::Value>` in a map
(`context.rs:293`) keyed by `format!("{}:{}:{}", app_id, deploy_token,
collection)` (`context.rs:616-624`), and `DbBinding` is
`{ app_id, deploy_token }` (`crates/zeroship-data-v8/src/binding.rs`).
The only durable identity is the manifest's sha256 of the blob
(`crates/zeroship-bundle/src/manifest.rs:183-186`), which the worker uses to
fetch it (`sync.rs:47-49`) and plugin-db never observes.

So the descriptor's identity is the **deploy token**, and a test proves one
deploy's binding cannot read another's entry (`descriptor.rs:113-133`). An epoch
row would be introducing a revision number on the backend where the descriptor
is already re-delivered on change, to be compared against a value the runtime
does not hold.

### The epoch's own defined response is unimplementable after decision 8

This is the sharpest argument and it does not depend on symmetry at all.

Fork C defines what an epoch mismatch **means**: "An epoch mismatch means
*re-resolve*: the schema moved, pick up the new one"
(`docs/proposals/2026-08-26-sc5-service-ownership.md:216-220`; same in
`design:229-233`). That verb was implementable when the runtime could re-resolve
schema by introspecting the catalog. Decision 8 deleted that capability
(`632c1d1fa`: `crud/introspect_schema.rs` 1054 lines and `live_metadata.rs` 517
lines removed), and the rule replacing it is absolute (`design:1014-1015`):

> The runtime descriptor is the sole authority for schema, and the data plane
> never reads the catalog.

There is nothing for a SQLite binding to re-resolve **to**. The descriptor
arrives from the deploy artifact, not from the database. So a SQLite epoch
mismatch could only ever deny, and the only thing that clears a denial is a new
isolate with a new descriptor - which is SC-4 Decision 2's supervised restart
(`sc4:99-110`). The row would buy a faster, worse-worded trigger for a restart
the supervisor already owns.

### The asymmetry, looked for rather than assumed

The brief is right that the justification would have to be a SQLite-specific
failure mode. There are three candidates. I checked each; two run against the
row and the third is already better served elsewhere.

**(a) The file is swapped beneath a live connection.** This is the genuinely
SQLite-shaped hazard, and it is exactly the case the row cannot see. An open
connection stays bound to the old inode - the tree asserts this in its own words
at `design:2613-2615` ("a lock release must not leave a connection bound to an
obsolete inode") and `sc2:288-289`. A reservation reading `__zeroship_state`
after an out-of-band swap reads the **old** row out of the **old** inode and
reports that nothing happened. The row is blind to the one hazard that is
peculiar to this backend.

The tree's actual answer is a protocol command, not a row:
`Command::ReattachFile` DETACHes the alias on both connections, `rename`s, and
re-ATTACHes (`crates/zeroship-data-v8/src/backend/sqlite/session.rs`,
implemented at `:1851-1866`). It is the shipped form of SC-2's `DetachApp` and
it carries **no** incarnation and reads **no** row.

**(b) The file is modified in place by another process.** This is the real dev
case: `pnpm migrate` writing DDL into the same file while `pnpm dev` runs. Here
the row *would* work, and this is the strongest keep-argument. It is answered in
section "Argue against this conclusion" below; the short form is that the tree
already detects it, outside the data plane, with a better instrument.

**(c) Drift between the applied schema and the descriptor.** On PostgreSQL this
is a live risk with a named cost: "The deploy pipeline's ordering guarantee is
now load-bearing" (`design:1252`, `index:336-341`) - a separate migration
service and a separate deploy can disagree. **On SQLite they are one command.**
`migrate-dev.ts:106-113` regenerates `env.db.ts` + `schema.runtime.json` FIRST
and derives the apply's collection list from what it just wrote, with the
comment saying why: "generating and applying in one command is what keeps them
from disagreeing." The descriptor and the file come out of the same migrations
directory in the same invocation.

So the asymmetry exists and points the other way: **SQLite is the backend where
descriptor-vs-database drift is structurally hardest to produce, and where a
database-resident revision marker is least able to detect the drift that
remains.** The symmetry argument is not lazy here; it is generous to the row.

### The one thing the row's read WAS load-bearing for, and it survives

`design:2603-2611` deletes the per-operation cross-process flock lease and
justifies it with a sentence that names the row:

> Every data operation runs inside an actor transaction reading the epoch row as
> its first statement: a deferred WAL transaction sees one stable snapshot ...

Read the colon carefully. The substance after it is the **deferred WAL
transaction**, not the row. What the flock deletion needs is that every data
operation runs inside an explicit `BEGIN DEFERRED ... COMMIT`, and that is SC-2
Decision 1 (`sc2:67-68`), already built:
`session.rs:1628` issues `BEGIN DEFERRED`, `session.rs:1242` sets
`busy_timeout = 5000`, and the module doc states the wrap at
`session.rs:10-12`. **Deleting the row does not reinstate the flock.**

### Decision 8 is fully landed, which removes the fallback reading

One reading would keep the row as the SQLite substitute for a catalog read that
still happens elsewhere. It does not happen anywhere. Measured across
`crates/zeroship-data-v8/src/`:

- The `SchemaIntrospect` trait and both implementations are compiled out of
  every release build: `backend/mod.rs:556-568`, `backend/postgres.rs:334-344`,
  `backend/sqlite/mod.rs:804-846`, all `#[cfg(any(test, feature =
  "test-helpers"))]`. `zeroship_schema::diff::read_live_schema`
  (`crates/zeroship-schema/src/diff.rs:598`) has exactly one caller in the tree
  and it is the gated one at `postgres.rs:344`.
- The only ungated catalog read left is `PRAGMA table_info` resolving **column
  names for CDC events**, fail-soft with a positional fallback
  (`backend/sqlite/cdc.rs:652-663`, fallback at `:588-599`). It never reaches
  `collection_schema`.
- ~~`epoch` occurs **0 times** in `crates/zeroship-schema/src/`. All 20 hits in
  `crates/zeroship-data-v8/src/` are Unix-time arithmetic or prose; the one
  schema-epoch mention is an explicit negation at
  `crates/zeroship-data-v8/src/auth/mod.rs`.~~

  **CORRECTED 2026-09-04: ALL THREE OF THOSE NUMBERS ARE NOW WRONG, and the
  cited file does not exist.** The 2026-09-03 engine extraction moved the data
  plane out of `zeroship-data-v8` into `zeroship-data-orm`, which
  invalidated the measurement rather than merely the path - so repointing the
  citation alone would have left two stale counts standing beside a freshly
  corrected link. Re-measured today with `grep -rio epoch`:
  `crates/zeroship-schema/src/` **6** (not 0), `crates/zeroship-data-v8/src/`
  **0** (not 20), `crates/zeroship-data-orm/src/` **69**. The quoted
  negation is verbatim at `crates/zeroship-data-orm/src/auth/mod.rs`.

  The 69 also change what the bullet ARGUED. It was offered as evidence that the
  data plane does not carry an epoch; the data plane now carries the whole
  consumer half of one - `SchemaEpoch`, the `ReResolve` verdict and the
  comparison - with only the producer missing. The negation at `:36-38` is still
  in the tree and still says the runtime descriptor is the schema authority, so
  the two now sit side by side and the contradiction is the live question, not
  a settled one.

### And if the epoch's question ever returns, the carrier already exists

**CORRECTED 2026-09-04: IT DOES NOT. THIS WHOLE SUBSECTION IS NOW FALSE, and it
is left standing rather than deleted because a later document leans on it.** The
carrier and all four of the code citations below are gone.
`crates/zeroship-data-v8/src/audit.rs` does not exist; the deletion is recorded
in two places - `crates/zeroship-data-orm/src/backend/mod.rs` says the
audit-table operations "(`ensure_audit_table`, `next_schema_version`,
`write_audit_row`, ...) and the `IndexBuilder` capability they existed to record
are both DELETED", because with the data plane's last DDL removed the provenance
log had nothing to record. `grep -rn next_schema_version crates/` now matches
only that comment.

So "nobody needs to re-mint `__zeroship_state`, the carrier already exists" is no
longer a reason to do nothing: **there is no carrier on either tier.** This
matters beyond bookkeeping, because
`docs/proposals/2026-08-26-sc4-dev-and-hmr-mechanism.md` cites this review for
its "No dev epoch, and no dev authority domain - there is nothing here to
specify" conclusion, and
`docs/proposals/2026-08-28-app-database-decoupling.md` says the opposite, that "a
dev-tier equivalent is owed". Whoever settles that disagreement must start from
the tree as it is, not from the paragraph below.

The original text follows, for the record:

Worth recording so nobody re-mints `__zeroship_state` later. A monotonic per-app
schema revision, written by the migration path into the **app's own schema**,
already exists on both backends: `__zeroship_migrations.schema_version`
(`crates/zeroship-data-v8/src/audit.rs`, DDL at `:251`, minted by
`next_schema_version` at `:343-352`; SQLite counterpart at
`backend/sqlite/mod.rs:1287-1332`). It also satisfies `AGENTS.md`'s "if the
worker can do it, it is not privileged" rule by construction, because it is not
in a platform schema.

Two honest caveats: the SQLite `AuditWriter` impl carrying it is
`#[cfg(any(test, feature = "test-helpers"))]` (`backend/sqlite/mod.rs:1287`), so
it is not written in a shipped dev build today; and it is a migration-audit
counter, not a fence, with none of Fork C's properties. The point is narrow: the
shape of a future answer is "a column the migration path already writes", not "a
new state table".

---

## 2. Where does the identity half belong? Three answers, and only one is open

### `ceiling` - already homed; the parent has already issued the retraction

Decision 4 (2026-08-27) made the operator ceiling **worker configuration**,
delivered at composition, met with the creator draft once at binding
construction (`design:113`, `sc6:17-22`). SC-6 then deleted, by name, "**the
SQLite ceiling home**, which existed because `__zeroship_admin` is a PostgreSQL
schema" (`sc6:36`), and stated the replacement: "It is replaced by the dev
composition point" (`sc6:168-172`).

The parent proposal already tells SC-2 to drop the column, in one sentence SC-2
has not absorbed (`design:227-228`):

> SC-2's `AuthorityRead` row still lists `ceiling` as a component of the
> authority row it returns; that component goes.

**This also removes `AuthorityRead`'s entire stated reason to exist.** SC-2
justifies the command as follows (`sc2:250-252`):

> `AuthorityRead` exists because SC-6 assigns the dev tier's mid-transaction
> authority read to `op_conn`, and this protocol had no command that could carry
> it.

SC-6 no longer assigns that read; it retracted it (`sc6:124-130`: "Under
decision 4 there is nothing to pin and nothing to re-read: the effective policy
is a field of the binding, identical inside and outside a transaction"). SC-2's
justification is a citation of a section that was retracted the day after SC-2
was written. With `ceiling` gone by decision 4 and `epoch` gone by decision 7,
`AuthorityRead` returns only `incarnation` - and section 2b says that value does
not come from the file either. The command has no payload left.

Measured: `AuthorityRead`, `authority_read` and `AuthorityRow` occur **0 times**
under `crates/`, `libs/` and `sdks/`.

**And a file-resident ceiling would be the wrong shape, not merely a redundant
one.** SC-6's replacement of the privilege-posture arm rests on "Worker
configuration is not tenant-writable by any grant", so the property "holds by
construction" (`sc6:164-166`). A ceiling in the app's own SQLite file is written
by the migration path the developer runs, on a tier where SC-2 itself concedes
"the developer owns the bytes, and no scheme in the file can change that"
(`sc2:338-339`). That is a ceiling the governed party writes. It inverts the one
security property the ceiling has.

### `incarnation` and `deprovisioned_at` - homeless, and with no subject on this tier

Homeless is settled and is not mine to solve: `design:1327-1354`, `sc5:134-140`,
`index:375-387`. The control plane is "the obvious candidate" and explicitly
"not this document's call" (`design:1339-1340`).

What is worth adding is that **SC-2 does not need the answer**, because the
lifecycle Fork C fences does not reach the SQLite tier:

- **The worker refuses SQLite outright.** `worker_rejects_db_url` returns true
  for any SQLite DSN (`crates/zeroship-worker/src/main.rs:140-149`), enforced at
  boot (`:315-331`) with the comment "the authority is the worker's IDENTITY,
  not an env flag". Deploy-pinned isolates and worker-side delayed deprovision -
  two of the three long-lived stale-handle carriers SC-5 enumerates
  (`sc5:422-427`) - exist only in the worker.
- **SQLite's deprovision is a no-op.** `SqliteChangeStream::deprovision` returns
  `Ok(())` and says so: "No-op. Disarming hooks for a session whose app is being
  torn down is not implemented."
  (`crates/zeroship-data-v8/src/backend/sqlite/cdc.rs`).
- **The dev app id is a literal.** `export const DEV_APP_ID = "default"`
  (`packages/vite-plugin/src/gen-types/dev-apply.ts:35`), used by both the migrate
  command (`migrate-dev.ts:125`) and the dev server (`dev-server.ts:411`). On
  this tier same-id recreation is not an exotic operator action - it is
  `rm .zeroship/dev.sqlite && pnpm migrate`, done routinely. An incarnation
  fence keyed on it either fires on every such reset or means nothing.

**And SC-2's version of the token is the exact shape SC-5's acceptance arm is
built to reject.** SC-5 makes the qualification load-bearing: the incarnation "is
qualified by the authority domain `(system_identifier, timeline_id)`, or PITR
simply resurrects an old token ... This is the correction that makes the token
worth adding at all" (`sc5:204-210`), and its arm "must be shown to **fail**
against an unqualified incarnation - otherwise it is testing the token and not
the defence" (`sc5:416-421`). SC-2 concedes it cannot supply the qualification:
"There is no authority domain on this tier ... The dev tier therefore has no
PITR-resurrection defence" (`sc2:335-339`).

So the row would persist precisely the token SC-5 says is not worth adding, on
the tier where the thing it fences does not happen.

### The residue: `DetachApp`'s `expected_incarnation`

`DetachApp(app_id, expected_incarnation)` (`sc2:245`) is the one place SC-2
reads an incarnation for its own purposes. Its stated job is narrow and
process-local: settle outstanding reservations and close both connections
"**before** restore's file swap, so a lock release cannot leave a connection
bound to an obsolete inode" (`sc2:288-289`).

That job needs a **process-local attach generation**, not Fork C's durable,
domain-qualified token. SC-2 already has the concept: its cancel target is
`(reservation, lane, connection generation, command sequence)` (`sc2:165`), and
the implementation carries per-lane connection generations that refuse an
interrupt aimed at a retired connection (`session.rs:269-280`, `:326-346`,
`:1314-1346`). Renaming the parameter to that is a wording change inside SC-2,
not an invented home.

This must be settled with SC-5, which scopes its own no-abort arm to PostgreSQL
*because* of SC-2's `DetachApp` (`sc5:396-405`). The two documents share the
parameter and must agree on what it is.

---

## 3. What breaks if the row is deleted? Measured: nothing

This one is not an argument. **The reservation protocol has already been built
without the row**, in `a21640bf4` ("feat(db): give the sqlite actor two
connections and a reservation protocol") plus five follow-up fixes
(`b2318f0a8`, `2ac2359d3`, `e195e71ea`, `1ee5314c1`, `f2b751c3f`) and one doc
commit (`5cb6368bf`). `session.rs` is 2449 lines and contains:

- `Reserve { reservation }` and reservation-qualified commands, with the actor
  refusing a command whose reservation does not own the lane
  (`session.rs:177-181`, doc at `:171-176`);
- both connections, `BEGIN DEFERRED` wrapping and `busy_timeout=5000`
  (`session.rs:10-12`, `:1242`, `:1628`);
- the interrupt registry with per-lane connection generations
  (`session.rs:269-346`);
- `TerminalIntent` / terminal-outcome machinery and the `is_autocommit`
  classifier (`session.rs:160-165`, `:1605`, `:1806-1817`);
- `Attach`, `VacuumInto`, `ReattachFile`, `Cancel`, `Shutdown`
  (`session.rs:231-262`).

Measured: `__zeroship_state` occurs **0 times** under `crates/` and `libs/`, and
so do `AuthorityRead` / `AuthorityRow`. The protocol runs; the row was never
reached for.

Walking the command list (`sc2:238-246`) without it:

| command | needs the row? |
| --- | --- |
| `Reserve(kind)` | No. Lane ownership is caller-side `Weak` upgrade (`session.rs:550-580`). |
| data commands | No. The reservation id is the check. |
| `AuthorityRead` | **The command is deleted with the row.** Its three components are dead (`epoch`), homed elsewhere (`ceiling`), or not file-resident (`incarnation`). |
| `Settle` / `Release` / `Cancel` | No. Terminal state is the actor's word, classified by `is_autocommit` (`sc2:202-215`). |
| `DetachApp` | Only for the parameter discussed above; the implemented `ReattachFile` already takes none (`session.rs:249-258`). |

Acceptance arms:

- The coherence arm (`sc2:389-409`) loses its first clause and loses nothing,
  because SC-2 already records that the arm "stays green on an implementation
  that does none of the work this document proposes" (`sc2:405-407`). The
  discriminating half - `SQLITE_BUSY_SNAPSHOT` on write upgrade - is unaffected
  and is still owed (`sc2:428-430`).
- The other five arms in `sc2:355-388` never mention the row.

**The one concrete thing that genuinely goes, and why it does not matter.** The
row was the only marker distinguishing "this file exists and has had migrations
applied" from "this file exists and is empty". `ensure_attached` keys
`SCHEMA_NOT_APPLIED` to file **absence** only (`design:2623-2626`), so without
the row an unmigrated-but-present file surfaces as SQLite's `no such table`.

That gap is already filled, better, and outside the data plane.
`reportDevSchemaState` (`packages/vite-plugin/src/dev-server.ts:392-443`) opens the
app file read-only, reads `sqlite_master`, diffs it against the descriptor's
collection list, and names both the missing collections and the fixing command
(`:439-443`). Its own doc comment states the design rule that makes this the
right place: "the dev server starts a runtime; it does not mutate the
developer's database as a side effect of being started" (`:371-381`). It is a
supervisor-side catalog read, which decision 8 does not forbid - decision 8
binds the **data plane** (`design:1014-1015`).

So SC-4's owed arm "a data operation before any apply fails with
`SCHEMA_NOT_APPLIED`" (`sc4:233-234`) should be rewritten against this existing
detector rather than against a row.

---

## Does settling this make SC-2 implementable? Partly. It also exposes one real gate

**What it closes.**

1. SC-2's `UNRESOLVED 2026-08-27` block (`sc2:291-324`), which the document
   itself calls the thing to settle "before implementing SC-2".
2. `AuthorityRead`, its section (`sc2:248-268`), and the stale SC-6 citation
   under it.
3. **An SC-4 dependency, in the closing direction.** SC-2 currently defers
   "which process writes the first stable row on each dev path" to SC-4
   (`sc2:346-351`), and SC-4 accepts it with a larger OWED attached - no row
   shape, no dev authority domain, no incarnation (`sc4:170-187`). Deleting the
   row deletes that whole section of SC-4 and its arm. This is the only place in
   the set where settling one question removes work from two documents.
4. The parent's own instruction at `design:227-228` stops being unexecuted, and
   `design:2635-2636` ("Which object holds the SQLite epoch ... is still open
   there") becomes moot.

**What it exposes. One gate, and it is a decision rather than implementation.**

The per-app-file actor. SC-2 Decision 1 derives admission per
`(runtime_instance_id, app_id)` from "one actor thread per attached app file"
(`sc2:80-86`), and the blockquote at `sc2:88-108` records that the
implementation built the two connections but not the per-app actor. That is
still true, and the code says so in its own words
(`session.rs:538-546`):

> **Scope of the admission key.** SC-1 keys a transaction slot on
> `(runtime_instance_id, app_id)`. One `SqliteSession` serves **every** ATTACHed
> app, so the key here is effectively `(runtime_instance_id, session)`: app B's
> `db.transaction()` is refused while app A holds one. That is narrower than
> SC-1 asks for ... widening it means a `tx_conn` per app, which is a
> connection-count decision nobody has taken.

That is the gate you buy visibility of. It is a live cross-tenant coupling on a
tier that ATTACHes multiple app files (`run_attach_both`, `session.rs:1834-1850`;
`attachments` replay at `:1220`, `:1314-1346`), and the code names it as an
undecided connection-count question rather than a missing implementation.

**Two smaller items, both writable now rather than blocking.**

- The refuse-vs-queue divergence: SQLite refuses immediately
  (`transaction_connection_busy`, `session.rs:563`) where PostgreSQL queues on
  the pool deadline (`session.rs:527-537`). SC-2 flags that neither divergence
  is in `docs/reference/sqlite-divergences.md` and that the same edit deleted
  the sentence documenting the error (`sc2:98-104`). Documentation debt on a
  creator-visible error.
- The three owed arms (`sc2:411-431`): the terminal classifier table, the
  `SQLITE_INTERRUPT` mapping, and `SQLITE_BUSY_SNAPSHOT` on write upgrade. None
  depends on the row; all are writable today.

**Verdict.** Settling the row converts SC-2 from "do not implement, an unsettled
subject" into "implementable, with one open decision named". It does not make
SC-2 free. It buys a clean answer plus a correctly-scoped question, which is
better than the current state where the open question is hidden behind a dead
one.

---

## Argue against this conclusion

The strongest case for keeping the row is not the symmetry argument's mirror
image. It is this:

**"SQLite's failure modes genuinely differ, and the one that matters is in-place
mutation by a second process. On PostgreSQL a migration and a deploy are
coordinated by a pipeline the platform owns (`design:1252`). On SQLite, a
developer runs `pnpm migrate` against a file a live `pnpm dev` isolate is
holding open. Same inode, so SQLite's own change counter and schema cookie make
the new schema visible to the actor's next transaction, while the isolate's
installed descriptor moves only if the HMR channel happens to deliver. That is
real drift, it is invisible, and SC-4's supervised restart does not close it -
SC-4 admits the supervisor 'names no process, no restart trigger and no drain'
(`sc4:115-121`). The row would catch it in one `SELECT`."**

That is the honest case and I want it on the record rather than softened. It is
also the reason the "the descriptor is fixed at construction" answer is not
sufficient on its own: it is true of the worker vector and **false of the dev
vector**, which is the only one SQLite runs in
(`dev-bootstrap/index.ts:182-186`, `context.rs:627-630`). Four things answer it,
and the last is the one that decides.

1. **It is a dev-tier convenience, not a security property.** SC-2 concedes the
   developer owns the bytes (`sc2:337-339`); SC-6 draws the same split
   (`sc6:168-172`), pointing at `docs/reference/auth-dev-tier.md`. A defence
   against the operator of the machine is not a defence.

2. **The channel it would duplicate is already live and already carries more.**
   The dev descriptor is re-applied on HMR and the store overwrites in place
   (`dev-bootstrap/index.ts:55-81`, `:182-186`; `context.rs:627-630`), fed by
   the same command that applies the migrations (`migrate-dev.ts:106-113`). A
   row would deliver "a number moved"; the existing channel delivers the new
   schema. If that channel has a delivery gap, the gap is the thing to fix.

3. **The detector for the residual gap already exists, and is better.**
   `reportDevSchemaState` (`dev-server.ts:392-443`) makes exactly this
   comparison - descriptor collections against `sqlite_master` - names which
   collections are missing, and names the fixing command. Building a worse
   detector inside the data plane, on one backend, is not defence in depth; it
   is a second instrument that disagrees less usefully.

4. **The row's answer is unactionable after decision 8, which is what turns this
   from a trade-off into a mistake.** Fork C defines an epoch mismatch as
   *re-resolve* (`sc5:216-220`). After `632c1d1fa` there is nothing to
   re-resolve to: the descriptor is the sole authority and it comes from the
   artifact, not the database (`design:1014-1015`, `descriptor.rs:1`). So a
   SQLite epoch mismatch can only deny, and only a re-delivered descriptor
   clears the denial - which is SC-4's HMR path or Decision 2's restart
   (`sc4:99-110`). **The correct fix for the drift the objection names is the
   OWED supervisor in SC-4, not a row in SC-2** - and a row would let SC-4's
   owed supervisor stay owed while appearing handled.

There is a fourth answer specific to the shape of the mistake being repeated.
Decision 7 is described in the tree as finding that the surviving epoch row "did
not survive because it was unsafe. It was safe, and it was **redundant** - which
is a failure mode no amount of scrutiny of its privilege posture would have
surfaced" (`design:148-150`). The SQLite row is the same shape one backend over:
its privilege posture is fine (SC-2's argument at `:341-345` that the file is
reachable only through the actor is correct), and it is redundant against an
authority that already answers the question. Keeping it because "SQLite is
different" without checking whether its question is still asked is precisely
the audit decision 7 says nobody performed.

**Where the objection is right and should be preserved.** Its factual core -
that a live SQLite isolate can be overtaken by an in-place migration with
nothing detecting it - is true, is not currently covered by any acceptance arm,
and is stronger than the parent's general "mid-life drift" note
(`design:1313-1325`) because on this tier the second writer is a routine
developer command rather than an operator action outside the normal path. That
belongs in SC-4 as a requirement on the supervisor, and should not be lost when
the row is retracted.

---

## What I could not determine from the tree

1. **Whether decision 7 was intended to reach SQLite.** The commit
   (`390f4b97b`) and the write-up (`design:955-1005`) are about
   `__zeroship_admin`, a PostgreSQL schema. "Reasons that are not
   backend-specific" is SC-2's own gloss (`sc2:322-324`), not the operator's
   words. Everything above argues the row should go on its own merits; it does
   not establish that decision 7 already decided it. **This is an operator
   ratification, not a finding.**

2. **What app id the `zeroship serve` vector uses, and whether either SQLite
   vector can host more than one app.** `DEV_APP_ID = "default"` is the Vite
   path's constant (`dev-apply.ts:35`). The actor is built for many
   (`run_attach_both`, `attachments`, `session.rs:1834-1850`), and
   `session.rs:541-546` treats multi-app ATTACH as the live case. I could not
   find where `zeroship serve` mints its app id. If some vector does host
   several apps with operator-chosen ids, the "no same-id recreation on this
   tier" argument in section 2 weakens - though the Fork C argument stands
   regardless, because the domain qualification is still unavailable
   (`sc2:335-339`).

3. **Whether the dev/`zeroship serve` ceiling source will exist.** SC-6 leaves
   it OWED and warns that its absence plus "failure is denial" denies every
   non-`auto` unmask in dev permanently (`sc6:174-179`), and SC-5 repeats the
   gap (`sc5:79-84`). Deleting `ceiling` from this row does not create that gap
   and does not close it. If the answer turns out to be "there is no dev ceiling
   source", someone will be tempted to reach for a file-resident one; section 2
   is the argument against that, written in advance.

4. **Whether the deferred startup DDL validation feature (`design:1008-1012`)
   will be backend-symmetric.** If it is ever built, it is the correct home for
   the in-place-drift objection above, on both backends. Nothing in the tree
   states whether it is planned as one feature or two, so I could not judge
   whether the SQLite half would subsume `reportDevSchemaState` or sit beside
   it.

5. **SC-1's SQLite admission key still names `incarnation`**
   (`sc1:119`, argued at `:129-136`). That is a live Fork C dependency in a
   different document, and I did not assess whether it survives the same
   subjectlessness argument. It does not block SC-2 - SC-1 says SC-2 "must land
   first for the SQLite arm" (`sc1:183-185`) - but if SC-2's row is retracted,
   somebody should check that cell in the same pass. SC-1's invariant 1 also
   still reads "Epoch mismatch re-resolves" (`sc1:479-481`), which is stale for
   the reason in section 1.

6. **Whether SC-4's supervised restart keeps the HMR descriptor re-apply.** This
   is the one operational risk this recommendation carries and I could not
   resolve it from the tree. Section 1 rests partly on the dev descriptor being
   re-delivered on change (`dev-bootstrap/index.ts:182-186`). SC-4 Decision 2
   rejects that in-place mechanism in favour of a supervised restart and notes
   the in-place path already produces the observable outcome (`sc4:99-113`,
   `:207-214`) - while the supervisor that replaces it is OWED
   (`sc4:115-121`). If the re-apply is deleted before a working restart trigger
   exists, the drift window in the counter-argument becomes real for the first
   time. **The retraction proposed here should not land before someone confirms
   the dev descriptor keeps a delivery path.** I did not measure whether the
   dev server actually watches the generated descriptor and fires that callback;
   I only found the callback's declaration site.

---

## The concrete edit this argues for

For whoever revises SC-2. Stated so the retraction is executable and does not
have to be re-derived:

1. **Delete the section "The SQLite epoch"** (`sc2:291-351`) in full, including
   the `UNRESOLVED` block and the SC-4 handoff. Replace it with a short
   retraction naming decisions 4, 7 and 8 and stating that the descriptor is the
   sole schema authority on this backend and always was
   (`design:1027-1035`, `descriptor.rs:30-32`).
2. **Delete `AuthorityRead` from the command list** (`sc2:242`) and its section
   (`sc2:248-268`). Keep the surviving rule from that section - an authority read
   never rides the data snapshot (Fork B, `design:207-212`) - as a one-line note
   that this protocol currently has no such read, since `design:213-221` says the
   clause has lost its only client.
3. **Rename `DetachApp`'s second parameter** to the process-local attach or
   connection generation SC-2 already uses at `:165` and the code implements at
   `session.rs:269-346`. Co-ordinate with SC-5 (`sc5:396-405`).
4. **Drop the epoch clause from the coherence arm** (`sc2:389-391`) and keep the
   `SQLITE_BUSY_SNAPSHOT` arm as owed (`sc2:428-430`).
5. **Delete SC-4's "The SQLite dev epoch"** (`sc4:170-187`) and rewrite its arm
   (`sc4:233-234`) against `reportDevSchemaState`
   (`dev-server.ts:392-443`), which already implements it.
6. **Do not touch** SC-6's flip or the `platform-cli` question. Neither is
   reached by any of the above.
