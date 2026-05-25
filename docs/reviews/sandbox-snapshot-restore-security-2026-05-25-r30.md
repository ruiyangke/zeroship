# Sandbox/snapshot-restore — security r30 review

Date: 2026-05-25 (UTC)
HEAD at audit: `37a53878` (worktree `.worktrees/sandbox-snapshot-restore`, branch `feat/sandbox-snapshot-restore`). Clean worktree.
Predecessor: r29 at `ce062846` (`docs/reviews/sandbox-snapshot-restore-security-2026-05-25-r29.md`). Lens: security (READ-ONLY).

Landings since r29 (sandbox/ + scripts/ scope):

- `9ac5b850` — sandbox: cfg-gate 5 test-scaffolding pub items under test-support feature (R28-API2 sweep). **Primary focal commit this round (security closure).**
- `62b083e1` — sandbox/nomad-ch: replace spawn_delayed_release with typed Task + inline-await helpers (R29-C1 + r29-A2 class-fix). Internal control-flow fix; no new auth/wire surface.
- `81b6e689` — sandbox/registry: parallelize snap-idle-gc stops (R29-P1). **Primary focal commit this round (security audit of fan-out).**
- `0ee106d2` — sandbox/scripts: bump controller pin v37→v38 (R29-P1 GC parallelize). Script-only; no code surface.
- `3a53b7ba` — pilot round-44 reviewer artefacts (code-quality r30, concurrency r30; arch r30 inline). Docs only.
- `37a53878` — sandbox/scripts: bump driver pin v20→v22 (COW rootfs + lock-wait probe). Scripts only; SHA updated to `291f9d9f373143b4c48cf55d26d0444c153cdce6fa6616fff9f8d9e379ea4841`. R20-S3 carry updated.

## Summary

**r30 produces ZERO new CRITICALs, ZERO new IMPORTANTs, ZERO new MINORs.**

### Closures this round

- **r29-carry-API2 `Database::set_role_dsns_for_test`**: **CLOSED at `9ac5b850`**. The function is DELETED (not merely gated). `grep -rn set_role_dsns_for_test crates/sandbox/` returns zero src/ hits; only historical comments in `tests/sandbox_pg_e2e.rs` rationale text remain. The threat model that warranted MINOR tracking (silent DSN override of audit/GDPR pools reachable if a future PR exposed `&mut Database` from a handler) is structurally eliminated: the symbol does not exist.
- **r29-carry-API2-siblings** (4 items: `from_test_config`, `StubRestoreBackend`, `StubSourceVmOps`, `RecordingIdleSnapshotter`): **CLOSED at `9ac5b850`** via `#[cfg(any(test, feature = "test-support"))]` gates applied uniformly to each item plus all its `impl` blocks. Verified by code-quality r30 at HEAD: `nm` confirms zero occurrences in production rlib. Security-lens below-MINOR classification confirmed; cleanup-debt shape was the only concern and it is resolved.
- **r29-carry-M1 (`extract_failed_task_event_msgs` byte-indexed truncation)**: **ALREADY CLOSED prior to r29** at commit `dfccd049` ("char-boundary truncation in error message cap — r27-M2"). The r29 review incorrectly carried this as open by tracking the wrong line numbers. Audit at HEAD `nomad_ch.rs:3151-3158` confirms `is_char_boundary` walk-back is in place. **Closing this carry at r30; no action required.**

### Focal questions (per r30 brief)

Four focal questions were raised. All reviewed as CLEAN; details in sections below.

1. **r30-A1 proposed semaphore — any auth implication?** The "semaphore" in the brief refers to `SANDBOX_SNAPSHOT_PER_WORKER_CONCURRENCY` in `sweep.rs` (already landed) which controls the per-worker concurrent snapshot upload cap via `join_all` chunking at `sweep.rs:64`. **No auth implication.** The cap governs I/O fan-out against GCS, not any auth surface. No new wire surface; no signing-key or credential path.

2. **R29-P1 parallel GC: race condition leaking state across users in fan-out?** **CLEAN.** Detailed audit below at [r30-FOCAL-P1 OK].

3. **R30-I1 join_all panic-amplification: does a panicking handler leak per-sandbox secrets in logs?** **CLEAN (no secret leak).** Detailed audit below at [r30-FOCAL-I1 OK].

4. **Driver v21+v22 changes affecting controller's secret-passing?** **OUT OF SCOPE (driver-side) — CLEAN at boundary.** The only controller-side change is the SHA pin bump at `gcp-worker-startup.sh`. The SHA-pin guard is unchanged; the new binary is authenticated before use. No new secret-passing interface introduced on the controller side.

