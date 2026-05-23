# Test-coverage review — 2026-05-24 round 2

**Reviewer**: pilot-cron Part 1 (read-only)
**Worktree HEAD**: 09dfd902
**Lens**: test-coverage
**Last reviewed (this lens)**: 2026-05-23 r1 (same file family)

## Summary

8 findings (3 CRITICAL, 3 IMPORTANT, 2 MINOR). r1's B15 wiring gap is
now pinned (`crates/sandbox/src/backend/nomad_ch.rs:4169-4289` +
`tests/sandbox_pg_e2e.rs:3085-3210`). The other two r1 CRITICALs — the
`read_snapshot_row` decode tail and the wrapper script — remain
**open**. Three new CRITICAL gaps surface from the new-reviewer
findings (security A1/A2, concurrency C1) and the new T6
`ControllerIdleSnapshotter` lacks any non-pg test.

## CRITICAL

### 1. AEAD not wired in production — no test asserts the store is wrapped (security r1 finding #1)
`crates/sandbox/src/lib.rs:316-345` builds either bare
`LocalDiskSnapshotStore` or `TieredSnapshotStore<LocalDisk, Gcs>` and
**never** wraps with `AeadSnapshotStore`. `config.snapshot_root_kek_path`
is read (`config.rs:163,685`) and logged (`lib.rs:356`) but never
threaded to a store constructor. Yet `snapshot_handler.rs:358` records
`Some("v1")` for the `snapshot_aead_dek_id` column — pg attestation
lies. No test in the tree asserts the production-build store IS
AEAD-wrapped when `snapshot_root_kek_path = Some(...)`. The dedicated
unit tests (`snapshot_aead.rs:728/816/848`) construct
`AeadSnapshotStore::new(...)` directly — they never exercise
`AppState::from_config` with a KEK path set.
**Fix**: add a `from_config_uses_aead_when_kek_set` test that builds
`AppState` with a temp-file KEK + `snapshot_enabled=true`, then
downcasts `state.snapshot_store` (or invokes `put` and asserts the
returned `meta.ch_version` contains `+aead-cc20p1305`, which only the
AEAD layer at `snapshot_aead.rs:613` stamps).

### 2. GCS `verify()` discards `expected_sha256` — no tampered-blob test (security r1 finding #2)
`crates/sandbox/src/snapshot_store_gcs.rs:660-682` does
`let _ = expected_sha256;` and only HEADs the three files. There is
**no test** that calls `GcsSnapshotStore::verify` with a wrong sha and
asserts the failure: the GCS test module
(`snapshot_store_gcs.rs:1085-1232`) covers tier put/get/delete + url
encoding only. `verify_with_wrong_sha256_*` exists for
`LocalDiskSnapshotStore` (`snapshot_store.rs:341`) — symmetric coverage
is missing. Cannot be unit-tested cheaply without a fake-GCS HTTP
mock, but a `MockGcsClient`-style stub returning canned `x-goog-hash`
HEAD responses would pin "verify SHOULD compare and reject" the day
the bug is fixed.
**Fix**: extract the HEAD-and-compare logic behind a trait the unit
test can drive with a recorded fixture, OR add a `#[ignore]`'d
GCS-live test that uploads a known blob and verifies with wrong sha
expecting `ChecksumMismatch`. Today even a code change reverting the
sha-comparison would not light up any red test.

### 3. T6 `ControllerIdleSnapshotter` has zero non-pg coverage
`crates/sandbox/src/sweep.rs:292-383` is a new 90-line production
adapter that resolves a backend handle, builds a `ResolvedSourceVmOps`,
calls `snapshot_handler::snapshot_sandbox`, and post-teardowns via
`backend.teardown_source_for_snapshot`. The two pg-gated tests
(`sandbox_pg_e2e.rs:2755, 2795`) use `RecordingIdleSnapshotter`, never
the production type. `sweep.rs::unit_tests` (line 558-582) contains
exactly one test (`recovery_target_pins_proposal_table`). Neither
covers `ControllerIdleSnapshotter::snapshot_one`'s wiring-missing
guard (line 319), the `StateMismatch → debug` swallow (line 368-378),
nor the teardown-failure warn path (line 359-365). A regression that
e.g. inverts the `Ok/Err` arms of the `lookup_source_vm_ops` match
(line 325-328) would pass every test in the tree.
**Fix**: factor the wiring-trio guard + outcome-mapping into a pure
helper `fn classify_snapshot_outcome(...)` and add three table-driven
tests. The lookup + handler call can be exercised behind a
`MockBackend` (the `backend::Backend` enum already has stub variants
used by existing tests).

## IMPORTANT

### 4. r1 CRITICAL #2 still open — `read_snapshot_row` decode tail has no non-pg test
`crates/sandbox/src/restore_handler.rs:242-280`. r1 proposed
extracting the row-decode tail into `decode_snapshot_row(row: &Row)`
with three unit cases (missing-vm_index → `Internal`; wrong-sha-length
→ `Internal`; well-formed → `Ok`). No such extraction or test landed
in this round.

