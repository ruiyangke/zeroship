# ADR - platform migration journal versions stay derived from sorted-filename ordinal

- **Date:** 2026-08-20
- **Status:** Accepted
- **References:** `crates/zeroship-migrate-adapter/src/platform.rs`,
  `crates/zeroship-migrate-adapter/tests/platform_migrate.rs`,
  `db/released_migrations.tsv`, `deploy/scripts/deploy-remote.sh`,
  `third_party/zero-migrate/crates/zeroship-migrate-ir/src/migration.rs`

## Context

### What the derivation actually is

`platform.rs` stamps every lowered step of every `db/migrations-ts/*.ts` file
with a journal version computed from the file's ORDINAL in sorted-filename
order. `discover_ts_files` sorts on the file name, `run_platform_migrations`
enumerates the result, and the loop index is the ordinal:

```rust
const FILE_VERSION_STRIDE: u64 = 1 << 20;

fn restamp_stable_versions(
    lowered: &mut zero_migrate::render::lower::LoweredArtifact,
    file_ordinal: usize,
    version_prefix: &str,
) -> Result<(), PlatformMigrateError> {
    let base = (file_ordinal as u64) * FILE_VERSION_STRIDE;
    ...
            PlanStep::Ddl(m) => {
                let version = base + step_index as u64;
```

called as

```rust
let version_prefix = version_prefix_from_filename(&migration.path);
restamp_stable_versions(&mut lowered, index, &version_prefix)?;
```