### Carry status changes

- **r29-carry-M1** (byte-indexed truncation): CLOSED — was already fixed at `dfccd049` pre-r29. The r29 carry was erroneous.
- **r29-carry-API2** and **r29-carry-API2-siblings**: CLOSED — deleted/gated at `9ac5b850`.
- All other carries: unchanged. See [Carries below].

**Net new findings this round: 0 CRITICAL, 0 IMPORTANT, 0 MINOR.**

**Cutover gate status: R13-S1 (`--scopes=storage-rw` on worker GCE SA) remains the SOLE pre-cutover Ops blocker. Controller-side pre-cutover CLEAR of IMPORTANTs.**

---

## CRITICAL

None.

## IMPORTANT

None.

## MINOR

### [r30-FOCAL-P1 OK] R29-P1 `gc_stop_chunked` join_all fan-out — no cross-tenant state leak

- **Status**: **CLEAN** (NO new finding). Filed as security verification for the brief's focal question.
- **File**: `crates/sandbox/src/registry.rs:956-1006`.

#### Audit

The `AppStateGcStopper::stop_one` body (lines 956-987) captures `Arc<AppState>` and dispatches `state.backend.stop(id).await` for a single sandbox UUID. The `join_all` at line 1004 fans up to `cap=8` of these futures concurrently. Security concern: does any future in the chunk hold or expose secret material of OTHER sandboxes?

1. **Data isolation per future**: each `stop_one` closure captures a cloned `Arc<AppState>` (shared, read-only ref count bump) and one `Uuid` argument. The `state.backend.stop(id).await` path inside `stop_inner` reads the sandbox's own `NomadChSandbox` entry from the `state.write()` map, immediately removes it (`state.write().remove(&sandbox_id)` at `nomad_ch.rs:1232`), and holds the removed `NomadChSandbox` struct locally for the remainder of `stop_inner`. Each future thus holds ONLY its own sandbox's data; no future in the chunk can read another sandbox's data after the remove-from-map step.

2. **No cross-future secret sharing**: the `signing_key: Arc<SigningKey>` field of `NomadChSandbox` is held inside the local `sandbox` binding after `remove` returns. It is used only for `http_signed_async(... "/shutdown" ...)` at `nomad_ch.rs:1261`. It is never placed into a shared structure or logged (the `NomadChSandbox::Debug` impl at lines 269-280 explicitly omits `signing_key` via `finish_non_exhaustive()`). No other future in the `join_all` chunk has access to this binding.

3. **join_all completion guarantee**: `futures::future::join_all` awaits ALL futures in the chunk before yielding. If future A completes and future B panics, `join_all`'s drop runs, which drops all pending futures including future C. The `Arc<NomadChSandbox>` inside C's removed-from-map binding is dropped — no memory-level cross-tenant access.

4. **Log output from `AppStateGcStopper::stop_one`** (lines 963-980):
   - `tracing::info!` at line 963: `sandbox_id`, `user_id`, `project_id`, `backend` — no secrets.
   - `tracing::warn!` at line 976: `sandbox_id = %id`, `error = %e` where `e` is the `String` from `stop_inner`'s `errs.join("; ")`. That string is constructed from `/shutdown`, `stop_nomad_job`, `wait_for_job_gone` errors formatted with `sandbox.job_id` only (see `nomad_ch.rs:1268-1294`). No signing key, no DSN, no AEAD key in the error string.

5. **Panic unwind**: if a future panics (the only identified panic path in `stop_one` is the `state.sandboxes.get(&id)` `read().unwrap()` on a poisoned RwLock — see concurrency r30 R30-I1 and code-quality r30 R30-M1), the panic payload is the `&str` of the `unwrap()` failure message (e.g. `"called \`Result::unwrap()\` on an \`Err\` value: PoisonError { ... }"`). The panic backtrace printed to stderr by the OS thread shows function names and file/line numbers, NOT variable contents. The `signing_key` bytes are NOT in any local variable accessible from the backtrace-formatted output.

**Conclusion**: R29-P1's fan-out introduces no cross-tenant state leak. Each future holds only its own sandbox's data; the `NomadChSandbox::Debug` secret-hygiene guard (lines 259-280) prevents signing-key exposure in any panic-backtrace or debug-log path; the per-sandbox `remove-from-map-then-operate` pattern structurally isolates concurrently executing futures. No new MINOR.

---

### [r30-FOCAL-I1 OK] R30-I1 join_all panic: no per-sandbox secret leak in logs or backtrace; vm_index leaked, NOT handed out