### 5. r1 CRITICAL #3 still open — wrapper script has zero in-repo test coverage
`crates/sandbox/scripts/nomad-vm-wrapper.sh` (421 LOC) plus
`scripts/init.sh` (141 LOC) remain entirely unexercised by any Rust /
bats / shellcheck driver in the tree. Five of fifteen cluster bugs
(#6, #10, #11, #14a, #15) bottom out in the wrapper; the wrapper sed
rewrite is the security r1 finding #5 RCE-vector. No `tests/wrapper_lint.rs`
runner, no `bash -n` smoke, no `shellcheck` invocation. The new
`stop_preserving_state` Rust unit test (`backend/nomad_ch.rs:4169`)
pins the Rust side of B15 but the wrapper's
`[ ! -f $ZSBX_WORKSPACE_IMG ]` gate at wrapper line 222 — the failure
surface itself — is still untested. **One-line drop-in**: a
`tests/wrapper_lint.rs` with `Command::new("bash").arg("-n").arg(...)`
+ `Command::new("shellcheck").arg("--severity=error")` would catch
syntactic regressions for free; both binaries are commonly available
on dev/CI hosts.

### 6. r1 IMPORTANT #2 still open — `run_idle_eviction_once` + `run_transient_takeover_once` have no non-pg tests
The "feature disabled → no-op" path was added pg-gated
(`sandbox_pg_e2e.rs:2770`) but the equivalent check at
`sweep.rs:422-426` (threshold_secs <= 0, snapshot_enabled = false,
database is None) is pure-Rust and trivially testable without pg via
a no-op `IdleSnapshotter` fixture and a `build_state` helper that
sets `database = None`. No such test exists.

### 7. The new async `IdleSnapshotter::snapshot_one` trait is testable without compio time-mocking but lacks a recording Fake reachable from non-pg tests
`crates/sandbox/src/sweep.rs:249-281` defines the trait + the
`RecordingIdleSnapshotter` Fake (which records `Vec<Uuid>` + has an
`AtomicBool fail` knob). The Fake is doc-hidden but `pub`, so it IS
reachable from `#[cfg(test)] mod` siblings — yet no non-pg sibling
uses it. The "snapshotter failed → sweep continues" branch
(behavioural expectation from `run_idle_eviction_once`) has no
coverage outside the pg-gated path. Wire one in-module test:
construct `RecordingIdleSnapshotter { fail: true, .. }`, feed it
through a `run_idle_eviction_once_using<MockDb>` helper, assert
`attempted.len() > 0 && the next-iteration retry still selects the
row`.

## MINOR

### 8. Cluster smoke is the only thing actually validating restore wake end-to-end — and there are local-only alternatives
Today the wake path is verified by (a) `sandbox_pg_e2e` for the pg
state machine, (b) the cluster smoke for the wrapper + cloud-hypervisor
+ Nomad path. The five bugs that made bug #16 sting (#11 virtio-fs
pivot, #14a stage-empty, #14b NO-CARRIER, #15 workspace.img missing,
#16 bash-not-installed-on-host) all reproduce on a developer machine
with cloud-hypervisor + a single-node Nomad agent in docker-compose
(see `docs/runbooks/sandbox-nomad-ch.md`). A `tests/wake_local.sh`
that boots ch-in-docker, drives the controller through a single
snapshot+wake cycle, and asserts `/livez` would catch bugs #11, #14,
#15 in <60s of dev-machine work without a GCP smoke. Not in the tree.

### 9. r1 MINOR #1 still open — wrapper/controller MAC+TAP constants drift
`crates/sandbox/src/restore_handler.rs:421-427` derives `12:34:56:78:9b:<index>`
and `zsbx-nm-<index>`. The wrapper at
`scripts/nomad-vm-wrapper.sh` encodes the same format independently.
No test reads the wrapper via `include_str!` and asserts the literal
matches `derive_tap(i)`. Cheap insurance against the next bug-#6-class
drift; not landed.

---

## What's pinned vs. open from r1

| r1 finding | Status |
| --- | --- |
| C1 (B15 wiring — `stop_preserving_state` is dead code) | **Pinned** by `backend/nomad_ch.rs:4169-4289` + `tests/sandbox_pg_e2e.rs:3085-3210`. |
| C2 (`read_snapshot_row` decode tail no non-pg test) | **Open** (this round's #4). |
| C3 (wrapper has zero in-repo test coverage) | **Open** (this round's #5). |
| I1 (`err(500, ...)` envelope drift) | **Open**; api-surface r1 finding #1 re-flags. |
| I2 (sweep async entrypoints have no non-pg tests) | **Open** (this round's #6). |
| I3 (`fs[].socket` virtio-fs fixtures stale) | **Open**, not re-checked this round. |
| I4 (restore-handler rollback not unit-tested) | **Open**; not re-checked. |
| I5 (snapshot-handler rollback not unit-tested) | **Open**; not re-checked. |
| M1 (MAC+TAP wrapper drift) | **Open** (this round's #9). |
| M2 (GCS retry / AEAD time-of-put) | **Open**; not re-checked. |
| M3 (livez-poll integration not tested) | **Open**; not re-checked. |