`AGENTS.md` is correct: it is the ordinal, not the date prefix. The date prefix
IS parsed, by `version_prefix_from_filename`, but the value reaches only a
`debug_assert!` message and an error label - never the version. The doc comment
on that function claimed the opposite ("the deterministic identity every lowered
step of the file is re-stamped from"); it is corrected in the same commit as
this ADR.

### What the two tables store

There are two records, and confusing them is the main source of wrong intuition
here.

- **The engine journal** (`zeroship_migrations.schema_migrations`) is keyed on an
  opaque `mig_<base62 uuidv7>` string. `migration_id_for_version(v)` packs the
  numeric `v` into the UUID's high 48 bits, so string order equals numeric order.
  Nothing maps a journal row back to a filename; a row records a version, a
  checksum over `up`/`down`/`flags`/`owner_app`/`depends_on`, and a phase.
- **The completion ledger** (`zeroship_migrations.platform_migration_files`) is
  keyed on FILENAME and stores a sha256 of the file's source bytes.
  `db/released_migrations.tsv` is a verbatim dump of it: 34 rows against 35 files
  in the tree today.

The ledger, not the journal, decides whether a file runs. A file already in the
ledger with a matching checksum is skipped before authoring, lowering, or any
engine call; a file in the ledger whose bytes changed raises `ChecksumMismatch`.
The ordinal-derived version therefore matters only for files that are about to be
applied.

### The incident

On 2026-08-20 `20260819000000_app_egress_rules.ts` landed while the deployed
journal's highest version was ordinal 33. In the merged corpus ordinal 33 was the
new file, so the next roll would have compared a recorded checksum against a
different file's body and aborted with `ChecksumDrift`. The abort names the
version, an opaque `mig_...` id, wrapped in the filename of the file being
applied - which is not the file that moved, and says nothing about ordering. The
file was renamed to `20260820000100_app_egress_rules.ts`.

Two guards landed the same day and both are live:

- `unreleased_migrations_sort_after_every_released_one` in
  `crates/zeroship-migrate-adapter/tests/platform_migrate.rs`. DB-free, and CI
  runs it: `.github/workflows/ci.yml` has an unconditional
  `cargo test -p zeroship-migrate-adapter --features platform-cli --test platform_migrate`
  step, added precisely because the whole module is `#[cfg(feature =
  "platform-cli")]` and `cargo test --workspace` compiled it away.
- `released_ledger_misordered` in `deploy/scripts/deploy-remote.sh`, run against
  the real journal before the roll.

## Decision

**Keep the sorted-filename ordinal derivation.** Do not move to a date-prefix
version, a declared id, or a content-addressed one.

## Rationale

### 1. The ordering rule is intrinsic to the corpus, not to the derivation

The runner applies files in filename order and skips any file the ledger already
records. On a deployed database a newly inserted mid-corpus file therefore lands
AFTER every file that sorts later than it, because those already ran; on a fresh
database it lands in sorted position. The two databases converge only if the
inserted migration commutes with everything it jumped.

That divergence is a property of "a linear once-applied corpus replayed in
filename order". Every candidate derivation has it. Changing the numbering does
not retire the rule "a new migration must sort after every applied one", so no
candidate buys the simplification that would justify it.

### 2. The ordinal derivation is the only candidate that DETECTS a violation

Under the ordinal scheme a mid-corpus insert collides with a version the journal
already holds, and the run aborts. That abort is currently accidental and badly
worded, but it is a real last line of defence behind two guards that are both
bounded to ONE deployment's journal snapshot: a second cluster further behind, or
an operator who ignores `deploy-remote.sh`'s non-zero exit, is invisible to both.

Under any position-independent scheme the same insert gets a fresh unused version,
applies cleanly, and produces a database whose schema silently differs from a
fresh one. Trading a loud abort for a silent divergence is a strict regression,
and it is the outcome candidates (b), (c) and (d) all share.

### 3. This is not "defer because pre-launch"

`AGENTS.md` is explicit that a shape a launch would freeze should be fixed now.
This decision does not invoke that exemption and does not rest on simplicity.
The claim is that the ordinal derivation IS the end state: it is the only one of
the four that turns the corpus's intrinsic ordering requirement into something a
machine can refuse. What is genuinely wrong today is the wording of the refusal,
and that is fixable without touching the derivation (see "Not settled here").

## Alternatives considered

### (b) Derive the version from the filename's date prefix

Two sub-shapes, both rejected.

**(b1) The 14-digit `YYYYMMDDHHMMSS` prefix as the numeric base, plus the step
index.** Refuted by measurement. The smallest gap between adjacent prefixes in
the corpus is 100 (`20260702000100` to `20260702000200`, and seven more pairs in
that run), so a file may occupy at most 100 versions before it overruns the next
file's base. `20260702000600_constraints_indexes_fks.ts` contains 196 top-level
DSL calls (counted in the source; the lowered step count is at least that if each
op lowers to at least one step, and `platform.rs`'s own stride comment states the
largest platform file lowers "a few hundred" steps). The scheme collides on the
corpus as it stands.

**(b2) The prefix converted to a Unix millisecond timestamp as the base.** This
is what `MigrationId`'s 48-bit field is actually for, the gaps become 60000 ms or
more, and it is adoptable without touching an applied file: the ledger skip is
filename-keyed so applied files never re-derive a version, and a 2026 millisecond
timestamp exceeds 1.76e12 while the entire ordinal-derived space in use is below
`35 * 2^20` = 36700160, so old and new versions cannot collide. It is expressible.
It is rejected on rationale 2 - it makes a mid-corpus insert apply silently - and
because it strands the journal with two numbering schemes while requiring
`journal_state_by_file` and `is_complete_legacy_range` to be deleted or reworked.

### (c) An explicit declared id in each migration's source

Same silent-insert failure as (b2), plus a new one the others do not have: the id
becomes editable text inside an applied file, so a typo is both a version change
and a `ChecksumMismatch` on the file's bytes, and the two failures report
separately. It also adds a uniqueness obligation across the corpus that today is
free, since filenames are unique by the filesystem.

### (d) Content-addressed or DAG-based

The vendored engine already offers both halves and neither fits.

- `MigrationId::derive(tag, seed)` is a deterministic content hash, but it
  deliberately stamps `0xFF` into all six high bytes so that derived ids can
  never collide with versioned ones. It is therefore NOT order-preserving, and
  `platform.rs`'s own comment records why that matters: the engine runs a pending
  batch in ascending version order, so hash-ordered versions would reshuffle a
  file's steps and break intra-file dependencies such as a table CREATE before
  the CHECK that references it.
- `depends_on` is a real DAG and `order_pending` topologically sorts on it, but
  the platform runner only remaps edges WITHIN one file. Making cross-file edges
  explicit would replace a filename convention with a hand-maintained dependency
  graph over 35 files, and would still not remove the apply-order divergence in
  rationale 1, because the ledger skip is per file regardless of edges.

## The deployed-database constraint

Recorded because it is what a future revisit will need, and because it is not the
reason for this decision.

Changing the derivation is NOT blocked by the 34 applied rows. Applied files are
skipped on filename plus checksum before any version is derived, so their journal
rows are never recomputed and no applied file needs editing. A change would take
effect for new files only, and any scheme whose version space is disjoint from
`[0, 35 * 2^20)` cannot collide with what is recorded. Two consequences would have
to be handled in the same change: `journal_state_by_file` maps a journal version
back to a file ordinal by dividing by `FILE_VERSION_STRIDE` and would stop
resolving, and the `report.skipped` accounting that reads it would go empty.

## Not settled here

The abort message is a separate defect and is NOT fixed by this decision. Today a
version collision surfaces as `ChecksumDrift` carrying an opaque `mig_...` id,
wrapped in `PlatformMigrateError::Apply` under the filename of the file being
applied, which is the file that moved into the slot rather than the file that
owns it. `run_platform_migrations` already holds everything needed to say so
precisely: it reads the journal, and it computes the exact `file_versions` set
the file is about to claim. Intersecting the two before the engine call and
refusing with a dedicated error that names both files, and says "rename the
undeployed file so it sorts last", converts the accidental backstop this ADR
relies on into a deliberate one. That work is tracked separately.