- **Status**: **CLEAN (NO new finding).** The concurrency r30 R30-I1 finding stands as IMPORTANT for the blast-radius amplification (1 → up to 8 vm_index leaks). Security analysis below confirms the panic amplification does NOT create a cross-tenant attack surface — the leaked vm_index is held by the allocator (not released), so no new tenant can be assigned it.
- **File**: `crates/sandbox/src/registry.rs:956-1006` (fan-out path); `crates/sandbox/src/detach.rs:76-110` (OS thread + `block_on`).

#### Security analysis of the panic path

1. **Panic trigger path**: `AppStateGcStopper::stop_one` at line 963 calls `state.sandboxes.get(&id)`, which internally calls `self.by_sandbox.read().unwrap()` (bare `.unwrap()` on a potentially poisoned `RwLock`). If the lock is poisoned (a writer panicked inside the lock while the GC was running — an unlikely precondition), this `unwrap()` panics. The panic propagates up through the `Box::pin(async move { ... })` future and into `join_all`, which drops the remaining in-flight futures and unwinds the OS thread.

2. **Secret exposure in panic output**: the OS thread hosting `snap-idle-gc` (created by `detach_isolated("snap-idle-gc", ...)` at `registry.rs:1016`) unwinds via Rust's default panic handler, which prints the panic message + backtrace to stderr. The backtrace contains function names and file/line numbers, NOT variable values. The `signing_key: Arc<SigningKey>` is not in scope at the `get()` call site — it is only read inside `stop_inner` AFTER `state.write().remove(&sandbox_id)`. Therefore, even if the panic happened mid-`stop_inner` (a different unwrap site), the backtrace would not include the key bytes. **No secret leak via panic output.**

3. **vm_index fate on panic**: when `join_all` drops in-flight futures due to a panic:
   - The dropped future's `sandbox` local variable (holding the removed `NomadChSandbox`) is dropped.
   - `NomadChSandbox::drop` has no special destructor — it is just a plain struct drop.
   - The `vm_index` field of that sandbox was already removed from the registry map (step 1 of `stop_inner` at `nomad_ch.rs:1232`) but `VmIndexAllocator::release_vm_index_after` was not yet called (it runs at step 4 of `stop_inner`, after the fence).
   - **The vm_index is NOT released back to the free list.** The allocator still considers it in-use.
   - Consequence: the slot is held until the next-boot orphan-prune (the same path as any other vm_index leak scenario: host_fence timeout, wait_for_job_gone failure, etc.).
   - **A new tenant CANNOT be assigned the leaked vm_index** because the allocator never freed it. The cross-tenant IP-reuse attack (the FM-A race that the host_fence guards against) is NOT opened by this panic path.

4. **Security severity of the blast radius**: the concurrency r30 R30-I1 IMPORTANT finding is about _operational availability_ (up to 8 vm_index slots drained per panic event, requiring a controller restart to reclaim via orphan-prune). From the security lens, this is a **DoS-class concern** (pool exhaustion), not a data-exposure or cross-tenant concern. It does not escalate above the existing pre-R29-P1 baseline from the security perspective.

5. **Amplification from 1→8**: the pre-R29-P1 serial loop had a blast radius of 1 leaked vm_index per panic. Post-R29-P1 it is up to 8. From the security lens, both cases are DoS-class: a tenant who could trigger RwLock poisoning (requiring a prior panic inside a write-lock section, which has no known trigger today) achieves pool starvation. The concurrency r30 R30-I1 fix recommendation (`catch_unwind` inside `stop_one` body, wrapping `backend.stop(id).await`) is the appropriate remediation; the security lens defers to concurrency for priority weighting.

**Conclusion**: the R30-I1 panic-amplification is IMPORTANT from the concurrency lens but carries NO new MINOR security finding — it is DoS-class operationally (pool exhaustion), not data-exposure class. The existing panic-containment guarantee (locked-RwLock-poison as the only trigger; no production code poisons the lock today) remains; the cross-tenant attack surface is structurally blocked by the allocator's not-released invariant.

---

### [r30-SHA-pin-update] Driver SHA pin updated to v22

- **Status**: **CARRY UPDATE** (R20-S3 SHA-pin closure confirmed at new value).
- **File**: `crates/sandbox/scripts/gcp-worker-startup.sh:185-198`.
- **New SHA**: `291f9d9f373143b4c48cf55d26d0444c153cdce6fa6616fff9f8d9e379ea4841`.
- **Object**: `nomad-driver-ch.v22`.
- The startup script SHA-mismatch check (fatal exit on mismatch) is unchanged. The pin enforcement mechanism is intact. Driver v21+v22 changes (COW rootfs + OFD lock probe) are driver-side and out-of-scope for this review; controller's secret-passing interface (the per-sandbox `signing_key` exchanged via the agent HTTP surface) is unchanged — the driver bump does not alter any auth wire format.
- **Severity**: carry update only. R20-S3 remains CLOSED.

