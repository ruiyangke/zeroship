# sandbox-snapshot-restore — Deferred Backlog

Auto-managed by the pilot-cron-worker on `feat/sandbox-snapshot-restore`. Each cron fire reads this file, picks 1-2 actionable items, lands a fix per logical commit, and removes the entry in the same commit. Findings whose blocker still stands stay listed with an updated "last considered" line.

Last seeded: 2026-05-22 (post bug-#13 cluster smoke; cluster torn down).
Last updated: 2026-05-23 (cycle r4: B18 CLOSED via shared `Arc<Mutex<VmIndexAllocator>>`; R3-Q1+R3-T1 closed in code; A4 helper landed but call sites pending; new B19 next blocker for B-SLO; 7 r3/r4 reviewer reports added).
Branch HEAD at seed: `fce3e208`.
Branch HEAD at last update: `469e22c8` (A4 helper at `469e22c8`; B18 at `b4ddb98b`; R3-T1 at `f6497590`; R3-Q1 at `a11ccb2d`; A7 at `a9e568a2`; B17 at `dec489a1`).
Worktree: `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`.

---

## CRITICAL (open blockers on Phase B cluster validation)

### [B16] (RESOLVED 2026-05-24 r2) NixOS-built controller binary unrunnable on GCE Ubuntu workers
- **Status**: **CLOSED**. Verified fix via Docker cross-build (Option A from the action plan) in `rust:slim-bookworm` (note: brief said `rust:bookworm-slim`; correct Docker Hub tag is `slim-bookworm`). v13 binary uploaded to `gs://suger-dev-zsbx-artifacts/zeroship-sandbox.snapshot-v13` with portable interp `/lib64/ld-linux-x86-64.so.2`. Controller starts and serves `/livez` on GCE Ubuntu. See cluster review Appendix A for full transcript.
- **Original symptom**: `zsbx-ctl.service` exits 203/EXEC immediately; `/usr/local/bin/zeroship-sandbox: cannot execute: required file not found` (kernel's misleading text for missing PT_INTERP).
- **Original root cause**: `readelf -p .interp target/release/zeroship-sandbox` → `/nix/store/jms7zxzm7w1whczwny5m3gkgdjghmi2r-glibc-2.42-51/lib/ld-linux-x86-64.so.2`. The local NixOS-built binary's dynamic linker path is absent on GCE Ubuntu.
- **Pre-flight gate (still recommended)**: add `readelf -l target/release/zeroship-sandbox | grep INTERP` to `provision-gcp-cluster.sh` that REFUSES upload if the interp path contains `/nix/`. Cheap insurance for future cycles.
- **B14a/B14b status**: **CLOSED** (see below). Wake path now exercised end-to-end after B17 fix.

### [B17] (CLOSED 2026-05-24 r4) Restored VM agent unreachable post-CH-restore — `No route to host`
- **Status**: **CLOSED**. Root cause: CH `--restore` brings the VM back in a paused state; without `ch-remote resume` the vCPUs never run and the guest's virtio-net never replies to ARP, surfacing as EHOSTUNREACH. Fix: poll the CH API socket post-spawn with `ch-remote ping`, then issue `ch-remote resume`. Cluster smoke c=1 PASS (wake 200 OK in 9.5s); c=4 PASS for all 5 cycles that reached snapshot (5/5 wake). The 11/16 c=4 create failures are a separate bug (#18 below), not a wake regression.
- **Hypothesis history (refuted)**: tap/bridge attachment — there is no bridge in this architecture; the model is /30-per-tap with host as gateway. The wrapper's pre-branch tap-up sequence is identical for cold-boot and restore; the missing step was vCPU resume, not network plumbing.
- **Evidence**: wrapper stderr captured `tap … <NO-CARRIER>` before resume, `tap … <LOWER_UP>` after resume — exactly matches "vCPUs were paused, the virtio-net device was attached but no traffic was flowing".
- **Files**: `crates/sandbox/scripts/nomad-vm-wrapper.sh` (lines 366-419, restore branch).

### [B14a] (CLOSED 2026-05-24 r4) snap-stage `memory-ranges` absent at wake time
- **Status**: **CLOSED**. Cluster smoke shows wrapper restore-branch executes the full file-staging path (`ls -la $ZSBX_RESTORE_FROM` reports memory-ranges=1073741824, config.json=3813, state.json=102535) and proceeds to CH `--restore` successfully. No `FATAL: ZSBX_RESTORE_FROM=… missing` events observed across c=4.

### [B14b] (CLOSED 2026-05-24 r4) tap `NO-CARRIER` after CH `--restore`
- **Status**: **CLOSED**. The NO-CARRIER was a real second-order observation of the B17 root cause: CH `--restore` attaches the tap but the VM is paused, so no LOWER_UP. Post-resume the tap transitions to `<BROADCAST,MULTICAST,UP,LOWER_UP>` as expected. The speculative tap-up retry loop at lines 414-418 stays as belt-and-braces.

### [B18] (CLOSED 2026-05-23 B18-fixer cycle) Stale controller pubkey on VM slot reuse — create-side 401
- **Status**: **CLOSED**. Root cause was NOT in-VM stickiness; the init.sh path was already correct. Actual bug: two separate `vm_index` allocators (NomadCHBackend's `vm_index_allocator` vs RealRestoreBackend's private `VmIndexReservations`) — the wake path reserved slots into a private map invisible to the create-side allocator, so a subsequent create handed the same tap/IP to a fresh sandbox that collided with the live restored VM on that slot. The 401 surfaced because `/version` was answered by the **old** (restored) agent verifying a different signing-pubkey. Fix: share the `Arc<Mutex<VmIndexAllocator>>` between both backends. Two regression tests added. Cluster c=4 verification: 11/16 stale-pubkey 401s pre-fix → **0/16 post-fix**.
- **Cluster evidence**: `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-24-r2.md` Appendix B.

### [B19] Wake path doesn't register restored VM in backend state map (CRITICAL, open)
- **Source**: cluster smoke c=4 2026-05-23 (B18-fixer cycle, see `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-24-r2.md` Appendix B); previously observed in r2 c=1 (§ "Side observations") and r1 appendix.
- **Symptom**: After `restore_handler::do_restore_inner` returns Ok and the restored VM is alive on its agent_url, the sandbox is **not** inserted into `NomadCHBackend::state`. Any subsequent operation — `exec`, `read_file`, `stop`, `delete` — looks up via `NomadCHBackend::sandbox_keys` / `state.read().get(&id)` and hits the `"sandbox not found in nomad-ch backend"` branch.
  - `exec` post-wake: 500 `sandbox not found in nomad-ch backend`.
  - `stop` / `delete` post-wake: 404 `sandbox not found` (handler-side check).
  - **vm_index slot leak across the controller's uptime** — `stop_inner` returns Ok-idempotent without calling `vm_index_allocator.release()`. With B18 closed, this surfaces as `vm-index allocator exhausted (floor=1, ceil=12)` after ~12 successful wakes per controller boot (the test reached it at 10 wakes with c=4 × 4 stress).
- **Reproducer**: provision 1+1, run any c≥1 / cycles≥1 stress with `do_wake=True && do_stop=True`. Wake returns 200; stop returns 404. Or directly: `curl -X DELETE /sandboxes/<sbx>` after a successful wake.
- **Files**:
  - `crates/sandbox/src/restore_handler.rs` `do_restore_inner` step 7+ — needs to install the per-sandbox record into `NomadCHBackend::state` after `wait_for_livez` Ok.
  - `crates/sandbox/src/backend/nomad_ch.rs` — provides `restore_from_pg_and_sealed` (used by controller-restart restart-restore) which already does the install; could be reused or factored into a new `Backend::register_restored(sandbox_id, vm_index, signing_key_bytes, agent_url, user_id)` method.
- **Fix shape** (two options, pick one):
  - (a) New `Backend::register_restored(...)` method that takes the explicit `(sandbox_id, vm_index, signing_key_bytes, agent_url, user_id)` tuple and does the same `state.write().insert(...)` `NomadCHBackend::create` does. Call from `do_restore_inner` after `wait_for_livez`.
  - (b) Re-use `restore_from_pg_and_sealed` (pg → row + sealed → install + probe). The probe is redundant (we already know the agent is live, having just called `wait_for_livez`), but the code path is established and tested.
- **Note**: this bug was latent pre-B18 (the c=4 stress never reached enough successful wakes to exhaust the pool — the 401 race blocked at cycle ~6). Now that B18 is closed, **B19 is the next blocker on the snapshot/restore path's production usability**.

### [A1] AEAD never wraps prod snapshot store (CRITICAL, security-r1)
- **Source**: 2026-05-24 security review (also flagged by arch-r1)
- **File**: `crates/sandbox/src/lib.rs:316-345`
- **Symptom**: production builds bare `LocalDiskSnapshotStore` or `TieredSnapshotStore<LocalDisk, Gcs>`; `AeadSnapshotStore` never composed. `snapshot_handler.rs:358` still stamps `snapshot_aead_dek_id="v1"` into pg, so operators see "encrypted" in the audit trail while guest RAM hits GCS in plaintext.
- **Action**: wrap the inner store in `AeadSnapshotStore` if `SANDBOX_SNAPSHOT_AEAD_ENABLED=1` (or unconditionally for prod). Add a startup log line stating the effective AEAD posture. Add an integration test that asserts get/put roundtrip through AEAD layer.

### [A3] 1 GB sync I/O on compio worker (CRITICAL, perf-r1 + concurrency-r1)
- **Source**: 2026-05-24 performance + concurrency reviews
- **Files**:
  - `crates/sandbox/src/snapshot_handler.rs:316-373` — sync 1 GB SHA + AEAD + rename on compio worker (violates trait's own contract in `snapshot_store.rs:95`)
  - `crates/sandbox/src/restore_handler.rs:286-412,1088,1121` — `store.get` does two full 1 GB passes (verify + AEAD decrypt) + `std::thread::sleep` in `wait_for_alloc_running_blocking` / `wait_for_livez_blocking`
  - `crates/sandbox/src/snapshot_handler.rs:335-338` — `ChRemoteClient::pause/snapshot` sync from async handler
- **Symptom**: stalls a single-threaded compio worker for the full SHA-256 + AEAD + GCS roundtrip. Up to 90s blocking on ureq + Command + std::fs.
- **Action**: wrap all `SnapshotStore::put`/`get` calls + `ChRemoteClient` calls in `compio::runtime::spawn_blocking` (pattern already used in `persist.rs:641,656,676`). Replace `std::thread::sleep` with `compio::time::sleep`.

### [A4] (CLOSED 2026-05-23 cycle r4) HTTP error envelope §10.0 — fully migrated
- **Status**: **CLOSED**. A4 fixer reconvened late and landed 4 more commits after the pilot's r4 artifacts commit: `64db0d30` admin_handlers, `c0296c76` preview, `5330acd9` preview-share, `2928d5ae` wire-shape tests. Adherence: **8/26 (31%) → 26/26 (100%)**. All wire error responses now funnel through `crates/sandbox/src/error_envelope.rs::ErrorEnvelope` / `error_response()`. 21 new tests pin the shape at HEAD `2928d5ae`. Test count: 238 → 266 (+28).

### [A7] `SandboxConfig.token` is `pub` — last credential field exposure (CRITICAL, A6b-fixer spotted)
- **Source**: 2026-05-24 A6b-fixer report (`2380605e`)
- **File**: `crates/sandbox/src/config.rs:19` — `pub token: ApiToken` on `SandboxConfig`
- **Symptom**: with A6b closing the 5 `AppState` fields, the last creator-side credential field that still allows external clobber lives on the embedded `SandboxConfig`. `ApiToken` is the bearer for creator-facing endpoints; swap = bypass-auth.
- **Action**: same shape as A5/A6/A6b — `pub(crate)`-restrict the field + add `with_token(...)` builder on `SandboxConfig`. Add a setter test mirroring `admin_token_setter_tests`. Migrate any out-of-crate write-sites (likely test-only).

### [C3] `do_restore_inner` is cancel-unsafe — wedge on future-drop (CRITICAL, concurrency-r2)
- **Source**: 2026-05-24 concurrency-r2
- **File**: `crates/sandbox/src/restore_handler.rs:286-412`
- **Symptom**: if the surrounding handler future drops between `submit_restore_job` success and final `update_sandbox_status(Running)` await, the sandbox row wedges in `Restoring` state with `vm_index` leaked. The only recovery path is the transient-takeover sweep — **which is dead code per C1** ([C1] in this file). Two open critical issues compound: cancel-unsafe restore + no recovery sweep = permanent wedge.
- **Action**: wrap the post-`submit_restore_job` section in a `pin_project` / scope guard that, on drop without success, marks the row `RestoringAborted` (or rolls back to `Snapshotted`) before yielding. Compio's cancellation semantics + a defer/scope-guard pattern from `crates/sandbox/src/admin_handlers.rs::with_lease` (if it exists; otherwise introduce).

### [A2b] `verify` re-stream is 1 GB GCS egress on L1 eviction (CRITICAL, performance-r2)
- **Source**: 2026-05-24 performance-r2 (a side-effect of the A2 fix at `f32507ce`)
- **File**: `crates/sandbox/src/snapshot_store_gcs.rs:586-611,920-930`
- **Symptom**: A2 fix made `verify` re-stream the whole artifact and recompute SHA. Fine while L1 is warm. But `TieredSnapshotStore::verify` falls through to L2 the moment L1 is evicted, exposing operators to a sustained 1 GB GCS egress + a SHA-bound core whenever a periodic verify sweep lands on cold rows.
- **Action**: add a `verify_metadata_only(&self, expected_sha256: &Hash)` fast-path that compares against the `x-goog-meta-sha256` header we set on put. The full re-stream stays as the "deep verify" mode. Periodic sweeps use fast-path; integrity audits (manual) use deep.

### [W1] Wrapper unanchored `sed` rewrite of attacker-influenceable `config.json` (CRITICAL, security-r2 survived 2 rounds)
- **Source**: 2026-05-24 security-r2 (also flagged in r1)
- **File**: `crates/sandbox/scripts/nomad-vm-wrapper.sh:359`
- **Symptom**: `sed -i "s|...|...|" config.json` where the substitution pattern includes user-influenceable fields. Unanchored, no escaping of `&`/`/`/`\`. With A1 still open (snapshot plaintext on GCS), an attacker with bucket-write could substitute a config.json whose sed-target field contains sed metacharacters, achieving code execution as raw_exec root. (Currently dormant because A1 is locally-mitigated by the worker-local L1 — but A1 will close to AEAD prod-wrap, which doesn't fix the wrapper sink.)
- **Action**: replace `sed` with a Python or `jq`-based rewrite that validates each field is a JSON string (not metacharacter-bearing). If `jq` isn't in the rootfs (likely), use a small inline Python invocation (`/usr/bin/python3 -c '...'`). Or: switch to a Rust pre-stage step in the controller (the controller already does some path rewriting in `restore_handler::rewrite_config_json`).

### [C1] Lease-takeover sweep is dead code (CRITICAL, concurrency-r1 + arch-r1)
- **Source**: 2026-05-24 concurrency review (corroborates 2026-05-23 architecture-r1)
- **Files**: `crates/sandbox/src/db.rs:2305` (`update_lessee` — zero callers); `crates/sandbox/src/db.rs:1712-1724` (`update_sandbox_status` never sets `lessee_updated_at`); `crates/sandbox/src/db.rs:2361` (sweep query filter `WHERE lessee_updated_at IS NOT NULL` excludes every real transient row)
- **Symptom**: §6.1 crash recovery never fires. Under a controller crash mid-Snapshotting/Restoring, the sandbox row is stuck in transient state forever.
- **Action**: either (a) wire `update_lessee` into every state transition that crosses transient boundaries OR (b) remove the dead code + redesign §6.1 around `updated_at` timestamps with a separate `transient_since` column.

---

## IMPORTANT (round-r3 reviewers — structural decay + T6/T7 regressions)

### [R3-A1] Backend enum masquerades as trait — 4 methods return Err for 2/3 variants (IMPORTANT, arch-r3)
- **File**: `crates/sandbox/src/backend/mod.rs:166-497`
- **Symptom**: `Backend` enum has 19 methods; four (`lookup_source_vm_ops`, `teardown_source_for_snapshot`, `restore_from_sealed`, `restore_from_pg_and_sealed`) return `Err("backend X doesn't support …")` for two of three variants. Compile-time-checkable design hole.
- **Action**: split `SnapshotCapableBackend` trait. Only `NomadCHBackend` implements it. Use `&dyn SnapshotCapableBackend` at call sites that need the surface.

### [R3-A2] `restore_handler` is silently a second NomadCHBackend implementation (IMPORTANT, arch-r3)
- **File**: `crates/sandbox/src/restore_handler.rs` (full)
- **Symptom**: restore lifecycle logic lives outside `nomad_ch.rs` despite being nomad-ch-specific. Two seams to keep in sync.
- **Action**: move the body of `restore_handler::do_restore_inner` into `NomadCHBackend::do_restore_inner` (or a sibling private fn in `nomad_ch.rs`). The HTTP handler in `restore_handler.rs` becomes a thin shim.

### [R3-A3] Wrapper bash should move to Rust sidecar (IMPORTANT, arch-r3; closes W1 structurally)
- **File**: `crates/sandbox/scripts/nomad-vm-wrapper.sh` (421 LOC bash)
- **Symptom**: bash does JSON surgery (`sed -i`), cmdline building, and now CH-API polling. Each new feature adds bash. Eventually rewrites in Rust.
- **Action**: extract a small Rust binary `zsbx-vm-wrapper` (or fold into `zeroship-sandbox` as a subcommand) that handles the per-VM ops. Bash shrinks to `exec zsbx-vm-wrapper "$@"`. Bonus: closes W1 (sed code-exec sink) structurally.

### [R3-A4] `StopDisposition` enum would prevent C2-class bugs (IMPORTANT, arch-r3)
- **Files**: `crates/sandbox/src/backend/nomad_ch.rs` (`stop_inner(_, remove_host_dir: bool)`)
- **Symptom**: bool flags accumulate (B15 added `remove_host_dir`, C2 reused it; future may add `cleanup_metrics`, `cancel_alloc`, etc.). Bool-soup signature.
- **Action**: introduce `enum StopDisposition { TearDown, PreserveForSnapshotWake }` (or similar). Methods take the enum; the boolean knobs become exhaustive match arms.

### [R3-Q2] A6 added infallible `Result<Self, String>` builder signatures (MINOR, code-quality-r3)
- **File**: `crates/sandbox/src/lib.rs:272` (`with_persistence -> Result<Self, String>`)
- **Symptom**: `Result<Self, String>` that cannot fail forces in-crate callers to `.expect()` on an infallible operation. Signature smell.
- **Action**: change to `pub fn with_persistence(self, p: Arc<Persistence>) -> Self`. Future invariants can switch to `Result` when they actually need it. Same shape applies to A6b's 5 new builders — review them.

### [R3-Q3] Stale `alloc_running_timeout_secs: 60` in 11+ test fixtures post-T3 (MINOR, code-quality-r3)
- **Files**: 11 test fixtures across `crates/sandbox/src/config.rs:744`, `backend/docker.rs:832`, `backend/nomad_ch.rs:3449`, 4 e2e tests; plus A6 newly copy-pasted `60` at `lib.rs:1267,1399`
- **Symptom**: T3 bumped production default 60→120 but synthetic test fixtures stayed at 60. Not a bug today (fixtures don't exercise the default-loading path) but a noise source for grep-based refactors.
- **Action**: bulk-update fixture literals 60→120. One-line per fixture. No test logic change.

### [R4-A1] AppState builder pattern is becoming a typed-state builder by accretion (CRITICAL, arch-r4)
- **Files**: `crates/sandbox/src/lib.rs:50-407` — 8 `with_*` builders across 3 different return-type shapes (`Self`, `Result<Self, String>`, mixed) + a 14-field `new_fixture()` shadow constructor on both `AppState` AND `SandboxConfig`
- **Symptom**: A5/A6/A6b/A7 trail evidences a typed-state builder being built by accretion rather than design. `new_fixture()` now spans 2 production types as `pub` API — test scaffolding has leaked into production surface.
- **Action**: refactor to a single `AppStateBuilder` typed-state pattern. `new_fixture()` becomes `#[cfg(test)]` or feature-gated behind `pub(crate)`. R3-Q2 (infallible Result) auto-closes by consistent shape.

### [R4-A2] State HashMap eviction + vm_index_allocator release straddle 60-120s of async fence work (CRITICAL, arch-r4)
- **Files**: `crates/sandbox/src/backend/nomad_ch.rs:944-952` (state HashMap removal) + `:1087-1091` (vm_index_allocator release)
- **Symptom**: two separate locked structures span 60-120s of async fence work with no unified RAII guard. This is the architectural coupling underneath B18 — even though B18's surface fix (shared allocator) closed the symptom, the underlying race-window pattern remains. B19 is a symptom of the same design hole (forgotten state-map registration).
- **Action**: introduce a `LeasedVmSlot` RAII guard that holds the state-map entry + vm_index reservation atomically; release on Drop or explicit commit. Subsumes the B18 fix and prevents B19-class bugs.

### [R4-Q1] A7 newly copy-pasted stale `alloc_running_timeout_secs: 60` literal (MINOR, quality-r4)
- **File**: `crates/sandbox/src/config.rs:652` (A7's new `new_fixture()` shadow constructor)
- **Symptom**: same commit whose doc cites R3-Q2 by name introduced 2 more `60` literals (5 → 7 in-src). R3-Q3 already tracks the 11 fixture sites; this expands the count without addressing the root pattern.
- **Action**: bulk-update 60 → 120 in fixtures (R3-Q3 catch-all). Subsume R4-Q1.

### [R4-T1] `shellcheck --severity=error` would land GREEN as a free regression guard (CRITICAL, test-cov-r4)
- **Source**: 2026-05-24 test-coverage-r4 ran shellcheck on `crates/sandbox/scripts/nomad-vm-wrapper.sh` and got 5 INFO-level only, 0 errors/warnings.
- **Action**: add `shellcheck --severity=error crates/sandbox/scripts/*.sh` as a workspace gate. Costs minutes; would have caught B12/B13/B17-like issues pre-cluster. Sister of R3-T3 (full coverage) — this is the cheap-first-step.

### [R4-T2] A3 local wake-latency canary is feasible (IMPORTANT, test-cov-r4)
- **Source**: 2026-05-24 test-coverage-r4
- **Note**: two concurrent `do_restore_inner` calls + a `SlowGetSnapshotStore` (returns 1 GB in 100ms simulated) would expose A3's 9.5s wake p50 without a real cluster. `restore_handler.rs:286-412` already takes `&dyn` traits.
- **Action**: add a `wake_latency_canary` integration test that asserts wake completes within N×100ms when N concurrent waker tasks run. Catches A3 regression before cluster.

### [R3-T2] A5/A6/A6b builders cover happy/empty only — no fuzz/edge (MINOR, test-coverage-r3)
- **Files**: `lib.rs::admin_token_setter_tests` + `persist_setter_tests` + new `field_setter_tests`
- **Symptom**: no all-whitespace, NUL-byte, oversized-input, or fuzz coverage. Edge cases that production input validators usually need to catch.
- **Action**: add `quickcheck` or `proptest` to `dev-dependencies`; lock builders against pathological inputs (e.g., 1 MB token, NUL-embedded token).

### [R3-T3] Wrapper still zero in-repo coverage despite B17 (CRITICAL, test-coverage-r3)
- **File**: `crates/sandbox/scripts/nomad-vm-wrapper.sh` (421 LOC bash; B17 closed by editing lines 366-419, no test)
- **Symptom**: no `shellcheck`, no `bats`, no `bash -n` gate in CI. B17 (and prior B12/B13) were caught at cluster smoke — i.e., $0.50+ per discovery instead of pre-commit.
- **Action**: add `shellcheck crates/sandbox/scripts/*.sh` to the workspace CI. Optionally add `bats` smoke tests that exercise the wrapper's path-handling without spawning CH.

---

## IMPORTANT (T6 regressions — round-r2 reviewers flagged)

### [T8] `ControllerIdleSnapshotter` zero non-pg coverage (test-coverage-r2)
- **Source**: 2026-05-24 test-coverage round-r2
- **File**: `crates/sandbox/src/sweep.rs:292-383` (90 LOC of new prod code)
- **Symptom**: pg-gated tests exercise only `RecordingIdleSnapshotter`. The real prod implementation that bridges to `snapshot_handler::snapshot_sandbox` has no unit test.
- **Action**: add a non-pg unit test using a `MockSnapshotSandbox` trait/fake. Mirror `RecordingIdleSnapshotter`'s pattern but for production code paths.

### [T9] `ControllerIdleSnapshotter` duplicates `admin_handlers` orchestration (architecture-r2)
- **Source**: 2026-05-24 architecture round-r2
- **Files**: `crates/sandbox/src/sweep.rs:283-407` vs `crates/sandbox/src/admin_handlers.rs:1109-1206`
- **Symptom**: near-byte-for-byte parallel orchestrator — same `lookup_source_vm_ops` preflight, same `snap_stage_dir` resolution, same post-snapshot best-effort `teardown_source_for_snapshot`, same `StateMismatch` race tolerance. Private `ResolvedSourceVmOps` struct is byte-identical between the two files.
- **Action**: extract shared orchestrator function `pub(crate) fn perform_snapshot(...)` in a new `crates/sandbox/src/snapshot_orchestrator.rs`; both callers invoke it. Move `ResolvedSourceVmOps` to a `pub(crate)` location.

### [T10] `ControllerIdleSnapshotter` holds `Arc<AppState>` with 4-field reach (architecture-r2)
- **Source**: 2026-05-24 architecture round-r2
- **Files**: `crates/sandbox/src/sweep.rs:283-294` + `crates/sandbox/src/lib.rs:449-454`
- **Symptom**: the bridge reaches into `state.backend`, `state.config`, `state.database`, `state.snapshot_store` from sweep — turning what was a pure DB→CAS sweep module into an AppState god-Arc consumer. Layering escape hatch.
- **Action**: pass the required fields explicitly via a small constructor-bound struct (`SnapshotterDeps { backend, config, database, snapshot_store }`). Sweep then doesn't need `AppState`. Easier to unit-test.

---

## IMPORTANT (Phase B follow-ups; blocked on #14 closing)

### [B-SLO] 5-worker × 20-cycle SLO empirical validation
- **Blocked-by**: **B18** (slot-reuse pubkey 401 — blocks scale past ~6 cycles per worker). B14a/B14b/B17 all closed.
- **Action**: close B18 first. Then scale to 3+5 + run cluster stress; capture create / snapshot / wake / post-exec / stop p50/p95/p99/max; compare wake p50 (this cycle measured 9.5s on c=1 single-cycle) to 4243ms cold-boot baseline (§ 10.2 SLO targets: p50 ≤ 1.0s; p95 ≤ 1.5s; p99 ≤ 2.0s; p99.9 ≤ 6.0s). Note: 9.5s p50 is FAR worse than the 1.0s target — likely improvable once A3 (sync I/O on compio worker) closes; document the gap when stress lands.

---

## IMPORTANT (main-repo sandbox TODO P1 — pick up once Phase B is closed)

### [T1] Read-only `sandbox_admin_ro` role for admin read endpoints
- **Source**: `crates/sandbox/TODO.md` P1 (Round-4 IMPORTANT #7 deferred)
- **Scope**: new migration (5th role + read-only grants); `Database::pool_admin_ro()` helper; per-handler routing decision (`open_app_pool` → `open_admin_ro_pool` for the list/detail/export read paths).
- **Last considered**: 2026-05-22 — clean follow-up; no blocker

### [T2] Rate-limit on heavyweight admin endpoints
- **Source**: `crates/sandbox/TODO.md` P1 (Round-4 IMPORTANT #8)
- **Scope**: counting bucket on `GET /admin/users/{u}/export` (REPEATABLE READ + multi-table aggregate) + `DELETE /admin/users/{u}` (multi-statement cascade). Default 1 req/s sustained, burst 5; bypass for `force_takeover` admin scope.
- **Last considered**: 2026-05-22 — clean follow-up; no blocker

### [T4] Controller-side stop semaphore
- **Source**: May-5 cluster stress observations
- **Scope**: cap concurrent host_fence polls per worker via `Mutex<HashMap<worker_id, RateBucket>>` on AppState. Default 16 concurrent stops; surfaces metric.

### [T5] Restore handler: signed `/version` fingerprint check
- **Source**: Phase A subagent report (deferred from initial impl)
- **Scope**: v1 polls only unsigned `/livez`; the signed `/version` fingerprint requires plumbing the per-sandbox signing key out of the sealed record. Mitigates a hypothetical stale-tenant race on v2 cluster-wide restore. Defer until cross-worker restore is on the roadmap.

---

## MINOR (defer to broader cleanup)

### [Q1] AEAD DEK derivation `time_of_put_unix_secs` vs pg `snapshot_taken_at`
- **Source**: Phase A subagent report
- **Note**: DEK depends on `snapshot_taken_at` per proposal § 4.3; controller can't pre-stage exact value since pg writes it server-side. v1 stamps `time_of_put_unix_secs` into the AEAD header itself; get-path re-derives. Consider whether this divergence from the proposal text matters.

### [Q2] GCS upload retry-with-backoff on `x-goog-hash` mismatch
- **Source**: Phase A subagent report
- **Note**: proposal § 4 step 3 mandates "retried up to 3× before alerting"; current `GcsSnapshotStore::put` is single-shot. Add retry loop on upload completion mismatch.

### [Q3] `TieredSnapshotStore` L2-back-fill on L1 miss
- **Source**: Phase A subagent report
- **Note**: L1-miss → L2-hit doesn't populate L1 as side effect; just delegates. Documented as TODO in the GCS PR; fix lands when the GCS adapter graduates from stub.

### [Q4] Per-creator workspace.img size override
- **Source**: virtio-blk pivot (2026-05-22)
- **Note**: `SANDBOX_WORKSPACE_IMAGE_SIZE_GB` is controller-wide (default 20). Premium tier might want larger workspaces. Add per-creator override + lifecycle policy hooks.

### [Q5] `crates/sandbox/tests/sandbox_pg_e2e.rs` fixture refresh post-virtio-blk
- **Source**: virtio-blk pivot subagent report
- **Note**: two `#[ignore]`d pg-gated tests still carry an `fs[].socket` entry in their fixture `config.json`. The rewrite invariant ("doesn't touch fs[]") still holds, but the fixture's virtiofs shape is legacy. Refresh to virtio-blk-shaped fixtures.

### [Q6] Doc comments on `NomadCHConfig::keys_dir/workspace_dir/user_home_dir_root`
- **Source**: virtio-blk pivot subagent report
- **Note**: post-pivot, these are image-root paths not dir-mount roots. Doc still says "directory persists across sandbox lifetimes" — functionally accurate but lexically stale. Doc-only pass.

---

## SCOPE GUARDS (read by every cron cycle)

**Allowed**:
- `crates/sandbox/**` (primary target)
- `crates/sandbox-agent/**` (in-VM agent — touch only when a finding genuinely demands it)
- `docs/reviews/sandbox-snapshot-*` (review artifacts, this file)
- `crates/sandbox/TODO.md` (backlog cross-linking)
- `crates/sandbox/migrations/**` (only for new migrations approved by the pilot)

**Forbidden** (drift here is a `git restore` + re-dispatch, not a "let me just fix this too"):
- `crates/gateway/**`, `crates/control/**`, `crates/worker/**`, `crates/runtime/**`, `crates/runtime-macros/**`, etc. (any non-sandbox crate)
- `docs/proposals/sandbox-snapshot-restore.md` (per workflow rule — proposal commits with implementing PR only)
- Any `git push` to remote (user rule for this cron)
- Any change to the `main` branch directly
- Deleting GCS buckets, firewall rules, VPC, or shared infra
- Force-push, --no-verify, --no-gpg-sign

**Real-env testing**:
- Each cluster cycle MUST end with `bash crates/sandbox/scripts/teardown-gcp-cluster.sh` regardless of outcome
- $30 hard cap per cycle (despite the 100K GCP credits — bound burn rate, not budget)
- Honor the "13-bug observation chain" lesson: log every cluster bug verbatim before clearing it