---

## Carry table at r30

| Finding | r29 state | r30 state |
|---|---|---|
| **r29-carry-API2** `set_role_dsns_for_test` | OPEN MINOR (security) / IMPORTANT (api-surface) | **CLOSED** — deleted at `9ac5b850` |
| **r29-carry-API2-siblings** (4 items) | OPEN below-MINOR (security) | **CLOSED** — cfg-gated at `9ac5b850` |
| **r29-carry-M1** byte-indexed truncation | OPEN MINOR | **CLOSED** — was already fixed at `dfccd049` pre-r28; r29 carry was erroneous |
| **r29-carry-HEREDOC** 11 unquoted heredocs | OPEN MINOR (security) / CRITICAL (architecture) | **CARRY** — unchanged at HEAD; architecture r29 r29-A1 owns the fix shape |
| **r29-carry-r28-M1** T5 body excerpt 256-char WARN | OPEN MINOR | **CARRY** — unchanged at `restore_handler.rs:3308-3318` |
| **r29-carry-S1** Guard B node-id cross-check missing | OPEN MINOR | **CARRY** — `fetch_local_nomad_node_id` at `nomad_ch.rs:3405-3418` unchanged |
| **r29-carry-M3** `admin_handlers.rs:1988` wake-poll RFC1918 | OPEN MINOR | **CARRY** — unchanged |
| **r29-carry-M4** `admin_handlers.rs:1515` artifact_path Full-bearer | OPEN MINOR | **CARRY** — unchanged |
| **r29-carry-M5** CI policy carry | OPEN POLICY | **CARRY** — unchanged |
| **R22-S1** sanitize_error_message ROOTS whitelist | CLOSED | Re-verified CLOSED at `wake_machine.rs:932-1213` |
| **R20-S3** driver SHA-pin | CLOSED | **CARRY UPDATE** — v22 SHA `291f9d9f…` at `gcp-worker-startup.sh:185-198` |
| **R21-S1 / R21-S2 / R20-S2 / R19-S1 / R13-S1 / R9-S3** | OPEN IMPORTANT | **CARRY** — unchanged. **R13-S1 remains the sole pre-cutover Ops blocker.** |
| **r27-S1 Guard A** validate_nomad_addr_loopback | CLOSED | Re-verified CLOSED — config.rs unchanged |
| **r27-M1** sanitize whitelist + hyphenated UUID | CLOSED | Re-verified CLOSED — wake_machine.rs:1331 unchanged |
| **r27-M2** Latin-1 cast (6 sites) | CLOSED | Re-verified CLOSED — wake_machine.rs:1054 unchanged |

---

## Focal-list checks (per r30 brief)

### Focal #1 — r30-A1 proposed semaphore — any auth implication?

**Status: CLEAN.** The "semaphore" in the brief refers to the `SANDBOX_SNAPSHOT_PER_WORKER_CONCURRENCY` env-knob (default 2, exposed at `sweep.rs:56-64`) which gates concurrent snapshot operations via `join_all` chunking. This is already landed. The semaphore controls GCS I/O fan-out only — it has no auth surface, no secret-passing interface, and no wire-format impact. No new MINOR.

### Focal #2 — R29-P1 parallel GC: race condition leaking state across users?

**Status: CLEAN.** See [r30-FOCAL-P1 OK] above. The per-future isolation guarantee (each future removes its own sandbox from the shared map before operating on it; no future can read another's `NomadChSandbox` or `signing_key`) is structural. No cross-tenant data exposure path. No new MINOR.

### Focal #3 — R30-I1 join_all panic-amplification: per-sandbox secret leak in logs?

**Status: CLEAN.** See [r30-FOCAL-I1 OK] above. A panic in `stop_one` propagates through `join_all` to the OS thread, where Rust's default panic handler emits a backtrace to stderr. The backtrace contains function names only, not variable values. The `signing_key` is not in scope at the only identified panic trigger (the `get()` `read().unwrap()` site). The `NomadChSandbox::Debug` secret-hygiene guard (lines 259-280) provides defense-in-depth. The vm_index leaked by a cancellation is held by the allocator, not released — cross-tenant IP-reuse does not occur. No new MINOR.

### Focal #4 — Driver v21+v22 interface change affecting controller's secret-passing?

**Status: CLEAN.** The controller's secret-passing interface (per-sandbox HMAC signing key injected at CREATE time via `http_signed_async`) is unchanged in both the controller crate and the driver's RPC surface. Driver v21 adds a `wake_rootfs_lock_wait` stage (OFD lock probe) and v22 adds `stage_rootfs` COW (FICLONE + io.Copy fallback for unique inode per wake). Neither stage modifies the signing-key bootstrap, the Nomad job spec secret-injection path, or the `/shutdown`/`/version`/`/_clock_resync` agent endpoints. No new MINOR.

---

## Cross-lens consensus

- **R29-P1 cross-lens**: concurrency r30 IMPORTANT (R30-I1 blast-radius amplification; `catch_unwind` inside `stop_one` is the fix); code-quality r30 MINOR (asymmetric panic guard — `get()` unwrapped, `remove()` wrapped); security r30 CLEAN (no secret leak, no cross-tenant exposure, DoS-class only). The concurrency r30 `catch_unwind`-inside-`stop_one` fix is the appropriate remediation and will close both the concurrency and code-quality findings. Security defers entirely to concurrency for priority weighting; no additional security-lens action.
- **R28-API2 cross-lens**: CLOSED at `9ac5b850`. Security r29 MINOR (defense-in-depth) and api-surface r28 IMPORTANT (hygiene) are both resolved. The deletion of `set_role_dsns_for_test` (not just gating) is the preferred pre-launch outcome per the no-back-compat stance.
- **r29-carry-HEREDOC cross-lens**: architecture r29 r29-A1 CRITICAL (class-level heredoc fix needed); security r30 MINOR (operator-trust boundary). Carry unchanged. The architecture r29 Option 1 fix (quoted-EOF heredocs / systemd drop-ins) remains the recommended shape and closes both lenses.
- **Driver SHA-pin (R20-S3)**: updated to v22. The pin-enforcement mechanism is intact.

---

## Lens hand-off

- **Sandbox controller (Rust)** — security pre-cutover is CLEAR of IMPORTANTs. Post-cutover work:
  - **Guard B** (r30-carry-S1; ~30 LOC) — unchanged from r29. `nomad_ch.rs:3405-3418`.
  - **r29-carry-r28-M1** (~3 LOC) — strip control chars from T5 body excerpt. `restore_handler.rs:3308-3318`.
  - **r29-carry-HEREDOC** — architecture r29 r29-A1 Option 1 (systemd drop-ins + quoted-EOF) closes both architecture CRITICAL and security MINOR. Cross-lens reinforcement: security agrees with architecture's Option 1 recommendation.
  - **r29-carry-M3/M4/M5** — unchanged; cosmetic/policy.

- **Ops / cluster bring-up** — **R13-S1 (`--scopes=storage-rw`) remains the SOLE pre-cutover Ops blocker.** The r29-carry-HEREDOC class (architecture CRITICAL; security MINOR) is the next operational priority: block stress-r10 on either the architecture r29 Option 1 landing (systemd drop-ins) OR the lint.sh severity-gate promotion (architecture r29 Option 2).

- **nomad-driver-ch maintainers (cross-worktree)** — v22 landed at `37a53878`. Driver v21+v22 combine OFD lock probe + COW rootfs. No security-lens action on the controller side this round.

- **Concurrency r30 owners** — R30-I1 `catch_unwind` inside `AppStateGcStopper::stop_one` is the next registry-side hardening pass. Security defers entirely; the fix closes both concurrency IMPORTANT and code-quality MINOR. Priority: IMPORTANT (concurrency lens).

---

## Counts

- CRITICAL: 0 new; 0 carry.
- IMPORTANT: 0 new. Carries: R21-S1, R21-S2, R20-S2, R19-S1, R13-S1, R9-S3.
- MINOR: 0 new. Carries: r30-carry-HEREDOC (cross-lens from architecture r29 r29-A1 CRITICAL), r30-carry-r28-M1, r30-carry-S1, r30-carry-M3, r30-carry-M4. Policy carry: r30-carry-M5.
- Closed this round: **r29-carry-API2** (`set_role_dsns_for_test` deleted at `9ac5b850`), **r29-carry-API2-siblings** (cfg-gated at `9ac5b850`), **r29-carry-M1** (was already closed pre-r29 at `dfccd049`; erroneous carry corrected).
- Total NEW this round: **0 IMPORTANT, 0 MINOR.**
- **Cutover gate: R13-S1 (storage-rw scope) remains the SOLE pre-cutover Ops blocker. Controller-side pre-cutover remains CLEAR of IMPORTANTs.**
